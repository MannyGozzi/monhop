use std::time::Duration;

use monhop_core::{
    DeviceId, Display, DisplayId, Edge, EdgeLink, HidUsage, LogicalSize, Machine, ModifierState,
    MouseButton, NativeSize, NormalizedSpan, Platform, Point, Topology,
};
use monhop_protocol::{
    DisconnectCode, DisplayDescription, DisplayTopology, Frame, Key, Message, Motion, SessionEpoch,
};
use monhop_transport::session_health::{
    HOLD_LIMIT, HealthError, PEER_LIVENESS, RETREAT_AFTER, SUPPRESSION_LEASE_CAP,
};
use monhop_transport::session_receiver::{
    DestinationAction, DestinationFailure, InputDestination, InputReceiver, ReceiverFailure,
};
use monhop_transport::session_source::{
    NormalizedInput, SourceController, SourceEffect, SourceFailure, SourceMode, SourceOutcome,
    TaggedInput,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn device(value: u8) -> DeviceId {
    DeviceId([value; 16])
}

fn topology() -> Topology {
    let local = Display::new(
        DisplayId(1),
        device(1),
        "local".into(),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(0.0, 0.0),
        1.0,
        None,
        true,
    );
    let local_second = Display::new(
        DisplayId(4),
        device(1),
        "local-second".into(),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(100.0, 0.0),
        1.0,
        None,
        false,
    );
    let remote = Display::new(
        DisplayId(2),
        device(2),
        "remote".into(),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(0.0, 0.0),
        1.0,
        None,
        true,
    );
    let remote_second = Display::new(
        DisplayId(3),
        device(2),
        "remote-second".into(),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(0.0, 0.0),
        1.0,
        None,
        false,
    );
    Topology::new(
        vec![
            Machine::new(device(1), Platform::Windows),
            Machine::new(device(2), Platform::MacOs),
        ],
        vec![local, local_second, remote, remote_second],
        vec![
            EdgeLink::new(
                DisplayId(1),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(2),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
            EdgeLink::new(
                DisplayId(4),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(2),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
            EdgeLink::new(
                DisplayId(2),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(1),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
            EdgeLink::new(
                DisplayId(2),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(3),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

fn source() -> SourceController {
    source_for(device(1), DisplayId(1))
}

fn source_for(local_machine: DeviceId, local_display: DisplayId) -> SourceController {
    SourceController::new(
        topology(),
        local_machine,
        local_display,
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap()
}

fn mac_source_controller() -> SourceController {
    let fixture = topology();
    let windows = fixture.display(DisplayId(1)).unwrap().clone();
    let mac = fixture.display(DisplayId(2)).unwrap().clone();
    let topology = Topology::new(
        vec![
            Machine::new(device(1), Platform::Windows),
            Machine::new(device(2), Platform::MacOs),
        ],
        vec![windows, mac],
        vec![
            EdgeLink::new(
                DisplayId(1),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(2),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
            EdgeLink::new(
                DisplayId(2),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(1),
                Edge::Right,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap(),
        ],
    )
    .unwrap();
    SourceController::new(
        topology,
        device(2),
        DisplayId(2),
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap()
}

fn destination_topology(displays: &[DisplayId]) -> DisplayTopology {
    DisplayTopology::new(
        displays
            .iter()
            .map(|id| match id {
                DisplayId(1) => DisplayDescription {
                    id: *id,
                    name: "windows-primary".into(),
                    native_width: 100,
                    native_height: 100,
                    logical_origin: Point::new(0.0, 0.0),
                    logical_size: Point::new(100.0, 100.0),
                    scale_factor: 1.0,
                    is_primary: true,
                    monitor: None,
                },
                DisplayId(2) => DisplayDescription {
                    id: *id,
                    name: "mac-primary".into(),
                    native_width: 100,
                    native_height: 100,
                    logical_origin: Point::new(0.0, 0.0),
                    logical_size: Point::new(100.0, 100.0),
                    scale_factor: 1.0,
                    is_primary: true,
                    monitor: None,
                },
                DisplayId(3) => DisplayDescription {
                    id: *id,
                    name: "mac-second".into(),
                    native_width: 100,
                    native_height: 100,
                    logical_origin: Point::new(0.0, 0.0),
                    logical_size: Point::new(100.0, 100.0),
                    scale_factor: 1.0,
                    is_primary: false,
                    monitor: None,
                },
                DisplayId(4) => DisplayDescription {
                    id: *id,
                    name: "windows-second".into(),
                    native_width: 100,
                    native_height: 100,
                    logical_origin: Point::new(100.0, 0.0),
                    logical_size: Point::new(100.0, 100.0),
                    scale_factor: 1.0,
                    is_primary: false,
                    monitor: None,
                },
                _ => unreachable!("fixture displays are fixed"),
            })
            .collect(),
    )
    .unwrap()
}

#[derive(Debug, PartialEq)]
enum RecordedAction {
    Move(Point),
    Key(HidUsage, bool),
    Button(MouseButton, bool),
    Scroll(f64, f64),
    ReleaseAll,
}

#[derive(Default)]
struct RecordingDestination {
    actions: Vec<RecordedAction>,
    reject_key_down: bool,
}

impl InputDestination for RecordingDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let recorded = match action {
            DestinationAction::MoveTo(point) => RecordedAction::Move(point),
            DestinationAction::Key { usage, pressed } => RecordedAction::Key(usage, pressed),
            DestinationAction::Button { button, pressed } => {
                RecordedAction::Button(button, pressed)
            }
            DestinationAction::Scroll {
                horizontal,
                vertical,
            } => RecordedAction::Scroll(horizontal, vertical),
            DestinationAction::ReleaseAll => RecordedAction::ReleaseAll,
        };
        let rejected = self.reject_key_down && matches!(&recorded, RecordedAction::Key(_, true));
        self.actions.push(recorded);
        rejected.then_some(DestinationFailure).map_or(Ok(()), Err)
    }
}

/// Pumps only actual protocol responses. Native route barriers are the sole simulated boundary.
struct ReceiverBridge {
    receiver: InputReceiver,
    destination: RecordingDestination,
    sent: Vec<Frame>,
    responses: usize,
    next_ticket: u64,
    response_input_epoch: SessionEpoch,
    response_input_sequence: u64,
    response_control_sequence: u64,
}

impl ReceiverBridge {
    fn new(displays: DisplayTopology) -> Self {
        Self {
            receiver: InputReceiver::new(displays, SessionEpoch::new(3).unwrap(), Duration::ZERO),
            destination: RecordingDestination::default(),
            sent: Vec::new(),
            responses: 0,
            next_ticket: 1,
            response_input_epoch: SessionEpoch::new(3).unwrap(),
            response_input_sequence: 0,
            response_control_sequence: 3,
        }
    }

    fn pump(
        &mut self,
        source: &mut SourceController,
        outcome: SourceOutcome,
        now: Duration,
    ) -> Result<(), ReceiverFailure> {
        assert_eq!(outcome.failure, None);
        for effect in outcome.effects.iter().cloned() {
            match effect {
                SourceEffect::RemoteFrame(frame) => {
                    self.sent.push(frame.clone());
                    if let Some(message) =
                        self.receiver.receive(&frame, now, &mut self.destination)?
                    {
                        self.responses += 1;
                        let response = self.response_frame(message);
                        let follow_up = source.on_remote_frame(&response, now);
                        self.pump(source, follow_up, now)?;
                    }
                }
                SourceEffect::ActivateRemote { request } => {
                    self.bind_and_cross(source, request, true, now)?;
                }
                SourceEffect::RestoreLocalAt { request, .. } => {
                    self.bind_and_cross(source, request, false, now)?;
                }
            }
        }
        Ok(())
    }

    fn bind_and_cross(
        &mut self,
        source: &mut SourceController,
        request: monhop_transport::session_source::RouteRequest,
        remote: bool,
        now: Duration,
    ) -> Result<(), ReceiverFailure> {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        assert_eq!(
            source.bind_capture_route(request, ticket, now).failure,
            None
        );
        let crossed = source.on_captured(
            routed(
                NormalizedInput::RouteChanged {
                    remote,
                    revision: ticket,
                },
                remote,
                ticket,
            ),
            now,
        );
        self.pump(source, crossed, now)
    }

    fn response_frame(&mut self, message: Message) -> Frame {
        if matches!(&message, Message::Ping(_) | Message::Pong(_)) {
            let sequence = self.response_control_sequence;
            self.response_control_sequence += 1;
            return Frame::new(self.receiver.control_epoch(), sequence, message);
        }
        let epoch = self.receiver.epoch();
        if epoch != self.response_input_epoch {
            self.response_input_epoch = epoch;
            self.response_input_sequence = 0;
        }
        let sequence = self.response_input_sequence;
        self.response_input_sequence += 1;
        Frame::new(epoch, sequence, message)
    }
}

fn local(event: NormalizedInput) -> TaggedInput {
    TaggedInput {
        event,
        routing_revision: 0,
        remote: false,
    }
}

fn routed(event: NormalizedInput, remote: bool, revision: u64) -> TaggedInput {
    TaggedInput {
        event,
        routing_revision: revision,
        remote,
    }
}

fn activation_ack() -> Frame {
    Frame::new(
        SessionEpoch::new(4).unwrap(),
        0,
        Message::ActivationAck(DisplayId(2)),
    )
}

fn activation_ack_for(epoch: u64, sequence: u64, display: DisplayId) -> Frame {
    Frame::new(
        SessionEpoch::new(epoch).unwrap(),
        sequence,
        Message::ActivationAck(display),
    )
}

fn assert_frame(effect: &SourceEffect, epoch: u64, sequence: u64, message: Message) {
    assert_eq!(
        effect,
        &SourceEffect::RemoteFrame(Frame::new(
            SessionEpoch::new(epoch).unwrap(),
            sequence,
            message,
        ))
    );
}

fn route_request(
    outcome: &monhop_transport::session_source::SourceOutcome,
) -> monhop_transport::session_source::RouteRequest {
    match outcome.effects.iter().find_map(|effect| match effect {
        SourceEffect::ActivateRemote { request } | SourceEffect::RestoreLocalAt { request, .. } => {
            Some(*request)
        }
        _ => None,
    }) {
        Some(request) => request,
        None => panic!("source must request native route control"),
    }
}

fn activate_remote(source: &mut SourceController) {
    assert!(
        source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(0)
            )
            .effects
            .is_empty()
    );
    let edge = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    assert_eq!(acknowledged.failure, None);
    assert_eq!(acknowledged.effects.iter().count(), 1);
    assert!(matches!(
        acknowledged.effects.iter().next(),
        Some(SourceEffect::ActivateRemote { .. })
    ));
    assert_eq!(
        source
            .bind_capture_route(route_request(&acknowledged), 1, ms(0))
            .failure,
        None
    );
    let barrier = source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: true,
                revision: 1,
            },
            true,
            1,
        ),
        ms(0),
    );
    assert_eq!(barrier.failure, None);
    assert_eq!(source.mode(), SourceMode::Remote);
}

#[test]
fn edge_activation_uses_input_epoch_while_health_stays_on_base_epoch() {
    let mut source = source();
    let ping = source.tick(ms(0));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 3, Message::Ping(1));

    assert!(
        source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(1)
            )
            .effects
            .is_empty()
    );
    let edge = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(1),
    );
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );

    let pong = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(3).unwrap(), 3, Message::Pong(1)),
        ms(2),
    );
    assert_eq!(pong.failure, None);
    let activation = source.on_remote_frame(&activation_ack(), ms(3));
    assert_eq!(activation.failure, None);
    assert_eq!(source.mode(), SourceMode::AwaitRemoteCaptureBarrier);
}

