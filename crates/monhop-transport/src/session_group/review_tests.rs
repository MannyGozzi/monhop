//! Hub core scenarios from the correctness review: native control that is busy or stopping, a
//! lost restore, stale destination events, handover edge cases, a mid-session joiner and a local
//! request made mid-transition.

use super::tests::{
    Command, FakeCapture, LETTER_A, Mesh, SHIFT, TRIP_START, pair, push, rest_at, ring,
};
use super::*;
use crate::{session_health::RETREAT_AFTER, session_receiver_set::SlotFailure};
use monhop_core::{ModifierState, capture::StopReason};

/// The harness's capture as native control answers while busy with an earlier command, or while
/// capture is stopping.
struct Control<'a> {
    capture: &'a mut FakeCapture,
    state: ControlState,
}

#[derive(Clone, Copy)]
enum ControlState {
    /// Every command waits behind one still being applied.
    Busy,
    /// Every command fails, and none completes past `completed`.
    Stopping { completed: u64 },
}

impl CaptureControl for Control<'_> {
    fn activate_remote(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
        self.refuse()
    }
    fn restore_local_at(&mut self, _: u64, _: Point) -> Result<u64, CaptureRefusal> {
        self.refuse()
    }
    fn renew_suppression(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
        self.refuse()
    }
    fn completed_control_revision(&self) -> Option<u64> {
        match self.state {
            ControlState::Busy => self.capture.completed_control_revision(),
            ControlState::Stopping { completed } => Some(completed),
        }
    }
    fn is_ready_for_suppression(&self) -> bool {
        self.capture.is_ready_for_suppression()
    }
    fn blocking_presses(&self) -> Vec<HeldInput> {
        self.capture.blocking_presses()
    }
    fn stop_reason(&self) -> Option<StopReason> {
        match self.state {
            ControlState::Busy => None,
            ControlState::Stopping { .. } => Some(StopReason::Requested),
        }
    }
    fn request_stop(&self) {}
}

impl Control<'_> {
    fn refuse(&self) -> Result<u64, CaptureRefusal> {
        Err(match self.state {
            ControlState::Busy => CaptureRefusal::Pending,
            ControlState::Stopping { .. } => CaptureRefusal::Failed,
        })
    }
}

/// `id` loses its link to `peer` while native control is in `state`.
fn end_link_while(mesh: &mut Mesh, id: u8, peer: u8, state: Option<ControlState>) {
    let computer = mesh.computer(id);
    let slot = computer.slot(peer);
    let result = Err(SessionFailure::Wire);
    let ended = match state {
        Some(state) => {
            let mut control = Control {
                capture: &mut computer.capture,
                state,
            };
            computer.core.remove_link(Some(&mut control), slot, result)
        }
        None => computer
            .core
            .remove_link(Some(&mut computer.capture), slot, result),
    };
    assert_eq!(ended, Ok(()), "computer {id} failed");
}

/// `id` has asked native input to stop; capture takes twice a suppression lease to finish while
/// the core keeps ticking, then the harness reports the stop.
fn stop_slowly(mesh: &mut Mesh, id: u8, completed: u64) {
    for _ in 0..2 * MAX_SUPPRESSION_TTL.as_millis() {
        mesh.clock.advance(Duration::from_millis(1));
        let computer = mesh.computer(id);
        let mut control = Control {
            capture: &mut computer.capture,
            state: ControlState::Stopping { completed },
        };
        let ticked = computer.core.on_tick(Some(&mut control), || None);
        assert_eq!(ticked, Ok(()), "computer {id} failed while stopping");
    }
    let core = &mesh.at(id).core;
    assert!(
        core.actions
            .iter()
            .any(|action| matches!(action, HubAction::StopNative))
    );
    assert_eq!(core.native, Native::Stopping);
    assert!(core.orphan.is_none() && core.pending.is_none() && core.submitted.is_none());
    mesh.pump();
}

/// After the only link ends, `id` holds nothing: the floor is free, the route and ledger start
/// over, and native input is stopped.
fn assert_stopped(mesh: &Mesh, id: u8, peer: u8) {
    let computer = mesh.at(id);
    assert!(!computer.native);
    assert_eq!(computer.native_stops, 1);
    assert_eq!(computer.core.native, Native::Idle);
    assert_eq!(computer.floor(), (FloorState::Free, None));
    assert_eq!(computer.core.route(), (false, 0));
    assert_eq!(computer.core.held(), (Vec::new(), Vec::new()));
    assert!(!computer.core.gate().injected_held());
    assert_eq!(computer.ended, [peer]);
}

/// Ticks until both ends of the link between `a` and `b` have started.
fn start_link(mesh: &mut Mesh, a: u8, b: u8) {
    for _ in 0..200 {
        mesh.tick();
        let ready = |id: u8, peer: u8| {
            let computer = mesh.at(id);
            computer.core.is_ready(computer.slot(peer))
        };
        if ready(a, b) && ready(b, a) {
            return;
        }
    }
    panic!("the link never started");
}

