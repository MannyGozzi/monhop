//! Native socket preparation for one selected, directly connected private peer.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{Arc, Mutex},
};

use monhop_core::RevocationSignal;

use crate::policy::{
    InterfaceKind, InterfaceSnapshot, NetworkLock, PolicyError, RouteSnapshot, validate_interface,
    validate_peer,
};

use super::{NetworkSelection, RouteCheckFailure};

#[cfg(target_os = "macos")]
use monhop_platform_macos::{
    network::{self, Adapter},
    network_watch, udp_receive,
};
#[cfg(windows)]
use monhop_platform_windows::{
    network::{self, Adapter},
    network_watch, udp_receive,
};

#[cfg(target_os = "macos")]
pub(super) type NativeSocket = udp_receive::AsyncUdpReceiver;
#[cfg(windows)]
pub(super) type NativeSocket = udp_receive::AsyncUdpReceiver;

#[cfg(target_os = "macos")]
pub(super) type Watch = network_watch::NetworkChangeWatcher;
#[cfg(windows)]
pub(super) type Watch = network_watch::NetworkChangeWatch;

pub(super) struct PreparedNetwork {
    pub socket: NativeSocket,
    pub lock: NetworkLock,
    pub signal: RevocationSignal,
    pub watch: Watch,
}

struct PreparedSelection {
    initial: Adapter,
    lock: NetworkLock,
    signal: RevocationSignal,
    watch: Watch,
}

pub(super) fn prepare(selection: &NetworkSelection) -> io::Result<PreparedNetwork> {
    let PreparedSelection {
        initial,
        mut lock,
        signal,
        watch,
    } = prepare_selection(selection, None)?;

    require_active(&signal, None)?;
    let socket = UdpSocket::bind(selection.local).map_err(native_category)?;
    verify_bound_address(&socket, selection.local)?;
    network::restrict_udp_interface(&socket, selection.interface_index).map_err(native_category)?;
    let socket = udp_receive::UdpReceiver::configure(socket).map_err(native_category)?;
    let socket = socket.into_async().map_err(native_category)?;
    if socket.local_addr().map_err(native_category)? != selection.local {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }

    revalidate_selection(&initial, selection, &mut lock, &signal, None)?;

    Ok(PreparedNetwork {
        socket,
        lock,
        signal,
        watch,
    })
}

#[cfg(target_os = "macos")]
pub(super) fn request_local_network_access_after_local_action(
    selection: &NetworkSelection,
    cancel: &RevocationSignal,
) -> io::Result<()> {
    let PreparedSelection {
        initial,
        mut lock,
        signal,
        watch: _watch,
    } = prepare_selection(selection, Some(cancel))?;

    request_with_gates(
        || require_active(&signal, Some(cancel)),
        || UdpSocket::bind(selection.local).map_err(native_category),
        |socket| {
            verify_bound_address(socket, selection.local)?;
            network::restrict_udp_interface(socket, selection.interface_index)
                .map_err(native_category)
        },
        || revalidate_selection(&initial, selection, &mut lock, &signal, Some(cancel)),
        |socket| socket.connect(selection.peer).map_err(native_category),
    )
}

fn prepare_selection(
    selection: &NetworkSelection,
    cancel: Option<&RevocationSignal>,
) -> io::Result<PreparedSelection> {
    validate_exact_bind(selection.local)?;
    require_cancel_active(cancel)?;
    let initial = find_exact_adapter(&enumerate_initial().map_err(native_category)?, selection)?;
    require_cancel_active(cancel)?;
    validate_initial_adapter(&initial)?;

    let pinned: PinnedLock = Arc::default();
    let watch = start_watch(&initial, selection, &pinned)?;
    let signal = watch.revocation_signal();
    require_active(&signal, cancel)?;

    let (selected, route) = current_selection(&initial, selection, &signal, cancel)?;
    let lock =
        NetworkLock::new(selected, *selection.peer.ip(), route, false).map_err(policy_category)?;
    *pinned
        .lock()
        .map_err(|_| io::Error::from(io::ErrorKind::Other))? = Some(lock.clone());
    require_active(&signal, cancel)?;

    Ok(PreparedSelection {
        initial,
        lock,
        signal,
        watch,
    })
}

