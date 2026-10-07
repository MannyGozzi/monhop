//! One input receiver per peer slot, with injection admitted only for the peer that owns the floor.

use crate::{
    session_actor::OutputSequences,
    session_receiver::{
        DestinationAction, DestinationFailure, InputDestination, InputReceiver, ReceiverFailure,
    },
};
use monhop_core::{FloorOwner, FloorPeer, MAX_GROUP_PEERS, SharedFloor, TakeBackGate};
use monhop_protocol::{Frame, Message};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotFailure {
    /// Not a group slot, or no receiver runs in it.
    UnknownSlot,
    /// A receiver already runs in the slot.
    Occupied,
    Receiver(ReceiverFailure),
    /// A response sequence would wrap; the slot's receiver is stopped.
    SequenceExhausted,
}

/// A slot's response, sequenced for that slot's peer, if it produced one.
pub type SlotResult = Result<Option<Frame>, SlotFailure>;

struct Slot {
    receiver: InputReceiver,
    sequences: OutputSequences,
}

/// Every peer's receiver on one gate and floor. At most one holds the floor: only it reaches the
/// destination and only it is handed the take-back.
pub struct ReceiverSet {
    gate: TakeBackGate,
    slots: [Option<Slot>; MAX_GROUP_PEERS],
}

impl ReceiverSet {
    pub fn new(gate: TakeBackGate) -> Self {
        Self {
            gate,
            slots: std::array::from_fn(|_| None),
        }
    }

    /// Runs `receiver`, fresh from startup, for `slot` on this set's gate. `next_heartbeat_out`
    /// continues the heartbeat sequence startup used toward that peer.
    pub fn add(
        &mut self,
        slot: FloorPeer,
        receiver: InputReceiver,
        local_lower: bool,
        enabled: bool,
        next_heartbeat_out: u64,
    ) -> Result<(), SlotFailure> {
        let index = index(slot).ok_or(SlotFailure::UnknownSlot)?;
        if self.slots[index].is_some() {
            return Err(SlotFailure::Occupied);
        }
        let receiver = receiver
            .with_floor(self.gate.clone(), local_lower, enabled)
            .shared(slot);
        let sequences = OutputSequences {
            control: next_heartbeat_out,
            input: 0,
            epoch: receiver.epoch(),
        };
        self.slots[index] = Some(Slot {
            receiver,
            sequences,
        });
        Ok(())
    }

    /// Stops `slot`'s receiver and empties the slot once nothing it injected awaits release. False
    /// while that release keeps failing and the slot keeps the floor: call again, e.g. every tick.
    pub fn remove(&mut self, slot: FloorPeer, destination: &mut impl InputDestination) -> bool {
        let Some(index) = index(slot) else {
            return true;
        };
        let Some(entry) = self.slots[index].as_mut() else {
            return true;
        };
        let mut guarded = OwnerGuard {
            destination,
            floor: self.gate.floor(),
            slot,
        };
        entry
            .receiver
            .stop(ReceiverFailure::PeerStopped, &mut guarded);
        if entry.receiver.cleanup_pending() {
            return false;
        }
        self.slots[index] = None;
        true
    }

    pub fn receiver(&self, slot: FloorPeer) -> Option<&InputReceiver> {
        index(slot).and_then(|index| self.slots[index].as_ref().map(|entry| &entry.receiver))
    }

    /// Slots holding a receiver, in slot order. Borrows nothing, so each can be driven in turn.
    pub fn occupied(&self) -> impl Iterator<Item = FloorPeer> + use<> {
        let occupied: [bool; MAX_GROUP_PEERS] =
            std::array::from_fn(|index| self.slots[index].is_some());
        (1..=MAX_GROUP_PEERS)
            .filter(move |number| occupied[number - 1])
            .filter_map(|number| u8::try_from(number).ok().and_then(FloorPeer::slot))
    }