#[test]
fn control_sequence_starts_after_the_handshake() {
    let mut source = source();
    let stale = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(3).unwrap(), 2, Message::Ping(1)),
        ms(0),
    );
    assert_eq!(stale.failure, None);
    assert!(stale.effects.is_empty());
}

#[test]
fn suppression_budget_only_refreshes_after_a_valid_pong() {
    let mut source = source();
    assert_eq!(source.suppression_budget(ms(0)), Ok(SUPPRESSION_LEASE_CAP));
    source.tick(ms(0));
    // The lease stays capped until the liveness window has fewer than the cap's millis left.
    let shrinking = PEER_LIVENESS - SUPPRESSION_LEASE_CAP + ms(30);
    assert_eq!(source.suppression_budget(ms(30)), Ok(SUPPRESSION_LEASE_CAP));
    assert_eq!(
        source.suppression_budget(shrinking),
        Ok(SUPPRESSION_LEASE_CAP - ms(30))
    );
    let pong = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(3).unwrap(), 3, Message::Pong(1)),
        shrinking,
    );
    assert_eq!(pong.failure, None);
    assert_eq!(
        source.suppression_budget(shrinking),
        Ok(SUPPRESSION_LEASE_CAP)
    );
    assert_eq!(
        source.suppression_budget(shrinking + PEER_LIVENESS),
        Err(SourceFailure::PeerHealth(HealthError::DeadlineExpired))
    );
}