fn revalidate_selection(
    initial: &Adapter,
    selection: &NetworkSelection,
    lock: &mut NetworkLock,
    signal: &RevocationSignal,
    cancel: Option<&RevocationSignal>,
) -> io::Result<()> {
    let (current, route) = current_selection(initial, selection, signal, cancel)?;
    lock.revalidate(Some(&current), route)
        .map_err(policy_category)?;
    require_active(signal, cancel)
}

fn current_selection(
    initial: &Adapter,
    selection: &NetworkSelection,
    signal: &RevocationSignal,
    cancel: Option<&RevocationSignal>,
) -> io::Result<(InterfaceSnapshot, RouteSnapshot)> {
    let observed = observe_selected_adapter(initial, selection)?;
    require_active(signal, cancel)?;
    let selected = interface_snapshot(&observed)?;
    let peer = *selection.peer.ip();
    validate_peer(&selected, peer).map_err(policy_category)?;
    let route = route_snapshot(&selected, peer)?;
    require_active(signal, cancel)?;
    Ok((selected, route))
}

#[cfg(target_os = "macos")]
fn request_with_gates<T>(
    mut require_active: impl FnMut() -> io::Result<()>,
    bind: impl FnOnce() -> io::Result<T>,
    pin: impl FnOnce(&T) -> io::Result<()>,
    mut revalidate: impl FnMut() -> io::Result<()>,
    connect: impl FnOnce(&T) -> io::Result<()>,
) -> io::Result<()> {
    require_active()?;
    let socket = bind()?;
    require_active()?;
    pin(&socket)?;
    require_active()?;
    revalidate()?;
    require_active()?;
    connect(&socket)?;
    require_active()
}

#[cfg(target_os = "macos")]
fn enumerate_initial() -> io::Result<Vec<Adapter>> {
    network::enumerate_adapters()
}

#[cfg(windows)]
fn enumerate_initial() -> io::Result<Vec<Adapter>> {
    network::enumerate_adapters()
}

#[cfg(target_os = "macos")]
fn enumerate_current() -> io::Result<Vec<Adapter>> {
    network::enumerate_adapters_with_attachment()
}

#[cfg(windows)]
fn enumerate_current() -> io::Result<Vec<Adapter>> {
    network::enumerate_adapters()
}

/// The watcher's private copy of the session lock; empty until the first snapshot is taken.
type PinnedLock = Arc<Mutex<Option<NetworkLock>>>;

#[cfg(target_os = "macos")]
fn start_watch(
    adapter: &Adapter,
    selection: &NetworkSelection,
    pinned: &PinnedLock,
) -> io::Result<Watch> {
    Watch::start_after_local_enable(
        &adapter.name,
        adapter.index,
        pinned_check(adapter, selection, pinned),
    )
    .map_err(io::Error::other)
}

#[cfg(windows)]
fn start_watch(
    adapter: &Adapter,
    selection: &NetworkSelection,
    pinned: &PinnedLock,
) -> io::Result<Watch> {
    Watch::start(adapter, pinned_check(adapter, selection, pinned)).map_err(native_category)
}

/// Runs on the native change-notice thread: the same adapter, attachment, peer, and on-link route
/// revalidation the session performs. Anything unreadable, or a notice before the first snapshot,
/// reads as a change.
fn pinned_check(
    initial: &Adapter,
    selection: &NetworkSelection,
    pinned: &PinnedLock,
) -> network_watch::PinnedCheck {
    let initial = initial.clone();
    let selection = selection.clone();
    let pinned = Arc::clone(pinned);
    Box::new(move || {
        let current = observe_selected_adapter(&initial, &selection)
            .and_then(|observed| interface_snapshot(&observed));
        let peer = *selection.peer.ip();
        let route = current.as_ref().ok().map(|current| {
            validate_peer(current, peer)
                .map_err(policy_category)
                .and_then(|()| route_snapshot(current, peer))
        });
        let Ok(mut lock) = pinned.lock() else {
            return false;
        };
        pinned_facts_hold(lock.as_mut(), current.ok().as_ref(), route)
    })
}

fn pinned_facts_hold(
    lock: Option<&mut NetworkLock>,
    current: Option<&InterfaceSnapshot>,
    route: Option<io::Result<RouteSnapshot>>,
) -> bool {
    match (lock, current, route) {
        (Some(lock), Some(current), Some(Ok(route))) => {
            lock.revalidate(Some(current), route).is_ok()
        }
        _ => false,
    }
}

