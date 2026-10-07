//! One destination actor thread shared by every peer link of the share hub.

use crate::{
    session_actor::{
        ActorFailure, DestinationGuard, EnvironmentWatch, ReceiverStats, Stats, Status, TICK,
        WatchedDestination, check_environment, cleanup_without_receiver, construct_destination,
        micros_since, note_hold, note_receiver_failure, stop_receiver,
    },
    session_clock::{SessionClock, millis_u64},
    session_receiver::{DestinationAction, DestinationFailure, InputDestination, InputReceiver},
    session_receiver_set::{ReceiverSet, SlotFailure, SlotResult},
    session_startup::ReadyControl,
};
use monhop_core::{
    DisplayId, FloorPeer, InjectionPermit, MAX_GROUP_PEERS, RevocationSignal, TakeBackGate,
};
use monhop_protocol::{DisplayTopology, Frame};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const CAPACITY: usize = crate::session::SESSION_QUEUE_CAPACITY;
/// A slot has at most one join and one leave queued: it is joined again only once its leave is
/// observed.
const COMMANDS: usize = 2 * MAX_GROUP_PEERS;

/// What one peer's slot reports, in the order the actor produced it.
#[derive(Clone, Debug, PartialEq)]
pub enum SlotEvent {
    /// A reply for the slot's peer, already sequenced for it.
    Response(Frame),
    /// The slot's receiver stopped on its own; [`SlotEvent::Left`] follows once its release lands.
    Failed(SlotFailure),
    /// The slot is empty: its receiver stopped, nothing it injected is still held and the floor is
    /// no longer its. Exactly one follows every join.
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRefusal {
    /// The actor stopped: every slot ended and native input was released.
    Stopped(ActorFailure),
    /// Not a group slot or no peer runs in it; on join, the slot's last peer has not left yet.
    Slot(SlotFailure),
    /// This peer's frames outran injection; only its link need end.
    QueueFull,
}

/// One injection thread, destination and environment watch for every peer. Each peer's receiver
/// runs in its slot of one [`ReceiverSet`], so only the floor's owner reaches native input.
pub struct HubDestinationActor {
    commands: SyncSender<Command>,
    outbound: Receiver<(FloorPeer, SlotEvent)>,
    seats: [Seat; MAX_GROUP_PEERS],
    status: Arc<Status>,
    worker: Option<JoinHandle<()>>,
}

/// A slot as the handle sees it: joined from `add_peer` until its [`SlotEvent::Left`] is taken.
#[derive(Default)]
struct Seat {
    joined: bool,
    /// None once the peer is removed.
    frames: Option<SyncSender<Frame>>,
    /// The latest peer's, kept after it leaves so its end can still be reported.
    status: Option<Arc<Status>>,
}

enum Command {
    Join(Box<Join>),
    Leave(FloorPeer),
}

struct Join {
    slot: FloorPeer,
    control: ReadyControl,
    origin: SessionClock,
    local_lower: bool,
    enabled: bool,
    frames: Receiver<Frame>,
    status: Arc<Status>,
}

impl HubDestinationActor {
    /// Factory construction and all destination operations stay on the actor thread. The
    /// environment is watched from native readiness until the actor stops, with or without peers.
    pub(crate) fn start_after_local_enable<D, F>(
        displays: DisplayTopology,
        revocation: RevocationSignal,
        ownership: InjectionPermit,
        gate: TakeBackGate,
        factory: F,
    ) -> Result<Self, ActorFailure>
    where
        D: WatchedDestination + 'static,
        F: FnOnce(InjectionPermit) -> Result<D, DestinationFailure> + Send + 'static,
    {
        if revocation.is_stopping() {
            return Err(ActorFailure::Revoked);
        }
        let (commands, incoming) = mpsc::sync_channel(COMMANDS);
        let (outgoing, outbound) = mpsc::sync_channel(CAPACITY * MAX_GROUP_PEERS);
        let status = Arc::new(Status::default());
        let worker_status = status.clone();
        let worker = thread::Builder::new()
            .name("monhop-hub-destination".into())
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
                let mut environment = destination.destination.environment();
                if revocation.is_stopping() || status.failure().is_some() {
                    status.stop(ActorFailure::Revoked);
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                if let Err(failure) = check_environment(&mut environment) {
                    status.stop(failure);
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                }
                let Ok(watch) =
                    EnvironmentWatch::start(environment, status.clone(), revocation.clone())
                else {
                    status.stop(ActorFailure::Startup);
                    cleanup_without_receiver(&mut destination, &status);
                    return;
                };
                status.native_ready.store(true, Ordering::Release);
                let mut worker = Worker {
                    set: ReceiverSet::new(gate.clone()),
                    peers: std::array::from_fn(|_| None),
                    destination,
                    displays,
                    gate,
                    commands: incoming,
                    outgoing,
                    revocation,
                    status,
                };
                worker.run();
                drop(watch);
            })
            .map_err(|_| ActorFailure::Startup)?;
        Ok(Self {
            commands,
            outbound,
            seats: Default::default(),
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

    /// Runs `slot`'s receiver for a peer fresh from startup, on that peer's session clock.
    pub(crate) fn add_peer(
        &mut self,
        slot: FloorPeer,
        control: ReadyControl,
        origin: SessionClock,
        local_lower: bool,
        enabled: bool,
    ) -> Result<(), PeerRefusal> {
        let index = index(slot).ok_or(PeerRefusal::Slot(SlotFailure::UnknownSlot))?;
        if let Some(failure) = self.failure() {
            return Err(PeerRefusal::Stopped(failure));
        }
        if self.seats[index].joined {
            return Err(PeerRefusal::Slot(SlotFailure::Occupied));
        }
        let (frames, incoming) = mpsc::sync_channel(CAPACITY);
        let status = Arc::new(Status::default());
        self.command(Command::Join(Box::new(Join {
            slot,
            control,
            origin,
            local_lower,
            enabled,
            frames: incoming,
            status: status.clone(),
        })))?;
        self.seats[index] = Seat {
            joined: true,
            frames: Some(frames),
            status: Some(status),
        };
        Ok(())
    }

    /// Stops `slot`'s receiver; [`SlotEvent::Left`] follows once everything it injected is
    /// released. A slot with no peer, or one already leaving, is left as it is.
    pub fn remove_peer(&mut self, slot: FloorPeer) -> Result<(), PeerRefusal> {
        let index = index(slot).ok_or(PeerRefusal::Slot(SlotFailure::UnknownSlot))?;
        if let Some(failure) = self.failure() {
            return Err(PeerRefusal::Stopped(failure));
        }
        if self.seats[index].frames.take().is_none() {
            return Ok(());
        }
        self.command(Command::Leave(slot))
    }

    pub fn try_submit(&self, slot: FloorPeer, frame: Frame) -> Result<(), PeerRefusal> {
        if let Some(failure) = self.failure() {
            return Err(PeerRefusal::Stopped(failure));
        }
        let frames = index(slot)
            .and_then(|index| self.seats[index].frames.as_ref())
            .ok_or(PeerRefusal::Slot(SlotFailure::UnknownSlot))?;
        frames.try_send(frame).map_err(|error| match error {
            TrySendError::Full(_) => PeerRefusal::QueueFull,
            TrySendError::Disconnected(_) => PeerRefusal::Slot(SlotFailure::UnknownSlot),
        })?;
        self.wake_worker();
        Ok(())
    }

    pub fn try_response(&mut self) -> Result<Option<(FloorPeer, SlotEvent)>, ActorFailure> {
        if let Some(reason) = self.failure() {
            return Err(reason);
        }
        match self.outbound.try_recv() {
            Ok(event) => {
                if let Some(reason) = self.failure() {
                    return Err(reason);
                }
                if let (slot, SlotEvent::Left) = &event
                    && let Some(index) = index(*slot)
                {
                    self.seats[index].joined = false;
                    self.seats[index].frames = None;
                }
                Ok(Some(event))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.status.stop(ActorFailure::PeerGone);
                Err(ActorFailure::PeerGone)
            }
        }
    }

    pub fn is_started(&self, slot: FloorPeer) -> bool {
        self.failure().is_none()
            && self.peer(slot).is_some_and(|status| {
                status.started.load(Ordering::Acquire) && status.failure().is_none()
            })
    }

    /// True while `slot`'s source has been silent past the deadline and its receiver waits for
    /// the barrier with everything released.
    pub fn is_held(&self, slot: FloorPeer) -> bool {
        self.peer(slot)
            .is_some_and(|status| status.held.load(Ordering::Acquire))
    }

    pub fn active_display(&self, slot: FloorPeer) -> Option<DisplayId> {
        let status = self.peer(slot)?;
        (self.is_started(slot) && status.active.load(Ordering::Acquire))
            .then(|| DisplayId(status.display.load(Ordering::Relaxed)))
    }

    /// Why `slot`'s latest peer stopped; kept after it left.
    pub fn peer_failure(&self, slot: FloorPeer) -> Option<ActorFailure> {
        self.peer(slot).and_then(Status::failure)
    }

    /// True while the release of what `slot`'s leaving peer injected keeps failing.
    pub fn peer_cleanup_pending(&self, slot: FloorPeer) -> bool {
        self.peer(slot)
            .is_some_and(|status| status.cleanup_pending.load(Ordering::Acquire))
    }

    /// `slot`'s latest peer's receiver load; kept after it left.
    pub fn stats(&self, slot: FloorPeer) -> ReceiverStats {
        let mut stats = self
            .peer(slot)
            .map(|status| status.stats.report())
            .unwrap_or_default();
        stats.env_max_micros = self.status.stats.env_max_micros.load(Ordering::Relaxed);
        stats
    }

    pub fn request_stop(&self) {
        self.status.stop(ActorFailure::Requested);
        self.wake_worker();
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

    /// True while the actor's final native release keeps failing.
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

    fn peer(&self, slot: FloorPeer) -> Option<&Status> {
        index(slot).and_then(|index| self.seats[index].status.as_deref())
    }

    fn command(&self, command: Command) -> Result<(), PeerRefusal> {
        self.commands.try_send(command).map_err(|error| {
            let failure = match error {
                TrySendError::Full(_) => ActorFailure::QueueFull,
                TrySendError::Disconnected(_) => ActorFailure::PeerGone,
            };
            self.status.stop(failure);
            PeerRefusal::Stopped(failure)
        })?;
        self.wake_worker();
        Ok(())
    }

    fn wake_worker(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}

impl Drop for HubDestinationActor {
    fn drop(&mut self) {
        self.request_stop();
    }
}

/// A peer on the actor thread; `frames` is None once it leaves.
struct Peer {
    slot: FloorPeer,
    origin: SessionClock,
    frames: Option<Receiver<Frame>>,
    status: Arc<Status>,
    last_frame_at: Option<Duration>,
}

struct Worker<D: WatchedDestination> {
    set: ReceiverSet,
    peers: [Option<Peer>; MAX_GROUP_PEERS],
    destination: DestinationGuard<D>,
    displays: DisplayTopology,
    gate: TakeBackGate,
    commands: Receiver<Command>,
    outgoing: SyncSender<(FloorPeer, SlotEvent)>,
    revocation: RevocationSignal,
    status: Arc<Status>,
}

impl<D: WatchedDestination> Worker<D> {
    fn run(&mut self) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.drive())) {
            std::mem::forget(payload);
            self.status.stop(ActorFailure::Panicked);
        }
        self.shut_down();
    }

    /// The pairwise loop for every slot: the take-back before input, each slot's tick, then each
    /// slot's queued frames.
    fn drive(&mut self) {
        while self.status.failure().is_none() {
            if self.revocation.is_stopping() {
                self.status.stop(ActorFailure::Revoked);
                break;
            }
            let mut busy = self.take_commands();
            self.take_back();
            for index in 0..MAX_GROUP_PEERS {
                self.tick(index);
            }
            self.destination.tick();
            if self.status.failure().is_some() {
                break;
            }
            for index in 0..MAX_GROUP_PEERS {
                busy |= self.receive(index);
            }
            if !busy {
                thread::park_timeout(TICK);
            }
        }
    }

    fn take_commands(&mut self) -> bool {
        let mut taken = false;
        loop {
            match self.commands.try_recv() {
                Ok(Command::Join(join)) => self.join(*join),
                Ok(Command::Leave(slot)) => self.leave(slot),
                Err(TryRecvError::Empty) => return taken,
                Err(TryRecvError::Disconnected) => {
                    self.status.stop(ActorFailure::PeerGone);
                    return taken;
                }
            }
            taken = true;
        }
    }

    fn join(&mut self, join: Join) {
        let Join {
            slot,
            control,
            origin,
            local_lower,
            enabled,
            frames,
            status,
        } = join;
        let Some(index) = index(slot) else {
            return;
        };
        let next_heartbeat_out = control.next_heartbeat_out;
        let joined =
            match InputReceiver::after_startup(self.displays.clone(), control, origin.elapsed()) {
                Ok(receiver) => {
                    self.set
                        .add(slot, receiver, local_lower, enabled, next_heartbeat_out)
                }
                Err(failure) => {
                    status.stop(note_receiver_failure(failure));
                    Err(SlotFailure::Receiver(failure))
                }
            };
        match joined {
            Ok(()) => {
                status.started.store(true, Ordering::Release);
                self.peers[index] = Some(Peer {
                    slot,
                    origin,
                    frames: Some(frames),
                    status,
                    last_frame_at: None,
                });
            }
            Err(failure) => {
                self.emit(slot, SlotEvent::Failed(failure));
                // An occupied slot's Left belongs to the peer still in it.
                if self.peers[index].is_none() {
                    self.emit(slot, SlotEvent::Left);
                }
            }
        }
    }

    fn leave(&mut self, slot: FloorPeer) {
        if let Some(peer) = index(slot).and_then(|index| self.peers[index].as_mut()) {
            peer.status.stop(ActorFailure::Requested);
            peer.frames = None;
        }
    }

    /// The set hands a trigger only to the floor's owner; a leaving owner's removal answers it.
    fn take_back(&mut self) {
        let Some((slot, result)) = self.set.take_back(&mut self.destination) else {
            return;
        };
        let Some(peer) = index(slot).and_then(|index| self.peers[index].as_ref()) else {
            return;
        };
        if peer.frames.is_some() {
            let now = peer.origin.elapsed();
            self.respond(slot, result, now);
        }
    }

    fn tick(&mut self, index: usize) {
        let Some(peer) = &self.peers[index] else {
            return;
        };
        let (slot, now) = (peer.slot, peer.origin.elapsed());
        if peer.frames.is_none() {
            self.retire(index);
            return;
        }
        let ticked = self.set.tick(slot, now, &mut self.destination);
        self.note_receiver(index, now);
        self.respond(slot, ticked, now);
    }

    /// Retried every pass: `remove` keeps a receiving slot's floor until its release lands.
    fn retire(&mut self, index: usize) {
        let Some(peer) = &self.peers[index] else {
            return;
        };
        let slot = peer.slot;
        let removed = self.set.remove(slot, &mut self.destination);
        peer.status
            .cleanup_pending
            .store(!removed, Ordering::Release);
        if removed {
            self.peers[index] = None;
            self.emit(slot, SlotEvent::Left);
        }
    }

    /// Everything already queued for the slot is validated at one time and injected once per
    /// burst, as the pairwise loop does.
    fn receive(&mut self, index: usize) -> bool {
        let Some(peer) = self.peers[index].as_mut() else {
            return false;
        };
        let Some(Ok(first)) = peer.frames.as_ref().map(Receiver::try_recv) else {
            return false;
        };
        let (slot, now) = (peer.slot, peer.origin.elapsed());
        if let Some(previous) = peer.last_frame_at {
            Stats::max(
                &peer.status.stats.gap_max_millis,
                millis_u64(now.saturating_sub(previous)),
            );
        }
        peer.last_frame_at = Some(now);
        let mut next = Some(first);
        let mut batch = 0_u64;
        while let Some(frame) = next.take() {
            batch += 1;
            self.take_back();
            if !self.is_receiving(index) {
                break;
            }
            let received = self.set.receive(slot, &frame, now, &mut self.destination);
            self.note_receiver(index, now);
            let stopped = received.is_err();
            self.respond(slot, received, now);
            if stopped {
                break;
            }
            if batch < CAPACITY as u64 {
                next = self.queued(index);
            }
        }
        if self.status.failure().is_none()
            && self.is_receiving(index)
            && let Err(failure) = self.set.flush(slot, &mut self.destination)
        {
            self.fail(slot, failure, now);
        }
        if let Some(peer) = &self.peers[index] {
            record_batch(&peer.status.stats, batch, micros_since(&peer.origin, now));
        }
        true
    }

    fn is_receiving(&self, index: usize) -> bool {
        self.peers[index]
            .as_ref()
            .is_some_and(|peer| peer.frames.is_some())
    }

    fn queued(&self, index: usize) -> Option<Frame> {
        self.peers[index]
            .as_ref()
            .and_then(|peer| peer.frames.as_ref())
            .and_then(|frames| frames.try_recv().ok())
    }

    fn note_receiver(&self, index: usize, now: Duration) {
        let Some(peer) = &self.peers[index] else {
            return;
        };
        let Some(receiver) = self.set.receiver(peer.slot) else {
            return;
        };
        let status = &peer.status;
        status.active.store(false, Ordering::Release);
        if let Some(display) = receiver.active_display() {
            status.display.store(display.0, Ordering::Relaxed);
            status.active.store(true, Ordering::Release);
        }
        note_hold(status, receiver, now);
    }

    fn respond(&mut self, slot: FloorPeer, result: SlotResult, now: Duration) {
        match result {
            Ok(Some(frame)) => self.emit(slot, SlotEvent::Response(frame)),
            Ok(None) => {}
            Err(failure) => self.fail(slot, failure, now),
        }
    }

    /// Reported once; the slot then leaves as a removed one does.
    fn fail(&mut self, slot: FloorPeer, failure: SlotFailure, now: Duration) {
        let Some(peer) = index(slot).and_then(|index| self.peers[index].as_mut()) else {
            return;
        };
        if peer.frames.take().is_none() {
            return;
        }
        match (failure, self.set.receiver(slot)) {
            (SlotFailure::Receiver(stopped), Some(receiver)) => {
                stop_receiver(&peer.status, receiver, stopped, now, peer.last_frame_at);
            }
            (SlotFailure::Receiver(stopped), None) => {
                peer.status.stop(note_receiver_failure(stopped));
            }
            (SlotFailure::SequenceExhausted, _) => {
                peer.status.stop(ActorFailure::SequenceExhausted);
            }
            (SlotFailure::UnknownSlot | SlotFailure::Occupied, _) => {
                peer.status.stop(ActorFailure::Startup);
            }
        }
        self.emit(slot, SlotEvent::Failed(failure));
    }

    fn emit(&self, slot: FloorPeer, event: SlotEvent) {
        if let Err(error) = self.outgoing.try_send((slot, event)) {
            self.status.stop(match error {
                TrySendError::Full(_) => ActorFailure::QueueFull,
                TrySendError::Disconnected(_) => ActorFailure::PeerGone,
            });
        }
        if let Some(wake) = self.status.response_waker.get() {
            wake();
        }
    }

    /// Native input is released once, directly and whoever holds the floor, after admission
    /// closes; each receiver then stops as bookkeeping, which frees the floor last.
    fn shut_down(&mut self) {
        let reason = self.status.failure().unwrap_or(ActorFailure::Requested);
        self.gate.open_injection(0);
        cleanup_without_receiver(&mut self.destination, &self.status);
        for peer in self.peers.iter_mut().filter_map(Option::take) {
            peer.status.stop(reason);
            self.set.remove(peer.slot, &mut Released);
            peer.status.cleanup_pending.store(false, Ordering::Release);
        }
    }
}

/// Stands in for a destination the actor already released directly: a stopping receiver's own
/// release is bookkeeping, and nothing else may reach native input any more.
struct Released;

impl InputDestination for Released {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        if action.is_release() {
            Ok(())
        } else {
            Err(DestinationFailure)
        }
    }
}

fn record_batch(stats: &Stats, batch: u64, applied_micros: u64) {
    stats.frames.fetch_add(batch, Ordering::Relaxed);
    Stats::max(&stats.batch_max, batch);
    let bucket = match batch {
        0 | 1 => 0,
        2 | 3 => 1,
        4..=8 => 2,
        _ => 3,
    };
    stats.batch_buckets[bucket].fetch_add(1, Ordering::Relaxed);
    Stats::max(&stats.apply_max_micros, applied_micros);
    if applied_micros > 2_000 {
        stats.slow_applies.fetch_add(1, Ordering::Relaxed);
    }
}

/// Slot 1 is index 0; [`FloorPeer::NONE`] has none.
fn index(slot: FloorPeer) -> Option<usize> {
    usize::from(slot.get())
        .checked_sub(1)
        .filter(|&index| index < MAX_GROUP_PEERS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session_actor::{EnvironmentFailure, WatchedEnvironment},
        session_health::PeerHealth,
    };
    use DestinationAction::{EndGestures, MoveTo, ReleaseAll};
    use monhop_core::{
        FloorSnapshot, FloorState, HidUsage, ModifierState, Point, SharedFloor,
        capture::{CaptureEvent, CaptureStop, capture_channel},
        capture_physical::PhysicalCapture,
    };
    use monhop_protocol::{
        DeclineReason, DisplayDescription, Key, Message, RateLimiter, SessionEpoch,
    };
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize},
        time::Instant,
    };

    struct Environment {
        failing: Arc<AtomicBool>,
    }

    impl WatchedEnvironment for Environment {
        fn validate(&mut self) -> Result<(), EnvironmentFailure> {
            if self.failing.load(Ordering::Acquire) {
                Err(EnvironmentFailure::DisplaysChanged)
            } else {
                Ok(())
            }
        }
    }

    /// Records each action with the floor it met; ReleaseAll fails while `refuse_release` is set.
    struct Probe {
        _ownership: InjectionPermit,
        floor: SharedFloor,
        actions: SyncSender<(DestinationAction, FloorSnapshot)>,
        refuse_release: Arc<AtomicBool>,
        environment: Option<Environment>,
        ticks: Arc<AtomicUsize>,
    }

    impl InputDestination for Probe {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            let _ = self.actions.try_send((action, self.floor.snapshot()));
            if action == ReleaseAll && self.refuse_release.load(Ordering::Acquire) {
                return Err(DestinationFailure);
            }
            Ok(())
        }
    }

    impl WatchedDestination for Probe {
        type Environment = Environment;
        fn environment(&mut self) -> Environment {
            self.environment.take().expect("taken once")
        }
        fn tick(&mut self) {
            self.ticks.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct Hub {
        actor: HubDestinationActor,
        gate: TakeBackGate,
        actions: Receiver<(DestinationAction, FloorSnapshot)>,
        refuse_release: Arc<AtomicBool>,
        environment_fails: Arc<AtomicBool>,
        wakes: Arc<AtomicUsize>,
        ticks: Arc<AtomicUsize>,
    }

    impl Hub {
        fn start() -> Self {
            let gate = TakeBackGate::new(SharedFloor::new());
            let (actions, recorded) = mpsc::sync_channel(4096);
            let refuse_release = Arc::new(AtomicBool::new(false));
            let environment_fails = Arc::new(AtomicBool::new(false));
            let floor = gate.floor().clone();
            let refused = refuse_release.clone();
            let failing = environment_fails.clone();
            let ticks = Arc::new(AtomicUsize::new(0));
            let ticked = ticks.clone();
            let ownership = monhop_core::NativeSessionClaim::claim()
                .expect("test owns native input")
                .split()
                .1;
            let actor = HubDestinationActor::start_after_local_enable(
                displays(),
                RevocationSignal::default(),
                ownership,
                gate.clone(),
                move |ownership| {
                    Ok(Probe {
                        _ownership: ownership,
                        floor,
                        actions,
                        refuse_release: refused,
                        environment: Some(Environment { failing }),
                        ticks: ticked,
                    })
                },
            )
            .unwrap();
            assert!(gate.set_waker(actor.waker()));
            let wakes = Arc::new(AtomicUsize::new(0));
            let counted = wakes.clone();
            actor.set_response_waker(Arc::new(move || {
                counted.fetch_add(1, Ordering::AcqRel);
            }));
            wait_until(|| actor.is_native_ready());
            Self {
                actor,
                gate,
                actions: recorded,
                refuse_release,
                environment_fails,
                wakes,
                ticks,
            }
        }

        /// On a clock that never advances, so the peer never goes silent mid-test.
        fn join(&mut self, slot: FloorPeer) {
            let frozen = SessionClock::with_test_reader(|| Duration::from_millis(1));
            self.actor
                .add_peer(slot, ready_control(), frozen, false, true)
                .unwrap();
            wait_until(|| self.actor.is_started(slot));
        }

        fn submit(&self, slot: FloorPeer, frame: Frame) {
            self.actor.try_submit(slot, frame).unwrap();
        }

        /// Activates `slot` and presses a key there; returns the floor it then holds.
        fn take_floor(&mut self, slot: FloorPeer) -> FloorSnapshot {
            self.submit(slot, activation(2));
            self.submit(slot, key(2, 1, true));
            self.events_until(
                slot,
                SlotEvent::Response(frame(2, 0, Message::ActivationAck(DisplayId(1)))),
            );
            assert_eq!(self.next_action().0, MoveTo(Point::new(10.0, 10.0)));
            assert_eq!(self.next_action().0, pressed(true));
            let floor = self.gate.floor().snapshot();
            assert_eq!((floor.state, floor.peer), (FloorState::Receiving, slot));
            floor
        }

        fn next_action(&self) -> (DestinationAction, FloorSnapshot) {
            self.actions
                .recv_timeout(Duration::from_millis(500))
                .unwrap()
        }

        fn recorded(&self) -> Vec<(DestinationAction, FloorSnapshot)> {
            self.actions.try_iter().collect()
        }

        /// Every event taken up to and including `wanted` from `slot`.
        fn events_until(
            &mut self,
            slot: FloorPeer,
            wanted: SlotEvent,
        ) -> Vec<(FloorPeer, SlotEvent)> {
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut seen = Vec::new();
            loop {
                if let Some(event) = self.actor.try_response().unwrap() {
                    let found = event.0 == slot && event.1 == wanted;
                    seen.push(event);
                    if found {
                        return seen;
                    }
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "{wanted:?} never reached {slot:?}; saw {seen:?}"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }

        fn drain(&mut self) -> Vec<(FloorPeer, SlotEvent)> {
            std::iter::from_fn(|| self.actor.try_response().unwrap()).collect()
        }

        /// What reached the destination since it was last read, the shutdown release included.
        fn finish(&mut self) -> Vec<(DestinationAction, FloorSnapshot)> {
            self.actor.request_stop();
            wait_until(|| self.actor.finish());
            assert!(!self.actor.cleanup_pending());
            assert!(!monhop_core::NativeSessionClaim::is_claimed());
            self.recorded()
        }
    }

    fn lock_test() -> std::sync::MutexGuard<'static, ()> {
        crate::NATIVE_OWNERSHIP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while !condition() {
            assert!(Instant::now() < deadline);
            thread::sleep(TICK);
        }
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

    fn ready_control() -> ReadyControl {
        ReadyControl {
            epoch: SessionEpoch::new(1).unwrap(),
            next_incoming: 3,
            next_outgoing: 3,
            next_heartbeat_out: 3,
            last_heartbeat_in: Some(2),
            health: PeerHealth::new(Duration::ZERO),
            limiter: RateLimiter::new(20_000).unwrap(),
            now: Duration::ZERO,
        }
    }

    fn slot(number: u8) -> FloorPeer {
        FloorPeer::slot(number).unwrap()
    }

    fn frame(epoch: u64, sequence: u64, message: Message) -> Frame {
        Frame::new(SessionEpoch::new(epoch).unwrap(), sequence, message)
    }

    fn activation(epoch: u64) -> Frame {
        frame(
            epoch,
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(10.0, 10.0),
                control_as_command: false,
            },
        )
    }

    fn key(epoch: u64, sequence: u64, is_down: bool) -> Frame {
        frame(
            epoch,
            sequence,
            Message::Key(Key {
                usage: HidUsage(4),
                is_down,
                repeat: false,
                modifiers: ModifierState(0),
            }),
        )
    }

    fn pressed(pressed: bool) -> DestinationAction {
        DestinationAction::Key {
            usage: HidUsage(4),
            pressed,
        }
    }

    fn actions(recorded: &[(DestinationAction, FloorSnapshot)]) -> Vec<DestinationAction> {
        recorded.iter().map(|(action, _)| *action).collect()
    }

    fn take_back_locally(gate: &TakeBackGate) {
        let stop = CaptureStop::new();
        let (mut producer, _consumer) = capture_channel(stop.clone());
        let mut physical = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        physical.process(
            CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 },
            false,
            Duration::ZERO,
            &mut producer,
            &stop,
        );
    }

    #[test]
    fn a_second_peer_joins_and_leaves_without_disturbing_the_first() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        let owned = hub.take_floor(slot(1));
        hub.join(slot(2));
        // The newcomer answers its own heartbeat, on its own sequence.
        hub.events_until(slot(2), SlotEvent::Response(frame(1, 3, Message::Ping(1))));
        hub.actor.remove_peer(slot(2)).unwrap();
        hub.events_until(slot(2), SlotEvent::Left);
        assert_eq!(
            hub.actor.peer_failure(slot(2)),
            Some(ActorFailure::Requested)
        );
        assert_eq!(
            hub.actor.try_submit(slot(2), key(2, 1, true)),
            Err(PeerRefusal::Slot(SlotFailure::UnknownSlot))
        );
        assert!(hub.recorded().is_empty(), "the leaver released nothing");
        assert_eq!(hub.gate.floor().snapshot(), owned);
        assert!(hub.gate.admits_injection());
        assert_eq!(hub.actor.active_display(slot(1)), Some(DisplayId(1)));
        hub.submit(slot(1), key(2, 2, false));
        assert_eq!(hub.next_action().0, pressed(false));
        // Its Left taken, the slot takes a new peer.
        hub.join(slot(2));
        assert_eq!(hub.actor.peer_failure(slot(2)), None);
        assert!(hub.wakes.load(Ordering::Acquire) > 0);
        assert_eq!(actions(&hub.finish()), [ReleaseAll]);
    }

    #[test]
    fn the_destination_is_ticked_while_no_input_arrives() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        let before = hub.ticks.load(Ordering::Acquire);
        wait_until(|| hub.ticks.load(Ordering::Acquire) >= before + 5);
        hub.finish();
    }

    #[test]
    fn take_back_trigger_is_delivered_to_the_owner_only() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        hub.join(slot(2));
        hub.take_floor(slot(2));
        take_back_locally(&hub.gate);
        let mut seen =
            hub.events_until(slot(2), SlotEvent::Response(frame(2, 1, Message::TakeBack)));
        assert_eq!(actions(&hub.recorded()), [EndGestures, ReleaseAll]);
        assert_eq!(hub.gate.take_triggered(), None, "taken exactly once");
        let floor = hub.gate.floor().snapshot();
        assert_eq!((floor.state, floor.peer), (FloorState::Yielding, slot(2)));
        thread::sleep(TICK * 4);
        seen.extend(hub.drain());
        // Slot 1 heard nothing but its own heartbeat.
        assert!(seen.iter().all(|(from, event)| *from == slot(2)
            || matches!(event, SlotEvent::Response(frame) if matches!(frame.message, Message::Ping(_)))));
        assert_eq!(actions(&hub.finish()), [ReleaseAll]);
    }

    #[test]
    fn an_environment_failure_stops_every_slot() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        hub.join(slot(2));
        let owned = hub.take_floor(slot(1));
        hub.environment_fails.store(true, Ordering::Release);
        wait_until(|| hub.actor.failure().is_some());
        assert_eq!(
            hub.actor.try_response(),
            Err(ActorFailure::LocalDisplaysChanged)
        );
        // One direct release while the owner still held the floor, then the floor is freed.
        assert_eq!(hub.finish(), [(ReleaseAll, owned)]);
        for number in [1, 2] {
            assert_eq!(
                hub.actor.peer_failure(slot(number)),
                Some(ActorFailure::LocalDisplaysChanged)
            );
            assert!(!hub.actor.is_started(slot(number)));
        }
        assert_eq!(hub.gate.floor().snapshot().state, FloorState::Free);
        assert!(!hub.gate.admits_injection());
    }

    #[test]
    fn removing_the_receiving_slot_releases_its_injected_input_before_freeing_the_floor() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        hub.join(slot(2));
        let owned = hub.take_floor(slot(1));
        hub.refuse_release.store(true, Ordering::Release);
        hub.actor.remove_peer(slot(1)).unwrap();
        wait_until(|| hub.actor.peer_cleanup_pending(slot(1)));
        // While its release keeps failing the leaver keeps the floor, so no one else is admitted.
        assert_eq!(hub.gate.floor().snapshot(), owned);
        assert!(!hub.gate.admits_injection());
        hub.submit(slot(2), activation(2));
        hub.events_until(
            slot(2),
            SlotEvent::Response(frame(
                2,
                0,
                Message::ActivationDeclined {
                    display_id: DisplayId(1),
                    reason: DeclineReason::Busy,
                },
            )),
        );
        hub.refuse_release.store(false, Ordering::Release);
        let seen = hub.events_until(slot(1), SlotEvent::Left);
        assert!(
            !seen
                .iter()
                .any(|(from, event)| *from == slot(1) && matches!(event, SlotEvent::Failed(_)))
        );
        assert!(!hub.actor.peer_cleanup_pending(slot(1)));
        assert_eq!(hub.gate.floor().snapshot().state, FloorState::Free);
        let released = hub.recorded();
        assert!(
            released
                .iter()
                .all(|(action, floor)| action.is_release() && *floor == owned)
        );
        assert_eq!(released.last(), Some(&(ReleaseAll, owned)));
        hub.submit(slot(2), activation(3));
        hub.events_until(
            slot(2),
            SlotEvent::Response(frame(3, 0, Message::ActivationAck(DisplayId(1)))),
        );
        hub.finish();
    }

    #[test]
    fn a_non_owner_release_all_is_answered_but_never_releases_the_owner() {
        let _test = lock_test();
        let mut hub = Hub::start();
        hub.join(slot(1));
        hub.join(slot(2));
        let owned = hub.take_floor(slot(1));
        hub.submit(slot(2), activation(2));
        hub.submit(slot(2), frame(2, 1, Message::ReleaseAll));
        hub.events_until(
            slot(2),
            SlotEvent::Response(frame(2, 1, Message::ReleaseAck)),
        );
        assert!(hub.recorded().is_empty(), "nothing reached native input");
        assert_eq!(hub.gate.floor().snapshot(), owned);
        assert!(hub.gate.admits_injection());
        hub.submit(slot(1), key(2, 2, false));
        assert_eq!(hub.next_action().0, pressed(false));
        assert_eq!(actions(&hub.finish()), [ReleaseAll]);
    }
}
