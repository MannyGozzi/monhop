use std::{cell::RefCell, sync::Once, time::Duration};

use monhop_core::capture::DECLINE_RETRY_AFTER;
use monhop_core::{
    DeviceId, Display, DisplayId, Edge, EdgeLink, HidUsage, LogicalSize, Machine, ModifierState,
    MouseButton, NativeSize, NormalizedSpan, Platform, Point, Topology,
};
use monhop_protocol::{
    DeclineReason, DisconnectCode, DisplayDescription, DisplayTopology, Frame, Key, Message,
    Motion, SessionEpoch,
};
use monhop_transport::session_health::{
    HOLD_LIMIT, HealthError, PEER_LIVENESS, RETREAT_AFTER, SUPPRESSION_LEASE_CAP,
};
use monhop_transport::session_receiver::{
    DestinationAction, DestinationFailure, InputDestination, InputReceiver, ReceiverFailure,
};
use monhop_transport::session_source::{
    ENTRY_GUARD_DISTANCE, NormalizedInput, PUSH_THROUGH_DISTANCE, PUSH_THROUGH_RESET,
    SourceController, SourceEffect, SourceFailure, SourceMode, SourceOutcome, TaggedInput,
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
        let crossed = source.fresh_capture(
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
        floor_generation: 1,
        routing_revision: 0,
        remote: false,
    }
}

fn routed(event: NormalizedInput, remote: bool, revision: u64) -> TaggedInput {
    TaggedInput {
        event,
        floor_generation: 1,
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
    activate_remote_from(
        source,
        Point::new(99.0, 50.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 50.0),
    );
}

/// Arrives at `at` on the local display, pushes through along the unit `push` and completes the
/// activation of display 2, which the pointer enters at `entry`.
fn activate_remote_from(source: &mut SourceController, at: Point, push: Point, entry: Point) {
    assert!(
        source
            .fresh_capture(local(NormalizedInput::AbsoluteMotion(at)), ms(0))
            .effects
            .is_empty()
    );
    let edge = push_through(source, push, ms(0));
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: entry,
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
    let barrier = source.fresh_capture(
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

/// Tags `event` with the capture route the source is on right now.
fn capture_on_route(
    source: &mut SourceController,
    event: NormalizedInput,
    now: Duration,
) -> SourceOutcome {
    let (remote, revision) = source.capture_route();
    source.fresh_capture(routed(event, remote, revision), now)
}

fn move_horizontally(source: &mut SourceController, dx: f64, now: Duration) -> SourceOutcome {
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(Point::new(dx, 0.0)),
        now,
    )
}

fn scaled(push: Point, by: f64) -> Point {
    Point::new(push.x * by, push.y * by)
}

/// From a pointer resting on an edge: one px along the unit `push` (an arrival's own relative
/// half, which never counts), then a full push through, whose outcome it yields.
fn push_through(source: &mut SourceController, push: Point, now: Duration) -> SourceOutcome {
    let first = capture_on_route(source, NormalizedInput::RelativeMotion(push), now);
    assert_eq!(first.failure, None);
    assert!(first.effects.is_empty(), "one px never pushes through");
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(scaled(push, PUSH_THROUGH_DISTANCE)),
        now,
    )
}

/// Brings the local pointer from inside display 1 onto its linked right edge, and delivers that
/// arrival's relative half.
fn rest_on_right_edge(source: &mut SourceController, now: Duration) {
    move_to(source, Point::new(50.0, 50.0), now);
    move_to(source, Point::new(99.0, 50.0), now);
    let overshoot = move_horizontally(source, 1.0, now);
    assert_eq!(overshoot.failure, None);
    assert!(overshoot.effects.is_empty());
}

/// Answers the source's next health challenge at `now`, in peer heartbeat `sequence`.
fn stay_alive(source: &mut SourceController, sequence: u64, now: Duration) {
    let challenge = source.tick(now);
    let token = challenge
        .effects
        .iter()
        .find_map(|effect| match effect {
            SourceEffect::RemoteFrame(Frame {
                message: Message::Ping(token),
                ..
            }) => Some(*token),
            _ => None,
        })
        .expect("a challenge is due");
    let pong = Frame::new(
        SessionEpoch::new(3).unwrap(),
        sequence,
        Message::Pong(token),
    );
    assert_eq!(source.on_remote_frame(&pong, now).failure, None);
}

/// Acknowledges the ReleaseAll of a return from display 2 and completes the native restore,
/// yielding the display and position the local pointer landed on.
fn complete_return(
    source: &mut SourceController,
    ack_sequence: u64,
    now: Duration,
) -> (DisplayId, Point) {
    let restore = source.on_remote_frame(
        &Frame::new(
            SessionEpoch::new(4).unwrap(),
            ack_sequence,
            Message::ReleaseAck,
        ),
        now,
    );
    assert_eq!(restore.failure, None);
    let Some(SourceEffect::RestoreLocalAt {
        request,
        display,
        position,
    }) = restore.effects.iter().next()
    else {
        panic!("the acknowledgement restores the local pointer");
    };
    let ticket = source.capture_route().1 + 1;
    assert_eq!(
        source.bind_capture_route(*request, ticket, now).failure,
        None
    );
    let barrier = source.fresh_capture(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: ticket,
            },
            false,
            ticket,
        ),
        now,
    );
    assert_eq!(barrier.failure, None);
    assert_eq!(source.mode(), SourceMode::Local);
    (*display, *position)
}

/// A local absolute sample that must neither cross nor fail.
fn move_to(source: &mut SourceController, point: Point, now: Duration) {
    let moved = capture_on_route(source, NormalizedInput::AbsoluteMotion(point), now);
    assert_eq!(moved.failure, None);
    assert!(moved.effects.is_empty());
}

fn sends(outcome: &SourceOutcome, wanted: fn(&Message) -> bool) -> bool {
    outcome
        .effects
        .iter()
        .any(|effect| matches!(effect, SourceEffect::RemoteFrame(frame) if wanted(&frame.message)))
}

fn sends_release_all(outcome: &SourceOutcome) -> bool {
    sends(outcome, |message| matches!(message, Message::ReleaseAll))
}

fn sends_activation(outcome: &SourceOutcome) -> bool {
    sends(outcome, |message| {
        matches!(message, Message::ActivateDisplayAt { .. })
    })
}

/// Collects log lines per thread; libtest runs each test on its own thread.
struct ThreadLog;

thread_local! {
    static LOG_LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

impl log::Log for ThreadLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        LOG_LINES.with(|lines| lines.borrow_mut().push(record.args().to_string()));
    }

    fn flush(&self) {}
}

