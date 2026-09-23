//! Local input wins: physical input on a controlled computer hands it back at capture time.
//!
//! The capture callback, the injector and the coordinator share this gate. Every method is
//! lock-free and allocation-free; the waker must only signal.

use std::{
    fmt,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::floor::{FloorSnapshot, FloorState, SharedFloor};

/// Relative motion whose vector sum reaches this many units within [`TAKE_BACK_WINDOW`] takes back.
pub const TAKE_BACK_MOTION: f64 = 6.0;

/// Relative motion accumulates for this long from a window's first sample.
pub const TAKE_BACK_WINDOW: Duration = Duration::from_millis(100);

const CLOSED: u64 = 0;

/// Take-back state for one session.
///
/// SeqCst throughout: the injector counts a press in `held` then loads `admission` and the floor,
/// the callback moves the floor off Receiving then loads `held`, so at least one of them sees the
/// other (no injected press can land after a take-back that decided not to withhold). Admission
/// needs the floor still Receiving at its generation, so closing it only for the generation the
/// callback yielded never leaves a yielded generation admitted and never closes a newer one.
#[derive(Clone)]
pub struct TakeBackGate(Arc<GateInner>);

struct GateInner {
    floor: SharedFloor,
    /// The Receiving generation injection was opened for, or [`CLOSED`].
    admission: AtomicU64,
    /// Injected keys and buttons currently down; counted before the post, uncounted after the up.
    held: AtomicU32,
    /// Bumped after every note to `held`, so a read spanning an unchanged value overlapped none.
    changes: AtomicU64,
    /// The Yielding generation a take-back produced and the coordinator has not taken yet.
    triggered: AtomicU64,
    waker: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) enum TakeBackOutcome {
    NotReceiving,
    Triggered { withhold: bool },
}

impl fmt::Debug for TakeBackGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TakeBackGate")
            .field("floor", &self.0.floor)
            .field("admits_injection", &self.admits_injection())
            .field("injected_held", &self.injected_held())
            .finish()
    }
}

impl TakeBackGate {
    pub fn new(floor: SharedFloor) -> Self {
        Self(Arc::new(GateInner {
            floor,
            admission: AtomicU64::new(CLOSED),
            held: AtomicU32::new(0),
            changes: AtomicU64::new(0),
            triggered: AtomicU64::new(0),
            waker: OnceLock::new(),
        }))
    }

    pub fn floor(&self) -> &SharedFloor {
        &self.0.floor
    }

    /// Admits injection while the floor stays Receiving at `generation`.
    pub fn open_injection(&self, generation: u64) {
        self.0.admission.store(generation, Ordering::SeqCst);
    }

    /// Checked before every native post. False once take-back closed the current generation.
    pub fn admits_injection(&self) -> bool {
        let admitted = self.0.admission.load(Ordering::SeqCst);
        admitted != CLOSED
            && self.0.floor.snapshot()
                == (FloorSnapshot {
                    state: FloorState::Receiving,
                    generation: admitted,
                })
    }

    /// Call before the [`Self::admits_injection`] check that precedes any key or button down, and
    /// uncount with [`Self::note_injected_up`] if that down is not posted.
    pub fn note_injected_press(&self) {
        self.0.held.fetch_add(1, Ordering::SeqCst);
        self.note_injection_change();
    }