#[test]
fn local_absolute_resolves_nonprimary_before_edge_intent() {
    let mut source = source();
    let first = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(199.0, 50.0))),
        ms(0),
    );
    assert!(first.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));

    let edge = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

#[test]
fn ordinary_local_display_motion_stays_local_without_synthetic_effects() {
    let mut source = source();
    assert!(
        source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let second = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
        ms(1),
    );
    assert!(second.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    assert_eq!(source.capture_route(), (false, 0));
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
}

#[test]
fn captured_events_keep_their_route_until_the_remote_barrier() {
    let mut source = source();
    assert!(
        source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(0)
            )
            .effects
            .is_empty()
    );
    source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    source.bind_capture_route(route_request(&acknowledged), 1, ms(0));

    let held_local = source.on_captured(
        local(NormalizedInput::Key {
            usage: HidUsage(0x04),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(0),
        }),
        ms(0),
    );
    assert!(held_local.effects.is_empty());
    let barrier = source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: true,
                revision: 1,
            },
            true,
            1,
        ),
        ms(0),
    );
    assert!(barrier.effects.is_empty());

    let old_release = source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: false,
                repeat: false,
                modifiers: ModifierState(0),
            },
            true,
            1,
        ),
        ms(1),
    );
    assert!(old_release.effects.is_empty());
    let fresh_press = source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(0),
            },
            true,
            1,
        ),
        ms(2),
    );
    assert_frame(
        fresh_press.effects.iter().next().unwrap(),
        4,
        1,
        Message::Key(monhop_protocol::Key {
            usage: HidUsage(0x04),
            is_down: true,
            repeat: false,
            modifiers: ModifierState(0),
        }),
    );
}

#[test]
fn route_barrier_uses_the_native_ticket_bound_after_submission() {
    let mut source = source();
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    let request = route_request(&acknowledged);
    assert_ne!(request.get(), 9);
    assert_eq!(source.bind_capture_route(request, 9, ms(0)).failure, None);
    assert_eq!(
        source
            .on_captured(
                routed(
                    NormalizedInput::RouteChanged {
                        remote: true,
                        revision: 9,
                    },
                    true,
                    9,
                ),
                ms(1),
            )
            .failure,
        None
    );
    assert_eq!(source.capture_route(), (true, 9));
    assert_eq!(source.mode(), SourceMode::Remote);
}

#[test]
fn remote_relative_motion_and_fractional_scroll_keep_logical_units() {
    let mut source = source();
    activate_remote(&mut source);
    let target = source.motion_target().expect("stable remote target");
    assert_eq!(target.target.display, DisplayId(2));
    assert_eq!(target.platform, Platform::MacOs);
    assert_eq!(target.scale_factor, 1.0);
    let motion = source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(2.5, -1.25)),
            true,
            1,
        ),
        ms(1),
    );
    assert_frame(
        motion.effects.iter().next().unwrap(),
        4,
        1,
        Message::Motion(Motion::Absolute(Point::new(3.5, 48.75))),
    );
    let scroll = source.on_captured(
        routed(
            NormalizedInput::Scroll {
                horizontal: 0.25,
                vertical: -0.75,
            },
            true,
            1,
        ),
        ms(2),
    );
    assert_frame(
        scroll.effects.iter().next().unwrap(),
        4,
        2,
        Message::Scroll(monhop_protocol::Scroll {
            horizontal: 0.25,
            vertical: -0.75,
        }),
    );
}