fn capture_log() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&ThreadLog).unwrap();
        log::set_max_level(log::LevelFilter::Info);
    });
    LOG_LINES.with(|lines| lines.borrow_mut().clear());
}

fn logged(fragment: &str) -> usize {
    LOG_LINES.with(|lines| {
        lines
            .borrow()
            .iter()
            .filter(|line| line.contains(fragment))
            .count()
    })
}

/// A 100 px tall display at `origin`, `width` px wide.
fn display_at(id: u64, machine: u8, origin: Point, width: u32) -> Display {
    Display::new(
        DisplayId(id),
        device(machine),
        format!("display-{id}"),
        NativeSize::new(width, 100),
        LogicalSize::new(f64::from(width), 100.0),
        origin,
        1.0,
        None,
        false,
    )
}

fn full_link(from: u64, from_edge: Edge, to: u64, to_edge: Edge) -> EdgeLink {
    let full = NormalizedSpan::new(0.0, 1.0).unwrap();
    EdgeLink::new(
        DisplayId(from),
        from_edge,
        full,
        DisplayId(to),
        to_edge,
        full,
        1.0,
    )
    .unwrap()
}

fn two_machine_topology(displays: Vec<Display>, links: Vec<EdgeLink>) -> Topology {
    Topology::new(
        vec![
            Machine::new(device(1), Platform::Windows),
            Machine::new(device(2), Platform::MacOs),
        ],
        displays,
        links,
    )
    .unwrap()
}

