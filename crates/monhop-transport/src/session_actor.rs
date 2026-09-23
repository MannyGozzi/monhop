//! A destination watchdog whose progress never depends on the network writer.

use crate::{
    session_clock::{SessionClock, micros_u64, millis_u64},
    session_receiver::{
        DestinationAction, DestinationFailure, InputDestination, InputReceiver, ReceiverFailure,
    },
    session_startup::ReadyControl,
};
use monhop_core::{InjectionPermit, RevocationSignal, TakeBackGate};
use monhop_protocol::{DisplayTopology, Frame, Message, SessionEpoch};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const CAPACITY: usize = crate::session::SESSION_QUEUE_CAPACITY;
const TICK: Duration = Duration::from_millis(5);

/// The rule that last stopped a receiver, as a code the desktop can name; never input content.
static LAST_RECEIVER_FAILURE: AtomicU8 = AtomicU8::new(0);

pub fn last_receiver_failure() -> u8 {
    LAST_RECEIVER_FAILURE.load(Ordering::Acquire)
}

pub const fn receiver_failure_code(failure: ReceiverFailure) -> u8 {
    use crate::session_health::HealthError;
    match failure {
        ReceiverFailure::Health(HealthError::ClockRegression) => 20,
        ReceiverFailure::Health(HealthError::DeadlineExpired) => 21,
        ReceiverFailure::Health(HealthError::UnexpectedResponse) => 22,
        ReceiverFailure::Health(HealthError::TokenExhausted) => 23,
        ReceiverFailure::InvalidEpoch => 24,
        ReceiverFailure::InvalidSequence => 25,
        ReceiverFailure::RateLimited => 26,
        ReceiverFailure::WrongDisplay => 27,
        ReceiverFailure::InvalidPoint => 28,
        ReceiverFailure::NotActive => 29,
        ReceiverFailure::InvalidPressedState => 30,
        ReceiverFailure::UnexpectedMessage => 31,
        ReceiverFailure::NativeDelivery => 32,
        ReceiverFailure::PeerStopped => 33,
        ReceiverFailure::InvalidGesture => 34,
    }
}

fn note_receiver_failure(failure: ReceiverFailure) -> ActorFailure {
    LAST_RECEIVER_FAILURE.store(receiver_failure_code(failure), Ordering::Release);
    ActorFailure::Receiver
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ActorFailure {
    Requested = 1,
    Revoked,
    QueueFull,
    PeerGone,
    Receiver,
    Native,
    SequenceExhausted,
    Panicked,
    Startup,
    LocalDisplaysChanged,
}

struct Status {
    stopped: AtomicU8,
    cleanup_pending: AtomicBool,
    native_ready: AtomicBool,
    started: AtomicBool,
    handoff_sent: AtomicBool,
    active: AtomicBool,
    held: AtomicBool,
    display: AtomicU64,
    stats: Stats,
    response_waker: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

/// Receiver-side load counters for the session end report; never input content.
struct Stats {
    frames: AtomicU64,
    batch_max: AtomicU64,
    /// Batches of 1, 2-3, 4-8 and 9+ frames; a smooth link keeps almost everything in the first.
    batch_buckets: [AtomicU64; 4],
    /// Batches whose injection took longer than 2 ms.
    slow_applies: AtomicU64,
    apply_max_micros: AtomicU64,
    env_max_micros: AtomicU64,
    reply_age_millis: AtomicU64,
    ping_age_millis: AtomicU64,
    frame_age_millis: AtomicU64,
    gap_max_millis: AtomicU64,
    holds: AtomicU64,
    held_max_millis: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frames: AtomicU64::new(0),
            batch_max: AtomicU64::new(0),
            batch_buckets: [const { AtomicU64::new(0) }; 4],
            slow_applies: AtomicU64::new(0),
            apply_max_micros: AtomicU64::new(0),
            env_max_micros: AtomicU64::new(0),
            reply_age_millis: AtomicU64::new(0),
            ping_age_millis: AtomicU64::new(u64::MAX),
            frame_age_millis: AtomicU64::new(u64::MAX),
            gap_max_millis: AtomicU64::new(0),
            holds: AtomicU64::new(0),
            held_max_millis: AtomicU64::new(0),
        }
    }
}

