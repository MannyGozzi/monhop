//! Native socket preparation for a fixed set of selected, directly connected private peers.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use monhop_core::RevocationSignal;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::policy::{
    InterfaceKind, InterfaceSnapshot, NetworkLock, PeerRoute, PolicyError, RouteSnapshot,
    validate_interface, validate_peer,
};

#[cfg(target_os = "macos")]
use super::NetworkSelection;
use super::{ListenSelection, PinnedNetwork, RouteCheckFailure, socket::Reachability};

#[cfg(target_os = "macos")]
use monhop_platform_macos::{
    network::{self, Adapter, AdapterReading},
    network_watch, udp_receive,
};
#[cfg(windows)]
use monhop_platform_windows::{
    network::{self, Adapter, AdapterReading},
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
    /// A datagram the OS truncated and already discarded, rather than a failed socket.
    pub(super) fn is_truncated(error: &io::Error) -> bool {
        udp_receive::is_truncated(error)
    }

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
    /// The watch's own check, for the readmission timer to rerun.
    pub check: SharedCheck,
}

struct PreparedSelection {
    initial: Adapter,
    lock: NetworkLock,
    reachability: Arc<Reachability>,
    signal: RevocationSignal,
    watch: Watch,
    check: SharedCheck,
}

pub(super) fn prepare(pinned: &PinnedNetwork) -> io::Result<PreparedNetwork> {
    let PreparedSelection {
        initial,
        mut lock,
        reachability,
        signal,
        watch,
        check,
    } = prepare_selection(pinned, None)?;

    require_active(&signal, None)?;
    let socket = bind_on_interface(pinned.local, pinned.interface_index)?;
    // Wi-Fi queues marked datagrams ahead of bulk traffic; a refusal only costs that priority.
    #[cfg(target_os = "macos")]
    let _ = network::mark_interactive_traffic(&socket).inspect_err(warn_unmarked);
    #[cfg(windows)]
    let traffic = network::mark_interactive_traffic(&socket, &pinned.peers)
        .inspect_err(warn_unmarked)
        .ok();
    let socket = NativeSocket {
        receiver: receiver(socket, pinned.local)?,
        #[cfg(windows)]
        _traffic: traffic,
    };

    revalidate_selection(&initial, pinned, &mut lock, &signal, None)?;
    reachability.set(lock.reachable());

    Ok(PreparedNetwork {
        socket,
        lock,
        reachability,
        signal,
        watch,
        check,
    })
}

fn warn_unmarked(error: &io::Error) {
    log::warn!("session datagrams keep ordinary Wi-Fi priority: {error}");
}

/// A socket bound to exactly `local` and restricted to the selected interface.
fn bind_on_interface(local: SocketAddrV4, interface_index: u32) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(local).map_err(native_category)?;
    verify_bound_address(&socket, local)?;
    network::restrict_udp_interface(&socket, interface_index).map_err(native_category)?;
    Ok(socket)
}

