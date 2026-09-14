//! Runtime for an explicitly enabled, authenticated input session.

#[cfg(any(windows, target_os = "macos"))]
pub use crate::session_source_runtime::run_source;
#[cfg(any(windows, target_os = "macos"))]
pub use crate::session_source_runtime::run_trial_source;
use crate::{
    session_actor::DestinationActor,
    session_clock::{SessionClock, micros_u64, millis_u64},
    session_handshake::NegotiatedSession,
    session_startup::{StartupControl, startup_failure},
    session_trial::TrialInputGuard,
    session_wire::{FrameReader, FrameWriter, decode_datagram, is_heartbeat},
};
use monhop_core::{DisplayId, NativeInputOwnership, RevocationSignal};
use monhop_protocol::{Frame, Message};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

const OUTBOUND_CAPACITY: usize = 512;
pub const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Default)]
pub struct SessionProgress(Arc<ProgressState>);
#[derive(Default)]
struct ProgressState {
    started: AtomicBool,
    sent: AtomicU64,
    received: AtomicU64,
    rtt_us: AtomicU64,
    display: AtomicU64,
    route: AtomicU64,
    /// The peer went silent past the deadline; input is local until the link proves itself.
    held: AtomicBool,
    /// The user ended this session (pause, switch, quit): its close tells the peer it was no failure.
    deliberate: AtomicBool,
    receiver: std::sync::Mutex<Option<crate::session_actor::ReceiverStats>>,
    link: std::sync::Mutex<Option<LinkStats>>,
}

/// How the QUIC connection ended, as seen locally; peer-supplied text is classified, never shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkClose {
    #[default]
    Open,
    PeerEnded,
    /// The other computer's session ended on its own, not by its user; its Home says why.
    PeerFailed,
    PeerRevoked,
    PeerClosed,
    IdleTimeout,
    Local,
    Transport,
}