#[test]
fn remote_edges_return_locally_and_reanchor_between_remote_displays() {
    let mut returning = source();
    activate_remote(&mut returning);
    let release = returning.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(-3.0, 0.0)),
            true,
            1,
        ),
        ms(1),
    );
    assert_frame(
        release.effects.iter().next().unwrap(),
        4,
        1,
        Message::ReleaseAll,
    );
    assert_eq!(
        returning.mode(),
        SourceMode::AwaitRemoteReleaseAcknowledgement
    );
    let restore = returning.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::ReleaseAck),
        ms(2),
    );
    assert!(matches!(
        restore.effects.iter().next(),
        Some(SourceEffect::RestoreLocalAt {
            display: DisplayId(1),
            position,
            ..
        }) if *position == Point::new(99.0, 50.0)
    ));
    returning.bind_capture_route(route_request(&restore), 2, ms(2));
    assert_eq!(
        returning
            .on_captured(
                routed(
                    NormalizedInput::RouteChanged {
                        remote: false,
                        revision: 2,
                    },
                    false,
                    2,
                ),
                ms(3),
            )
            .failure,
        None
    );
    assert_eq!(returning.mode(), SourceMode::Local);

    let mut remote_to_remote = source();
    activate_remote(&mut remote_to_remote);
    remote_to_remote.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0xe0),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(ModifierState::LEFT_CONTROL),
            },
            true,
            1,
        ),
        ms(1),
    );
    remote_to_remote.on_captured(
        routed(
            NormalizedInput::Button {
                button: MouseButton::Left,
                pressed: true,
            },
            true,
            1,
        ),
        ms(2),
    );
    remote_to_remote.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(98.0, 0.0)),
            true,
            1,
        ),
        ms(3),
    );
    let handoff = remote_to_remote.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(3.0, 0.0)),
            true,
            1,
        ),
        ms(4),
    );
    assert_frame(
        handoff.effects.iter().next().unwrap(),
        5,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(3),
            position: Point::new(1.0, 50.0),
        },
    );
    let remote_ack =
        remote_to_remote.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), ms(5));
    assert!(remote_ack.effects.is_empty());
    assert_eq!(remote_to_remote.mode(), SourceMode::Remote);
    let control_release = remote_to_remote.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0xe0),
                pressed: false,
                repeat: false,
                modifiers: ModifierState(0),
            },
            true,
            1,
        ),
        ms(6),
    );
    assert_frame(
        control_release.effects.iter().next().unwrap(),
        5,
        1,
        Message::Key(monhop_protocol::Key {
            usage: HidUsage(0xe0),
            is_down: false,
            repeat: false,
            modifiers: ModifierState(0),
        }),
    );
    let button_release = remote_to_remote.on_captured(
        routed(
            NormalizedInput::Button {
                button: MouseButton::Left,
                pressed: false,
            },
            true,
            1,
        ),
        ms(7),
    );
    assert_frame(
        button_release.effects.iter().next().unwrap(),
        5,
        2,
        Message::Button(monhop_protocol::Button {
            button: MouseButton::Left,
            is_down: false,
        }),
    );
    let resumed = remote_to_remote.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(1.0, 0.0)),
            true,
            1,
        ),
        ms(8),
    );
    assert_frame(
        resumed.effects.iter().next().unwrap(),
        5,
        3,
        Message::Motion(Motion::Absolute(Point::new(2.0, 50.0))),
    );
}

#[test]
fn remote_motion_carries_the_tracked_position_so_the_far_edge_is_never_overshot() {
    let mut source = source();
    let mut bridge = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    bridge.pump(&mut source, edge, ms(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    // From y = 1, 76 tenth-point steps leave an anchor whose relative echo rounds past y = 100
    // on the receiver; the absolute position lands exactly on the clamped, unlinked bottom edge.
    let mut deltas = vec![-49.0];
    deltas.extend(std::iter::repeat_n(0.1, 76));
    deltas.push(1_000.0);
    for (step, delta) in (1_u64..).zip(deltas) {
        let motion = source.on_captured(
            routed(
                NormalizedInput::RelativeMotion(Point::new(0.0, delta)),
                true,
                1,
            ),
            ms(step),
        );
        bridge.pump(&mut source, motion, ms(step)).unwrap();
    }
    bridge.receiver.flush(&mut bridge.destination).unwrap();
    let far_edge = 100.0_f64.next_down();
    let motions: Vec<_> = bridge
        .sent
        .iter()
        .filter_map(|frame| match &frame.message {
            Message::Motion(motion) => Some(*motion),
            _ => None,
        })
        .collect();
    assert_eq!(motions.len(), 78);
    assert!(
        motions
            .iter()
            .all(|motion| matches!(motion, Motion::Absolute(_)))
    );
    assert_eq!(
        motions.last(),
        Some(&Motion::Absolute(Point::new(1.0, far_edge)))
    );
    assert_eq!(
        bridge.destination.actions.last(),
        Some(&RecordedAction::Move(Point::new(1.0, far_edge)))
    );
    let stalled = source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(0.0, 1.0)),
            true,
            1,
        ),
        ms(78),
    );
    assert!(stalled.effects.is_empty());
}