/// `id` asks for its input back, without delivering anything.
fn ask_local(mesh: &mut Mesh, id: u8) {
    let computer = mesh.computer(id);
    let asked = computer.core.request_local(Some(&mut computer.capture));
    assert_eq!(asked, Ok(()));
}

fn never_activated(mesh: &Mesh, id: u8) {
    let computer = mesh.at(id);
    assert!(
        computer
            .actor
            .received
            .iter()
            .all(|(_, message)| !matches!(message, Message::ActivateDisplayAt { .. }))
    );
    assert!(
        computer
            .actor
            .injector
            .actions
            .iter()
            .all(|action| action.is_release())
    );
}

#[test]
fn a_last_peer_leaving_with_a_pending_restore_stops_native_without_a_hub_failure() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    let held = mesh.at(1).core.floor();
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(2)));

    // The link drops while native control is busy with a lease renewal: the restore must wait,
    // and with no link left native input stops instead.
    end_link_while(&mut mesh, 1, 2, Some(ControlState::Busy));
    assert_eq!(mesh.at(1).core.floor(), held);
    stop_slowly(&mut mesh, 1, 1);

    // Stopping capture brought input home; the restore never had to.
    assert_stopped(&mesh, 1, 2);
    assert_eq!(mesh.at(1).capture.commands(), [Command::Activate]);
    let [(2, end)] = &mesh.at(1).closed[..] else {
        panic!("one link closed");
    };
    assert_eq!(end.result, Err(SessionFailure::Wire));
    assert_stopped(&mesh, 2, 1);
}

#[test]
fn a_slow_native_stop_never_trips_the_orphan_timeout() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);

    // The restore is granted, but capture stops before its barrier is read.
    end_link_while(&mut mesh, 1, 2, None);
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands(),
        [Command::Activate, Command::Restore(TRIP_START)]
    );
    assert_eq!(computer.capture.queue.len(), 1);
    stop_slowly(&mut mesh, 1, 1);

    assert_stopped(&mesh, 1, 2);
    assert_eq!(mesh.at(1).capture.queue.len(), 1);
    assert_stopped(&mesh, 2, 1);
}

#[test]
fn a_dropped_restore_effect_still_restores_local_input() {
    // 1 keeps its link to 3, so native input runs on.
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    let held = mesh.at(1).core.floor();
    assert_eq!(mesh.at(1).core.route(), (true, 1));

    // The owner's controller fails and the restore it yields is lost, as a full effect buffer or
    // exhausted route requests lose it; then its link ends.
    let computer = mesh.computer(1);
    let now = computer.core.now();
    let index = index_of(computer.slot(2)).unwrap();
    let Slot::Linked(link) = &mut computer.core.slots[index] else {
        panic!("a live link");
    };
    assert_eq!(
        link.source.abandon(now).failure,
        Some(SourceFailure::LinkEnded)
    );
    mesh.end_link(1, 2);
    let computer = mesh.at(1);
    let home = Point::new(0.0, 0.0);
    assert_eq!(
        computer.capture.commands.last(),
        Some(&(Command::Restore(home), held))
    );
    assert_eq!(computer.core.floor(), held);

    mesh.pump();
    let computer = mesh.at(1);
    assert!(computer.native);
    assert_eq!(computer.floor(), (FloorState::Free, None));
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(computer.source(3).capture_route(), (false, 2));
    assert_eq!(computer.ended, [2]);
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
}

#[test]
fn a_stale_destination_event_does_not_end_a_new_link_in_the_same_slot() {
    let mut mesh = Mesh::start(pair());
    mesh.drop_link(1, 2);
    mesh.pump();
    assert!(!mesh.at(1).native);

    // Events the stopped actor still held for the slot's earlier receiver reach the new link
    // while it starts.
    mesh.connect(1, 2);
    let computer = mesh.computer(1);
    let slot = computer.slot(2);
    for event in [SlotEvent::Failed(SlotFailure::UnknownSlot), SlotEvent::Left] {
        let handled = computer
            .core
            .on_destination(None::<&mut FakeCapture>, slot, event);
        assert_eq!(handled, Ok(()));
    }
    assert!(computer.core.actions.is_empty());
    start_link(&mut mesh, 1, 2);
    assert_eq!(mesh.at(1).closed.len(), 1);
    assert_eq!(mesh.at(1).ended, [2]);
    assert_eq!(mesh.at(1).started, [2, 2]);

    // A stop report from the earlier native run changes nothing in this one.
    mesh.cross_right(1);
    let (held, route) = (mesh.at(1).core.floor(), mesh.at(1).core.route());
    assert_eq!(held.state, FloorState::Sending);
    mesh.computer(1).core.native_stopped();
    assert_eq!(mesh.at(1).core.floor(), held);
    assert_eq!(mesh.at(1).core.route(), route);
    mesh.tap(1, LETTER_A);
    assert_eq!(mesh.at(2).keys(), [(LETTER_A, true), (LETTER_A, false)]);
}