#[test]
fn edge_activation_uses_input_epoch_while_health_stays_on_base_epoch() {
    let mut source = source();
    let ping = source.tick(ms(0));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 3, Message::Ping(1));

    assert!(
        source
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(1)
            )
            .effects
            .is_empty()
    );
    let edge = push_through(&mut source, Point::new(1.0, 0.0), ms(1));
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
    let first = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(199.0, 50.0))),
        ms(0),
    );
    assert!(first.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));

    let edge = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
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
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let second = source.fresh_capture(
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
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(0)
            )
            .effects
            .is_empty()
    );
    push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    source.bind_capture_route(route_request(&acknowledged), 1, ms(0));

    let held_local = source.fresh_capture(
        local(NormalizedInput::Key {
            usage: HidUsage(0x04),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(0),
        }),
        ms(0),
    );
    assert!(held_local.effects.is_empty());
    let barrier = source.fresh_capture(
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

    let old_release = source.fresh_capture(
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
    let fresh_press = source.fresh_capture(
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
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    let request = route_request(&acknowledged);
    assert_ne!(request.get(), 9);
    assert_eq!(source.bind_capture_route(request, 9, ms(0)).failure, None);
    assert_eq!(
        source
            .fresh_capture(
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
    let motion = source.fresh_capture(
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
    let scroll = source.fresh_capture(
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
    assert_eq!(
        move_horizontally(&mut returning, ENTRY_GUARD_DISTANCE, ms(1)).failure,
        None
    );
    // Reaching the entry edge stops there; the push after it returns.
    let arrival = move_horizontally(&mut returning, -ENTRY_GUARD_DISTANCE - 3.0, ms(1));
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let release = push_through(&mut returning, Point::new(-1.0, 0.0), ms(1));
    assert_frame(
        release.effects.iter().next().unwrap(),
        4,
        3,
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
            .fresh_capture(
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
    remote_to_remote.fresh_capture(
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
    remote_to_remote.fresh_capture(
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
    remote_to_remote.fresh_capture(
        routed(
            NormalizedInput::RelativeMotion(Point::new(98.0, 0.0)),
            true,
            1,
        ),
        ms(3),
    );
    let short = move_horizontally(&mut remote_to_remote, PUSH_THROUGH_DISTANCE - 1.0, ms(4));
    assert_eq!(short.failure, None);
    assert!(!sends_activation(&short));
    let handoff = move_horizontally(&mut remote_to_remote, 1.0, ms(4));
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
    let control_release = remote_to_remote.fresh_capture(
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
    let button_release = remote_to_remote.fresh_capture(
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
    let resumed = remote_to_remote.fresh_capture(
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
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    bridge.pump(&mut source, edge, ms(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    // From y = 1, 76 tenth-point steps leave an anchor whose relative echo rounds past y = 100
    // on the receiver; the absolute position lands exactly on the clamped, unlinked bottom edge.
    let mut deltas = vec![-49.0];
    deltas.extend(std::iter::repeat_n(0.1, 76));
    deltas.push(1_000.0);
    for (step, delta) in (1_u64..).zip(deltas) {
        let motion = source.fresh_capture(
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
    let stalled = source.fresh_capture(
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
fn a_pointer_held_on_a_far_edge_stays_inside_the_receivers_own_coordinates() {
    // The remote display's bottom edge sits at y = 0 here and at y = 1653 on its own computer:
    // one ulp inside 0 rounds onto 1653 when translated, which the receiver rejects.
    let topology = two_machine_topology(
        vec![
            display_at(1, 1, Point::new(0.0, 0.0), 100),
            display_at(2, 2, Point::new(100.0, -100.0), 100),
        ],
        vec![
            full_link(1, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 1, Edge::Right),
        ],
    );
    let mut source = SourceController::new(
        topology,
        device(1),
        DisplayId(1),
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap();
    source.set_peer_offset(Point::new(2660.0, -1653.0));
    let mut bridge = ReceiverBridge::new(
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(2),
            name: "remote".into(),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::new(-2560.0, 1553.0),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap(),
    );
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    bridge.pump(&mut source, edge, ms(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    for (step, delta) in (1_u64..).zip([Point::new(0.0, 1_000.0), Point::new(1_000.0, 0.0)]) {
        let motion = capture_on_route(
            &mut source,
            NormalizedInput::RelativeMotion(delta),
            ms(step),
        );
        bridge.pump(&mut source, motion, ms(step)).unwrap();
    }
    bridge.receiver.flush(&mut bridge.destination).unwrap();
    let corner = Point::new((-2460.0_f64).next_down(), 1653.0_f64.next_down());
    assert_eq!(
        bridge.sent.last().map(|frame| &frame.message),
        Some(&Message::Motion(Motion::Absolute(corner)))
    );
    assert_eq!(
        bridge.destination.actions.last(),
        Some(&RecordedAction::Move(corner))
    );
}

#[test]
fn returning_local_waits_for_release_ack_before_one_native_restore_command() {
    let mut source = source();
    activate_remote(&mut source);
    let control = source.fresh_capture(
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
    source.fresh_capture(
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
    source.fresh_capture(
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

    let local_barrier = source.fresh_capture(
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
    controller.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    assert!(sends_activation(&push_through(
        &mut controller,
        Point::new(1.0, 0.0),
        ms(0)
    )));
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
    let mismatch = other.fresh_capture(
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
    let returned = source.fresh_capture(
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
    source.fresh_capture(
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
    // Pushing through the seam while held keeps input local.
    let wall = source.fresh_capture(
        routed(
            NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0)),
            false,
            2,
        ),
        ms(425),
    );
    assert_eq!(wall.failure, None);
    assert!(wall.effects.is_empty());
    let wall = push_through(&mut source, Point::new(1.0, 0.0), ms(426));
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
    source.fresh_capture(
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
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let enter = push_through(&mut windows_source, Point::new(1.0, 0.0), ms(0));
    mac_receiver
        .pump(&mut windows_source, enter, ms(0))
        .unwrap();
    assert_eq!(windows_source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut windows_source, ENTRY_GUARD_DISTANCE, ms(1));
    mac_receiver.pump(&mut windows_source, away, ms(1)).unwrap();
    let back = move_horizontally(&mut windows_source, -ENTRY_GUARD_DISTANCE - 3.0, ms(1));
    mac_receiver.pump(&mut windows_source, back, ms(1)).unwrap();
    let returned = push_through(&mut windows_source, Point::new(-1.0, 0.0), ms(1));
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
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(0.0, 50.0))),
                ms(0),
            )
            .effects
            .is_empty()
    );
    let enter = push_through(&mut mac_source, Point::new(-1.0, 0.0), ms(0));
    windows_receiver
        .pump(&mut mac_source, enter, ms(0))
        .unwrap();
    assert_eq!(mac_source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut mac_source, -ENTRY_GUARD_DISTANCE, ms(1));
    windows_receiver.pump(&mut mac_source, away, ms(1)).unwrap();
    let back = move_horizontally(&mut mac_source, ENTRY_GUARD_DISTANCE + 3.0, ms(1));
    windows_receiver.pump(&mut mac_source, back, ms(1)).unwrap();
    let returned = push_through(&mut mac_source, Point::new(1.0, 0.0), ms(1));
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
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
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
        assert!(source.fresh_capture(local(event), ms(0)).effects.is_empty());
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

    let away = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, ms(1));
    bridge.pump(&mut source, away, ms(1)).unwrap();
    let back = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, ms(1));
    bridge.pump(&mut source, back, ms(1)).unwrap();
    let return_edge = push_through(&mut source, Point::new(-1.0, 0.0), ms(1));
    bridge.pump(&mut source, return_edge, ms(1)).unwrap();
    assert_eq!(source.mode(), SourceMode::Local);
    let local_release = source.fresh_capture(
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
    rejected_source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = push_through(&mut rejected_source, Point::new(1.0, 0.0), ms(0));
    rejected.pump(&mut rejected_source, edge, ms(0)).unwrap();
    rejected.destination.reject_key_down = true;
    let revision = rejected_source.capture_route().1;
    let rejected_key = rejected_source.fresh_capture(
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
    held_source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0))),
        ms(0),
    );
    let edge = push_through(&mut held_source, Point::new(1.0, 0.0), ms(0));
    held.pump(&mut held_source, edge, ms(0)).unwrap();
    let revision = held_source.capture_route().1;
    let held_key = held_source.fresh_capture(
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
    let inside = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
        ms(0),
    );
    assert_eq!(inside.failure, None);
    // Below display 1 there is no display in the layout and no link on that edge.
    let outside = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 150.0))),
        ms(1),
    );
    assert_eq!(outside.failure, None);
    assert!(outside.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    let wandering = source.fresh_capture(
        local(NormalizedInput::RelativeMotion(Point::new(5.0, 5.0))),
        ms(2),
    );
    assert_eq!(wandering.failure, None);
    assert!(wandering.effects.is_empty());
    let back = source.fresh_capture(
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
    let outside = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 150.0))),
        ms(0),
    );
    assert_eq!(outside.failure, None);
    assert!(outside.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    let inside = source.fresh_capture(
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
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(195.0, 50.0))),
        ms(0),
    );
    // Half a pixel past display 4's right edge is inside the link's dead zone and outside every
    // in-use display, as on a panel the layout leaves out.
    let dead_zone = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(200.5, 50.0))),
        ms(1),
    );
    assert_eq!(dead_zone.failure, None);
    assert!(dead_zone.effects.is_empty());
    // Slow samples on through it push through once the full distance past where they arrived.
    let arrived_at = 200.5;
    for x in [203.0, 240.0, arrived_at + PUSH_THROUGH_DISTANCE - 0.5] {
        move_to(&mut source, Point::new(x, 50.0), ms(2));
    }
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            arrived_at + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
        ms(3),
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
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(195.0, 50.0))),
        ms(0),
    );
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(200.5, 50.0))),
        ms(1),
    );
    let push = push_through(&mut source, Point::new(1.0, 0.0), ms(2));
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
    two_machine_topology(
        vec![
            local(1, 0.0, true),
            local(4, 100.0, true),
            local(5, 200.0, false),
            local(6, -100.0, false),
            remote,
        ],
        vec![
            full_link(1, Edge::Right, 4, Edge::Left),
            full_link(4, Edge::Left, 1, Edge::Right),
            full_link(4, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 4, Edge::Right),
        ],
    )
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
    let settled = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
        ms(0),
    );
    assert!(settled.effects.is_empty());
    // Arriving deep inside display 5 stops at display 4's edge; pushing on across display 5, at
    // a different height, crosses where the pointer is, not where it left display 4.
    move_to(&mut source, Point::new(230.0, 60.0), ms(1));
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            230.0 + PUSH_THROUGH_DISTANCE,
            70.0,
        ))),
        ms(2),
    );
    assert_eq!(crossing.failure, None);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 70.0),
        },
    );
}

