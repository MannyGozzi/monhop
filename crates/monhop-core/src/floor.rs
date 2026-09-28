//! The session floor: at most one sharing direction, for one peer, is active per process.
//!
//! State, owning peer and generation share one atomic word so the capture callback reads them
//! lock-free and every change is a single compare-and-swap on the exact snapshot it was decided
//! from.

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

/// Group slots a floor can name: `1..=MAX_GROUP_PEERS`.
pub const MAX_GROUP_PEERS: usize = 7;

/// The peer a held floor belongs to. [`FloorPeer::NONE`] accompanies Free and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FloorPeer(u8);

impl FloorPeer {
    pub const NONE: Self = Self(0);
    /// Slot 1: the pairwise session's peer, which [`SharedFloor::transition`] names out of Free.
    pub const SOLE: Self = Self(1);

    /// Group slot `1..=MAX_GROUP_PEERS`, or None.
    pub const fn slot(slot: u8) -> Option<Self> {
        match slot {
            1..=MAX_SLOT => Some(Self(slot)),
            _ => None,
        }
    }

    /// The slot number, 0 for [`FloorPeer::NONE`].
    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloorSnapshot {
    pub state: FloorState,
    pub generation: u64,
    pub peer: FloorPeer,
}

const STATE_BITS: u32 = 3;
const PEER_BITS: u32 = 4;
const STATE_MASK: u64 = (1 << STATE_BITS) - 1;
const PEER_MASK: u64 = (1 << PEER_BITS) - 1;
const GENERATION_SHIFT: u32 = STATE_BITS + PEER_BITS;
const MAX_GENERATION: u64 = u64::MAX >> GENERATION_SHIFT;
const MAX_SLOT: u8 = MAX_GROUP_PEERS as u8;
const _: () = assert!(MAX_SLOT as u64 <= PEER_MASK);

impl FloorSnapshot {
    const fn pack(self) -> u64 {
        (self.generation << GENERATION_SHIFT)
            | ((self.peer.0 as u64) << STATE_BITS)
            | self.state.code()
    }

    const fn unpack(word: u64) -> Self {
        Self {
            state: FloorState::from_code(word & STATE_MASK),
            generation: word >> GENERATION_SHIFT,
            peer: FloorPeer(((word >> STATE_BITS) & PEER_MASK) as u8),
        }
    }

    /// Exactly the snapshots SharedFloor writes; anything else could pack onto another word.
    const fn is_well_formed(self) -> bool {
        self.generation != 0
            && self.generation <= MAX_GENERATION
            && self.peer.0 <= MAX_SLOT
            && matches!(self.state, FloorState::Free) == (self.peer.0 == FloorPeer::NONE.0)
    }

    /// Generation 0 is never issued: it marks records captured without a floor.
    const fn next(self, state: FloorState, peer: FloorPeer) -> Self {
        let generation = if self.generation >= MAX_GENERATION {
            1
        } else {
            self.generation + 1
        };
        Self {
            state,
            generation,
            peer,
        }
    }

    const fn freed(self) -> Self {
        self.next(FloorState::Free, FloorPeer::NONE)
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
            peer: FloorPeer::NONE,
        };
        Self(Arc::new(AtomicU64::new(free.pack())))
    }

    pub fn snapshot(&self) -> FloorSnapshot {
        FloorSnapshot::unpack(self.0.load(Ordering::SeqCst))
    }

    /// CAS on the exact snapshot; bumps generation. Leaving Free names [`FloorPeer::SOLE`],
    /// entering Free names none, every other edge keeps the peer. Err carries the current snapshot.
    pub fn transition(
        &self,
        from: FloorSnapshot,
        to: FloorState,
    ) -> Result<FloorSnapshot, FloorSnapshot> {
        if !from.is_well_formed() || !from.state.allows(to) {
            return Err(self.snapshot());
        }
        let peer = match (from.state, to) {
            (_, FloorState::Free) => FloorPeer::NONE,
            (FloorState::Free, _) => FloorPeer::SOLE,
            _ => from.peer,
        };
        self.swap_exact(from, from.next(to, peer))
    }

    /// Takes the floor for `peer`: Free -> Requesting | Receiving, or Requesting -> Receiving only
    /// from a request by the same peer. CAS on the exact snapshot; bumps generation.
    pub fn claim(
        &self,
        from: FloorSnapshot,
        to: FloorState,
        peer: FloorPeer,
    ) -> Result<FloorSnapshot, FloorSnapshot> {
        let allowed = match (from.state, to) {
            (FloorState::Free, FloorState::Requesting | FloorState::Receiving) => {
                peer != FloorPeer::NONE
            }
            (FloorState::Requesting, FloorState::Receiving) => from.peer == peer,
            _ => false,
        };
        if !from.is_well_formed() || !allowed {
            return Err(self.snapshot());
        }
        self.swap_exact(from, from.next(to, peer))
    }