#[test]
fn returning_local_waits_for_release_ack_before_one_native_restore_command() {
    let mut source = source();
    activate_remote(&mut source);
    let control = source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0xe0),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(ModifierState::LEFT_CONTROL),
            },
            true,
            1,
        ),
        ms(1),
    );
    assert_frame(
        control.effects.iter().next().unwrap(),
        4,
        1,
        Message::Key(monhop_protocol::Key {
            usage: HidUsage(0xe0),
            is_down: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_CONTROL),
        }),
    );
    source.on_captured(
        routed(
            NormalizedInput::Button {
                button: MouseButton::Left,
                pressed: true,
            },
            true,
            1,
        ),
        ms(2),
    );
    source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(ModifierState::LEFT_CONTROL),
            },
            true,
            1,
        ),
        ms(3),
    );

    let release = source.request_local(ms(4));
    assert_frame(
        release.effects.iter().next().unwrap(),
        4,
        4,
        Message::ReleaseAll,
    );
    assert_eq!(source.mode(), SourceMode::AwaitRemoteReleaseAcknowledgement);
    let ack = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::ReleaseAck),
        ms(5),
    );
    assert_eq!(ack.failure, None);
    assert!(matches!(
        ack.effects.iter().next(),
        Some(SourceEffect::RestoreLocalAt {
            display: DisplayId(1),
            position,
            ..
        }) if *position == Point::new(99.0, 50.0)
    ));
    assert_eq!(ack.effects.iter().count(), 1);
    source.bind_capture_route(route_request(&ack), 2, ms(5));

    let local_barrier = source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: 2,
            },
            false,
            2,
        ),
        ms(6),
    );
    assert!(local_barrier.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
}

#[test]
fn stale_ack_and_mismatched_capture_route_fail_closed() {
    let mut controller = source();
    controller.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    controller.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    let stale = controller.on_remote_frame(
        &Frame::new(
            SessionEpoch::new(3).unwrap(),
            0,
            Message::ActivationAck(DisplayId(2)),
        ),
        ms(1),
    );
    assert_eq!(
        stale.failure,
        Some(SourceFailure::InvalidActivationAcknowledgement)
    );
    assert_eq!(controller.mode(), SourceMode::Failed);

    let mut other = source();
    activate_remote(&mut other);
    let mismatch = other.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(1.0, 0.0)),
            false,
            1,
        ),
        ms(1),
    );
    assert_eq!(mismatch.failure, Some(SourceFailure::RouteMismatch));
    assert_eq!(other.mode(), SourceMode::Failed);
}

#[test]
fn a_silent_peer_makes_a_remote_source_retreat_before_its_lease_can_expire() {
    let mut source = source();
    activate_remote(&mut source);
    let ping = source.tick(ms(0));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 3, Message::Ping(1));
    assert!(source.tick(RETREAT_AFTER - ms(1)).failure.is_none());
    assert!(source.held_since().is_none());
    assert_eq!(
        source.suppression_budget(RETREAT_AFTER - ms(1)),
        Ok(SUPPRESSION_LEASE_CAP)
    );
    let retreat = source.tick(RETREAT_AFTER);
    assert_eq!(retreat.failure, None);
    assert_eq!(source.held_since(), Some(RETREAT_AFTER));
    assert!(retreat.effects.iter().any(|effect| matches!(
        effect,
        SourceEffect::RemoteFrame(Frame {
            message: Message::ReleaseAll,
            ..
        })
    )));
    assert!(retreat.effects.iter().any(|effect| matches!(
        effect,
        SourceEffect::RestoreLocalAt {
            display: DisplayId(1),
            ..
        }
    )));
    // The lease is never renewed while held, and the native return completes as usual.
    assert_eq!(
        source.suppression_budget(RETREAT_AFTER),
        Err(SourceFailure::PeerHealth(HealthError::DeadlineExpired))
    );
    let request = route_request(&retreat);
    assert!(
        source
            .bind_capture_route(request, 2, RETREAT_AFTER)
            .failure
            .is_none()
    );
    let returned = source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: 2,
            },
            false,
            2,
        ),
        RETREAT_AFTER,
    );
    assert!(returned.failure.is_none());
    assert_eq!(source.mode(), SourceMode::Local);
    // Past the deadline the hold continues instead of failing, still challenging the peer.
    let past_deadline = source.tick(PEER_LIVENESS + ms(40));
    assert_eq!(past_deadline.failure, None);
    assert!(past_deadline.effects.iter().any(|effect| matches!(
        effect,
        SourceEffect::RemoteFrame(Frame {
            message: Message::Ping(_),
            ..
        })
    )));
}

#[test]
fn a_hold_resumes_only_after_the_barrier_is_acknowledged_and_a_fresh_reply_arrives() {
    let mut source = source();
    activate_remote(&mut source);
    assert_frame(
        source.tick(ms(0)).effects.iter().next().unwrap(),
        3,
        3,
        Message::Ping(1),
    );
    let retreat = source.tick(RETREAT_AFTER);
    let barrier = retreat
        .effects
        .iter()
        .find_map(|effect| match effect {
            SourceEffect::RemoteFrame(frame) if frame.message == Message::ReleaseAll => {
                Some(frame.clone())
            }
            _ => None,
        })
        .expect("the retreat sends the barrier");
    assert_eq!(barrier.epoch, SessionEpoch::new(4).unwrap());
    source.bind_capture_route(route_request(&retreat), 2, RETREAT_AFTER);
    source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: 2,
            },
            false,
            2,
        ),
        RETREAT_AFTER,
    );
    // A reply to the challenge sent before the stall proves nothing.
    let stale = Frame::new(SessionEpoch::new(3).unwrap(), 3, Message::Pong(1));
    assert!(source.on_remote_frame(&stale, ms(400)).failure.is_none());
    assert!(source.held_since().is_some());
    // The next challenge goes out during the hold; its reply is fresh, but the barrier is still owed.
    let ping = source.tick(ms(410));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 4, Message::Ping(2));
    let fresh = Frame::new(SessionEpoch::new(3).unwrap(), 4, Message::Pong(2));
    assert!(source.on_remote_frame(&fresh, ms(420)).failure.is_none());
    assert!(source.held_since().is_some());
    // Crossing the seam while held keeps input local.
    let wall = source.on_captured(
        routed(
            NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0)),
            false,
            2,
        ),
        ms(425),
    );
    assert_eq!(wall.failure, None);
    assert!(wall.effects.is_empty());
    let wall = source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(1.0, 0.0)),
            false,
            2,
        ),
        ms(426),
    );
    assert_eq!(wall.failure, None);
    assert!(
        wall.effects.is_empty(),
        "{:?}",
        wall.effects.iter().collect::<Vec<_>>()
    );
    assert_eq!(source.mode(), SourceMode::Local);
    // The receiver's acknowledgement of the barrier ends the hold.
    let ack = Frame::new(barrier.epoch, 1, Message::ReleaseAck);
    let resumed = source.on_remote_frame(&ack, ms(430));
    assert!(resumed.failure.is_none());
    assert_eq!(source.held_since(), None);
    assert_eq!(source.hold_stats(ms(430)), (1, ms(430) - RETREAT_AFTER));
    // Ordinary liveness resumes from the fresh reply: a new deadline, a new retreat.
    assert!(
        source
            .tick(ms(430) + RETREAT_AFTER - ms(1))
            .failure
            .is_none()
    );
    assert!(source.held_since().is_none());
    let again = source.tick(ms(430) + PEER_LIVENESS);
    assert_eq!(again.failure, None);
    assert!(source.held_since().is_some());
    assert_eq!(source.hold_stats(ms(430) + PEER_LIVENESS).0, 2);
}

