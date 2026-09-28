//! Native socket preparation for a fixed set of selected, directly connected private peers.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use monhop_core::RevocationSignal;

use crate::policy::{
    InterfaceKind, InterfaceSnapshot, NetworkLock, PolicyError, RouteSnapshot, validate_interface,
    validate_peer,
};

#[cfg(target_os = "macos")]
use super::NetworkSelection;
use super::{PinnedNetwork, RouteCheckFailure};

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

/// The session socket, owning on Windows the flow that keeps its datagrams marked interactive.
pub(super) struct NativeSocket {
    // Declared first so the flow is removed while its socket is still open.
    #[cfg(windows)]
    _traffic: Option<network::InteractiveTraffic>,
    receiver: udp_receive::AsyncUdpReceiver,
}

impl NativeSocket {
    pub(super) fn poll_receive(
        &self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<udp_receive::ReceivedDatagram>> {
        self.receiver.poll_receive(cx, buffer)
    }

    pub(super) fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
        self.receiver.try_send_to(buffer, peer)
    }

    pub(super) async fn writable(&self) -> io::Result<()> {
        self.receiver.writable().await
    }

    pub(super) fn local_addr(&self) -> io::Result<SocketAddrV4> {
        self.receiver.local_addr()
    }
}

impl From<udp_receive::AsyncUdpReceiver> for NativeSocket {
    fn from(receiver: udp_receive::AsyncUdpReceiver) -> Self {
        Self {
            receiver,
            #[cfg(windows)]
            _traffic: None,
        }
    }
}

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

pub(super) fn prepare(pinned: &PinnedNetwork) -> io::Result<PreparedNetwork> {
    let PreparedSelection {
        initial,
        mut lock,
        signal,
        watch,
    } = prepare_selection(pinned, None)?;

    require_active(&signal, None)?;
    let socket = UdpSocket::bind(pinned.local).map_err(native_category)?;
    verify_bound_address(&socket, pinned.local)?;
    network::restrict_udp_interface(&socket, pinned.interface_index).map_err(native_category)?;
    // Wi-Fi queues marked datagrams ahead of bulk traffic; a refusal only costs that priority.
    #[cfg(target_os = "macos")]
    let _ = network::mark_interactive_traffic(&socket).inspect_err(warn_unmarked);
    #[cfg(windows)]
    let traffic = network::mark_interactive_traffic(&socket, &pinned.peers)
        .inspect_err(warn_unmarked)
        .ok();
    let socket = udp_receive::UdpReceiver::configure(socket).map_err(native_category)?;
    let socket = NativeSocket {
        receiver: socket.into_async().map_err(native_category)?,
        #[cfg(windows)]
        _traffic: traffic,
    };
    if socket.local_addr().map_err(native_category)? != pinned.local {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }

    revalidate_selection(&initial, pinned, &mut lock, &signal, None)?;

    Ok(PreparedNetwork {
        socket,
        lock,
        signal,
        watch,
    })
}

fn warn_unmarked(error: &io::Error) {
    log::warn!("session datagrams keep ordinary Wi-Fi priority: {error}");
}

#[cfg(target_os = "macos")]
pub(super) fn request_local_network_access_after_local_action(
    selection: &NetworkSelection,
    cancel: &RevocationSignal,
) -> io::Result<()> {
    let pinned = PinnedNetwork::new(
        &selection.stable_id,
        selection.interface_index,
        selection.local,
        &[selection.peer],
    )?;
    let PreparedSelection {
        initial,
        mut lock,
        signal,
        watch: _watch,
    } = prepare_selection(&pinned, Some(cancel))?;

    request_with_gates(
        || require_active(&signal, Some(cancel)),
        || UdpSocket::bind(selection.local).map_err(native_category),
        |socket| {
            verify_bound_address(socket, selection.local)?;
            network::restrict_udp_interface(socket, selection.interface_index)
                .map_err(native_category)
        },
        || revalidate_selection(&initial, &pinned, &mut lock, &signal, Some(cancel)),
        |socket| socket.connect(selection.peer).map_err(native_category),
    )
}

/// Each pinned peer with its current route, in the pinned order.
type PeerRoutes = Vec<(Ipv4Addr, RouteSnapshot)>;

