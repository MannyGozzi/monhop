//! Native socket preparation for a fixed set of selected, directly connected private peers.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use monhop_core::RevocationSignal;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::policy::{
    InterfaceKind, InterfaceSnapshot, NetworkLock, PeerRoute, PolicyError, RouteSnapshot,
    validate_interface, validate_peer,
};

#[cfg(target_os = "macos")]
use super::NetworkSelection;
use super::{PinnedNetwork, RouteCheckFailure, socket::Reachability};

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
    pub reachability: Arc<Reachability>,
    pub signal: RevocationSignal,
    pub watch: Watch,
    pub readmission: Option<Readmission>,
}

struct PreparedSelection {
    initial: Adapter,
    lock: NetworkLock,
    reachability: Arc<Reachability>,
    signal: RevocationSignal,
    watch: Watch,
    check: SharedCheck,
}

/// `network` is the runtime that will drive the endpoint; it hosts the readmission timer.
pub(super) fn prepare(pinned: &PinnedNetwork, network: &Handle) -> io::Result<PreparedNetwork> {
    let PreparedSelection {
        initial,
        mut lock,
        reachability,
        signal,
        watch,
        check,
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
    reachability.set(lock.reachable());
    let readmission = Readmission::for_group(network, check, &signal, &reachability);

    Ok(PreparedNetwork {
        socket,
        lock,
        reachability,
        signal,
        watch,
        readmission,
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
        ..
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
type PeerRoutes = Vec<(Ipv4Addr, PeerRoute)>;

fn prepare_selection(
    pinned: &PinnedNetwork,
    cancel: Option<&RevocationSignal>,
) -> io::Result<PreparedSelection> {
    validate_exact_bind(pinned.local)?;
    require_cancel_active(cancel)?;
    let initial = find_exact_adapter(&enumerate_initial().map_err(native_category)?, pinned)?;
    require_cancel_active(cancel)?;
    validate_initial_adapter(&initial)?;

    let reachability = Reachability::new(pinned.peers.len());
    let watched: PinnedLock = Arc::new(Watched {
        lock: Mutex::new(None),
        reachability: reachability.clone(),
    });
    let check = pinned_check(&initial, pinned, &watched);
    let watch = start_watch(&initial, &check)?;
    let signal = watch.revocation_signal();
    require_active(&signal, cancel)?;

    let (selected, routes) = current_selection(&initial, pinned, &signal, cancel)?;
    let lock = NetworkLock::new_group(selected, &routes, false).map_err(policy_category)?;
    {
        let mut watched_lock = watched
            .lock
            .lock()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        reachability.set(lock.reachable());
        *watched_lock = Some(lock.clone());
    }
    require_active(&signal, cancel)?;

    Ok(PreparedSelection {
        initial,
        lock,
        reachability,
        signal,
        watch,
        check,
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
    let sole = pinned.peers.len() == 1;
    let mut routes = Vec::with_capacity(pinned.peers.len());
    for peer in &pinned.peers {
        routes.push(peer_route(&selected, *peer.ip(), sole)?);
        require_active(signal, cancel)?;
    }
    Ok((selected, routes))
}

/// Every peer must be on the selected subnet before its route is even read.
fn peer_route(
    selected: &InterfaceSnapshot,
    peer: Ipv4Addr,
    sole: bool,
) -> io::Result<(Ipv4Addr, PeerRoute)> {
    validate_peer(selected, peer).map_err(policy_category)?;
    Ok((peer, member_route(route_snapshot(selected, peer), sole)?))
}

/// A route the platform reports unreachable sets aside only that member. A lone member's route
/// failing still fails the whole check, as does every other failure: with one member there is no
/// other session to protect.
fn member_route(route: io::Result<RouteSnapshot>, sole: bool) -> io::Result<PeerRoute> {
    match route {
        Ok(route) => Ok(PeerRoute::Found(route)),
        Err(error) if !sole && unreachable_route(&error) => {
            log::debug!("network check: a member has no usable route: {error}");
            Ok(PeerRoute::Unreachable)
        }
        Err(error) => Err(route_check_error(error)),
    }
}

/// The kinds each platform's route lookup gives a peer it cannot reach right now; anything else
/// is a failed check.
fn unreachable_route(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable
    )
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

/// The watcher's private copy of the session lock, empty until the first snapshot is taken, and
/// the member reachability each check that keeps the lock hands to the socket and dialer.
struct Watched {
    lock: Mutex<Option<NetworkLock>>,
    reachability: Arc<Reachability>,
}

type PinnedLock = Arc<Watched>;

/// The one check both the change watch and the readmission timer run; `false` revokes.
pub(super) type SharedCheck = Arc<dyn Fn() -> bool + Send + Sync>;

#[cfg(target_os = "macos")]
fn start_watch(adapter: &Adapter, check: &SharedCheck) -> io::Result<Watch> {
    Watch::start_after_local_enable(&adapter.name, adapter.index, notice_check(check))
        .map_err(io::Error::other)
}

#[cfg(windows)]
fn start_watch(adapter: &Adapter, check: &SharedCheck) -> io::Result<Watch> {
    Watch::start(adapter, notice_check(check)).map_err(native_category)
}

fn notice_check(check: &SharedCheck) -> network_watch::PinnedCheck {
    let check = Arc::clone(check);
    Box::new(move || check())
}

/// What one check reads: the selected adapter, and each pinned peer's route through it.
type Observation = (Option<InterfaceSnapshot>, Option<io::Result<PeerRoutes>>);

/// The same adapter, attachment, peer, and on-link route revalidation the session performs, for
/// every pinned peer. Anything unreadable, or a check before the first snapshot, reads as a
/// change. A check that keeps the lock sets each member's reachability, so a member set aside
/// earlier is admitted again once its route is back.
fn pinned_check(initial: &Adapter, pinned: &PinnedNetwork, watched: &PinnedLock) -> SharedCheck {
    let initial = initial.clone();
    let pinned = pinned.clone();
    checking(watched, move || observe_pinned(&initial, &pinned))
}

fn observe_pinned(initial: &Adapter, pinned: &PinnedNetwork) -> Observation {
    let current = observe_selected_adapter(initial, pinned)
        .and_then(|observed| interface_snapshot(&observed))
        .inspect_err(|error| {
            log::warn!(
                "network recheck: the selected adapter did not read back ({:?})",
                error.kind()
            );
        });
    let sole = pinned.peers.len() == 1;
    let routes = current.as_ref().ok().map(|current| {
        pinned
            .peers
            .iter()
            .map(|peer| peer_route(current, *peer.ip(), sole))
            .collect::<io::Result<PeerRoutes>>()
    });
    (current.ok(), routes)
}

/// A check that applies what `observe` reads to the watched lock.
fn checking(
    watched: &PinnedLock,
    observe: impl Fn() -> Observation + Send + Sync + 'static,
) -> SharedCheck {
    let watched = Arc::clone(watched);
    Arc::new(move || {
        let (current, routes) = observe();
        recheck(&watched, current.as_ref(), routes)
    })
}

/// How long after a member is set aside, and then how often, the timer reruns the check.
const READMIT_INTERVAL: Duration = Duration::from_secs(2);

/// Reruns the change watch's check while any member is set aside, because nothing may announce
/// that member's route coming back: macOS reports no neighbor-entry change, for one. Dropping it
/// stops the timer.
pub(super) struct Readmission(AbortHandle);

impl Readmission {
    /// None for a lone member: it is never set aside, so there is nothing to readmit.
    fn for_group(
        network: &Handle,
        check: SharedCheck,
        signal: &RevocationSignal,
        reachability: &Arc<Reachability>,
    ) -> Option<Self> {
        (reachability.len() > 1).then(|| {
            Self::start(
                network,
                check,
                signal.clone(),
                Arc::clone(reachability),
                READMIT_INTERVAL,
            )
        })
    }

    /// The first check runs `interval` after a member is set aside, then one every `interval`
    /// until every member is back or `signal` is revoked. Checks run on `network`'s blocking
    /// pool, never on the workers driving the endpoint.
    pub(super) fn start(
        network: &Handle,
        check: SharedCheck,
        signal: RevocationSignal,
        reachability: Arc<Reachability>,
        interval: Duration,
    ) -> Self {
        Self(
            network
                .spawn(readmit(check, signal, reachability, interval))
                .abort_handle(),
        )
    }
}

impl Drop for Readmission {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn readmit(
    check: SharedCheck,
    signal: RevocationSignal,
    reachability: Arc<Reachability>,
    interval: Duration,
) {
    loop {
        reachability.some_set_aside().await;
        tokio::time::sleep(interval).await;
        if signal.is_revoked() {
            return;
        }
        if reachability.reaches_all() {
            continue;
        }
        let timed = {
            let (check, signal) = (check.clone(), signal.clone());
            move || timed_recheck(&check, &signal)
        };
        match tokio::task::spawn_blocking(timed).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(_) => {
                log::warn!("network watch: a timed recheck could not run; revoked");
                signal.revoke();
                return;
            }
        }
    }
}

/// Runs `check` as the change watch runs it on a notice: never once revoked, and a check that
/// fails or panics revokes.
fn timed_recheck(check: &SharedCheck, signal: &RevocationSignal) -> bool {
    if signal.is_revoked() {
        return false;
    }
    let holds = catch_unwind(AssertUnwindSafe(|| check())).unwrap_or_else(|payload| {
        std::mem::forget(payload);
        false
    });
    if !holds {
        log::warn!("network watch: a timed recheck found the pinned network changed; revoked");
        signal.revoke();
    }
    holds
}

fn recheck(
    watched: &Watched,
    current: Option<&InterfaceSnapshot>,
    routes: Option<io::Result<PeerRoutes>>,
) -> bool {
    let Ok(mut lock) = watched.lock.lock() else {
        return false;
    };
    let holds = pinned_facts_hold(lock.as_mut(), current, routes);
    if holds && let Some(lock) = lock.as_ref() {
        watched.reachability.set(lock.reachable());
    }
    holds
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
    let route = network::best_route(selected.address, peer)?;
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
    use std::time::Instant;

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
        vec![(PEER, route.into())]
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

    fn member_routes() -> PeerRoutes {
        [11, 12, 13]
            .map(|host| (Ipv4Addr::new(192, 168, 50, host), route().into()))
            .to_vec()
    }

    fn gateway() -> PeerRoute {
        PeerRoute::Found(RouteSnapshot {
            next_hop: Ipv4Addr::new(192, 168, 50, 1),
            ..route()
        })
    }

    /// The watcher's state after a bind that found `routes`.
    fn watching(selected: &InterfaceSnapshot, routes: &PeerRoutes) -> Watched {
        let lock = NetworkLock::new_group(selected.clone(), routes, false).unwrap();
        let reachability = Reachability::new(routes.len());
        reachability.set(lock.reachable());
        Watched {
            lock: Mutex::new(Some(lock)),
            reachability,
        }
    }

    fn reaches(watched: &Watched) -> Vec<bool> {
        (0..watched.reachability.len())
            .map(|member| watched.reachability.reaches(member))
            .collect()
    }

    fn revoked(watched: &Watched) -> bool {
        watched
            .lock
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(NetworkLock::is_revoked)
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
        let members = member_routes();
        let lock = NetworkLock::new_group(selected.clone(), &members, false).unwrap();
        let mut unchanged = lock.clone();
        assert!(pinned_facts_hold(
            Some(&mut unchanged),
            Some(&selected),
            Some(Ok(members.clone()))
        ));
        for member in 0..members.len() {
            let mut routed = members.clone();
            routed[member].1 = gateway();
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
    fn a_member_route_failure_isolates_only_that_member() {
        // Only the kinds a platform gives an unreachable peer set a member aside.
        for kind in [
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::NetworkUnreachable,
        ] {
            assert_eq!(
                member_route(Err(io::Error::from(kind)), false).unwrap(),
                PeerRoute::Unreachable
            );
        }
        for kind in [io::ErrorKind::Other, io::ErrorKind::NotFound] {
            let error = member_route(Err(io::Error::from(kind)), false).unwrap_err();
            assert!(
                error
                    .get_ref()
                    .is_some_and(|cause| cause.is::<RouteCheckFailure>())
            );
        }

        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let watched = watching(&selected, &members);
        let mut second_down = members.clone();
        second_down[1].1 = PeerRoute::Unreachable;
        assert!(recheck(&watched, Some(&selected), Some(Ok(second_down))));
        assert_eq!(reaches(&watched), [true, false, true]);
        assert!(!revoked(&watched));

        let mut outer_down = members.clone();
        outer_down[0].1 = PeerRoute::Unreachable;
        outer_down[2].1 = PeerRoute::Unreachable;
        assert!(recheck(&watched, Some(&selected), Some(Ok(outer_down))));
        assert_eq!(reaches(&watched), [false, true, false]);

        // The next check that finds their routes admits them again.
        assert!(recheck(
            &watched,
            Some(&selected),
            Some(Ok(members.clone()))
        ));
        assert_eq!(reaches(&watched), [true, true, true]);
        assert!(!revoked(&watched));

        // A bind may start with a member already set aside.
        let mut bound_down = members;
        bound_down[2].1 = PeerRoute::Unreachable;
        assert_eq!(
            reaches(&watching(&selected, &bound_down)),
            [true, true, false]
        );
    }

    #[test]
    fn an_interface_change_still_revokes_every_member() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let mut set_aside = member_routes();
        set_aside[1].1 = PeerRoute::Unreachable;
        let mut other_network = selected.clone();
        other_network.network_signature = vec![2; 32];
        let mut renumbered = selected.clone();
        renumbered.index = 9;
        let mut readdressed = selected.clone();
        readdressed.address = Ipv4Addr::new(192, 168, 50, 20);
        let mut resubnetted = selected.clone();
        resubnetted.prefix_len = 16;
        for current in [other_network, renumbered, readdressed, resubnetted] {
            let watched = watching(&selected, &set_aside);
            assert!(!recheck(
                &watched,
                Some(&current),
                Some(Ok(set_aside.clone()))
            ));
            assert!(revoked(&watched));
        }

        // A found route stays exactly as strict while another member is set aside.
        let elsewhere = PeerRoute::Found(RouteSnapshot {
            interface_index: 9,
            ..route()
        });
        let other_source = PeerRoute::Found(RouteSnapshot {
            source: Ipv4Addr::new(192, 168, 50, 20),
            ..route()
        });
        for found in [gateway(), elsewhere, other_source] {
            let watched = watching(&selected, &set_aside);
            let mut routes = set_aside.clone();
            routes[2].1 = found;
            assert!(!recheck(&watched, Some(&selected), Some(Ok(routes))));
            assert!(revoked(&watched));
        }

        let watched = watching(&selected, &set_aside);
        assert!(!recheck(&watched, None, None));
        let watched = watching(&selected, &set_aside);
        assert!(!recheck(
            &watched,
            Some(&selected),
            Some(Err(io::Error::from(io::ErrorKind::Other)))
        ));
    }

    #[test]
    fn a_group_of_one_still_revokes_on_its_route_failure() {
        // Its lone member's route failing fails the check, whatever the platform calls it.
        for kind in [
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::NetworkUnreachable,
            io::ErrorKind::Other,
        ] {
            let error = member_route(Err(io::Error::from(kind)), true).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(
                error
                    .get_ref()
                    .is_some_and(|cause| cause.is::<RouteCheckFailure>())
            );
            let selected = interface_snapshot(&adapter()).unwrap();
            let watched = watching(&selected, &peer_routes(route()));
            assert!(!recheck(&watched, Some(&selected), Some(Err(error))));
        }

        // The lock refuses a lone unreachable member too, at bind and on a check.
        let selected = interface_snapshot(&adapter()).unwrap();
        let alone = vec![(PEER, PeerRoute::Unreachable)];
        assert_eq!(
            NetworkLock::new_group(selected.clone(), &alone, false).unwrap_err(),
            PolicyError::PeerUnreachable
        );
        let watched = watching(&selected, &peer_routes(route()));
        assert!(!recheck(&watched, Some(&selected), Some(Ok(alone))));
        assert!(revoked(&watched));
    }

    /// Stands in for the two seconds, so the timer's cadence runs in real time.
    const INTERVAL: Duration = Duration::from_millis(20);
    const SETTLE: Duration = Duration::from_secs(5);

    /// What the next check reads in place of the native adapter and route lookups, and when each
    /// check read it.
    struct Network {
        current: Mutex<(Option<InterfaceSnapshot>, PeerRoutes)>,
        reads: Mutex<Vec<Instant>>,
    }

    impl Network {
        fn observe(&self) -> Observation {
            self.reads.lock().unwrap().push(Instant::now());
            let (current, routes) = self.current.lock().unwrap().clone();
            let routes = current.is_some().then_some(Ok(routes));
            (current, routes)
        }

        fn set(&self, current: Option<&InterfaceSnapshot>, routes: &PeerRoutes) {
            *self.current.lock().unwrap() = (current.cloned(), routes.clone());
        }

        fn reads(&self) -> Vec<Instant> {
            self.reads.lock().unwrap().clone()
        }
    }

    /// A group bound with `routes`, the check its watch and timer share, and the running timer.
    struct Readmitting {
        watched: PinnedLock,
        network: Arc<Network>,
        check: SharedCheck,
        signal: RevocationSignal,
        _timer: Readmission,
    }

    impl Readmitting {
        fn bound(selected: &InterfaceSnapshot, routes: &PeerRoutes) -> Self {
            let watched = Arc::new(watching(selected, routes));
            let network = Arc::new(Network {
                current: Mutex::new((Some(selected.clone()), routes.clone())),
                reads: Mutex::default(),
            });
            let reading = Arc::clone(&network);
            let check = checking(&watched, move || reading.observe());
            let signal = RevocationSignal::default();
            let timer = Readmission::start(
                &Handle::current(),
                Arc::clone(&check),
                signal.clone(),
                Arc::clone(&watched.reachability),
                INTERVAL,
            );
            Self {
                watched,
                network,
                check,
                signal,
                _timer: timer,
            }
        }

        /// A change notice that reads `routes`: the watch runs the shared check once.
        fn notice(&self, selected: &InterfaceSnapshot, routes: &PeerRoutes) {
            self.network.set(Some(selected), routes);
            assert!((self.check)());
        }

        fn checks(&self) -> usize {
            self.network.reads().len()
        }
    }

    fn with_down(members: &PeerRoutes, down: &[usize]) -> PeerRoutes {
        let mut routes = members.clone();
        for &member in down {
            routes[member].1 = PeerRoute::Unreachable;
        }
        routes
    }

    async fn until(what: &str, condition: impl Fn() -> bool) {
        tokio::time::timeout(SETTLE, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    #[tokio::test]
    async fn a_member_marked_unreachable_is_readmitted_on_the_next_timed_recheck_without_a_notice()
    {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let group = Readmitting::bound(&selected, &members);
        let marked = Instant::now();
        group.notice(&selected, &with_down(&members, &[1]));
        assert_eq!(reaches(&group.watched), [true, false, true]);

        // Its route resolves again, and no notice says so.
        group.network.set(Some(&selected), &members);
        until("the timer readmits the member", || {
            reaches(&group.watched) == [true, true, true]
        })
        .await;
        let reads = group.network.reads();
        assert!(reads[1] >= marked + INTERVAL, "the first timed check waits");
        assert!(!group.signal.is_revoked());
        assert!(!revoked(&group.watched));
    }

    #[tokio::test]
    async fn the_timer_stops_once_every_member_is_reachable() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let group = Readmitting::bound(&selected, &members);
        group.notice(&selected, &with_down(&members, &[2]));
        group.network.set(Some(&selected), &members);
        until("the timer readmits the member", || {
            reaches(&group.watched) == [true, true, true]
        })
        .await;
        let settled = group.checks();
        tokio::time::sleep(INTERVAL * 10).await;
        assert_eq!(group.checks(), settled);

        // A later member set aside starts it again.
        group.notice(&selected, &with_down(&members, &[0]));
        until("the timer rechecks", || group.checks() > settled + 1).await;
        assert_eq!(reaches(&group.watched), [false, true, true]);
        group.network.set(Some(&selected), &members);
        until("the timer readmits the member", || {
            reaches(&group.watched) == [true, true, true]
        })
        .await;
        assert!(!group.signal.is_revoked());
    }

    #[tokio::test]
    async fn a_timed_recheck_that_finds_an_interface_change_revokes() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let mut other_network = selected.clone();
        other_network.network_signature = vec![2; 32];
        let mut renumbered = selected.clone();
        renumbered.index = 9;
        let mut readdressed = selected.clone();
        readdressed.address = Ipv4Addr::new(192, 168, 50, 20);
        let mut resubnetted = selected.clone();
        resubnetted.prefix_len = 16;
        let unreadable = None;
        for current in [
            Some(other_network),
            Some(renumbered),
            Some(readdressed),
            Some(resubnetted),
            unreadable,
        ] {
            let group = Readmitting::bound(&selected, &members);
            group.notice(&selected, &with_down(&members, &[1]));
            // Every route is back, on an adapter that no longer reads back as selected.
            group.network.set(current.as_ref(), &members);
            until("the timed check revokes", || group.signal.is_revoked()).await;
            assert_eq!(reaches(&group.watched), [true, false, true]);
            assert_eq!(revoked(&group.watched), current.is_some());
            let stopped = group.checks();
            tokio::time::sleep(INTERVAL * 5).await;
            assert_eq!(group.checks(), stopped);
        }
    }

    #[tokio::test]
    async fn a_still_unreachable_member_stays_set_aside() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let down = with_down(&members, &[1]);
        let group = Readmitting::bound(&selected, &members);
        group.notice(&selected, &down);
        until("the timer rechecks three times", || group.checks() >= 4).await;
        assert_eq!(reaches(&group.watched), [true, false, true]);
        assert!(!group.signal.is_revoked());
        assert!(!revoked(&group.watched));

        // A route that comes back off-link or elsewhere readmits no one: it revokes everything.
        let elsewhere = PeerRoute::Found(RouteSnapshot {
            interface_index: 9,
            ..route()
        });
        let other_source = PeerRoute::Found(RouteSnapshot {
            source: Ipv4Addr::new(192, 168, 50, 20),
            ..route()
        });
        for found in [gateway(), elsewhere, other_source] {
            let group = Readmitting::bound(&selected, &members);
            group.notice(&selected, &down);
            let mut back = members.clone();
            back[1].1 = found;
            group.network.set(Some(&selected), &back);
            until("the timed check revokes", || group.signal.is_revoked()).await;
            assert_eq!(reaches(&group.watched), [true, false, true]);
            assert!(revoked(&group.watched));
        }
    }

    #[tokio::test]
    async fn a_group_of_one_has_nothing_to_readmit() {
        let check: SharedCheck = Arc::new(|| true);
        let signal = RevocationSignal::default();
        let network = Handle::current();
        let alone = Reachability::new(1);
        assert!(Readmission::for_group(&network, check.clone(), &signal, &alone).is_none());
        let group = Reachability::new(2);
        assert!(Readmission::for_group(&network, check, &signal, &group).is_some());
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