/// The asynchronous receiver that reports each datagram's destination and arrival interface.
fn receiver(socket: UdpSocket, local: SocketAddrV4) -> io::Result<udp_receive::AsyncUdpReceiver> {
    let receiver = udp_receive::UdpReceiver::configure(socket)
        .and_then(udp_receive::UdpReceiver::into_async)
        .map_err(native_category)?;
    if receiver.local_addr().map_err(native_category)? != local {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(receiver)
}

/// A pairing listener's network: the selected adapter, bound and watched, with no peer. The
/// adapter or its attachment changing in any way revokes it.
pub(super) struct PreparedListener {
    pub socket: NativeSocket,
    pub interface: InterfaceSnapshot,
    pub signal: RevocationSignal,
    pub watch: Watch,
}

pub(super) fn prepare_listener(selection: &ListenSelection) -> io::Result<PreparedListener> {
    let key = AdapterKey::of_listener(selection);
    validate_exact_bind(selection.local)?;
    let initial = find_exact_adapter(&enumerate_initial().map_err(native_category)?, key)?;
    validate_initial_adapter(&initial)?;

    let pinned: Arc<Mutex<Option<InterfaceSnapshot>>> = Arc::default();
    let ended: Arc<OnceLock<RevocationSignal>> = Arc::default();
    let check: SharedCheck = {
        let (initial, pinned, ended) = (initial.clone(), pinned.clone(), ended.clone());
        Arc::new(move || {
            let settled = settle(
                REREADS,
                || session_ended(&ended),
                |_| listener_verdict(&pinned, read_selected(&initial)),
            );
            concluded(settled, &ended)
        })
    };
    let watch = start_watch(&initial, &check)?;
    let signal = watch.revocation_signal();
    let _ = ended.set(signal.clone());
    require_active(&signal, None)?;
    let interface = read_selected(&initial)?;
    *pinned
        .lock()
        .map_err(|_| io::Error::from(io::ErrorKind::Other))? = Some(interface.clone());
    require_active(&signal, None)?;

    let socket = bind_on_interface(selection.local, selection.interface_index)?;
    let socket = NativeSocket::from(receiver(socket, selection.local)?);
    if read_selected(&initial)? != interface {
        return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
    }
    require_active(&signal, None)?;
    Ok(PreparedListener {
        socket,
        interface,
        signal,
        watch,
    })
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
    let initial = find_exact_adapter(
        &enumerate_initial().map_err(native_category)?,
        AdapterKey::of(pinned),
    )?;
    require_cancel_active(cancel)?;
    validate_initial_adapter(&initial)?;

    let reachability = Reachability::new(pinned.peers.len());
    let watched: PinnedLock = Arc::new(Watched {
        lock: Mutex::new(None),
        reachability: reachability.clone(),
        ended: OnceLock::new(),
    });
    let check = pinned_check(&initial, pinned, &watched);
    let watch = start_watch(&initial, &check)?;
    let signal = watch.revocation_signal();
    let _ = watched.ended.set(signal.clone());
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
    let selected = read_selected(initial)?;
    require_active(signal, cancel)?;
    let sole = pinned.peers.len() == 1;
    let mut routes = Vec::with_capacity(pinned.peers.len());
    for peer in &pinned.peers {
        routes.push((
            *peer.ip(),
            read_route(&selected, *peer.ip(), sole, route_snapshot)?,
        ));
        require_active(signal, cancel)?;
    }
    Ok((selected, routes))
}

/// Why a read of the pinned network gave no whole answer.
#[derive(Debug)]
enum Unread {
    /// The read shows the selected adapter gone or different.
    Changed(io::Error),
    /// The read failed or caught the adapter between states, so it decides nothing yet.
    Unreadable(io::Error),
}

impl From<Unread> for io::Error {
    fn from(unread: Unread) -> Self {
        match unread {
            Unread::Changed(error) | Unread::Unreadable(error) => error,
        }
    }
}

/// Reads the selected adapter on its own, so another adapter coming or going cannot fail it.
fn read_selected(initial: &Adapter) -> Result<InterfaceSnapshot, Unread> {
    judge_adapter(initial, network::read_selected_adapter(initial))
}

/// An adapter that is gone or reads back as another is a change. A failed read, an adapter
/// between states, or one whose network is not identified yet is unreadable.
fn judge_adapter(
    initial: &Adapter,
    read: io::Result<AdapterReading>,
) -> Result<InterfaceSnapshot, Unread> {
    let observed = match read {
        Ok(AdapterReading::Present(observed)) => observed,
        Ok(AdapterReading::Lost) => {
            return Err(Unread::Changed(io::Error::from(io::ErrorKind::NotFound)));
        }
        #[cfg(windows)]
        Ok(AdapterReading::Settling) => {
            return Err(Unread::Unreadable(policy_category(
                PolicyError::InterfaceDown,
            )));
        }
        Err(error) => return Err(Unread::Unreadable(native_category(error))),
    };
    if !same_adapter_identity(initial, &observed) {
        return Err(Unread::Changed(io::Error::from(
            io::ErrorKind::ConnectionAborted,
        )));
    }
    if observed.attachment.is_none() {
        return Err(Unread::Unreadable(policy_category(
            PolicyError::UnknownAttachment,
        )));
    }
    interface_snapshot(&observed).map_err(Unread::Changed)
}

/// One pinned peer's route through `selected`, which it must be on the subnet of before the
/// route is even read. A route the platform answers with but the policy refuses is a change; any
/// other failed lookup is unreadable, except that a group member the platform cannot reach is
/// only set aside.
fn read_route(
    selected: &InterfaceSnapshot,
    peer: Ipv4Addr,
    sole: bool,
    lookup: impl FnOnce(&InterfaceSnapshot, Ipv4Addr) -> io::Result<RouteSnapshot>,
) -> Result<PeerRoute, Unread> {
    validate_peer(selected, peer).map_err(|error| Unread::Changed(policy_category(error)))?;
    match lookup(selected, peer) {
        Err(error) if route_refused(&error) => Err(Unread::Changed(route_check_error(error))),
        route => member_route(route, sole).map_err(Unread::Unreadable),
    }
}

fn route_refused(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<network::RouteRefused>())
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

/// The watcher's private copy of the session lock, empty until the first snapshot is taken, the
/// member reachability each check that keeps the lock hands to the socket and dialer, and the
/// watch's revocation, known once the watch has started, which cuts short a check's rereads.
struct Watched {
    lock: Mutex<Option<NetworkLock>>,
    reachability: Arc<Reachability>,
    ended: OnceLock<RevocationSignal>,
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

/// What one whole read finds: the selected adapter, and each pinned peer's route through it.
type Observed = (InterfaceSnapshot, PeerRoutes);

/// How long a check may keep reading a network it could not read whole, counted from its first
/// read, and how long it waits between reads. No read starts once the window has ended, but a
/// read already under way is not cut short, so a check takes at most the window plus one read.
#[derive(Clone, Copy)]
struct Rereads {
    window: Duration,
    interval: Duration,
}

/// A network caught between states gets 1 s plus one read to read back whole, with reads 300 ms
/// apart. Only a macOS route lookup gives up when that second ends; adapter, Wi-Fi and Windows
/// route reads run to completion.
const REREADS: Rereads = Rereads {
    window: Duration::from_secs(1),
    interval: Duration::from_millis(300),
};

/// How often a wait between reads looks for the session ending.
const REREAD_POLL: Duration = Duration::from_millis(20);

/// What one read decided; a read that revokes says why, for the log once the session is revoked.
#[derive(Debug)]
enum Verdict {
    Holds,
    Revokes(String),
    Undecided(io::Error),
}

/// How a check ended.
enum Settled {
    Holds,
    /// What to log once the session is revoked.
    Revokes(String),
    /// The session ended first; there is nothing to report.
    Ended,
}

/// The same adapter, attachment, peer, and on-link route revalidation the session performs, for
/// every pinned peer. A read that shows a change revokes at once; one that cannot read the network
/// whole is read again, and revokes once the `REREADS` window ends. A check before the first
/// snapshot revokes. A check that keeps the lock sets each member's reachability, so a member set
/// aside earlier is admitted again once its route is back.
fn pinned_check(initial: &Adapter, pinned: &PinnedNetwork, watched: &PinnedLock) -> SharedCheck {
    let initial = initial.clone();
    let pinned = pinned.clone();
    checking(watched, REREADS, move |deadline| {
        observe_pinned(&initial, &pinned, deadline)
    })
}

/// Its route lookups give up at `deadline`.
fn observe_pinned(
    initial: &Adapter,
    pinned: &PinnedNetwork,
    deadline: Instant,
) -> Result<Observed, Unread> {
    let current = read_selected(initial)?;
    let sole = pinned.peers.len() == 1;
    let lookup = |selected: &InterfaceSnapshot, peer| route_snapshot_by(selected, peer, deadline);
    let routes = pinned
        .peers
        .iter()
        .map(|peer| Ok((*peer.ip(), read_route(&current, *peer.ip(), sole, lookup)?)))
        .collect::<Result<PeerRoutes, Unread>>()?;
    Ok((current, routes))
}

/// A check that applies each read `observe` makes to the watched lock, reading again while it
/// cannot decide; `observe` is handed the window's deadline.
fn checking(
    watched: &PinnedLock,
    rereads: Rereads,
    observe: impl Fn(Instant) -> Result<Observed, Unread> + Send + Sync + 'static,
) -> SharedCheck {
    let watched = Arc::clone(watched);
    Arc::new(move || {
        let settled = settle(
            rereads,
            || session_ended(&watched.ended),
            |deadline| recheck(&watched, observe(deadline)),
        );
        concluded(settled, &watched.ended)
    })
}

/// True while the network holds. Once the watch has started, a check that revokes revokes the
/// session itself before it writes the log line, so the revoke never waits on the log.
fn concluded(settled: Settled, ended: &OnceLock<RevocationSignal>) -> bool {
    match settled {
        Settled::Holds => true,
        Settled::Ended => false,
        Settled::Revokes(reason) => {
            if let Some(signal) = ended.get() {
                signal.revoke();
            }
            log::warn!("{reason}");
            false
        }
    }
}

/// Reads until one read decides, handing each read the deadline one window after the first. An
/// undecided read is read again `rereads.interval` later; once no further read starts before the
/// deadline, or the session has ended, it revokes. Nothing about the socket changes in between:
/// its interface, address, peers and pins stay as bound.
fn settle(
    rereads: Rereads,
    ended: impl Fn() -> bool,
    mut read: impl FnMut(Instant) -> Verdict,
) -> Settled {
    let deadline = Instant::now() + rereads.window;
    let mut reads = 0_u32;
    loop {
        reads += 1;
        let error = match read(deadline) {
            Verdict::Holds => return Settled::Holds,
            Verdict::Revokes(reason) => return Settled::Revokes(reason),
            Verdict::Undecided(error) => error,
        };
        if Instant::now() + rereads.interval < deadline {
            log::info!(
                "network recheck: the selected adapter did not read back ({:?}: {error}); reading again",
                error.kind()
            );
            if !wait_unless_ended(rereads.interval, &ended) {
                return Settled::Ended;
            }
            if Instant::now() < deadline {
                continue;
            }
        }
        return Settled::Revokes(format!(
            "network recheck: the selected adapter still did not read back after {reads} reads ({:?}: {error}); revoked",
            error.kind()
        ));
    }
}

/// False as soon as `ended`, otherwise true once `interval` has passed.
fn wait_unless_ended(interval: Duration, ended: &impl Fn() -> bool) -> bool {
    let until = Instant::now() + interval;
    loop {
        if ended() {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(REREAD_POLL));
    }
}

fn session_ended(signal: &OnceLock<RevocationSignal>) -> bool {
    signal.get().is_some_and(RevocationSignal::is_revoked)
}

/// A pairing listener holds only while its adapter reads back exactly as it was bound.
fn listener_verdict(
    pinned: &Mutex<Option<InterfaceSnapshot>>,
    current: Result<InterfaceSnapshot, Unread>,
) -> Verdict {
    let Ok(pinned) = pinned.lock() else {
        return Verdict::Revokes(
            "pairing network recheck: the pinned adapter could not be read".to_owned(),
        );
    };
    match (pinned.as_ref(), current) {
        (Some(pinned), Ok(current)) if *pinned == current => Verdict::Holds,
        (Some(_), Err(Unread::Unreadable(error))) => Verdict::Undecided(error),
        _ => Verdict::Revokes("pairing network recheck: the selected adapter changed".to_owned()),
    }
}

/// How often the timer reruns the check while a member is set aside, and how long each member set
/// aside waits for each resolution probe.
#[derive(Clone, Copy)]
pub(super) struct Cadence {
    pub recheck: Duration,
    pub probe: Duration,
}

/// macOS holds an unanswered neighbor rejected for about 20 s and fails every send to it until
/// then, so a probe every 10 s lands within 10 s of that hold-down ending.
pub(super) const CADENCE: Cadence = Cadence {
    recheck: Duration::from_secs(2),
    probe: Duration::from_secs(10),
};

/// Sends one resolution probe to a member slot.
pub(super) type Probe = Box<dyn Fn(usize) + Send + Sync>;

/// Reruns the change watch's check while any member is set aside, because nothing may announce
/// that member's route coming back: macOS reports no neighbor-entry change, for one. It also
/// probes each member set aside, because macOS lifts an unanswered neighbor's rejection only once
/// something sends to it after the hold-down, and the socket sends such a member nothing else.
/// Dropping it stops the timer.
pub(super) struct Readmission(AbortHandle);

impl Readmission {
    /// None for a lone member: it is never set aside, so there is nothing to readmit or probe.
    pub(super) fn for_group(
        network: &Handle,
        check: SharedCheck,
        probe: Probe,
        signal: &RevocationSignal,
        reachability: &Arc<Reachability>,
        cadence: Cadence,
    ) -> Option<Self> {
        (reachability.len() > 1).then(|| {
            Self::start(
                network,
                check,
                probe,
                signal.clone(),
                Arc::clone(reachability),
                cadence,
            )
        })
    }

    /// The first check runs `cadence.recheck` after a member is set aside, then one every
    /// `cadence.recheck` until every member is back or `signal` is revoked. Each member set aside
    /// is probed just before a check, first `cadence.probe` after the timer sees it set aside and
    /// then no more often. Checks run on `network`'s blocking pool, never on the workers driving
    /// the endpoint.
    pub(super) fn start(
        network: &Handle,
        check: SharedCheck,
        probe: Probe,
        signal: RevocationSignal,
        reachability: Arc<Reachability>,
        cadence: Cadence,
    ) -> Self {
        Self(
            network
                .spawn(readmit(check, probe, signal, reachability, cadence))
                .abort_handle(),
        )
    }
}

impl Drop for Readmission {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// When each member set aside is next due a resolution probe; none while it is reachable.
struct ProbeSchedule {
    due: Box<[Option<Instant>]>,
    interval: Duration,
}

impl ProbeSchedule {
    fn new(members: usize, interval: Duration) -> Self {
        Self {
            due: vec![None; members].into(),
            interval,
        }
    }

    /// Starts the wait of each member newly set aside and clears it for each reachable again; with
    /// `probe`, also probes each member set aside whose wait ran out.
    fn run(&mut self, reachability: &Reachability, probe: Option<&Probe>) {
        let now = Instant::now();
        for (member, due) in self.due.iter_mut().enumerate() {
            if reachability.reaches(member) {
                *due = None;
                continue;
            }
            match (*due, probe) {
                (None, _) => *due = Some(now + self.interval),
                (Some(at), Some(probe)) if at <= now => {
                    probe(member);
                    // Counted from after the send, so two sends are never closer than `interval`.
                    *due = Some(Instant::now() + self.interval);
                }
                (Some(_), _) => {}
            }
        }
    }
}

async fn readmit(
    check: SharedCheck,
    probe: Probe,
    signal: RevocationSignal,
    reachability: Arc<Reachability>,
    cadence: Cadence,
) {
    let mut probes = ProbeSchedule::new(reachability.len(), cadence.probe);
    loop {
        if reachability.reaches_all() {
            // A member set aside again later waits a whole interval for its first probe.
            probes.due.fill(None);
        }
        reachability.some_set_aside().await;
        probes.run(&reachability, None);
        tokio::time::sleep(cadence.recheck).await;
        if signal.is_revoked() {
            return;
        }
        // Just before a check, so the check reads what the probe changed.
        probes.run(&reachability, Some(&probe));
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
                signal.revoke();
                log::warn!("network watch: a timed recheck could not run; revoked");
                return;
            }
        }
    }
}

/// Runs `check` as the change watch runs it on a notice: never once revoked, and a check that
/// fails or panics revokes. A session that ended while the check ran is not reported as changed.
fn timed_recheck(check: &SharedCheck, signal: &RevocationSignal) -> bool {
    if signal.is_revoked() {
        return false;
    }
    let holds = catch_unwind(AssertUnwindSafe(|| check())).unwrap_or_else(|payload| {
        std::mem::forget(payload);
        false
    });
    if !holds && !signal.is_revoked() {
        signal.revoke();
        log::warn!("network watch: a timed recheck found the pinned network changed; revoked");
    }
    holds
}

fn recheck(watched: &Watched, observed: Result<Observed, Unread>) -> Verdict {
    let Ok(mut lock) = watched.lock.lock() else {
        return Verdict::Revokes(
            "network recheck failed: the pinned lock could not be read".to_owned(),
        );
    };
    let verdict = judge(lock.as_mut(), observed);
    if matches!(verdict, Verdict::Holds)
        && let Some(lock) = lock.as_ref()
    {
        watched.reachability.set(lock.reachable());
    }
    verdict
}

/// A whole read goes to the lock, which revokes for good on any difference, as a change the read
/// shows does. An unreadable read leaves the lock and every member's reachability as they were.
fn judge(lock: Option<&mut NetworkLock>, observed: Result<Observed, Unread>) -> Verdict {
    let failure = match (lock, observed) {
        (None, _) => "no pinned snapshot yet".to_owned(),
        (Some(lock), Ok((current, routes))) => match lock.revalidate(Some(&current), &routes) {
            Ok(()) => return Verdict::Holds,
            Err(error) => format!("{error:?}"),
        },
        (Some(lock), Err(Unread::Changed(error))) => {
            let _ = lock.revalidate::<PeerRoute>(None, &[]);
            format!("the selected adapter changed ({:?}: {error})", error.kind())
        }
        (Some(lock), Err(Unread::Unreadable(_))) if lock.is_revoked() => {
            format!("{:?}", PolicyError::SessionClosed)
        }
        (Some(_), Err(Unread::Unreadable(error))) => return Verdict::Undecided(error),
    };
    Verdict::Revokes(format!("network recheck failed: {failure}"))
}

/// What names the selected adapter: its stable id, its index and the address bound on it.
#[derive(Clone, Copy)]
struct AdapterKey<'a> {
    stable_id: &'a str,
    index: u32,
    address: Ipv4Addr,
}