#[test]
fn a_pointer_already_resting_on_a_display_not_in_use_crosses_once_pushed_on() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    move_to(&mut source, Point::new(250.0, 80.0), ms(0));
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            250.0 + PUSH_THROUGH_DISTANCE,
            80.0,
        ))),
        ms(1),
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
    // through its shared edge at once, the next rests on 4's edge, and a push on crosses.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(1));
    let reached = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(250.0, 50.0))),
        ms(0),
    );
    assert_eq!(reached.failure, None);
    assert!(reached.effects.is_empty());
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
    move_to(&mut source, Point::new(250.0, 50.0), ms(1));
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            250.0 + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
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
fn a_display_not_in_use_past_an_unlinked_edge_keeps_input_local_on_the_display_it_left() {
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(1));
    source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(50.0, 50.0))),
        ms(0),
    );
    let outside = source.fresh_capture(
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
    let arrival = source.observe_pointer(Point::new(230.0, 60.0), ms(1));
    assert_eq!(arrival.failure, None);
    assert!(arrival.effects.is_empty());
    let crossing = source.observe_pointer(Point::new(230.0 + PUSH_THROUGH_DISTANCE, 60.0), ms(1));
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
    let push = source.fresh_capture(
        local(NormalizedInput::RelativeMotion(Point::new(1.0, 0.0))),
        ms(2),
    );
    assert!(
        push.effects.is_empty(),
        "a push away from every edge stays local"
    );
    let edge = source.observe_pointer(Point::new(199.0, 50.0), ms(3));
    assert!(edge.effects.is_empty());
    let crossing = push_through(&mut source, Point::new(1.0, 0.0), ms(4));
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
            .fresh_capture(
                local(NormalizedInput::AbsoluteMotion(Point::new(150.0, 50.0))),
                ms(0),
            )
            .failure,
        None
    );
    move_to(&mut source, Point::new(250.0, 50.0), ms(1));
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            250.0 + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
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
fn a_stray_pixel_back_toward_the_entry_edge_keeps_the_pointer_on_the_entered_display() {
    let mut source = source();
    activate_remote(&mut source);
    // Entered through display 2's left edge at x = 1: without the guard 3 px left crosses home.
    let wobble = move_horizontally(&mut source, -3.0, ms(1));
    assert_frame(
        wobble.effects.iter().next().unwrap(),
        4,
        1,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    // A mostly vertical flick with a pixel of sideways noise slides along the edge.
    let flick = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(-1.0, 10.0)),
        ms(2),
    );
    assert_frame(
        flick.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 60.0))),
    );
    let pressed = move_horizontally(&mut source, -2.0, ms(3));
    assert_eq!(pressed.failure, None);
    assert!(pressed.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Remote);
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(2));
}

#[test]
fn the_entry_edge_crosses_back_once_the_pointer_has_moved_the_guard_distance_away() {
    let mut source = source();
    activate_remote(&mut source);
    // From the entry at x = 1 this reaches x = 23, one px short of the guard distance.
    let short = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE - 2.0, ms(1));
    assert_eq!(short.failure, None);
    let held = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, ms(2));
    assert_frame(
        held.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    assert_eq!(source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, ms(3));
    assert_eq!(away.failure, None);
    let arrival = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, ms(4));
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        4,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let returned = push_through(&mut source, Point::new(-1.0, 0.0), ms(4));
    assert_frame(
        returned.effects.iter().next().unwrap(),
        4,
        5,
        Message::ReleaseAll,
    );
    assert_eq!(source.mode(), SourceMode::AwaitRemoteReleaseAcknowledgement);
    assert_eq!(
        complete_return(&mut source, 1, ms(5)),
        (DisplayId(1), Point::new(99.0, 50.0))
    );
}

#[test]
fn a_return_through_an_edge_guards_that_home_edge_until_the_pointer_moves_away() {
    let mut source = source();
    activate_remote(&mut source);
    assert_eq!(
        move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, ms(1)).failure,
        None
    );
    assert_eq!(
        move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, ms(2)).failure,
        None
    );
    assert!(sends_release_all(&push_through(
        &mut source,
        Point::new(-1.0, 0.0),
        ms(2)
    )));
    let (display, home) = complete_return(&mut source, 1, ms(3));
    assert_eq!((display, home), (DisplayId(1), Point::new(99.0, 50.0)));
    let wobble = move_horizontally(&mut source, 1.0, ms(4));
    assert_eq!(wobble.failure, None);
    assert!(
        wobble.effects.is_empty(),
        "a wobble at the home edge stays home"
    );
    let shove = move_horizontally(&mut source, PUSH_THROUGH_DISTANCE, ms(4));
    assert_eq!(shove.failure, None);
    assert!(shove.effects.is_empty(), "a guarded edge takes no push");
    // Display 1's right edge line is x = 100: 23 px from it keeps the guard, 24 px rearms it.
    let right_edge = 100.0;
    move_to(
        &mut source,
        Point::new(right_edge - ENTRY_GUARD_DISTANCE + 1.0, 50.0),
        ms(5),
    );
    move_to(&mut source, home, ms(6));
    let still_guarded = move_horizontally(&mut source, PUSH_THROUGH_DISTANCE, ms(7));
    assert_eq!(still_guarded.failure, None);
    assert!(still_guarded.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    move_to(
        &mut source,
        Point::new(right_edge - ENTRY_GUARD_DISTANCE, 50.0),
        ms(8),
    );
    move_to(&mut source, home, ms(9));
    let crossing = push_through(&mut source, Point::new(1.0, 0.0), ms(10));
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        5,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

#[test]
fn a_take_back_return_leaves_the_home_edge_unguarded() {
    let mut source = source();
    activate_remote(&mut source);
    let taken = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::TakeBack),
        ms(1),
    );
    assert!(sends_release_all(&taken));
    // The same spot an edge return lands on, but nothing guards it.
    assert_eq!(
        complete_return(&mut source, 2, ms(2)),
        (DisplayId(1), Point::new(99.0, 50.0))
    );
    let push = push_through(&mut source, Point::new(1.0, 0.0), ms(3));
    assert_frame(
        push.effects.iter().next().unwrap(),
        5,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

/// The other computer's display 3 sits above its display 2, linked through 2's top edge.
fn topology_with_a_display_above_the_entered_one() -> Topology {
    two_machine_topology(
        vec![
            display_at(1, 1, Point::new(0.0, 0.0), 100),
            display_at(2, 2, Point::new(0.0, 0.0), 100),
            display_at(3, 2, Point::new(0.0, -100.0), 100),
        ],
        vec![
            full_link(1, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 1, Edge::Right),
            full_link(2, Edge::Top, 3, Edge::Bottom),
            full_link(3, Edge::Bottom, 2, Edge::Top),
        ],
    )
}

#[test]
fn an_entry_guard_leaves_the_other_edges_of_the_display_crossable() {
    let mut source = source_on(
        topology_with_a_display_above_the_entered_one(),
        DisplayId(1),
    );
    activate_remote(&mut source);
    let up = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(0.0, -49.0)),
        ms(1),
    );
    assert_frame(
        up.effects.iter().next().unwrap(),
        4,
        1,
        Message::Motion(Motion::Absolute(Point::new(1.0, 1.0))),
    );
    // Unguarded, this diagonal would push on the left edge first; guarded, its left part stops
    // on that edge and the rest pushes on the top edge.
    let corner = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(-3.0, -3.0)),
        ms(2),
    );
    assert_eq!(corner.failure, None);
    assert_frame(
        corner.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 0.0))),
    );
    let through = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(-PUSH_THROUGH_DISTANCE, -PUSH_THROUGH_DISTANCE)),
        ms(3),
    );
    assert_eq!(through.failure, None);
    assert!(!sends_release_all(&through));
    assert!(matches!(
        through.effects.iter().next(),
        Some(SourceEffect::RemoteFrame(Frame {
            message: Message::ActivateDisplayAt {
                display_id: DisplayId(3),
                ..
            },
            ..
        }))
    ));
    assert_eq!(source.mode(), SourceMode::AwaitActivationAcknowledgement);
}

