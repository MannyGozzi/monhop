//! The share hub: one network-thread loop that owns native input and serves every peer link.
//!
//! [`HubRun::run`] drives a [`HubCore`] on the network thread's current-thread runtime. Each
//! link's reader is its own task filling a bounded queue, which the loop takes from in turn so one
//! flooding peer cannot starve another; each link's writer task sends. Native capture starts on
//! the blocking pool and the destination actor on its own thread once the first link is healthy,
//! and both stop after the last link leaves. [`ShareHub`] sends commands from any thread and
//! [`HubEvents`] reports each peer's start and end.

use crate::{
    session::{
        LinkIo, LinkStats, SESSION_POLL_INTERVAL, SESSION_QUEUE_CAPACITY, SessionDiagnostics,
        SessionFailure, SessionHalf, SessionProgress, SessionStartupFailure, TickGap,
        check_read_displays, close_reason, destination_failure, session_millis, settle_end,
        worker_end,
    },
    session_actor::ActorFailure,
    session_clock::{SessionClock, millis_u64},
    session_group::{
        DestinationJoin, HubAction, HubConfig as CoreConfig, HubCore, LinkEnd, LinkRefusal,
        LinkSetup,
    },
    session_handshake::NegotiatedSession,
    session_hub_actor::{HubDestinationActor, PeerRefusal, SlotEvent},
    session_native::{
        NativeDestination, current_displays, current_pointer_position, double_click_interval,
    },
    session_source::SourceController,
    session_source_runtime::{
        CaptureControl, NativeCapture, NativeCaptureError, native_capture_start_failure,
    },
};
use monhop_core::{
    CapturePermit, DeviceId, DisplayId, FloorOwner, FloorPeer, FloorState, InjectionPermit,
    MAX_GROUP_PEERS, NativeSessionClaim, Point, RevocationSignal, SharedFloor, TakeBackGate,
    Topology,
    capture::{CapturedEvent, StopReason},
};
use monhop_protocol::{DisplayTopology, Frame, SessionEpoch, SessionPurpose};
use std::{
    collections::VecDeque,
    marker::PhantomData,
    mem,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

/// How long native input may take to release once stopped, as a pairwise session allows.
const CLEANUP_WAIT: Duration = Duration::from_millis(250);
/// Frames or captured records handled per loop pass before the loop yields.
const DRAIN_LIMIT: usize = 256;

type Wake = Arc<dyn Fn() + Send + Sync>;

/// What a hub is built from.
pub struct HubConfig {
    pub local: DeviceId,
    /// This computer's displays, as each of its sessions announces them.
    pub local_displays: DisplayTopology,
    pub group: Topology,
    /// The group's other computers in slot order, at most [`MAX_GROUP_PEERS`].
    pub members: Vec<DeviceId>,
    /// The sharing generation native capture routes under.
    pub generation: u64,
    /// Ends the whole hub. Native capture and injection watch it themselves, and a native failure
    /// requests a stop on it.
    pub revocation: RevocationSignal,
}

/// How the app ends a link or the hub, which decides the close reason the other computer reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerEnd {
    /// The user ended it: the other computer records no failure.
    pub deliberate: bool,
    /// A control-change resync.
    pub control_change: bool,
}

// A handful per link over its whole life; boxing would only complicate the app's matches.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum HubEvent {
    /// Both directions of the peer's link finished startup.
    PeerStarted { peer: DeviceId },
    /// Exactly one per session [`ShareHub::add_peer`] accepted, started or not.
    PeerEnded {
        peer: DeviceId,
        result: Result<(), SessionFailure>,
        diagnostics: SessionDiagnostics,
    },
    /// Native input stopped after the last link left; the native claim is free.
    NativeIdle,
}

/// The hub's loop has ended; nothing more reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubClosed;

#[derive(Clone, Debug)]
pub struct HubStatus {
    pub floor: FloorState,
    /// The peer on the other end of the floor while one holds it.
    pub floor_peer: Option<DeviceId>,
    pub active_display: Option<DisplayId>,
    pub active_is_local: Option<bool>,
    /// Each peer whose link is starting, up or still ending, in slot order.
    pub peers: Vec<(DeviceId, SessionDiagnostics)>,
}

/// Commands for a running hub, from any thread.
#[derive(Clone)]
pub struct ShareHub {
    commands: mpsc::UnboundedSender<Command>,
    wake: Arc<Notify>,
    shared: Arc<Shared>,
}

/// What the loop publishes for [`ShareHub::status`].
struct Shared {
    floor: SharedFloor,
    /// Each installed link's peer and progress, by slot.
    peers: Mutex<[Option<(DeviceId, SessionProgress)>; MAX_GROUP_PEERS]>,
}

enum Command {
    AddPeer(Box<Arrival>),
    RemovePeer(DeviceId, PeerEnd),
    RequestLocal,
    Stop(PeerEnd),
}

struct Arrival {
    session: NegotiatedSession,
    progress: SessionProgress,
    cancel: RevocationSignal,
}

impl ShareHub {
    /// Serves a negotiated Share session. `cancel` is that link's own: stopping it ends only this
    /// link. A link to a member whose previous link has not finished ending, or that arrives while
    /// native input stops, waits until it can join.
    pub fn add_peer(
        &self,
        session: NegotiatedSession,
        progress: SessionProgress,
        cancel: RevocationSignal,
    ) -> Result<(), HubClosed> {
        let arrival = Box::new(Arrival {
            session,
            progress,
            cancel,
        });
        if let Err(mpsc::error::SendError(refused)) = self.commands.send(Command::AddPeer(arrival))
        {
            if let Command::AddPeer(arrival) = refused {
                let reason = close_reason(
                    &Err(SessionFailure::Revoked),
                    arrival.progress.ended_deliberately(),
                    arrival.cancel.is_stopping_for_control_change(),
                );
                arrival.session.connection.close(0_u32.into(), reason);
            }
            return Err(HubClosed);
        }
        self.wake.notify_one();
        Ok(())
    }

    /// Ends every link to `peer`; each reports its end once its injected input is released.
    pub fn remove_peer(&self, peer: DeviceId, end: PeerEnd) -> Result<(), HubClosed> {
        self.send(Command::RemovePeer(peer, end))
    }

    /// Brings input home from whichever peer this computer controls.
    pub fn request_local(&self) -> Result<(), HubClosed> {
        self.send(Command::RequestLocal)
    }

    /// Ends every link and native input; [`HubRun::run`] then returns.
    pub fn stop(&self, end: PeerEnd) {
        let _ = self.send(Command::Stop(end));
    }

    pub fn status(&self) -> HubStatus {
        let floor = self.shared.floor.snapshot();
        let peers = lock(&self.shared.peers).clone();
        let owner = index_of(floor.peer).and_then(|index| peers[index].as_ref());
        let diagnostics: Vec<_> = peers
            .iter()
            .flatten()
            .map(|(peer, progress)| (*peer, progress.diagnostics()))
            .collect();
        let active = owner
            .map(|(_, progress)| progress.diagnostics())
            .or_else(|| diagnostics.first().map(|(_, diagnostics)| *diagnostics));
        HubStatus {
            floor: floor.state,
            floor_peer: owner.map(|(peer, _)| *peer),
            active_display: active.and_then(|active| active.active_display),
            active_is_local: active.and_then(|active| active.active_is_local),
            peers: diagnostics,
        }
    }

    fn send(&self, command: Command) -> Result<(), HubClosed> {
        self.commands.send(command).map_err(|_| HubClosed)?;
        self.wake.notify_one();
        Ok(())
    }
}

/// The hub's reports, in the order they happened.
pub struct HubEvents(mpsc::UnboundedReceiver<HubEvent>);

impl HubEvents {
    /// None once the hub has ended and every event was taken.
    pub async fn recv(&mut self) -> Option<HubEvent> {
        self.0.recv().await
    }

    pub fn try_recv(&mut self) -> Option<HubEvent> {
        self.0.try_recv().ok()
    }
}

/// The hub's loop. It is `!Send`: it runs on the network thread's current-thread runtime, which
/// also hosts the endpoint and every connection driver.
pub struct HubRun(HubLoop<PlatformInput>);

impl HubRun {
    /// Ok once [`ShareHub::stop`] ended the hub. An Err is hub-fatal: every link has ended and
    /// native input was released first, as a pairwise session's end does.
    pub async fn run(self) -> Result<(), SessionFailure> {
        self.0.run().await
    }
}

pub fn start_share_hub(config: HubConfig) -> Result<(ShareHub, HubEvents, HubRun), SessionFailure> {
    let input = PlatformInput {
        local: config.local,
    };
    let (hub, events, run) = start_with(config, input, double_click_interval())?;
    Ok((hub, events, HubRun(run)))
}

fn start_with<N: NativeInput>(
    config: HubConfig,
    input: N,
    double_click: Duration,
) -> Result<(ShareHub, HubEvents, HubLoop<N>), SessionFailure> {
    let HubConfig {
        local,
        local_displays,
        group,
        members,
        generation,
        revocation,
    } = config;
    let origin = SessionClock::try_now()?;
    let core = HubCore::new(
        CoreConfig {
            local,
            group,
            members,
            generation,
            double_click,
        },
        origin.clone(),
    )?;
    let wake = Arc::new(Notify::new());
    let actor_waker: Arc<Mutex<Option<Wake>>> = Arc::default();
    let (gate_wake, running) = (wake.clone(), actor_waker.clone());
    // HubCore::new made this gate, so this is its only waker; each native run's actor is
    // reached through `running`.
    let _ = core.gate().set_waker(Arc::new(move || {
        // The capture callback never waits: a busy slot leaves the actor to its own poll.
        if let Ok(actor) = running.try_lock()
            && let Some(wake_actor) = actor.as_ref()
        {
            wake_actor();
        }
        gate_wake.notify_one();
    }));
    let shared = Arc::new(Shared {
        floor: core.gate().floor().clone(),
        peers: Mutex::default(),
    });
    let (commands, incoming) = mpsc::unbounded_channel();
    let (events, reports) = mpsc::unbounded_channel();
    let workers = Workers {
        input,
        gate: core.gate().clone(),
        revocation: revocation.clone(),
        displays: local_displays.clone(),
        generation,
        wake: wake.clone(),
        actor_waker,
        state: NativeState::Idle,
        capture: None,
        starting: None,
        injection: None,
        destination: None,
    };
    let now = origin.elapsed();
    let run = HubLoop {
        core,
        origin,
        local,
        displays: local_displays,
        revocation,
        workers,
        seats: std::array::from_fn(|_| Seat::Empty),
        queued: VecDeque::new(),
        leaving: [None; MAX_GROUP_PEERS],
        commands: incoming,
        events,
        shared: shared.clone(),
        wake: wake.clone(),
        cursor: 0,
        batch: Vec::new(),
        gaps: TickGap::new(now),
        last_floor: None,
        #[cfg(test)]
        passes: Rc::default(),
        _network_thread: PhantomData,
    };
    let hub = ShareHub {
        commands,
        wake,
        shared,
    };
    Ok((hub, HubEvents(reports), run))
}

