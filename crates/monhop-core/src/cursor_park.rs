//! Which pointer is live. While control is elsewhere this computer's real cursor waits (a park):
//! the thread that knows publishes it, and the renderer dims a copy of the cursor in its place and
//! rings where control lands. A source park is this computer's pointer out on a peer; a destination
//! park is this computer idle after control left it. Points are in the capture's coordinates: top-left
//! global points on macOS, physical virtual-desktop pixels on Windows.

use std::{
    sync::{
        Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::types::Point;

/// A shown park whose producer has not beaten for this long is revealed and given up on.
pub const DEADMAN: Duration = Duration::from_millis(500);
/// How far the cursor may sit from where a park shows before the park is taken as over.
pub const DRIFT_TOLERANCE: f64 = 2.0;
/// The consumer's cadence while a destination idle shows: one 60 Hz frame.
pub const IDLE_POLL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkKind {
    /// This computer's pointer is on a peer; published by the capture owner loop.
    Source,
    /// Control left this computer while it was receiving; published by the injection thread.
    Destination,
}

impl ParkKind {
    const ALL: [Self; 2] = [Self::Source, Self::Destination];

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkId {
    pub kind: ParkKind,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Park {
    /// The real cursor waits at `at`.
    Held { at: Point, epoch: u64 },
    /// Park `epoch` ended; `home` is where control landed on this computer, if it did.
    Free { epoch: u64, home: Option<Point> },
}

const UNPARKED: Park = Park::Free {
    epoch: 0,
    home: None,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotRead {
    /// Moves on every publish, so the consumer meets each edge once.
    pub seq: u64,
    pub park: Park,
    /// Since the consumer last saw this slot's producer beat.
    pub since_beat: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParkRead {
    pub source: SlotRead,
    pub destination: SlotRead,
}

impl ParkRead {
    pub fn slot(&self, kind: ParkKind) -> &SlotRead {
        match kind {
            ParkKind::Source => &self.source,
            ParkKind::Destination => &self.destination,
        }
    }
}

/// One kind's park, written by exactly one thread.
struct Slot {
    state: Mutex<(u64, Park)>,
    beat: AtomicU64,
    /// The newest epoch any tracker of this kind started, so epochs keep rising across trackers.
    epochs: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: Mutex::new((0, UNPARKED)),
            beat: AtomicU64::new(0),
            epochs: AtomicU64::new(0),
        }
    }
}

/// The parks, the switch and the app's quit, shared by the producers and the one consumer.
pub struct ParkSignal {
    slots: [Slot; 2],
    enabled: AtomicBool,
    closing: AtomicBool,
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// The consumer's last sight of each beat and when it moved.
    beats_seen: Mutex<[Option<(u64, Instant)>; 2]>,
}

/// Off until the app applies the saved switch.
pub static PARK: ParkSignal = ParkSignal::new();

impl Default for ParkSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl ParkSignal {
    pub const fn new() -> Self {
        Self {
            slots: [Slot::new(), Slot::new()],
            enabled: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            wake: OnceLock::new(),
            beats_seen: Mutex::new([None, None]),
        }
    }

    /// Registers the consumer's wake, called on every edge from a producer's thread, so it must
    /// return at once. A second registration is refused.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) -> bool {
        self.wake.set(Box::new(wake)).is_ok()
    }

    /// Both slots, for the one consumer: each read also advances its view of the beats.
    pub fn read(&self) -> ParkRead {
        let now = Instant::now();
        let mut seen = lock(&self.beats_seen);
        let mut slot = |kind: ParkKind| {
            let slot = &self.slots[kind.index()];
            let (seq, park) = *lock(&slot.state);
            let beat = slot.beat.load(Ordering::Acquire);
            let since_beat = match &mut seen[kind.index()] {
                Some((last, at)) if *last == beat => now.saturating_duration_since(*at),
                entry => {
                    *entry = Some((beat, now));
                    Duration::ZERO
                }
            };
            SlotRead {
                seq,
                park,
                since_beat,
            }
        };
        ParkRead {
            source: slot(ParkKind::Source),
            destination: slot(ParkKind::Destination),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
        self.wake();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// The app is quitting: nothing cloaks or rings from here on.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.wake();
    }

    pub fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    fn wake(&self) {
        if let Some(wake) = self.wake.get() {
            wake();
        }
    }

    /// False when `wait` is off and the consumer holds the slot; the caller retries next pass.
    fn publish(&self, kind: ParkKind, park: Park, wait: bool) -> bool {
        let state = &self.slots[kind.index()].state;
        let mut slot = if wait {
            lock(state)
        } else {
            match state.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return false,
            }
        };
        *slot = (slot.0.wrapping_add(1), park);
        drop(slot);
        self.wake();
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One per producer thread, for one kind. Level-triggered: a publish lost to a busy slot is retried
/// on the next pass. Holds no heap and never blocks outside `end`.
pub struct ParkTracker {
    signal: &'static ParkSignal,
    kind: ParkKind,
    park: Park,
    /// Whether `park` has reached the slot.
    published: bool,
}

impl ParkTracker {
    /// Epochs continue from the slot's last, whichever tracker started it.
    pub fn new(signal: &'static ParkSignal, kind: ParkKind) -> Self {
        Self {
            signal,
            kind,
            park: UNPARKED,
            published: true,
        }
    }

    /// Where the real cursor waits now, if it does; `home` is asked only as a park ends. Beats on
    /// every call and publishes only edges: a park starting, its point moving, or its end.
    pub fn observe(&mut self, held: Option<Point>, home: impl FnOnce() -> Option<Point>) {
        let slot = &self.signal.slots[self.kind.index()];
        slot.beat.fetch_add(1, Ordering::Release);
        let held = held.filter(|at| at.is_finite());
        let next = match (self.park, held) {
            (Park::Held { at, .. }, Some(now)) if at == now => None,
            (Park::Held { epoch, .. }, None) => Some(Park::Free {
                epoch,
                home: home(),
            }),
            (_, Some(at)) => Some(Park::Held {
                at,
                epoch: slot.epochs.fetch_add(1, Ordering::AcqRel) + 1,
            }),
            (Park::Free { .. }, None) => None,
        };
        if let Some(next) = next {
            self.park = next;
            self.published = false;
        }
        if !self.published {
            self.published = self.signal.publish(self.kind, self.park, false);
        }
    }

    /// Frees a held park with `home`, or lands a release still unpublished, waiting for the slot.
    /// A tracker already free publishes nothing, so a teardown never rings.
    pub fn end(&mut self, home: Option<Point>) {
        if let Park::Held { epoch, .. } = self.park {
            self.park = Park::Free {
                epoch,
                home: home.filter(|at| at.is_finite()),
            };
            self.published = false;
        }
        if !self.published {
            self.published = self.signal.publish(self.kind, self.park, true);
        }
    }
}

impl Drop for ParkTracker {
    fn drop(&mut self) {
        self.end(None);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkCommand {
    /// Put the copy up at `at` at full opacity, then hide the real cursor and dim the copy.
    Cloak { id: MarkId, at: Point },
    /// Show the real cursor, then take the copy down.
    Uncloak { id: MarkId, reveal: Reveal },
    /// Ring where control landed.
    Ring { at: Point },
}

/// Why a copy came down; the consumer logs `Deadman` and `Drift` once each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    /// The park ended or gave way, the switch went off, or the app is quitting.
    Ended,
    /// The park's producer stopped beating.
    Deadman,
    /// The cursor left the park's point without MonHop, or could not be read at all.
    Drift,
    /// Someone used this computer while it idled as a destination.
    LocalInput,
}

#[derive(Clone, Copy, Debug)]
pub struct MarkInputs {
    pub read: ParkRead,
    pub enabled: bool,
    pub closing: bool,
    /// The OS cursor now, read on every step; needed while a destination park is held or shows.
    pub cursor_at: Option<Point>,
    /// Whether `cursor_at` stays on a source park's pin; false keeps source parks out of the
    /// drift check.
    pub trust_source_cursor: bool,
    /// OS input newer than the shown destination idle's cloak; false where it cannot be read.
    pub input_since_cloak: bool,
}

/// The consumer's state machine, shared by both renderers. At most one copy shows.
#[derive(Debug, Default)]
pub struct MarkModel {
    shown: Option<(MarkId, Point)>,
    seen: [u64; 2],
    /// Per kind, the newest epoch given up on; it and every older one never cloak again.
    abandoned: [Option<u64>; 2],
}

impl MarkModel {
    /// Commands in the order the renderer runs them. Runs on the consumer thread.
    pub fn step(&mut self, inputs: MarkInputs) -> Vec<MarkCommand> {
        let mut commands = Vec::new();
        let live = inputs.enabled && !inputs.closing;
        for kind in ParkKind::ALL {
            let slot = inputs.read.slot(kind);
            if std::mem::replace(&mut self.seen[kind.index()], slot.seq) == slot.seq {
                continue;
            }
            let held = match slot.park {
                Park::Held { epoch, .. } => Some(epoch),
                Park::Free { .. } => None,
            };
            if let Some((id, _)) = self.shown
                && id.kind == kind
                && held != Some(id.epoch)
            {
                self.uncloak(Reveal::Ended, false, &mut commands);
            }
            if let Park::Free { home: Some(at), .. } = slot.park
                && live
            {
                commands.push(MarkCommand::Ring { at });
            }
        }
        if let Some((id, at)) = self.shown
            && let Err((reveal, abandon)) = self.keeps(id, at, &inputs)
        {
            self.uncloak(reveal, abandon, &mut commands);
        }
        if self.shown.is_none() && live {
            self.cloak(&inputs, &mut commands);
        }
        commands
    }

    /// Sets the consumer's poll cadence.
    pub fn showing(&self) -> Option<ParkKind> {
        self.shown.map(|(id, _)| id.kind)
    }

    /// A destination entry with no idle open, from the arrival sink.
    pub fn arrival(&self, enabled: bool, closing: bool, at: Point) -> Option<MarkCommand> {
        (enabled && !closing).then_some(MarkCommand::Ring { at })
    }

    /// Whether the shown copy stays; else why it comes down and whether its epoch is given up.
    fn keeps(&self, id: MarkId, at: Point, inputs: &MarkInputs) -> Result<(), (Reveal, bool)> {
        if !inputs.enabled || inputs.closing {
            return Err((Reveal::Ended, false));
        }
        let slot = inputs.read.slot(id.kind);
        if !matches!(slot.park, Park::Held { epoch, .. } if epoch == id.epoch) {
            return Err((Reveal::Ended, false));
        }
        if slot.since_beat > DEADMAN {
            return Err((Reveal::Deadman, true));
        }
        match id.kind {
            ParkKind::Source => {
                if inputs.trust_source_cursor && inputs.cursor_at.is_some_and(|now| !near(now, at))
                {
                    return Err((Reveal::Drift, true));
                }
            }
            ParkKind::Destination => {
                if self.cloakable(ParkKind::Source, &inputs.read).is_some() {
                    return Err((Reveal::Ended, true));
                }
                match inputs.cursor_at {
                    None => return Err((Reveal::Drift, true)),
                    Some(now) if !near(now, at) || inputs.input_since_cloak => {
                        return Err((Reveal::LocalInput, true));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// A source park comes first; a destination idle shows only with the cursor still where
    /// control left it.
    fn cloak(&mut self, inputs: &MarkInputs, commands: &mut Vec<MarkCommand>) {
        let (kind, epoch, pin) =
            if let Some((epoch, pin)) = self.cloakable(ParkKind::Source, &inputs.read) {
                (ParkKind::Source, epoch, pin)
            } else if let Some((epoch, pin)) = self.cloakable(ParkKind::Destination, &inputs.read) {
                (ParkKind::Destination, epoch, pin)
            } else {
                return;
            };
        let id = MarkId { kind, epoch };
        let at = match (kind, inputs.cursor_at) {
            (ParkKind::Source, Some(now)) if inputs.trust_source_cursor && !near(now, pin) => None,
            (ParkKind::Source, _) => Some(pin),
            (ParkKind::Destination, Some(now)) if near(now, pin) => Some(now),
            (ParkKind::Destination, _) => None,
        };
        match at {
            Some(at) => {
                self.shown = Some((id, at));
                commands.push(MarkCommand::Cloak { id, at });
            }
            None => self.abandon(id),
        }
    }

    /// The kind's held epoch and point when it may cloak: not given up on, its producer beating.
    fn cloakable(&self, kind: ParkKind, read: &ParkRead) -> Option<(u64, Point)> {
        let slot = read.slot(kind);
        match slot.park {
            Park::Held { at, epoch }
                if slot.since_beat <= DEADMAN
                    && self.abandoned[kind.index()].is_none_or(|given_up| epoch > given_up) =>
            {
                Some((epoch, at))
            }
            _ => None,
        }
    }

    fn uncloak(&mut self, reveal: Reveal, abandon: bool, commands: &mut Vec<MarkCommand>) {
        if let Some((id, _)) = self.shown.take() {
            commands.push(MarkCommand::Uncloak { id, reveal });
            if abandon {
                self.abandon(id);
            }
        }
    }

    fn abandon(&mut self, id: MarkId) {
        let given_up = &mut self.abandoned[id.kind.index()];
        *given_up = Some(given_up.map_or(id.epoch, |older| older.max(id.epoch)));
    }
}

/// False for any non-finite reading, so doubt never counts as on the point.
fn near(a: Point, b: Point) -> bool {
    (a.x - b.x).hypot(a.y - b.y) <= DRIFT_TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicUsize};

    fn signal() -> &'static ParkSignal {
        Box::leak(Box::new(ParkSignal::new()))
    }

    fn point(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn state(signal: &ParkSignal, kind: ParkKind) -> (u64, Park) {
        *lock(&signal.slots[kind.index()].state)
    }

    fn beat(signal: &ParkSignal, kind: ParkKind) -> u64 {
        signal.slots[kind.index()].beat.load(Ordering::Acquire)
    }

    fn counted_wakes(signal: &'static ParkSignal) -> Arc<AtomicUsize> {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        assert!(signal.set_wake(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(!signal.set_wake(|| {}), "the wake registers once");
        wakes
    }

    #[test]
    fn a_park_publishes_once_and_renewals_publish_nothing() {
        let signal = signal();
        let wakes = counted_wakes(signal);
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        tracker.observe(None, || panic!("home is asked only as a park ends"));
        assert_eq!(state(signal, ParkKind::Source), (0, UNPARKED));
        let pin = point(10.0, 20.0);
        for _ in 0..5 {
            tracker.observe(Some(pin), || unreachable!());
        }
        assert_eq!(
            state(signal, ParkKind::Source),
            (1, Park::Held { at: pin, epoch: 1 })
        );
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "only the edge wakes");
        assert_eq!(beat(signal, ParkKind::Source), 6, "every pass beats");
    }

    #[test]
    fn a_release_carries_home_and_the_next_park_a_new_epoch() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        tracker.observe(Some(point(1.0, 1.0)), || None);
        tracker.observe(None, || Some(point(5.0, 6.0)));
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                2,
                Park::Free {
                    epoch: 1,
                    home: Some(point(5.0, 6.0))
                }
            )
        );
        tracker.observe(None, || panic!("asked once"));
        tracker.observe(Some(point(1.0, 1.0)), || None);
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                3,
                Park::Held {
                    at: point(1.0, 1.0),
                    epoch: 2
                }
            )
        );
    }

    #[test]
    fn a_moved_pin_starts_a_new_epoch_and_a_non_finite_one_is_no_park() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        tracker.observe(Some(point(f64::NAN, 1.0)), || None);
        assert_eq!(state(signal, ParkKind::Source), (0, UNPARKED));
        tracker.observe(Some(point(1.0, 1.0)), || None);
        tracker.observe(Some(point(2.0, 1.0)), || panic!("not a release"));
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                2,
                Park::Held {
                    at: point(2.0, 1.0),
                    epoch: 2
                }
            )
        );
    }

    #[test]
    fn end_frees_a_held_park_once_and_a_free_tracker_publishes_nothing() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Destination);
        tracker.end(Some(point(9.0, 9.0)));
        assert_eq!(
            state(signal, ParkKind::Destination),
            (0, UNPARKED),
            "no teardown ring"
        );
        tracker.observe(Some(point(3.0, 4.0)), || None);
        tracker.end(Some(point(7.0, 8.0)));
        tracker.end(None);
        assert_eq!(
            state(signal, ParkKind::Destination),
            (
                2,
                Park::Free {
                    epoch: 1,
                    home: Some(point(7.0, 8.0))
                }
            )
        );
        drop(tracker);
        assert_eq!(state(signal, ParkKind::Destination).0, 2);
    }

    #[test]
    fn dropping_a_held_tracker_frees_its_park_without_home() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        tracker.observe(Some(point(3.0, 4.0)), || None);
        drop(tracker);
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                2,
                Park::Free {
                    epoch: 1,
                    home: None
                }
            )
        );
    }

    #[test]
    fn a_busy_slot_publishes_on_the_next_observe_and_end_waits_for_it() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        let held = lock(&signal.slots[0].state);
        tracker.observe(Some(point(1.0, 2.0)), || None);
        drop(held);
        assert_eq!(state(signal, ParkKind::Source), (0, UNPARKED));
        tracker.observe(Some(point(1.0, 2.0)), || None);
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                1,
                Park::Held {
                    at: point(1.0, 2.0),
                    epoch: 1
                }
            )
        );

        let held = lock(&signal.slots[0].state);
        tracker.observe(None, || Some(point(4.0, 4.0)));
        drop(held);
        // The release lost to the busy slot lands with its own home, not the teardown's.
        tracker.end(None);
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                2,
                Park::Free {
                    epoch: 1,
                    home: Some(point(4.0, 4.0))
                }
            )
        );
    }

    #[test]
    fn epochs_keep_rising_across_trackers_of_one_kind() {
        let signal = signal();
        let mut first = ParkTracker::new(signal, ParkKind::Source);
        first.observe(Some(point(1.0, 1.0)), || None);
        drop(first);
        let mut second = ParkTracker::new(signal, ParkKind::Source);
        second.observe(Some(point(1.0, 1.0)), || None);
        assert_eq!(
            state(signal, ParkKind::Source).1,
            Park::Held {
                at: point(1.0, 1.0),
                epoch: 2
            }
        );
    }

    #[test]
    fn the_two_kinds_never_touch_each_others_slot_or_beat() {
        let signal = signal();
        let mut source = ParkTracker::new(signal, ParkKind::Source);
        let mut destination = ParkTracker::new(signal, ParkKind::Destination);
        source.observe(Some(point(1.0, 1.0)), || None);
        source.observe(Some(point(1.0, 1.0)), || None);
        assert_eq!(state(signal, ParkKind::Destination), (0, UNPARKED));
        assert_eq!(beat(signal, ParkKind::Destination), 0);
        destination.observe(Some(point(8.0, 8.0)), || None);
        destination.observe(None, || Some(point(9.0, 9.0)));
        assert_eq!(
            state(signal, ParkKind::Source),
            (
                1,
                Park::Held {
                    at: point(1.0, 1.0),
                    epoch: 1
                }
            )
        );
        assert_eq!(beat(signal, ParkKind::Source), 2);
        assert_eq!(beat(signal, ParkKind::Destination), 2);
        assert_eq!(
            state(signal, ParkKind::Destination).1,
            Park::Free {
                epoch: 1,
                home: Some(point(9.0, 9.0))
            },
            "each kind numbers its own epochs"
        );
    }

    #[test]
    fn since_beat_grows_without_observe_and_resets_on_a_beat() {
        let signal = signal();
        let mut tracker = ParkTracker::new(signal, ParkKind::Destination);
        tracker.observe(Some(point(1.0, 1.0)), || None);
        assert_eq!(signal.read().destination.since_beat, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        let read = signal.read();
        assert!(read.destination.since_beat >= Duration::from_millis(20));
        assert!(read.source.since_beat >= Duration::from_millis(20));
        tracker.observe(Some(point(1.0, 1.0)), || None);
        let read = signal.read();
        assert_eq!(read.destination.since_beat, Duration::ZERO);
        assert!(read.source.since_beat >= Duration::from_millis(20));
    }

    #[test]
    fn the_switch_and_quit_store_then_wake() {
        let signal = signal();
        let wakes = counted_wakes(signal);
        assert!(!signal.enabled() && !signal.closing());
        signal.set_enabled(true);
        assert!(signal.enabled());
        signal.close();
        assert!(signal.closing());
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    // ---------- the model ----------

    const PIN: Point = Point::new(100.0, 200.0);

    fn held(seq: u64, epoch: u64) -> SlotRead {
        SlotRead {
            seq,
            park: Park::Held { at: PIN, epoch },
            since_beat: Duration::ZERO,
        }
    }

    fn free(seq: u64, epoch: u64, home: Option<Point>) -> SlotRead {
        SlotRead {
            seq,
            park: Park::Free { epoch, home },
            since_beat: Duration::ZERO,
        }
    }

    fn idle() -> SlotRead {
        free(0, 0, None)
    }

    fn inputs(source: SlotRead, destination: SlotRead) -> MarkInputs {
        MarkInputs {
            read: ParkRead {
                source,
                destination,
            },
            enabled: true,
            closing: false,
            cursor_at: Some(PIN),
            trust_source_cursor: false,
            input_since_cloak: false,
        }
    }

    fn source(epoch: u64) -> MarkId {
        MarkId {
            kind: ParkKind::Source,
            epoch,
        }
    }

    fn destination(epoch: u64) -> MarkId {
        MarkId {
            kind: ParkKind::Destination,
            epoch,
        }
    }

    fn cloak(id: MarkId, at: Point) -> MarkCommand {
        MarkCommand::Cloak { id, at }
    }

    fn uncloak(id: MarkId, reveal: Reveal) -> MarkCommand {
        MarkCommand::Uncloak { id, reveal }
    }

    fn ring(at: Point) -> MarkCommand {
        MarkCommand::Ring { at }
    }

    /// Steps `inputs` once with `kind`'s slot as given and the other slot idle.
    fn only(kind: ParkKind, slot: SlotRead) -> MarkInputs {
        match kind {
            ParkKind::Source => inputs(slot, idle()),
            ParkKind::Destination => inputs(idle(), slot),
        }
    }

    fn id(kind: ParkKind, epoch: u64) -> MarkId {
        MarkId { kind, epoch }
    }

    #[test]
    fn a_park_cloaks_once_and_its_release_uncloaks_then_rings_home() {
        for kind in ParkKind::ALL {
            let mut model = MarkModel::default();
            assert_eq!(model.step(only(kind, idle())), []);
            assert_eq!(
                model.step(only(kind, held(1, 1))),
                [cloak(id(kind, 1), PIN)]
            );
            assert_eq!(model.showing(), Some(kind));
            assert_eq!(model.step(only(kind, held(1, 1))), [], "{kind:?}: steady");
            let home = point(5.0, 5.0);
            assert_eq!(
                model.step(only(kind, free(2, 1, Some(home)))),
                [uncloak(id(kind, 1), Reveal::Ended), ring(home)],
                "{kind:?}"
            );
            assert_eq!(model.showing(), None);
            assert_eq!(model.step(only(kind, free(2, 1, Some(home)))), []);
        }
    }

    #[test]
    fn a_release_without_home_uncloaks_and_never_rings() {
        for kind in ParkKind::ALL {
            let mut model = MarkModel::default();
            model.step(only(kind, held(1, 1)));
            assert_eq!(
                model.step(only(kind, free(2, 1, None))),
                [uncloak(id(kind, 1), Reveal::Ended)]
            );
        }
    }

    #[test]
    fn a_missed_release_gives_way_to_the_new_park_without_a_ring() {
        let mut model = MarkModel::default();
        model.step(only(ParkKind::Source, held(1, 1)));
        assert_eq!(
            model.step(only(ParkKind::Source, held(3, 2))),
            [uncloak(source(1), Reveal::Ended), cloak(source(2), PIN)]
        );
    }

    #[test]
    fn a_destination_idle_needs_the_cursor_where_control_left_it() {
        let mut model = MarkModel::default();
        let mut near_pin = only(ParkKind::Destination, held(1, 1));
        near_pin.cursor_at = Some(point(PIN.x + 1.0, PIN.y + 1.0));
        assert_eq!(
            model.step(near_pin),
            [cloak(destination(1), point(PIN.x + 1.0, PIN.y + 1.0))],
            "the copy goes where the cursor is"
        );

        let mut model = MarkModel::default();
        let mut moved = only(ParkKind::Destination, held(1, 1));
        moved.cursor_at = Some(point(PIN.x + 3.0, PIN.y));
        assert_eq!(model.step(moved), []);
        assert_eq!(
            model.step(only(ParkKind::Destination, held(1, 1))),
            [],
            "abandoned for good"
        );

        let mut model = MarkModel::default();
        let mut unread = only(ParkKind::Destination, held(1, 1));
        unread.cursor_at = None;
        assert_eq!(model.step(unread), []);
        assert_eq!(model.step(only(ParkKind::Destination, held(1, 1))), []);
    }

    #[test]
    fn local_input_ends_a_destination_idle_without_a_ring_and_it_never_returns() {
        let mut moved = MarkModel::default();
        moved.step(only(ParkKind::Destination, held(1, 1)));
        let mut away = only(ParkKind::Destination, held(1, 1));
        away.cursor_at = Some(point(PIN.x, PIN.y + 2.5));
        assert_eq!(
            moved.step(away),
            [uncloak(destination(1), Reveal::LocalInput)]
        );
        assert_eq!(moved.step(only(ParkKind::Destination, held(1, 1))), []);

        let mut typed = MarkModel::default();
        typed.step(only(ParkKind::Destination, held(1, 1)));
        let mut key = only(ParkKind::Destination, held(1, 1));
        key.input_since_cloak = true;
        assert_eq!(
            typed.step(key),
            [uncloak(destination(1), Reveal::LocalInput)]
        );

        let mut unread = MarkModel::default();
        unread.step(only(ParkKind::Destination, held(1, 1)));
        let mut lost = only(ParkKind::Destination, held(1, 1));
        lost.cursor_at = None;
        assert_eq!(unread.step(lost), [uncloak(destination(1), Reveal::Drift)]);
    }

    #[test]
    fn an_abandoned_destination_idle_still_rings_when_control_lands() {
        let mut model = MarkModel::default();
        model.step(only(ParkKind::Destination, held(1, 1)));
        let mut away = only(ParkKind::Destination, held(1, 1));
        away.cursor_at = Some(point(0.0, 0.0));
        model.step(away);
        let entry = point(40.0, 50.0);
        assert_eq!(
            model.step(only(ParkKind::Destination, free(2, 1, Some(entry)))),
            [ring(entry)]
        );
    }

    #[test]
    fn a_source_park_takes_over_a_destination_idle() {
        let mut model = MarkModel::default();
        model.step(only(ParkKind::Destination, held(1, 1)));
        assert_eq!(
            model.step(inputs(held(1, 1), held(1, 1))),
            [
                uncloak(destination(1), Reveal::Ended),
                cloak(source(1), PIN)
            ]
        );
        // Once the source park ends, the given-up idle never comes back.
        assert_eq!(
            model.step(inputs(free(2, 1, None), held(1, 1))),
            [uncloak(source(1), Reveal::Ended)]
        );
    }

    #[test]
    fn a_destination_idle_waits_while_a_source_park_shows() {
        let mut model = MarkModel::default();
        model.step(only(ParkKind::Source, held(1, 1)));
        assert_eq!(model.step(inputs(held(1, 1), held(1, 4))), []);
        assert_eq!(model.showing(), Some(ParkKind::Source));
        assert_eq!(
            model.step(inputs(free(2, 1, None), held(1, 4))),
            [
                uncloak(source(1), Reveal::Ended),
                cloak(destination(4), PIN)
            ]
        );
    }

    #[test]
    fn the_switch_off_reveals_without_a_ring_and_on_cloaks_again() {
        for kind in ParkKind::ALL {
            let mut model = MarkModel::default();
            model.step(only(kind, held(1, 1)));
            let mut off = only(kind, held(1, 1));
            off.enabled = false;
            assert_eq!(model.step(off), [uncloak(id(kind, 1), Reveal::Ended)]);
            let mut off = only(kind, free(2, 1, Some(PIN)));
            off.enabled = false;
            assert_eq!(model.step(off), [], "{kind:?}: no ring while off");
            assert_eq!(
                model.step(only(kind, held(3, 2))),
                [cloak(id(kind, 2), PIN)]
            );

            let mut model = MarkModel::default();
            let mut off = only(kind, held(1, 1));
            off.enabled = false;
            assert_eq!(model.step(off), []);
            assert_eq!(
                model.step(only(kind, held(1, 1))),
                [cloak(id(kind, 1), PIN)],
                "{kind:?}: on while held"
            );
        }
    }

    #[test]
    fn a_silent_producer_reveals_and_gives_up_its_park() {
        for kind in ParkKind::ALL {
            let mut model = MarkModel::default();
            model.step(only(kind, held(1, 1)));
            let mut late = held(1, 1);
            late.since_beat = DEADMAN;
            assert_eq!(model.step(only(kind, late)), [], "{kind:?}: at the limit");
            late.since_beat = DEADMAN + Duration::from_millis(1);
            assert_eq!(
                model.step(only(kind, late)),
                [uncloak(id(kind, 1), Reveal::Deadman)]
            );
            assert_eq!(model.step(only(kind, held(1, 1))), []);

            let mut model = MarkModel::default();
            assert_eq!(model.step(only(kind, late)), [], "{kind:?}: never starts");
        }
    }

    #[test]
    fn a_trusted_cursor_off_the_pin_ends_a_source_park() {
        let mut model = MarkModel::default();
        model.step(only(ParkKind::Source, held(1, 1)));
        let mut away = only(ParkKind::Source, held(1, 1));
        away.cursor_at = Some(point(PIN.x + 3.0, PIN.y));
        assert_eq!(model.step(away), [], "untrusted reads are ignored");
        let mut away = only(ParkKind::Source, held(1, 1));
        away.cursor_at = Some(point(PIN.x + 3.0, PIN.y));
        away.trust_source_cursor = true;
        assert_eq!(model.step(away), [uncloak(source(1), Reveal::Drift)]);
        assert_eq!(model.step(only(ParkKind::Source, held(1, 1))), []);

        let mut model = MarkModel::default();
        let mut away = only(ParkKind::Source, held(1, 1));
        away.cursor_at = Some(point(f64::NAN, PIN.y));
        away.trust_source_cursor = true;
        assert_eq!(model.step(away), [], "doubt skips the copy");
    }

    #[test]
    fn closing_reveals_and_nothing_cloaks_or_rings_after() {
        for kind in ParkKind::ALL {
            let mut model = MarkModel::default();
            model.step(only(kind, held(1, 1)));
            let mut closing = only(kind, held(1, 1));
            closing.closing = true;
            assert_eq!(model.step(closing), [uncloak(id(kind, 1), Reveal::Ended)]);
            let mut closing = only(kind, free(2, 1, Some(PIN)));
            closing.closing = true;
            assert_eq!(model.step(closing), []);
            let mut closing = only(kind, held(3, 2));
            closing.closing = true;
            assert_eq!(model.step(closing), []);
            assert_eq!(model.arrival(true, true, PIN), None);
        }
    }

    #[test]
    fn an_arrival_rings_only_while_on() {
        let model = MarkModel::default();
        assert_eq!(model.arrival(true, false, PIN), Some(ring(PIN)));
        assert_eq!(model.arrival(false, false, PIN), None);
        assert_eq!(model.arrival(true, true, PIN), None);
    }

    #[test]
    fn the_model_follows_the_signal_end_to_end() {
        let signal = signal();
        signal.set_enabled(true);
        let mut model = MarkModel::default();
        let mut tracker = ParkTracker::new(signal, ParkKind::Source);
        let step = |model: &mut MarkModel| {
            model.step(MarkInputs {
                read: signal.read(),
                enabled: signal.enabled(),
                closing: signal.closing(),
                cursor_at: None,
                trust_source_cursor: false,
                input_since_cloak: false,
            })
        };
        assert_eq!(step(&mut model), []);
        tracker.observe(Some(PIN), || None);
        assert_eq!(step(&mut model), [cloak(source(1), PIN)]);
        tracker.observe(None, || Some(point(1.0, 2.0)));
        assert_eq!(
            step(&mut model),
            [uncloak(source(1), Reveal::Ended), ring(point(1.0, 2.0))]
        );
    }
}