fn observe_selected_adapter(
    initial: &Adapter,
    selection: &NetworkSelection,
) -> io::Result<Adapter> {
    let adapters = enumerate_current().map_err(native_category)?;
    let observed = find_exact_adapter(&adapters, selection)?;
    if !same_adapter_identity(initial, &observed) {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    validate_initial_adapter(&observed)?;
    Ok(observed)
}

fn find_exact_adapter(adapters: &[Adapter], selection: &NetworkSelection) -> io::Result<Adapter> {
    let mut matches = adapters.iter().filter(|adapter| {
        adapter.stable_id == selection.stable_id
            && adapter.index == selection.interface_index
            && adapter.address == *selection.local.ip()
    });
    let adapter = matches
        .next()
        .cloned()
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    if matches.next().is_some() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(adapter)
}

fn validate_initial_adapter(adapter: &Adapter) -> io::Result<()> {
    if adapter.index == 0 || adapter.stable_id.is_empty() || adapter.address.is_unspecified() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    if !adapter.physical || !adapter.up || adapter_kind(adapter).is_none() {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    Ok(())
}

fn same_adapter_identity(initial: &Adapter, observed: &Adapter) -> bool {
    initial.stable_id == observed.stable_id
        && initial.name == observed.name
        && initial.index == observed.index
        && initial.address == observed.address
        && initial.prefix_len == observed.prefix_len
        && initial.physical == observed.physical
        && initial.up == observed.up
        && initial.ethernet == observed.ethernet
        && initial.wifi == observed.wifi
}

fn interface_snapshot(adapter: &Adapter) -> io::Result<InterfaceSnapshot> {
    validate_initial_adapter(adapter)?;
    let snapshot = InterfaceSnapshot {
        stable_id: adapter.stable_id.clone(),
        name: adapter.name.clone(),
        index: adapter.index,
        address: adapter.address,
        prefix_len: adapter.prefix_len,
        kind: adapter_kind(adapter).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?,
        is_hardware: adapter.physical,
        is_up: adapter.up,
        network_signature: adapter
            .attachment
            .clone()
            .ok_or_else(|| policy_category(PolicyError::UnknownAttachment))?,
    };
    validate_interface(&snapshot).map_err(policy_category)?;
    Ok(snapshot)
}

fn adapter_kind(adapter: &Adapter) -> Option<InterfaceKind> {
    match (adapter.ethernet, adapter.wifi) {
        (true, false) => Some(InterfaceKind::Ethernet),
        (false, true) => Some(InterfaceKind::WiFi),
        _ => None,
    }
}

fn route_snapshot(selected: &InterfaceSnapshot, peer: Ipv4Addr) -> io::Result<RouteSnapshot> {
    let route = network::best_route(selected.address, peer).map_err(route_check_error)?;
    Ok(RouteSnapshot {
        interface_index: route.interface_index,
        source: route.source,
        next_hop: route.next_hop,
    })
}

fn route_check_error(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), RouteCheckFailure)
}

fn validate_exact_bind(address: SocketAddrV4) -> io::Result<()> {
    if address.ip().is_unspecified() || address.port() == 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(())
}

