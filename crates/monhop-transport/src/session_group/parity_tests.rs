//! A hub with one peer against the pairwise runtime's scenarios.
//!
//! Each test below drives the same [`HubCore`] the multi-peer hub uses, through the harness in
//! [`super::tests`], with exactly one linked peer: a hub reduced to one link must behave exactly
//! like `run_session`'s pairwise coordinator did. Two of the ten scenarios
//! (`every_seam_is_a_wall_until_capture_can_suppress` and
//! `a_capture_that_cannot_suppress_yet_unwinds_the_crossing_instead_of_failing`) need a capture
//! that can refuse readiness or an activation; [`super::tests::FakeCapture`] never refuses either
//! (it is a fixed "always ready, always succeeds" stub), so those two drive `HubCore::on_captured`
//! / `on_frame` directly with [`StubCapture`] below instead of going through [`Mesh`]'s normal
//! per-tick capture.

use super::tests::{LETTER_A, Mesh, moved, pair, push, rest_at};
use super::*;
use monhop_core::{
    capture::{CaptureEvent, CaptureStop, StopReason, capture_channel},
    capture_physical::PhysicalCapture,
};
use monhop_protocol::DeclineReason;

fn ms(t: u64) -> Duration {
    Duration::from_millis(t)
}

/// Delivers exactly the next wire frame and settles its effects on the receiving computer, without
/// delivering whatever that in turn produces. The hub-level analogue of the pairwise harness's
/// single-frame `Pair::step`, which the mesh's own actor/action split has no equivalent for.
fn step_one(mesh: &mut Mesh) {
    let (to, from, frame) = mesh.wire.pop_front().expect("a frame is queued");
    mesh.deliver(to, from, frame);
    while mesh.step(usize::from(to - 1)) {}
}

/// Jumps the mesh clock forward to an absolute point in time, mirroring the pairwise harness's
/// `Pair::at` (which sets each coordinator's clock directly instead of accumulating deltas).
fn advance_to(mesh: &mut Mesh, target: Duration) {
    let now = mesh.clock.now();
    if target > now {
        mesh.clock.advance(target - now);
    }
}

/// A capture stub used only for direct `HubCore` calls that bypass `FakeCapture`, to drive
/// readiness or an activation refusal that the mesh's own capture never produces.
#[derive(Default)]
struct StubCapture {
    ready: bool,
    refuse_activate: bool,
    revision: u64,
}

impl CaptureControl for StubCapture {
    fn activate_remote(&mut self, _: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        if self.refuse_activate {
            return Err(CaptureRefusal::NotReady);
        }
        assert!(!ttl.is_zero());
        self.revision += 1;
        Ok(self.revision)
    }
    fn restore_local_at(&mut self, _: u64, _: Point) -> Result<u64, CaptureRefusal> {
        self.revision += 1;
        Ok(self.revision)
    }
    fn renew_suppression(&mut self, _: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        assert!(!ttl.is_zero());
        Ok(self.revision)
    }
    fn completed_control_revision(&self) -> Option<u64> {
        Some(self.revision)
    }
    fn is_ready_for_suppression(&self) -> bool {
        self.ready
    }
    fn blocking_presses(&self) -> Vec<HeldInput> {
        Vec::new()
    }
    fn stop_reason(&self) -> Option<StopReason> {
        None
    }
    fn request_stop(&self) {}
}

// session_runtime.rs:444
#[test]
fn lower_device_wins_tie_in_both_arrival_orders() {
    for reversed in [false, true] {
        let mut mesh = Mesh::start(pair());
        mesh.queue(1, rest_at(99.0, 50.0));
        mesh.queue(1, push(1.0, 0.0));
        mesh.queue(2, rest_at(0.0, 50.0));
        mesh.queue(2, push(-1.0, 0.0));
        mesh.stage();
        if reversed {
            mesh.wire.make_contiguous().reverse();
        }
        mesh.pump();
        assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(2)));
        assert_eq!(mesh.at(2).floor(), (FloorState::Receiving, Some(1)));
        assert_eq!(mesh.at(2).source(1).mode(), SourceMode::Local);
    }
}

// session_runtime.rs:459
#[test]
fn declined_epoch_is_consumed() {
    let mut mesh = Mesh::start(pair());
    mesh.queue(1, rest_at(99.0, 50.0));
    mesh.queue(1, push(1.0, 0.0));
    mesh.queue(2, rest_at(0.0, 50.0));
    mesh.queue(2, push(-1.0, 0.0));
    mesh.stage();
    mesh.pump();
    let slot = mesh.at(1).slot(2);
    let epoch = mesh
        .at(1)
        .actor
        .set
        .receiver(slot)
        .expect("the link is up")
        .epoch();
    assert_eq!(epoch.get(), 11);
}