impl Stats {
    fn max(slot: &AtomicU64, value: u64) {
        slot.fetch_max(value, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    pub frames: u64,
    pub batch_max: u64,
    /// Batches of 1, 2-3, 4-8 and 9+ frames.
    pub batch_buckets: [u64; 4],
    /// Batches whose injection took longer than 2 ms.
    pub slow_applies: u64,
    pub apply_max_micros: u64,
    pub env_max_micros: u64,
    pub reply_age_millis: u64,
    pub ping_age_millis: Option<u64>,
    /// How long before the receiver stopped the last frame of any kind arrived.
    pub frame_age_millis: Option<u64>,
    /// The longest silence between two frames while the session ran.
    pub gap_max_millis: u64,
    /// How often the source went silent past the deadline and the session held instead of ending.
    pub holds: u64,
    pub held_max_millis: u64,
}

impl Status {
    fn stop(&self, reason: ActorFailure) {
        let _ = self
            .stopped
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    fn failure(&self) -> Option<ActorFailure> {
        match self.stopped.load(Ordering::Acquire) {
            0 => None,
            1 => Some(ActorFailure::Requested),
            2 => Some(ActorFailure::Revoked),
            3 => Some(ActorFailure::QueueFull),
            4 => Some(ActorFailure::PeerGone),
            5 => Some(ActorFailure::Receiver),
            6 => Some(ActorFailure::Native),
            7 => Some(ActorFailure::SequenceExhausted),
            8 => Some(ActorFailure::Panicked),
            10 => Some(ActorFailure::LocalDisplaysChanged),
            _ => Some(ActorFailure::Startup),
        }
    }
}

pub trait WatchedDestination: InputDestination {
    fn validate_environment(&mut self) -> Result<(), DestinationFailure>;
    fn local_displays_changed(&self) -> bool {
        false
    }
}

struct StartupHandoff {
    control: ReadyControl,
    origin: SessionClock,
}

pub struct DestinationActor {
    inbound: SyncSender<Frame>,
    outbound: Receiver<Frame>,
    startup: SyncSender<StartupHandoff>,
    status: Arc<Status>,
    worker: Option<JoinHandle<()>>,
}

impl DestinationActor {
    /// Factory construction and all destination operations stay on the actor thread.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_after_local_enable<D, F>(
        displays: DisplayTopology,
        revocation: RevocationSignal,
        ownership: InjectionPermit,
        gate: TakeBackGate,
        local_lower: bool,
        enabled: bool,
        factory: F,
    ) -> Result<Self, ActorFailure>
    where
        D: WatchedDestination + 'static,
        F: FnOnce(InjectionPermit) -> Result<D, DestinationFailure> + Send + 'static,
    {
        if revocation.is_stopping() {
            return Err(ActorFailure::Revoked);
        }
        let (inbound, incoming) = mpsc::sync_channel(CAPACITY);
        let (outgoing, outbound) = mpsc::sync_channel(CAPACITY);
        let (startup, handoffs) = mpsc::sync_channel(1);
        let status = Arc::new(Status {
            stopped: AtomicU8::new(0),
            cleanup_pending: AtomicBool::new(false),
            native_ready: AtomicBool::new(false),
            started: AtomicBool::new(false),
            handoff_sent: AtomicBool::new(false),
            active: AtomicBool::new(false),
            held: AtomicBool::new(false),
            display: AtomicU64::new(0),
            stats: Stats::default(),
            response_waker: OnceLock::new(),
        });
        let worker_status = status.clone();
        let worker = thread::Builder::new()
            .name("monhop-destination".into())
            .spawn(move || {
                crate::session_threads::mark_time_sensitive();
                let status = worker_status;
                let destination =
                    match construct_destination(&revocation, &status, ownership, factory) {
                        Ok(destination) => destination,
                        Err(failure) => {
                            status.stop(failure);
                            return;
                        }
                    };
                let mut destination = DestinationGuard {
                    destination,
                    status: status.clone(),
                    armed: true,
                };
                if revocation.is_stopping() || status.failure().is_some() {
                    status.stop(ActorFailure::Revoked);
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                if destination.validate_environment().is_err() {
                    status.stop(if destination.local_displays_changed() {
                        ActorFailure::LocalDisplaysChanged
                    } else {
                        ActorFailure::Native
                    });
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                status.native_ready.store(true, Ordering::Release);
                let handoff: StartupHandoff = loop {
                    if revocation.is_stopping() {
                        status.stop(ActorFailure::Revoked);
                    }
                    if status.failure().is_some() {
                        cleanup_without_receiver(&mut destination, &status);
                        return;
                    }
                    match handoffs.recv_timeout(TICK) {
                        Ok(handoff) => break handoff,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            status.stop(ActorFailure::PeerGone);
                        }
                    }
                };
                if revocation.is_stopping() || status.failure().is_some() {
                    status.stop(ActorFailure::Revoked);
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                if destination.validate_environment().is_err() {
                    status.stop(if destination.local_displays_changed() {
                        ActorFailure::LocalDisplaysChanged
                    } else {
                        ActorFailure::Native
                    });
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                let now = handoff.origin.elapsed();
                let sequences = OutputSequences {
                    control: handoff.control.next_heartbeat_out,
                    input: 0,
                    epoch: handoff.control.epoch,
                };
                let mut receiver =
                    match InputReceiver::after_startup(displays, handoff.control, now) {
                        Ok(receiver) => receiver.with_floor(gate, local_lower, enabled),
                        Err(failure) => {
                            status.stop(note_receiver_failure(failure));
                            cleanup_without_receiver(&mut destination, &status);
                            return;
                        }
                    };
                if revocation.is_stopping() || status.failure().is_some() {
                    status.stop(ActorFailure::Revoked);
                    receiver.stop(ReceiverFailure::PeerStopped, &mut destination);
                    cleanup_receiver(&mut receiver, &mut destination, &status);
                    return;
                }
                status.started.store(true, Ordering::Release);
                run_receiver(
                    &mut receiver,
                    &mut destination,
                    sequences,
                    incoming,
                    outgoing,
                    revocation,
                    status,
                    handoff.origin,
                );
            })
            .map_err(|_| ActorFailure::Startup)?;
        Ok(Self {
            inbound,
            outbound,
            startup,
            status,
            worker: Some(worker),
        })
    }

    pub(crate) fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let thread = self.worker.as_ref().expect("actor worker").thread().clone();
        Arc::new(move || thread.unpark())
    }
    pub(crate) fn set_response_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.status.response_waker.set(waker);
    }

    pub(crate) fn is_native_ready(&self) -> bool {
        self.status.native_ready.load(Ordering::Acquire) && self.failure().is_none()
    }

    pub(crate) fn handoff_ready(
        &self,
        control: ReadyControl,
        origin: SessionClock,
    ) -> Result<(), ActorFailure> {
        if let Some(failure) = self.failure() {
            return Err(failure);
        }
        if !self.status.native_ready.load(Ordering::Acquire)
            || self.status.started.load(Ordering::Acquire)
            || self
                .status
                .handoff_sent
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            self.status.stop(ActorFailure::Startup);
            return Err(ActorFailure::Startup);
        }
        self.startup
            .try_send(StartupHandoff { control, origin })
            .map_err(|error| {
                let failure = match error {
                    mpsc::TrySendError::Full(_) => ActorFailure::Startup,
                    mpsc::TrySendError::Disconnected(_) => ActorFailure::PeerGone,
                };
                self.status.stop(failure);
                failure
            })
    }

    pub(crate) fn is_started(&self) -> bool {
        self.status.started.load(Ordering::Acquire) && self.failure().is_none()
    }

    pub fn try_submit(&self, frame: Frame) -> Result<(), ActorFailure> {
        if let Some(reason) = self.failure() {
            return Err(reason);
        }
        if !self.status.handoff_sent.load(Ordering::Acquire) {
            self.status.stop(ActorFailure::Startup);
            return Err(ActorFailure::Startup);
        }
        self.inbound.try_send(frame).map_err(|error| {
            let failure = match error {
                mpsc::TrySendError::Full(_) => ActorFailure::QueueFull,
                mpsc::TrySendError::Disconnected(_) => ActorFailure::PeerGone,
            };
            self.status.stop(failure);
            failure
        })?;
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        Ok(())
    }

    pub fn try_response(&mut self) -> Result<Option<Frame>, ActorFailure> {
        if let Some(reason) = self.failure() {
            return Err(reason);
        }
        match self.outbound.try_recv() {
            Ok(frame) => self.failure().map_or(Ok(Some(frame)), Err),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.status.stop(ActorFailure::PeerGone);
                Err(ActorFailure::PeerGone)
            }
        }
    }

    pub fn request_stop(&self) {
        self.status.stop(ActorFailure::Requested);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }

    pub fn stats(&self) -> ReceiverStats {
        let stats = &self.status.stats;
        let ping_age = stats.ping_age_millis.load(Ordering::Relaxed);
        let frame_age = stats.frame_age_millis.load(Ordering::Relaxed);
        ReceiverStats {
            frames: stats.frames.load(Ordering::Relaxed),
            batch_max: stats.batch_max.load(Ordering::Relaxed),
            batch_buckets: std::array::from_fn(|index| {
                stats.batch_buckets[index].load(Ordering::Relaxed)
            }),
            slow_applies: stats.slow_applies.load(Ordering::Relaxed),
            apply_max_micros: stats.apply_max_micros.load(Ordering::Relaxed),
            env_max_micros: stats.env_max_micros.load(Ordering::Relaxed),
            reply_age_millis: stats.reply_age_millis.load(Ordering::Relaxed),
            ping_age_millis: (ping_age != u64::MAX).then_some(ping_age),
            frame_age_millis: (frame_age != u64::MAX).then_some(frame_age),
            gap_max_millis: stats.gap_max_millis.load(Ordering::Relaxed),
            holds: stats.holds.load(Ordering::Relaxed),
            held_max_millis: stats.held_max_millis.load(Ordering::Relaxed),
        }
    }

    /// True while the source has been silent past the deadline and the receiver waits for its
    /// barrier with everything released.
    pub fn is_held(&self) -> bool {
        self.status.held.load(Ordering::Acquire)
    }

    pub fn active_display(&self) -> Option<monhop_core::DisplayId> {
        (self.is_started() && self.status.active.load(Ordering::Acquire))
            .then(|| monhop_core::DisplayId(self.status.display.load(Ordering::Relaxed)))
    }

    pub fn failure(&self) -> Option<ActorFailure> {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && self.status.failure().is_none()
        {
            self.status.stop(ActorFailure::Panicked);
        }
        self.status.failure()
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn cleanup_pending(&self) -> bool {
        self.status.cleanup_pending.load(Ordering::Acquire)
    }

    pub fn finish(&mut self) -> bool {
        self.request_stop();
        if !self.is_finished() {
            return false;
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            self.status.stop(ActorFailure::Panicked);
        }
        true
    }
}

impl Drop for DestinationActor {
    fn drop(&mut self) {
        self.request_stop();
    }
}

fn construct_destination<D, F>(
    revocation: &RevocationSignal,
    status: &Status,
    ownership: InjectionPermit,
    factory: F,
) -> Result<D, ActorFailure>
where
    F: FnOnce(InjectionPermit) -> Result<D, DestinationFailure>,
{
    if let Some(failure) = status.failure() {
        return Err(failure);
    }
    if revocation.is_stopping() {
        return Err(ActorFailure::Revoked);
    }
    match catch_unwind(AssertUnwindSafe(|| factory(ownership))) {
        Ok(result) => result.map_err(|_| ActorFailure::Native),
        Err(payload) => {
            std::mem::forget(payload);
            Err(ActorFailure::Panicked)
        }
    }
}

struct DestinationGuard<D: WatchedDestination> {
    destination: D,
    status: Arc<Status>,
    armed: bool,
}

impl<D: WatchedDestination> InputDestination for DestinationGuard<D> {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        match catch_unwind(AssertUnwindSafe(|| self.destination.apply(action))) {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                self.status.stop(ActorFailure::Panicked);
                Err(DestinationFailure)
            }
        }
    }
}