#[test]
fn a_hold_without_the_link_coming_back_ends_at_the_limit() {
    let mut source = source();
    activate_remote(&mut source);
    source.tick(ms(0));
    let retreat = source.tick(RETREAT_AFTER);
    source.bind_capture_route(route_request(&retreat), 2, RETREAT_AFTER);
    source.on_captured(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: 2,
            },
            false,
            2,
        ),
        RETREAT_AFTER,
    );
    assert!(
        source
            .tick(RETREAT_AFTER + HOLD_LIMIT - ms(1))
            .failure
            .is_none()
    );
    let ended = source.tick(RETREAT_AFTER + HOLD_LIMIT);
    assert_eq!(
        ended.failure,
        Some(SourceFailure::PeerHealth(HealthError::DeadlineExpired))
    );
    assert_eq!(source.mode(), SourceMode::Failed);
}

#[test]
fn a_local_source_holds_at_the_deadline_and_still_sends_the_barrier() {
    let mut source = source();
    assert_frame(
        source.tick(ms(0)).effects.iter().next().unwrap(),
        3,
        3,
        Message::Ping(1),
    );
    let held = source.tick(PEER_LIVENESS);
    assert_eq!(held.failure, None);
    assert_eq!(source.held_since(), Some(PEER_LIVENESS));
    assert_eq!(held.effects.iter().len(), 1);
    assert_frame(
        held.effects.iter().next().unwrap(),
        3,
        0,
        Message::ReleaseAll,
    );
    assert_eq!(source.mode(), SourceMode::Local);
    let ping = source.tick(PEER_LIVENESS + ms(30));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 4, Message::Ping(2));
    let pong = Frame::new(SessionEpoch::new(3).unwrap(), 4, Message::Pong(2));
    source.on_remote_frame(&pong, PEER_LIVENESS + ms(40));
    let ack = Frame::new(SessionEpoch::new(3).unwrap(), 0, Message::ReleaseAck);
    assert!(
        source
            .on_remote_frame(&ack, PEER_LIVENESS + ms(41))
            .failure
            .is_none()
    );
    assert_eq!(source.held_since(), None);
}

#[test]
fn diagnostics_redact_normalized_input_values() {
    let debug = format!(
        "{:?}",
        NormalizedInput::RelativeMotion(Point::new(41.5, -99.25))
    );
    assert!(debug.contains("redacted"));
    assert!(!debug.contains("41.5"));
}

#[test]
fn real_receiver_pump_crosses_configured_edges_from_windows_and_macos() {
    let mut windows_source = source();
    let mut mac_receiver = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    assert!(
        windows_source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let enter = windows_source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    mac_receiver
        .pump(&mut windows_source, enter, ms(0))
        .unwrap();
    assert_eq!(windows_source.mode(), SourceMode::Remote);
    let revision = windows_source.capture_route().1;
    let returned = windows_source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(-3.0, 0.0)),
            true,
            revision,
        ),
        ms(1),
    );
    mac_receiver
        .pump(&mut windows_source, returned, ms(1))
        .unwrap();
    assert_eq!(windows_source.mode(), SourceMode::Local);
    assert!(
        mac_receiver
            .destination
            .actions
            .contains(&RecordedAction::ReleaseAll)
    );

    let mut mac_source = mac_source_controller();
    let mut windows_receiver =
        ReceiverBridge::new(destination_topology(&[DisplayId(1), DisplayId(4)]));
    assert!(
        mac_source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(0.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let enter = mac_source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(-1.0, 0.0))),
        ms(0),
    );
    windows_receiver
        .pump(&mut mac_source, enter, ms(0))
        .unwrap();
    assert_eq!(mac_source.mode(), SourceMode::Remote);
    let revision = mac_source.capture_route().1;
    let returned = mac_source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(3.0, 0.0)),
            true,
            revision,
        ),
        ms(1),
    );
    windows_receiver
        .pump(&mut mac_source, returned, ms(1))
        .unwrap();
    assert_eq!(mac_source.mode(), SourceMode::Local);
    assert!(
        windows_receiver
            .destination
            .actions
            .contains(&RecordedAction::ReleaseAll)
    );
}