    /// Returning for one peer -> Sending for another, so suppression never lapses between them.
    /// CAS on the exact snapshot; bumps generation.
    pub fn hand_over(
        &self,
        from: FloorSnapshot,
        to: FloorPeer,
    ) -> Result<FloorSnapshot, FloorSnapshot> {
        if !from.is_well_formed()
            || from.state != FloorState::Returning
            || to == FloorPeer::NONE
            || to == from.peer
        {
            return Err(self.snapshot());
        }
        self.swap_exact(from, from.next(FloorState::Sending, to))
    }

    /// Frees the floor only if `owner` owns it at `generation`.
    pub fn release(&self, owner: FloorOwner, generation: u64) -> bool {
        let current = self.snapshot();
        current.generation == generation
            && current.state.owner() == Some(owner)
            && self.swap_exact(current, current.freed()).is_ok()
    }

    /// Frees the floor only if `peer` holds it at `generation`, in any held state.
    pub fn release_peer(&self, peer: FloorPeer, generation: u64) -> bool {
        let current = self.snapshot();
        peer != FloorPeer::NONE
            && current.peer == peer
            && current.generation == generation
            && self.swap_exact(current, current.freed()).is_ok()
    }

    /// Session end: unconditionally Free, bumps generation.
    pub fn reset(&self) {
        let _ = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
                Some(FloorSnapshot::unpack(word).freed().pack())
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

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [FloorState; 6] = [
        FloorState::Free,
        FloorState::Requesting,
        FloorState::Sending,
        FloorState::Returning,
        FloorState::Receiving,
        FloorState::Yielding,
    ];

    fn floor_holding(snapshot: FloorSnapshot) -> SharedFloor {
        assert!(snapshot.is_well_formed());
        SharedFloor(Arc::new(AtomicU64::new(snapshot.pack())))
    }

    fn last_slot() -> FloorPeer {
        FloorPeer::slot(MAX_SLOT).expect("the last group slot")
    }

    #[test]
    fn every_field_round_trips_at_the_edges_of_its_bits() {
        assert_eq!(MAX_GENERATION, (1 << 57) - 1);
        for state in STATES {
            let peers: &[FloorPeer] = if state == FloorState::Free {
                &[FloorPeer::NONE]
            } else {
                &[FloorPeer::SOLE, last_slot()]
            };
            for &peer in peers {
                for generation in [1, 2, MAX_GENERATION - 1, MAX_GENERATION] {
                    let snapshot = FloorSnapshot {
                        state,
                        generation,
                        peer,
                    };
                    assert_eq!(FloorSnapshot::unpack(snapshot.pack()), snapshot);
                }
            }
        }
    }

    #[test]
    fn generation_wraps_past_its_57_bits_to_1_never_0() {
        let top = |state, peer| FloorSnapshot {
            state,
            generation: MAX_GENERATION,
            peer,
        };

        let sending = top(FloorState::Sending, last_slot());
        let floor = floor_holding(sending);
        let returning = floor
            .transition(sending, FloorState::Returning)
            .expect("the top generation still changes");
        assert_eq!(
            returning,
            FloorSnapshot {
                state: FloorState::Returning,
                generation: 1,
                peer: last_slot(),
            }
        );
        assert_eq!(floor.snapshot(), returning);

        let returning = top(FloorState::Returning, last_slot());
        let floor = floor_holding(returning);
        let handed = floor.hand_over(returning, FloorPeer::SOLE).unwrap();
        assert_eq!((handed.generation, handed.peer), (1, FloorPeer::SOLE));

        let free = top(FloorState::Free, FloorPeer::NONE);
        let floor = floor_holding(free);
        let claimed = floor
            .claim(free, FloorState::Receiving, last_slot())
            .unwrap();
        assert_eq!(claimed.generation, 1);

        let floor = floor_holding(top(FloorState::Receiving, last_slot()));
        assert!(floor.release(FloorOwner::Inbound, MAX_GENERATION));
        assert_eq!(floor.snapshot().generation, 1);

        let floor = floor_holding(top(FloorState::Requesting, last_slot()));
        assert!(floor.release_peer(last_slot(), MAX_GENERATION));
        assert_eq!(floor.snapshot().generation, 1);

        let floor = floor_holding(top(FloorState::Yielding, FloorPeer::SOLE));
        floor.reset();
        assert_eq!(
            floor.snapshot(),
            FloorSnapshot {
                state: FloorState::Free,
                generation: 1,
                peer: FloorPeer::NONE,
            }
        );

        // Past the budget a generation would pack onto another word; it is refused, not aliased.
        let current = top(FloorState::Sending, FloorPeer::SOLE);
        let floor = floor_holding(current);
        for generation in [0, MAX_GENERATION + 1, u64::MAX] {
            let forged = FloorSnapshot {
                generation,
                ..current
            };
            assert_eq!(
                floor.transition(forged, FloorState::Returning),
                Err(current)
            );
        }
        assert_eq!(floor.snapshot(), current);
    }
}
