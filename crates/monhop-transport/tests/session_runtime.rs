//! Two deterministic coordinators: manual time, capture FIFO and scoped in-memory frame pipe.
use monhop_core::{
    DeviceId, Display, DisplayId, Edge, EdgeLink, FloorState, LogicalSize, Machine, NativeSize,
    NormalizedSpan, Platform, Point, SharedFloor, TakeBackGate, Topology,
};
use monhop_protocol::{DisplayDescription, DisplayTopology, Frame, Message, SessionEpoch};
use monhop_transport::{
    session::SessionScopes,
    session_receiver::{DestinationAction, DestinationFailure, InputDestination, InputReceiver},
    session_source::{
        NormalizedInput, PUSH_THROUGH_DISTANCE, SourceController, SourceEffect, SourceMode,
        SourceOutcome, TaggedInput,
    },
};
use std::{collections::VecDeque, time::Duration};
fn ms(t: u64) -> Duration {
    Duration::from_millis(t)
}
fn device(id: u8) -> DeviceId {
    DeviceId([id; 16])
}
fn displays(id: u64) -> DisplayTopology {
    DisplayTopology::new(
        [id, id + 10]
            .map(|display| DisplayDescription {
                id: DisplayId(display),
                name: "fixture".into(),
                native_width: 100,
                native_height: 100,
                logical_origin: Point::new(0.0, if display == id { 0.0 } else { 100.0 }),
                logical_size: Point::new(100.0, 100.0),
                scale_factor: 1.0,
                is_primary: display == id,
                monitor: None,
            })
            .to_vec(),
    )
    .unwrap()
}
fn topology(local: u8) -> Topology {
    let other = 3 - local;
    let span = NormalizedSpan::new(0.0, 1.0).unwrap();
    let mut links = vec![
        EdgeLink::new(
            DisplayId(u64::from(local)),
            Edge::Right,
            span,
            DisplayId(u64::from(other)),
            Edge::Left,
            span,
            1.0,
        )
        .unwrap(),
        EdgeLink::new(
            DisplayId(u64::from(other)),
            Edge::Left,
            span,
            DisplayId(u64::from(local)),
            Edge::Right,
            span,
            1.0,
        )
        .unwrap(),
    ];
    for id in [local, other] {
        links.push(
            EdgeLink::new(
                DisplayId(u64::from(id)),
                Edge::Bottom,
                span,
                DisplayId(u64::from(id) + 10),
                Edge::Top,
                span,
                1.0,
            )
            .unwrap(),
        );
        links.push(
            EdgeLink::new(
                DisplayId(u64::from(id) + 10),
                Edge::Top,
                span,
                DisplayId(u64::from(id)),
                Edge::Bottom,
                span,
                1.0,
            )
            .unwrap(),
        );
    }
    Topology::new(
        vec![
            Machine::new(device(local), Platform::MacOs),
            Machine::new(device(other), Platform::MacOs),
        ],
        [local, other]
            .into_iter()
            .flat_map(|id| {
                displays(u64::from(id))
                    .displays()
                    .iter()
                    .map(|d| {
                        Display::new(
                            d.id,
                            device(id),
                            "fixture".into(),
                            NativeSize::new(100, 100),
                            LogicalSize::new(100.0, 100.0),
                            d.logical_origin,
                            1.0,
                            None,
                            d.is_primary,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
        links,
    )
    .unwrap()
}
struct Destination {
    actions: Vec<DestinationAction>,
    gate: TakeBackGate,
    keys: [bool; 256],
    buttons: [bool; 5],
}
impl Destination {
    fn new(gate: TakeBackGate) -> Self {
        Self {
            actions: Vec::new(),
            gate,
            keys: [false; 256],
            buttons: [false; 5],
        }
    }
}
impl InputDestination for Destination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let down = match action {
            DestinationAction::Key {
                usage,
                pressed: true,
            } => !self.keys[usize::from(usage.0)],
            DestinationAction::Button {
                button,
                pressed: true,
            } => !self.buttons[button.index()],
            _ => false,
        };
        if down {
            self.gate.note_injected_press();
        }
        if !matches!(action, DestinationAction::ReleaseAll) && !self.gate.admits_injection() {
            if down {
                self.gate.note_injected_up();
            }
            return Ok(());
        }
        self.actions.push(action);
        match action {
            DestinationAction::Key { usage, pressed } => {
                if !pressed && self.keys[usize::from(usage.0)] {
                    self.gate.note_injected_up();
                }
                self.keys[usize::from(usage.0)] = pressed;
            }
            DestinationAction::Button { button, pressed } => {
                if !pressed && self.buttons[button.index()] {
                    self.gate.note_injected_up();
                }
                self.buttons[button.index()] = pressed;
            }
            DestinationAction::ReleaseAll => {
                self.keys.fill(false);
                self.buttons.fill(false);
                self.gate.note_injected_released();
            }
            _ => {}
        }
        Ok(())
    }
}
struct Coordinator {
    source: SourceController,
    receiver: InputReceiver,
    floor: SharedFloor,
    gate: TakeBackGate,
    destination: Destination,
    capture: VecDeque<TaggedInput>,
    scopes: SessionScopes,
    reply_epoch: SessionEpoch,
    reply_sequence: u64,
    heartbeat_sequence: u64,
    ticket: u64,
    now: Duration,
}
impl Coordinator {
    fn new(id: u8, allowed: bool) -> Self {
        let floor = SharedFloor::new();
        let gate = TakeBackGate::new(floor.clone());
        let epoch = SessionEpoch::new(10).unwrap();
        Self {
            source: SourceController::new(
                topology(id),
                device(id),
                DisplayId(u64::from(id)),
                epoch,
                3,
                ms(0),
            )
            .unwrap()
            .with_floor(floor.clone(), allowed),
            receiver: InputReceiver::new(displays(u64::from(id)), epoch, ms(0)).with_floor(
                gate.clone(),
                id == 1,
                allowed,
            ),
            floor,
            gate: gate.clone(),
            destination: Destination::new(gate),
            capture: VecDeque::new(),
            scopes: SessionScopes::new(device(id), device(3 - id)).unwrap(),
            reply_epoch: epoch,
            reply_sequence: 0,
            heartbeat_sequence: 3,
            ticket: 0,
            now: ms(0),
        }
    }
    fn effects(&mut self, outcome: SourceOutcome) -> Vec<Frame> {
        assert_eq!(outcome.failure, None);
        let mut frames = Vec::new();
        for effect in outcome.effects.iter() {
            match effect {
                SourceEffect::RemoteFrame(frame) => {
                    frames.push(frame.clone().with_scope(self.scopes.outbound))
                }
                SourceEffect::ActivateRemote { request }
                | SourceEffect::RestoreLocalAt { request, .. } => {
                    self.ticket += 1;
                    let remote = matches!(effect, SourceEffect::ActivateRemote { .. });
                    let bound = self
                        .source
                        .bind_capture_route(*request, self.ticket, self.now);
                    assert_eq!(bound.failure, None);
                    self.capture.push_back(TaggedInput {
                        event: NormalizedInput::RouteChanged {
                            remote,
                            revision: self.ticket,
                        },
                        remote,
                        routing_revision: self.ticket,
                        floor_generation: self.floor.snapshot().generation,
                    });
                }
            }
        }
        frames
    }
    fn response(&mut self, message: Message) -> Frame {
        let heartbeat = matches!(message, Message::Ping(_) | Message::Pong(_));
        let epoch = if heartbeat {
            self.receiver.control_epoch()
        } else {
            self.receiver.epoch()
        };
        if epoch != self.reply_epoch && !heartbeat {
            self.reply_epoch = epoch;
            self.reply_sequence = 0;
        }
        let sequence = if heartbeat {
            &mut self.heartbeat_sequence
        } else {
            &mut self.reply_sequence
        };
        let frame = Frame::new(epoch, *sequence, message).with_scope(self.scopes.inbound);
        *sequence += 1;
        frame
    }
    fn receive(&mut self, frame: Frame) -> Vec<Frame> {
        self.scopes.validate(&frame).unwrap();
        if frame.scope == self.scopes.outbound {
            let o = self.source.on_remote_frame(&frame, self.now);
            self.effects(o)
        } else {
            let response = self
                .receiver
                .receive(&frame, self.now, &mut self.destination)
                .unwrap();
            self.receiver.flush(&mut self.destination).unwrap();
            response.map(|m| self.response(m)).into_iter().collect()
        }
    }
    fn input(&mut self, event: NormalizedInput) -> Vec<Frame> {
        let (remote, routing_revision) = self.source.capture_route();
        let record = TaggedInput {
            event,
            remote,
            routing_revision,
            floor_generation: self.floor.snapshot().generation,
        };
        let o = self.source.on_captured(record, self.now);
        self.effects(o)
    }
    /// Rests on the linked right edge and pushes through it; an arrival's own delta never counts.
    fn cross(&mut self) -> Vec<Frame> {
        let o = self
            .source
            .observe_pointer(Point::new(99.0, 50.0), self.now);
        assert!(self.effects(o).is_empty());
        let mut frames = self.input(NormalizedInput::RelativeMotion(Point::new(5.0, 0.0)));
        frames.extend(self.input(NormalizedInput::RelativeMotion(Point::new(
            PUSH_THROUGH_DISTANCE,
            0.0,
        ))));
        frames
    }
    fn drain(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(record) = self.capture.pop_front() {
            let o = self.source.on_captured(record, self.now);
            frames.extend(self.effects(o));
        }
        frames
    }
}
struct Pair {
    computers: [Coordinator; 2],
    pipe: VecDeque<(usize, Frame)>,
}
impl Pair {
    fn new() -> Self {
        Self {
            computers: [Coordinator::new(1, true), Coordinator::new(2, true)],
            pipe: VecDeque::new(),
        }
    }
    fn enqueue(&mut self, from: usize, frames: Vec<Frame>) {
        self.pipe.extend(frames.into_iter().map(|f| (1 - from, f)));
    }
    fn cross(&mut self, id: usize) {
        let frames = self.computers[id].cross();
        self.enqueue(id, frames);
    }
    fn step(&mut self) {
        let (to, frame) = self.pipe.pop_front().unwrap();
        let frames = self.computers[to].receive(frame);
        self.enqueue(to, frames);
    }
    fn at(&mut self, t: u64) {
        for c in &mut self.computers {
            c.now = ms(t);
        }
    }
    fn home(&mut self, id: usize) {
        let c = &mut self.computers[id];
        let o = c.source.request_local(c.now);
        let frames = c.effects(o);
        self.enqueue(id, frames);
    }
    fn input(&mut self, id: usize, event: NormalizedInput) {
        let frames = self.computers[id].input(event);
        self.enqueue(id, frames);
    }
    fn end(
        &mut self,
        reason: monhop_transport::session::SessionFailure,
    ) -> monhop_transport::session::SessionFailure {
        for c in &mut self.computers {
            c.gate.open_injection(0);
            let frame = Frame::new(
                c.receiver.epoch(),
                u64::MAX,
                Message::Disconnect(monhop_protocol::DisconnectCode::Requested),
            );
            let stopped = c.source.on_remote_frame(&frame, c.now);
            assert!(stopped.failure.is_some());
            c.capture.clear();
            c.receiver.stop(
                monhop_transport::session_receiver::ReceiverFailure::PeerStopped,
                &mut c.destination,
            );
            assert!(!c.receiver.cleanup_pending());
            assert!(!c.gate.injected_held());
            c.floor.reset();
        }
        self.pipe.clear();
        reason
    }
    fn pump(&mut self) {
        for _ in 0..100 {
            if let Some((to, frame)) = self.pipe.pop_front() {
                let frames = self.computers[to].receive(frame);
                self.enqueue(to, frames);
            } else {
                let mut any = false;
                for id in 0..2 {
                    any |= !self.computers[id].capture.is_empty();
                    let frames = self.computers[id].drain();
                    self.enqueue(id, frames);
                }
                if !any {
                    return;
                }
            }
        }
        panic!("pipe failed to settle");
    }
    fn take_back(&mut self, id: usize) {
        use monhop_core::capture::{CaptureEvent, CaptureStop, capture_channel};
        let c = &mut self.computers[id];
        let mut physical = monhop_core::capture_physical::PhysicalCapture::new(c.now)
            .with_take_back(c.gate.clone());
        let stop = CaptureStop::default();
        let (mut producer, _consumer) = capture_channel(stop.clone());
        physical.process(
            CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 },
            false,
            c.now,
            &mut producer,
            &stop,
        );
        let message = c.receiver.take_back(&mut c.destination).unwrap().unwrap();
        let frame = c.response(message);
        self.enqueue(id, vec![frame]);
    }
}
#[test]
fn lower_device_wins_tie_in_both_arrival_orders() {
    for reverse in [false, true] {
        let mut p = Pair::new();
        p.cross(0);
        p.cross(1);
        if reverse {
            p.pipe.make_contiguous().reverse();
        }
        p.pump();
        assert_eq!(p.computers[0].floor.snapshot().state, FloorState::Sending);
        assert_eq!(p.computers[1].floor.snapshot().state, FloorState::Receiving);
        assert_eq!(p.computers[1].source.mode(), SourceMode::Local);
    }
}
#[test]
fn declined_epoch_is_consumed() {
    let mut p = Pair::new();
    p.cross(0);
    p.cross(1);
    p.pump();
    assert_eq!(p.computers[0].receiver.epoch().get(), 11);
}
#[test]
fn take_back_in_remote_returns_home() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.take_back(1);
    p.pump();
    assert_eq!(p.computers[0].source.mode(), SourceMode::Local);
    assert!(
        p.computers
            .iter()
            .all(|c| c.floor.snapshot().state == FloorState::Free)
    );
}
#[test]
fn idle_half_hold_leaves_active_floor_untouched() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let c = &mut p.computers[0];
    let floor = c.floor.snapshot();
    c.receiver.tick(ms(500), &mut c.destination).unwrap();
    assert_eq!(c.floor.snapshot(), floor);
}
#[test]
fn stale_generation_motion_never_crosses() {
    let mut p = Pair::new();
    let c = &mut p.computers[0];
    c.source.observe_pointer(Point::new(99.0, 50.0), ms(0));
    assert!(
        c.input(NormalizedInput::RelativeMotion(Point::new(5.0, 0.0)))
            .is_empty()
    );
    let record = TaggedInput {
        event: NormalizedInput::RelativeMotion(Point::new(PUSH_THROUGH_DISTANCE, 0.0)),
        routing_revision: 0,
        remote: false,
        floor_generation: 0,
    };
    assert!(c.source.on_captured(record, ms(0)).effects.is_empty());
}
#[test]
fn disabled_direction_is_a_wall_and_declined() {
    let mut c = Coordinator::new(2, false);
    assert!(c.cross().is_empty());
    let frame = Frame::new(
        SessionEpoch::new(11).unwrap(),
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    )
    .with_scope(c.scopes.inbound);
    let frames = c.receive(frame);
    assert!(matches!(
        frames[0].message,
        Message::ActivationDeclined {
            reason: monhop_protocol::DeclineReason::Disabled,
            ..
        }
    ));
}
#[test]
fn scope_mismatch_fails_closed() {
    let c = Coordinator::new(1, true);
    let frame = Frame::new(SessionEpoch::new(10).unwrap(), 3, Message::Ping(1));
    assert!(c.scopes.validate(&frame).is_err());
}