#[test]
fn real_receiver_preserves_modifier_drag_and_quarantines_held_ordinary_key() {
    let mut source = source();
    let mut bridge = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    for event in [
        NormalizedInput::Key {
            usage: HidUsage(0xe0),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_CONTROL),
        },
        NormalizedInput::Key {
            usage: HidUsage(0xe4),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_CONTROL | ModifierState::RIGHT_CONTROL),
        },
        NormalizedInput::Key {
            usage: HidUsage(0x04),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_CONTROL | ModifierState::RIGHT_CONTROL),
        },
        NormalizedInput::Button {
            button: MouseButton::Left,
            pressed: true,
        },
    ] {
        assert!(source.on_captured(local(event), ms(0)).effects.is_empty());
    }
    bridge.pump(&mut source, edge, ms(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    assert!(bridge.sent.iter().any(|frame| matches!(
        &frame.message,
        Message::Key(Key {
            usage: HidUsage(0xe0),
            is_down: true,
            repeat: false,
            modifiers: ModifierState(ModifierState::LEFT_CONTROL),
        })
    )));
    assert!(bridge.sent.iter().any(|frame| {
        let Message::Key(key) = &frame.message else {
            return false;
        };
        key.usage == HidUsage(0xe4)
            && key.is_down
            && !key.repeat
            && key.modifiers
                == ModifierState(ModifierState::LEFT_CONTROL | ModifierState::RIGHT_CONTROL)
    }));
    assert!(
        bridge
            .destination
            .actions
            .contains(&RecordedAction::Button(MouseButton::Left, true))
    );
    assert!(!bridge.sent.iter().any(|frame| matches!(
        &frame.message,
        Message::Key(Key {
            usage: HidUsage(0x04),
            ..
        })
    )));

    let revision = source.capture_route().1;
    let return_edge = source.on_captured(
        routed(
            NormalizedInput::RelativeMotion(Point::new(-3.0, 0.0)),
            true,
            revision,
        ),
        ms(1),
    );
    bridge.pump(&mut source, return_edge, ms(1)).unwrap();
    assert_eq!(source.mode(), SourceMode::Local);
    let local_release = source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: false,
                repeat: false,
                modifiers: ModifierState(
                    ModifierState::LEFT_CONTROL | ModifierState::RIGHT_CONTROL,
                ),
            },
            false,
            source.capture_route().1,
        ),
        ms(2),
    );
    assert!(local_release.effects.is_empty());
    assert!(
        !bridge
            .destination
            .actions
            .iter()
            .any(|action| matches!(action, RecordedAction::Key(HidUsage(0x04), _)))
    );
    assert_eq!(
        bridge
            .destination
            .actions
            .iter()
            .filter(|action| matches!(action, RecordedAction::ReleaseAll))
            .count(),
        1
    );
}

#[test]
fn receiver_rejection_has_no_response_and_disconnect_releases_held_input() {
    let mut rejected_source = source();
    let mut rejected = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    rejected_source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = rejected_source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    rejected.pump(&mut rejected_source, edge, ms(0)).unwrap();
    rejected.destination.reject_key_down = true;
    let revision = rejected_source.capture_route().1;
    let rejected_key = rejected_source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(0),
            },
            true,
            revision,
        ),
        ms(1),
    );
    assert_eq!(
        rejected.pump(&mut rejected_source, rejected_key, ms(1)),
        Err(ReceiverFailure::NativeDelivery)
    );
    assert_eq!(
        rejected.responses, 1,
        "only the real activation acknowledgement was sent"
    );
    assert_eq!(rejected_source.mode(), SourceMode::Remote);

    let mut held_source = source();
    let mut held = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    held_source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = held_source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(0),
    );
    held.pump(&mut held_source, edge, ms(0)).unwrap();
    let revision = held_source.capture_route().1;
    let held_key = held_source.on_captured(
        routed(
            NormalizedInput::Key {
                usage: HidUsage(0x04),
                pressed: true,
                repeat: false,
                modifiers: ModifierState(0),
            },
            true,
            revision,
        ),
        ms(1),
    );
    held.pump(&mut held_source, held_key, ms(1)).unwrap();
    assert!(
        held.destination
            .actions
            .contains(&RecordedAction::Key(HidUsage(0x04), true))
    );
    let disconnect = Frame::new(
        held.receiver.epoch(),
        2,
        Message::Disconnect(DisconnectCode::TransportLost),
    );
    assert_eq!(
        held.receiver
            .receive(&disconnect, ms(2), &mut held.destination),
        Err(ReceiverFailure::PeerStopped)
    );
    assert_eq!(
        held.destination.actions.last(),
        Some(&RecordedAction::ReleaseAll)
    );
}

#[test]
fn a_cursor_on_a_display_outside_the_layout_keeps_the_session() {
    let mut source = source();
    let inside = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
        ms(0),
    );
    assert_eq!(inside.failure, None);
    // Below display 1 there is no display in the layout and no link on that edge.
    let outside = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 150.0))),
        ms(1),
    );
    assert_eq!(outside.failure, None);
    assert!(outside.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    let wandering = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(5.0, 5.0))),
        ms(2),
    );
    assert_eq!(wandering.failure, None);
    assert!(wandering.effects.is_empty());
    let back = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
        ms(3),
    );
    assert_eq!(back.failure, None);
    assert!(back.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
}

#[test]
fn a_session_that_starts_with_the_cursor_outside_the_layout_waits_for_it() {
    let mut source = source();
    let outside = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 150.0))),
        ms(0),
    );
    assert_eq!(outside.failure, None);
    assert!(outside.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    let inside = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(60.0, 60.0))),
        ms(1),
    );
    assert_eq!(inside.failure, None);
    assert!(inside.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(1));
}