impl<D: WatchedDestination> WatchedDestination for DestinationGuard<D> {
    fn local_displays_changed(&self) -> bool {
        self.destination.local_displays_changed()
    }
    fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
        match catch_unwind(AssertUnwindSafe(|| self.destination.validate_environment())) {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                self.status.stop(ActorFailure::Panicked);
                Err(DestinationFailure)
            }
        }
    }
}

impl<D: WatchedDestination> Drop for DestinationGuard<D> {
    fn drop(&mut self) {
        if self.armed {
            let pending = self.apply(DestinationAction::ReleaseAll).is_err();
            self.status
                .cleanup_pending
                .store(pending, Ordering::Release);
        }
    }
}

fn cleanup_without_receiver<D: WatchedDestination>(
    destination: &mut DestinationGuard<D>,
    status: &Status,
) {
    status.cleanup_pending.store(true, Ordering::Release);
    while destination.apply(DestinationAction::ReleaseAll).is_err() {
        thread::sleep(Duration::from_millis(25));
    }
    status.cleanup_pending.store(false, Ordering::Release);
    destination.armed = false;
}

fn cleanup_receiver<D: WatchedDestination>(
    receiver: &mut InputReceiver,
    destination: &mut DestinationGuard<D>,
    status: &Status,
) {
    status.cleanup_pending.store(true, Ordering::Release);
    receiver.stop(ReceiverFailure::PeerStopped, destination);
    while receiver.cleanup_pending() {
        thread::sleep(Duration::from_millis(25));
        receiver.retry_cleanup(destination);
    }
    status.cleanup_pending.store(false, Ordering::Release);
    destination.armed = false;
}