#[test]
fn reverse_request_during_returning_is_busy_then_retried() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.home(0);
    p.step(); // ReleaseAll frees B. Its acknowledgement has not reached A yet.
    assert_eq!(p.computers[0].floor.snapshot().state, FloorState::Returning);
    p.cross(1);
    let ack = p.pipe.pop_front().unwrap();
    p.step(); // B's crossing reaches A before ReleaseAck.
    assert!(matches!(
        p.pipe.front().unwrap().1.message,
        Message::ActivationDeclined {
            reason: monhop_protocol::DeclineReason::Busy,
            ..
        }
    ));
    p.pump();
    p.pipe.push_back(ack);
    p.pump();
    for t in [10, 20, 30, 40] {
        p.at(t);
        p.cross(1);
        p.pump();
        assert_eq!(p.computers[1].source.mode(), SourceMode::Local);
    }
    p.at(50);
    p.cross(1);
    p.pump();
    assert_eq!(p.computers[1].source.mode(), SourceMode::Remote);
}
#[test]
fn seam_jiggle_never_fails_session() {
    for _ in 0..20 {
        reverse_request_during_returning_is_busy_then_retried();
    }
}
#[test]
fn reanchor_allowed_while_sending_and_receiving() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let a = p.computers[0].floor.snapshot();
    let b = p.computers[1].floor.snapshot();
    p.input(0, NormalizedInput::RelativeMotion(Point::new(0.0, 100.0)));
    p.input(
        0,
        NormalizedInput::RelativeMotion(Point::new(0.0, PUSH_THROUGH_DISTANCE)),
    );
    p.pump();
    assert_eq!(p.computers[0].floor.snapshot(), a);
    assert_eq!(p.computers[1].floor.snapshot(), b);
    assert_eq!(
        p.computers[1].receiver.active_display(),
        Some(DisplayId(12))
    );
}
#[test]
fn take_back_during_initial_activation_applies_on_remote() {
    let mut p = Pair::new();
    p.cross(0);
    p.step(); // Native anchor completed, Ack still in flight.
    p.take_back(1);
    p.step(); // Ack requests the capture barrier.
    assert_eq!(
        p.computers[0].source.mode(),
        SourceMode::AwaitRemoteCaptureBarrier
    );
    p.step(); // TakeBack latches before that barrier.
    p.pump();
    assert_eq!(p.computers[0].source.mode(), SourceMode::Local);
}
#[test]
fn take_back_during_reanchor_applies_on_remote() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.input(0, NormalizedInput::RelativeMotion(Point::new(0.0, 100.0)));
    p.pump();
    p.input(
        0,
        NormalizedInput::RelativeMotion(Point::new(0.0, PUSH_THROUGH_DISTANCE)),
    );
    // Capture takes back before the destination consumes the already queued reanchor.
    p.take_back(1);
    let take = p.pipe.pop_back().unwrap();
    p.pipe.push_front(take);
    p.step();
    assert_eq!(
        p.computers[0].source.mode(),
        SourceMode::AwaitActivationAcknowledgement
    );
    p.pump();
    assert_eq!(p.computers[0].source.mode(), SourceMode::Local);
}
#[test]
fn take_back_racing_release_all_is_harmless() {
    for before in [true, false] {
        let mut p = Pair::new();
        p.cross(0);
        p.pump();
        p.home(0);
        if before {
            p.take_back(1);
            p.pump();
        } else {
            use monhop_core::capture::{CaptureEvent, CaptureStop, capture_channel};
            let c = &mut p.computers[1];
            let stop = CaptureStop::new();
            let (mut producer, _consumer) = capture_channel(stop.clone());
            let mut physical = monhop_core::capture_physical::PhysicalCapture::new(ms(0))
                .with_take_back(c.gate.clone());
            physical.process(
                CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 },
                false,
                ms(0),
                &mut producer,
                &stop,
            );
            p.pump();
            let c = &mut p.computers[1];
            assert!(c.receiver.take_back(&mut c.destination).unwrap().is_none());
        }
        assert!(
            p.computers
                .iter()
                .all(|c| c.floor.snapshot().state == FloorState::Free)
        );
    }
}
#[test]
fn yielding_receiver_injects_nothing() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.take_back(1);
    let count = p.computers[1].destination.actions.len();
    let scope = p.computers[1].scopes.inbound;
    let messages = [
        Message::Key(monhop_protocol::Key {
            usage: monhop_core::HidUsage(4),
            is_down: true,
            repeat: false,
            modifiers: monhop_core::ModifierState::default(),
        }),
        Message::Motion(monhop_protocol::Motion::Absolute(Point::new(20.0, 20.0))),
        Message::Button(monhop_protocol::Button {
            button: monhop_core::MouseButton::Left,
            is_down: true,
        }),
    ];
    for (i, message) in messages.into_iter().enumerate() {
        p.computers[1].receive(
            Frame::new(SessionEpoch::new(11).unwrap(), i as u64 + 1, message).with_scope(scope),
        );
    }
    let ack = p.computers[1].receive(
        Frame::new(
            SessionEpoch::new(12).unwrap(),
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(12),
                position: Point::new(20.0, 120.0),
            },
        )
        .with_scope(scope),
    );
    assert!(matches!(
        ack[0].message,
        Message::ActivationAck(DisplayId(12))
    ));
    assert_eq!(p.computers[1].destination.actions.len(), count);
}
#[test]
fn crossings_rearm_from_fresh_poll_after_free() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let old = p.computers[1].floor.snapshot().generation;
    p.home(0);
    p.pump();
    let c = &mut p.computers[1];
    let record = TaggedInput {
        event: NormalizedInput::RelativeMotion(Point::new(5.0, 0.0)),
        routing_revision: 0,
        remote: false,
        floor_generation: old,
    };
    assert!(c.source.on_captured(record, c.now).effects.is_empty());
    assert!(
        c.input(NormalizedInput::AbsoluteMotion(Point::new(110.0, 50.0)))
            .is_empty()
    );
    assert!(
        c.source
            .observe_pointer(Point::new(99.0, 50.0), c.now)
            .effects
            .is_empty()
    );
    assert!(
        !c.input(NormalizedInput::RelativeMotion(Point::new(
            PUSH_THROUGH_DISTANCE,
            0.0
        )))
        .is_empty()
    );
}
#[test]
fn disconnect_mid_transition_releases_both_halves() {
    let mut p = Pair::new();
    p.cross(0);
    p.step();
    p.end(monhop_transport::session::SessionFailure::Wire);
    assert!(
        p.computers
            .iter()
            .all(|c| c.floor.snapshot().state == FloorState::Free && !c.gate.admits_injection())
    );
}
#[test]
fn deliberate_end_while_remote_releases_injected_keys() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.input(
        0,
        NormalizedInput::Key {
            usage: monhop_core::HidUsage(4),
            pressed: true,
            repeat: false,
            modifiers: monhop_core::ModifierState::default(),
        },
    );
    p.pump();
    assert!(p.computers[1].gate.injected_held());
    p.end(monhop_transport::session::SessionFailure::Revoked);
    assert!(!p.computers[1].destination.keys[4]);
}
#[test]
fn drag_in_progress_take_back_withholds_until_release() {
    use monhop_core::capture::{CaptureEvent, CaptureStop, capture_channel};
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.input(
        0,
        NormalizedInput::Button {
            button: monhop_core::MouseButton::Left,
            pressed: true,
        },
    );
    p.pump();
    let c = &mut p.computers[1];
    let stop = CaptureStop::new();
    let (mut producer, mut consumer) = capture_channel(stop.clone());
    let mut physical =
        monhop_core::capture_physical::PhysicalCapture::new(ms(0)).with_take_back(c.gate.clone());
    let motion = CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 };
    assert!(physical.process(motion, false, ms(0), &mut producer, &stop));
    assert!(c.gate.injected_held());
    assert!(!c.gate.admits_injection());
    assert!(!consumer.try_pop_tagged().unwrap().unwrap().remote);
    assert!(matches!(
        c.receiver.take_back(&mut c.destination).unwrap(),
        Some(Message::TakeBack)
    ));
    assert!(!c.destination.buttons[0]);
    assert!(!c.gate.injected_held());
    assert!(!physical.process(motion, false, ms(1), &mut producer, &stop));
}
#[test]
fn emergency_escape_on_controlled_computer_ends_session() {
    use monhop_core::{
        HidUsage, ModifierState,
        capture::{CaptureEvent, CaptureStop, StopReason, capture_channel},
    };
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let c = &mut p.computers[1];
    let stop = CaptureStop::new();
    let (mut producer, _consumer) = capture_channel(stop.clone());
    let mut physical =
        monhop_core::capture_physical::PhysicalCapture::new(ms(0)).with_take_back(c.gate.clone());
    for (usage, modifiers) in [(0xe0, 1), (0xe4, 17), (0x29, 17)] {
        physical.process(
            CaptureEvent::Key {
                usage: HidUsage(usage),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(modifiers),
            },
            false,
            ms(0),
            &mut producer,
            &stop,
        );
    }
    physical.tick(ms(2000), &stop);
    assert_eq!(stop.reason(), Some(StopReason::EmergencyEscape));
    p.end(monhop_transport::session::SessionFailure::Revoked);
}
#[test]
fn local_display_change_while_remote_ends_with_local_displays_changed() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let expected = displays(1);
    let changed = displays(3);
    let failure = monhop_transport::session::check_local_displays(&expected, &changed).unwrap_err();
    assert_eq!(
        p.end(failure),
        monhop_transport::session::SessionFailure::LocalDisplaysChanged
    );
}