#[test]
fn a_slow_crossing_through_the_dead_zone_still_reaches_the_linked_display() {
    let mut source = source();
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(195.0, 50.0))),
        ms(0),
    );
    // Half a pixel past display 4's right edge is inside the link's dead zone and outside every
    // in-use display, as on a panel the layout leaves out.
    let dead_zone = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(200.5, 50.0))),
        ms(1),
    );
    assert_eq!(dead_zone.failure, None);
    assert!(dead_zone.effects.is_empty());
    let crossing = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(203.0, 50.0))),
        ms(2),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

#[test]
fn a_push_from_the_dead_zone_still_reaches_the_linked_display() {
    let mut source = source();
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(195.0, 50.0))),
        ms(0),
    );
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(200.5, 50.0))),
        ms(1),
    );
    let push = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(2),
    );
    assert_eq!(push.failure, None);
    assert_frame(
        push.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

/// Local displays 1 and 4 side by side, a display 5 the layout leaves out to the right of 4 and
/// a display 6 it leaves out to the left of 1; only 4's right edge links to the other computer.
fn topology_with_displays_not_in_use() -> Topology {
    let local = |id: u64, x: f64, in_use: bool| {
        Display::new(
            DisplayId(id),
            device(1),
            format!("local-{id}"),
            NativeSize::new(100, 100),
            LogicalSize::new(100.0, 100.0),
            Point::new(x, 0.0),
            1.0,
            None,
            id == 1,
        )
        .with_in_use(in_use)
    };
    let remote = Display::new(
        DisplayId(2),
        device(2),
        "remote".into(),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(0.0, 0.0),
        1.0,
        None,
        true,
    );
    let full = || NormalizedSpan::new(0.0, 1.0).unwrap();
    let link = |from: u64, from_edge: Edge, to: u64, to_edge: Edge| {
        EdgeLink::new(
            DisplayId(from),
            from_edge,
            full(),
            DisplayId(to),
            to_edge,
            full(),
            1.0,
        )
        .unwrap()
    };
    Topology::new(
        vec![
            Machine::new(device(1), Platform::Windows),
            Machine::new(device(2), Platform::MacOs),
        ],
        vec![
            local(1, 0.0, true),
            local(4, 100.0, true),
            local(5, 200.0, false),
            local(6, -100.0, false),
            remote,
        ],
        vec![
            link(1, Edge::Right, 4, Edge::Left),
            link(4, Edge::Left, 1, Edge::Right),
            link(4, Edge::Right, 2, Edge::Left),
            link(2, Edge::Left, 4, Edge::Right),
        ],
    )
    .unwrap()
}

fn source_on(topology: Topology, display: DisplayId) -> SourceController {
    SourceController::new(
        topology,
        device(1),
        display,
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap()
}

#[test]
fn a_display_not_in_use_is_space_past_the_edge_of_the_display_the_pointer_left() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let settled = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
        ms(0),
    );
    assert!(settled.effects.is_empty());
    // Deep inside display 5, at a different height: the crossing follows the pointer, not the
    // point where it left display 4.
    let crossing = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(230.0, 60.0))),
        ms(1),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 60.0),
        },
    );
}

#[test]
fn a_pointer_already_resting_on_a_display_not_in_use_crosses_on_its_first_sample() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let crossing = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(250.0, 80.0))),
        ms(0),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 80.0),
        },
    );
}

#[test]
fn a_pointer_on_a_display_not_in_use_settles_through_the_in_use_display_that_borders_it() {
    // The pointer's display is 1; display 5 lies past display 4. The first sample reaches 4
    // through its shared edge, the next one crosses from 4 to the other computer.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(1));
    let reached = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(250.0, 50.0))),
        ms(0),
    );
    assert_eq!(reached.failure, None);
    assert!(reached.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
    let crossing = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(250.0, 50.0))),
        ms(1),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

#[test]
fn a_display_not_in_use_past_an_unlinked_edge_keeps_input_local_on_the_display_it_left() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(1));
    source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
        ms(0),
    );
    let outside = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(-50.0, 50.0))),
        ms(1),
    );
    assert_eq!(outside.failure, None);
    assert!(outside.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(1));
}

#[test]
fn an_observed_pointer_position_crosses_like_a_hook_sample() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let crossing = source.observe_pointer(Point::new(230.0, 60.0), ms(1));
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 60.0),
        },
    );
    // Once the crossing is under way the poll changes nothing.
    let ignored = source.observe_pointer(Point::new(150.0, 50.0), ms(2));
    assert_eq!(ignored.failure, None);
    assert!(ignored.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::AwaitActivationAcknowledgement);
}

#[test]
fn an_observed_pointer_position_on_the_pointer_display_only_moves_the_anchor() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let inside = source.observe_pointer(Point::new(150.0, 50.0), ms(1));
    assert_eq!(inside.failure, None);
    assert!(inside.effects.is_empty());
    let push = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(2),
    );
    assert!(
        push.effects.is_empty(),
        "a push away from every edge stays local"
    );
    let edge = source.observe_pointer(Point::new(199.0, 50.0), ms(3));
    assert!(edge.effects.is_empty());
    let crossing = source.on_captured(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(4),
    );
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

#[test]
fn a_crossing_into_space_outside_the_layout_still_reaches_the_linked_display() {
    let mut source = source();
    assert_eq!(
        source
            .on_captured(
                local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
                ms(0),
            )
            .failure,
        None
    );
    let crossing = source.on_captured(
        local(NormalizedInput::AbsoluteMotion(Point::new(250.0, 50.0))),
        ms(1),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}