    pub fn receive(
        &mut self,
        slot: FloorPeer,
        frame: &Frame,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> SlotResult {
        let (entry, mut guarded) = self.slot(slot, destination)?;
        let response = entry
            .receiver
            .receive(frame, now, &mut guarded)
            .map_err(SlotFailure::Receiver)?;
        entry.respond(response, &mut guarded)
    }

    pub fn tick(
        &mut self,
        slot: FloorPeer,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> SlotResult {
        let (entry, mut guarded) = self.slot(slot, destination)?;
        let response = entry
            .receiver
            .tick(now, &mut guarded)
            .map_err(SlotFailure::Receiver)?;
        entry.respond(response, &mut guarded)
    }

    pub fn flush(
        &mut self,
        slot: FloorPeer,
        destination: &mut impl InputDestination,
    ) -> Result<(), SlotFailure> {
        let (entry, mut guarded) = self.slot(slot, destination)?;
        entry
            .receiver
            .flush(&mut guarded)
            .map_err(SlotFailure::Receiver)
    }

    /// Takes the gate's trigger exactly once and hands it only to the slot the floor names. A
    /// trigger that slot no longer holds is dropped, as a pairwise receiver drops a stale one; a
    /// stopped owner's pending cleanup, retried by its tick, releases and frees the floor instead.
    pub fn take_back(
        &mut self,
        destination: &mut impl InputDestination,
    ) -> Option<(FloorPeer, SlotResult)> {
        let generation = self.gate.take_triggered()?;
        let owner = self.gate.floor().snapshot().peer;
        let (entry, mut guarded) = self.slot(owner, destination).ok()?;
        if let Some(failure) = entry.receiver.failure() {
            return Some((owner, Err(SlotFailure::Receiver(failure))));
        }
        let result = match entry.receiver.yield_to_local(generation, &mut guarded) {
            Ok(response) => entry.respond(response, &mut guarded),
            Err(failure) => Err(SlotFailure::Receiver(failure)),
        };
        Some((owner, result))
    }

    fn slot<'a, D: InputDestination>(
        &'a mut self,
        slot: FloorPeer,
        destination: &'a mut D,
    ) -> Result<(&'a mut Slot, OwnerGuard<'a, D>), SlotFailure> {
        let Self { gate, slots } = self;
        let entry = index(slot)
            .and_then(|index| slots[index].as_mut())
            .ok_or(SlotFailure::UnknownSlot)?;
        let guarded = OwnerGuard {
            destination,
            floor: gate.floor(),
            slot,
        };
        Ok((entry, guarded))
    }
}

impl Slot {
    /// Sequenced as the pairwise actor sequences replies.
    fn respond(
        &mut self,
        response: Option<Message>,
        destination: &mut impl InputDestination,
    ) -> SlotResult {
        let Some(message) = response else {
            return Ok(None);
        };
        let Some(frame) = self.sequences.frame(message, &self.receiver) else {
            self.receiver
                .stop(ReceiverFailure::PeerStopped, destination);
            return Err(SlotFailure::SequenceExhausted);
        };
        Ok(Some(frame))
    }
}

/// The destination as one slot's receiver reaches it: while the floor names another slot inbound,
/// nothing reaches native input, releases included, so another peer's barrier or failure never
/// lifts the owner's held keys or drag. With no inbound owner releases pass, so input a freed
/// claim left down still comes up. Dropped posts succeed, as refused native admission does.
struct OwnerGuard<'a, D> {
    destination: &'a mut D,
    floor: &'a SharedFloor,
    slot: FloorPeer,
}

impl<D: InputDestination> InputDestination for OwnerGuard<'_, D> {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let floor = self.floor.snapshot();
        let inbound = floor.state.owner() == Some(FloorOwner::Inbound);
        let admitted = if inbound {
            floor.peer == self.slot
        } else {
            action.is_release()
        };
        if !admitted {
            return Ok(());
        }
        self.destination.apply(action)
    }
}