// session_runtime.rs:467
#[test]
fn take_back_in_remote_returns_home() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    mesh.take_back(2);
    assert_eq!(mesh.at(1).source(2).mode(), SourceMode::Local);
    for id in [1, 2] {
        assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
    }
}

// session_runtime.rs:535
#[test]
fn reverse_request_during_returning_is_busy_then_retried() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    {
        let computer = mesh.computer(1);
        let capture = computer.native.then_some(&mut computer.capture);
        assert_eq!(computer.core.request_local(capture), Ok(()));
    }
    while mesh.step(0) {}
    step_one(&mut mesh); // ReleaseAll frees 2. Its acknowledgement has not reached 1 yet.
    assert_eq!(mesh.at(1).floor(), (FloorState::Returning, Some(2)));

    // A fresh floor generation needs a real pointer poll (not a captured motion) to rearm 2's
    // controller before it can cross; done through `on_tick` directly so the withheld
    // acknowledgement below is not swept up by `Mesh`'s own auto-pumping helpers.
    mesh.clock.advance(ms(20));
    {
        let computer = mesh.computer(2);
        computer.capture.pointer = Point::new(0.0, 50.0);
        let pointer = computer.capture.pointer;
        let capture = computer.native.then_some(&mut computer.capture);
        assert_eq!(computer.core.on_tick(capture, || Some(pointer)), Ok(()));
    }
    mesh.queue(2, push(-1.0, 0.0));
    mesh.stage();
    let ack = mesh
        .wire
        .pop_front()
        .expect("2's acknowledgement is queued");
    step_one(&mut mesh); // 2's crossing reaches 1 before the ReleaseAck.
    assert!(matches!(
        mesh.wire.front().expect("the decline is queued").2.message,
        Message::ActivationDeclined {
            reason: DeclineReason::Busy,
            ..
        }
    ));
    mesh.pump(); // 2 records the decline here, on the clock `base` below reads.
    mesh.wire.push_back(ack);
    mesh.pump();

    // 2 keeps pushing: each press past the decline waits out its retry delay, timed from when
    // the decline above was recorded rather than from the mesh's own start.
    let base = mesh.clock.now();
    for t in [10, 20, 30, 40] {
        advance_to(&mut mesh, base + ms(t));
        mesh.input(2, push(-1.0, 0.0));
        assert_eq!(mesh.at(2).source(1).mode(), SourceMode::Local);
    }
    advance_to(&mut mesh, base + ms(50));
    mesh.input(2, push(-1.0, 0.0));
    assert_eq!(mesh.at(2).source(1).mode(), SourceMode::Remote);
}

// session_runtime.rs:703
#[test]
fn crossings_rearm_from_fresh_poll_after_free() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    let old = mesh.at(2).core.floor().generation;
    // Queued now, while 2's floor is still busy, so it is stamped with the stale generation;
    // nothing drains it until the release below, after the generation has moved on.
    mesh.queue(2, moved(5.0, 0.0));
    {
        let computer = mesh.computer(1);
        let capture = computer.native.then_some(&mut computer.capture);
        assert_eq!(computer.core.request_local(capture), Ok(()));
    }
    while mesh.step(0) {}
    step_one(&mut mesh); // Frees 2's floor and, in the same settle, drains the stale record.
    mesh.pump();
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
    assert_ne!(mesh.at(2).core.floor().generation, old);

    // A stale rest position elsewhere is polled, but arms nothing (not at a linked edge).
    mesh.computer(2).capture.pointer = Point::new(50.0, 50.0);
    mesh.idle(20);
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));

    // Only a fresh poll at the edge, followed by a press, rearms the crossing.
    mesh.computer(2).capture.pointer = Point::new(0.0, 50.0);
    mesh.idle(20);
    mesh.input(2, push(-1.0, 0.0));
    assert_eq!(mesh.at(2).floor(), (FloorState::Sending, Some(1)));
}

// session_runtime.rs:731
#[test]
fn disconnect_mid_transition_releases_both_halves() {
    let mut mesh = Mesh::start(pair());
    mesh.queue(1, rest_at(99.0, 50.0));
    mesh.queue(1, push(1.0, 0.0));
    mesh.stage();
    step_one(&mut mesh); // 2 activates and acknowledges; the ack is still in flight.
    mesh.drop_link(1, 2);
    mesh.pump();
    for id in [1, 2] {
        assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
        assert!(!mesh.at(id).core.gate().admits_injection());
    }
}