#[test]
fn control_mismatch_is_symmetric() {
    use monhop_protocol::{
        Capabilities, ControlPermissions, Hello, PROTOCOL_VERSION, SessionPurpose, SessionSetup,
    };
    use monhop_transport::{
        crypto::{DeviceIdentity, VerifiedPeer},
        session_handshake::{
            HandshakeConfig, HandshakeError, device_id_from_fingerprint, validate_peer_handshake,
        },
    };
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let pin = |id: &DeviceIdentity| {
        VerifiedPeer::from_certificate_der(id.certificate_der(), &id.fingerprint().full_hex())
            .unwrap()
    };
    let ap = pin(&a);
    let bp = pin(&b);
    let displays = displays(1);
    let capabilities =
        Capabilities::new(Capabilities::DISPLAY_TOPOLOGY | Capabilities::RELATIVE_MOTION).unwrap();
    let controls = [
        ControlPermissions::BOTH,
        ControlPermissions {
            lower_controls_higher: true,
            higher_controls_lower: false,
        },
    ];
    for (local, peer, pin, control, other) in [
        (&a, &b, &bp, controls[0], controls[1]),
        (&b, &a, &ap, controls[1], controls[0]),
    ] {
        let config = HandshakeConfig::new(
            local,
            pin,
            Platform::MacOs,
            Platform::MacOs,
            capabilities,
            capabilities,
            &displays,
            control,
            SessionPurpose::Share,
        )
        .unwrap();
        let epoch = SessionEpoch::new(10).unwrap();
        let frames = [
            Frame::new(
                epoch,
                0,
                Message::Hello(Hello {
                    device_id: device_id_from_fingerprint(peer.fingerprint()),
                    platform: Platform::MacOs,
                    protocol_version: PROTOCOL_VERSION,
                    capabilities,
                }),
            ),
            Frame::new(epoch, 1, Message::DisplayTopology(displays.clone())),
            Frame::new(
                epoch,
                2,
                Message::SessionSetup(SessionSetup {
                    purpose: SessionPurpose::Share,
                    control: other,
                }),
            ),
        ];
        assert_eq!(
            validate_peer_handshake(&config, peer.fingerprint(), epoch, &frames),
            Err(HandshakeError::ControlMismatch)
        );
    }
}