fn prepare_selection(
    pinned: &PinnedNetwork,
    cancel: Option<&RevocationSignal>,
) -> io::Result<PreparedSelection> {
    validate_exact_bind(pinned.local)?;
    require_cancel_active(cancel)?;
    let initial = find_exact_adapter(&enumerate_initial().map_err(native_category)?, pinned)?;
    require_cancel_active(cancel)?;
    validate_initial_adapter(&initial)?;

    let watched: PinnedLock = Arc::default();
    let watch = start_watch(&initial, pinned, &watched)?;
    let signal = watch.revocation_signal();
    require_active(&signal, cancel)?;

    let (selected, routes) = current_selection(&initial, pinned, &signal, cancel)?;
    let lock = NetworkLock::new_group(selected, &routes, false).map_err(policy_category)?;
    *watched
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
    pinned: &PinnedNetwork,
    lock: &mut NetworkLock,
    signal: &RevocationSignal,
    cancel: Option<&RevocationSignal>,
) -> io::Result<()> {
    let (current, routes) = current_selection(initial, pinned, signal, cancel)?;
    lock.revalidate(Some(&current), &routes)
        .map_err(policy_category)?;
    require_active(signal, cancel)
}

fn current_selection(
    initial: &Adapter,
    pinned: &PinnedNetwork,
    signal: &RevocationSignal,
    cancel: Option<&RevocationSignal>,
) -> io::Result<(InterfaceSnapshot, PeerRoutes)> {
    let observed = observe_selected_adapter(initial, pinned)?;
    require_active(signal, cancel)?;
    let selected = interface_snapshot(&observed)?;
    let mut routes = Vec::with_capacity(pinned.peers.len());
    for peer in &pinned.peers {
        routes.push(peer_route(&selected, *peer.ip())?);
        require_active(signal, cancel)?;
    }
    Ok((selected, routes))
}

/// Every peer must be on the selected subnet before its route is even read.
fn peer_route(
    selected: &InterfaceSnapshot,
    peer: Ipv4Addr,
) -> io::Result<(Ipv4Addr, RouteSnapshot)> {
    validate_peer(selected, peer).map_err(policy_category)?;
    Ok((peer, route_snapshot(selected, peer)?))
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
    pinned: &PinnedNetwork,
    watched: &PinnedLock,
) -> io::Result<Watch> {
    Watch::start_after_local_enable(
        &adapter.name,
        adapter.index,
        pinned_check(adapter, pinned, watched),
    )
    .map_err(io::Error::other)
}

#[cfg(windows)]
fn start_watch(
    adapter: &Adapter,
    pinned: &PinnedNetwork,
    watched: &PinnedLock,
) -> io::Result<Watch> {
    Watch::start(adapter, pinned_check(adapter, pinned, watched)).map_err(native_category)
}

/// Runs on the native change-notice thread: the same adapter, attachment, peer, and on-link route
/// revalidation the session performs, for every pinned peer. Anything unreadable, or a notice
/// before the first snapshot, reads as a change.
fn pinned_check(
    initial: &Adapter,
    pinned: &PinnedNetwork,
    watched: &PinnedLock,
) -> network_watch::PinnedCheck {
    let initial = initial.clone();
    let pinned = pinned.clone();
    let watched = Arc::clone(watched);
    Box::new(move || {
        let current = observe_selected_adapter(&initial, &pinned)
            .and_then(|observed| interface_snapshot(&observed))
            .inspect_err(|error| {
                log::warn!(
                    "network recheck: the selected adapter did not read back ({:?})",
                    error.kind()
                );
            });
        let routes = current.as_ref().ok().map(|current| {
            pinned
                .peers
                .iter()
                .map(|peer| peer_route(current, *peer.ip()))
                .collect::<io::Result<PeerRoutes>>()
        });
        let Ok(mut lock) = watched.lock() else {
            return false;
        };
        pinned_facts_hold(lock.as_mut(), current.ok().as_ref(), routes)
    })
}

fn pinned_facts_hold(
    lock: Option<&mut NetworkLock>,
    current: Option<&InterfaceSnapshot>,
    routes: Option<io::Result<PeerRoutes>>,
) -> bool {
    let failure = match (lock, current, routes) {
        (Some(lock), Some(current), Some(Ok(routes))) => {
            match lock.revalidate(Some(current), &routes) {
                Ok(()) => return true,
                Err(error) => format!("{error:?}"),
            }
        }
        (None, ..) => "no pinned snapshot yet".to_owned(),
        (_, _, Some(Err(error))) => error.to_string(),
        _ => return false,
    };
    log::warn!("network recheck failed: {failure}");
    false
}