/// Native capture as the hub drives it.
pub(crate) trait HubCapture: CaptureControl + Send + 'static {
    fn set_waker(&self, waker: Wake);
    fn try_next_event(&mut self) -> Result<Option<CapturedEvent>, StopReason>;
    fn is_finished(&self) -> bool;
    fn finish(&mut self) -> Result<(), NativeCaptureError>;
}

/// This computer's native input, started once the first peer is healthy.
pub(crate) trait NativeInput: Clone + Send + 'static {
    type Capture: HubCapture;
    /// Blocks; runs on the blocking pool.
    fn start_capture(
        &self,
        generation: u64,
        displays: &DisplayTopology,
        revocation: RevocationSignal,
        permit: CapturePermit,
        gate: TakeBackGate,
    ) -> Result<Self::Capture, SessionFailure>;
    fn start_destination(
        &self,
        displays: DisplayTopology,
        revocation: RevocationSignal,
        permit: InjectionPermit,
        gate: TakeBackGate,
    ) -> Result<HubDestinationActor, ActorFailure>;
    fn pointer(&self) -> Option<Point>;
}

#[derive(Clone)]
struct PlatformInput {
    local: DeviceId,
}

impl HubCapture for NativeCapture {
    fn set_waker(&self, waker: Wake) {
        NativeCapture::set_waker(self, waker);
    }
    fn try_next_event(&mut self) -> Result<Option<CapturedEvent>, StopReason> {
        NativeCapture::try_next_event(self)
    }
    fn is_finished(&self) -> bool {
        NativeCapture::is_finished(self)
    }
    fn finish(&mut self) -> Result<(), NativeCaptureError> {
        NativeCapture::finish(self)
    }
}

impl NativeInput for PlatformInput {
    type Capture = NativeCapture;

    fn start_capture(
        &self,
        generation: u64,
        displays: &DisplayTopology,
        revocation: RevocationSignal,
        permit: CapturePermit,
        gate: TakeBackGate,
    ) -> Result<NativeCapture, SessionFailure> {
        if revocation.is_stopping() {
            return Err(SessionFailure::Revoked);
        }
        check_read_displays(displays, current_displays(self.local))?;
        NativeCapture::start_for_session(generation, revocation, permit, gate)
            .map_err(native_capture_start_failure)
    }

    fn start_destination(
        &self,
        displays: DisplayTopology,
        revocation: RevocationSignal,
        permit: InjectionPermit,
        gate: TakeBackGate,
    ) -> Result<HubDestinationActor, ActorFailure> {
        let local = self.local;
        let (native_displays, native_revocation, native_gate) =
            (displays.clone(), revocation.clone(), gate.clone());
        HubDestinationActor::start_after_local_enable(
            displays,
            revocation,
            permit,
            gate,
            move |permit| {
                NativeDestination::new_after_local_enable(
                    local,
                    native_displays,
                    permit,
                    native_revocation,
                    native_gate,
                )
            },
        )
    }