#[allow(clippy::too_many_arguments)]
fn run_receiver<D: WatchedDestination>(
    receiver: &mut InputReceiver,
    destination: &mut DestinationGuard<D>,
    mut sequences: OutputSequences,
    incoming: Receiver<Frame>,
    outgoing: SyncSender<Frame>,
    revocation: RevocationSignal,
    status: Arc<Status>,
    origin: SessionClock,
) {
    let processing = catch_unwind(AssertUnwindSafe(|| {
        let mut checked_environment = Duration::ZERO;
        let mut last_frame_at: Option<Duration> = None;
        while status.failure().is_none() {
            let now = origin.elapsed();
            if revocation.is_stopping() {
                status.stop(ActorFailure::Revoked);
                break;
            }
            if now.saturating_sub(checked_environment) >= crate::session::DISPLAY_CHECK_INTERVAL {
                let validated = destination.validate_environment();
                Stats::max(&status.stats.env_max_micros, micros_since(&origin, now));
                if validated.is_err() {
                    status.stop(if destination.local_displays_changed() {
                        ActorFailure::LocalDisplaysChanged
                    } else {
                        ActorFailure::Native
                    });
                    break;
                }
                checked_environment = now;
            }
            match receiver.take_back(destination) {
                Ok(Some(message)) => emit(message, receiver, &mut sequences, &outgoing, &status),
                Ok(None) => {}
                Err(failure) => {
                    stop_receiver(&status, receiver, failure, now, last_frame_at);
                    break;
                }
            }
            match receiver.tick(now, destination) {
                Ok(Some(message)) => emit(message, receiver, &mut sequences, &outgoing, &status),
                Ok(None) => {}
                Err(failure) => {
                    stop_receiver(&status, receiver, failure, now, last_frame_at);
                    break;
                }
            }
            note_hold(&status, receiver, now);
            if status.failure().is_some() {
                break;
            }
            let mut next = match incoming.try_recv() {
                Ok(frame) => Some(frame),
                Err(mpsc::TryRecvError::Empty) => {
                    thread::park_timeout(TICK);
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    status.stop(ActorFailure::PeerGone);
                    break;
                }
            };
            if next.is_none() || status.failure().is_some() || revocation.is_stopping() {
                continue;
            }
            // Everything already queued is validated at one time and injected once per burst, so
            // a fast mouse cannot push the peer's health replies behind its own motion.
            let now = origin.elapsed();
            if let Some(previous) = last_frame_at {
                Stats::max(
                    &status.stats.gap_max_millis,
                    millis_u64(now.saturating_sub(previous)),
                );
            }
            last_frame_at = Some(now);
            let mut batch = 0_u64;
            while let Some(frame) = next.take() {
                batch += 1;
                match receiver.take_back(destination) {
                    Ok(Some(message)) => {
                        emit(message, receiver, &mut sequences, &outgoing, &status)
                    }
                    Ok(None) => {}
                    Err(failure) => {
                        stop_receiver(&status, receiver, failure, now, last_frame_at);
                        break;
                    }
                }
                let received = receiver.receive(&frame, now, destination);
                status.active.store(false, Ordering::Release);
                if let Some(display) = receiver.active_display() {
                    status.display.store(display.0, Ordering::Relaxed);
                    status.active.store(true, Ordering::Release);
                }
                note_hold(&status, receiver, now);
                match received {
                    Ok(Some(message)) => {
                        emit(message, receiver, &mut sequences, &outgoing, &status)
                    }
                    Ok(None) => {}
                    Err(failure) => {
                        stop_receiver(&status, receiver, failure, now, last_frame_at);
                        break;
                    }
                }
                if batch < CAPACITY as u64 {
                    next = incoming.try_recv().ok();
                }
            }
            status.stats.frames.fetch_add(batch, Ordering::Relaxed);
            Stats::max(&status.stats.batch_max, batch);
            let bucket = match batch {
                0 | 1 => 0,
                2 | 3 => 1,
                4..=8 => 2,
                _ => 3,
            };
            status.stats.batch_buckets[bucket].fetch_add(1, Ordering::Relaxed);
            if status.failure().is_none()
                && let Err(failure) = receiver.flush(destination)
            {
                stop_receiver(&status, receiver, failure, now, last_frame_at);
            }
            let applied_micros = micros_since(&origin, now);
            Stats::max(&status.stats.apply_max_micros, applied_micros);
            if applied_micros > 2_000 {
                status.stats.slow_applies.fetch_add(1, Ordering::Relaxed);
            }
        }
    }));
    if let Err(payload) = processing {
        std::mem::forget(payload);
        status.stop(ActorFailure::Panicked);
    }
    cleanup_receiver(receiver, destination, &status);
}