/// Enters display 2 by pushing along the unit `push` from `at`, moves off the entry edge, back
/// onto it and pushes through it, yielding where the returned pointer landed.
fn round_trip(
    source: &mut SourceController,
    at: Point,
    push: Point,
    entry: Point,
) -> (DisplayId, Point) {
    activate_remote_from(source, at, push, entry);
    let along = |by: f64| NormalizedInput::RelativeMotion(scaled(push, by));
    assert_eq!(
        capture_on_route(source, along(ENTRY_GUARD_DISTANCE), ms(1)).failure,
        None
    );
    let arrival = capture_on_route(source, along(-ENTRY_GUARD_DISTANCE - 3.0), ms(2));
    assert!(
        !sends_release_all(&arrival),
        "reaching the edge never crosses it"
    );
    assert!(sends_release_all(&push_through(
        source,
        scaled(push, -1.0),
        ms(2)
    )));
    complete_return(source, 1, ms(3))
}

#[test]
fn a_guarded_home_edge_releases_once_the_pointer_is_that_far_onto_a_display_not_in_use() {
    // Display 5, past display 4's right edge line at x = 200, is not in use: the OS cursor moves
    // onto it while the tracked cursor stays pinned inside display 4.
    let right_edge = 200.0;
    for (past, crosses) in [
        (1.0, false),
        (ENTRY_GUARD_DISTANCE - 1.0, false),
        (ENTRY_GUARD_DISTANCE, true),
        (30.0, true),
        (60.0, true),
    ] {
        let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
        let landed = round_trip(
            &mut source,
            Point::new(199.0, 50.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 50.0),
        );
        assert_eq!(landed, (DisplayId(4), Point::new(199.0, 50.0)));
        // Guarded or just released, the edge has taken no push yet.
        move_to(&mut source, Point::new(right_edge + past, 50.0), ms(4));
        let beyond = right_edge + past + PUSH_THROUGH_DISTANCE;
        let push = capture_on_route(
            &mut source,
            NormalizedInput::AbsoluteMotion(Point::new(beyond, 50.0)),
            ms(5),
        );
        assert_eq!(push.failure, None);
        assert_eq!(sends_activation(&push), crosses, "{past} px past the edge");
        if !crosses {
            let further = capture_on_route(
                &mut source,
                NormalizedInput::AbsoluteMotion(Point::new(beyond + PUSH_THROUGH_DISTANCE, 50.0)),
                ms(6),
            );
            assert!(
                sends_activation(&further),
                "a held pointer still crosses later"
            );
        }
    }
}

const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

fn opposite(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

/// The middle of `edge` of a 100 px square display at the origin, `inset` px inside it
/// (negative: past it).
fn edge_point(edge: Edge, inset: f64) -> Point {
    match edge {
        Edge::Left => Point::new(inset, 50.0),
        Edge::Right => Point::new(100.0 - inset, 50.0),
        Edge::Top => Point::new(50.0, inset),
        Edge::Bottom => Point::new(50.0, 100.0 - inset),
    }
}

/// A relative motion of `distance` px across `edge`, out of the display.
fn across(edge: Edge, distance: f64) -> Point {
    match edge {
        Edge::Left => Point::new(-distance, 0.0),
        Edge::Right => Point::new(distance, 0.0),
        Edge::Top => Point::new(0.0, -distance),
        Edge::Bottom => Point::new(0.0, distance),
    }
}

fn push_across(source: &mut SourceController, edge: Edge, distance: f64, at: u64) -> SourceOutcome {
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(across(edge, distance)),
        ms(at),
    )
}

/// Each edge of display 1 links to the opposite edge of the other computer's display 2, and back.
fn topology_linked_on_every_edge() -> Topology {
    let origin = Point::new(0.0, 0.0);
    two_machine_topology(
        vec![display_at(1, 1, origin, 100), display_at(2, 2, origin, 100)],
        EDGES
            .into_iter()
            .flat_map(|edge| {
                [
                    full_link(1, edge, 2, opposite(edge)),
                    full_link(2, opposite(edge), 1, edge),
                ]
            })
            .collect(),
    )
}

#[test]
fn every_entry_edge_holds_small_pushes_on_every_motion_path_until_the_pointer_moves_away() {
    // The remote model clamps into half-open bounds, one ulp short of the right and bottom lines.
    let last = 100.0_f64.next_down();
    for entered in EDGES {
        let home_edge = opposite(entered);
        let at = edge_point(home_edge, 1.0);
        let push = across(home_edge, 1.0);
        let entry = edge_point(entered, 1.0);

        // The modelled pointer on the other computer: a 3 px push back stops on the edge line,
        // and while guarded even a full push takes nothing.
        let mut remote = source_on(topology_linked_on_every_edge(), DisplayId(1));
        activate_remote_from(&mut remote, at, push, entry);
        let held = push_across(&mut remote, entered, 3.0, 1);
        let line = edge_point(entered, 0.0);
        assert_frame(
            held.effects.iter().next().unwrap(),
            4,
            1,
            Message::Motion(Motion::Absolute(Point::new(
                line.x.min(last),
                line.y.min(last),
            ))),
        );
        let shoved = push_across(&mut remote, entered, PUSH_THROUGH_DISTANCE, 1);
        assert_eq!(shoved.failure, None);
        assert!(shoved.effects.is_empty(), "{entered:?}");
        assert_eq!(remote.mode(), SourceMode::Remote, "{entered:?}");
        // Once 24 px inside, back on the line it takes the full push to return.
        push_across(&mut remote, entered, -ENTRY_GUARD_DISTANCE, 2);
        assert!(!sends_release_all(&push_across(
            &mut remote,
            entered,
            ENTRY_GUARD_DISTANCE + 3.0,
            2
        )));
        let short = push_across(&mut remote, entered, PUSH_THROUGH_DISTANCE - 1.0, 3);
        assert!(short.effects.is_empty(), "{entered:?}");
        assert!(
            sends_release_all(&push_across(&mut remote, entered, 1.0, 3)),
            "{entered:?}"
        );

        // Local relative pushes at the home edge hold until the pointer has been 24 px inside,
        // then take the full push.
        let mut relative = source_on(topology_linked_on_every_edge(), DisplayId(1));
        assert_eq!(
            round_trip(&mut relative, at, push, entry),
            (DisplayId(1), at),
            "{entered:?}"
        );
        let pushed = push_across(&mut relative, home_edge, PUSH_THROUGH_DISTANCE, 4);
        assert_eq!(pushed.failure, None);
        assert!(pushed.effects.is_empty(), "{entered:?}");
        move_to(
            &mut relative,
            edge_point(home_edge, ENTRY_GUARD_DISTANCE),
            ms(5),
        );
        move_to(&mut relative, at, ms(6));
        let crossing = push_through(&mut relative, push, ms(7));
        assert!(sends_activation(&crossing), "{entered:?}");

        // Local absolute samples past the home edge: 3 px is held, 24 px releases the guard and
        // the full distance past that pushes through.
        let mut absolute = source_on(topology_linked_on_every_edge(), DisplayId(1));
        round_trip(&mut absolute, at, push, entry);
        move_to(&mut absolute, edge_point(home_edge, -3.0), ms(4));
        move_to(
            &mut absolute,
            edge_point(home_edge, -ENTRY_GUARD_DISTANCE),
            ms(5),
        );
        move_to(
            &mut absolute,
            edge_point(
                home_edge,
                -ENTRY_GUARD_DISTANCE - PUSH_THROUGH_DISTANCE + 1.0,
            ),
            ms(6),
        );
        let crossing = capture_on_route(
            &mut absolute,
            NormalizedInput::AbsoluteMotion(edge_point(
                home_edge,
                -ENTRY_GUARD_DISTANCE - PUSH_THROUGH_DISTANCE,
            )),
            ms(7),
        );
        assert!(sends_activation(&crossing), "{entered:?}");
    }
}