/// Transport facts at the end of a session for the drop line; counts and timings only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub session_millis: u64,
    /// Longest pause between two loop iterations: a stalled runtime thread shows here.
    pub max_tick_gap_micros: u64,
    pub queued_frames: u64,
    pub written_frames: u64,
    pub rtt_micros: u64,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub received_datagrams: u64,
    pub congestion_events: u64,
    /// Silences the session survived by holding instead of ending, and the longest one.
    pub holds: u64,
    pub held_max_millis: u64,
    pub close: LinkClose,
    /// Where the session's revocation signal was first revoked, when it was.
    pub revoked_at: Option<&'static std::panic::Location<'static>>,
}
#[derive(Clone, Copy, Default, Debug)]
pub struct SessionDiagnostics {
    pub sent_events: u64,
    pub received_events: u64,
    pub round_trip_micros: u64,
    pub active_display: Option<DisplayId>,
    pub active_is_local: Option<bool>,
    /// True while the session waits for the other computer to answer again.
    pub held: bool,
    /// Receiving-side load at the end of a destination session.
    pub receiver: Option<crate::session_actor::ReceiverStats>,
    /// Transport facts at the end of the session.
    pub link: Option<LinkStats>,
}
impl SessionProgress {
    pub fn is_started(&self) -> bool {
        self.0.started.load(Ordering::Acquire)
    }
    pub(crate) fn mark_started(&self) {
        self.0.started.store(true, Ordering::Release);
    }
    pub fn is_held(&self) -> bool {
        self.0.held.load(Ordering::Acquire)
    }
    /// Marks the session as ended on purpose so its close reason tells the peer it was no drop.
    pub fn end_deliberately(&self) {
        self.0.deliberate.store(true, Ordering::Release);
    }
    pub(crate) fn ended_deliberately(&self) -> bool {
        self.0.deliberate.load(Ordering::Acquire)
    }
    pub(crate) fn hold(&self, held: bool) {
        self.0.held.store(held, Ordering::Release);
    }
    pub(crate) fn record_receiver(&self, stats: crate::session_actor::ReceiverStats) {
        if let Ok(mut slot) = self.0.receiver.lock() {
            *slot = Some(stats);
        }
    }
    pub(crate) fn record_link(&self, stats: LinkStats) {
        if let Ok(mut slot) = self.0.link.lock() {
            *slot = Some(stats);
        }
    }
    pub fn diagnostics(&self) -> SessionDiagnostics {
        let route = self.0.route.load(Ordering::Acquire);
        SessionDiagnostics {
            sent_events: self.0.sent.load(Ordering::Relaxed),
            received_events: self.0.received.load(Ordering::Relaxed),
            round_trip_micros: self.0.rtt_us.load(Ordering::Relaxed),
            active_display: (route != 0).then(|| DisplayId(self.0.display.load(Ordering::Relaxed))),
            active_is_local: match route {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
            held: self.0.held.load(Ordering::Acquire),
            receiver: self.0.receiver.lock().ok().and_then(|slot| *slot),
            link: self.0.link.lock().ok().and_then(|slot| *slot),
        }
    }
    pub(crate) fn route(&self, display: Option<DisplayId>, local: bool) {
        self.0.route.store(0, Ordering::Release);
        if let Some(display) = display {
            self.0.display.store(display.0, Ordering::Relaxed);
            self.0
                .route
                .store(if local { 1 } else { 2 }, Ordering::Release);
        }
    }
    pub(crate) fn received(&self, frame: &Frame) {
        if is_input(frame) {
            self.0.received.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn is_input(frame: &Frame) -> bool {
    matches!(
        frame.message,
        Message::Key(_) | Message::Button(_) | Message::Motion(_) | Message::Scroll(_)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionFailure {
    Revoked,
    Wire,
    QueueFull,
    UnexpectedStream,
    Destination,
    /// The receiving actor stopped; the code names the native step (1-14) or receiver rule
    /// (20-33) that failed, never any input.
    DestinationActor(crate::session_actor::ActorFailure, u8),
    Source,
    /// The source controller refused to continue; the reason names the check, never any input.
    SourceController(crate::session_source::SourceFailure),
    Native,
    NativeStartup,
    Startup(SessionStartupFailure),
    NativeCaptureStartup(NativeCaptureStartupFailure),
    NativeCleanup,
    InvalidLayout,
    TrialShortcut,
}

/// A bounded failure observed during session startup before input delivery begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStartupFailure {
    PeerHealth(StartupPeerHealthFailure),
    Deadline,
    Sequence,
    UnexpectedMessage,
    RateLimited,
    NotReady,
    DisplayUnavailable,
    CaptureTaskUnavailable,
    CaptureWorkerUnavailable,
    RevocationWatchUnavailable,
    NativeOwnershipUnavailable,
    ReceiverUnavailable,
    CaptureEnded,
    CaptureStopped(monhop_core::capture::StopReason),
}

/// A peer-health condition observed while establishing an input session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupPeerHealthFailure {
    ClockRegression,
    DeadlineExpired,
    UnexpectedResponse,
    TokenExhausted,
}

/// A redacted native source-capture construction result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCaptureStartupFailure {
    AlreadyActive,
    TrialWindowNotForeground,
    DesktopUnavailable,
    InvalidDuration,
    StartFailed,
    StartupTimeout,
    CleanupPending,
    ControlPending,
    ControlFailed,
    WorkerPanicked,
    Stopped(monhop_core::capture::StopReason),
    WindowsOperation {
        operation: &'static str,
        code: u32,
    },
    /// This platform has no controlled-trial capture, so it cannot supply trial input.
    TrialSourceUnavailable,
    Platform,
}

/// The writer may await socket capacity while the session continues reading and polling recovery.
pub(crate) struct SessionIo {
    pub(crate) connection: quinn::Connection,
    pub(crate) reader: FrameReader,
    pub(crate) recv: quinn::RecvStream,
    outbound: tokio::sync::mpsc::Sender<Frame>,
    writer: tokio::task::JoinHandle<Result<(), SessionFailure>>,
    progress: SessionProgress,
    queued: AtomicU64,
    written: Arc<AtomicU64>,
}

impl SessionIo {
    pub(crate) fn new(session: NegotiatedSession, progress: SessionProgress) -> Self {
        let connection = session.connection;
        let writer_connection = connection.clone();
        let mut send = session.control_streams.send;
        let (outbound, mut pending) = tokio::sync::mpsc::channel::<Frame>(OUTBOUND_CAPACITY);
        let writer_progress = progress.clone();
        let written = Arc::new(AtomicU64::new(0));
        let writer_written = written.clone();
        let writer = tokio::spawn(async move {
            let mut encoder = FrameWriter::default();
            while let Some(frame) = pending.recv().await {
                if is_heartbeat(&frame.message) {
                    encoder
                        .send_datagram(&writer_connection, &frame)
                        .map_err(|_| SessionFailure::Wire)?;
                } else {
                    encoder
                        .write(&writer_connection, &mut send, &frame)
                        .await
                        .map_err(|_| SessionFailure::Wire)?;
                }
                writer_written.fetch_add(1, Ordering::Relaxed);
                if is_input(&frame) {
                    writer_progress.0.sent.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        });
        Self {
            connection,
            reader: session.control_streams.reader,
            recv: session.control_streams.recv,
            outbound,
            writer,
            progress,
            queued: AtomicU64::new(0),
            written,
        }
    }
    pub(crate) fn send(&self, frame: Frame) -> Result<(), SessionFailure> {
        self.outbound.try_send(frame).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => SessionFailure::QueueFull,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => SessionFailure::Wire,
        })?;
        self.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    /// The next frame from the ordered stream or a heartbeat datagram. A peer that opens a
    /// stream ends the session; a closed connection surfaces as Wire.
    pub(crate) async fn next_frame(&mut self) -> Result<Frame, SessionFailure> {
        tokio::select! {
            biased;
            frame = self.reader.read_frame(&mut self.recv) => frame.map_err(|_| SessionFailure::Wire),
            datagram = self.connection.read_datagram() => match datagram {
                Ok(bytes) => decode_datagram(&bytes).map_err(|_| SessionFailure::UnexpectedStream),
                Err(_) => Err(SessionFailure::Wire),
            },
            event = self.connection.accept_bi() => Err(peer_event(event)),
            event = self.connection.accept_uni() => Err(peer_event(event)),
        }
    }
    pub(crate) fn link_stats(
        &self,
        session_millis: u64,
        max_tick_gap_micros: u64,
        holds: (u64, u64),
        revocation: &RevocationSignal,
    ) -> LinkStats {
        let stats = self.connection.stats();
        LinkStats {
            session_millis,
            max_tick_gap_micros,
            holds: holds.0,
            held_max_millis: holds.1,
            revoked_at: revocation.origin(),
            queued_frames: self.queued.load(Ordering::Relaxed),
            written_frames: self.written.load(Ordering::Relaxed),
            rtt_micros: micros_u64(stats.path.rtt),
            lost_packets: stats.path.lost_packets,
            sent_packets: stats.path.sent_packets,
            received_datagrams: stats.udp_rx.datagrams,
            congestion_events: stats.path.congestion_events,
            close: classify_close(self.connection.close_reason()),
        }
    }
    /// A user's stop closes as ended and the peer records no drop; any other end closes as
    /// failed. A revocation that nobody asked for (native capture stopped) is a failure too.
    pub(crate) fn close_after(&self, result: &Result<(), SessionFailure>, deliberate: bool) {
        let reason = match result {
            Ok(()) => SESSION_ENDED_REASON,
            Err(SessionFailure::Revoked) if deliberate => SESSION_ENDED_REASON,
            Err(_) => SESSION_FAILED_REASON,
        };
        self.connection.close(0_u32.into(), reason);
    }
    pub(crate) fn check(&self) -> Result<(), SessionFailure> {
        self.progress
            .0
            .rtt_us
            .store(micros_u64(self.connection.rtt()), Ordering::Relaxed);
        if self.writer.is_finished() || self.connection.close_reason().is_some() {
            log::warn!(
                "session wire ended: writer finished={}, close reason {:?}",
                self.writer.is_finished(),
                self.connection.close_reason()
            );
            Err(SessionFailure::Wire)
        } else {
            Ok(())
        }
    }
}
pub(crate) const SESSION_ENDED_REASON: &[u8] = b"input session ended";
pub(crate) const SESSION_FAILED_REASON: &[u8] = b"input session failed";
pub(crate) const NETWORK_REVOKED_REASON: &[u8] = b"network revoked";

fn classify_close(reason: Option<quinn::ConnectionError>) -> LinkClose {
    match reason {
        None => LinkClose::Open,
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            if close.reason.as_ref() == SESSION_ENDED_REASON {
                LinkClose::PeerEnded
            } else if close.reason.as_ref() == SESSION_FAILED_REASON {
                LinkClose::PeerFailed
            } else if close.reason.as_ref() == NETWORK_REVOKED_REASON {
                LinkClose::PeerRevoked
            } else {
                LinkClose::PeerClosed
            }
        }
        Some(quinn::ConnectionError::TimedOut) => LinkClose::IdleTimeout,
        Some(quinn::ConnectionError::LocallyClosed) => LinkClose::Local,
        Some(_) => LinkClose::Transport,
    }
}

/// A closed connection surfaces on the stream acceptors too; only a real new stream is unexpected.
fn peer_event<T>(result: Result<T, quinn::ConnectionError>) -> SessionFailure {
    match result {
        Ok(_) => SessionFailure::UnexpectedStream,
        Err(_) => SessionFailure::Wire,
    }
}

/// Longest pause between loop iterations, so a stalled runtime thread is visible on the drop line.
pub(crate) struct TickGap {
    last: Duration,
    max: Duration,
}

impl TickGap {
    pub(crate) fn new(now: Duration) -> Self {
        Self {
            last: now,
            max: Duration::ZERO,
        }
    }
    pub(crate) fn observe(&mut self, now: Duration) {
        self.max = self.max.max(now.saturating_sub(self.last));
        self.last = now;
    }
    pub(crate) fn max_micros(&self) -> u64 {
        micros_u64(self.max)
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        log::debug!(
            "closing session connection (peer close reason before ours: {:?})",
            self.connection.close_reason()
        );
        self.connection.close(0_u32.into(), SESSION_ENDED_REASON);
        self.writer.abort();
    }
}

/// Native ownership is claimed before the actor thread can begin native construction.
#[cfg(any(windows, target_os = "macos"))]
pub async fn run_destination(
    session: NegotiatedSession,
    revocation: RevocationSignal,
    progress: SessionProgress,
) -> Result<(), SessionFailure> {
    let local = session.local.device_id;
    let native_displays = session.local.topology.clone();
    run_destination_with(
        session,
        revocation.clone(),
        progress,
        crate::session_handshake::SessionPurpose::Share,
        move |ownership| {
            crate::session_native::NativeDestination::new_after_local_enable(
                local,
                native_displays,
                ownership,
                revocation,
            )
        },
        None,
    )
    .await
}

/// Runs the receiver half of a separately authenticated controlled trial, on either platform.
#[cfg(any(windows, target_os = "macos"))]
pub async fn run_trial_destination(
    session: NegotiatedSession,
    cancel: RevocationSignal,
    progress: SessionProgress,
    authorization: crate::session_trial::TrialAuthorization,
) -> Result<(), SessionFailure> {
    authorization.require_active()?;
    // Either platform may hold either role; only the peer being the source is required here.
    if session.source != session.peer.device_id {
        return Err(SessionFailure::Destination);
    }
    let _external_revocation = authorization.link_external_revocation(cancel)?;
    let receiver_bounds = authorization
        .receiver_bounds()
        .ok_or(SessionFailure::Destination)?;
    let trial_revocation = authorization.revocation();
    let trial_guard = authorization.input_guard();
    let phase_guard = authorization.input_guard();
    let device = session.local.device_id;
    let displays = session.local.topology.clone();
    let result = run_destination_with(
        session,
        trial_revocation,
        progress,
        crate::session_handshake::SessionPurpose::ControlledTrial,
        move |ownership| {
            crate::session_native::TrialDestination::new_after_local_enable(
                device,
                displays,
                receiver_bounds,
                ownership,
                trial_guard,
            )
        },
        Some(phase_guard),
    )
    .await;
    let result = match (authorization.rejected_system_shortcut(), result) {
        (_, Err(SessionFailure::NativeCleanup)) => Err(SessionFailure::NativeCleanup),
        (true, _) => Err(SessionFailure::TrialShortcut),
        (false, result) => result,
    };
    authorization.revocation().revoke();
    result
}

async fn run_destination_with<D, F>(
    session: NegotiatedSession,
    revocation: RevocationSignal,
    progress: SessionProgress,
    purpose: crate::session_handshake::SessionPurpose,
    factory: F,
    trial: Option<TrialInputGuard>,
) -> Result<(), SessionFailure>
where
    D: crate::session_actor::WatchedDestination + 'static,
    F: FnOnce(NativeInputOwnership) -> Result<D, crate::session_receiver::DestinationFailure>
        + Send
        + 'static,
{
    if session.local_is_source || session.purpose() != purpose {
        return Err(SessionFailure::Destination);
    }
    if revocation.is_stopping() {
        return Err(SessionFailure::Revoked);
    }
    let displays = session.local.topology.clone();
    let epoch = session.initial_epoch;
    let next_sequence = session.control.next_sequence();
    let ownership = NativeInputOwnership::claim().ok_or(SessionFailure::Startup(
        SessionStartupFailure::NativeOwnershipUnavailable,
    ))?;
    let io = SessionIo::new(session, progress.clone());
    let destination = DestinationActor::start_after_local_enable(
        displays,
        revocation.clone(),
        ownership,
        factory,
    )
    .map_err(|_| SessionFailure::Startup(SessionStartupFailure::ReceiverUnavailable))?;
    run_destination_actor(
        io,
        destination,
        revocation,
        progress,
        epoch,
        next_sequence,
        trial,
    )
    .await
}

fn destination_failure(reason: crate::session_actor::ActorFailure) -> SessionFailure {
    let detail = match reason {
        crate::session_actor::ActorFailure::Receiver => {
            crate::session_actor::last_receiver_failure()
        }
        _ => crate::session_native::last_destination_step(),
    };
    SessionFailure::DestinationActor(reason, detail)
}

#[allow(clippy::too_many_arguments)]
async fn run_destination_actor(
    mut io: SessionIo,
    mut destination: DestinationActor,
    revocation: RevocationSignal,
    progress: SessionProgress,
    epoch: monhop_protocol::SessionEpoch,
    next_sequence: u64,
    trial: Option<TrialInputGuard>,
) -> Result<(), SessionFailure> {
    let origin = SessionClock::try_now()?;
    let mut startup = Some(StartupControl::new(
        epoch,
        next_sequence,
        false,
        Duration::ZERO,
    ));
    let mut handoff_sent = false;
    let mut progress_started = false;
    let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut gaps = TickGap::new(origin.elapsed());
    let mut started_at = None;
    let result = loop {
        tokio::select! {
            biased;
            _=tick.tick()=>{
                gaps.observe(origin.elapsed());
                if revocation.is_stopping(){break Err(SessionFailure::Revoked);}
                if let Err(error)=io.check(){break Err(error);}
                if let Some(reason)=destination.failure(){break Err(destination_failure(reason));}
                let now=origin.elapsed();
                if !handoff_sent {
                    let control=startup.as_mut().expect("startup remains until actor handoff");
                    match control.poll(now) {
                        Ok(Some(frame))=>if let Err(error)=io.send(frame){break Err(error);},
                        Ok(None)=>{}
                        Err(error)=>break Err(startup_failure(error)),
                    }
                    if destination.is_native_ready() && trial_permits_ready(trial.as_ref()) {
                        match control.can_announce_ready(now) {
                            Ok(true)=>match control.announce_ready(now) {
                                Ok(frame)=>if let Err(error)=io.send(frame){break Err(error);},
                                Err(error)=>break Err(startup_failure(error)),
                            },
                            Ok(false)=>{}
                            Err(error)=>break Err(startup_failure(error)),
                        }
                    }
                    match handoff_if_ready(&mut startup, &destination, &origin, trial.as_ref()) {
                        Ok(ready) => handoff_sent = ready,
                        Err(error) => break Err(error),
                    }
                    if handoff_sent { started_at.get_or_insert(origin.elapsed()); }
                } else if !progress_started && destination.is_started() {
                    progress.mark_started();
                    progress_started=true;
                }
                if progress_started {progress.route(destination.active_display(),true);progress.hold(destination.is_held());}
                let mut failure=None;
                for _ in 0..OUTBOUND_CAPACITY {
                    match destination.try_response() {
                        Ok(Some(frame))=>if let Err(error)=io.send(frame){failure=Some(error);break;},
                        Ok(None)=>break,
                        Err(reason)=>{failure=Some(destination_failure(reason));break;}
                    }
                }
                if let Some(error)=failure {break Err(error);}
            }
            frame=io.next_frame()=>{
                if revocation.is_stopping(){break Err(SessionFailure::Revoked);}
                let frame=match frame {Ok(frame)=>frame,Err(error)=>break Err(error)};
                if handoff_sent {
                    progress.received(&frame);
                    if let Err(reason)=destination.try_submit(frame){break Err(destination_failure(reason));}
                } else {
                    let now=origin.elapsed();
                    let control=startup.as_mut().expect("startup remains until actor handoff");
                    match control.receive(&frame,now) {
                        Ok(Some(response))=>if let Err(error)=io.send(response){break Err(error);},
                        Ok(None)=>{}
                        Err(error)=>break Err(startup_failure(error)),
                    }
                    // Source input can follow Ready in the same stream read cycle.
                    match handoff_if_ready(&mut startup, &destination, &origin, trial.as_ref()) {
                        Ok(ready) => handoff_sent = ready,
                        Err(error) => break Err(error),
                    }
                    if handoff_sent { started_at.get_or_insert(origin.elapsed()); }
                }
            }
        }
    };
    destination.request_stop();
    let receiver = destination.stats();
    progress.record_receiver(receiver);
    progress.record_link(io.link_stats(
        session_millis(started_at, origin.elapsed()),
        gaps.max_micros(),
        (receiver.holds, receiver.held_max_millis),
        &revocation,
    ));
    io.close_after(&result, progress.ended_deliberately());
    drop(io);
    // Native cleanup has its own thread and deadline. Give it time without depending on the peer.
    let end = tokio::time::Instant::now() + Duration::from_millis(250);
    while !destination.finish() && tokio::time::Instant::now() < end {
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }
    if !destination.is_finished() || destination.cleanup_pending() {
        log::error!("destination session ended with native cleanup pending");
        return Err(SessionFailure::NativeCleanup);
    }
    match &result {
        Ok(()) => log::info!("destination session ended by the peer"),
        Err(error) => log::warn!("destination session ended: {error:?}"),
    }
    result
}

pub(crate) fn session_millis(started_at: Option<Duration>, now: Duration) -> u64 {
    started_at.map_or(0, |started| millis_u64(now.saturating_sub(started)))
}

/// A trial in its startup phase holds readiness back rather than failing on a lost window.
fn trial_permits_ready(trial: Option<&TrialInputGuard>) -> bool {
    trial.is_none_or(TrialInputGuard::allows_new_input)
}

fn handoff_if_ready(
    startup: &mut Option<StartupControl>,
    destination: &DestinationActor,
    origin: &SessionClock,
    trial: Option<&TrialInputGuard>,
) -> Result<bool, SessionFailure> {
    if !destination.is_native_ready() {
        return Ok(false);
    }
    let control = startup.as_mut().ok_or(SessionFailure::Destination)?;
    if !control
        .is_ready(origin.elapsed())
        .map_err(startup_failure)?
    {
        return Ok(false);
    }
    // Native readiness plus both SessionReady messages: the trial's active phase starts here.
    if let Some(guard) = trial {
        guard.begin_active();
    }
    let ready = startup
        .take()
        .ok_or(SessionFailure::Destination)?
        .into_ready(origin.elapsed())
        .map_err(startup_failure)?;
    destination
        .handoff_ready(ready, origin.clone())
        .map_err(|_| SessionFailure::Destination)?;
    Ok(true)
}

#[cfg(test)]
#[path = "session_startup_tests.rs"]
mod startup_tests;

#[cfg(test)]
mod close_tests {
    use super::*;

    fn closed(reason: &[u8]) -> Option<quinn::ConnectionError> {
        Some(quinn::ConnectionError::ApplicationClosed(
            quinn::ApplicationClose {
                error_code: 0_u32.into(),
                reason: reason.to_vec().into(),
            },
        ))
    }

    #[test]
    fn a_failed_close_is_told_apart_from_an_ended_one() {
        assert_eq!(classify_close(None), LinkClose::Open);
        assert_eq!(
            classify_close(closed(SESSION_ENDED_REASON)),
            LinkClose::PeerEnded
        );
        assert_eq!(
            classify_close(closed(SESSION_FAILED_REASON)),
            LinkClose::PeerFailed
        );
        assert_eq!(
            classify_close(closed(NETWORK_REVOKED_REASON)),
            LinkClose::PeerRevoked
        );
        assert_eq!(
            classify_close(closed(b"anything else")),
            LinkClose::PeerClosed
        );
    }

    #[test]
    fn a_session_ends_deliberately_only_once_its_owner_says_so() {
        let progress = SessionProgress::default();
        assert!(!progress.ended_deliberately());
        progress.end_deliberately();
        assert!(progress.ended_deliberately());
        assert!(progress.clone().ended_deliberately());
    }
}