fn verify_bound_address(socket: &UdpSocket, selected: SocketAddrV4) -> io::Result<()> {
    if socket.local_addr().map_err(native_category)? != SocketAddr::V4(selected) {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(())
}

fn require_active(signal: &RevocationSignal, cancel: Option<&RevocationSignal>) -> io::Result<()> {
    if signal.is_revoked() || cancel.is_some_and(RevocationSignal::is_revoked) {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    Ok(())
}

fn require_cancel_active(cancel: Option<&RevocationSignal>) -> io::Result<()> {
    if cancel.is_some_and(RevocationSignal::is_revoked) {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    Ok(())
}

fn native_category(error: io::Error) -> io::Error {
    io::Error::from(error.kind())
}

fn policy_category(error: PolicyError) -> io::Error {
    let kind = match error {
        PolicyError::InterfaceChanged | PolicyError::SessionClosed => {
            io::ErrorKind::ConnectionAborted
        }
        _ => io::ErrorKind::InvalidInput,
    };
    io::Error::new(kind, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_failures_keep_their_stage_and_kind_without_platform_details() {
        for kind in [io::ErrorKind::Other, io::ErrorKind::PermissionDenied] {
            let error = route_check_error(io::Error::new(kind, "private platform details"));
            assert_eq!(error.kind(), kind);
            assert!(
                error
                    .get_ref()
                    .is_some_and(|cause| cause.is::<RouteCheckFailure>())
            );
            assert_eq!(
                error.to_string(),
                "the selected physical route could not be verified"
            );
            assert!(!error.to_string().contains("private"));
        }
        let other = native_category(io::Error::other("private platform details"));
        assert!(
            !other
                .get_ref()
                .is_some_and(|cause| cause.is::<RouteCheckFailure>())
        );
    }

    #[cfg(target_os = "macos")]
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    fn selection() -> NetworkSelection {
        NetworkSelection {
            stable_id: "selected-adapter".to_owned(),
            interface_index: 7,
            local: SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 10), 49_152),
            peer: SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 11), 49_153),
        }
    }

    fn adapter() -> Adapter {
        Adapter {
            stable_id: "selected-adapter".to_owned(),
            name: "physical0".to_owned(),
            index: 7,
            address: Ipv4Addr::new(192, 168, 50, 10),
            prefix_len: 24,
            physical: true,
            up: true,
            ethernet: true,
            wifi: false,
            attachment: Some(vec![1; 32]),
        }
    }

    fn route() -> RouteSnapshot {
        RouteSnapshot {
            interface_index: 7,
            source: Ipv4Addr::new(192, 168, 50, 10),
            next_hop: Ipv4Addr::UNSPECIFIED,
        }
    }

    #[test]
    fn exact_adapter_lookup_rejects_absence_and_ambiguity() {
        let selection = selection();
        assert_eq!(
            find_exact_adapter(&[], &selection)
                .err()
                .expect("missing adapter must fail")
                .kind(),
            io::ErrorKind::NotFound
        );
        let adapter = adapter();
        assert_eq!(
            find_exact_adapter(&[adapter.clone(), adapter], &selection)
                .err()
                .expect("ambiguous adapter must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_change_notice_keeps_the_session_only_while_adapter_attachment_and_route_hold() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let peer = Ipv4Addr::new(192, 168, 50, 11);
        let lock = NetworkLock::new(selected.clone(), peer, route(), false).unwrap();

        let mut unchanged = lock.clone();
        assert!(pinned_facts_hold(
            Some(&mut unchanged),
            Some(&selected),
            Some(Ok(route()))
        ));
        assert!(pinned_facts_hold(
            Some(&mut unchanged),
            Some(&selected),
            Some(Ok(route()))
        ));
        assert!(!unchanged.is_revoked());

        let mut other_network = lock.clone();
        let mut moved = selected.clone();
        moved.network_signature = vec![2; 32];
        assert!(!pinned_facts_hold(
            Some(&mut other_network),
            Some(&moved),
            Some(Ok(route()))
        ));
        assert!(other_network.is_revoked());

        let mut gateway = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut gateway),
            Some(&selected),
            Some(Ok(RouteSnapshot {
                next_hop: Ipv4Addr::new(192, 168, 50, 1),
                ..route()
            }))
        ));
        assert!(gateway.is_revoked());

        let mut other_interface = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut other_interface),
            Some(&selected),
            Some(Ok(RouteSnapshot {
                interface_index: 9,
                ..route()
            }))
        ));

        let mut unreadable = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut unreadable),
            Some(&selected),
            Some(Err(io::Error::from(io::ErrorKind::Other)))
        ));
        assert!(!pinned_facts_hold(Some(&mut lock.clone()), None, None));
        assert!(!pinned_facts_hold(None, Some(&selected), Some(Ok(route()))));

        let mut revoked = lock;
        revoked.revalidate(None, route()).unwrap_err();
        assert!(!pinned_facts_hold(
            Some(&mut revoked),
            Some(&selected),
            Some(Ok(route()))
        ));
    }

    #[test]
    fn snapshot_requires_current_attachment_and_maps_platform_fields() {
        let adapter = adapter();
        let snapshot = interface_snapshot(&adapter).unwrap();
        assert_eq!(snapshot.stable_id, adapter.stable_id);
        assert_eq!(snapshot.kind, InterfaceKind::Ethernet);
        assert_eq!(snapshot.network_signature, vec![1; 32]);

        let without_attachment = Adapter {
            attachment: None,
            ..adapter
        };
        assert_eq!(
            interface_snapshot(&without_attachment).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn changed_adapter_identity_revokes_revalidation() {
        let adapter = adapter();
        let selected = interface_snapshot(&adapter).unwrap();
        let peer = Ipv4Addr::new(192, 168, 50, 11);
        let mut lock = NetworkLock::new(selected, peer, route(), false).unwrap();
        let changed = Adapter {
            name: "reused-index".to_owned(),
            ..adapter.clone()
        };
        let changed_kind = Adapter {
            ethernet: false,
            wifi: true,
            ..adapter.clone()
        };

        assert!(!same_adapter_identity(&adapter, &changed));
        assert!(!same_adapter_identity(&adapter, &changed_kind));
        let current = interface_snapshot(&changed).unwrap();
        assert_eq!(
            lock.revalidate(Some(&current), route()),
            Err(PolicyError::InterfaceChanged)
        );
        assert!(lock.is_revoked());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_request_gates_bind_pin_revalidation_and_connect() {
        let events = Rc::new(RefCell::new(Vec::new()));
        assert!(
            request_with_gates(
                {
                    let events = events.clone();
                    move || {
                        events.borrow_mut().push("active");
                        Ok(())
                    }
                },
                {
                    let events = events.clone();
                    move || {
                        events.borrow_mut().push("bind");
                        Ok(())
                    }
                },
                {
                    let events = events.clone();
                    move |_| {
                        events.borrow_mut().push("pin");
                        Ok(())
                    }
                },
                {
                    let events = events.clone();
                    move || {
                        events.borrow_mut().push("revalidate");
                        Ok(())
                    }
                },
                {
                    let events = events.clone();
                    move |_| {
                        events.borrow_mut().push("connect");
                        Ok(())
                    }
                },
            )
            .is_ok()
        );
        assert_eq!(
            events.borrow().as_slice(),
            [
                "active",
                "bind",
                "active",
                "pin",
                "active",
                "revalidate",
                "active",
                "connect",
                "active",
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_request_cancellation_before_bind_skips_socket_creation() {
        let bound = Cell::new(false);
        let result = request_with_gates(
            || Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
            || {
                bound.set(true);
                Ok(())
            },
            |_| Ok(()),
            || Ok(()),
            |_| Ok(()),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert!(!bound.get());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_request_cancellation_before_connect_skips_connect() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let checks = Cell::new(0);
        let result = request_with_gates(
            || {
                let count = checks.get() + 1;
                checks.set(count);
                events.borrow_mut().push("active");
                if count == 4 {
                    Err(io::Error::from(io::ErrorKind::ConnectionAborted))
                } else {
                    Ok(())
                }
            },
            || {
                events.borrow_mut().push("bind");
                Ok(())
            },
            |_| {
                events.borrow_mut().push("pin");
                Ok(())
            },
            || {
                events.borrow_mut().push("revalidate");
                Ok(())
            },
            |_| {
                events.borrow_mut().push("connect");
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            events.borrow().as_slice(),
            [
                "active",
                "bind",
                "active",
                "pin",
                "active",
                "revalidate",
                "active"
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_request_reports_cancellation_after_connect() {
        let checks = Cell::new(0);
        let connected = Cell::new(false);
        let result = request_with_gates(
            || {
                let count = checks.get() + 1;
                checks.set(count);
                if count == 5 {
                    Err(io::Error::from(io::ErrorKind::ConnectionAborted))
                } else {
                    Ok(())
                }
            },
            || Ok(()),
            |_| Ok(()),
            || Ok(()),
            |_| {
                connected.set(true);
                Ok(())
            },
        );
        assert!(connected.get());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn permission_request_skips_connect_when_revalidation_fails() {
        let connected = Cell::new(false);
        let result = request_with_gates(
            || Ok(()),
            || Ok(()),
            |_| Ok(()),
            || Err(io::Error::from(io::ErrorKind::InvalidInput)),
            |_| {
                connected.set(true);
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(!connected.get());
    }
}