impl<'a> AdapterKey<'a> {
    fn of(pinned: &'a PinnedNetwork) -> Self {
        Self {
            stable_id: &pinned.stable_id,
            index: pinned.interface_index,
            address: *pinned.local.ip(),
        }
    }

    fn of_listener(selection: &'a ListenSelection) -> Self {
        Self {
            stable_id: &selection.stable_id,
            index: selection.interface_index,
            address: *selection.local.ip(),
        }
    }
}

fn find_exact_adapter(adapters: &[Adapter], key: AdapterKey<'_>) -> io::Result<Adapter> {
    let mut matches = adapters.iter().filter(|adapter| {
        adapter.stable_id == key.stable_id
            && adapter.index == key.index
            && adapter.address == key.address
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
    network::best_route(selected.address, peer).map(snapshot_of)
}

/// As `route_snapshot`, its wait for the kernel's reply ending at `deadline`.
#[cfg(target_os = "macos")]
fn route_snapshot_by(
    selected: &InterfaceSnapshot,
    peer: Ipv4Addr,
    deadline: Instant,
) -> io::Result<RouteSnapshot> {
    network::best_route_by(selected.address, peer, deadline).map(snapshot_of)
}

/// Windows reads its route table in one synchronous call, with no reply to wait for.
#[cfg(windows)]
fn route_snapshot_by(
    selected: &InterfaceSnapshot,
    peer: Ipv4Addr,
    _deadline: Instant,
) -> io::Result<RouteSnapshot> {
    route_snapshot(selected, peer)
}

fn snapshot_of(route: network::Route) -> RouteSnapshot {
    RouteSnapshot {
        interface_index: route.interface_index,
        source: route.source,
        next_hop: route.next_hop,
    }
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
            ended: OnceLock::new(),
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

    fn unreadable() -> Unread {
        Unread::Unreadable(io::Error::from(io::ErrorKind::Other))
    }

    fn changed() -> Unread {
        Unread::Changed(io::Error::from(io::ErrorKind::NotFound))
    }

    fn whole(current: &InterfaceSnapshot, routes: PeerRoutes) -> Result<Observed, Unread> {
        Ok((current.clone(), routes))
    }

    /// `Some(holds)` once a read decides, `None` while it cannot.
    fn decided(verdict: Verdict) -> Option<bool> {
        match verdict {
            Verdict::Holds => Some(true),
            Verdict::Revokes(_) => Some(false),
            Verdict::Undecided(_) => None,
        }
    }

    #[test]
    fn exact_adapter_lookup_rejects_absence_and_ambiguity() {
        let selection = pinned();
        assert_eq!(
            find_exact_adapter(&[], AdapterKey::of(&selection))
                .err()
                .expect("missing adapter must fail")
                .kind(),
            io::ErrorKind::NotFound
        );
        let adapter = adapter();
        assert_eq!(
            find_exact_adapter(&[adapter.clone(), adapter], AdapterKey::of(&selection))
                .err()
                .expect("ambiguous adapter must fail")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn an_adapter_gone_or_read_back_as_another_is_a_change_and_one_between_states_is_unreadable() {
        let initial = adapter();
        let present = |adapter: Adapter| Ok(AdapterReading::Present(adapter));
        assert_eq!(
            judge_adapter(&initial, present(initial.clone())).unwrap(),
            interface_snapshot(&initial).unwrap()
        );
        #[cfg(windows)]
        assert!(matches!(
            judge_adapter(&initial, Ok(AdapterReading::Settling)),
            Err(Unread::Unreadable(_))
        ));
        for between_states in [
            Err(io::Error::from(io::ErrorKind::Other)),
            present(Adapter {
                attachment: None,
                ..initial.clone()
            }),
        ] {
            assert!(matches!(
                judge_adapter(&initial, between_states),
                Err(Unread::Unreadable(_))
            ));
        }
        for gone_or_another in [
            Ok(AdapterReading::Lost),
            present(Adapter {
                name: "reused-index".to_owned(),
                ..initial.clone()
            }),
            present(Adapter {
                stable_id: "another-adapter".to_owned(),
                ..initial.clone()
            }),
            present(Adapter {
                prefix_len: 16,
                ..initial.clone()
            }),
        ] {
            assert!(matches!(
                judge_adapter(&initial, gone_or_another),
                Err(Unread::Changed(_))
            ));
        }
        // Another network name reads back whole; the lock then judges it a change.
        let other_network = judge_adapter(
            &initial,
            present(Adapter {
                attachment: Some(vec![2; 32]),
                ..initial.clone()
            }),
        )
        .unwrap();
        assert_ne!(other_network, interface_snapshot(&initial).unwrap());
    }

    #[test]
    fn a_failed_route_lookup_is_unreadable_and_a_refused_route_or_off_subnet_peer_a_change() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let found = |_: &InterfaceSnapshot, _: Ipv4Addr| Ok(route());
        assert_eq!(
            read_route(&selected, PEER, true, found).unwrap(),
            PeerRoute::Found(route())
        );
        for kind in [
            io::ErrorKind::Other,
            io::ErrorKind::TimedOut,
            io::ErrorKind::NotFound,
        ] {
            for sole in [true, false] {
                let failed = read_route(&selected, PEER, sole, |_, _| Err(io::Error::from(kind)));
                assert!(matches!(failed, Err(Unread::Unreadable(_))), "{kind:?}");
            }
        }
        // A peer the platform cannot reach sets aside a group member; a lone one is unreadable.
        for kind in [
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::NetworkUnreachable,
        ] {
            let unreachable = |_: &InterfaceSnapshot, _: Ipv4Addr| Err(io::Error::from(kind));
            assert_eq!(
                read_route(&selected, PEER, false, unreachable).unwrap(),
                PeerRoute::Unreachable
            );
            assert!(matches!(
                read_route(&selected, PEER, true, unreachable),
                Err(Unread::Unreadable(_))
            ));
        }
        // A route the policy refuses is a change for a group member and a lone peer alike.
        for sole in [true, false] {
            let refused = read_route(&selected, PEER, sole, |_, _| {
                Err(io::Error::other(network::RouteRefused("ineligible route")))
            });
            let Err(Unread::Changed(error)) = refused else {
                panic!("a refused route must be a change");
            };
            assert!(
                error
                    .get_ref()
                    .is_some_and(|cause| cause.is::<RouteCheckFailure>())
            );
        }
        let off_subnet = Ipv4Addr::new(192, 168, 51, 11);
        assert!(matches!(
            read_route(&selected, off_subnet, true, found),
            Err(Unread::Changed(_))
        ));
    }

    #[test]
    fn a_whole_read_keeps_the_session_only_while_adapter_attachment_and_route_hold() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let peer = Ipv4Addr::new(192, 168, 50, 11);
        let lock = NetworkLock::new(selected.clone(), peer, route(), false).unwrap();

        let mut unchanged = lock.clone();
        for _ in 0..2 {
            let read = whole(&selected, peer_routes(route()));
            assert_eq!(decided(judge(Some(&mut unchanged), read)), Some(true));
        }
        assert!(!unchanged.is_revoked());

        let mut moved = selected.clone();
        moved.network_signature = vec![2; 32];
        let gateway = RouteSnapshot {
            next_hop: Ipv4Addr::new(192, 168, 50, 1),
            ..route()
        };
        let elsewhere = RouteSnapshot {
            interface_index: 9,
            ..route()
        };
        for (current, route) in [
            (&moved, route()),
            (&selected, gateway),
            (&selected, elsewhere),
        ] {
            let mut lock = lock.clone();
            let read = whole(current, peer_routes(route));
            assert_eq!(decided(judge(Some(&mut lock), read)), Some(false));
            assert!(lock.is_revoked());
        }

        // A read that shows the adapter gone or different revokes the lock for good.
        let mut gone = lock.clone();
        assert_eq!(decided(judge(Some(&mut gone), Err(changed()))), Some(false));
        assert!(gone.is_revoked());
        let read = whole(&selected, peer_routes(route()));
        assert_eq!(decided(judge(Some(&mut gone), read)), Some(false));

        // An unreadable one decides nothing and leaves the lock as it was.
        let mut unread = lock.clone();
        assert_eq!(decided(judge(Some(&mut unread), Err(unreadable()))), None);
        assert!(!unread.is_revoked());
        let read = whole(&selected, peer_routes(route()));
        assert_eq!(decided(judge(Some(&mut unread), read)), Some(true));

        // Before the first snapshot, or once revoked, there is nothing to read again.
        let read = whole(&selected, peer_routes(route()));
        assert_eq!(decided(judge(None, read)), Some(false));
        assert_eq!(decided(judge(None, Err(unreadable()))), Some(false));
        assert_eq!(
            decided(judge(Some(&mut gone), Err(unreadable()))),
            Some(false)
        );
    }

    #[test]
    fn a_whole_read_revokes_the_whole_set_when_any_member_route_moves() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let lock = NetworkLock::new_group(selected.clone(), &members, false).unwrap();
        let mut unchanged = lock.clone();
        let read = whole(&selected, members.clone());
        assert_eq!(decided(judge(Some(&mut unchanged), read)), Some(true));
        for member in 0..members.len() {
            let mut routed = members.clone();
            routed[member].1 = gateway();
            let mut lock = lock.clone();
            assert_eq!(
                decided(judge(Some(&mut lock), whole(&selected, routed))),
                Some(false)
            );
            assert!(lock.is_revoked());
        }
        let mut missing = lock.clone();
        let read = whole(&selected, members[1..].to_vec());
        assert_eq!(decided(judge(Some(&mut missing), read)), Some(false));
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
        let second_down = with_down(&members, &[1]);
        assert_eq!(
            decided(recheck(&watched, whole(&selected, second_down))),
            Some(true)
        );
        assert_eq!(reaches(&watched), [true, false, true]);
        assert!(!revoked(&watched));

        let outer_down = with_down(&members, &[0, 2]);
        assert_eq!(
            decided(recheck(&watched, whole(&selected, outer_down))),
            Some(true)
        );
        assert_eq!(reaches(&watched), [false, true, false]);

        // An unreadable read sets no member aside and admits none.
        assert_eq!(decided(recheck(&watched, Err(unreadable()))), None);
        assert_eq!(reaches(&watched), [false, true, false]);

        // The next check that finds their routes admits them again.
        assert_eq!(
            decided(recheck(&watched, whole(&selected, members.clone()))),
            Some(true)
        );
        assert_eq!(reaches(&watched), [true, true, true]);
        assert!(!revoked(&watched));

        // A bind may start with a member already set aside.
        assert_eq!(
            reaches(&watching(&selected, &with_down(&members, &[2]))),
            [true, true, false]
        );
    }

    #[test]
    fn an_interface_change_still_revokes_every_member() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let set_aside = with_down(&member_routes(), &[1]);
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
            let read = whole(&current, set_aside.clone());
            assert_eq!(decided(recheck(&watched, read)), Some(false));
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
            assert_eq!(
                decided(recheck(&watched, whole(&selected, routes))),
                Some(false)
            );
            assert!(revoked(&watched));
        }

        let watched = watching(&selected, &set_aside);
        assert_eq!(decided(recheck(&watched, Err(changed()))), Some(false));
        assert!(revoked(&watched));
        assert_eq!(reaches(&watched), [true, false, true]);
    }

    /// Stands in for the one-second window and the 300 ms between rereads.
    const REREAD: Rereads = Rereads {
        window: Duration::from_millis(300),
        interval: Duration::from_millis(50),
    };

    /// How late a loaded machine may run a sleep or a step past its due time.
    const SLACK: Duration = Duration::from_millis(150);

    /// A window no test outlasts, for tests about what a read decides rather than when.
    const PATIENT: Rereads = Rereads {
        window: SETTLE,
        interval: Duration::from_millis(30),
    };

    /// A check on a lock bound to `selected` and one peer, whose `n`th read returns
    /// `read(n, deadline)`, and when it made each read with the deadline it was handed.
    struct Scripted {
        watched: PinnedLock,
        check: SharedCheck,
        reads: Arc<Mutex<Vec<(Instant, Instant)>>>,
    }

    impl Scripted {
        fn new(
            selected: &InterfaceSnapshot,
            rereads: Rereads,
            read: impl Fn(usize, Instant) -> Result<Observed, Unread> + Send + Sync + 'static,
        ) -> Self {
            let watched = Arc::new(watching(selected, &peer_routes(route())));
            let reads = Arc::new(Mutex::new(Vec::new()));
            let check = {
                let reads = Arc::clone(&reads);
                checking(&watched, rereads, move |deadline| {
                    let read_number = {
                        let mut reads = reads.lock().unwrap();
                        reads.push((Instant::now(), deadline));
                        reads.len() - 1
                    };
                    read(read_number, deadline)
                })
            };
            Self {
                watched,
                check,
                reads,
            }
        }

        fn reads(&self) -> Vec<Instant> {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .map(|read| read.0)
                .collect()
        }

        fn deadlines(&self) -> Vec<Instant> {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .map(|read| read.1)
                .collect()
        }
    }

    #[test]
    fn a_read_caught_between_states_is_read_again_and_an_unchanged_one_keeps_the_session() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let current = selected.clone();
        let script = Scripted::new(&selected, PATIENT, move |read, _| {
            if read < 2 {
                Err(unreadable())
            } else {
                whole(&current, peer_routes(route()))
            }
        });
        assert!((script.check)());
        let reads = script.reads();
        assert_eq!(reads.len(), 3);
        assert!(
            reads
                .windows(2)
                .all(|pair| pair[1] - pair[0] >= PATIENT.interval)
        );
        assert!(!revoked(&script.watched));
    }

    #[test]
    fn a_network_unreadable_for_the_whole_window_revokes_when_it_ends() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let script = Scripted::new(&selected, REREAD, |_, _| Err(unreadable()));
        let started = Instant::now();
        assert!(!(script.check)());
        let took = started.elapsed();
        // Every 50 ms up to 250 ms on an idle machine; a loaded one fits fewer, never one.
        assert!((2..=6).contains(&script.reads().len()));
        assert!(took >= REREAD.window - REREAD.interval);
        assert!(took < SETTLE);
    }

    #[test]
    fn every_read_shares_one_deadline_so_slow_reads_cannot_stretch_the_window() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let rereads = Rereads {
            window: Duration::from_millis(300),
            interval: Duration::from_millis(30),
        };
        let slow = Duration::from_millis(120);
        // Four reads of 120 ms would take 570 ms; the window stops the third from starting.
        let script = Scripted::new(&selected, rereads, move |_, _| {
            std::thread::sleep(slow);
            Err(unreadable())
        });
        let started = Instant::now();
        assert!(!(script.check)());
        let finished = Instant::now();
        let (reads, deadlines) = (script.reads(), script.deadlines());
        assert!(reads.len() <= 2);
        assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
        assert!(deadlines[0] >= started + rereads.window);
        assert!(deadlines[0] <= reads[0] + rereads.window);
        // No read starts after the deadline, and the check ends with its last read.
        assert!(reads.iter().all(|read| *read < deadlines[0]));
        assert!(finished < reads[reads.len() - 1] + slow + SLACK);

        // A lookup that gives up only at its deadline leaves no time for another read.
        let script = Scripted::new(&selected, rereads, |_, deadline| {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Err(unreadable())
        });
        assert!(!(script.check)());
        let finished = Instant::now();
        assert_eq!(script.reads().len(), 1);
        assert!(finished < script.deadlines()[0] + SLACK);
    }

    #[test]
    fn a_read_that_shows_a_change_revokes_without_reading_again() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let mut moved = selected.clone();
        moved.network_signature = vec![2; 32];
        let script = Scripted::new(&selected, PATIENT, move |_, _| {
            whole(&moved, peer_routes(route()))
        });
        assert!(!(script.check)());
        assert_eq!(script.reads().len(), 1);
        assert!(revoked(&script.watched));

        let script = Scripted::new(&selected, PATIENT, |_, _| Err(changed()));
        assert!(!(script.check)());
        assert_eq!(script.reads().len(), 1);
        assert!(revoked(&script.watched));

        // A change found on a reread revokes on that read.
        let script = Scripted::new(&selected, PATIENT, |read, _| {
            Err(if read == 0 { unreadable() } else { changed() })
        });
        assert!(!(script.check)());
        assert_eq!(script.reads().len(), 2);
        assert!(revoked(&script.watched));
    }

    #[test]
    fn a_session_that_ends_between_reads_stops_reading_at_once() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let signal = RevocationSignal::default();
        let ending = signal.clone();
        let long = Rereads {
            window: SETTLE * 4,
            interval: SETTLE * 2,
        };
        let script = Scripted::new(&selected, long, move |_, _| {
            ending.revoke();
            Err(unreadable())
        });
        assert!(script.watched.ended.set(signal).is_ok());
        let started = Instant::now();
        assert!(!(script.check)());
        assert!(started.elapsed() < SETTLE);
        assert_eq!(script.reads().len(), 1);
    }

    #[test]
    fn a_lone_member_whose_route_stays_unreadable_revokes_once_the_window_ends() {
        // Its lone member's route failing fails the read, whatever the platform calls it.
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
        }
        let selected = interface_snapshot(&adapter()).unwrap();
        let current = selected.clone();
        let script = Scripted::new(&selected, REREAD, move |_, _| {
            let route = read_route(&current, PEER, true, |_, _| {
                Err(io::Error::from(io::ErrorKind::HostUnreachable))
            })?;
            whole(&current, vec![(PEER, route)])
        });
        assert!(!(script.check)());
        assert!((2..=6).contains(&script.reads().len()));

        // The lock refuses a lone unreachable member too, at bind and on a whole read.
        let alone = vec![(PEER, PeerRoute::Unreachable)];
        assert_eq!(
            NetworkLock::new_group(selected.clone(), &alone, false).unwrap_err(),
            PolicyError::PeerUnreachable
        );
        let watched = watching(&selected, &peer_routes(route()));
        assert_eq!(
            decided(recheck(&watched, whole(&selected, alone))),
            Some(false)
        );
        assert!(revoked(&watched));
    }

    /// Stands in for the two seconds, so the timer's cadence runs in real time.
    const INTERVAL: Duration = Duration::from_millis(20);
    /// Stands in for the ten seconds between probes.
    const PROBE_INTERVAL: Duration = Duration::from_millis(150);
    const SETTLE: Duration = Duration::from_secs(5);

    /// What the next check reads in place of the native adapter and route lookups, and when each
    /// check read it.
    struct Network {
        current: Mutex<(Option<InterfaceSnapshot>, PeerRoutes)>,
        reads: Mutex<Vec<Instant>>,
    }

    impl Network {
        fn observe(&self) -> Result<Observed, Unread> {
            self.reads.lock().unwrap().push(Instant::now());
            let (current, routes) = self.current.lock().unwrap().clone();
            current
                .map(|current| (current, routes))
                .ok_or_else(unreadable)
        }

        fn set(&self, current: Option<&InterfaceSnapshot>, routes: &PeerRoutes) {
            *self.current.lock().unwrap() = (current.cloned(), routes.clone());
        }

        fn reads(&self) -> Vec<Instant> {
            self.reads.lock().unwrap().clone()
        }
    }

    /// A group bound with `routes`, the check its watch and timer share, the running timer, and
    /// each probe it sent, by member slot.
    struct Readmitting {
        watched: PinnedLock,
        network: Arc<Network>,
        check: SharedCheck,
        signal: RevocationSignal,
        probes: Arc<Mutex<Vec<(usize, Instant)>>>,
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
            let check = checking(&watched, REREAD, move |_| reading.observe());
            let signal = RevocationSignal::default();
            let probes = Arc::new(Mutex::new(Vec::new()));
            let probe: Probe = {
                let probes = Arc::clone(&probes);
                Box::new(move |member| probes.lock().unwrap().push((member, Instant::now())))
            };
            let timer = Readmission::start(
                &Handle::current(),
                Arc::clone(&check),
                probe,
                signal.clone(),
                Arc::clone(&watched.reachability),
                Cadence {
                    recheck: INTERVAL,
                    probe: PROBE_INTERVAL,
                },
            );
            Self {
                watched,
                network,
                check,
                signal,
                probes,
                _timer: timer,
            }
        }

        /// When each probe to `member` went out.
        fn probed(&self, member: usize) -> Vec<Instant> {
            self.probes
                .lock()
                .unwrap()
                .iter()
                .filter(|(probed, _)| *probed == member)
                .map(|(_, at)| *at)
                .collect()
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
    async fn each_member_set_aside_waits_its_own_interval_for_a_probe() {
        let selected = interface_snapshot(&adapter()).unwrap();
        let members = member_routes();
        let group = Readmitting::bound(&selected, &members);
        let first_marked = Instant::now();
        group.notice(&selected, &with_down(&members, &[0]));
        tokio::time::sleep(PROBE_INTERVAL / 2).await;
        let second_marked = Instant::now();
        group.notice(&selected, &with_down(&members, &[0, 2]));
        until("both members are probed twice", || {
            group.probed(0).len() >= 2 && group.probed(2).len() >= 2
        })
        .await;
        for (member, marked) in [(0, first_marked), (2, second_marked)] {
            let probed = group.probed(member);
            assert!(
                probed[0] >= marked + PROBE_INTERVAL,
                "slot {member} probed early"
            );
            assert!(
                probed[1] >= probed[0] + PROBE_INTERVAL,
                "slot {member} probed twice"
            );
        }
        assert!(group.probed(1).is_empty());
        assert_eq!(reaches(&group.watched), [false, true, false]);
        assert!(!group.signal.is_revoked());
    }

    #[tokio::test]
    async fn a_group_of_one_has_nothing_to_readmit_or_probe() {
        let check: SharedCheck = Arc::new(|| true);
        let probe = || -> Probe { Box::new(|_| panic!("a lone member was probed")) };
        let signal = RevocationSignal::default();
        let network = Handle::current();
        let alone = Reachability::new(1);
        assert!(
            Readmission::for_group(&network, check.clone(), probe(), &signal, &alone, CADENCE)
                .is_none()
        );
        let group = Reachability::new(2);
        assert!(
            Readmission::for_group(&network, check, probe(), &signal, &group, CADENCE).is_some()
        );
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