#[test]
fn a_pause_in_seam_pressure_restarts_the_decline_delay() {
    let mut p = Pair::new();
    p.cross(0);
    p.cross(1);
    p.pump();
    p.home(0);
    p.pump();
    p.at(10);
    p.cross(1);
    p.pump();
    p.at(20);
    p.computers[1].input(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0)));
    for t in [30, 40, 50, 60, 70] {
        p.at(t);
        p.cross(1);
        p.pump();
        assert_eq!(p.computers[1].source.mode(), SourceMode::Local);
    }
    p.at(80);
    p.cross(1);
    p.pump();
    assert_eq!(p.computers[1].source.mode(), SourceMode::Remote);
}

#[test]
fn outbound_hold_on_the_controlled_computer_does_not_release_inbound_floor() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    let c = &mut p.computers[1];
    let before = c.floor.snapshot();
    let outcome = c.source.tick(ms(500));
    assert_eq!(outcome.failure, None);
    assert_eq!(c.floor.snapshot(), before);
}

#[test]
fn yielding_still_rejects_invalid_pressed_state() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.take_back(1);
    let c = &mut p.computers[1];
    let frame = Frame::new(
        c.receiver.epoch(),
        1,
        Message::Key(monhop_protocol::Key {
            usage: monhop_core::HidUsage(4),
            is_down: false,
            repeat: false,
            modifiers: monhop_core::ModifierState::default(),
        }),
    );
    assert_eq!(
        c.receiver.receive(&frame, c.now, &mut c.destination),
        Err(monhop_transport::session_receiver::ReceiverFailure::InvalidPressedState)
    );
    assert!(!c.gate.admits_injection());
}