fn observe_selected_adapter(initial: &Adapter, pinned: &PinnedNetwork) -> io::Result<Adapter> {
    let adapters = enumerate_current().map_err(native_category)?;
    let observed = find_exact_adapter(&adapters, pinned)?;
    if !same_adapter_identity(initial, &observed) {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    validate_initial_adapter(&observed)?;
    Ok(observed)
}

fn find_exact_adapter(adapters: &[Adapter], pinned: &PinnedNetwork) -> io::Result<Adapter> {
    let mut matches = adapters.iter().filter(|adapter| {
        adapter.stable_id == pinned.stable_id
            && adapter.index == pinned.interface_index
            && adapter.address == *pinned.local.ip()
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
    // The platform message names the failed rule and carries no addresses; callers see only the kind.
    log::warn!("network check: the route to the peer failed: {error}");
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

    const PEER: Ipv4Addr = Ipv4Addr::new(192, 168, 50, 11);

    fn pinned() -> PinnedNetwork {
        PinnedNetwork {
            stable_id: "selected-adapter".to_owned(),
            interface_index: 7,
            local: SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 10), 49_152),
            peers: [SocketAddrV4::new(PEER, 49_153)].into(),
        }
    }

    fn peer_routes(route: RouteSnapshot) -> PeerRoutes {
        vec![(PEER, route)]
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
        let selection = pinned();
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
            Some(Ok(peer_routes(route())))
        ));
        assert!(pinned_facts_hold(
            Some(&mut unchanged),
            Some(&selected),
            Some(Ok(peer_routes(route())))
        ));
        assert!(!unchanged.is_revoked());

        let mut other_network = lock.clone();
        let mut moved = selected.clone();
        moved.network_signature = vec![2; 32];
        assert!(!pinned_facts_hold(
            Some(&mut other_network),
            Some(&moved),
            Some(Ok(peer_routes(route())))
        ));
        assert!(other_network.is_revoked());

        let mut gateway = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut gateway),
            Some(&selected),
            Some(Ok(peer_routes(RouteSnapshot {
                next_hop: Ipv4Addr::new(192, 168, 50, 1),
                ..route()
            })))
        ));
        assert!(gateway.is_revoked());

        let mut other_interface = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut other_interface),
            Some(&selected),
            Some(Ok(peer_routes(RouteSnapshot {
                interface_index: 9,
                ..route()
            })))
        ));

        let mut unreadable = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut unreadable),
            Some(&selected),
            Some(Err(io::Error::from(io::ErrorKind::Other)))
        ));
        assert!(!pinned_facts_hold(Some(&mut lock.clone()), None, None));
        assert!(!pinned_facts_hold(
            None,
            Some(&selected),
            Some(Ok(peer_routes(route())))
        ));

        let mut revoked = lock;
        revoked.revalidate(None, &peer_routes(route())).unwrap_err();
        assert!(!pinned_facts_hold(
            Some(&mut revoked),
            Some(&selected),
            Some(Ok(peer_routes(route())))
        ));
    }

    #[test]
    fn a_change_notice_revokes_the_whole_set_when_any_member_route_fails() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members: PeerRoutes = [11, 12, 13]
            .map(|host| (Ipv4Addr::new(192, 168, 50, host), route()))
            .to_vec();
        let lock = NetworkLock::new_group(selected.clone(), &members, false).unwrap();
        let mut unchanged = lock.clone();
        assert!(pinned_facts_hold(
            Some(&mut unchanged),
            Some(&selected),
            Some(Ok(members.clone()))
        ));
        for member in 0..members.len() {
            let mut routed = members.clone();
            routed[member].1.next_hop = Ipv4Addr::new(192, 168, 50, 1);
            let mut lock = lock.clone();
            assert!(!pinned_facts_hold(
                Some(&mut lock),
                Some(&selected),
                Some(Ok(routed))
            ));
            assert!(lock.is_revoked());
        }
        let mut missing = lock.clone();
        assert!(!pinned_facts_hold(
            Some(&mut missing),
            Some(&selected),
            Some(Ok(members[1..].to_vec()))
        ));
        assert!(missing.is_revoked());
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
            lock.revalidate(Some(&current), &peer_routes(route())),
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