#[test]
fn a_guard_is_dropped_once_the_pointer_is_on_another_display() {
    let mut source = source();
    let landed = round_trip(
        &mut source,
        Point::new(99.0, 50.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 50.0),
    );
    assert_eq!(landed, (DisplayId(1), Point::new(99.0, 50.0)));
    // 10 px onto display 4 is still within the guard distance of display 1's right edge line.
    move_to(&mut source, Point::new(110.0, 50.0), ms(4));
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
    // Display 4's own right edge is linked too; display 1's guard must not hold it.
    move_to(&mut source, Point::new(199.0, 50.0), ms(5));
    let push = push_through(&mut source, Point::new(1.0, 0.0), ms(6));
    assert_frame(
        push.effects.iter().next().unwrap(),
        5,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 50.0),
        },
    );
}

/// The other computer's display 2 is 20 px wide, less than twice the guard distance.
fn topology_with_a_narrow_entered_display() -> Topology {
    let origin = Point::new(0.0, 0.0);
    two_machine_topology(
        vec![display_at(1, 1, origin, 100), display_at(2, 2, origin, 20)],
        vec![
            full_link(1, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 1, Edge::Right),
        ],
    )
}

#[test]
fn a_display_too_narrow_for_the_guard_distance_releases_halfway_across() {
    let mut source = source_on(topology_with_a_narrow_entered_display(), DisplayId(1));
    activate_remote(&mut source);
    // Entered at x = 1: x = 9 is short of half the 20 px width, x = 10 reaches it.
    assert_eq!(move_horizontally(&mut source, 8.0, ms(1)).failure, None);
    let held = move_horizontally(&mut source, -12.0, ms(2));
    assert_frame(
        held.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    assert_eq!(move_horizontally(&mut source, 10.0, ms(3)).failure, None);
    let arrival = move_horizontally(&mut source, -13.0, ms(4));
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        4,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let returned = push_through(&mut source, Point::new(-1.0, 0.0), ms(4));
    assert_frame(
        returned.effects.iter().next().unwrap(),
        4,
        5,
        Message::ReleaseAll,
    );
}

#[test]
fn a_capture_that_cannot_take_over_yet_logs_its_own_return_reason() {
    capture_log();
    let mut source = source();
    move_to(&mut source, Point::new(99.0, 50.0), ms(0));
    assert!(sends_activation(&push_through(
        &mut source,
        Point::new(1.0, 0.0),
        ms(0)
    )));
    let acknowledged = source.on_remote_frame(&activation_ack(), ms(0));
    let refused = source.refuse_capture_activation(route_request(&acknowledged), ms(1));
    assert_eq!(refused.failure, None);
    assert!(sends_release_all(&refused));
    assert_eq!(
        logged("return: this computer's capture could not take over input yet"),
        1
    );
    assert_eq!(logged("declined"), 0);
}

#[test]
fn declines_log_once_per_streak_of_seam_pressure_with_their_reason() {
    fn press(source: &mut SourceController, at: u64) -> SourceOutcome {
        move_horizontally(source, PUSH_THROUGH_DISTANCE, ms(at))
    }
    fn decline(source: &mut SourceController, epoch: u64, reason: DeclineReason, at: u64) {
        let frame = Frame::new(
            SessionEpoch::new(epoch).unwrap(),
            0,
            Message::ActivationDeclined {
                display_id: DisplayId(2),
                reason,
            },
        );
        assert_eq!(source.on_remote_frame(&frame, ms(at)).failure, None);
        assert_eq!(source.mode(), SourceMode::Local);
    }
    capture_log();
    let retry = u64::try_from(DECLINE_RETRY_AFTER.as_millis()).unwrap();
    let mut seam = source();
    move_to(&mut seam, Point::new(99.0, 50.0), ms(0));
    assert!(sends_activation(&push_through(
        &mut seam,
        Point::new(1.0, 0.0),
        ms(0)
    )));
    decline(&mut seam, 4, DeclineReason::Busy, 0);
    // Pushing on retries every DECLINE_RETRY_AFTER; the same answer is not logged again.
    assert!(press(&mut seam, retry / 2).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, retry)));
    decline(&mut seam, 5, DeclineReason::Busy, retry);
    assert_eq!(logged("crossing declined by the other computer (Busy)"), 1);
    // A different answer is logged.
    assert!(press(&mut seam, retry * 3 / 2).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, retry * 2)));
    decline(&mut seam, 6, DeclineReason::Disabled, retry * 2);
    assert_eq!(
        logged("crossing declined by the other computer (Disabled)"),
        1
    );
    // Letting go of the seam ends the streak.
    let pause = retry * 2 + 10;
    move_to(&mut seam, Point::new(50.0, 50.0), ms(pause));
    move_to(&mut seam, Point::new(99.0, 50.0), ms(pause + 1));
    assert!(
        move_horizontally(&mut seam, 1.0, ms(pause + 1))
            .effects
            .is_empty()
    );
    assert!(press(&mut seam, pause + 2).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, pause + 2 + retry)));
    decline(&mut seam, 7, DeclineReason::Disabled, pause + 2 + retry);
    assert_eq!(
        logged("crossing declined by the other computer (Disabled)"),
        2
    );
    assert_eq!(logged("return:"), 0, "control never left this computer");

    // A declined hop between the other computer's displays does bring control home.
    let mut hop = source();
    activate_remote(&mut hop);
    assert_eq!(move_horizontally(&mut hop, 98.0, ms(1)).failure, None);
    assert!(sends_activation(&press(&mut hop, 2)));
    let frame = Frame::new(
        SessionEpoch::new(5).unwrap(),
        0,
        Message::ActivationDeclined {
            display_id: DisplayId(3),
            reason: DeclineReason::Contended,
        },
    );
    let declined = hop.on_remote_frame(&frame, ms(3));
    assert_eq!(declined.failure, None);
    assert!(sends_release_all(&declined));
    assert_eq!(
        logged("return: the other computer declined control (Contended)"),
        1
    );
}