#[test]
fn every_seam_is_a_wall_until_capture_can_suppress() {
    let mut c = Coordinator::new(1, true);
    c.source.set_capture_ready(false);
    assert!(c.cross().is_empty());
    assert_eq!(c.floor.snapshot().state, FloorState::Free);
    c.source.set_capture_ready(true);
    assert!(!c.cross().is_empty());
}

#[test]
fn a_capture_that_cannot_suppress_yet_unwinds_the_crossing_instead_of_failing() {
    let mut p = Pair::new();
    p.cross(0);
    p.step(); // The peer activates and acknowledges.
    let (to, ack) = p.pipe.pop_front().unwrap();
    assert_eq!(to, 0);
    let c = &mut p.computers[0];
    let acknowledged = c.source.on_remote_frame(&ack, c.now);
    assert_eq!(acknowledged.failure, None);
    let request = acknowledged
        .effects
        .iter()
        .find_map(|effect| match effect {
            SourceEffect::ActivateRemote { request } => Some(*request),
            _ => None,
        })
        .expect("the acknowledgement asks capture to go remote");
    let unwound = c.source.refuse_capture_activation(request, c.now);
    let frames = c.effects(unwound);
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame.message, Message::ReleaseAll))
    );
    p.enqueue(0, frames);
    p.pump();
    assert_eq!(p.computers[0].source.mode(), SourceMode::Local);
    assert_eq!(p.computers[1].receiver.active_display(), None);
    assert!(
        p.computers
            .iter()
            .all(|c| c.floor.snapshot().state == FloorState::Free)
    );
    // The seam stays a wall until the runtime reports capture ready again.
    assert!(p.computers[0].cross().is_empty());
}