#[test]
fn an_owner_leaving_during_its_own_restore_is_not_restored_twice() {
    // 1 keeps its link to 3, so native input runs on.
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    ask_local(&mut mesh, 1);
    mesh.pump_until(|mesh| mesh.at(1).capture.commands.len() == 2);
    let returning = mesh.at(1).core.floor();
    assert_eq!(returning.state, FloorState::Returning);
    assert_eq!(mesh.at(1).capture.queue.len(), 1);

    // X's link drops after its restore went out but before that barrier is read.
    mesh.end_link(1, 2);
    assert_eq!(mesh.at(1).core.floor(), returning);
    mesh.pump();
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands(),
        [Command::Activate, Command::Restore(TRIP_START)]
    );
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(computer.source(3).capture_route(), (false, 2));
    assert_eq!(
        computer.floors,
        [
            (FloorState::Free, None),
            (FloorState::Requesting, Some(2)),
            (FloorState::Sending, Some(2)),
            (FloorState::Returning, Some(2)),
            (FloorState::Free, None),
        ]
    );
    assert_eq!(computer.ended, [2]);
}

#[test]
fn a_live_y_refusing_the_handover_restores_local_input() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    // Y's replies stop reaching 1 long enough that its controller would hand the pointer straight
    // home, though its link is still up.
    mesh.cut.push((3, 1));
    mesh.idle(u64::try_from(RETREAT_AFTER.as_millis()).unwrap() + 20);
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(2)));

    mesh.push_on(1);
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands(),
        [Command::Activate, Command::Restore(TRIP_START)]
    );
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(
        computer.floors,
        [
            (FloorState::Free, None),
            (FloorState::Requesting, Some(2)),
            (FloorState::Sending, Some(2)),
            (FloorState::Returning, Some(2)),
            (FloorState::Free, None),
        ]
    );
    assert_eq!(computer.source(3).mode(), SourceMode::Local);
    assert_eq!(computer.source(3).capture_route(), (false, 2));
    assert!(computer.closed.is_empty());
    assert_eq!(mesh.at(2).released(), 1);
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
    assert_eq!(mesh.at(3).floors, [(FloorState::Free, None)]);
    never_activated(&mesh, 3);

    // Heard from again, Y takes the next crossing.
    mesh.cut.clear();
    mesh.idle(40);
    mesh.cross_left(1);
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(3)));
    assert_eq!(mesh.at(3).floor(), (FloorState::Receiving, Some(1)));
}

#[test]
fn y_dropping_after_accepting_restores_local_input() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    mesh.moved(1, 98.0);
    mesh.queue(1, push(1.0, 0.0));
    mesh.pump_until(|mesh| mesh.at(1).floor() == (FloorState::Sending, Some(3)));
    assert_eq!(mesh.at(1).capture.commands(), [Command::Activate]);

    // Y's link drops before Y acknowledges the activation.
    mesh.drop_link(1, 3);
    let computer = mesh.at(1);
    let (restore, held) = computer.capture.commands.last().copied().unwrap();
    assert_eq!(restore, Command::Restore(TRIP_START));
    assert_eq!(
        (held.state, held.peer),
        (FloorState::Sending, computer.slot(3))
    );
    assert_eq!(computer.core.floor(), held);

    mesh.pump();
    let computer = mesh.at(1);
    assert_eq!(computer.capture.commands.len(), 2);
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(
        computer.floors,
        [
            (FloorState::Free, None),
            (FloorState::Requesting, Some(2)),
            (FloorState::Sending, Some(2)),
            (FloorState::Returning, Some(2)),
            (FloorState::Sending, Some(3)),
            (FloorState::Free, None),
        ]
    );
    assert_eq!(computer.source(2).mode(), SourceMode::Local);
    assert_eq!(computer.source(2).capture_route(), (false, 2));
    assert_eq!(computer.ended, [3]);
    assert_eq!(mesh.at(2).released(), 1);
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
    never_activated(&mesh, 3);
}