    fn pointer(&self) -> Option<Point> {
        current_pointer_position()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeState {
    Idle,
    Starting,
    Ready,
    /// Stopping after the last link left; cleanup must finish by the deadline.
    Stopping(Instant),
}

/// Native capture and the destination actor for one run of native input, from the first healthy
/// link until the last one leaves.
struct Workers<N: NativeInput> {
    input: N,
    gate: TakeBackGate,
    revocation: RevocationSignal,
    displays: DisplayTopology,
    generation: u64,
    wake: Arc<Notify>,
    /// Where the gate's waker finds the running destination actor.
    actor_waker: Arc<Mutex<Option<Wake>>>,
    state: NativeState,
    capture: Option<N::Capture>,
    starting: Option<JoinHandle<Result<N::Capture, SessionFailure>>>,
    /// Held from the claim until the destination actor takes it.
    injection: Option<InjectionPermit>,
    destination: Option<HubDestinationActor>,
}

/// What each native worker was doing when `CLEANUP_WAIT` ran out.
struct CleanupPending {
    startup_running: bool,
    capture: Option<NativeCaptureError>,
    injection_running: bool,
    injected_release_failed: bool,
}

impl<N: NativeInput> Workers<N> {
    fn is_live(&self) -> bool {
        matches!(self.state, NativeState::Starting | NativeState::Ready)
    }

    /// Why the hub must stop now. Workers being stopped on purpose are not failures.
    fn failure(&self) -> Option<SessionFailure> {
        if !self.is_live() {
            return self
                .revocation
                .is_stopping()
                .then_some(SessionFailure::Revoked);
        }
        worker_end(
            self.revocation.is_stopping(),
            false,
            self.capture_stop(),
            self.capture.as_ref().is_some_and(HubCapture::is_finished),
            self.destination_failure(),
        )
    }

    fn capture_stop(&self) -> Option<StopReason> {
        self.capture.as_ref().and_then(CaptureControl::stop_reason)
    }

    fn destination_failure(&self) -> Option<ActorFailure> {
        self.destination
            .as_ref()
            .and_then(HubDestinationActor::failure)
    }

    /// Claims native input and starts capture off the network thread.
    fn start(&mut self) -> Result<(), SessionFailure> {
        let (capture, injection) = NativeSessionClaim::claim()
            .ok_or(SessionFailure::Startup(
                SessionStartupFailure::NativeOwnershipUnavailable,
            ))?
            .split();
        self.injection = Some(injection);
        let input = self.input.clone();
        let (displays, revocation, gate, generation) = (
            self.displays.clone(),
            self.revocation.clone(),
            self.gate.clone(),
            self.generation,
        );
        self.starting = Some(tokio::task::spawn_blocking(move || {
            input.start_capture(generation, &displays, revocation, capture, gate)
        }));
        self.state = NativeState::Starting;
        Ok(())
    }

    /// Starts the destination actor once capture runs; true once both are native-ready.
    async fn poll_start(&mut self) -> Result<bool, SessionFailure> {
        if self.state != NativeState::Starting {
            return Ok(false);
        }
        if self.starting.as_ref().is_some_and(JoinHandle::is_finished) {
            let capture = self
                .starting
                .take()
                .expect("finished capture start")
                .await
                .map_err(|_| {
                    SessionFailure::Startup(SessionStartupFailure::CaptureWorkerUnavailable)
                })??;
            let wake = self.wake.clone();
            capture.set_waker(Arc::new(move || wake.notify_one()));
            self.capture = Some(capture);
            let permit = self.injection.take().ok_or(SessionFailure::Destination)?;
            let actor = self
                .input
                .start_destination(
                    self.displays.clone(),
                    self.revocation.clone(),
                    permit,
                    self.gate.clone(),
                )
                .map_err(destination_failure)?;
            *lock(&self.actor_waker) = Some(actor.waker());
            let wake = self.wake.clone();
            actor.set_response_waker(Arc::new(move || wake.notify_one()));
            self.destination = Some(actor);
        }
        let ready = self.capture.is_some()
            && self
                .destination
                .as_ref()
                .is_some_and(HubDestinationActor::is_native_ready);
        if ready {
            self.state = NativeState::Ready;
        }
        Ok(ready)
    }

    /// A start still in flight is abandoned: it never reaches injection, and the capture it
    /// yields is stopped at once.
    fn begin_stop(&mut self) {
        self.injection = None;
        self.stop();
        self.state = NativeState::Stopping(Instant::now() + CLEANUP_WAIT);
    }

    /// True once a stop has finished.
    async fn poll_stop(&mut self) -> Result<bool, SessionFailure> {
        let NativeState::Stopping(deadline) = self.state else {
            return Ok(false);
        };
        match self.finish_step().await {
            None => Ok(true),
            Some(pending) if Instant::now() >= deadline => {
                log_cleanup(&pending, &Ok(()));
                Err(SessionFailure::NativeCleanup)
            }
            Some(_) => Ok(false),
        }
    }

    fn stop(&self) {
        self.gate.open_injection(0);
        if let Some(capture) = &self.capture {
            CaptureControl::request_stop(capture);
        }
        if let Some(destination) = &self.destination {
            destination.request_stop();
        }
    }

    /// One check of the pairwise runtime's finish: None once every worker has stopped and
    /// released what it held.
    async fn finish_step(&mut self) -> Option<CleanupPending> {
        self.stop();
        if self.starting.as_ref().is_some_and(JoinHandle::is_finished)
            && let Ok(Ok(capture)) = self.starting.take().expect("finished capture start").await
        {
            CaptureControl::request_stop(&capture);
            self.capture = Some(capture);
        }
        let capture = self.capture.as_mut().and_then(|capture| {
            if capture.is_finished() {
                capture.finish().err()
            } else {
                Some(NativeCaptureError::CleanupPending)
            }
        });
        let injection_running = self
            .destination
            .as_mut()
            .is_some_and(|actor| !actor.finish());
        let injected_release_failed = !injection_running
            && self
                .destination
                .as_ref()
                .is_some_and(HubDestinationActor::cleanup_pending);
        if self.starting.is_none()
            && capture.is_none()
            && !injection_running
            && !injected_release_failed
        {
            return None;
        }
        Some(CleanupPending {
            startup_running: self.starting.is_some(),
            capture,
            injection_running,
            injected_release_failed,
        })
    }

    async fn finish(&mut self) -> Result<(), CleanupPending> {
        let deadline = Instant::now() + CLEANUP_WAIT;
        loop {
            let Some(pending) = self.finish_step().await else {
                return Ok(());
            };
            if Instant::now() >= deadline {
                return Err(pending);
            }
            tokio::time::sleep(SESSION_POLL_INTERVAL).await;
        }
    }

    /// Drops the finished workers, which frees the native claim.
    fn release(&mut self) {
        *lock(&self.actor_waker) = None;
        self.capture = None;
        self.destination = None;
        self.injection = None;
        self.state = NativeState::Idle;
    }
}

impl<N: NativeInput> Drop for Workers<N> {
    fn drop(&mut self) {
        self.stop();
        // A detached blocking start must never admit input after its hub is gone.
        if self.starting.is_some() {
            self.revocation.request_stop();
        }
    }
}

fn log_cleanup(pending: &CleanupPending, result: &Result<(), SessionFailure>) {
    log::warn!(
        "hub: native cleanup outlasted {CLEANUP_WAIT:?} (startup running {}, capture {}, injection running {}, injected release failed {}) after the hub ended with {result:?}",
        pending.startup_running,
        pending
            .capture
            .map_or_else(|| "done".to_owned(), |error| format!("{error:?}")),
        pending.injection_running,
        pending.injected_release_failed,
    );
}

/// What the core needs from a session, kept so a waiting link can be offered again.
struct Setup {
    outbound_enabled: bool,
    inbound_enabled: bool,
    epoch: SessionEpoch,
    next_sequence: u64,
    local_displays: DisplayTopology,
    peer_displays: DisplayTopology,
}

impl Setup {
    fn of(session: &NegotiatedSession) -> Self {
        Self {
            outbound_enabled: session.permissions.allows(session.outbound_scope()),
            inbound_enabled: session.permissions.allows(session.inbound_scope()),
            epoch: session.initial_epoch,
            next_sequence: session.control.next_sequence(),
            local_displays: session.local.topology.clone(),
            peer_displays: session.peer.topology.clone(),
        }
    }

    fn link(&self, peer: DeviceId) -> LinkSetup {
        LinkSetup {
            peer,
            outbound_enabled: self.outbound_enabled,
            inbound_enabled: self.inbound_enabled,
            epoch: self.epoch,
            next_sequence: self.next_sequence,
            local_displays: self.local_displays.clone(),
            peer_displays: self.peer_displays.clone(),
        }
    }
}

/// One peer's connection as the loop holds it: the writer half, and the queue its reader task
/// fills.
struct Link {
    peer: DeviceId,
    io: LinkIo,
    inbox: mpsc::Receiver<Result<Frame, SessionFailure>>,
    reader: JoinHandle<()>,
    progress: SessionProgress,
    cancel: RevocationSignal,
    /// How the app ended it, when it did.
    end: PeerEnd,
    /// Its receiver runs in the destination actor.
    joined: bool,
    started_at: Option<Duration>,
}

impl Link {
    fn open(
        session: NegotiatedSession,
        progress: SessionProgress,
        cancel: RevocationSignal,
        wake: Arc<Notify>,
    ) -> Self {
        let peer = session.peer.device_id;
        let (mut reader, io) = LinkIo::split(session, progress.clone());
        let (frames, inbox) = mpsc::channel(SESSION_QUEUE_CAPACITY);
        let reader = tokio::spawn(async move {
            loop {
                let next = reader.next_frame().await;
                let ended = next.is_err();
                if frames.send(next).await.is_err() {
                    return;
                }
                wake.notify_one();
                if ended {
                    return;
                }
            }
        });
        Self {
            peer,
            io,
            inbox,
            reader,
            progress,
            cancel,
            end: PeerEnd::default(),
            joined: false,
            started_at: None,
        }
    }

    fn end_by(&mut self, end: PeerEnd) {
        if end.deliberate {
            self.progress.end_deliberately();
        }
        self.end.deliberate |= end.deliberate;
        self.end.control_change |= end.control_change;
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A link waiting for its member's slot, or for native input to finish stopping.
struct Queued {
    setup: Setup,
    link: Link,
}

enum Seat {
    Empty,
    Live(Box<Link>),
    /// Closed; its peer's end is reported once the core frees the slot.
    Closed(Box<Closed>),
}

struct Closed {
    peer: DeviceId,
    progress: SessionProgress,
    result: Result<(), SessionFailure>,
    stats: LinkStats,
    joined: bool,
}

pub(crate) struct HubLoop<N: NativeInput> {
    core: HubCore,
    origin: SessionClock,
    local: DeviceId,
    displays: DisplayTopology,
    revocation: RevocationSignal,
    workers: Workers<N>,
    seats: [Seat; MAX_GROUP_PEERS],
    queued: VecDeque<Queued>,
    /// When each leaving slot's receiver was asked to stop.
    leaving: [Option<Duration>; MAX_GROUP_PEERS],
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<HubEvent>,
    shared: Arc<Shared>,
    wake: Arc<Notify>,
    /// The slot whose queue is read first on the next pass.
    cursor: usize,
    batch: Vec<HubAction>,
    gaps: TickGap,
    last_floor: Option<(FloorState, FloorPeer)>,
    /// Loop passes so far.
    #[cfg(test)]
    passes: Rc<std::cell::Cell<u32>>,
    _network_thread: PhantomData<Rc<()>>,
}

impl<N: NativeInput> HubLoop<N> {
    pub(crate) async fn run(mut self) -> Result<(), SessionFailure> {
        let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let outcome = loop {
            let housekeeping = if self.is_idle() {
                self.park().await;
                false
            } else {
                tokio::select! {
                    _ = tick.tick() => true,
                    () = self.wake.notified() => false,
                }
            };
            #[cfg(test)]
            self.passes.set(self.passes.get() + 1);
            match self.step(housekeeping).await {
                Ok(None) => {}
                Ok(Some(end)) => break Ok(end),
                Err(failure) => break Err(failure),
            }
        };
        self.teardown(outcome).await
    }

    /// No link, nothing queued and native input idle: the tick has nothing to time.
    fn is_idle(&self) -> bool {
        self.workers.state == NativeState::Idle
            && self.queued.is_empty()
            && self.seats.iter().all(|seat| matches!(seat, Seat::Empty))
    }

    /// Waits, with no timer, for a command or a revocation. A stop request wakes nothing, so it
    /// is seen at the next wake.
    async fn park(&mut self) {
        self.report();
        tokio::select! {
            () = self.wake.notified() => {}
            () = std::future::poll_fn(|cx| self.revocation.poll_revoked(cx)) => {}
        }
        // Time spent parked is not a stalled tick.
        self.gaps = TickGap::new(self.origin.elapsed());
    }

    /// One pass: native lifecycle, commands, each link's frames in turn, captured records,
    /// destination events, then housekeeping on the tick. Some(end) stops the hub.
    async fn step(&mut self, housekeeping: bool) -> Result<Option<PeerEnd>, SessionFailure> {
        if let Some(failure) = self.workers.failure() {
            return Err(failure);
        }
        if self.workers.poll_start().await? {
            self.core.native_ready();
        }
        if self.workers.poll_stop().await? {
            self.native_stopped()?;
        }
        if let Some(end) = self.take_commands()? {
            return Ok(Some(end));
        }
        self.read_links()?;
        self.read_capture()?;
        self.read_destination()?;
        if housekeeping {
            self.housekeeping()?;
        }
        Ok(None)
    }

    fn take_commands(&mut self) -> Result<Option<PeerEnd>, SessionFailure> {
        loop {
            let command = match self.commands.try_recv() {
                Ok(command) => command,
                Err(mpsc::error::TryRecvError::Empty) => return Ok(None),
                // Every handle is gone, so nothing could stop the hub any more.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Ok(Some(PeerEnd::default()));
                }
            };
            match command {
                Command::AddPeer(arrival) => self.arrive(*arrival),
                Command::RemovePeer(peer, end) => self.remove(peer, end)?,
                Command::RequestLocal => {
                    self.core.request_local(self.workers.capture.as_mut())?;
                    self.execute()?;
                }
                Command::Stop(end) => return Ok(Some(end)),
            }
        }
    }

    fn arrive(&mut self, arrival: Arrival) {
        let Arrival {
            session,
            progress,
            cancel,
        } = arrival;
        let refusal = if session.purpose() != SessionPurpose::Share {
            Some(SessionFailure::Destination)
        } else if session.local.device_id != self.local {
            Some(SessionFailure::InvalidLayout)
        } else if !session.local.topology.same_geometry(&self.displays) {
            Some(SessionFailure::LocalDisplaysChanged)
        } else {
            None
        };
        let setup = Setup::of(&session);
        let link = Link::open(session, progress, cancel, self.wake.clone());
        match refusal {
            Some(failure) => self.refuse(link, Err(failure)),
            None => {
                self.queued.push_back(Queued { setup, link });
                self.admit_queued();
            }
        }
    }

    /// Offers every waiting link its slot, oldest first.
    fn admit_queued(&mut self) {
        for _ in 0..self.queued.len() {
            let Some(queued) = self.queued.pop_front() else {
                return;
            };
            match self.core.add_link(queued.setup.link(queued.link.peer)) {
                Ok(slot) => self.install(slot, queued.link),
                Err(LinkRefusal::Stopping | LinkRefusal::Occupied) => {
                    self.queued.push_back(queued);
                }
                Err(LinkRefusal::NotMember | LinkRefusal::Layout) => {
                    self.refuse(queued.link, Err(SessionFailure::InvalidLayout));
                }
            }
        }
    }

    fn install(&mut self, slot: FloorPeer, link: Link) {
        let index = seat(slot);
        lock(&self.shared.peers)[index] = Some((link.peer, link.progress.clone()));
        self.seats[index] = Seat::Live(Box::new(link));
    }

    fn remove(&mut self, peer: DeviceId, end: PeerEnd) -> Result<(), SessionFailure> {
        let mut index = 0;
        while index < self.queued.len() {
            if self.queued[index].link.peer != peer {
                index += 1;
                continue;
            }
            if let Some(mut queued) = self.queued.remove(index) {
                queued.link.end_by(end);
                self.refuse(queued.link, Err(SessionFailure::Revoked));
            }
        }
        for index in 0..MAX_GROUP_PEERS {
            let Seat::Live(link) = &mut self.seats[index] else {
                continue;
            };
            if link.peer != peer {
                continue;
            }
            link.end_by(end);
            let result = Err(SessionFailure::Revoked);
            self.core
                .remove_link(self.workers.capture.as_mut(), slot_at(index), result)?;
            self.execute()?;
        }
        Ok(())
    }

    /// Each link's queue in turn, so a flooding peer yields to every other between its frames.
    fn read_links(&mut self) -> Result<(), SessionFailure> {
        for _ in 0..DRAIN_LIMIT {
            let seats = &mut self.seats;
            let Some((index, next)) = next_in_turn(&mut self.cursor, MAX_GROUP_PEERS, |index| {
                let Seat::Live(link) = &mut seats[index] else {
                    return None;
                };
                match link.inbox.try_recv() {
                    Ok(next) => Some(next),
                    Err(mpsc::error::TryRecvError::Empty) => None,
                    Err(mpsc::error::TryRecvError::Disconnected) => Some(Err(SessionFailure::Wire)),
                }
            }) else {
                return Ok(());
            };
            let slot = slot_at(index);
            match next {
                Ok(frame) => {
                    if let Seat::Live(link) = &self.seats[index] {
                        link.progress.received(&frame);
                    }
                    self.core
                        .on_frame(self.workers.capture.as_mut(), slot, frame)?;
                }
                Err(failure) => {
                    self.core
                        .remove_link(self.workers.capture.as_mut(), slot, Err(failure))?;
                }
            }
            self.execute()?;
        }
        self.wake.notify_one();
        Ok(())
    }

    fn read_capture(&mut self) -> Result<(), SessionFailure> {
        for _ in 0..DRAIN_LIMIT {
            let Some(capture) = self.workers.capture.as_mut() else {
                return Ok(());
            };
            let Some(record) = capture
                .try_next_event()
                .map_err(|_| SessionFailure::Native)?
            else {
                return Ok(());
            };
            self.core.on_captured(capture, record)?;
            self.execute()?;
        }
        self.wake.notify_one();
        Ok(())
    }

    fn read_destination(&mut self) -> Result<(), SessionFailure> {
        for _ in 0..SESSION_QUEUE_CAPACITY {
            if !self.workers.is_live() {
                return Ok(());
            }
            let Some(actor) = self.workers.destination.as_mut() else {
                return Ok(());
            };
            let Some((slot, event)) = actor.try_response().map_err(destination_failure)? else {
                return Ok(());
            };
            // The core ends a failed slot as Destination; the actor knows the precise reason.
            let reason = matches!(event, SlotEvent::Failed(_))
                .then(|| actor.peer_failure(slot))
                .flatten();
            if let Some(reason) = reason {
                let result = Err(destination_failure(reason));
                self.core
                    .remove_link(self.workers.capture.as_mut(), slot, result)?;
            }
            self.core
                .on_destination(self.workers.capture.as_mut(), slot, event)?;
            self.execute()?;
        }
        self.wake.notify_one();
        Ok(())
    }

    fn housekeeping(&mut self) -> Result<(), SessionFailure> {
        let now = self.origin.elapsed();
        self.gaps.observe(now);
        for index in 0..MAX_GROUP_PEERS {
            let Seat::Live(link) = &self.seats[index] else {
                continue;
            };
            // Three quinn lock round trips per link: the tick, never each frame, pays for them.
            let ended = if link.cancel.is_stopping() {
                Some(SessionFailure::Revoked)
            } else {
                link.io.check().err()
            };
            if let Some(failure) = ended {
                self.core.remove_link(
                    self.workers.capture.as_mut(),
                    slot_at(index),
                    Err(failure),
                )?;
                self.execute()?;
            }
        }
        self.check_queued();
        let Workers { input, capture, .. } = &mut self.workers;
        self.core.on_tick(capture.as_mut(), || input.pointer())?;
        self.execute()?;
        self.report();
        self.check_leaving(now)
    }

    /// A waiting link whose connection or cancel ended leaves the queue.
    fn check_queued(&mut self) {
        let mut index = 0;
        while index < self.queued.len() {
            let link = &self.queued[index].link;
            let ended = if link.cancel.is_stopping() {
                Some(SessionFailure::Revoked)
            } else {
                link.io.check().err()
            };
            match ended.and_then(|failure| Some((failure, self.queued.remove(index)?))) {
                Some((failure, queued)) => self.refuse(queued.link, Err(failure)),
                None => index += 1,
            }
        }
    }

    /// A leaving slot keeps the floor until its injected input is released, which native cleanup
    /// must manage within `CLEANUP_WAIT` as it does when a pairwise session ends.
    fn check_leaving(&self, now: Duration) -> Result<(), SessionFailure> {
        if !self.workers.is_live() {
            return Ok(());
        }
        for (index, since) in self.leaving.iter().enumerate() {
            if since.is_some_and(|since| now.saturating_sub(since) >= CLEANUP_WAIT) {
                let failing = self
                    .workers
                    .destination
                    .as_ref()
                    .is_some_and(|actor| actor.peer_cleanup_pending(slot_at(index)));
                log::warn!(
                    "hub: slot {} kept its injected input {CLEANUP_WAIT:?} after its link ended (release failing {failing})",
                    index + 1
                );
                return Err(SessionFailure::NativeCleanup);
            }
        }
        Ok(())
    }

    /// Each link's route, active half, hold and blocking presses, as a pairwise session reports
    /// its own.
    fn report(&mut self) {
        let floor = self.core.floor();
        if self.last_floor != Some((floor.state, floor.peer)) {
            self.last_floor = Some((floor.state, floor.peer));
            log::info!(
                "hub control: floor {:?} with slot {}, capture remote {}",
                floor.state,
                floor.peer.get(),
                self.core.route().0
            );
        }
        let actor = self.workers.destination.as_ref();
        for (index, seat) in self.seats.iter().enumerate() {
            let Seat::Live(link) = seat else {
                continue;
            };
            let slot = slot_at(index);
            let owner = (floor.peer == slot).then(|| floor.state.owner()).flatten();
            let inbound = owner == Some(FloorOwner::Inbound);
            let source = self.core.source(slot);
            let target = source
                .and_then(SourceController::motion_target)
                .map(|motion| motion.target);
            link.progress.route(
                if inbound {
                    actor.and_then(|actor| actor.active_display(slot))
                } else {
                    target.map(|target| target.display)
                },
                inbound || target.is_some_and(|target| target.machine == self.local),
            );
            link.progress.active_half(owner.map(|owner| match owner {
                FloorOwner::Outbound => SessionHalf::Outbound,
                FloorOwner::Inbound => SessionHalf::Inbound,
            }));
            link.progress.hold(
                source.is_some_and(|source| source.held_since().is_some())
                    || actor.is_some_and(|actor| actor.is_held(slot)),
            );
            link.progress.block(self.core.blocking_presses());
        }
    }

    /// Executes what the core asked for, in order, until it asks for nothing more.
    fn execute(&mut self) -> Result<(), SessionFailure> {
        loop {
            self.batch.extend(self.core.actions());
            if self.batch.is_empty() {
                return Ok(());
            }
            let mut batch = mem::take(&mut self.batch);
            let mut result = Ok(());
            for action in batch.drain(..) {
                if result.is_ok() {
                    result = self.perform(action);
                }
            }
            self.batch = batch;
            result?;
        }
    }

    fn perform(&mut self, action: HubAction) -> Result<(), SessionFailure> {
        match action {
            HubAction::SendOutbound { slot, frame } => self.send(slot, frame, true),
            HubAction::SendInbound { slot, frame } => self.send(slot, frame, false),
            HubAction::StartNative => self.workers.start(),
            HubAction::StopNative => {
                self.workers.begin_stop();
                Ok(())
            }
            HubAction::JoinDestination(join) => self.join(*join),
            HubAction::Submit { slot, frame } => self.submit(slot, frame),
            HubAction::LeaveDestination { slot } => self.leave(slot),
            HubAction::CloseLink { slot, end } => {
                self.close(slot, end);
                Ok(())
            }
            HubAction::LinkEnded { slot } => {
                self.ended(slot);
                Ok(())
            }
            HubAction::LinkStarted { slot, peer } => {
                if let Seat::Live(link) = &mut self.seats[seat(slot)] {
                    link.started_at = Some(self.origin.elapsed());
                    link.progress.mark_started();
                }
                self.emit(HubEvent::PeerStarted { peer });
                Ok(())
            }
        }
    }

    /// A link that cannot take a frame ends; the others never wait for it.
    fn send(
        &mut self,
        slot: FloorPeer,
        frame: Frame,
        outbound: bool,
    ) -> Result<(), SessionFailure> {
        let Seat::Live(link) = &self.seats[seat(slot)] else {
            return Ok(());
        };
        let sent = if outbound {
            link.io.send_outbound(frame)
        } else {
            link.io.send_inbound(frame)
        };
        match sent {
            Ok(()) => Ok(()),
            Err(failure) => {
                self.core
                    .remove_link(self.workers.capture.as_mut(), slot, Err(failure))
            }
        }
    }

    fn join(&mut self, join: DestinationJoin) -> Result<(), SessionFailure> {
        let DestinationJoin {
            slot,
            control,
            local_lower,
            enabled,
        } = join;
        let actor = self
            .workers
            .destination
            .as_mut()
            .ok_or(SessionFailure::Destination)?;
        match actor.add_peer(slot, control, self.origin.clone(), local_lower, enabled) {
            Ok(()) => {
                if let Seat::Live(link) = &mut self.seats[seat(slot)] {
                    link.joined = true;
                }
                Ok(())
            }
            Err(PeerRefusal::Stopped(failure)) => Err(destination_failure(failure)),
            // Its receiver never ran, so no Left will come to free the slot.
            Err(PeerRefusal::Slot(_) | PeerRefusal::QueueFull) => {
                let result = Err(SessionFailure::Destination);
                self.core
                    .remove_link(self.workers.capture.as_mut(), slot, result)?;
                self.core
                    .on_destination(self.workers.capture.as_mut(), slot, SlotEvent::Left)
            }
        }
    }

    fn submit(&mut self, slot: FloorPeer, frame: Frame) -> Result<(), SessionFailure> {
        let actor = self
            .workers
            .destination
            .as_ref()
            .ok_or(SessionFailure::Destination)?;
        let failure = match actor.try_submit(slot, frame) {
            Ok(()) => return Ok(()),
            Err(PeerRefusal::Stopped(failure)) => return Err(destination_failure(failure)),
            Err(PeerRefusal::QueueFull) => SessionFailure::QueueFull,
            Err(PeerRefusal::Slot(_)) => SessionFailure::Destination,
        };
        self.core
            .remove_link(self.workers.capture.as_mut(), slot, Err(failure))
    }

    fn leave(&mut self, slot: FloorPeer) -> Result<(), SessionFailure> {
        self.leaving[seat(slot)] = Some(self.origin.elapsed());
        match self.workers.destination.as_mut() {
            Some(actor) => actor.remove_peer(slot).map_err(|refusal| match refusal {
                PeerRefusal::Stopped(failure) => destination_failure(failure),
                PeerRefusal::Slot(_) | PeerRefusal::QueueFull => SessionFailure::Destination,
            }),
            // Stopping native input frees every leaving slot.
            None => Ok(()),
        }
    }

    fn close(&mut self, slot: FloorPeer, end: LinkEnd) {
        let index = seat(slot);
        if !matches!(self.seats[index], Seat::Live(_)) {
            return;
        }
        let Seat::Live(link) = mem::replace(&mut self.seats[index], Seat::Empty) else {
            return;
        };
        let LinkEnd {
            peer,
            result,
            started_at,
            holds,
        } = end;
        let stats = self.close_link(&link, &result, started_at, holds);
        self.seats[index] = Seat::Closed(Box::new(Closed {
            peer,
            progress: link.progress.clone(),
            result,
            stats,
            joined: link.joined,
        }));
    }

    fn ended(&mut self, slot: FloorPeer) {
        let index = seat(slot);
        self.leaving[index] = None;
        if !matches!(self.seats[index], Seat::Closed(_)) {
            return;
        }
        let Seat::Closed(closed) = mem::replace(&mut self.seats[index], Seat::Empty) else {
            return;
        };
        lock(&self.shared.peers)[index] = None;
        self.report_end(slot, *closed);
        self.admit_queued();
    }

    /// The link's transport stats as it closes; the receiver's are added once its slot is free.
    fn close_link(
        &self,
        link: &Link,
        result: &Result<(), SessionFailure>,
        started_at: Option<Duration>,
        (holds, held_max): (u32, Duration),
    ) -> LinkStats {
        let now = self.origin.elapsed();
        let stats = link.io.link_stats(
            session_millis(started_at, now),
            self.gaps.max_micros(),
            (u64::from(holds), millis_u64(held_max)),
            &link.cancel,
        );
        link.io.close_after(
            result,
            link.progress.ended_deliberately(),
            link.end.control_change || link.cancel.is_stopping_for_control_change(),
        );
        stats
    }

    fn report_end(&self, slot: FloorPeer, closed: Closed) {
        let Closed {
            peer,
            progress,
            result,
            mut stats,
            joined,
        } = closed;
        if joined && let Some(actor) = &self.workers.destination {
            let receiver = actor.stats(slot);
            progress.record_receiver(receiver);
            stats.holds += receiver.holds;
            stats.held_max_millis = stats.held_max_millis.max(receiver.held_max_millis);
        }
        progress.record_link(stats);
        self.emit(HubEvent::PeerEnded {
            peer,
            result,
            diagnostics: progress.diagnostics(),
        });
    }

    /// Ends a link the core never ran.
    fn refuse(&self, link: Link, result: Result<(), SessionFailure>) {
        let stats = self.close_link(&link, &result, None, (0, Duration::ZERO));
        link.progress.record_link(stats);
        self.emit(HubEvent::PeerEnded {
            peer: link.peer,
            result,
            diagnostics: link.progress.diagnostics(),
        });
    }

    /// Native input stopped after the last link left: the slots it held are free, the claim is
    /// released and waiting links may start it again.
    fn native_stopped(&mut self) -> Result<(), SessionFailure> {
        self.core.native_stopped();
        self.execute()?;
        self.workers.release();
        self.emit(HubEvent::NativeIdle);
        self.admit_queued();
        Ok(())
    }

    fn emit(&self, event: HubEvent) {
        let _ = self.events.send(event);
    }

    /// Native input is released before any link closes, as a pairwise session's end does; every
    /// link then reports its end, and native cleanup must finish within `CLEANUP_WAIT`.
    async fn teardown(
        mut self,
        outcome: Result<PeerEnd, SessionFailure>,
    ) -> Result<(), SessionFailure> {
        self.commands.close();
        let (result, end) = match outcome {
            Ok(end) => (Ok(()), end),
            Err(failure) => (
                settle_end(
                    Err(failure),
                    false,
                    self.workers.capture_stop(),
                    self.workers.destination_failure(),
                ),
                PeerEnd::default(),
            ),
        };
        let native_ran = self.workers.state != NativeState::Idle;
        self.workers.stop();
        if self.workers.starting.is_some() {
            self.revocation.request_stop();
        }
        let links = result.and(Err(SessionFailure::Revoked));
        while let Ok(command) = self.commands.try_recv() {
            if let Command::AddPeer(arrival) = command {
                let Arrival {
                    session,
                    progress,
                    cancel,
                } = *arrival;
                session.connection.close(
                    0_u32.into(),
                    close_reason(
                        &links,
                        progress.ended_deliberately() || end.deliberate,
                        cancel.is_stopping_for_control_change() || end.control_change,
                    ),
                );
                self.emit(HubEvent::PeerEnded {
                    peer: session.peer.device_id,
                    result: links,
                    diagnostics: progress.diagnostics(),
                });
            }
        }
        let now = self.origin.elapsed();
        for index in 0..MAX_GROUP_PEERS {
            let slot = slot_at(index);
            let closed = match mem::replace(&mut self.seats[index], Seat::Empty) {
                Seat::Empty => continue,
                Seat::Closed(closed) => *closed,
                Seat::Live(mut link) => {
                    link.end_by(end);
                    let holds = self
                        .core
                        .source(slot)
                        .map_or((0, Duration::ZERO), |source| source.hold_stats(now));
                    let stats = self.close_link(&link, &links, link.started_at, holds);
                    Closed {
                        peer: link.peer,
                        progress: link.progress.clone(),
                        result: links,
                        stats,
                        joined: link.joined,
                    }
                }
            };
            self.report_end(slot, closed);
        }
        while let Some(queued) = self.queued.pop_front() {
            let mut link = queued.link;
            link.end_by(end);
            self.refuse(link, links);
        }
        *lock(&self.shared.peers) = Default::default();
        if let Err(pending) = self.workers.finish().await {
            log_cleanup(&pending, &result);
            return Err(SessionFailure::NativeCleanup);
        }
        self.workers.release();
        if native_ran {
            self.emit(HubEvent::NativeIdle);
        }
        result
    }
}

/// Takes the next item from `count` queues in turn, starting after the one served last.
fn next_in_turn<T>(
    cursor: &mut usize,
    count: usize,
    mut take: impl FnMut(usize) -> Option<T>,
) -> Option<(usize, T)> {
    for offset in 0..count {
        let index = (*cursor + offset) % count;
        if let Some(item) = take(index) {
            *cursor = (index + 1) % count;
            return Some((index, item));
        }
    }
    None
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Slot 1 is index 0; [`FloorPeer::NONE`] has none.
fn index_of(slot: FloorPeer) -> Option<usize> {
    usize::from(slot.get())
        .checked_sub(1)
        .filter(|&index| index < MAX_GROUP_PEERS)
}

/// The seat of a slot the core named.
fn seat(slot: FloorPeer) -> usize {
    index_of(slot).expect("a group slot")
}

fn slot_at(index: usize) -> FloorPeer {
    u8::try_from(index + 1)
        .ok()
        .and_then(FloorPeer::slot)
        .expect("a group slot index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
        session::SessionIo,
        session_actor::{EnvironmentFailure, WatchedDestination, WatchedEnvironment},
        session_handshake::{HandshakeConfig, device_id_from_fingerprint, negotiate},
        session_receiver::{
            DestinationAction, DestinationFailure, InputDestination, InputReceiver,
        },
        session_receiver_set::{ReceiverSet, SlotResult},
        session_source::PUSH_THROUGH_DISTANCE,
        session_source_runtime::CaptureRefusal,
    };
    use monhop_core::{
        Display, Edge, EdgeLink, HidUsage, LogicalSize, Machine, ModifierState, MouseButton,
        NativeSize, NormalizedSpan, Platform, capture::CaptureEvent, capture_physical::HeldInput,
    };
    use monhop_protocol::{Capabilities, ControlPermissions, DisplayDescription};
    use std::{
        cell::{Cell, Ref, RefCell},
        future::Future,
        net::{Ipv4Addr, SocketAddr},
        sync::atomic::{AtomicBool, AtomicU32, Ordering},
    };
    use tokio::{sync::oneshot, task::LocalSet, time::timeout};

    const LETTER_A: u16 = 0x04;
    const LETTER_B: u16 = 0x05;
    const SHIFT: u16 = 0xe1;
    const LEFT_ARROW: u16 = 0x50;
    /// Far longer than any loopback exchange: what has not happened by then never will.
    const PATIENCE: Duration = Duration::from_secs(5);
    /// Long enough for anything the hub owes to be reported.
    const QUIET: Duration = Duration::from_millis(40);
    const DELIBERATE: PeerEnd = PeerEnd {
        deliberate: true,
        control_change: false,
    };

    // The app holds command handles on any thread.
    const _: () = {
        const fn shared<T: Clone + Send + Sync>() {}
        shared::<ShareHub>();
    };

    /// Runs on one thread with the hub and every member, as the network thread does, owning
    /// native input for the whole scenario.
    fn scenario(test: impl Future<Output = ()>) {
        let _native = lock(&crate::NATIVE_OWNERSHIP_TEST_LOCK);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let limited = timeout(Duration::from_secs(30), test);
                LocalSet::new().run_until(limited).await
            })
            .expect("the scenario finished in time");
    }

    struct Computer {
        identity: DeviceIdentity,
        pin: VerifiedPeer,
        device: DeviceId,
        displays: DisplayTopology,
    }

    fn display(index: usize) -> DisplayId {
        DisplayId(u64::try_from(index).unwrap() + 1)
    }

    /// Computer `index` has one 100 px display, `display(index)`.
    fn computer(index: usize) -> Computer {
        let identity = DeviceIdentity::generate().expect("identity");
        let pin = VerifiedPeer::from_certificate_der(
            identity.certificate_der(),
            &identity.fingerprint().full_hex(),
        )
        .expect("a full matching pin");
        let displays = DisplayTopology::new(vec![DisplayDescription {
            id: display(index),
            name: format!("display-{index}"),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::default(),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .expect("displays");
        Computer {
            device: device_id_from_fingerprint(identity.fingerprint()),
            identity,
            pin,
            displays,
        }
    }

    /// Each computer's right edge leads to the next one's left edge, the last back to the first.
    fn ring(computers: &[Computer]) -> Topology {
        let full = NormalizedSpan::new(0.0, 1.0).unwrap();
        let link = |from, from_edge, to, to_edge| {
            EdgeLink::new(
                display(from),
                from_edge,
                full,
                display(to),
                to_edge,
                full,
                1.0,
            )
            .unwrap()
        };
        let count = computers.len();
        Topology::new(
            computers
                .iter()
                .map(|computer| Machine::new(computer.device, Platform::MacOs))
                .collect(),
            computers
                .iter()
                .enumerate()
                .map(|(index, computer)| {
                    Display::new(
                        display(index),
                        computer.device,
                        format!("display-{index}"),
                        NativeSize::new(100, 100),
                        LogicalSize::new(100.0, 100.0),
                        Point::default(),
                        1.0,
                        None,
                        true,
                    )
                })
                .collect(),
            (0..count)
                .flat_map(|index| {
                    let next = (index + 1) % count;
                    [
                        link(index, Edge::Right, next, Edge::Left),
                        link(next, Edge::Left, index, Edge::Right),
                    ]
                })
                .collect(),
        )
        .unwrap()
    }

    /// Computer 0 runs the hub, with every other computer a member in order.
    fn config(computers: &[Computer], group: &Topology) -> HubConfig {
        HubConfig {
            local: computers[0].device,
            local_displays: computers[0].displays.clone(),
            group: group.clone(),
            members: computers[1..].iter().map(|member| member.device).collect(),
            generation: 1,
            revocation: RevocationSignal::default(),
        }
    }

    fn share<'a>(local: &'a Computer, peer: &'a Computer) -> HandshakeConfig<'a> {
        let features =
            Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
                .unwrap();
        HandshakeConfig::new(
            &local.identity,
            &peer.pin,
            Platform::MacOs,
            Platform::MacOs,
            features,
            features,
            &local.displays,
            ControlPermissions::BOTH,
            SessionPurpose::Share,
        )
        .unwrap()
    }

    /// One Share session over loopback, as each of its two computers holds it.
    struct Pair {
        ours: NegotiatedSession,
        theirs: NegotiatedSession,
        endpoints: [quinn::Endpoint; 2],
    }

    async fn negotiated(ours: &Computer, theirs: &Computer) -> Pair {
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = quinn::Endpoint::server(
            SecureQuicConfig::server(&theirs.identity, &ours.pin).unwrap(),
            loopback,
        )
        .unwrap();
        let mut dialer = quinn::Endpoint::client(loopback).unwrap();
        dialer.set_default_client_config(
            SecureQuicConfig::client(&ours.identity, &theirs.pin).unwrap(),
        );
        let connecting = dialer
            .connect(listener.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
            .unwrap();
        let (dialed, accepted) = tokio::join!(async { connecting.await.unwrap() }, async {
            listener.accept().await.unwrap().await.unwrap()
        });
        let (ours_session, theirs_session) = tokio::join!(
            negotiate(dialed, share(ours, theirs)),
            negotiate(accepted, share(theirs, ours)),
        );
        Pair {
            ours: ours_session.unwrap(),
            theirs: theirs_session.unwrap(),
            endpoints: [dialer, listener],
        }
    }

    /// Native capture that grants every route change at once and queues its barrier behind the
    /// records already captured.
    struct Model {
        floor: SharedFloor,
        route: (bool, u64),
        queue: VecDeque<CapturedEvent>,
        /// The OS pointer, which local absolute motion and restores move.
        pointer: Point,
    }

    impl Model {
        fn new(floor: SharedFloor) -> Self {
            Self {
                floor,
                route: (false, 0),
                queue: VecDeque::new(),
                pointer: Point::new(50.0, 50.0),
            }
        }

        fn capture(&mut self, event: CaptureEvent) {
            if let CaptureEvent::LogicalAbsoluteMotion { x, y } = event
                && !self.route.0
            {
                self.pointer = Point::new(x, y);
            }
            self.queue.push_back(CapturedEvent {
                event,
                routing_revision: self.route.1,
                remote: self.route.0,
                floor_generation: self.floor.snapshot().generation,
            });
        }

        fn reroute(&mut self, remote: bool) -> u64 {
            self.route = (remote, self.route.1 + 1);
            let revision = self.route.1;
            self.capture(CaptureEvent::RouteChanged { remote, revision });
            revision
        }
    }

    impl CaptureControl for Model {
        fn activate_remote(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
            Ok(self.reroute(true))
        }
        fn restore_local_at(&mut self, _: u64, position: Point) -> Result<u64, CaptureRefusal> {
            self.pointer = position;
            Ok(self.reroute(false))
        }
        fn renew_suppression(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
            Ok(self.route.1)
        }
        fn completed_control_revision(&self) -> Option<u64> {
            Some(self.route.1)
        }
        fn is_ready_for_suppression(&self) -> bool {
            true
        }
        fn blocking_presses(&self) -> Vec<HeldInput> {
            Vec::new()
        }
        fn stop_reason(&self) -> Option<StopReason> {
            None
        }
        fn request_stop(&self) {}
    }

    /// The hub computer's native input: a capture the test feeds, and a real destination actor
    /// over a destination that accepts everything.
    #[derive(Clone, Default)]
    struct FakeInput(Arc<Faked>);

    #[derive(Default)]
    struct Faked {
        model: Mutex<Option<Model>>,
        waker: Mutex<Option<Wake>>,
        starts: AtomicU32,
        /// While set, a stopped capture has not finished.
        finish_held: AtomicBool,
        /// Presses held since capture started; suppression is refused until none are.
        blocking: Mutex<Vec<HeldInput>>,
    }

    impl FakeInput {
        fn capture(&self, event: CaptureEvent) {
            lock(&self.0.model)
                .as_mut()
                .expect("capture runs")
                .capture(event);
            self.wake();
        }

        fn wake(&self) {
            let waker = lock(&self.0.waker).clone();
            if let Some(waker) = waker {
                waker();
            }
        }

        fn starts(&self) -> u32 {
            self.0.starts.load(Ordering::Acquire)
        }

        fn hold_finish(&self, held: bool) {
            self.0.finish_held.store(held, Ordering::Release);
        }

        fn hold_since_start(&self, presses: &[HeldInput]) {
            *lock(&self.0.blocking) = presses.to_vec();
        }
    }

    struct FakeCapture {
        input: FakeInput,
        stopped: AtomicBool,
        _permit: CapturePermit,
    }

    impl FakeCapture {
        fn route(
            &self,
            command: impl FnOnce(&mut Model) -> Result<u64, CaptureRefusal>,
        ) -> Result<u64, CaptureRefusal> {
            let ticket = command(lock(&self.input.0.model).as_mut().expect("capture runs"));
            self.input.wake();
            ticket
        }
    }

    impl CaptureControl for FakeCapture {
        fn activate_remote(
            &mut self,
            generation: u64,
            ttl: Duration,
        ) -> Result<u64, CaptureRefusal> {
            self.route(|model| model.activate_remote(generation, ttl))
        }
        fn restore_local_at(
            &mut self,
            generation: u64,
            position: Point,
        ) -> Result<u64, CaptureRefusal> {
            self.route(|model| model.restore_local_at(generation, position))
        }
        fn renew_suppression(
            &mut self,
            generation: u64,
            ttl: Duration,
        ) -> Result<u64, CaptureRefusal> {
            self.route(|model| model.renew_suppression(generation, ttl))
        }
        fn completed_control_revision(&self) -> Option<u64> {
            lock(&self.input.0.model)
                .as_ref()?
                .completed_control_revision()
        }
        fn is_ready_for_suppression(&self) -> bool {
            lock(&self.input.0.blocking).is_empty()
        }
        fn blocking_presses(&self) -> Vec<HeldInput> {
            lock(&self.input.0.blocking).clone()
        }
        fn stop_reason(&self) -> Option<StopReason> {
            self.stopped
                .load(Ordering::Acquire)
                .then_some(StopReason::Requested)
        }
        fn request_stop(&self) {
            self.stopped.store(true, Ordering::Release);
        }
    }

    impl HubCapture for FakeCapture {
        fn set_waker(&self, waker: Wake) {
            *lock(&self.input.0.waker) = Some(waker);
        }
        fn try_next_event(&mut self) -> Result<Option<CapturedEvent>, StopReason> {
            Ok(lock(&self.input.0.model)
                .as_mut()
                .and_then(|model| model.queue.pop_front()))
        }
        fn is_finished(&self) -> bool {
            self.stopped.load(Ordering::Acquire)
                && !self.input.0.finish_held.load(Ordering::Acquire)
        }
        fn finish(&mut self) -> Result<(), NativeCaptureError> {
            if self.is_finished() {
                Ok(())
            } else {
                Err(NativeCaptureError::CleanupPending)
            }
        }
    }

    impl NativeInput for FakeInput {
        type Capture = FakeCapture;

        fn start_capture(
            &self,
            _: u64,
            _: &DisplayTopology,
            _: RevocationSignal,
            permit: CapturePermit,
            gate: TakeBackGate,
        ) -> Result<FakeCapture, SessionFailure> {
            self.0.starts.fetch_add(1, Ordering::AcqRel);
            *lock(&self.0.model) = Some(Model::new(gate.floor().clone()));
            Ok(FakeCapture {
                input: self.clone(),
                stopped: AtomicBool::new(false),
                _permit: permit,
            })
        }

        fn start_destination(
            &self,
            displays: DisplayTopology,
            revocation: RevocationSignal,
            permit: InjectionPermit,
            gate: TakeBackGate,
        ) -> Result<HubDestinationActor, ActorFailure> {
            HubDestinationActor::start_after_local_enable(
                displays,
                revocation,
                permit,
                gate,
                |permit| Ok(Accepting { _permit: permit }),
            )
        }

        fn pointer(&self) -> Option<Point> {
            lock(&self.0.model).as_ref().map(|model| model.pointer)
        }
    }

    struct Accepting {
        _permit: InjectionPermit,
    }

    impl InputDestination for Accepting {
        fn apply(&mut self, _: DestinationAction) -> Result<(), DestinationFailure> {
            Ok(())
        }
    }

    impl WatchedDestination for Accepting {
        type Environment = Steady;
        fn environment(&mut self) -> Steady {
            Steady
        }
    }

    struct Steady;

    impl WatchedEnvironment for Steady {
        fn validate(&mut self) -> Result<(), EnvironmentFailure> {
            Ok(())
        }
    }

    /// Native injection on a member: what reached it while its gate admitted input.
    struct Injector {
        gate: TakeBackGate,
        actions: Vec<DestinationAction>,
    }

    impl InputDestination for Injector {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            if action.is_release() || self.gate.admits_injection() {
                self.actions.push(action);
            }
            Ok(())
        }
    }

    /// A member computer: a hub core of its own over its one link, with native input faked in
    /// place and its receiver run synchronously.
    struct Peer {
        core: HubCore,
        origin: SessionClock,
        capture: Model,
        set: ReceiverSet,
        injector: Injector,
        displays: DisplayTopology,
        native: bool,
        leaving: Vec<FloorPeer>,
        events: VecDeque<(FloorPeer, SlotEvent)>,
        /// Frames for the link: (outbound, frame).
        out: Vec<(bool, Frame)>,
        closed: bool,
    }

    impl Peer {
        fn new(
            computer: &Computer,
            group: Topology,
            hub: DeviceId,
            session: &NegotiatedSession,
        ) -> Self {
            let origin = SessionClock::try_now().unwrap();
            let config = CoreConfig {
                local: computer.device,
                group,
                members: vec![hub],
                generation: 1,
                double_click: Duration::from_millis(500),
            };
            let mut core = HubCore::new(config, origin.clone()).unwrap();
            assert_eq!(
                core.add_link(Setup::of(session).link(hub)),
                Ok(FloorPeer::SOLE)
            );
            let gate = core.gate().clone();
            Self {
                capture: Model::new(gate.floor().clone()),
                set: ReceiverSet::new(gate.clone()),
                injector: Injector {
                    gate,
                    actions: Vec::new(),
                },
                displays: computer.displays.clone(),
                core,
                origin,
                native: false,
                leaving: Vec::new(),
                events: VecDeque::new(),
                out: Vec::new(),
                closed: false,
            }
        }

        fn keys(&self) -> Vec<(u16, bool)> {
            self.injector
                .actions
                .iter()
                .filter_map(|action| match action {
                    DestinationAction::Key { usage, pressed } => Some((usage.0, *pressed)),
                    _ => None,
                })
                .collect()
        }

        fn released(&self) -> usize {
            self.injector
                .actions
                .iter()
                .filter(|action| **action == DestinationAction::ReleaseAll)
                .count()
        }

        fn frame(&mut self, frame: Frame) {
            let capture = self.native.then_some(&mut self.capture);
            let received = self.core.on_frame(capture, FloorPeer::SOLE, frame);
            assert_eq!(received, Ok(()), "the member's core runs");
            self.pump();
        }

        fn tick(&mut self) {
            let now = self.origin.elapsed();
            if self.native {
                for slot in self.set.occupied() {
                    if !self.leaving.contains(&slot) {
                        let ticked = self.set.tick(slot, now, &mut self.injector);
                        self.respond(slot, ticked);
                    } else if self.set.remove(slot, &mut self.injector) {
                        self.leaving.retain(|leaving| *leaving != slot);
                        self.events.push_back((slot, SlotEvent::Left));
                    }
                }
            }
            let pointer = self.capture.pointer;
            let capture = self.native.then_some(&mut self.capture);
            let ticked = self.core.on_tick(capture, || Some(pointer));
            assert_eq!(ticked, Ok(()), "the member's core runs");
            self.pump();
        }

        fn pump(&mut self) {
            loop {
                let actions: Vec<HubAction> = self.core.actions().collect();
                let mut busy = !actions.is_empty();
                for action in actions {
                    self.execute(action);
                }
                while let Some((slot, event)) = self.events.pop_front() {
                    busy = true;
                    let capture = self.native.then_some(&mut self.capture);
                    let handled = self.core.on_destination(capture, slot, event);
                    assert_eq!(handled, Ok(()), "the member's core runs");
                }
                while self.native
                    && let Some(record) = self.capture.queue.pop_front()
                {
                    busy = true;
                    let captured = self.core.on_captured(&mut self.capture, record);
                    assert_eq!(captured, Ok(()), "the member's core runs");
                }
                if !busy {
                    return;
                }
            }
        }

        fn execute(&mut self, action: HubAction) {
            let now = self.origin.elapsed();
            match action {
                HubAction::SendOutbound { frame, .. } => self.out.push((true, frame)),
                HubAction::SendInbound { frame, .. } => self.out.push((false, frame)),
                HubAction::StartNative => {
                    self.native = true;
                    self.core.native_ready();
                }
                HubAction::StopNative => {
                    self.native = false;
                    self.core.native_stopped();
                }
                HubAction::JoinDestination(join) => {
                    let DestinationJoin {
                        slot,
                        control,
                        local_lower,
                        enabled,
                    } = *join;
                    let heartbeat = control.next_heartbeat_out;
                    let receiver =
                        InputReceiver::after_startup(self.displays.clone(), control, now)
                            .expect("the member's receiver starts");
                    self.set
                        .add(slot, receiver, local_lower, enabled, heartbeat)
                        .expect("the member's slot is free");
                }
                HubAction::Submit { slot, frame } => {
                    if self.leaving.contains(&slot) {
                        return;
                    }
                    let received = self.set.receive(slot, &frame, now, &mut self.injector);
                    let stopped = received.is_err();
                    self.respond(slot, received);
                    if !stopped && let Err(failure) = self.set.flush(slot, &mut self.injector) {
                        self.respond(slot, Err(failure));
                    }
                }
                HubAction::LeaveDestination { slot } => self.leaving.push(slot),
                HubAction::CloseLink { .. } => self.closed = true,
                HubAction::LinkEnded { .. } | HubAction::LinkStarted { .. } => {}
            }
        }

        fn respond(&mut self, slot: FloorPeer, result: SlotResult) {
            match result {
                Ok(Some(frame)) => self.events.push_back((slot, SlotEvent::Response(frame))),
                Ok(None) => {}
                Err(failure) => {
                    if !self.leaving.contains(&slot) {
                        self.events.push_back((slot, SlotEvent::Failed(failure)));
                        self.leaving.push(slot);
                    }
                }
            }
        }
    }

    /// A member's network loop: its link's frames and a 1 ms tick, from `go` until the link ends.
    async fn drive(peer: Rc<RefCell<Peer>>, mut io: SessionIo, go: oneshot::Receiver<()>) {
        if go.await.is_err() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        while !peer.borrow().closed {
            tokio::select! {
                frame = io.next_frame() => match frame {
                    Ok(frame) => peer.borrow_mut().frame(frame),
                    Err(_) => break,
                },
                _ = tick.tick() => peer.borrow_mut().tick(),
            }
            let out = mem::take(&mut peer.borrow_mut().out);
            for (outbound, frame) in out {
                let sent = if outbound {
                    io.send_outbound(frame)
                } else {
                    io.send_inbound(frame)
                };
                if sent.is_err() {
                    break;
                }
            }
        }
        peer.borrow_mut().closed = true;
    }

    /// Computer 0 runs the hub over a fake native input; every other computer is a member run
    /// by its own core, linked to the hub over loopback.
    struct Group {
        computers: Vec<Computer>,
        topology: Topology,
        input: FakeInput,
        hub: ShareHub,
        events: HubEvents,
        run: JoinHandle<Result<(), SessionFailure>>,
        /// The hub loop's passes so far.
        passes: Rc<Cell<u32>>,
        peers: Vec<Option<Rc<RefCell<Peer>>>>,
        /// The hub's progress for each member's link, as the app reads it.
        progress: Vec<Option<SessionProgress>>,
        endpoints: Vec<[quinn::Endpoint; 2]>,
    }

    impl Group {
        fn start(count: usize) -> Self {
            let computers: Vec<Computer> = (0..count).map(computer).collect();
            let topology = ring(&computers);
            let input = FakeInput::default();
            let (hub, events, run) = start_with(
                config(&computers, &topology),
                input.clone(),
                Duration::from_millis(500),
            )
            .expect("the hub starts");
            Self {
                passes: run.passes.clone(),
                run: tokio::task::spawn_local(run.run()),
                peers: (0..count).map(|_| None).collect(),
                progress: (0..count).map(|_| None).collect(),
                endpoints: Vec::new(),
                computers,
                topology,
                input,
                hub,
                events,
            }
        }

        fn device(&self, id: usize) -> DeviceId {
            self.computers[id].device
        }

        async fn negotiate(&self, id: usize) -> Pair {
            negotiated(&self.computers[0], &self.computers[id]).await
        }

        /// Hands the hub its side of `pair`; computer `id` runs the other once `go` fires.
        fn attach(&mut self, id: usize, pair: Pair) -> oneshot::Sender<()> {
            let Pair {
                ours,
                theirs,
                endpoints,
            } = pair;
            let peer = Peer::new(
                &self.computers[id],
                self.topology.clone(),
                self.device(0),
                &theirs,
            );
            let peer = Rc::new(RefCell::new(peer));
            let (go, wait) = oneshot::channel();
            let io = SessionIo::new(theirs, SessionProgress::default());
            tokio::task::spawn_local(drive(peer.clone(), io, wait));
            self.peers[id] = Some(peer);
            self.endpoints.push(endpoints);
            let progress = SessionProgress::default();
            self.progress[id] = Some(progress.clone());
            self.hub
                .add_peer(ours, progress, RevocationSignal::default())
                .expect("the hub runs");
            go
        }

        fn blocking(&self, id: usize) -> Vec<HeldInput> {
            self.progress[id]
                .as_ref()
                .expect("a linked member")
                .blocking_presses()
        }

        async fn join(&mut self, id: usize) {
            let pair = self.negotiate(id).await;
            let _ = self.attach(id, pair).send(());
            self.started(id).await;
        }

        async fn event(&mut self) -> HubEvent {
            timeout(PATIENCE, self.events.recv())
                .await
                .expect("an event in time")
                .expect("the hub runs")
        }

        async fn started(&mut self, id: usize) {
            match self.event().await {
                HubEvent::PeerStarted { peer } => assert_eq!(peer, self.device(id)),
                other => panic!("expected computer {id} to start: {other:?}"),
            }
        }

        async fn ended(&mut self, id: usize) -> Result<(), SessionFailure> {
            match self.event().await {
                HubEvent::PeerEnded { peer, result, .. } => {
                    assert_eq!(peer, self.device(id));
                    result
                }
                other => panic!("expected computer {id} to end: {other:?}"),
            }
        }

        async fn native_idle(&mut self) {
            let event = self.event().await;
            assert!(matches!(event, HubEvent::NativeIdle), "{event:?}");
        }

        async fn quiet(&mut self) {
            if let Ok(event) = timeout(QUIET, self.events.recv()).await {
                panic!("nothing was due: {event:?}");
            }
        }

        fn peer(&self, id: usize) -> Ref<'_, Peer> {
            self.peers[id].as_ref().expect("a linked member").borrow()
        }

        async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + PATIENCE;
            while !done(self) {
                assert!(Instant::now() < deadline, "{what} never happened");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }

        async fn controls(&self, id: usize) {
            self.until(&format!("the hub controls computer {id}"), |group| {
                let status = group.hub.status();
                status.floor == FloorState::Sending && status.floor_peer == Some(group.device(id))
            })
            .await;
        }

        fn capture(&self, event: CaptureEvent) {
            self.input.capture(event);
        }

        fn key(&self, usage: u16, pressed: bool, modifiers: u8) {
            self.capture(CaptureEvent::Key {
                usage: HidUsage(usage),
                pressed,
                repeat: false,
                modifiers: ModifierState(modifiers),
            });
        }

        fn tap(&self, usage: u16) {
            self.key(usage, true, 0);
            self.key(usage, false, 0);
        }

        /// Rests on the right or left edge and pushes through it.
        fn cross(&self, right: bool) {
            let (x, direction) = if right { (99.0, 1.0) } else { (0.0, -1.0) };
            self.capture(CaptureEvent::LogicalAbsoluteMotion { x, y: 50.0 });
            self.capture(moved(direction * PUSH_THROUGH_DISTANCE));
        }

        /// Stops the hub deliberately; it ends cleanly and the native claim is free.
        async fn finish(mut self) {
            self.hub.stop(DELIBERATE);
            let ended = timeout(PATIENCE, &mut self.run)
                .await
                .expect("the hub stops in time")
                .expect("the hub's task");
            assert_eq!(ended, Ok(()));
            assert!(!NativeSessionClaim::is_claimed());
        }
    }

    fn moved(dx: f64) -> CaptureEvent {
        CaptureEvent::LogicalRelativeMotion { dx, dy: 0.0 }
    }

    #[test]
    fn hub_commands_to_a_closed_hub_are_errors() {
        scenario(async {
            let mut group = Group::start(2);
            group.hub.stop(DELIBERATE);
            let ended = timeout(PATIENCE, &mut group.run).await.unwrap().unwrap();
            assert_eq!(ended, Ok(()));
            assert!(group.events.recv().await.is_none(), "no peer, no report");

            let Pair {
                ours,
                theirs,
                endpoints: _endpoints,
            } = group.negotiate(1).await;
            let progress = SessionProgress::default();
            let cancel = RevocationSignal::default();
            assert_eq!(group.hub.add_peer(ours, progress, cancel), Err(HubClosed));
            // The refused session is closed, never left open.
            assert!(timeout(PATIENCE, theirs.connection.closed()).await.is_ok());
            assert_eq!(
                group.hub.remove_peer(group.device(1), DELIBERATE),
                Err(HubClosed)
            );
            assert_eq!(group.hub.request_local(), Err(HubClosed));
            group.hub.stop(PeerEnd::default());
            let status = group.hub.status();
            assert_eq!((status.floor, status.floor_peer), (FloorState::Free, None));
            assert!(status.peers.is_empty());

            // A run dropped before it ran closes its hub the same way.
            let (hub, mut events, run) = start_with(
                config(&group.computers, &group.topology),
                FakeInput::default(),
                Duration::from_millis(500),
            )
            .unwrap();
            drop(run);
            assert_eq!(hub.request_local(), Err(HubClosed));
            assert!(events.recv().await.is_none());
        });
    }

    #[test]
    fn a_flooding_peer_does_not_starve_another_peers_frames() {
        let mut queues: [VecDeque<u32>; MAX_GROUP_PEERS] = Default::default();
        queues[0].extend(0..1_000);
        queues[4].extend([1, 2]);
        let mut cursor = 0;
        let served: Vec<usize> = std::iter::from_fn(|| {
            next_in_turn(&mut cursor, MAX_GROUP_PEERS, |index| {
                queues[index].pop_front()
            })
        })
        .take(6)
        .map(|(index, _)| index)
        .collect();
        assert_eq!(served, [0, 4, 0, 4, 0, 0]);
        assert_eq!(queues[0].len(), 996);
    }

    #[test]
    fn native_workers_start_with_the_first_ready_peer_and_stop_after_the_last_leaves() {
        scenario(async {
            let mut group = Group::start(3);
            let pair = group.negotiate(1).await;
            let go = group.attach(1, pair);
            group
                .until("the hub holds computer 1's link", |group| {
                    !group.hub.status().peers.is_empty()
                })
                .await;
            // A link whose peer has not answered yet claims nothing.
            tokio::time::sleep(QUIET).await;
            assert_eq!(group.input.starts(), 0);
            assert!(!NativeSessionClaim::is_claimed());
            go.send(()).unwrap();
            group.started(1).await;
            assert_eq!(group.input.starts(), 1);
            assert!(NativeSessionClaim::is_claimed());
            group.join(2).await;
            assert_eq!(group.input.starts(), 1, "one native run serves every peer");

            group.hub.remove_peer(group.device(1), DELIBERATE).unwrap();
            assert_eq!(group.ended(1).await, Err(SessionFailure::Revoked));
            group.quiet().await;
            assert!(NativeSessionClaim::is_claimed());

            group.hub.remove_peer(group.device(2), DELIBERATE).unwrap();
            assert_eq!(group.ended(2).await, Err(SessionFailure::Revoked));
            group.native_idle().await;
            assert!(!NativeSessionClaim::is_claimed());
            assert!(group.hub.status().peers.is_empty());
            group.finish().await;
        });
    }

    #[test]
    fn a_peer_joining_mid_session_inherits_held_modifiers_and_the_remote_route() {
        scenario(async {
            let mut group = Group::start(3);
            group.join(1).await;
            group.cross(true);
            group.controls(1).await;
            group.key(SHIFT, true, ModifierState::LEFT_SHIFT);
            group
                .until("Shift reaches computer 1", |group| {
                    group.peer(1).keys() == [(SHIFT, true)]
                })
                .await;

            // Computer 2 joins while input is remote and Shift is down.
            group.join(2).await;
            // Pushed on through computer 1's far edge, the pointer goes straight to computer 2.
            group.capture(moved(98.0));
            group.capture(moved(PUSH_THROUGH_DISTANCE));
            group.controls(2).await;
            group
                .until("Shift carries over to computer 2", |group| {
                    group.peer(2).keys() == [(SHIFT, true)]
                })
                .await;
            assert_eq!(group.peer(1).released(), 1);
            group.key(SHIFT, false, 0);
            group
                .until("Shift comes up on computer 2", |group| {
                    group.peer(2).keys() == [(SHIFT, true), (SHIFT, false)]
                })
                .await;
            assert_eq!(group.peer(1).keys(), [(SHIFT, true)]);
            group.quiet().await;
            group.finish().await;
        });
    }

    #[test]
    fn every_link_names_the_presses_that_keep_the_pointer_home_until_they_are_released() {
        scenario(async {
            let mut group = Group::start(3);
            group.join(1).await;
            group.join(2).await;
            let held = [
                HeldInput::Key(HidUsage(LEFT_ARROW)),
                HeldInput::Button(MouseButton::Middle),
            ];
            group.input.hold_since_start(&held);
            group
                .until("both links name the held presses", |group| {
                    [1, 2].into_iter().all(|id| group.blocking(id) == held)
                })
                .await;
            group.input.hold_since_start(&[]);
            group
                .until("the release clears them from both links", |group| {
                    [1, 2].into_iter().all(|id| group.blocking(id).is_empty())
                })
                .await;
            group.finish().await;
        });
    }

    #[test]
    fn a_peer_ending_while_another_starts_never_disturbs_the_starting_one() {
        scenario(async {
            let mut group = Group::start(3);
            group.join(1).await;
            let pair = group.negotiate(2).await;
            let go = group.attach(2, pair);
            group
                .until("the hub holds computer 2's starting link", |group| {
                    group.hub.status().peers.len() == 2
                })
                .await;
            group.hub.remove_peer(group.device(1), DELIBERATE).unwrap();
            assert_eq!(group.ended(1).await, Err(SessionFailure::Revoked));
            go.send(()).unwrap();
            group.started(2).await;
            assert_eq!(group.input.starts(), 1, "native input ran on throughout");

            group.cross(false);
            group.controls(2).await;
            group.tap(LETTER_A);
            group
                .until("computer 2 types", |group| {
                    group.peer(2).keys() == [(LETTER_A, true), (LETTER_A, false)]
                })
                .await;
            group.finish().await;
        });
    }

    #[test]
    fn a_peer_added_while_native_stops_is_queued_and_joins_after() {
        scenario(async {
            let mut group = Group::start(3);
            group.join(1).await;
            let pair = group.negotiate(2).await;
            group.input.hold_finish(true);
            group.hub.remove_peer(group.device(1), DELIBERATE).unwrap();
            let _ = group.attach(2, pair).send(());
            // Native input is still stopping: the leaver's end and the newcomer both wait.
            group.quiet().await;
            let waiting = group.hub.status();
            assert!(
                !waiting
                    .peers
                    .iter()
                    .any(|(peer, _)| *peer == group.device(2))
            );
            assert!(NativeSessionClaim::is_claimed());

            group.input.hold_finish(false);
            assert_eq!(group.ended(1).await, Err(SessionFailure::Revoked));
            group.native_idle().await;
            group.started(2).await;
            assert_eq!(group.input.starts(), 2);
            group.finish().await;
        });
    }

    #[test]
    fn closing_one_link_keeps_the_others_running() {
        scenario(async {
            let mut group = Group::start(3);
            group.join(1).await;
            group.join(2).await;
            group.cross(false);
            group.controls(2).await;
            group.tap(LETTER_A);
            group
                .until("computer 2 types", |group| {
                    group.peer(2).keys() == [(LETTER_A, true), (LETTER_A, false)]
                })
                .await;

            group.hub.remove_peer(group.device(1), DELIBERATE).unwrap();
            assert_eq!(group.ended(1).await, Err(SessionFailure::Revoked));
            group
                .until("computer 1 sees its link close", |group| {
                    group.peer(1).closed
                })
                .await;
            group.tap(LETTER_B);
            group
                .until("computer 2 still types", |group| {
                    group.peer(2).keys()
                        == [
                            (LETTER_A, true),
                            (LETTER_A, false),
                            (LETTER_B, true),
                            (LETTER_B, false),
                        ]
                })
                .await;
            let status = group.hub.status();
            assert_eq!(
                (status.floor, status.floor_peer),
                (FloorState::Sending, Some(group.device(2)))
            );
            let peers: Vec<DeviceId> = status.peers.iter().map(|(peer, _)| *peer).collect();
            assert_eq!(peers, [group.device(2)]);
            assert_eq!(status.active_is_local, Some(false));
            group.quiet().await;
            group.finish().await;
        });
    }

    /// Long enough for a ticking loop to make many passes, even on a coarse OS timer.
    const IDLE: Duration = Duration::from_millis(200);

    #[test]
    fn an_idle_hub_does_not_wake_until_a_command_arrives() {
        scenario(async {
            let mut group = Group::start(2);
            tokio::time::sleep(IDLE).await;
            let started = group.passes.get();
            assert!(started <= 2, "an idle hub made {started} passes");

            group.hub.request_local().unwrap();
            group
                .until("the hub serves the command", |group| {
                    group.passes.get() > started
                })
                .await;
            let served = group.passes.get();
            tokio::time::sleep(IDLE).await;
            let parked = group.passes.get() - served;
            assert!(
                parked <= 2,
                "the hub made {parked} passes after the command"
            );

            // A link brings the tick back, and the hub parks again once native input is idle.
            group.join(1).await;
            let joined = group.passes.get();
            tokio::time::sleep(IDLE).await;
            let ticked = group.passes.get() - joined;
            assert!(ticked >= 5, "a hub with a link made only {ticked} passes");
            group.hub.remove_peer(group.device(1), DELIBERATE).unwrap();
            assert_eq!(group.ended(1).await, Err(SessionFailure::Revoked));
            group.native_idle().await;
            let idle = group.passes.get();
            tokio::time::sleep(IDLE).await;
            let parked = group.passes.get() - idle;
            assert!(parked <= 2, "the hub made {parked} passes once idle again");
            group.finish().await;
        });
    }

    #[test]
    fn an_idle_hub_still_ends_on_its_revocation() {
        scenario(async {
            let computers: Vec<Computer> = (0..2).map(computer).collect();
            let config = config(&computers, &ring(&computers));
            let revocation = config.revocation.clone();
            let (_hub, _events, run) =
                start_with(config, FakeInput::default(), Duration::from_millis(500)).unwrap();
            let mut run = tokio::task::spawn_local(run.run());
            tokio::time::sleep(QUIET).await;
            revocation.revoke();
            let ended = timeout(PATIENCE, &mut run)
                .await
                .expect("the hub ends in time")
                .expect("the hub's task");
            assert_eq!(ended, Err(SessionFailure::Revoked));
            assert!(!NativeSessionClaim::is_claimed());
        });
    }
}
