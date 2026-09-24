//! Runtime for an explicitly enabled, authenticated input session.

use crate::session_actor::ActorFailure;
#[cfg(any(windows, target_os = "macos"))]
pub use crate::session_runtime::run_session;
use crate::{
    session_clock::{micros_u64, millis_u64},
    session_handshake::NegotiatedSession,
    session_wire::{FrameReader, FrameWriter, decode_datagram, is_heartbeat},
};
use monhop_core::{DeviceId, DisplayId, RevocationSignal, capture::StopReason};
use monhop_protocol::{Frame, FrameScope, Message};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

pub(crate) const SESSION_QUEUE_CAPACITY: usize = 512;
pub(crate) const DISPLAY_CHECK_INTERVAL: Duration = Duration::from_millis(30);
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
    half: AtomicU64,
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
    /// The other computer's own displays changed; its layout resyncs, nothing failed.
    PeerDisplaysChanged,
    /// The other computer changed who can control which computer; its link resyncs, nothing
    /// failed.
    PeerControlChanged,
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
    pub active_half: Option<SessionHalf>,
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
            active_half: match self.0.half.load(Ordering::Acquire) {
                1 => Some(SessionHalf::Outbound),
                2 => Some(SessionHalf::Inbound),
                _ => None,
            },
            held: self.0.held.load(Ordering::Acquire),
            receiver: self.0.receiver.lock().ok().and_then(|slot| *slot),
            link: self.0.link.lock().ok().and_then(|slot| *slot),
        }
    }
    pub(crate) fn active_half(&self, half: Option<SessionHalf>) {
        self.0.half.store(
            match half {
                Some(SessionHalf::Outbound) => 1,
                Some(SessionHalf::Inbound) => 2,
                None => 0,
            },
            Ordering::Release,
        );
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
        Message::Key(_)
            | Message::Button(_)
            | Message::Motion(_)
            | Message::Scroll(_)
            | Message::Gesture(_)
            | Message::SystemGesture(_)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionFailure {
    Revoked,
    Wire,
    QueueFull,
    UnexpectedStream,
    Destination,
    /// The receiving actor stopped; the code names the native step (1-17) or receiver rule
    /// (20-34) that failed, never any input.
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
    LocalDisplaysChanged,
}

/// A fresh native display snapshot never continues a session on stale geometry.
pub fn check_local_displays(
    expected: &monhop_protocol::DisplayTopology,
    current: &monhop_protocol::DisplayTopology,
) -> Result<(), SessionFailure> {
    if expected.same_geometry(current) {
        Ok(())
    } else {
        Err(SessionFailure::LocalDisplaysChanged)
    }
}

/// A fresh read that failed mid-change counts as a change: the session ends at once either way.
pub(crate) fn check_read_displays<E>(
    expected: &monhop_protocol::DisplayTopology,
    current: Result<monhop_protocol::DisplayTopology, E>,
) -> Result<(), SessionFailure> {
    check_local_displays(
        expected,
        &current.map_err(|_| SessionFailure::LocalDisplaysChanged)?,
    )
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
    DesktopUnavailable,
    InvalidDuration,
    StartFailed,
    StartupTimeout,
    CleanupPending,
    ControlPending,
    ControlFailed,
    WorkerPanicked,
    Stopped(monhop_core::capture::StopReason),
    WindowsOperation { operation: &'static str, code: u32 },
    Platform,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionHalf {
    Outbound,
    Inbound,
}

/// Scopes bind directions to device identity, never to who dialed the connection.
#[derive(Clone, Copy, Debug)]
pub struct SessionScopes {
    pub outbound: FrameScope,
    pub inbound: FrameScope,
}
impl SessionScopes {
    pub fn new(local: DeviceId, peer: DeviceId) -> Result<Self, SessionFailure> {
        if local == peer {
            return Err(SessionFailure::Destination);
        }
        let lower = local < peer;
        Ok(Self {
            outbound: if lower {
                FrameScope::LowerControlsHigher
            } else {
                FrameScope::HigherControlsLower
            },
            inbound: if lower {
                FrameScope::HigherControlsLower
            } else {
                FrameScope::LowerControlsHigher
            },
        })
    }
    pub fn validate(&self, frame: &Frame) -> Result<(), SessionFailure> {
        if frame.scope == FrameScope::Connection {
            if matches!(frame.message, Message::Disconnect(_) | Message::Error(_)) {
                return Ok(());
            }
        } else if (frame.scope == self.outbound || frame.scope == self.inbound)
            && !matches!(
                frame.message,
                Message::Hello(_) | Message::SessionSetup(_) | Message::DisplayTopology(_)
            )
        {
            return Ok(());
        }
        Err(SessionFailure::UnexpectedStream)
    }
}

/// The writer may await socket capacity while the session continues reading and polling recovery.
pub(crate) struct SessionIo {
    pub(crate) scopes: SessionScopes,
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
        let scopes = SessionScopes {
            outbound: session.outbound_scope(),
            inbound: session.inbound_scope(),
        };
        let connection = session.connection;
        let writer_connection = connection.clone();
        let mut send = session.control_streams.send;
        let (outbound, mut pending) = tokio::sync::mpsc::channel::<Frame>(SESSION_QUEUE_CAPACITY);
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
            scopes,
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
    fn send(&self, frame: Frame) -> Result<(), SessionFailure> {
        self.outbound.try_send(frame).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => SessionFailure::QueueFull,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => SessionFailure::Wire,
        })?;
        self.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub(crate) fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure> {
        self.send(frame.with_scope(self.scopes.outbound))
    }
    pub(crate) fn send_inbound(&self, frame: Frame) -> Result<(), SessionFailure> {
        self.send(frame.with_scope(self.scopes.inbound))
    }
    /// The next frame from the ordered stream or a heartbeat datagram. A peer that opens a
    /// stream ends the session; a closed connection surfaces as Wire.
    pub(crate) async fn next_frame(&mut self) -> Result<Frame, SessionFailure> {
        let frame = tokio::select! {
            biased;
            frame = self.reader.read_frame(&mut self.recv) => frame.map_err(|_| SessionFailure::Wire),
            datagram = self.connection.read_datagram() => match datagram {
                Ok(bytes) => decode_datagram(&bytes).map_err(|_| SessionFailure::UnexpectedStream),
                Err(_) => Err(SessionFailure::Wire),
            },
            event = self.connection.accept_bi() => Err(peer_event(event)),
            event = self.connection.accept_uni() => Err(peer_event(event)),
        }?;
        self.scopes.validate(&frame)?;
        Ok(frame)
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
    pub(crate) fn close_after(
        &self,
        result: &Result<(), SessionFailure>,
        deliberate: bool,
        control_change: bool,
    ) {
        self.connection.close(
            0_u32.into(),
            close_reason(result, deliberate, control_change),
        );
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
pub(crate) const DISPLAYS_CHANGED_REASON: &[u8] = b"displays changed";
pub(crate) const CONTROL_CHANGED_REASON: &[u8] = b"control changed";

/// A user's stop closes as ended and the peer records no drop; a display change or a control-flip
/// stop closes as such so the peer resyncs instead of reporting a failure. Any other end, native
/// stops included, failed.
pub(crate) fn close_reason(
    result: &Result<(), SessionFailure>,
    deliberate: bool,
    control_change: bool,
) -> &'static [u8] {
    match result {
        Ok(()) => SESSION_ENDED_REASON,
        Err(SessionFailure::Revoked) if deliberate && control_change => CONTROL_CHANGED_REASON,
        Err(SessionFailure::Revoked) if deliberate => SESSION_ENDED_REASON,
        Err(SessionFailure::LocalDisplaysChanged) => DISPLAYS_CHANGED_REASON,
        Err(_) => SESSION_FAILED_REASON,
    }
}

fn classify_close(reason: Option<quinn::ConnectionError>) -> LinkClose {
    match reason {
        None => LinkClose::Open,
        Some(quinn::ConnectionError::ApplicationClosed(close)) => match close.reason.as_ref() {
            SESSION_ENDED_REASON => LinkClose::PeerEnded,
            SESSION_FAILED_REASON => LinkClose::PeerFailed,
            NETWORK_REVOKED_REASON => LinkClose::PeerRevoked,
            DISPLAYS_CHANGED_REASON => LinkClose::PeerDisplaysChanged,
            CONTROL_CHANGED_REASON => LinkClose::PeerControlChanged,
            _ => LinkClose::PeerClosed,
        },
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

pub(crate) fn destination_failure(reason: ActorFailure) -> SessionFailure {
    if reason == ActorFailure::LocalDisplaysChanged {
        return SessionFailure::LocalDisplaysChanged;
    }
    let detail = match reason {
        ActorFailure::Receiver => crate::session_actor::last_receiver_failure(),
        _ => crate::session_native::last_destination_step(),
    };
    SessionFailure::DestinationActor(reason, detail)
}

fn displays_changed(capture: Option<StopReason>, destination: Option<ActorFailure>) -> bool {
    capture == Some(StopReason::DisplaysChanged)
        || destination == Some(ActorFailure::LocalDisplaysChanged)
}

/// Why the coordinator must stop now. A display change outranks the revocation the platform marks
/// alongside it; only a user's own stop outranks the display change.
pub(crate) fn worker_end(
    stopping: bool,
    deliberate: bool,
    capture: Option<StopReason>,
    capture_finished: bool,
    destination: Option<ActorFailure>,
) -> Option<SessionFailure> {
    let changed = displays_changed(capture, destination);
    if stopping && (deliberate || !changed) {
        return Some(SessionFailure::Revoked);
    }
    if changed {
        return Some(SessionFailure::LocalDisplaysChanged);
    }
    if let Some(reason) = destination {
        return Some(destination_failure(reason));
    }
    (capture_finished || capture.is_some()).then_some(SessionFailure::Native)
}

/// Reclassifies an end whose display change was recorded after the loop saw its revocation or
/// native stop.
pub(crate) fn settle_end(
    result: Result<(), SessionFailure>,
    deliberate: bool,
    capture: Option<StopReason>,
    destination: Option<ActorFailure>,
) -> Result<(), SessionFailure> {
    match result {
        Err(_) if !deliberate && displays_changed(capture, destination) => {
            Err(SessionFailure::LocalDisplaysChanged)
        }
        other => other,
    }
}

pub(crate) fn session_millis(started_at: Option<Duration>, now: Duration) -> u64 {
    started_at.map_or(0, |started| millis_u64(now.saturating_sub(started)))
}

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
            classify_close(closed(DISPLAYS_CHANGED_REASON)),
            LinkClose::PeerDisplaysChanged
        );
        assert_eq!(
            classify_close(closed(CONTROL_CHANGED_REASON)),
            LinkClose::PeerControlChanged
        );
        assert_eq!(
            classify_close(closed(b"anything else")),
            LinkClose::PeerClosed
        );
    }

    #[test]
    fn a_display_change_end_reaches_the_peer_as_a_display_change() {
        let changed = Err(SessionFailure::LocalDisplaysChanged);
        for deliberate in [false, true] {
            assert_eq!(
                classify_close(closed(close_reason(&changed, deliberate, false))),
                LinkClose::PeerDisplaysChanged
            );
        }
        assert_eq!(
            classify_close(closed(close_reason(
                &Err(SessionFailure::Native),
                false,
                false
            ))),
            LinkClose::PeerFailed
        );
        assert_eq!(
            classify_close(closed(close_reason(
                &Err(SessionFailure::Revoked),
                true,
                false
            ))),
            LinkClose::PeerEnded
        );
    }

    #[test]
    fn a_control_change_stop_closes_with_its_own_reason() {
        let revoked = Err(SessionFailure::Revoked);
        assert_eq!(close_reason(&revoked, true, true), CONTROL_CHANGED_REASON);
        assert_eq!(close_reason(&revoked, true, false), SESSION_ENDED_REASON);
        // A non-deliberate end never reads as a control change, whatever the flag says.
        assert_eq!(close_reason(&revoked, false, true), SESSION_FAILED_REASON);
        assert_eq!(
            classify_close(closed(close_reason(&revoked, true, true))),
            LinkClose::PeerControlChanged
        );
    }

    #[test]
    fn an_unreadable_display_list_is_a_display_change() {
        let displays =
            monhop_protocol::DisplayTopology::new(vec![monhop_protocol::DisplayDescription {
                id: DisplayId(1),
                name: "fixture".into(),
                native_width: 100,
                native_height: 100,
                logical_origin: monhop_core::Point::default(),
                logical_size: monhop_core::Point::new(100.0, 100.0),
                scale_factor: 1.0,
                is_primary: true,
                monitor: None,
            }])
            .unwrap();
        assert_eq!(
            check_read_displays(&displays, Err::<monhop_protocol::DisplayTopology, ()>(())),
            Err(SessionFailure::LocalDisplaysChanged)
        );
        assert_eq!(
            check_read_displays(&displays, Ok::<_, ()>(displays.clone())),
            Ok(())
        );
    }

    /// The platform records the capture stop and marks the revocation; the loop may see either
    /// first, and both orders must end as a display change.
    #[test]
    fn a_capture_display_change_outranks_its_revocation_in_both_orders() {
        use monhop_core::capture::CaptureStop;
        let ends = |cancel: &RevocationSignal, stop: &CaptureStop| {
            worker_end(cancel.is_stopping(), false, stop.reason(), false, None)
        };

        let (cancel, stop) = (RevocationSignal::default(), CaptureStop::new());
        stop.stop(StopReason::DisplaysChanged);
        cancel.mark_revoked_without_wake();
        assert_eq!(
            ends(&cancel, &stop),
            Some(SessionFailure::LocalDisplaysChanged)
        );

        let (cancel, stop) = (RevocationSignal::default(), CaptureStop::new());
        cancel.mark_revoked_without_wake();
        let seen = ends(&cancel, &stop).expect("revocation ends the loop");
        assert_eq!(seen, SessionFailure::Revoked);
        stop.stop(StopReason::DisplaysChanged);
        assert_eq!(
            settle_end(Err(seen), false, stop.reason(), None),
            Err(SessionFailure::LocalDisplaysChanged)
        );
    }

    #[test]
    fn a_destination_display_change_outranks_revocation_and_a_user_stop_outranks_both() {
        let watchdog = Some(ActorFailure::LocalDisplaysChanged);
        assert_eq!(
            worker_end(true, false, None, false, watchdog),
            Some(SessionFailure::LocalDisplaysChanged)
        );
        assert_eq!(
            settle_end(Err(SessionFailure::Revoked), false, None, watchdog),
            Err(SessionFailure::LocalDisplaysChanged)
        );
        let captured = Some(StopReason::DisplaysChanged);
        assert_eq!(
            worker_end(true, true, captured, true, None),
            Some(SessionFailure::Revoked)
        );
        assert_eq!(
            settle_end(Err(SessionFailure::Revoked), true, captured, None),
            Err(SessionFailure::Revoked)
        );
        assert_eq!(
            worker_end(false, false, Some(StopReason::NativeFailure), true, None),
            Some(SessionFailure::Native)
        );
        assert_eq!(worker_end(false, false, None, false, None), None);
        assert_eq!(settle_end(Ok(()), false, captured, None), Ok(()));
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