/// Slot 1 is index 0; [`FloorPeer::NONE`] has none.
pub(crate) fn index(slot: FloorPeer) -> Option<usize> {
    usize::from(slot.get())
        .checked_sub(1)
        .filter(|&index| index < MAX_GROUP_PEERS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_health::PEER_LIVENESS;
    use DestinationAction::{EndGestures, MoveTo, ReleaseAll};
    use monhop_core::{DisplayId, FloorState, HidUsage, ModifierState, Point};
    use monhop_protocol::{DeclineReason, DisplayDescription, DisplayTopology, Key, SessionEpoch};

    #[derive(Default)]
    struct Destination {
        calls: Vec<DestinationAction>,
        fail_release: bool,
    }

    impl InputDestination for Destination {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            self.calls.push(action);
            if action == ReleaseAll && self.fail_release {
                return Err(DestinationFailure);
            }
            Ok(())
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

    fn slot(number: u8) -> FloorPeer {
        FloorPeer::slot(number).unwrap()
    }

    fn receiver() -> InputReceiver {
        InputReceiver::new(displays(), SessionEpoch::new(1).unwrap(), Duration::ZERO)
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

    /// Epoch, sequence and message of a slot's response.
    fn sent(result: SlotResult) -> (u64, u64, Message) {
        let frame = result.unwrap().expect("a response");
        (frame.epoch.get(), frame.sequence, frame.message)
    }

    /// A set on a fresh floor with a receiver in each of `slots`, heartbeats continuing from 3.
    fn set_with(slots: &[u8]) -> (ReceiverSet, TakeBackGate) {
        let gate = TakeBackGate::new(SharedFloor::new());
        let mut set = ReceiverSet::new(gate.clone());
        for &number in slots {
            set.add(slot(number), receiver(), false, true, 3).unwrap();
        }
        (set, gate)
    }

    fn take_back_locally(gate: &TakeBackGate) {
        use monhop_core::capture::{CaptureEvent, CaptureStop, capture_channel};
        let stop = CaptureStop::new();
        let (mut producer, _consumer) = capture_channel(stop.clone());
        let mut physical = monhop_core::capture_physical::PhysicalCapture::new(Duration::ZERO)
            .with_take_back(gate.clone());
        physical.process(
            CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 },
            false,
            Duration::ZERO,
            &mut producer,
            &stop,
        );
    }

    #[test]
    fn take_back_reaches_only_the_receiving_peer() {
        let (mut set, gate) = set_with(&[1, 2]);
        let mut target = Destination::default();
        let at = Duration::ZERO;
        assert_eq!(
            sent(set.receive(slot(2), &activation(2), at, &mut target)),
            (2, 0, Message::ActivationAck(DisplayId(1)))
        );
        assert_eq!(
            set.receive(slot(2), &key(2, 1, true), at, &mut target),
            Ok(None)
        );
        // A shared receiver never takes the trigger itself, so none can swallow the owner's.
        let mut bystander = receiver()
            .with_floor(gate.clone(), false, true)
            .shared(slot(3));
        take_back_locally(&gate);
        assert_eq!(gate.floor().snapshot().state, FloorState::Yielding);
        assert_eq!(bystander.take_back(&mut Destination::default()), Ok(None));

        let (owner, taken) = set.take_back(&mut target).expect("the owner takes it");
        assert_eq!(owner, slot(2));
        assert_eq!(sent(taken), (2, 1, Message::TakeBack));
        assert!(set.take_back(&mut target).is_none(), "taken exactly once");
        assert_eq!(gate.take_triggered(), None);
        assert_eq!(
            target.calls,
            [
                MoveTo(Point::new(10.0, 10.0)),
                pressed(true),
                EndGestures,
                ReleaseAll
            ]
        );
        // The other slot answers only its own heartbeat, on its own sequence.
        assert_eq!(
            sent(set.tick(slot(1), at, &mut target)),
            (1, 3, Message::Ping(1))
        );
        assert_eq!(target.calls.len(), 4);
    }

    #[test]
    fn a_second_peers_activation_is_declined_busy_while_the_first_is_active() {
        let (mut set, gate) = set_with(&[1, 2]);
        let mut target = Destination::default();
        let at = Duration::ZERO;
        assert_eq!(
            sent(set.receive(slot(1), &activation(2), at, &mut target)),
            (2, 0, Message::ActivationAck(DisplayId(1)))
        );
        let first = gate.floor().snapshot();
        assert_eq!((first.state, first.peer), (FloorState::Receiving, slot(1)));
        assert_eq!(
            sent(set.receive(slot(2), &activation(2), at, &mut target)),
            (
                2,
                0,
                Message::ActivationDeclined {
                    display_id: DisplayId(1),
                    reason: DeclineReason::Busy,
                }
            )
        );
        assert!(!set.receiver(slot(2)).unwrap().is_active());
        // The declined peer's barrier is acknowledged and releases nothing of the first's.
        assert_eq!(
            sent(set.receive(slot(2), &frame(2, 1, Message::ReleaseAll), at, &mut target)),
            (2, 1, Message::ReleaseAck)
        );
        assert_eq!(gate.floor().snapshot(), first);
        assert!(gate.admits_injection());
        assert_eq!(
            set.receive(slot(1), &key(2, 1, true), at, &mut target),
            Ok(None)
        );
        assert_eq!(
            target.calls,
            [MoveTo(Point::new(10.0, 10.0)), pressed(true)]
        );

        // Once the first peer returns, the second is admitted.
        assert_eq!(
            sent(set.receive(slot(1), &frame(2, 2, Message::ReleaseAll), at, &mut target)),
            (2, 1, Message::ReleaseAck)
        );
        assert_eq!(gate.floor().snapshot().state, FloorState::Free);
        assert_eq!(
            sent(set.receive(slot(2), &activation(3), at, &mut target)),
            (3, 0, Message::ActivationAck(DisplayId(1)))
        );
        let second = gate.floor().snapshot();
        assert_eq!(
            (second.state, second.peer),
            (FloorState::Receiving, slot(2))
        );
        assert!(gate.admits_injection());
    }

    #[test]
    fn removing_a_held_bystander_spares_the_owner_who_leaves_only_once_released() {
        let (mut set, gate) = set_with(&[1, 2]);
        let mut target = Destination::default();
        set.receive(slot(1), &activation(2), Duration::ZERO, &mut target)
            .unwrap();
        set.receive(slot(1), &key(2, 1, true), Duration::ZERO, &mut target)
            .unwrap();
        let owned = gate.floor().snapshot();
        gate.note_injected_press();
        // Slot 2 hears nothing for the whole deadline and holds, then leaves.
        assert!(matches!(
            sent(set.tick(slot(2), Duration::ZERO, &mut target)),
            (1, 3, Message::Ping(_))
        ));
        assert_eq!(set.tick(slot(2), PEER_LIVENESS, &mut target), Ok(None));
        assert!(set.receiver(slot(2)).unwrap().held_since().is_some());
        assert!(set.remove(slot(2), &mut target));
        assert!(set.receiver(slot(2)).is_none());
        assert_eq!(gate.floor().snapshot(), owned);
        assert!(gate.admits_injection());
        assert!(gate.injected_held(), "the owner's press is still counted");
        assert_eq!(
            target.calls,
            [MoveTo(Point::new(10.0, 10.0)), pressed(true)]
        );
        assert_eq!(
            set.tick(slot(2), Duration::ZERO, &mut target),
            Err(SlotFailure::UnknownSlot)
        );

        // The owner keeps the floor until its release lands.
        target.fail_release = true;
        assert!(!set.remove(slot(1), &mut target));
        assert!(!gate.admits_injection());
        assert_eq!(gate.floor().snapshot(), owned);
        target.fail_release = false;
        assert!(set.remove(slot(1), &mut target));
        assert_eq!(gate.floor().snapshot().state, FloorState::Free);
        assert!(!gate.injected_held());
        assert_eq!(set.occupied().count(), 0);
        assert_eq!(
            target.calls[2..],
            [EndGestures, ReleaseAll, EndGestures, ReleaseAll]
        );
    }

    #[test]
    fn the_owner_guard_passes_the_inbound_owner_and_releases_while_none_receives() {
        fn through(target: &mut Destination, floor: &SharedFloor, slot: FloorPeer) -> usize {
            let before = target.calls.len();
            let mut guarded = OwnerGuard {
                destination: target,
                floor,
                slot,
            };
            for action in [ReleaseAll, EndGestures, pressed(true)] {
                assert_eq!(guarded.apply(action), Ok(()));
            }
            target.calls.len() - before
        }
        let floor = SharedFloor::new();
        let mut target = Destination::default();
        // Free, then held by the outbound half: releases only.
        assert_eq!(through(&mut target, &floor, slot(1)), 2);
        floor
            .claim(floor.snapshot(), FloorState::Requesting, slot(1))
            .unwrap();
        assert_eq!(through(&mut target, &floor, slot(2)), 2);
        assert_eq!(
            target.calls,
            [ReleaseAll, EndGestures, ReleaseAll, EndGestures]
        );
        // Held inbound: everything for the owner, nothing for any other slot.
        floor
            .claim(floor.snapshot(), FloorState::Receiving, slot(1))
            .unwrap();
        assert_eq!(through(&mut target, &floor, slot(2)), 0);
        assert_eq!(through(&mut target, &floor, slot(1)), 3);
        assert_eq!(target.calls[4..], [ReleaseAll, EndGestures, pressed(true)]);
        floor
            .transition(floor.snapshot(), FloorState::Yielding)
            .unwrap();
        assert_eq!(through(&mut target, &floor, slot(2)), 0);
    }

    #[test]
    fn a_release_is_let_through_when_the_floor_has_no_inbound_owner() {
        let (mut set, gate) = set_with(&[1, 2, 3]);
        let floor = gate.floor().clone();
        let mut target = Destination::default();
        let at = Duration::ZERO;

        // The claim is freed before its source's barrier lands; the barrier still lifts the key.
        set.receive(slot(1), &activation(2), at, &mut target)
            .unwrap();
        set.receive(slot(1), &key(2, 1, true), at, &mut target)
            .unwrap();
        gate.note_injected_press();
        assert!(floor.release_peer(slot(1), floor.snapshot().generation));
        assert_eq!(
            sent(set.receive(slot(1), &frame(2, 2, Message::ReleaseAll), at, &mut target)),
            (2, 1, Message::ReleaseAck)
        );
        assert!(!gate.injected_held());
        assert_eq!(
            target.calls,
            [
                MoveTo(Point::new(10.0, 10.0)),
                pressed(true),
                EndGestures,
                ReleaseAll
            ]
        );

        // Freed before its slot is removed, the stop still releases.
        set.receive(slot(2), &activation(2), at, &mut target)
            .unwrap();
        set.receive(slot(2), &key(2, 1, true), at, &mut target)
            .unwrap();
        gate.note_injected_press();
        floor.reset();
        assert!(set.remove(slot(2), &mut target));
        assert!(!gate.injected_held());
        assert_eq!(
            target.calls[4..],
            [
                MoveTo(Point::new(10.0, 10.0)),
                pressed(true),
                EndGestures,
                ReleaseAll
            ]
        );

        // Held by this computer's outbound half, the floor has no inbound owner either.
        floor
            .claim(floor.snapshot(), FloorState::Requesting, slot(2))
            .unwrap();
        assert!(set.remove(slot(1), &mut target));
        assert_eq!(target.calls[8..], [EndGestures, ReleaseAll]);

        // While another slot receives, a release is withheld.
        let requested = floor.snapshot();
        assert!(floor.release_peer(slot(2), requested.generation));
        assert_eq!(
            sent(set.receive(slot(3), &activation(2), at, &mut target)),
            (2, 0, Message::ActivationAck(DisplayId(1)))
        );
        set.add(slot(1), receiver(), false, true, 3).unwrap();
        assert!(set.remove(slot(1), &mut target));
        assert_eq!(target.calls[10..], [MoveTo(Point::new(10.0, 10.0))]);
        assert!(gate.admits_injection());
    }

    #[test]
    fn take_back_after_the_floor_owner_changed_reaches_the_new_owner() {
        let (mut set, gate) = set_with(&[1, 2]);
        let mut target = Destination::default();
        let at = Duration::ZERO;
        set.receive(slot(1), &activation(2), at, &mut target)
            .unwrap();
        // Local input yields slot 1's claim, whose barrier lands before the set takes the
        // trigger; slot 2 then takes the floor.
        take_back_locally(&gate);
        assert_eq!(
            sent(set.receive(slot(1), &frame(2, 1, Message::ReleaseAll), at, &mut target)),
            (2, 1, Message::ReleaseAck)
        );
        assert_eq!(gate.floor().snapshot().state, FloorState::Free);
        set.receive(slot(2), &activation(2), at, &mut target)
            .unwrap();
        set.receive(slot(2), &key(2, 1, true), at, &mut target)
            .unwrap();
        let owned = gate.floor().snapshot();
        assert_eq!((owned.state, owned.peer), (FloorState::Receiving, slot(2)));
        let calls = target.calls.len();

        // The stale trigger reaches the new owner, which neither answers nor releases.
        assert_eq!(set.take_back(&mut target), Some((slot(2), Ok(None))));
        assert_eq!(target.calls.len(), calls);
        assert_eq!(gate.floor().snapshot(), owned);
        assert!(gate.admits_injection());

        // A take-back of the new owner's own claim yields it.
        take_back_locally(&gate);
        let (owner, taken) = set.take_back(&mut target).expect("the owner takes it");
        assert_eq!(owner, slot(2));
        assert_eq!(sent(taken), (2, 1, Message::TakeBack));
        assert_eq!(target.calls[calls..], [EndGestures, ReleaseAll]);
        assert!(set.take_back(&mut target).is_none());
    }

    #[test]
    fn slots_are_one_to_seven_and_hold_one_receiver_each() {
        let (mut set, _gate) = set_with(&[2, 7]);
        assert_eq!(
            set.add(FloorPeer::NONE, receiver(), false, true, 0),
            Err(SlotFailure::UnknownSlot)
        );
        assert_eq!(
            set.add(slot(7), receiver(), false, true, 0),
            Err(SlotFailure::Occupied)
        );
        assert_eq!(set.occupied().collect::<Vec<_>>(), [slot(2), slot(7)]);
        assert_eq!(
            set.receive(
                FloorPeer::NONE,
                &activation(2),
                Duration::ZERO,
                &mut Destination::default()
            ),
            Err(SlotFailure::UnknownSlot)
        );
    }
}