// session_runtime.rs:762
#[test]
fn drag_in_progress_take_back_withholds_until_release() {
    // The harness's `Injector` only instruments key destinations, not buttons, so a held key
    // stands in for the drag; the take-back mechanics under test (withholding an already-queued
    // response until the physical release lands) are identical for both.
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    mesh.key(1, LETTER_A, true, 0);
    assert_eq!(mesh.at(2).keys(), [(LETTER_A, true)]);

    let gate = mesh.at(2).core.gate().clone();
    let now = mesh.clock.now();
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let mut physical = PhysicalCapture::new(now).with_take_back(gate.clone());
    let motion = CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 };
    assert!(physical.process(motion, false, now, &mut producer, &stop));
    assert!(gate.injected_held());
    assert!(!gate.admits_injection());
    assert!(!consumer.try_pop_tagged().unwrap().unwrap().remote);

    let computer = mesh.computer(2);
    let (slot, result) = computer
        .actor
        .set
        .take_back(&mut computer.actor.injector)
        .expect("the gate's trigger is pending");
    assert_eq!(slot, computer.slot(1));
    assert!(matches!(result, Ok(Some(frame)) if frame.message == Message::TakeBack));
    assert!(!computer.actor.injector.keys[usize::from(LETTER_A)]);
    assert!(!gate.injected_held());
    assert!(!physical.process(motion, false, now + ms(1), &mut producer, &stop));
}

// session_runtime.rs:1065
#[test]
fn every_seam_is_a_wall_until_capture_can_suppress() {
    let mut mesh = Mesh::start(pair());
    let route = mesh.at(1).core.route();
    let generation = mesh.at(1).core.floor().generation;
    let event_at = |event: CaptureEvent| CapturedEvent {
        event,
        routing_revision: route.1,
        remote: route.0,
        floor_generation: generation,
    };

    let mut not_ready = StubCapture {
        ready: false,
        ..StubCapture::default()
    };
    let computer = mesh.computer(1);
    assert_eq!(
        computer
            .core
            .on_captured(&mut not_ready, event_at(rest_at(99.0, 50.0))),
        Ok(())
    );
    assert_eq!(
        computer
            .core
            .on_captured(&mut not_ready, event_at(push(1.0, 0.0))),
        Ok(())
    );
    assert_eq!(mesh.at(1).floor(), (FloorState::Free, None));

    let mut ready = StubCapture {
        ready: true,
        ..StubCapture::default()
    };
    let computer = mesh.computer(1);
    assert_eq!(
        computer
            .core
            .on_captured(&mut ready, event_at(rest_at(99.0, 50.0))),
        Ok(())
    );
    assert_eq!(
        computer
            .core
            .on_captured(&mut ready, event_at(push(1.0, 0.0))),
        Ok(())
    );
    while mesh.step(0) {}
    assert_eq!(mesh.at(1).floor(), (FloorState::Requesting, Some(2)));
}

// session_runtime.rs:1075
#[test]
fn a_capture_that_cannot_suppress_yet_unwinds_the_crossing_instead_of_failing() {
    let mut mesh = Mesh::start(pair());
    mesh.queue(1, rest_at(99.0, 50.0));
    mesh.queue(1, push(1.0, 0.0));
    mesh.stage();
    step_one(&mut mesh); // 2 activates and acknowledges.
    let (to, from, ack) = mesh
        .wire
        .pop_front()
        .expect("the acknowledgement is in flight");
    assert_eq!((to, from), (1, 2));

    let slot = mesh.at(1).slot(2);
    let mut refusing = StubCapture {
        ready: false,
        refuse_activate: true,
        ..StubCapture::default()
    };
    {
        let computer = mesh.computer(1);
        let capture = computer.native.then_some(&mut refusing);
        assert_eq!(computer.core.on_frame(capture, slot, ack), Ok(()));
    }
    while mesh.step(0) {}
    mesh.pump();

    assert_eq!(mesh.at(1).source(2).mode(), SourceMode::Local);
    assert!(
        mesh.at(2)
            .actor
            .set
            .receiver(mesh.at(2).slot(1))
            .expect("the link is up")
            .active_display()
            .is_none()
    );
    for id in [1, 2] {
        assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
    }

    // The seam stays a wall until the runtime reports capture ready again: feed the same refusing
    // capture a fresh rest-and-push, tagged with the now-current route and floor generation.
    let route = mesh.at(1).core.route();
    let generation = mesh.at(1).core.floor().generation;
    let event_at = |event: CaptureEvent| CapturedEvent {
        event,
        routing_revision: route.1,
        remote: route.0,
        floor_generation: generation,
    };
    let computer = mesh.computer(1);
    assert_eq!(
        computer
            .core
            .on_captured(&mut refusing, event_at(rest_at(99.0, 50.0))),
        Ok(())
    );
    assert_eq!(
        computer
            .core
            .on_captured(&mut refusing, event_at(push(1.0, 0.0))),
        Ok(())
    );
    assert_eq!(mesh.at(1).floor(), (FloorState::Free, None));
}

// session_runtime.rs:1135
#[test]
fn crossing_straight_back_after_an_own_return_needs_no_fresh_poll() {
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    mesh.request_local(1);
    assert_eq!(mesh.at(1).source(2).mode(), SourceMode::Local);
    assert_eq!(mesh.at(1).floor(), (FloorState::Free, None));
    mesh.input(1, push(1.0, 0.0));
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(2)));
}