#[test]
fn a_fast_flick_onto_a_linked_edge_stops_there_however_far_it_overshoots() {
    // A clamped OS cursor: the absolute half lands on the edge, the relative half overshoots.
    let mut clamped = source();
    move_to(&mut clamped, Point::new(50.0, 50.0), ms(0));
    move_to(&mut clamped, Point::new(99.0, 50.0), ms(1));
    let overshoot = move_horizontally(&mut clamped, 500.0, ms(1));
    assert_eq!(overshoot.failure, None);
    assert!(overshoot.effects.is_empty());
    assert!(
        move_horizontally(&mut clamped, 1.0, ms(2))
            .effects
            .is_empty()
    );
    assert_eq!(clamped.mode(), SourceMode::Local);

    // A hook sample far past the edge, then its relative half: the arrival's depth never counts.
    let mut unclamped = source();
    move_to(&mut unclamped, Point::new(50.0, 50.0), ms(0));
    move_to(&mut unclamped, Point::new(600.0, 50.0), ms(1));
    assert!(
        move_horizontally(&mut unclamped, 550.0, ms(1))
            .effects
            .is_empty()
    );
    move_to(&mut unclamped, Point::new(600.0, 50.0), ms(2));
    assert!(
        move_horizontally(&mut unclamped, 1.0, ms(2))
            .effects
            .is_empty()
    );
    assert_eq!(unclamped.mode(), SourceMode::Local);

    // The pointer on the other computer stops on the entry edge it flicks back to.
    let mut remote = source();
    activate_remote(&mut remote);
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, ms(1));
    let flick = move_horizontally(&mut remote, -500.0, ms(2));
    assert_frame(
        flick.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    assert!(!sends_release_all(&flick));
    assert!(
        move_horizontally(&mut remote, -1.0, ms(3))
            .effects
            .is_empty()
    );
    assert_eq!(remote.mode(), SourceMode::Remote);
}

#[test]
fn a_push_crosses_only_once_it_reaches_the_full_distance() {
    let mut steps = source();
    rest_on_right_edge(&mut steps, ms(0));
    for _ in 1..PUSH_THROUGH_DISTANCE as u32 {
        let step = move_horizontally(&mut steps, 1.0, ms(1));
        assert_eq!(step.failure, None);
        assert!(step.effects.is_empty());
    }
    assert!(sends_activation(&move_horizontally(&mut steps, 1.0, ms(1))));

    let mut short = source();
    rest_on_right_edge(&mut short, ms(0));
    let pushed = move_horizontally(&mut short, PUSH_THROUGH_DISTANCE - 1.0, ms(1));
    assert_eq!(pushed.failure, None);
    assert!(pushed.effects.is_empty());
    assert_eq!(short.mode(), SourceMode::Local);

    let mut full = source();
    rest_on_right_edge(&mut full, ms(0));
    assert!(sends_activation(&move_horizontally(
        &mut full,
        PUSH_THROUGH_DISTANCE,
        ms(1)
    )));
}

#[test]
fn leaving_the_edge_inward_starts_the_push_over() {
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    assert!(
        move_horizontally(&mut local, PUSH_THROUGH_DISTANCE - 1.0, ms(1))
            .effects
            .is_empty()
    );
    move_to(&mut local, Point::new(97.0, 50.0), ms(2));
    move_to(&mut local, Point::new(99.0, 50.0), ms(3));
    assert!(move_horizontally(&mut local, 1.0, ms(3)).effects.is_empty());
    assert!(
        move_horizontally(&mut local, PUSH_THROUGH_DISTANCE - 1.0, ms(4))
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(&mut local, 1.0, ms(4))));

    let mut remote = source();
    activate_remote(&mut remote);
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, ms(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, ms(2));
    let pushed = move_horizontally(&mut remote, 1.0 - PUSH_THROUGH_DISTANCE, ms(3));
    assert!(pushed.effects.is_empty());
    // Two px inward is off the edge; coming back is a new arrival.
    move_horizontally(&mut remote, 2.0, ms(4));
    move_horizontally(&mut remote, -2.0, ms(5));
    let again = move_horizontally(&mut remote, 1.0 - PUSH_THROUGH_DISTANCE, ms(6));
    assert!(!sends_release_all(&again));
    assert!(sends_release_all(&move_horizontally(
        &mut remote,
        -1.0,
        ms(6)
    )));
}

#[test]
fn a_pause_in_the_push_starts_it_over() {
    let mut paused = source();
    rest_on_right_edge(&mut paused, ms(0));
    assert!(
        move_horizontally(&mut paused, PUSH_THROUGH_DISTANCE - 1.0, ms(0))
            .effects
            .is_empty()
    );
    stay_alive(&mut paused, 3, ms(400));
    let late = PUSH_THROUGH_RESET + ms(1);
    assert!(move_horizontally(&mut paused, 1.0, late).effects.is_empty());
    assert!(sends_activation(&move_horizontally(
        &mut paused,
        PUSH_THROUGH_DISTANCE - 1.0,
        late
    )));

    let mut brief = source();
    rest_on_right_edge(&mut brief, ms(0));
    assert!(
        move_horizontally(&mut brief, PUSH_THROUGH_DISTANCE - 1.0, ms(0))
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(
        &mut brief,
        1.0,
        PUSH_THROUGH_RESET - ms(1)
    )));
}