#[test]
fn x_dropping_before_the_handover_ack_restores_local_input() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    mesh.moved(1, 98.0);
    mesh.queue(1, push(1.0, 0.0));
    mesh.stage();
    let returning = mesh.at(1).core.floor();
    assert_eq!(mesh.at(1).floor(), (FloorState::Returning, Some(2)));

    // X's link drops while it waits for the release that would hand the pointer to Y.
    mesh.drop_link(1, 2);
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands.last(),
        Some(&(Command::Restore(TRIP_START), returning))
    );
    assert_eq!(computer.core.floor(), returning);

    mesh.pump();
    let computer = mesh.at(1);
    assert_eq!(computer.capture.commands.len(), 2);
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(computer.floor(), (FloorState::Free, None));
    assert_eq!(computer.source(3).mode(), SourceMode::Local);
    assert_eq!(computer.source(3).capture_route(), (false, 2));
    assert!(!computer.source(3).owns_capture_route());
    assert_eq!(computer.ended, [2]);
    let x = mesh.at(2);
    assert_eq!(x.floor(), (FloorState::Free, None));
    assert!(!x.actor.injector.keys.contains(&true));
    assert_eq!(mesh.at(3).floors, [(FloorState::Free, None)]);
    never_activated(&mesh, 3);
}

#[test]
fn a_peer_joining_mid_session_is_seeded_with_held_modifiers_and_the_route() {
    let mut mesh = Mesh::start(ring(3));
    mesh.drop_link(1, 3);
    mesh.pump();
    assert_eq!(mesh.at(1).ended, [3]);
    mesh.cross_right(1);
    mesh.key(1, SHIFT, true, ModifierState::LEFT_SHIFT);
    assert_eq!(mesh.at(2).keys(), [(SHIFT, true)]);

    // 3 links up again while 1 controls 2 with Shift down.
    mesh.connect(1, 3);
    start_link(&mut mesh, 1, 3);
    let computer = mesh.at(1);
    assert_eq!(computer.started, [2, 3, 3]);
    assert_eq!(computer.source(3).capture_route(), (true, 1));
    assert!(!computer.source(3).owns_capture_route());

    // Keys typed with Shift down match the joiner's ledger, so its link runs on.
    for pressed in [true, false] {
        mesh.key(1, LETTER_A, pressed, ModifierState::LEFT_SHIFT);
    }
    assert_eq!(
        mesh.at(2).keys(),
        [(SHIFT, true), (LETTER_A, true), (LETTER_A, false)]
    );

    // Handed the pointer, the joiner carries the Shift it never saw pressed.
    mesh.push_on(1);
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(3)));
    assert_eq!(mesh.at(3).keys(), [(SHIFT, true)]);
    mesh.key(1, SHIFT, false, 0);
    assert_eq!(mesh.at(3).keys(), [(SHIFT, true), (SHIFT, false)]);
    assert_eq!(mesh.at(1).closed.len(), 1);
}

#[test]
fn a_local_request_during_an_activation_brings_input_home_once_it_lands() {
    let mut mesh = Mesh::start(pair());
    for event in [rest_at(99.0, 50.0), push(1.0, 0.0)] {
        mesh.queue(1, event);
    }
    mesh.stage();
    assert_eq!(mesh.at(1).floor(), (FloorState::Requesting, Some(2)));
    ask_local(&mut mesh, 1);
    assert!(mesh.at(1).core.local_requested);

    mesh.pump();
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands(),
        [Command::Activate, Command::Restore(TRIP_START)]
    );
    assert_eq!(
        computer.floors,
        [
            (FloorState::Free, None),
            (FloorState::Requesting, Some(2)),
            (FloorState::Sending, Some(2)),
            (FloorState::Returning, Some(2)),
            (FloorState::Free, None),
        ]
    );
    assert!(!computer.core.local_requested);
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
    assert_eq!(mesh.at(2).released(), 1);
}

#[test]
fn a_local_request_lapses_with_a_declined_crossing() {
    let mut mesh = Mesh::start(ring(3));
    // 2 and 3 cross onto 1 at once; 2's request arrives first and wins.
    for event in [rest_at(0.0, 50.0), push(-1.0, 0.0)] {
        mesh.queue(2, event);
    }
    for event in [rest_at(99.0, 50.0), push(1.0, 0.0)] {
        mesh.queue(3, event);
    }
    mesh.stage();
    ask_local(&mut mesh, 3);
    assert!(mesh.at(3).core.local_requested);
    mesh.pump();
    assert_eq!(mesh.at(3).floor(), (FloorState::Free, None));
    assert!(!mesh.at(3).core.local_requested);

    // 3's next crossing, once 1 is free, stays on 1.
    mesh.request_local(2);
    assert_eq!(mesh.at(1).floor(), (FloorState::Free, None));
    let mut presses = 0;
    while mesh.at(3).floor() != (FloorState::Sending, Some(1)) {
        assert!(presses < 6, "the decline is retried");
        presses += 1;
        mesh.idle(20);
        mesh.cross_right(3);
    }
    mesh.idle(20);
    assert_eq!(mesh.at(3).floor(), (FloorState::Sending, Some(1)));
    assert_eq!(mesh.at(3).capture.commands(), [Command::Activate]);
}