fn micros_since(origin: &SessionClock, start: Duration) -> u64 {
    micros_u64(origin.elapsed().saturating_sub(start))
}

/// Stats first: a reader that acquires `held` then sees the hold it counts.
fn note_hold(status: &Status, receiver: &InputReceiver, now: Duration) {
    let (holds, held_max) = receiver.hold_stats(now);
    status
        .stats
        .holds
        .store(u64::from(holds), Ordering::Relaxed);
    status
        .stats
        .held_max_millis
        .store(millis_u64(held_max), Ordering::Relaxed);
    status
        .held
        .store(receiver.held_since().is_some(), Ordering::Release);
}

fn stop_receiver(
    status: &Status,
    receiver: &InputReceiver,
    failure: ReceiverFailure,
    now: Duration,
    last_frame_at: Option<Duration>,
) {
    let (reply_age, ping_age) = receiver.reply_ages(now);
    let stats = &status.stats;
    stats
        .reply_age_millis
        .store(millis_u64(reply_age), Ordering::Relaxed);
    stats
        .ping_age_millis
        .store(ping_age.map_or(u64::MAX, millis_u64), Ordering::Relaxed);
    stats.frame_age_millis.store(
        last_frame_at.map_or(u64::MAX, |at| millis_u64(now.saturating_sub(at))),
        Ordering::Relaxed,
    );
    status.stop(note_receiver_failure(failure));
}

struct OutputSequences {
    control: u64,
    input: u64,
    epoch: SessionEpoch,
}