#[test]
fn sliding_along_the_edge_keeps_the_push() {
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    let along = |dx: f64, dy: f64| NormalizedInput::RelativeMotion(Point::new(dx, dy));
    assert!(
        capture_on_route(&mut local, along(40.0, 0.0), ms(1))
            .effects
            .is_empty()
    );
    move_to(&mut local, Point::new(99.0, 70.0), ms(2));
    assert!(
        capture_on_route(&mut local, along(0.0, 10.0), ms(2))
            .effects
            .is_empty()
    );
    // Only the outward part of a diagonal push counts.
    assert!(
        capture_on_route(&mut local, along(39.0, 5.0), ms(3))
            .effects
            .is_empty()
    );
    let crossing = capture_on_route(&mut local, along(1.0, 0.0), ms(3));
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 70.0),
        },
    );

    let mut remote = source();
    activate_remote(&mut remote);
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, ms(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, ms(2));
    let slid = capture_on_route(&mut remote, along(-40.0, 10.0), ms(3));
    assert_frame(
        slid.effects.iter().next().unwrap(),
        4,
        3,
        Message::Motion(Motion::Absolute(Point::new(0.0, 60.0))),
    );
    capture_on_route(&mut remote, along(0.0, 10.0), ms(4));
    let short = capture_on_route(&mut remote, along(-39.0, 0.0), ms(5));
    assert!(short.effects.is_empty());
    assert!(sends_release_all(&capture_on_route(
        &mut remote,
        along(-1.0, 0.0),
        ms(5)
    )));
    assert_eq!(
        complete_return(&mut remote, 1, ms(6)),
        (DisplayId(1), Point::new(99.0, 70.0))
    );
}

#[test]
fn returning_home_takes_the_same_push_while_the_remote_pointer_stays_on_the_edge() {
    let mut source = source();
    let mut bridge = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    move_to(&mut source, Point::new(99.0, 50.0), ms(0));
    let enter = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    bridge.pump(&mut source, enter, ms(0)).unwrap();
    for (at, dx) in [(1, ENTRY_GUARD_DISTANCE), (2, -ENTRY_GUARD_DISTANCE - 3.0)] {
        let moved = move_horizontally(&mut source, dx, ms(at));
        bridge.pump(&mut source, moved, ms(at)).unwrap();
    }
    for _ in 1..PUSH_THROUGH_DISTANCE as u32 {
        let pushed = move_horizontally(&mut source, -1.0, ms(3));
        assert!(!sends_release_all(&pushed));
        bridge.pump(&mut source, pushed, ms(3)).unwrap();
        assert_eq!(source.mode(), SourceMode::Remote);
    }
    let through = move_horizontally(&mut source, -1.0, ms(3));
    bridge.pump(&mut source, through, ms(3)).unwrap();
    assert_eq!(source.mode(), SourceMode::Local);
    // The receiver saw the pointer move away and back onto the edge line, never past it.
    let motions: Vec<_> = bridge
        .sent
        .iter()
        .filter_map(|frame| match &frame.message {
            Message::Motion(Motion::Absolute(point)) => Some(*point),
            _ => None,
        })
        .collect();
    assert_eq!(
        motions,
        vec![
            Point::new(1.0 + ENTRY_GUARD_DISTANCE, 50.0),
            Point::new(0.0, 50.0)
        ]
    );
    assert!(
        bridge
            .destination
            .actions
            .iter()
            .all(|action| match action {
                RecordedAction::Move(point) => point.x >= 0.0,
                _ => true,
            })
    );
}

#[test]
fn absolute_samples_push_through_by_how_far_they_go_past_where_they_arrived() {
    // Space outside the layout past display 4, which the pointer arrives in 30 px deep.
    let mut deep = source();
    move_to(&mut deep, Point::new(150.0, 50.0), ms(0));
    move_to(&mut deep, Point::new(230.0, 50.0), ms(1));
    move_to(
        &mut deep,
        Point::new(230.0 + PUSH_THROUGH_DISTANCE - 1.0, 60.0),
        ms(2),
    );
    let crossing = capture_on_route(
        &mut deep,
        NormalizedInput::AbsoluteMotion(Point::new(230.0 + PUSH_THROUGH_DISTANCE, 60.0)),
        ms(3),
    );
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(1.0, 60.0),
        },
    );

    // An absolute sample and its relative half measure the same travel, not twice as much.
    let mut paired = source();
    move_to(&mut paired, Point::new(150.0, 50.0), ms(0));
    move_to(&mut paired, Point::new(199.0, 50.0), ms(1));
    assert!(
        move_horizontally(&mut paired, 1.0, ms(1))
            .effects
            .is_empty()
    );
    let crossed_at = (1..=10).find(|step| {
        let depth = f64::from(*step) * 10.0;
        let sample = capture_on_route(
            &mut paired,
            NormalizedInput::AbsoluteMotion(Point::new(200.0 + depth, 50.0)),
            ms(2),
        );
        sends_activation(&sample) || sends_activation(&move_horizontally(&mut paired, 10.0, ms(2)))
    });
    assert!(
        matches!(crossed_at, Some(7..=8)),
        "{crossed_at:?} steps of 10 px"
    );
}

#[test]
fn a_guarded_entry_edge_takes_no_push_until_the_pointer_moves_away() {
    let mut source = source();
    activate_remote(&mut source);
    for step in 1..=3 {
        let shoved = move_horizontally(&mut source, -2.0 * PUSH_THROUGH_DISTANCE, ms(step));
        assert!(!sends_release_all(&shoved));
    }
    // 22 px in and back keeps the guard, so the push still takes nothing.
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE - 2.0, ms(4));
    move_horizontally(&mut source, 2.0 - ENTRY_GUARD_DISTANCE, ms(5));
    assert!(!sends_release_all(&move_horizontally(
        &mut source,
        -2.0 * PUSH_THROUGH_DISTANCE,
        ms(6)
    )));
    assert_eq!(source.mode(), SourceMode::Remote);
    // 24 px in releases it; back on the edge the full push returns.
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, ms(7));
    move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE, ms(8));
    assert!(
        move_horizontally(&mut source, 1.0 - PUSH_THROUGH_DISTANCE, ms(9))
            .effects
            .is_empty()
    );
    assert!(sends_release_all(&move_horizontally(
        &mut source,
        -1.0,
        ms(9)
    )));
}

trait FreshCapture {
    fn fresh_capture(&mut self, record: TaggedInput, now: Duration) -> SourceOutcome;
}
impl FreshCapture for SourceController {
    fn fresh_capture(&mut self, mut record: TaggedInput, now: Duration) -> SourceOutcome {
        record.floor_generation = self.floor_generation();
        self.on_captured(record, now)
    }
}