    /// Call after a key or button up the injector posted successfully.
    pub fn note_injected_up(&self) {
        let _ = self
            .0
            .held
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |held| {
                held.checked_sub(1)
            });
        self.note_injection_change();
    }

    /// Call after a successful native ReleaseAll.
    pub fn note_injected_released(&self) {
        self.0.held.store(0, Ordering::SeqCst);
        self.note_injection_change();
    }

    fn note_injection_change(&self) {
        self.0.changes.fetch_add(1, Ordering::SeqCst);
    }

    pub fn injected_held(&self) -> bool {
        self.0.held.load(Ordering::SeqCst) > 0
    }

    /// Injected press and release bookkeeping so far. Sampled before and after a native state read,
    /// an unchanged value with nothing held means no injected transition was counted during it.
    pub fn injection_changes(&self) -> u64 {
        self.0.changes.load(Ordering::SeqCst)
    }

    /// Registers the one callback a take-back runs; a second registration is refused.
    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.0.waker.set(waker).is_ok()
    }

    /// The Yielding floor generation a take-back produced, returned once.
    pub fn take_triggered(&self) -> Option<u64> {
        match self.0.triggered.swap(0, Ordering::SeqCst) {
            0 => None,
            generation => Some(generation),
        }
    }

    /// Callback only: Receiving -> Yielding, close admission, then report whether injected input
    /// was held at the close.
    pub(crate) fn trigger(&self) -> TakeBackOutcome {
        match self.yield_floor() {
            Some(yielded) => self.close_yielded(yielded),
            None => TakeBackOutcome::NotReceiving,
        }
    }

    /// The trigger's first step: the Receiving snapshot it left and the Yielding one it made.
    fn yield_floor(&self) -> Option<(FloorSnapshot, FloorSnapshot)> {
        let floor = &self.0.floor;
        let mut current = floor.snapshot();
        loop {
            if current.state != FloorState::Receiving {
                return None;
            }
            match floor.transition(current, FloorState::Yielding) {
                Ok(yielding) => return Some((current, yielding)),
                Err(changed) => current = changed,
            }
        }
    }

    /// The trigger's second step. A newer activation may have opened admission meanwhile: only the
    /// yielded generation's admission is closed.
    fn close_yielded(
        &self,
        (receiving, yielding): (FloorSnapshot, FloorSnapshot),
    ) -> TakeBackOutcome {
        let inner = &*self.0;
        let _ = inner.admission.compare_exchange(
            receiving.generation,
            CLOSED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        let withhold = inner.held.load(Ordering::SeqCst) > 0;
        inner.triggered.store(yielding.generation, Ordering::SeqCst);
        if let Some(wake) = inner.waker.get() {
            wake();
        }
        TakeBackOutcome::Triggered { withhold }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::floor::FloorOwner;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn trigger_closes_admission_wakes_once_and_ignores_a_late_open() {
        let gate = TakeBackGate::new(SharedFloor::new());
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        assert!(gate.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));
        assert!(!gate.set_waker(Arc::new(|| {})));
        assert!(matches!(gate.trigger(), TakeBackOutcome::NotReceiving));
        assert!(!gate.admits_injection(), "injection is closed until opened");

        let receiving = gate
            .floor()
            .transition(gate.floor().snapshot(), FloorState::Receiving)
            .unwrap();
        assert!(!gate.admits_injection());
        gate.open_injection(receiving.generation);
        assert!(gate.admits_injection());
        gate.note_injected_press();
        assert!(matches!(
            gate.trigger(),
            TakeBackOutcome::Triggered { withhold: true }
        ));
        assert!(matches!(gate.trigger(), TakeBackOutcome::NotReceiving));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(!gate.admits_injection());

        gate.open_injection(receiving.generation);
        assert!(
            !gate.admits_injection(),
            "reopening a generation that yielded admits nothing"
        );
    }

    #[test]
    fn injected_input_counts_only_what_is_still_down() {
        let gate = TakeBackGate::new(SharedFloor::new());
        gate.note_injected_press();
        gate.note_injected_press();
        gate.note_injected_up();
        assert!(gate.injected_held(), "one injected press is still down");
        gate.note_injected_up();
        assert!(
            !gate.injected_held(),
            "typed and released keys withhold nothing"
        );
        gate.note_injected_up();
        assert!(!gate.injected_held(), "an extra up never wraps the count");
        gate.note_injected_press();
        gate.note_injected_released();
        assert!(!gate.injected_held());
    }

    #[test]
    fn every_injected_transition_changes_the_injection_count() {
        let gate = TakeBackGate::new(SharedFloor::new());
        let mut last = gate.injection_changes();
        let steps: [fn(&TakeBackGate); 4] = [
            TakeBackGate::note_injected_press,
            TakeBackGate::note_injected_up,
            TakeBackGate::note_injected_up,
            TakeBackGate::note_injected_released,
        ];
        for step in steps {
            step(&gate);
            let now = gate.injection_changes();
            assert_ne!(
                now, last,
                "a read spanning this step must not look coherent"
            );
            last = now;
        }
    }

    #[test]
    fn a_delayed_take_back_never_closes_a_newer_admission() {
        let gate = TakeBackGate::new(SharedFloor::new());
        let floor = gate.floor();
        let first = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        gate.open_injection(first.generation);

        // The callback yields the first activation, then is preempted before closing admission.
        let yielded = gate.yield_floor().expect("the floor was Receiving");
        assert!(
            !gate.admits_injection(),
            "a yielded generation admits nothing"
        );
        // The receiver applies ReleaseAll, frees the floor, and opens a newer activation.
        assert!(floor.release(FloorOwner::Inbound, yielded.1.generation));
        let newer = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        gate.open_injection(newer.generation);
        assert!(gate.admits_injection());

        // The delayed callback resumes.
        assert!(matches!(
            gate.close_yielded(yielded),
            TakeBackOutcome::Triggered { withhold: false }
        ));
        assert!(
            gate.admits_injection(),
            "the newer activation keeps its admission"
        );
        assert_eq!(floor.snapshot(), newer);
        assert_eq!(
            gate.take_triggered(),
            Some(yielded.1.generation),
            "the stale trigger names only the generation it yielded"
        );
    }
}