fn emit(
    message: Message,
    receiver: &InputReceiver,
    sequences: &mut OutputSequences,
    outgoing: &SyncSender<Frame>,
    status: &Status,
) {
    let control = matches!(message, Message::Ping(_) | Message::Pong(_));
    let epoch = if control {
        receiver.control_epoch()
    } else {
        receiver.epoch()
    };
    if !control && epoch != sequences.epoch {
        sequences.epoch = epoch;
        sequences.input = 0;
    }
    let sequence = if control {
        &mut sequences.control
    } else {
        &mut sequences.input
    };
    let Some(next) = sequence.checked_add(1) else {
        status.stop(ActorFailure::SequenceExhausted);
        return;
    };
    let frame = Frame::new(epoch, *sequence, message);
    *sequence = next;
    if let Err(error) = outgoing.try_send(frame) {
        status.stop(match error {
            mpsc::TrySendError::Full(_) => ActorFailure::QueueFull,
            mpsc::TrySendError::Disconnected(_) => ActorFailure::PeerGone,
        });
    }
    if let Some(wake) = status.response_waker.get() {
        wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session_health::{PEER_LIVENESS, PeerHealth},
        session_startup::ReadyControl,
    };
    use monhop_core::{DisplayId, HidUsage, ModifierState, Point};
    use monhop_protocol::{DisplayDescription, Key, RateLimiter};
    use std::time::Instant;

    struct Probe {
        _ownership: InjectionPermit,
        actions: SyncSender<DestinationAction>,
    }

    impl InputDestination for Probe {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            let _ = self.actions.try_send(action);
            Ok(())
        }
    }

    impl WatchedDestination for Probe {
        fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
            Ok(())
        }
    }

    fn lock_test() -> std::sync::MutexGuard<'static, ()> {
        crate::NATIVE_OWNERSHIP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn stop_before_worker_construction_never_calls_the_native_factory() {
        let _test = lock_test();
        let ownership = monhop_core::NativeSessionClaim::claim().unwrap().split().1;
        let status = Status {
            stopped: AtomicU8::new(ActorFailure::Requested as u8),
            cleanup_pending: AtomicBool::new(false),
            native_ready: AtomicBool::new(false),
            started: AtomicBool::new(false),
            handoff_sent: AtomicBool::new(false),
            active: AtomicBool::new(false),
            held: AtomicBool::new(false),
            display: AtomicU64::new(0),
            stats: Stats::default(),
            response_waker: OnceLock::new(),
        };
        let result: Result<Probe, ActorFailure> =
            construct_destination(&RevocationSignal::default(), &status, ownership, |_| {
                panic!("stopped worker cannot enter native factory")
            });
        assert!(matches!(result, Err(ActorFailure::Requested)));
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    fn displays() -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(1),
            name: "fixture".into(),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::default(),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap()
    }

    fn ready_control(epoch: SessionEpoch, sequence: u64) -> ReadyControl {
        ReadyControl {
            epoch,
            next_incoming: sequence,
            next_outgoing: sequence,
            next_heartbeat_out: sequence,
            last_heartbeat_in: sequence.checked_sub(1),
            health: PeerHealth::new(Duration::ZERO),
            limiter: RateLimiter::new(20_000).unwrap(),
            now: Duration::ZERO,
        }
    }

    fn liveness_millis() -> u64 {
        u64::try_from(PEER_LIVENESS.as_millis()).unwrap()
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !condition() {
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
    }

    fn ready_probe(actions: SyncSender<DestinationAction>) -> DestinationActor {
        let ownership = monhop_core::NativeSessionClaim::claim()
            .expect("test owns native input")
            .split()
            .1;
        let actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            ownership,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                Ok(Probe {
                    _ownership: ownership,
                    actions,
                })
            },
        )
        .unwrap();
        wait_until(|| actor.is_native_ready());
        actor
            .handoff_ready(
                ready_control(SessionEpoch::new(1).unwrap(), 3),
                SessionClock::try_now().unwrap(),
            )
            .unwrap();
        wait_until(|| actor.is_started());
        actor
    }

    fn activation() -> Frame {
        Frame::new(
            SessionEpoch::new(2).unwrap(),
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(20.0, 20.0),
            },
        )
    }

    #[test]
    fn delayed_factory_returns_without_blocking_and_retains_ownership_until_stop() {
        let _test = lock_test();
        let (started, started_at_factory) = mpsc::sync_channel(1);
        let (release, wait_for_release) = mpsc::sync_channel(1);
        let (actions, _) = mpsc::sync_channel(10);
        let ownership = monhop_core::NativeSessionClaim::claim()
            .expect("test owns native input")
            .split()
            .1;
        let before = Instant::now();
        let mut actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            ownership,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                let _ = started.send(());
                wait_for_release.recv().unwrap();
                Ok(Probe {
                    _ownership: ownership,
                    actions,
                })
            },
        )
        .unwrap();
        assert!(before.elapsed() < Duration::from_millis(50));
        started_at_factory
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert!(monhop_core::NativeSessionClaim::is_claimed());
        actor.request_stop();
        release.send(()).unwrap();
        wait_until(|| actor.finish());
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    #[test]
    fn revoked_before_factory_prevents_construction_and_releases_ownership() {
        let _test = lock_test();
        let revocation = RevocationSignal::default();
        revocation.revoke();
        let ownership = monhop_core::NativeSessionClaim::claim()
            .expect("test owns native input")
            .split()
            .1;
        let constructed = Arc::new(AtomicBool::new(false));
        let factory_constructed = constructed.clone();
        assert!(matches!(
            DestinationActor::start_after_local_enable(
                displays(),
                revocation,
                ownership,
                TakeBackGate::new(monhop_core::SharedFloor::new()),
                false,
                true,
                move |_| {
                    factory_constructed.store(true, Ordering::Release);
                    Err::<Probe, _>(DestinationFailure)
                },
            ),
            Err(ActorFailure::Revoked)
        ));
        assert!(!constructed.load(Ordering::Acquire));
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    #[test]
    fn input_is_rejected_before_ready_handoff() {
        let _test = lock_test();
        let (actions, received) = mpsc::sync_channel(10);
        let ownership = monhop_core::NativeSessionClaim::claim()
            .expect("test owns native input")
            .split()
            .1;
        let mut actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            ownership,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                Ok(Probe {
                    _ownership: ownership,
                    actions,
                })
            },
        )
        .unwrap();
        wait_until(|| actor.is_native_ready());
        assert_eq!(actor.try_submit(activation()), Err(ActorFailure::Startup));
        wait_until(|| actor.finish());
        let actions = received.try_iter().collect::<Vec<_>>();
        assert!(actions.iter().all(|action| action.is_release()));
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    #[test]
    fn blocked_network_consumer_cannot_prevent_native_release() {
        let _test = lock_test();
        let (tx, rx) = mpsc::sync_channel(10);
        let mut actor = ready_probe(tx);
        actor.try_submit(activation()).unwrap();
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(2).unwrap(),
                1,
                Message::Key(Key {
                    usage: HidUsage(4),
                    is_down: true,
                    repeat: false,
                    modifiers: ModifierState(0),
                }),
            ))
            .unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            DestinationAction::MoveTo(_)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            DestinationAction::Key { pressed: true, .. }
        ));
        // Intentionally never drain responses or send a pong, as if the writer were stalled.
        assert!(matches!(
            rx.recv_timeout(PEER_LIVENESS + Duration::from_millis(200))
                .unwrap(),
            DestinationAction::EndGestures
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            DestinationAction::ReleaseAll
        ));
        // The release does not wait for the network, and the session holds instead of ending.
        wait_until(|| actor.is_held());
        assert!(actor.failure().is_none());
        assert_eq!(actor.stats().holds, 1);
        actor.request_stop();
        while !actor.finish() {
            thread::sleep(TICK);
        }
        assert!(!actor.cleanup_pending());
    }

    enum PauseAt {
        HandoffEnvironment,
        HeldKey,
    }

    struct PausedProbe {
        _ownership: InjectionPermit,
        actions: SyncSender<DestinationAction>,
        pause_at: PauseAt,
        environment_reads: usize,
        paused: SyncSender<()>,
        resume: Receiver<()>,
        allow_cleanup: Arc<AtomicBool>,
    }

    impl PausedProbe {
        fn pause(&self) {
            self.paused.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }
    impl InputDestination for PausedProbe {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            let _ = self.actions.try_send(action);
            if matches!(action, DestinationAction::ReleaseAll) {
                return self
                    .allow_cleanup
                    .load(Ordering::Acquire)
                    .then_some(())
                    .ok_or(DestinationFailure);
            }
            if matches!(self.pause_at, PauseAt::HeldKey)
                && matches!(action, DestinationAction::Key { pressed: true, .. })
            {
                self.pause();
            }
            Ok(())
        }
    }
    impl WatchedDestination for PausedProbe {
        fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
            self.environment_reads += 1;
            if matches!(self.pause_at, PauseAt::HandoffEnvironment) && self.environment_reads == 2 {
                self.pause();
            }
            Ok(())
        }
    }

    #[test]
    fn sleep_during_queued_handoff_expires_before_start_and_retains_cleanup_ownership() {
        let _test = lock_test();
        let now = Arc::new(AtomicU64::new(31));
        let reader = now.clone();
        let clock = SessionClock::with_test_reader(move || {
            Duration::from_millis(reader.load(Ordering::Acquire))
        });
        let (actions, received) = mpsc::sync_channel(32);
        let (paused, wait_for_pause) = mpsc::sync_channel(1);
        let (resume, wait_for_resume) = mpsc::sync_channel(1);
        let allow_cleanup = Arc::new(AtomicBool::new(false));
        let worker_cleanup = allow_cleanup.clone();
        let mut actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            monhop_core::NativeSessionClaim::claim().unwrap().split().1,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                Ok(PausedProbe {
                    _ownership: ownership,
                    actions,
                    pause_at: PauseAt::HandoffEnvironment,
                    environment_reads: 0,
                    paused,
                    resume: wait_for_resume,
                    allow_cleanup: worker_cleanup,
                })
            },
        )
        .unwrap();
        wait_until(|| actor.is_native_ready());
        actor
            .handoff_ready(ready_control(SessionEpoch::new(1).unwrap(), 3), clock)
            .unwrap();
        wait_for_pause.recv_timeout(Duration::from_secs(1)).unwrap();
        now.store(31 + liveness_millis(), Ordering::Release);
        resume.send(()).unwrap();
        wait_until(|| actor.cleanup_pending());
        assert_eq!(actor.failure(), Some(ActorFailure::Receiver));
        assert!(!actor.is_started());
        assert!(monhop_core::NativeSessionClaim::is_claimed());
        allow_cleanup.store(true, Ordering::Release);
        wait_until(|| actor.finish());
        let actions: Vec<_> = received.try_iter().collect();
        assert!(!actions.is_empty());
        assert!(actions.iter().all(|action| action.is_release()));
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    #[test]
    fn queued_matching_pong_after_sleep_cannot_refresh_health_or_deliver_more_input() {
        let _test = lock_test();
        let now = Arc::new(AtomicU64::new(1));
        let reader_now = now.clone();
        let pause_read = Arc::new(AtomicBool::new(false));
        let reader_pause = pause_read.clone();
        let (clock_paused, wait_clock_pause) = mpsc::sync_channel(1);
        let (resume_clock, wait_resume_clock) = mpsc::sync_channel(1);
        let wait_resume_clock = std::sync::Mutex::new(wait_resume_clock);
        let clock = SessionClock::with_test_reader(move || {
            let sampled = reader_now.load(Ordering::Acquire);
            if reader_pause.swap(false, Ordering::AcqRel) {
                clock_paused.send(()).unwrap();
                wait_resume_clock
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
            Duration::from_millis(sampled)
        });
        let (actions, received) = mpsc::sync_channel(32);
        let (paused, wait_for_pause) = mpsc::sync_channel(1);
        let (resume, wait_for_resume) = mpsc::sync_channel(1);
        let mut actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            monhop_core::NativeSessionClaim::claim().unwrap().split().1,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                Ok(PausedProbe {
                    _ownership: ownership,
                    actions,
                    pause_at: PauseAt::HeldKey,
                    environment_reads: 0,
                    paused,
                    resume: wait_for_resume,
                    allow_cleanup: Arc::new(AtomicBool::new(true)),
                })
            },
        )
        .unwrap();
        wait_until(|| actor.is_native_ready());
        actor
            .handoff_ready(ready_control(SessionEpoch::new(1).unwrap(), 3), clock)
            .unwrap();
        wait_until(|| actor.is_started());
        actor.try_submit(activation()).unwrap();
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(2).unwrap(),
                1,
                Message::Key(Key {
                    usage: HidUsage(4),
                    is_down: true,
                    repeat: false,
                    modifiers: ModifierState(0),
                }),
            ))
            .unwrap();
        wait_for_pause.recv_timeout(Duration::from_secs(1)).unwrap();
        let ping = actor
            .outbound
            .try_iter()
            .find(|frame| matches!(frame.message, Message::Ping(_)))
            .unwrap();
        let Message::Ping(token) = ping.message else {
            unreachable!()
        };
        pause_read.store(true, Ordering::Release);
        resume.send(()).unwrap();
        wait_clock_pause
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(1).unwrap(),
                3,
                Message::Pong(token),
            ))
            .unwrap();
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(2).unwrap(),
                2,
                Message::Key(Key {
                    usage: HidUsage(5),
                    is_down: true,
                    repeat: false,
                    modifiers: ModifierState(0),
                }),
            ))
            .unwrap();
        // Resume the owner with an old loop sample, but a fresh receive-time clock past expiry.
        now.store(1 + liveness_millis(), Ordering::Release);
        resume_clock.send(()).unwrap();
        // The stale reply earns no credit and the queued key never lands: the receiver holds.
        wait_until(|| actor.is_held());
        assert!(actor.failure().is_none());
        actor.request_stop();
        wait_until(|| actor.is_finished());
        assert!(actor.finish());
        let actions: Vec<_> = received.try_iter().collect();
        assert!(actions.len() >= 3);
        assert!(matches!(actions[0], DestinationAction::MoveTo(_)));
        assert!(matches!(
            actions[1],
            DestinationAction::Key {
                usage: HidUsage(4),
                pressed: true
            }
        ));
        assert!(actions[2..].iter().all(|action| action.is_release()));
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }

    struct PanickingDestination {
        _ownership: InjectionPermit,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
        released: SyncSender<()>,
        held: bool,
    }

    impl InputDestination for PanickingDestination {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            match action {
                DestinationAction::Key { pressed: true, .. } => {
                    self.held = true;
                    panic!("fixture failure after native admission");
                }
                DestinationAction::ReleaseAll => {
                    let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    if self.held && attempt < 3 {
                        return Err(DestinationFailure);
                    }
                    self.held = false;
                    let _ = self.released.try_send(());
                }
                _ => {}
            }
            Ok(())
        }
    }

    impl WatchedDestination for PanickingDestination {
        fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
            Ok(())
        }
    }

    #[test]
    fn panic_keeps_native_ownership_until_release_succeeds_without_a_redundant_drop_release() {
        let _test = lock_test();
        let (released, done) = mpsc::sync_channel(10);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let ownership = monhop_core::NativeSessionClaim::claim()
            .expect("test owns native input")
            .split()
            .1;
        let mut actor = DestinationActor::start_after_local_enable(
            displays(),
            RevocationSignal::default(),
            ownership,
            TakeBackGate::new(monhop_core::SharedFloor::new()),
            false,
            true,
            move |ownership| {
                Ok(PanickingDestination {
                    _ownership: ownership,
                    attempts: worker_attempts,
                    released,
                    held: false,
                })
            },
        )
        .unwrap();
        wait_until(|| actor.is_native_ready());
        actor
            .handoff_ready(
                ready_control(SessionEpoch::new(1).unwrap(), 3),
                SessionClock::try_now().unwrap(),
            )
            .unwrap();
        wait_until(|| actor.is_started());
        actor.try_submit(activation()).unwrap();
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(2).unwrap(),
                1,
                Message::Key(Key {
                    usage: HidUsage(4),
                    is_down: true,
                    repeat: false,
                    modifiers: ModifierState(0),
                }),
            ))
            .unwrap();
        done.recv_timeout(Duration::from_millis(500)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        while !actor.finish() {
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
        assert_eq!(actor.failure(), Some(ActorFailure::Panicked));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(!actor.cleanup_pending());
        assert!(!monhop_core::NativeSessionClaim::is_claimed());
    }
}
