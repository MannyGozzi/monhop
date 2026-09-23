//! The session floor: at most one sharing direction is active per process.
//!
//! State and generation share one atomic word so the capture callback reads both lock-free and
//! every change is a single compare-and-swap on the exact snapshot it was decided from.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorState {
    Free,
    Requesting,
    Sending,
    Returning,
    Receiving,
    Yielding,
}

/// The half that may change or free a held floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorOwner {
    Outbound,
    Inbound,
}

impl FloorState {
    pub const fn owner(self) -> Option<FloorOwner> {
        match self {
            Self::Free => None,
            Self::Requesting | Self::Sending | Self::Returning => Some(FloorOwner::Outbound),
            Self::Receiving | Self::Yielding => Some(FloorOwner::Inbound),
        }
    }

    const fn code(self) -> u64 {
        match self {
            Self::Free => 0,
            Self::Requesting => 1,
            Self::Sending => 2,
            Self::Returning => 3,
            Self::Receiving => 4,
            Self::Yielding => 5,
        }
    }

    const fn from_code(code: u64) -> Self {
        match code {
            0 => Self::Free,
            1 => Self::Requesting,
            2 => Self::Sending,
            3 => Self::Returning,
            4 => Self::Receiving,
            5 => Self::Yielding,
            _ => panic!("floor words are written only by SharedFloor"),
        }
    }

    const fn allows(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Free, Self::Requesting | Self::Receiving)
                | (
                    Self::Requesting,
                    Self::Sending | Self::Free | Self::Receiving
                )
                | (Self::Sending, Self::Returning)
                | (Self::Returning, Self::Free)
                | (Self::Receiving, Self::Yielding | Self::Free)
                | (Self::Yielding, Self::Free)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloorSnapshot {
    pub state: FloorState,
    pub generation: u64,
}

const STATE_BITS: u32 = 3;
const STATE_MASK: u64 = (1 << STATE_BITS) - 1;
const MAX_GENERATION: u64 = u64::MAX >> STATE_BITS;

impl FloorSnapshot {
    const fn pack(self) -> u64 {
        (self.generation << STATE_BITS) | self.state.code()
    }

    const fn unpack(word: u64) -> Self {
        Self {
            state: FloorState::from_code(word & STATE_MASK),
            generation: word >> STATE_BITS,
        }
    }

    const fn is_representable(self) -> bool {
        self.generation != 0 && self.generation <= MAX_GENERATION
    }

    /// Generation 0 is never issued: it marks records captured without a floor.
    const fn next(self, state: FloorState) -> Self {
        let generation = if self.generation >= MAX_GENERATION {
            1
        } else {
            self.generation + 1
        };
        Self { state, generation }
    }
}

/// One process's floor, shared by both halves and the capture callback. Lock-free.
///
/// Every access is SeqCst so floor changes, injection admission and the injected-held flag in
/// [`crate::TakeBackGate`] fall in one total order.
#[derive(Clone)]
pub struct SharedFloor(Arc<AtomicU64>);

impl fmt::Debug for SharedFloor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SharedFloor")
            .field(&self.snapshot())
            .finish()
    }
}

impl Default for SharedFloor {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedFloor {
    pub fn new() -> Self {
        let free = FloorSnapshot {
            state: FloorState::Free,
            generation: 1,
        };
        Self(Arc::new(AtomicU64::new(free.pack())))
    }

    pub fn snapshot(&self) -> FloorSnapshot {
        FloorSnapshot::unpack(self.0.load(Ordering::SeqCst))
    }

    /// CAS on the exact snapshot; bumps generation. Err carries the current snapshot.
    pub fn transition(
        &self,
        from: FloorSnapshot,
        to: FloorState,
    ) -> Result<FloorSnapshot, FloorSnapshot> {
        if !from.is_representable() || !from.state.allows(to) {
            return Err(self.snapshot());
        }
        self.swap_exact(from, from.next(to))
    }

    /// Frees the floor only if `owner` owns it at `generation`.
    pub fn release(&self, owner: FloorOwner, generation: u64) -> bool {
        let current = self.snapshot();
        current.generation == generation
            && current.state.owner() == Some(owner)
            && self
                .swap_exact(current, current.next(FloorState::Free))
                .is_ok()
    }

    /// Session end: unconditionally Free, bumps generation.
    pub fn reset(&self) {
        let _ = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
                Some(FloorSnapshot::unpack(word).next(FloorState::Free).pack())
            });
    }

    fn swap_exact(
        &self,
        from: FloorSnapshot,
        to: FloorSnapshot,
    ) -> Result<FloorSnapshot, FloorSnapshot> {
        self.0
            .compare_exchange(from.pack(), to.pack(), Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| to)
            .map_err(FloorSnapshot::unpack)
    }
}