#[test]
fn a_take_back_delayed_past_a_reanchor_and_a_hold_is_ignored() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.input(0, NormalizedInput::RelativeMotion(Point::new(0.0, 100.0)));
    p.pump();
    p.input(
        0,
        NormalizedInput::RelativeMotion(Point::new(0.0, PUSH_THROUGH_DISTANCE)),
    );
    // The peer takes back in the old epoch; a stall delays its TakeBack past the reanchor.
    p.take_back(1);
    let (to, late) = p.pipe.pop_back().unwrap();
    assert_eq!(to, 0);
    assert!(matches!(late.message, Message::TakeBack));
    p.at(500);
    let c = &mut p.computers[0];
    let held = c.source.tick(c.now);
    c.effects(held);
    assert!(c.source.held_since().is_some());
    assert_eq!(c.source.mode(), SourceMode::AwaitLocalCaptureBarrier);
    let outcome = c.source.on_remote_frame(&late, c.now);
    assert_eq!(outcome.failure, None);
    assert_eq!(c.source.mode(), SourceMode::AwaitLocalCaptureBarrier);
}

#[test]
fn crossing_straight_back_after_an_own_return_needs_no_fresh_poll() {
    let mut p = Pair::new();
    p.cross(0);
    p.pump();
    p.home(0);
    p.pump();
    let c = &mut p.computers[0];
    assert_eq!(c.source.mode(), SourceMode::Local);
    assert_eq!(c.floor.snapshot().state, FloorState::Free);
    assert!(
        !c.input(NormalizedInput::RelativeMotion(Point::new(
            PUSH_THROUGH_DISTANCE,
            0.0
        )))
        .is_empty()
    );
}
