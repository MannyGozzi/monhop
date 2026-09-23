use std::{cell::RefCell, sync::Once, time::Duration};

use monhop_core::capture::DECLINE_RETRY_AFTER;
use monhop_core::clicks::DOUBLE_CLICK_SLOP;
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
    ENTRY_GUARD_DISTANCE, NormalizedInput, PUSH_THROUGH_DISTANCE, PUSH_THROUGH_LOGGED,
    PUSH_THROUGH_RECENT, PUSH_THROUGH_RESET, PUSH_THROUGH_SETTLE, SourceController, SourceEffect,
    SourceFailure, SourceMode, SourceOutcome, TaggedInput,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// `value` ms after a crossing whose push began at zero, as [`activate_remote`]'s does.
fn after_crossing(value: u64) -> Duration {
    PUSH_THROUGH_SETTLE + ms(value)
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
    Button(MouseButton, bool, u8),
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
            DestinationAction::Button {
                button,
                pressed,
                click_count,
            } => RecordedAction::Button(button, pressed, click_count),
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

/// Arrives at `at` on the local display at zero, pushes through along the unit `push` and
/// completes the activation of display 2, which the pointer enters at `entry`, at
/// `after_crossing(0)`.
fn activate_remote_from(source: &mut SourceController, at: Point, push: Point, entry: Point) {
    assert!(
        source
            .fresh_capture(local(NormalizedInput::AbsoluteMotion(at)), ms(0))
            .effects
            .is_empty()
    );
    let edge = push_through(source, push, ms(0));
    let now = after_crossing(0);
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: entry,
        },
    );
    let acknowledged = source.on_remote_frame(&activation_ack(), now);
    assert_eq!(acknowledged.failure, None);
    assert_eq!(acknowledged.effects.iter().count(), 1);
    assert!(matches!(
        acknowledged.effects.iter().next(),
        Some(SourceEffect::ActivateRemote { .. })
    ));
    assert_eq!(
        source
            .bind_capture_route(route_request(&acknowledged), 1, now)
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
        now,
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

fn move_vertically(source: &mut SourceController, dy: f64, now: Duration) -> SourceOutcome {
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(Point::new(0.0, dy)),
        now,
    )
}

fn scaled(push: Point, by: f64) -> Point {
    Point::new(push.x * by, push.y * by)
}

/// Pushes one px along the unit `push` every 10 ms from `from` until the push has settled at
/// `from + PUSH_THROUGH_SETTLE`; none of it crosses.
fn settle_push(source: &mut SourceController, push: Point, from: Duration) {
    let mut now = from;
    while now < from + PUSH_THROUGH_SETTLE {
        let held = capture_on_route(source, NormalizedInput::RelativeMotion(push), now);
        assert_eq!(held.failure, None);
        assert!(
            !sends_activation(&held) && !sends_release_all(&held),
            "an unsettled push never crosses"
        );
        now += ms(10);
    }
}

/// From a pointer resting on an edge: a push along the unit `push` that settles, then the full
/// distance at `now + PUSH_THROUGH_SETTLE`, whose outcome it yields.
fn push_through(source: &mut SourceController, push: Point, now: Duration) -> SourceOutcome {
    settle_push(source, push, now);
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(scaled(push, PUSH_THROUGH_DISTANCE)),
        now + PUSH_THROUGH_SETTLE,
    )
}

/// Pushes `by` every 10 ms from `from` and last at `from + PUSH_THROUGH_SETTLE`, once settled, long
/// and far enough to cross any open edge; true if any of it crossed.
fn sustained_push_crosses(source: &mut SourceController, by: Point, from: Duration) -> bool {
    let settled = from + PUSH_THROUGH_SETTLE;
    let mut crossed = false;
    let mut now = from;
    loop {
        let pushed = capture_on_route(source, NormalizedInput::RelativeMotion(by), now);
        assert_eq!(pushed.failure, None);
        crossed |= sends_activation(&pushed) || sends_release_all(&pushed);
        if now == settled {
            return crossed;
        }
        now = (now + ms(10)).min(settled);
    }
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

/// Samples `sample` from `from`, each `by` further, every 50 ms from `at` while before `until`;
/// yields the last point sampled.
fn creep(
    from: Point,
    by: Point,
    at: Duration,
    until: Duration,
    mut sample: impl FnMut(Point, Duration),
) -> Point {
    let mut point = from;
    let mut now = at;
    loop {
        sample(point, now);
        if now + ms(50) >= until {
            return point;
        }
        point = Point::new(point.x + by.x, point.y + by.y);
        now += ms(50);
    }
}

/// Local absolute samples from `from` that creep `step` further along the unit `push` every
/// 10 ms until a push begun at `now` has settled, then one the full distance on, whose outcome it
/// yields.
fn absolute_push_through(
    source: &mut SourceController,
    from: Point,
    push: Point,
    step: f64,
    now: Duration,
) -> SourceOutcome {
    let mut point = from;
    let mut at = now + ms(10);
    while at < now + PUSH_THROUGH_SETTLE {
        let by = scaled(push, step);
        point = Point::new(point.x + by.x, point.y + by.y);
        move_to(source, point, at);
        at += ms(10);
    }
    let by = scaled(push, PUSH_THROUGH_DISTANCE);
    capture_on_route(
        source,
        NormalizedInput::AbsoluteMotion(Point::new(point.x + by.x, point.y + by.y)),
        now + PUSH_THROUGH_SETTLE,
    )
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

/// Where `outcome` last moves the other computer's pointer to, if it does.
fn moved_to(outcome: &SourceOutcome) -> Option<Point> {
    outcome
        .effects
        .iter()
        .filter_map(|effect| match effect {
            SourceEffect::RemoteFrame(Frame {
                message: Message::Motion(Motion::Absolute(at)),
                ..
            }) => Some(*at),
            _ => None,
        })
        .last()
}

/// The display `outcome` asks the other computer to activate, and where.
fn activation(outcome: &SourceOutcome) -> Option<(DisplayId, Point)> {
    outcome.effects.iter().find_map(|effect| match effect {
        SourceEffect::RemoteFrame(Frame {
            message:
                Message::ActivateDisplayAt {
                    display_id,
                    position,
                },
            ..
        }) => Some((*display_id, *position)),
        _ => None,
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
        after_crossing(2),
    );
    assert_eq!(pong.failure, None);
    let activation = source.on_remote_frame(&activation_ack(), after_crossing(3));
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
    let acknowledged = source.on_remote_frame(&activation_ack(), after_crossing(0));
    source.bind_capture_route(route_request(&acknowledged), 1, after_crossing(0));

    let held_local = source.fresh_capture(
        local(NormalizedInput::Key {
            usage: HidUsage(0x04),
            pressed: true,
            repeat: false,
            modifiers: ModifierState(0),
        }),
        after_crossing(0),
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
        after_crossing(0),
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
        after_crossing(1),
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
        after_crossing(2),
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
    let acknowledged = source.on_remote_frame(&activation_ack(), after_crossing(0));
    let request = route_request(&acknowledged);
    assert_ne!(request.get(), 9);
    assert_eq!(
        source
            .bind_capture_route(request, 9, after_crossing(0))
            .failure,
        None
    );
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
                after_crossing(1),
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
        after_crossing(1),
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
        after_crossing(2),
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
        move_horizontally(&mut returning, ENTRY_GUARD_DISTANCE, after_crossing(1)).failure,
        None
    );
    // Reaching the entry edge stops there; the push after it returns.
    let arrival = move_horizontally(
        &mut returning,
        -ENTRY_GUARD_DISTANCE - 3.0,
        after_crossing(1),
    );
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let release = push_through(&mut returning, Point::new(-1.0, 0.0), after_crossing(1));
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
    let back = after_crossing(1) + PUSH_THROUGH_SETTLE;
    let restore = returning.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::ReleaseAck),
        back + ms(1),
    );
    assert!(matches!(
        restore.effects.iter().next(),
        Some(SourceEffect::RestoreLocalAt {
            display: DisplayId(1),
            position,
            ..
        }) if *position == Point::new(99.0, 50.0)
    ));
    returning.bind_capture_route(route_request(&restore), 2, back + ms(1));
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
                back + ms(2),
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
        after_crossing(1),
    );
    for pressed in [true, false, true] {
        remote_to_remote.fresh_capture(
            routed(
                NormalizedInput::Button {
                    button: MouseButton::Left,
                    pressed,
                    at: after_crossing(2),
                },
                true,
                1,
            ),
            after_crossing(2),
        );
    }
    // Reaching the last pixel before the other computer's next display stays on this one; going
    // on past it moves there at once, as that computer's own pointer would.
    let last_pixel = remote_to_remote.fresh_capture(
        routed(
            NormalizedInput::RelativeMotion(Point::new(98.0, 0.0)),
            true,
            1,
        ),
        after_crossing(3),
    );
    assert_eq!(last_pixel.failure, None);
    assert!(!sends_activation(&last_pixel));
    let hop = after_crossing(4);
    let handoff = move_horizontally(&mut remote_to_remote, 1.0, hop);
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
        remote_to_remote.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), hop + ms(1));
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
        hop + ms(2),
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
                at: hop + ms(3),
            },
            true,
            1,
        ),
        hop + ms(3),
    );
    assert_frame(
        button_release.effects.iter().next().unwrap(),
        5,
        2,
        Message::Button(monhop_protocol::Button {
            button: MouseButton::Left,
            is_down: false,
            click_count: 2,
        }),
    );
    let resumed = remote_to_remote.fresh_capture(
        routed(
            NormalizedInput::RelativeMotion(Point::new(1.0, 0.0)),
            true,
            1,
        ),
        hop + ms(4),
    );
    assert_frame(
        resumed.effects.iter().next().unwrap(),
        5,
        3,
        Message::Motion(Motion::Absolute(Point::new(2.0, 50.0))),
    );
}

/// The other computer's displays 2 and 3 sit side by side, linked both ways; display 2's left edge
/// is the seam with this computer's display 1.
fn topology_with_two_displays_side_by_side_on_the_other_computer() -> Topology {
    two_machine_topology(
        vec![
            display_at(1, 1, Point::new(0.0, 0.0), 100),
            display_at(2, 2, Point::new(0.0, 0.0), 100),
            display_at(3, 2, Point::new(100.0, 0.0), 100),
        ],
        vec![
            full_link(1, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 1, Edge::Right),
            full_link(2, Edge::Right, 3, Edge::Left),
            full_link(3, Edge::Left, 2, Edge::Right),
        ],
    )
}

/// Swipes by `speed` px a ms for up to 100 ms from `from`; yields the report, in ms, that moved
/// the pointer onto another display, and that display.
fn swipe(source: &mut SourceController, speed: f64, from: Duration) -> Option<(u64, DisplayId)> {
    reports(100, |t| Point::new(speed * t, 0.0))
        .into_iter()
        .find_map(|(t, counts)| {
            let swiped = capture_on_route(
                source,
                NormalizedInput::RelativeMotion(counts),
                from + ms(t),
            );
            assert_eq!(swiped.failure, None);
            activation(&swiped).map(|(display, _)| (t, display))
        })
}

#[test]
fn the_controlled_computers_own_display_boundaries_cross_on_arrival_but_the_seam_takes_a_push() {
    let mut source = source_on(
        topology_with_two_displays_side_by_side_on_the_other_computer(),
        DisplayId(1),
    );
    activate_remote(&mut source);
    stay_alive(&mut source, 3, after_crossing(0));
    // From x 1, the 33rd report of a 3 px a ms swipe is the first past display 2's last pixel.
    assert_eq!(
        swipe(&mut source, 3.0, after_crossing(1)),
        Some((33, DisplayId(3)))
    );
    let ack = source.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), after_crossing(35));
    assert!(ack.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Remote);
    // The entered edge's guard keeps a small move back on display 3.
    let back = move_horizontally(&mut source, -5.0, after_crossing(36));
    assert!(!sends_activation(&back));
    assert_eq!(moved_to(&back), Some(Point::new(100.0, 50.0)));
    // Swiping back from 40 px into display 3, the 14th report is the first past its left edge,
    // by 2 px that carry on from display 2's entry point once the hop lands.
    assert_eq!(
        move_horizontally(&mut source, 40.0, after_crossing(37)).failure,
        None
    );
    assert_eq!(
        swipe(&mut source, -3.0, after_crossing(38)),
        Some((14, DisplayId(2)))
    );
    let ack = source.on_remote_frame(&activation_ack_for(6, 0, DisplayId(2)), after_crossing(53));
    assert_eq!(moved_to(&ack), Some(Point::new(97.0, 50.0)));
    // The seam home on display 2's far edge still takes the full push.
    assert_eq!(
        move_horizontally(&mut source, -97.0, after_crossing(54)).failure,
        None
    );
    stay_alive(&mut source, 4, after_crossing(55));
    settle_push(&mut source, Point::new(-1.0, 0.0), after_crossing(55));
    let settled = after_crossing(55) + PUSH_THROUGH_SETTLE;
    let short = move_horizontally(&mut source, 1.0 - PUSH_THROUGH_DISTANCE, settled);
    assert!(short.effects.is_empty());
    assert!(sends_release_all(&move_horizontally(
        &mut source,
        -1.0,
        settled
    )));
}

/// Swipes right at 3 px a ms for `duration` ms from x 2 on display 2, crossing onto display 3 on
/// the 33rd report, 1 px past the boundary; the hop lands `ack_after` ms after that report.
/// Yields where the swipe ends.
fn swipe_across_a_hop(duration: u64, ack_after: u64) -> Point {
    let mut source = source_on(
        topology_with_two_displays_side_by_side_on_the_other_computer(),
        DisplayId(1),
    );
    activate_remote(&mut source);
    stay_alive(&mut source, 3, after_crossing(0));
    let mut at = moved_to(&move_horizontally(&mut source, 1.0, after_crossing(1))).unwrap();
    let mut hopped = None;
    for (t, counts) in reports(duration, |t| Point::new(3.0 * t, 0.0)) {
        let now = after_crossing(2) + ms(t);
        let swiped = capture_on_route(&mut source, NormalizedInput::RelativeMotion(counts), now);
        assert_eq!(swiped.failure, None);
        at = moved_to(&swiped).unwrap_or(at);
        if sends_activation(&swiped) {
            assert_eq!(t, 33);
            hopped = Some(t);
        }
        if hopped.is_some_and(|hop| t < hop + ack_after) {
            // Relative motion still has a display to land on while the hop is in flight.
            let landing = source.motion_target().map(|motion| motion.target.display);
            assert_eq!(landing, Some(DisplayId(3)));
        } else if hopped.is_some_and(|hop| t == hop + ack_after) {
            let ack = source.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), now);
            assert_eq!(ack.failure, None);
            at = moved_to(&ack).unwrap_or(at);
        }
    }
    assert_eq!(source.mode(), SourceMode::Remote);
    at
}

#[test]
fn motion_while_a_hop_is_in_flight_carries_on_from_where_it_lands() {
    // From x 2, 60 reports of 3 px end 180 px on, and 1 px more for the entry point's inset,
    // whenever the hop onto display 3 lands.
    let prompt = swipe_across_a_hop(60, 0);
    assert_eq!(prompt, Point::new(183.0, 50.0));
    assert_eq!(swipe_across_a_hop(60, 20), prompt);
    // Carried motion that runs past display 3 stops on its far edge, as the swipe itself does.
    let far = swipe_across_a_hop(70, 0);
    assert_eq!(far, Point::new(200.0_f64.next_down(), 50.0));
    assert_eq!(swipe_across_a_hop(70, 36), far);
}

#[test]
fn a_button_changed_while_a_hop_is_in_flight_lands_where_the_hand_was_then() {
    let messages = |outcome: &SourceOutcome| -> Vec<Message> {
        outcome
            .effects
            .iter()
            .filter_map(|effect| match effect {
                SourceEffect::RemoteFrame(frame) => Some(frame.message.clone()),
                _ => None,
            })
            .collect()
    };
    let left = |pressed: bool, at: Duration| NormalizedInput::Button {
        button: MouseButton::Left,
        pressed,
        at,
    };
    // From x 99 on display 2, 3 px goes 2 px past it onto display 3, then 3 px more in flight.
    let hop = |held: bool| {
        let mut source = source_on(
            topology_with_two_displays_side_by_side_on_the_other_computer(),
            DisplayId(1),
        );
        activate_remote(&mut source);
        stay_alive(&mut source, 3, after_crossing(0));
        if held {
            capture_on_route(
                &mut source,
                left(true, after_crossing(1)),
                after_crossing(1),
            );
        }
        move_horizontally(&mut source, 98.0, after_crossing(1));
        assert!(sends_activation(&move_horizontally(
            &mut source,
            3.0,
            after_crossing(2)
        )));
        assert!(
            move_horizontally(&mut source, 3.0, after_crossing(3))
                .effects
                .is_empty()
        );
        source
    };
    let moved = |x: f64| Message::Motion(Motion::Absolute(Point::new(x, 50.0)));
    let button = |is_down: bool| {
        Message::Button(monhop_protocol::Button {
            button: MouseButton::Left,
            is_down,
            click_count: 1,
        })
    };
    // Pressing or letting go in flight, then 3 px more: the change lands where the hand was,
    // 5 px into display 3, and the rest of the move follows it.
    for held in [false, true] {
        let mut source = hop(held);
        let changed = capture_on_route(
            &mut source,
            left(!held, after_crossing(4)),
            after_crossing(4),
        );
        assert!(changed.effects.is_empty());
        move_horizontally(&mut source, 3.0, after_crossing(5));
        let ack =
            source.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), after_crossing(6));
        assert_eq!(
            messages(&ack),
            [moved(106.0), button(!held), moved(109.0)],
            "held: {held}"
        );
    }
    // A drag held through the hop carries on with it.
    let mut source = hop(true);
    move_horizontally(&mut source, 3.0, after_crossing(5));
    let ack = source.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), after_crossing(6));
    assert_eq!(messages(&ack), [moved(109.0)]);
}

/// Hops from display 2 onto display 3 and carries 30 px in flight, by `after_crossing(12)`.
fn in_flight_to_display_3(alive: bool) -> SourceController {
    let mut source = source_on(
        topology_with_two_displays_side_by_side_on_the_other_computer(),
        DisplayId(1),
    );
    activate_remote(&mut source);
    if alive {
        stay_alive(&mut source, 3, after_crossing(0));
    }
    assert_eq!(
        move_horizontally(&mut source, 98.0, after_crossing(1)).failure,
        None
    );
    assert!(sends_activation(&move_horizontally(
        &mut source,
        3.0,
        after_crossing(2)
    )));
    for t in 3..=12 {
        let carried = move_horizontally(&mut source, 3.0, after_crossing(t));
        assert!(carried.effects.is_empty());
    }
    source
}

#[test]
fn a_hop_that_never_lands_moves_nothing() {
    // A hold in flight brings control home, and the acknowledgement arriving after it is ignored.
    let mut source = in_flight_to_display_3(false);
    let held = source.tick(RETREAT_AFTER);
    assert!(source.held_since().is_some());
    assert!(sends_release_all(&held));
    assert_eq!(moved_to(&held), None);
    let late = source.on_remote_frame(&activation_ack_for(5, 0, DisplayId(3)), RETREAT_AFTER);
    assert!(late.effects.is_empty());
    // So does a decline.
    let mut source = in_flight_to_display_3(true);
    let declined = source.on_remote_frame(
        &Frame::new(
            SessionEpoch::new(5).unwrap(),
            0,
            Message::ActivationDeclined {
                display_id: DisplayId(3),
                reason: DeclineReason::Contended,
            },
        ),
        after_crossing(13),
    );
    // Control comes home, and nothing carried moves the other computer's pointer.
    assert_eq!(declined.effects.iter().count(), 1);
    assert!(sends_release_all(&declined));
    let restore = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(5).unwrap(), 1, Message::ReleaseAck),
        after_crossing(14),
    );
    assert!(matches!(
        restore.effects.iter().next(),
        Some(SourceEffect::RestoreLocalAt {
            display: DisplayId(1),
            position,
            ..
        }) if *position == Point::new(99.0, 50.0)
    ));
}

/// This computer's display 1 sits above the other computer's display 2, whose top edge is the
/// seam home and whose right edge borders its display 3.
fn topology_with_the_seam_home_beside_another_display() -> Topology {
    two_machine_topology(
        vec![
            display_at(1, 1, Point::new(0.0, 0.0), 100),
            display_at(2, 2, Point::new(0.0, 0.0), 100),
            display_at(3, 2, Point::new(100.0, 0.0), 100),
        ],
        vec![
            full_link(1, Edge::Bottom, 2, Edge::Top),
            full_link(2, Edge::Top, 1, Edge::Bottom),
            full_link(2, Edge::Right, 3, Edge::Left),
            full_link(3, Edge::Left, 2, Edge::Right),
        ],
    )
}

/// Controls display 2 of [`topology_with_the_seam_home_beside_another_display`] with the pointer
/// on its seam home at `x`, the entry guard released, at `after_crossing(3)`.
fn on_the_seam_home_at(x: f64) -> SourceController {
    let mut source = source_on(
        topology_with_the_seam_home_beside_another_display(),
        DisplayId(1),
    );
    activate_remote_from(
        &mut source,
        Point::new(50.0, 99.0),
        Point::new(0.0, 1.0),
        Point::new(50.0, 1.0),
    );
    stay_alive(&mut source, 3, after_crossing(0));
    for (delta, at) in [
        (Point::new(0.0, ENTRY_GUARD_DISTANCE), 1),
        (Point::new(x - 50.0, 0.0), 2),
        (Point::new(0.0, -ENTRY_GUARD_DISTANCE - 1.0), 3),
    ] {
        let moved = capture_on_route(
            &mut source,
            NormalizedInput::RelativeMotion(delta),
            after_crossing(at),
        );
        assert!(!sends_activation(&moved) && !sends_release_all(&moved));
    }
    source
}

#[test]
fn a_push_home_keeps_its_corner_with_another_of_the_controlled_computers_displays() {
    // Pushing home up at 1 count a ms from 10 px beside display 3, drifting 5 or 10 degrees
    // toward it: the pointer reaches the corner before the push crosses, and crosses home there.
    for degrees in [5.0_f64, 10.0] {
        let (drift, up) = degrees.to_radians().sin_cos();
        let mut source = on_the_seam_home_at(90.0);
        let mut hopped = false;
        let crossed = reports(CROSSING_BUDGET_MS, |t| Point::new(drift * t, -up * t))
            .into_iter()
            .find_map(|(t, counts)| {
                let pushed = capture_on_route(
                    &mut source,
                    NormalizedInput::RelativeMotion(counts),
                    after_crossing(3) + ms(t),
                );
                hopped |= sends_activation(&pushed);
                sends_release_all(&pushed).then_some(ms(t))
            });
        assert!(!hopped, "{degrees} degrees");
        assert!(
            crossed_in_budget(crossed),
            "{crossed:?} at {degrees} degrees"
        );
    }
    // Sliding along the seam onto display 3 moves there on the report that goes past display 2.
    let mut source = on_the_seam_home_at(90.0);
    let slid = reports(20, |t| Point::new(3.0 * t, -0.6 * t))
        .into_iter()
        .find_map(|(t, counts)| {
            let slid = capture_on_route(
                &mut source,
                NormalizedInput::RelativeMotion(counts),
                after_crossing(3) + ms(t),
            );
            activation(&slid).map(|(display, _)| (t, display))
        });
    assert_eq!(slid, Some((4, DisplayId(3))));
    // Turning from a push home in the corner toward display 3 moves there once the hand heads
    // that way.
    let mut source = on_the_seam_home_at(99.0);
    let push = after_crossing(4);
    for t in 0..100 {
        let pushed = move_vertically(&mut source, -1.0, push + ms(t));
        assert!(pushed.effects.is_empty());
    }
    let turned = push + ms(100);
    let hopped = (0..2 * PUSH_THROUGH_RECENT.as_millis() as u64)
        .find(|t| sends_activation(&move_horizontally(&mut source, 1.0, turned + ms(*t))));
    // Not on the first report, while the heading still points home.
    assert!(hopped > Some(0), "{hopped:?}");
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
    bridge.pump(&mut source, edge, after_crossing(0)).unwrap();
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
            after_crossing(step),
        );
        bridge
            .pump(&mut source, motion, after_crossing(step))
            .unwrap();
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
        after_crossing(78),
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
    bridge.pump(&mut source, edge, after_crossing(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    for (step, delta) in (1_u64..).zip([Point::new(0.0, 1_000.0), Point::new(1_000.0, 0.0)]) {
        let motion = capture_on_route(
            &mut source,
            NormalizedInput::RelativeMotion(delta),
            after_crossing(step),
        );
        bridge
            .pump(&mut source, motion, after_crossing(step))
            .unwrap();
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

/// Control of a 400 px wide display on the peer, delivered through a real receiver.
fn controlling_a_wide_peer_display() -> (SourceController, ReceiverBridge) {
    let topology = two_machine_topology(
        vec![
            display_at(1, 1, Point::new(0.0, 0.0), 100),
            display_at(2, 2, Point::new(100.0, 0.0), 400),
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
    let mut bridge = ReceiverBridge::new(
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(2),
            name: "remote".into(),
            native_width: 400,
            native_height: 100,
            logical_origin: Point::new(100.0, 0.0),
            logical_size: Point::new(400.0, 100.0),
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
    bridge.pump(&mut source, edge, after_crossing(0)).unwrap();
    assert_eq!(source.mode(), SourceMode::Remote);
    (source, bridge)
}

fn click(
    source: &mut SourceController,
    bridge: &mut ReceiverBridge,
    button: MouseButton,
    now: Duration,
) {
    click_captured(source, bridge, button, now, now);
}

/// A click captured at `captured` and drained from the capture queue at `drained`.
fn click_captured(
    source: &mut SourceController,
    bridge: &mut ReceiverBridge,
    button: MouseButton,
    captured: Duration,
    drained: Duration,
) {
    for pressed in [true, false] {
        let press = NormalizedInput::Button {
            button,
            pressed,
            at: captured,
        };
        let outcome = capture_on_route(source, press, drained);
        bridge.pump(source, outcome, drained).unwrap();
    }
}

fn nudge(source: &mut SourceController, bridge: &mut ReceiverBridge, by: Point, now: Duration) {
    let outcome = capture_on_route(source, NormalizedInput::RelativeMotion(by), now);
    bridge.pump(source, outcome, now).unwrap();
}

fn delivered_clicks(bridge: &ReceiverBridge) -> Vec<(MouseButton, bool, u8)> {
    bridge
        .destination
        .actions
        .iter()
        .filter_map(|action| match *action {
            RecordedAction::Button(button, pressed, count) => Some((button, pressed, count)),
            _ => None,
        })
        .collect()
}

const LEFT_CLICK: [(MouseButton, bool, u8); 2] =
    [(MouseButton::Left, true, 1), (MouseButton::Left, false, 1)];
const LEFT_DOUBLE: [(MouseButton, bool, u8); 2] =
    [(MouseButton::Left, true, 2), (MouseButton::Left, false, 2)];

#[test]
fn quick_presses_far_apart_on_the_peer_are_single_clicks_though_the_pinned_cursor_never_moved() {
    // Capture reports both presses at this computer's cursor, pinned at the crossed edge; only
    // the peer cursor the source tracks moves between them.
    let (mut source, mut bridge) = controlling_a_wide_peer_display();
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(10),
    );
    nudge(
        &mut source,
        &mut bridge,
        Point::new(200.0, 0.0),
        after_crossing(20),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(30),
    );
    assert_eq!(delivered_clicks(&bridge), [LEFT_CLICK, LEFT_CLICK].concat());
}

#[test]
fn a_double_click_with_hand_jitter_inside_the_slop_arrives_as_a_double() {
    let jitter = DOUBLE_CLICK_SLOP - 1.0;
    let (mut source, mut bridge) = controlling_a_wide_peer_display();
    nudge(
        &mut source,
        &mut bridge,
        Point::new(50.0, 0.0),
        after_crossing(5),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(10),
    );
    nudge(
        &mut source,
        &mut bridge,
        Point::new(jitter, -jitter),
        after_crossing(20),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(30),
    );
    assert_eq!(
        delivered_clicks(&bridge),
        [LEFT_CLICK, LEFT_DOUBLE].concat()
    );
}

#[test]
fn the_source_users_double_click_interval_numbers_its_presses() {
    let (mut source, mut bridge) = controlling_a_wide_peer_display();
    source.set_double_click_interval(ms(150));
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(10),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(160),
    );
    stay_alive_through(
        &mut source,
        &mut bridge,
        after_crossing(170),
        after_crossing(300),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(311),
    );
    assert_eq!(
        delivered_clicks(&bridge),
        [LEFT_CLICK, LEFT_DOUBLE, LEFT_CLICK].concat(),
        "151 ms is outside this user's interval, though inside the 500 ms fallback"
    );
}

/// Both sides answer each other's health challenges every 100 ms from `from` until `until`.
fn stay_alive_through(
    source: &mut SourceController,
    bridge: &mut ReceiverBridge,
    from: Duration,
    until: Duration,
) {
    let mut now = from;
    while now < until {
        let challenge = source.tick(now);
        bridge.pump(source, challenge, now).unwrap();
        if let Some(ping) = bridge.receiver.tick(now, &mut bridge.destination).unwrap() {
            let frame = bridge.response_frame(ping);
            let pong = source.on_remote_frame(&frame, now);
            bridge.pump(source, pong, now).unwrap();
        }
        now += ms(100);
    }
}

#[test]
fn presses_are_numbered_by_when_they_were_captured_not_drained() {
    let (mut source, mut bridge) = controlling_a_wide_peer_display();
    source.set_double_click_interval(ms(500));
    click_captured(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(0),
        after_crossing(150),
    );
    stay_alive_through(
        &mut source,
        &mut bridge,
        after_crossing(160),
        after_crossing(600),
    );
    click_captured(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(600),
        after_crossing(600),
    );
    let (mut late, mut late_bridge) = controlling_a_wide_peer_display();
    late.set_double_click_interval(ms(500));
    click_captured(
        &mut late,
        &mut late_bridge,
        MouseButton::Left,
        after_crossing(0),
        after_crossing(0),
    );
    stay_alive_through(
        &mut late,
        &mut late_bridge,
        after_crossing(100),
        after_crossing(600),
    );
    click_captured(
        &mut late,
        &mut late_bridge,
        MouseButton::Left,
        after_crossing(450),
        after_crossing(600),
    );
    assert_eq!(
        delivered_clicks(&bridge),
        [LEFT_CLICK, LEFT_CLICK].concat(),
        "captured 600 ms apart though drained 450 ms apart"
    );
    assert_eq!(
        delivered_clicks(&late_bridge),
        [LEFT_CLICK, LEFT_DOUBLE].concat(),
        "captured 450 ms apart though drained 600 ms apart, inside the 500 ms interval"
    );
}

#[test]
fn another_button_between_two_quick_presses_breaks_the_double() {
    let (mut source, mut bridge) = controlling_a_wide_peer_display();
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(10),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Right,
        after_crossing(20),
    );
    click(
        &mut source,
        &mut bridge,
        MouseButton::Left,
        after_crossing(30),
    );
    assert_eq!(
        delivered_clicks(&bridge),
        [
            LEFT_CLICK,
            [
                (MouseButton::Right, true, 1),
                (MouseButton::Right, false, 1)
            ],
            LEFT_CLICK,
        ]
        .concat()
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
        after_crossing(1),
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
                at: after_crossing(2),
            },
            true,
            1,
        ),
        after_crossing(2),
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
        after_crossing(3),
    );

    let release = source.request_local(after_crossing(4));
    assert_frame(
        release.effects.iter().next().unwrap(),
        4,
        4,
        Message::ReleaseAll,
    );
    assert_eq!(source.mode(), SourceMode::AwaitRemoteReleaseAcknowledgement);
    let ack = source.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::ReleaseAck),
        after_crossing(5),
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
    source.bind_capture_route(route_request(&ack), 2, after_crossing(5));

    let local_barrier = source.fresh_capture(
        routed(
            NormalizedInput::RouteChanged {
                remote: false,
                revision: 2,
            },
            false,
            2,
        ),
        after_crossing(6),
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
        after_crossing(1),
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
        after_crossing(1),
    );
    assert_eq!(mismatch.failure, Some(SourceFailure::RouteMismatch));
    assert_eq!(other.mode(), SourceMode::Failed);
}

#[test]
fn a_silent_peer_makes_a_remote_source_retreat_before_its_lease_can_expire() {
    let mut source = source();
    activate_remote(&mut source);
    let ping = source.tick(after_crossing(0));
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
        source
            .tick(after_crossing(0))
            .effects
            .iter()
            .next()
            .unwrap(),
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
    let wall = source.fresh_capture(
        routed(
            NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0)),
            false,
            2,
        ),
        ms(405),
    );
    assert_eq!(wall.failure, None);
    assert!(wall.effects.is_empty());
    settle_push(&mut source, Point::new(1.0, 0.0), ms(406));
    let settled = ms(406) + PUSH_THROUGH_SETTLE;
    // The next challenge goes out during the hold; its reply is fresh, but the barrier is still owed.
    let ping = source.tick(settled + ms(4));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 4, Message::Ping(2));
    let fresh = Frame::new(SessionEpoch::new(3).unwrap(), 4, Message::Pong(2));
    assert!(
        source
            .on_remote_frame(&fresh, settled + ms(14))
            .failure
            .is_none()
    );
    assert!(source.held_since().is_some());
    // Pushing through the seam after that fresh reply still keeps input local.
    let wall = move_horizontally(&mut source, PUSH_THROUGH_DISTANCE, settled + ms(19));
    assert_eq!(wall.failure, None);
    assert!(
        wall.effects.is_empty(),
        "{:?}",
        wall.effects.iter().collect::<Vec<_>>()
    );
    assert_eq!(source.mode(), SourceMode::Local);
    // The receiver's acknowledgement of the barrier ends the hold.
    let acknowledged = settled + ms(24);
    let ack = Frame::new(barrier.epoch, 1, Message::ReleaseAck);
    let resumed = source.on_remote_frame(&ack, acknowledged);
    assert!(resumed.failure.is_none());
    assert_eq!(source.held_since(), None);
    assert_eq!(
        source.hold_stats(acknowledged),
        (1, acknowledged - RETREAT_AFTER)
    );
    // Ordinary liveness resumes from the fresh reply: a new deadline, a new retreat.
    assert!(
        source
            .tick(acknowledged + RETREAT_AFTER - ms(1))
            .failure
            .is_none()
    );
    assert!(source.held_since().is_none());
    let again = source.tick(acknowledged + PEER_LIVENESS);
    assert_eq!(again.failure, None);
    assert!(source.held_since().is_some());
    assert_eq!(source.hold_stats(acknowledged + PEER_LIVENESS).0, 2);
}

#[test]
fn a_hold_without_the_link_coming_back_ends_at_the_limit() {
    let mut source = source();
    activate_remote(&mut source);
    source.tick(after_crossing(0));
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
        .pump(&mut windows_source, enter, after_crossing(0))
        .unwrap();
    assert_eq!(windows_source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut windows_source, ENTRY_GUARD_DISTANCE, after_crossing(1));
    mac_receiver
        .pump(&mut windows_source, away, after_crossing(1))
        .unwrap();
    let back = move_horizontally(
        &mut windows_source,
        -ENTRY_GUARD_DISTANCE - 3.0,
        after_crossing(1),
    );
    mac_receiver
        .pump(&mut windows_source, back, after_crossing(1))
        .unwrap();
    let returned = push_through(
        &mut windows_source,
        Point::new(-1.0, 0.0),
        after_crossing(1),
    );
    mac_receiver
        .pump(
            &mut windows_source,
            returned,
            after_crossing(1) + PUSH_THROUGH_SETTLE,
        )
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
        .pump(&mut mac_source, enter, after_crossing(0))
        .unwrap();
    assert_eq!(mac_source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut mac_source, -ENTRY_GUARD_DISTANCE, after_crossing(1));
    windows_receiver
        .pump(&mut mac_source, away, after_crossing(1))
        .unwrap();
    let back = move_horizontally(
        &mut mac_source,
        ENTRY_GUARD_DISTANCE + 3.0,
        after_crossing(1),
    );
    windows_receiver
        .pump(&mut mac_source, back, after_crossing(1))
        .unwrap();
    let returned = push_through(&mut mac_source, Point::new(1.0, 0.0), after_crossing(1));
    windows_receiver
        .pump(
            &mut mac_source,
            returned,
            after_crossing(1) + PUSH_THROUGH_SETTLE,
        )
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
            at: after_crossing(0),
        },
    ] {
        assert!(
            source
                .fresh_capture(local(event), after_crossing(0))
                .effects
                .is_empty()
        );
    }
    bridge.pump(&mut source, edge, after_crossing(0)).unwrap();
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
            .contains(&RecordedAction::Button(MouseButton::Left, true, 1)),
        "a press carried across the edge is a fresh single click on the peer"
    );
    assert!(!bridge.sent.iter().any(|frame| matches!(
        &frame.message,
        Message::Key(Key {
            usage: HidUsage(0x04),
            ..
        })
    )));

    let away = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, after_crossing(1));
    bridge.pump(&mut source, away, after_crossing(1)).unwrap();
    let back = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(1));
    bridge.pump(&mut source, back, after_crossing(1)).unwrap();
    let return_edge = push_through(&mut source, Point::new(-1.0, 0.0), after_crossing(1));
    bridge
        .pump(
            &mut source,
            return_edge,
            after_crossing(1) + PUSH_THROUGH_SETTLE,
        )
        .unwrap();
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
        after_crossing(2) + PUSH_THROUGH_SETTLE,
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
    rejected
        .pump(&mut rejected_source, edge, after_crossing(0))
        .unwrap();
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
        after_crossing(1),
    );
    assert_eq!(
        rejected.pump(&mut rejected_source, rejected_key, after_crossing(1)),
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
    held.pump(&mut held_source, edge, after_crossing(0))
        .unwrap();
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
        after_crossing(1),
    );
    held.pump(&mut held_source, held_key, after_crossing(1))
        .unwrap();
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
            .receive(&disconnect, after_crossing(2), &mut held.destination),
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
    // Slow samples on through it push through once settled, the full distance past where they
    // were when the push settled.
    let settled = ms(1) + PUSH_THROUGH_SETTLE;
    let settled_at = creep(
        Point::new(203.0, 50.0),
        Point::new(10.0, 0.0),
        ms(51),
        settled,
        |point, now| move_to(&mut source, point, now),
    )
    .x;
    move_to(
        &mut source,
        Point::new(settled_at + PUSH_THROUGH_DISTANCE - 0.5, 50.0),
        settled,
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            settled_at + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
        settled + ms(1),
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
    let pushed = ms(1) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(230.0, 60.0),
        Point::new(5.0, 0.0),
        ms(1),
        pushed,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            last.x + PUSH_THROUGH_DISTANCE,
            70.0,
        ))),
        pushed,
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
    // Resting there long past a pause, the push on starts afresh and must settle.
    let settled = ms(200) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(255.0, 80.0),
        Point::new(5.0, 0.0),
        ms(200),
        settled,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            last.x + PUSH_THROUGH_DISTANCE,
            80.0,
        ))),
        settled,
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
    let settled = ms(2) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(255.0, 50.0),
        Point::new(5.0, 0.0),
        ms(2),
        settled,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            last.x + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
        settled,
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
    let settled = ms(1) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(230.0, 60.0),
        Point::new(5.0, 0.0),
        ms(1),
        settled,
        |point, now| {
            let pushed = source.observe_pointer(point, now);
            assert_eq!(pushed.failure, None);
            assert!(pushed.effects.is_empty());
        },
    );
    let crossing =
        source.observe_pointer(Point::new(last.x + PUSH_THROUGH_DISTANCE, 60.0), settled);
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
    let ignored = source.observe_pointer(Point::new(150.0, 50.0), settled + ms(1));
    assert_eq!(ignored.failure, None);
    assert!(ignored.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::AwaitActivationAcknowledgement);
}

#[test]
fn polled_positions_wiggling_past_an_edge_never_add_up_to_a_push() {
    // Only polled positions, as from a source without relative input: out on display 5 the
    // settled pointer goes 30 px further and back again and again, giving the depth back each time.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let settled = ms(1) + PUSH_THROUGH_SETTLE;
    let mut observe = |x: f64, at: Duration| {
        let observed = source.observe_pointer(Point::new(x, 50.0), at);
        assert_eq!(observed.failure, None);
        sends_activation(&observed)
    };
    let last = creep(
        Point::new(230.0, 50.0),
        Point::new(5.0, 0.0),
        ms(1),
        settled,
        |point, now| assert!(!observe(point.x, now)),
    )
    .x;
    let wiggles = (PUSH_THROUGH_DISTANCE / 30.0) as u64 + 1;
    for wiggle in 0..wiggles {
        let at = settled + ms(20 * wiggle);
        assert!(!observe(last + 30.0, at), "wiggle {wiggle} out");
        assert!(!observe(last, at + ms(10)), "wiggle {wiggle} back");
    }
    // Still settled, the full distance on crosses.
    assert!(observe(
        last + PUSH_THROUGH_DISTANCE,
        settled + ms(20 * wiggles)
    ));
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
    let settled = ms(1) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(250.0, 50.0),
        Point::new(5.0, 0.0),
        ms(1),
        settled,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            last.x + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
        settled,
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
    let wobble = move_horizontally(&mut source, -3.0, after_crossing(1));
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
        after_crossing(2),
    );
    assert_frame(
        flick.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 60.0))),
    );
    let pressed = move_horizontally(&mut source, -2.0, after_crossing(3));
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
    let short = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE - 2.0, after_crossing(1));
    assert_eq!(short.failure, None);
    let held = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    assert_frame(
        held.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    assert_eq!(source.mode(), SourceMode::Remote);
    let away = move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, after_crossing(3));
    assert_eq!(away.failure, None);
    let arrival = move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(4));
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        4,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let returned = push_through(&mut source, Point::new(-1.0, 0.0), after_crossing(4));
    assert_frame(
        returned.effects.iter().next().unwrap(),
        4,
        5,
        Message::ReleaseAll,
    );
    assert_eq!(source.mode(), SourceMode::AwaitRemoteReleaseAcknowledgement);
    assert_eq!(
        complete_return(&mut source, 1, after_crossing(5) + PUSH_THROUGH_SETTLE),
        (DisplayId(1), Point::new(99.0, 50.0))
    );
}

#[test]
fn a_return_through_an_edge_guards_that_home_edge_until_the_pointer_moves_away() {
    let mut source = source();
    activate_remote(&mut source);
    assert_eq!(
        move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, after_crossing(1)).failure,
        None
    );
    assert_eq!(
        move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2)).failure,
        None
    );
    assert!(sends_release_all(&push_through(
        &mut source,
        Point::new(-1.0, 0.0),
        after_crossing(2)
    )));
    let back = after_crossing(3) + PUSH_THROUGH_SETTLE;
    let (display, home) = complete_return(&mut source, 1, back);
    assert_eq!((display, home), (DisplayId(1), Point::new(99.0, 50.0)));
    stay_alive(&mut source, 3, back);
    let wobble = move_horizontally(&mut source, 1.0, back + ms(1));
    assert_eq!(wobble.failure, None);
    assert!(
        wobble.effects.is_empty(),
        "a wobble at the home edge stays home"
    );
    let right = Point::new(PUSH_THROUGH_DISTANCE, 0.0);
    assert!(
        !sustained_push_crosses(&mut source, right, back + ms(2)),
        "a guarded edge takes no push"
    );
    let rested = back + ms(2) + PUSH_THROUGH_SETTLE;
    stay_alive(&mut source, 4, rested + ms(1));
    // Display 1's right edge line is x = 100: 23 px from it keeps the guard, 24 px rearms it.
    let right_edge = 100.0;
    move_to(
        &mut source,
        Point::new(right_edge - ENTRY_GUARD_DISTANCE + 1.0, 50.0),
        rested + ms(2),
    );
    move_to(&mut source, home, rested + ms(3));
    assert!(!sustained_push_crosses(&mut source, right, rested + ms(4)));
    assert_eq!(source.mode(), SourceMode::Local);
    let rested = rested + ms(4) + PUSH_THROUGH_SETTLE;
    move_to(
        &mut source,
        Point::new(right_edge - ENTRY_GUARD_DISTANCE, 50.0),
        rested + ms(1),
    );
    move_to(&mut source, home, rested + ms(2));
    let crossing = push_through(&mut source, Point::new(1.0, 0.0), rested + ms(3));
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
        after_crossing(1),
    );
    assert!(sends_release_all(&taken));
    // The same spot an edge return lands on, but nothing guards it.
    assert_eq!(
        complete_return(&mut source, 2, after_crossing(2)),
        (DisplayId(1), Point::new(99.0, 50.0))
    );
    let push = push_through(&mut source, Point::new(1.0, 0.0), after_crossing(3));
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
        after_crossing(1),
    );
    assert_frame(
        up.effects.iter().next().unwrap(),
        4,
        1,
        Message::Motion(Motion::Absolute(Point::new(1.0, 1.0))),
    );
    let corner = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(-1.0, -1.0)),
        after_crossing(2),
    );
    assert_frame(
        corner.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 0.0))),
    );
    // Once the climb has faded from the hand's heading, a sustained push straight through the
    // guarded edge never takes the pointer home.
    let shove = after_crossing(2) + 4 * PUSH_THROUGH_RECENT;
    stay_alive(&mut source, 3, shove);
    assert!(!sustained_push_crosses(
        &mut source,
        Point::new(-PUSH_THROUGH_DISTANCE, 0.0),
        shove
    ));
    assert_eq!(source.mode(), SourceMode::Remote);
    // Going mostly up moves onto the other computer's display above at once, straight above.
    let through = capture_on_route(
        &mut source,
        NormalizedInput::RelativeMotion(Point::new(-0.25, -1.0)),
        shove + PUSH_THROUGH_SETTLE + ms(10),
    );
    assert_eq!(through.failure, None);
    assert!(!sends_release_all(&through));
    assert_eq!(
        activation(&through),
        Some((DisplayId(3), Point::new(0.0, -1.0)))
    );
    assert_eq!(source.mode(), SourceMode::AwaitActivationAcknowledgement);
}

/// Enters display 2 by pushing along the unit `push` from `at`, moves off the entry edge, back
/// onto it and pushes through it, yielding where the returned pointer landed at `after_return(0)`.
fn round_trip(
    source: &mut SourceController,
    at: Point,
    push: Point,
    entry: Point,
) -> (DisplayId, Point) {
    activate_remote_from(source, at, push, entry);
    let along = |by: f64| NormalizedInput::RelativeMotion(scaled(push, by));
    assert_eq!(
        capture_on_route(source, along(ENTRY_GUARD_DISTANCE), after_crossing(1)).failure,
        None
    );
    let arrival = capture_on_route(
        source,
        along(-ENTRY_GUARD_DISTANCE - 3.0),
        after_crossing(2),
    );
    assert!(
        !sends_release_all(&arrival),
        "reaching the edge never crosses it"
    );
    assert!(sends_release_all(&push_through(
        source,
        scaled(push, -1.0),
        after_crossing(2)
    )));
    complete_return(source, 1, after_return(0))
}

/// `value` ms after [`round_trip`] landed the pointer back home.
fn after_return(value: u64) -> Duration {
    after_crossing(3) + PUSH_THROUGH_SETTLE + ms(value)
}

#[test]
fn a_guarded_home_edge_releases_once_the_pointer_is_that_far_onto_a_display_not_in_use() {
    // Display 5, past display 4's right edge line at x = 200, is not in use: the OS cursor moves
    // onto it while the tracked cursor stays pinned inside display 4.
    let right_edge = 200.0;
    let right = Point::new(1.0, 0.0);
    // Samples creeping on by this little still push, and stay short of the guard distance.
    let creep = 0.04;
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
            right,
            Point::new(1.0, 50.0),
        );
        assert_eq!(landed, (DisplayId(4), Point::new(199.0, 50.0)));
        stay_alive(&mut source, 3, after_return(0));
        // The tracked anchor stays on the edge line: this sample alone decides the release.
        let beyond = Point::new(right_edge + past, 50.0);
        move_to(&mut source, beyond, after_return(1));
        let push = absolute_push_through(&mut source, beyond, right, creep, after_return(1));
        assert_eq!(push.failure, None);
        assert_eq!(sends_activation(&push), crosses, "{past} px past the edge");
        if !crosses {
            let released = Point::new(right_edge + ENTRY_GUARD_DISTANCE, 50.0);
            let pushed = after_return(1) + PUSH_THROUGH_SETTLE;
            move_to(&mut source, released, pushed + ms(1));
            assert!(
                sends_activation(&absolute_push_through(
                    &mut source,
                    released,
                    right,
                    creep,
                    pushed + ms(1)
                )),
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

fn push_across(
    source: &mut SourceController,
    edge: Edge,
    distance: f64,
    now: Duration,
) -> SourceOutcome {
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(across(edge, distance)),
        now,
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
        // and while guarded even a sustained full push takes nothing.
        let mut remote = source_on(topology_linked_on_every_edge(), DisplayId(1));
        activate_remote_from(&mut remote, at, push, entry);
        stay_alive(&mut remote, 3, after_crossing(0));
        let held = push_across(&mut remote, entered, 3.0, after_crossing(1));
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
        let shove = across(entered, PUSH_THROUGH_DISTANCE);
        assert!(
            !sustained_push_crosses(&mut remote, shove, after_crossing(2)),
            "{entered:?}"
        );
        assert_eq!(remote.mode(), SourceMode::Remote, "{entered:?}");
        // Once 24 px inside, back on the line it takes a full push to return.
        let moved = after_crossing(2) + PUSH_THROUGH_SETTLE;
        push_across(&mut remote, entered, -ENTRY_GUARD_DISTANCE, moved + ms(1));
        assert!(!sends_release_all(&push_across(
            &mut remote,
            entered,
            ENTRY_GUARD_DISTANCE + 3.0,
            moved + ms(2)
        )));
        settle_push(&mut remote, across(entered, 1.0), moved + ms(3));
        let settled = moved + ms(3) + PUSH_THROUGH_SETTLE;
        let short = push_across(&mut remote, entered, PUSH_THROUGH_DISTANCE - 1.0, settled);
        assert!(short.effects.is_empty(), "{entered:?}");
        assert!(
            sends_release_all(&push_across(&mut remote, entered, 1.0, settled)),
            "{entered:?}"
        );

        // Local relative pushes at the home edge hold until the pointer has been 24 px inside,
        // then take a full push.
        let mut relative = source_on(topology_linked_on_every_edge(), DisplayId(1));
        assert_eq!(
            round_trip(&mut relative, at, push, entry),
            (DisplayId(1), at),
            "{entered:?}"
        );
        stay_alive(&mut relative, 3, after_return(0));
        let shove = across(home_edge, PUSH_THROUGH_DISTANCE);
        assert!(
            !sustained_push_crosses(&mut relative, shove, after_return(1)),
            "{entered:?}"
        );
        let rested = after_return(1) + PUSH_THROUGH_SETTLE;
        move_to(
            &mut relative,
            edge_point(home_edge, ENTRY_GUARD_DISTANCE),
            rested + ms(2),
        );
        move_to(&mut relative, at, rested + ms(3));
        let crossing = push_through(&mut relative, push, rested + ms(4));
        assert!(sends_activation(&crossing), "{entered:?}");

        // Local absolute samples past the home edge: 3 px is held, 24 px releases the guard and
        // a push on from there crosses once settled.
        let mut absolute = source_on(topology_linked_on_every_edge(), DisplayId(1));
        round_trip(&mut absolute, at, push, entry);
        stay_alive(&mut absolute, 3, after_return(0));
        move_to(&mut absolute, edge_point(home_edge, -3.0), after_return(1));
        move_to(
            &mut absolute,
            edge_point(home_edge, -ENTRY_GUARD_DISTANCE),
            after_return(2),
        );
        let settled = after_return(2) + PUSH_THROUGH_SETTLE;
        let crept = creep(
            edge_point(home_edge, -ENTRY_GUARD_DISTANCE - 1.0),
            across(home_edge, 1.0),
            after_return(3),
            settled,
            |point, now| move_to(&mut absolute, point, now),
        );
        let beyond = |distance: f64| {
            let by = across(home_edge, distance);
            Point::new(crept.x + by.x, crept.y + by.y)
        };
        move_to(&mut absolute, beyond(PUSH_THROUGH_DISTANCE - 1.0), settled);
        let crossing = capture_on_route(
            &mut absolute,
            NormalizedInput::AbsoluteMotion(beyond(PUSH_THROUGH_DISTANCE)),
            settled + ms(1),
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
    stay_alive(&mut source, 3, after_return(0));
    // 10 px onto display 4 is still within the guard distance of display 1's right edge line.
    move_to(&mut source, Point::new(110.0, 50.0), after_return(1));
    assert_eq!(source.motion_target().unwrap().target.display, DisplayId(4));
    // Display 4's own right edge is linked too; display 1's guard must not hold it.
    move_to(&mut source, Point::new(199.0, 50.0), after_return(2));
    let push = push_through(&mut source, Point::new(1.0, 0.0), after_return(3));
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
    assert_eq!(
        move_horizontally(&mut source, 8.0, after_crossing(1)).failure,
        None
    );
    let held = move_horizontally(&mut source, -12.0, after_crossing(2));
    assert_frame(
        held.effects.iter().next().unwrap(),
        4,
        2,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    assert_eq!(
        move_horizontally(&mut source, 10.0, after_crossing(3)).failure,
        None
    );
    let arrival = move_horizontally(&mut source, -13.0, after_crossing(4));
    assert_frame(
        arrival.effects.iter().next().unwrap(),
        4,
        4,
        Message::Motion(Motion::Absolute(Point::new(0.0, 50.0))),
    );
    let returned = push_through(&mut source, Point::new(-1.0, 0.0), after_crossing(4));
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
    let acknowledged = source.on_remote_frame(&activation_ack(), after_crossing(0));
    let refused = source.refuse_capture_activation(route_request(&acknowledged), after_crossing(1));
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
    fn press(source: &mut SourceController, now: Duration) -> SourceOutcome {
        move_horizontally(source, PUSH_THROUGH_DISTANCE, now)
    }
    fn decline(source: &mut SourceController, epoch: u64, reason: DeclineReason, now: Duration) {
        let frame = Frame::new(
            SessionEpoch::new(epoch).unwrap(),
            0,
            Message::ActivationDeclined {
                display_id: DisplayId(2),
                reason,
            },
        );
        assert_eq!(source.on_remote_frame(&frame, now).failure, None);
        assert_eq!(source.mode(), SourceMode::Local);
    }
    capture_log();
    let retry = DECLINE_RETRY_AFTER;
    let mut seam = source();
    move_to(&mut seam, Point::new(99.0, 50.0), ms(0));
    assert!(sends_activation(&push_through(
        &mut seam,
        Point::new(1.0, 0.0),
        ms(0)
    )));
    let declined = after_crossing(0);
    decline(&mut seam, 4, DeclineReason::Busy, declined);
    // Pushing on retries every DECLINE_RETRY_AFTER; the same answer is not logged again.
    assert!(press(&mut seam, declined + retry / 2).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, declined + retry)));
    decline(&mut seam, 5, DeclineReason::Busy, declined + retry);
    assert_eq!(logged("crossing declined by the other computer (Busy)"), 1);
    // A different answer is logged.
    assert!(
        press(&mut seam, declined + retry * 3 / 2)
            .effects
            .is_empty()
    );
    assert!(sends_activation(&press(&mut seam, declined + retry * 2)));
    decline(&mut seam, 6, DeclineReason::Disabled, declined + retry * 2);
    assert_eq!(
        logged("crossing declined by the other computer (Disabled)"),
        1
    );
    // Letting go of the seam ends the streak; pressing it again settles before it retries.
    let pause = declined + retry * 2 + ms(10);
    stay_alive(&mut seam, 3, pause);
    move_to(&mut seam, Point::new(50.0, 50.0), pause);
    move_to(&mut seam, Point::new(99.0, 50.0), pause + ms(1));
    settle_push(&mut seam, Point::new(1.0, 0.0), pause + ms(1));
    let settled = pause + ms(1) + PUSH_THROUGH_SETTLE;
    assert!(press(&mut seam, settled).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, settled + retry)));
    decline(&mut seam, 7, DeclineReason::Disabled, settled + retry);
    assert_eq!(
        logged("crossing declined by the other computer (Disabled)"),
        2
    );
    assert_eq!(logged("return:"), 0, "control never left this computer");

    // A declined hop between the other computer's displays does bring control home.
    let mut hop = source();
    activate_remote(&mut hop);
    assert_eq!(
        move_horizontally(&mut hop, 98.0, after_crossing(1)).failure,
        None
    );
    let hopped = after_crossing(2);
    assert!(sends_activation(&move_horizontally(&mut hop, 1.0, hopped)));
    let frame = Frame::new(
        SessionEpoch::new(5).unwrap(),
        0,
        Message::ActivationDeclined {
            display_id: DisplayId(3),
            reason: DeclineReason::Contended,
        },
    );
    let declined = hop.on_remote_frame(&frame, hopped + ms(1));
    assert_eq!(declined.failure, None);
    assert!(sends_release_all(&declined));
    assert_eq!(
        logged("return: the other computer declined control (Contended)"),
        1
    );
}

#[test]
fn a_push_that_ran_long_enough_logs_one_line_when_it_ends() {
    capture_log();
    // The push that crossed logs once the other computer has the pointer, and the push back home
    // once the return begins.
    let mut source = source();
    activate_remote(&mut source);
    let settle_ms = PUSH_THROUGH_SETTLE.as_millis();
    // settle_push pushes 1 px every 10 ms until the settle.
    let settling = settle_ms.div_ceil(10);
    assert_eq!(
        logged(&format!(
            "push-through: edge=seam-out platform=Windows run_ms={settle_ms} \
             before_settle={settling}.0 after_settle={PUSH_THROUGH_DISTANCE:.1} off_axis=0 \
             end=Crossed"
        )),
        1
    );
    stay_alive(&mut source, 3, after_crossing(0));
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(1));
    assert!(sends_release_all(&push_through(
        &mut source,
        Point::new(-1.0, 0.0),
        after_crossing(1)
    )));
    assert_eq!(
        logged(&format!(
            "push-through: edge=seam-back platform=Windows run_ms={settle_ms}"
        )),
        1
    );
    assert_eq!(logged("push-through:"), 2);

    // At rest on the seam: a push that stops short of PUSH_THROUGH_LOGGED logs nothing, one that
    // ran longer logs why it ended, and every outward record too slanted to push is counted.
    capture_log();
    let mut source = windows_source();
    move_to(&mut source, Point::new(50.0, 50.0), ms(0));
    move_to(&mut source, Point::new(99.0, 50.0), ms(0));
    let step = |source: &mut SourceController, delta: Point, at: u64| {
        let stepped = capture_on_route(source, NormalizedInput::RelativeMotion(delta), ms(at));
        assert!(stepped.effects.is_empty());
    };
    let out = Point::new(1.0, 0.0);
    // The push began with the arrival at zero.
    let short = PUSH_THROUGH_LOGGED.as_millis() as u64 - 10;
    for at in (10..=short).step_by(10) {
        step(&mut source, out, at);
    }
    step(&mut source, Point::new(-1.0, 0.0), short + 10);
    assert_eq!(logged("push-through:"), 0);
    for at in (100..=160).step_by(10) {
        step(&mut source, out, at);
    }
    step(&mut source, Point::new(1.0, 4.0), 165);
    step(&mut source, Point::new(-1.0, 0.0), 170);
    assert_eq!(
        logged("run_ms=60 before_settle=7.0 after_settle=0.0 off_axis=1 end=Inward"),
        1
    );
    for at in (300..=360).step_by(10) {
        step(&mut source, out, at);
    }
    let resumed = 360 + PUSH_THROUGH_RESET.as_millis() as u64;
    step(&mut source, out, resumed);
    assert_eq!(
        logged("run_ms=60 before_settle=7.0 after_settle=0.0 off_axis=0 end=Pause"),
        1
    );
    assert_eq!(logged("push-through:"), 2);
}

#[test]
fn a_push_logs_why_it_ended_in_real_record_order_and_never_crossed_against_a_wall() {
    // Each platform delivers a record's position before its delta, so backing off, the position
    // leaves the edge first.
    capture_log();
    let mut windows = windows_source();
    move_to(&mut windows, Point::new(10.0, 50.0), ms(0));
    let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
    // Three fast reports reach the edge, then the hand pushes on at 1 count a ms.
    let out = |t: u64| if t <= 3 { 30.0 } else { 1.0 };
    for t in 1..=100 {
        let counts = Point::new(out(t), 0.0);
        assert!(!windows_report(
            &mut windows,
            &mut cursor,
            counts,
            false,
            ms(t)
        ));
    }
    assert!(!windows_report(
        &mut windows,
        &mut cursor,
        Point::new(-5.0, 0.0),
        false,
        ms(101)
    ));
    assert_eq!(logged("push-through: edge=seam-out platform=Windows"), 1);
    assert_eq!(logged("end=Inward"), 1);
    capture_log();
    let mut mac = mac_source_controller();
    move_to(&mut mac, Point::new(90.0, 50.0), ms(0));
    let mut cursor = OsCursor::on_square(Point::new(90.0, 50.0));
    assert!(!mac_event(
        &mut mac,
        &mut cursor,
        Point::new(-150.0, 0.0),
        ms(8)
    ));
    for t in (16..=104).step_by(8) {
        assert!(!mac_event(
            &mut mac,
            &mut cursor,
            Point::new(-8.0, 0.0),
            ms(t)
        ));
    }
    assert!(!mac_event(
        &mut mac,
        &mut cursor,
        Point::new(5.0, 0.0),
        ms(112)
    ));
    assert_eq!(logged("push-through: edge=seam-out platform=MacOs"), 1);
    assert_eq!(logged("end=Inward"), 1);

    // Against a seam that cannot take the pointer yet, the full push crosses nothing and ends
    // as the hand backs off.
    capture_log();
    let mut walled = windows_source();
    walled.set_capture_ready(false);
    move_to(&mut walled, Point::new(10.0, 50.0), ms(0));
    let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
    let full = PUSH_THROUGH_SETTLE.as_millis() as u64 + 2 * PUSH_THROUGH_DISTANCE as u64;
    for t in 1..=full {
        let counts = Point::new(out(t), 0.0);
        assert!(!windows_report(
            &mut walled,
            &mut cursor,
            counts,
            false,
            ms(t)
        ));
    }
    let backed_off = full + 1;
    let counts = Point::new(-5.0, 0.0);
    assert!(!windows_report(
        &mut walled,
        &mut cursor,
        counts,
        false,
        ms(backed_off)
    ));
    assert_eq!(logged("end=Inward"), 1);
    assert_eq!(logged("end=Crossed"), 0);

    // Sliding along the edge off the end of a partial link, the pointer never left the edge.
    capture_log();
    let half = NormalizedSpan::new(0.0, 0.5).unwrap();
    let full = NormalizedSpan::new(0.0, 1.0).unwrap();
    let mut partial = source_on(
        two_machine_topology(
            vec![
                display_at(1, 1, Point::new(0.0, 0.0), 100),
                display_at(2, 2, Point::new(0.0, 0.0), 100),
            ],
            vec![
                EdgeLink::new(
                    DisplayId(1),
                    Edge::Right,
                    half,
                    DisplayId(2),
                    Edge::Left,
                    full,
                    1.0,
                )
                .unwrap(),
                EdgeLink::new(
                    DisplayId(2),
                    Edge::Left,
                    full,
                    DisplayId(1),
                    Edge::Right,
                    half,
                    1.0,
                )
                .unwrap(),
            ],
        ),
        DisplayId(1),
    );
    move_to(&mut partial, Point::new(10.0, 40.0), ms(0));
    let mut cursor = OsCursor::on_square(Point::new(10.0, 40.0));
    for t in 1..=100 {
        let counts = Point::new(out(t), 0.0);
        assert!(!windows_report(
            &mut partial,
            &mut cursor,
            counts,
            false,
            ms(t)
        ));
    }
    let slid = Point::new(0.0, 20.0);
    assert!(!windows_report(
        &mut partial,
        &mut cursor,
        slid,
        false,
        ms(101)
    ));
    assert_eq!(logged("end=Cleared"), 1);
    assert_eq!(logged("end=Inward"), 0);

    // The other computer's pointer backing off the seam home.
    capture_log();
    let mut remote = source();
    activate_remote(&mut remote);
    stay_alive(&mut remote, 3, after_crossing(0));
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    for t in (3..=63).step_by(10) {
        assert!(
            move_horizontally(&mut remote, -1.0, after_crossing(t))
                .effects
                .is_empty()
        );
    }
    move_horizontally(&mut remote, 5.0, after_crossing(70));
    assert_eq!(
        logged("push-through: edge=seam-back platform=Windows run_ms=60"),
        1
    );
    assert_eq!(logged("end=Inward"), 1);
}

/// A Windows source whose display 1's right edge links to the Mac's display 2.
fn windows_source() -> SourceController {
    source_on(
        two_machine_topology(
            vec![
                display_at(1, 1, Point::new(0.0, 0.0), 100),
                display_at(2, 2, Point::new(0.0, 0.0), 100),
            ],
            vec![
                full_link(1, Edge::Right, 2, Edge::Left),
                full_link(2, Edge::Left, 1, Edge::Right),
            ],
        ),
        DisplayId(1),
    )
}

/// One Windows report of raw `counts` at `now`: the hook sample, then its raw counts. The OS
/// cursor stops on the display's edges; `hook_past_edge` has the hook report where the move would
/// land. True if either half crossed.
fn windows_report(
    source: &mut SourceController,
    cursor: &mut OsCursor,
    counts: Point,
    hook_past_edge: bool,
    now: Duration,
) -> bool {
    windows_report_entry(source, cursor, counts, hook_past_edge, now).is_some()
}

/// [`windows_report`], yielding where the pointer enters the other computer if either half
/// crossed.
fn windows_report_entry(
    source: &mut SourceController,
    cursor: &mut OsCursor,
    counts: Point,
    hook_past_edge: bool,
    now: Duration,
) -> Option<Point> {
    let landed = cursor.moved(counts);
    let hook = if hook_past_edge { landed } else { cursor.at };
    let mut entry = None;
    for event in [
        NormalizedInput::AbsoluteMotion(hook),
        NormalizedInput::RelativeMotion(counts),
    ] {
        let outcome = capture_on_route(source, event, now);
        assert_eq!(outcome.failure, None);
        entry = entry.or(activation(&outcome).map(|(_, at)| at));
    }
    entry
}

/// One Mac event of `delta` points at `now`: its location, stopped on the display's edges, then
/// its delta. True if either half crossed.
fn mac_event(
    source: &mut SourceController,
    cursor: &mut OsCursor,
    delta: Point,
    now: Duration,
) -> bool {
    cursor.moved(delta);
    let mut crossed = false;
    for event in [
        NormalizedInput::AbsoluteMotion(cursor.at),
        NormalizedInput::RelativeMotion(delta),
    ] {
        let outcome = capture_on_route(source, event, now);
        assert_eq!(outcome.failure, None);
        crossed |= sends_activation(&outcome);
    }
    crossed
}

/// An OS cursor held to the pixels of a display at the origin whose last pixel is `last`.
struct OsCursor {
    at: Point,
    last: Point,
}

impl OsCursor {
    /// On a 100 px square display.
    fn on_square(at: Point) -> Self {
        Self {
            at,
            last: Point::new(99.0, 99.0),
        }
    }

    /// Moves by `delta`, stopping on the display's edges; yields where the move would have landed.
    fn moved(&mut self, delta: Point) -> Point {
        let landed = Point::new(self.at.x + delta.x, self.at.y + delta.y);
        self.at = Point::new(
            landed.x.clamp(0.0, self.last.x),
            landed.y.clamp(0.0, self.last.y),
        );
        landed
    }
}

/// The reports a 1000 Hz mouse sends while the hand travels `travel(t)` counts in `t` ms, for
/// `duration` ms.
fn reports(duration: u64, travel: impl Fn(f64) -> Point) -> Vec<(u64, Point)> {
    records(duration, 1, travel)
}

/// The records a device sends every `every` ms while the hand travels `travel(t)` in `t` ms, for
/// `duration` ms: each carries the whole counts crossed since the last, as a sensor's do.
fn records(duration: u64, every: u64, travel: impl Fn(f64) -> Point) -> Vec<(u64, Point)> {
    (every..=duration)
        .step_by(every as usize)
        .filter_map(|t| {
            let (now, before) = (travel(t as f64), travel((t - every) as f64));
            let counts = Point::new(
                now.x.floor() - before.x.floor(),
                now.y.floor() - before.y.floor(),
            );
            (counts != Point::new(0.0, 0.0)).then_some((t, counts))
        })
        .collect()
}

/// Travel `t` ms into a throw's tail, which reaches the edge at `speed` a ms and stops `tail` ms
/// later, slowing like the second half of a minimum-jerk reach, the usual model of a hand's.
fn throw_tail(speed: f64, tail: f64, t: f64) -> f64 {
    let reached = |tau: f64| tau.powi(3) * (10.0 - 15.0 * tau + 6.0 * tau * tau);
    // A minimum-jerk reach peaks at 1.875 times its mean speed, halfway through.
    let amplitude = speed * 2.0 * tail / 1.875;
    amplitude * (reached(0.5 + t.min(tail) / (2.0 * tail)) - 0.5)
}

/// Height of the displays in [`tall_source`].
const TALL: f64 = 2400.0;

/// A deliberate push crosses within this many ms of setting off, settle included.
const CROSSING_BUDGET_MS: u64 = 350;

/// True if a deliberate push crossed `elapsed` after setting off, within [`CROSSING_BUDGET_MS`].
fn crossed_in_budget(elapsed: Option<Duration>) -> bool {
    elapsed.is_some_and(|elapsed| (PUSH_THROUGH_SETTLE..=ms(CROSSING_BUDGET_MS)).contains(&elapsed))
}

/// A source on `machine`'s display `display`, where display 1 on the Windows computer and display
/// 2 on the Mac are 100 px wide and [`TALL`], linked through 1's right edge and 2's left edge.
fn tall_source(machine: u8, display: u64) -> SourceController {
    let tall = |id: u64, machine: u8| {
        Display::new(
            DisplayId(id),
            device(machine),
            format!("tall-{id}"),
            NativeSize::new(100, TALL as u32),
            LogicalSize::new(100.0, TALL),
            Point::new(0.0, 0.0),
            1.0,
            None,
            true,
        )
    };
    SourceController::new(
        two_machine_topology(
            vec![tall(1, 1), tall(2, 2)],
            vec![
                full_link(1, Edge::Right, 2, Edge::Left),
                full_link(2, Edge::Left, 1, Edge::Right),
            ],
        ),
        device(machine),
        DisplayId(display),
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap()
}

#[test]
fn a_windows_flick_that_carries_on_past_the_edge_for_a_burst_never_crosses() {
    // A 1000 Hz mouse at 1600 DPI flicked fast: 30 raw counts in every 1 ms report.
    for carry_on in [20, 40, 60] {
        for hook_past_edge in [false, true] {
            let mut source = windows_source();
            move_to(&mut source, Point::new(10.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
            let mut crossed = false;
            // Three reports reach the edge, then the flick carries on for `carry_on` ms.
            for report in 1..=3 + carry_on {
                crossed |= windows_report(
                    &mut source,
                    &mut cursor,
                    Point::new(30.0, 0.0),
                    hook_past_edge,
                    ms(report),
                );
            }
            assert!(
                !crossed,
                "{carry_on} ms, hook past the edge: {hook_past_edge}"
            );
            assert_eq!(source.mode(), SourceMode::Local);
        }
    }
}

#[test]
fn a_mac_trackpad_flick_that_carries_on_past_the_edge_never_crosses() {
    // Trackpad records every 8 to 16 ms, 50 to 150 points each, slowing as the finger stops.
    for interval in [8, 12, 16] {
        let mut source = mac_source_controller();
        move_to(&mut source, Point::new(90.0, 50.0), ms(0));
        let mut cursor = OsCursor::on_square(Point::new(90.0, 50.0));
        let mut crossed = false;
        for (record, dx) in (1..).zip([-150.0, -120.0, -100.0, -80.0, -50.0]) {
            crossed |= mac_event(
                &mut source,
                &mut cursor,
                Point::new(dx, 0.0),
                ms(record * interval),
            );
        }
        assert!(!crossed, "records every {interval} ms");
        assert_eq!(source.mode(), SourceMode::Local);
    }
}

#[test]
fn a_throw_whose_hand_slows_to_a_stop_past_the_edge_never_crosses() {
    for tail in [200, 250] {
        // Windows: 30 counts a ms, about 0.5 m/s at 1600 DPI, reach the edge, then slow to a stop.
        for hook_past_edge in [false, true] {
            let mut source = windows_source();
            move_to(&mut source, Point::new(10.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
            let mut crossed = false;
            for report in 1..=3 {
                crossed |= windows_report(
                    &mut source,
                    &mut cursor,
                    Point::new(30.0, 0.0),
                    hook_past_edge,
                    ms(report),
                );
            }
            let slowing = reports(tail, |t| Point::new(throw_tail(30.0, tail as f64, t), 0.0));
            for (t, counts) in slowing {
                crossed |=
                    windows_report(&mut source, &mut cursor, counts, hook_past_edge, ms(3 + t));
            }
            assert!(
                !crossed,
                "{tail} ms tail, hook past the edge: {hook_past_edge}"
            );
            assert_eq!(source.mode(), SourceMode::Local);
        }
        // Mac: records of 150 points reach the edge, then slow to a stop.
        for interval in [8, 12, 16] {
            let mut source = mac_source_controller();
            move_to(&mut source, Point::new(90.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(90.0, 50.0));
            let speed = 150.0 / interval as f64;
            let slid = |t: u64| throw_tail(speed, tail as f64, t as f64);
            let mut crossed = mac_event(
                &mut source,
                &mut cursor,
                Point::new(-150.0, 0.0),
                ms(interval),
            );
            for t in (interval..tail + interval).step_by(interval as usize) {
                let step = Point::new(slid(t - interval) - slid(t), 0.0);
                crossed |= mac_event(&mut source, &mut cursor, step, ms(interval + t));
            }
            assert!(!crossed, "{tail} ms tail, records every {interval} ms");
            assert_eq!(source.mode(), SourceMode::Local);
        }
    }
}

#[test]
fn a_slide_along_a_linked_edge_that_leans_outward_never_crosses() {
    // A scrollbar thumb dragged down the linked edge at 1.5 px a ms, drifting outward by a tenth
    // of that: 300 ms down, then a second scrubbing up and down, whose drift alone would cross.
    let slide = |t: f64| {
        let slid = 1.5 * t;
        Point::new(0.1 * slid, 450.0 - (slid % 900.0 - 450.0).abs())
    };
    let duration = 1300;
    let beat = 400;
    for hook_past_edge in [false, true] {
        let mut source = tall_source(1, 1);
        move_to(&mut source, Point::new(50.0, 100.0), ms(0));
        move_to(&mut source, Point::new(99.0, 100.0), ms(1));
        let mut cursor = OsCursor {
            at: Point::new(99.0, 100.0),
            last: Point::new(99.0, TALL - 1.0),
        };
        let mut crossed = false;
        for (t, counts) in reports(duration, slide) {
            if t % beat == 0 {
                stay_alive(&mut source, 2 + t / beat, ms(1 + t));
            }
            crossed |= windows_report(&mut source, &mut cursor, counts, hook_past_edge, ms(1 + t));
        }
        assert!(!crossed, "hook past the edge: {hook_past_edge}");
        assert_eq!(source.mode(), SourceMode::Local);
    }
    // The Mac drags down its linked left edge the same way.
    for interval in [8, 16] {
        let mut source = tall_source(2, 2);
        move_to(&mut source, Point::new(50.0, 100.0), ms(0));
        move_to(&mut source, Point::new(0.0, 100.0), ms(1));
        let mut cursor = OsCursor {
            at: Point::new(0.0, 100.0),
            last: Point::new(99.0, TALL - 1.0),
        };
        let mut crossed = false;
        for t in (interval..=duration).step_by(interval as usize) {
            if t % beat == 0 {
                stay_alive(&mut source, 2 + t / beat, ms(1 + t));
            }
            let (to, from) = (slide(t as f64), slide((t - interval) as f64));
            let step = Point::new(from.x - to.x, to.y - from.y);
            crossed |= mac_event(&mut source, &mut cursor, step, ms(1 + t));
        }
        assert!(!crossed, "records every {interval} ms");
        assert_eq!(source.mode(), SourceMode::Local);
    }
}

#[test]
fn a_throw_into_a_corner_beside_a_linked_edge_stays_local_until_the_push_turns_outward() {
    // Display 1 links its right edge but not its bottom edge. A 45 degree throw into their corner,
    // and pushing on into it, push neither edge; turning to push right crosses as a fresh push.
    let diagonal = |counts: f64| Point::new(counts, counts);
    for hook_past_edge in [false, true] {
        let mut source = windows_source();
        move_to(&mut source, Point::new(10.0, 10.0), ms(0));
        let mut cursor = OsCursor::on_square(Point::new(10.0, 10.0));
        let mut crossed = false;
        for report in 1..=3 {
            crossed |= windows_report(
                &mut source,
                &mut cursor,
                diagonal(30.0),
                hook_past_edge,
                ms(report),
            );
        }
        for (t, counts) in reports(250, |t| diagonal(throw_tail(30.0, 250.0, t))) {
            crossed |= windows_report(&mut source, &mut cursor, counts, hook_past_edge, ms(3 + t));
        }
        stay_alive(&mut source, 3, ms(300));
        for t in 300..700 {
            crossed |= windows_report(
                &mut source,
                &mut cursor,
                diagonal(1.0),
                hook_past_edge,
                ms(t),
            );
        }
        assert!(!crossed, "hook past the edge: {hook_past_edge}");
        stay_alive(&mut source, 4, ms(700));
        let turned = (1..=CROSSING_BUDGET_MS).map(ms).find(|elapsed| {
            windows_report(
                &mut source,
                &mut cursor,
                Point::new(3.0, 1.0),
                hook_past_edge,
                ms(700) + *elapsed,
            )
        });
        assert!(
            crossed_in_budget(turned),
            "{turned:?}, hook past the edge: {hook_past_edge}"
        );
    }
    // The Mac's display 2 links only its left edge: its bottom-left corner, records every 8 ms.
    let mut source = mac_source_controller();
    move_to(&mut source, Point::new(90.0, 10.0), ms(0));
    let mut cursor = OsCursor::on_square(Point::new(90.0, 10.0));
    let slid = |t: u64| throw_tail(150.0 / 8.0, 250.0, t as f64);
    let mut crossed = mac_event(&mut source, &mut cursor, Point::new(-150.0, 150.0), ms(8));
    for t in (8..258).step_by(8) {
        let step = slid(t) - slid(t - 8);
        crossed |= mac_event(&mut source, &mut cursor, Point::new(-step, step), ms(8 + t));
    }
    stay_alive(&mut source, 3, ms(300));
    for t in (304..700).step_by(8) {
        crossed |= mac_event(&mut source, &mut cursor, Point::new(-8.0, 8.0), ms(t));
    }
    assert!(!crossed);
    stay_alive(&mut source, 4, ms(700));
    let turned = (1..=CROSSING_BUDGET_MS / 8)
        .map(|record| ms(8 * record))
        .find(|elapsed| {
            mac_event(
                &mut source,
                &mut cursor,
                Point::new(-8.0, 2.0),
                ms(700) + *elapsed,
            )
        });
    assert!(crossed_in_budget(turned), "{turned:?}");
}

#[test]
fn in_a_corner_of_two_linked_edges_a_push_goes_through_the_edge_it_points_at() {
    // Display 1 links every edge. After a push on its right edge in the bottom-right corner, a
    // push down that drifts slightly right goes through the bottom edge, not the right one.
    let mut source = source_on(topology_linked_on_every_edge(), DisplayId(1));
    move_to(&mut source, Point::new(50.0, 50.0), ms(0));
    move_to(&mut source, Point::new(99.0, 99.0), ms(1));
    assert!(
        move_horizontally(&mut source, 5.0, ms(2))
            .effects
            .is_empty()
    );
    // The hand turns down once the push right has faded from its heading.
    let turned = ms(2) + 4 * PUSH_THROUGH_RECENT;
    let crossing = push_through(&mut source, Point::new(0.25, 1.0), turned);
    assert_frame(
        crossing.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: Point::new(99.0, 1.0),
        },
    );
}

#[test]
fn a_push_right_that_drifts_toward_a_linked_bottom_edge_still_crosses_right() {
    // Display 1 links every edge. Pushing right in its bottom-right corner at `speed` counts a ms
    // while drifting `degrees` down, for up to `duration` ms: yields when it crossed, and where
    // it entered the other computer.
    let corner_push = |speed: f64, degrees: f64, hook_past_edge: bool, duration: u64| {
        let (down, right) = degrees.to_radians().sin_cos();
        let mut source = source_on(topology_linked_on_every_edge(), DisplayId(1));
        move_to(&mut source, Point::new(50.0, 50.0), ms(0));
        move_to(&mut source, Point::new(99.0, 99.0), ms(1));
        let mut cursor = OsCursor::on_square(Point::new(99.0, 99.0));
        let mut beats = 0;
        reports(duration, |t| {
            Point::new(speed * right * t, speed * down * t)
        })
        .into_iter()
        .find_map(|(t, counts)| {
            if t / 400 > beats {
                beats = t / 400;
                stay_alive(&mut source, 2 + beats, ms(1 + t));
            }
            windows_report_entry(&mut source, &mut cursor, counts, hook_past_edge, ms(1 + t))
                .map(|entry| (ms(t), entry))
        })
    };
    // At 1 count a ms, drifting 5 to 15 degrees: the reports that step only down never restart
    // the push right.
    for degrees in [5.0_f64, 10.0, 15.0] {
        for hook_past_edge in [false, true] {
            let crossing = corner_push(1.0, degrees, hook_past_edge, CROSSING_BUDGET_MS);
            let context = format!("{crossing:?} at {degrees} degrees, hook past: {hook_past_edge}");
            assert!(
                crossed_in_budget(crossing.map(|(elapsed, _)| elapsed)),
                "{context}"
            );
            // Through the right edge, onto display 2's left one.
            assert!(
                crossing.is_some_and(|(_, entry)| entry.x == 1.0),
                "{context}"
            );
        }
    }
    // At 0.5 counts a ms drifting 20 degrees it takes longer, its distance being in counts, but
    // still goes right.
    for hook_past_edge in [false, true] {
        let crossing = corner_push(0.5, 20.0, hook_past_edge, 1000);
        assert!(
            crossing.is_some_and(|(_, entry)| entry.x == 1.0),
            "{crossing:?}, hook past: {hook_past_edge}"
        );
    }
}

/// From rest on the linked edge of a [`tall_source`] display, on Windows unless `mac_every` gives
/// the Mac's record interval, moves the hand `travel(t)` out through the edge and down along it
/// for 4 s; true once it crosses.
fn along_the_edge_crosses(
    travel: impl Fn(f64) -> Point,
    mac_every: Option<u64>,
    hook_past_edge: bool,
) -> bool {
    let (machine, edge_x, out) = if mac_every.is_some() {
        (2, 0.0, -1.0)
    } else {
        (1, 99.0, 1.0)
    };
    let mut source = tall_source(machine, u64::from(machine));
    move_to(&mut source, Point::new(50.0, 100.0), ms(0));
    move_to(&mut source, Point::new(edge_x, 100.0), ms(1));
    let mut cursor = OsCursor {
        at: Point::new(edge_x, 100.0),
        last: Point::new(99.0, TALL - 1.0),
    };
    let mut beats = 0;
    let moved = |t: f64| {
        let moved = travel(t);
        Point::new(out * moved.x, moved.y)
    };
    records(4000, mac_every.unwrap_or(1), moved)
        .into_iter()
        .any(|(t, counts)| {
            if t / 400 > beats {
                beats = t / 400;
                stay_alive(&mut source, 2 + beats, ms(1 + t));
            }
            match mac_every {
                Some(_) => mac_event(&mut source, &mut cursor, counts, ms(1 + t)),
                None => windows_report(&mut source, &mut cursor, counts, hook_past_edge, ms(1 + t)),
            }
        })
}

#[test]
fn a_slow_slide_along_a_linked_edge_that_leans_outward_never_crosses() {
    // A scrollbar thumb dragged down the linked edge for 4 s at 0.3 and 0.5 counts a ms, 5 to 8
    // mm/s at 1600 DPI, leaning outward: most whole-count reports carry no outward count, and some
    // carry nothing else. The same hand turned to push out, leaning along the edge instead,
    // crosses. The Mac's whole-point deltas go a record every ms or every 8 ms.
    for (speed, lean) in [(0.3, 0.2), (0.3, 0.3), (0.5, 0.2), (0.5, 0.3)] {
        let slide = |t: f64| Point::new(lean * speed * t, speed * t);
        let push = |t: f64| Point::new(speed * t, lean * speed * t);
        for (mac_every, hook_past_edge) in [
            (None, false),
            (None, true),
            (Some(1), false),
            (Some(8), false),
        ] {
            let context = format!("{speed} a ms leaning {lean}, on {mac_every:?} {hook_past_edge}");
            assert!(
                !along_the_edge_crosses(slide, mac_every, hook_past_edge),
                "{context}"
            );
            assert!(
                along_the_edge_crosses(push, mac_every, hook_past_edge),
                "{context}"
            );
        }
    }
}

#[test]
fn a_deliberate_push_after_resting_on_the_edge_crosses_within_the_budget_on_both_platforms() {
    // Arrive, rest past a pause, then push on from here.
    let start = ms(150);
    // Windows at 600 and 1000 counts a second, a px each at the default pointer speed.
    for speed in [0.6, 1.0] {
        for hook_past_edge in [false, true] {
            let mut source = windows_source();
            move_to(&mut source, Point::new(10.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
            for report in 1..=3 {
                assert!(!windows_report(
                    &mut source,
                    &mut cursor,
                    Point::new(30.0, 0.0),
                    hook_past_edge,
                    ms(report)
                ));
            }
            stay_alive(&mut source, 3, start);
            let crossed_after = reports(CROSSING_BUDGET_MS, |t| Point::new(speed * t, 0.0))
                .into_iter()
                .find(|(t, counts)| {
                    windows_report(
                        &mut source,
                        &mut cursor,
                        *counts,
                        hook_past_edge,
                        start + ms(*t),
                    )
                })
                .map(|(t, _)| ms(t));
            assert!(
                crossed_in_budget(crossed_after),
                "{crossed_after:?} at {speed} counts a ms, hook past the edge: {hook_past_edge}"
            );
        }
    }
    // Mac at 600 and 1000 points a second, records every 8 or 16 ms, timed from the first.
    for speed in [0.6, 1.0] {
        for interval in [8, 16] {
            let mut source = mac_source_controller();
            move_to(&mut source, Point::new(50.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(50.0, 50.0));
            assert!(!mac_event(
                &mut source,
                &mut cursor,
                Point::new(-60.0, 0.0),
                ms(8)
            ));
            stay_alive(&mut source, 3, start);
            let step = Point::new(-speed * interval as f64, 0.0);
            let crossed_after = (0..=CROSSING_BUDGET_MS / interval)
                .map(|record| ms(record * interval))
                .find(|elapsed| mac_event(&mut source, &mut cursor, step, start + *elapsed));
            assert!(
                crossed_in_budget(crossed_after),
                "{crossed_after:?} at {speed} points a ms, records every {interval} ms"
            );
        }
    }
}

#[test]
fn a_poll_that_sees_the_arrival_first_leaves_the_arrivals_travel_uncounted() {
    let mut source = mac_source_controller();
    move_to(&mut source, Point::new(50.0, 50.0), ms(0));
    // The polled OS cursor is on the edge before the tap delivers the record that took it there.
    assert!(
        source
            .observe_pointer(Point::new(0.0, 50.0), ms(10))
            .effects
            .is_empty()
    );
    let mut cursor = OsCursor::on_square(Point::new(0.0, 50.0));
    let left = |dx: f64| Point::new(-dx, 0.0);
    assert!(!mac_event(&mut source, &mut cursor, left(120.0), ms(11)));
    // The push began with the poll; only travel from its settle on counts.
    let settled = ms(10) + PUSH_THROUGH_SETTLE;
    for at in (31..).step_by(20).map(ms).take_while(|at| *at < settled) {
        assert!(!mac_event(&mut source, &mut cursor, left(2.0), at));
    }
    assert!(!mac_event(
        &mut source,
        &mut cursor,
        left(PUSH_THROUGH_DISTANCE - 1.0),
        settled
    ));
    assert!(mac_event(
        &mut source,
        &mut cursor,
        left(1.0),
        settled + ms(1)
    ));
}

#[test]
fn past_an_edge_an_absolute_sample_and_its_relative_half_count_their_step_once() {
    // Display 5 past display 4's right edge is not in use, so the OS pointer really moves there.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    move_to(&mut source, Point::new(150.0, 50.0), ms(0));
    move_to(&mut source, Point::new(199.0, 50.0), ms(10));
    move_horizontally(&mut source, 49.0, ms(10));
    // 10 px every 10 ms from the arrival at 10 ms: the first step once settled is the first to
    // count, and the full distance is reached on the absolute half of the step that completes it.
    let first_counted = u64::try_from(PUSH_THROUGH_SETTLE.as_millis().div_ceil(10)).unwrap();
    let expected = first_counted + (PUSH_THROUGH_DISTANCE / 10.0) as u64 - 1;
    let crossed = (1..=expected + 1).find_map(|step| {
        let at = ms(10 + step * 10);
        let x = 199.0 + 10.0 * step as f64;
        let absolute = capture_on_route(
            &mut source,
            NormalizedInput::AbsoluteMotion(Point::new(x, 50.0)),
            at,
        );
        if sends_activation(&absolute) {
            return Some((step, "absolute"));
        }
        let relative = move_horizontally(&mut source, 10.0, at);
        sends_activation(&relative).then_some((step, "relative"))
    });
    assert_eq!(crossed, Some((expected, "absolute")));
}

#[test]
fn moving_back_inward_past_an_edge_starts_the_push_over() {
    // Display 5 past display 4's right edge is not in use: the tracked anchor stays on the edge
    // line while the OS pointer moves out there, so only the relative halves show direction.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    move_to(&mut source, Point::new(150.0, 50.0), ms(0));
    let step = |source: &mut SourceController, x: f64, dx: f64, at: Duration| {
        let absolute = capture_on_route(
            source,
            NormalizedInput::AbsoluteMotion(Point::new(x, 50.0)),
            at,
        );
        let relative = move_horizontally(source, dx, at);
        assert_eq!(relative.failure, None);
        sends_activation(&absolute) || sends_activation(&relative)
    };
    // 5 px every 20 ms from `x` keeps a push begun at `from` going until it has settled.
    let push_on = |source: &mut SourceController, mut x: f64, from: Duration| {
        let mut at = from + ms(20);
        while at < from + PUSH_THROUGH_SETTLE {
            x += 5.0;
            assert!(!step(source, x, 5.0, at));
            at += ms(20);
        }
        x
    };
    assert!(!step(&mut source, 230.0, 80.0, ms(10)));
    let x = push_on(&mut source, 230.0, ms(10));
    // Settled, 10 px short of crossing.
    let settled = ms(10) + PUSH_THROUGH_SETTLE;
    let x = x + PUSH_THROUGH_DISTANCE - 10.0;
    assert!(!step(&mut source, x, PUSH_THROUGH_DISTANCE - 10.0, settled));
    // A wiggle back in and out again: the push starts over and must settle again.
    assert!(!step(&mut source, x - 10.0, -10.0, settled + ms(10)));
    let again = settled + ms(20);
    let x = x + 10.0;
    assert!(!step(&mut source, x, 20.0, again));
    let x = push_on(&mut source, x, again);
    assert!(step(
        &mut source,
        x + PUSH_THROUGH_DISTANCE,
        PUSH_THROUGH_DISTANCE,
        again + PUSH_THROUGH_SETTLE
    ));
}

#[test]
fn a_push_crosses_only_once_it_reaches_the_full_distance() {
    let mut steps = source();
    rest_on_right_edge(&mut steps, ms(0));
    settle_push(&mut steps, Point::new(1.0, 0.0), ms(0));
    let settled = PUSH_THROUGH_SETTLE;
    for _ in 1..PUSH_THROUGH_DISTANCE as u32 {
        let step = move_horizontally(&mut steps, 1.0, settled);
        assert_eq!(step.failure, None);
        assert!(step.effects.is_empty());
    }
    assert!(sends_activation(&move_horizontally(
        &mut steps, 1.0, settled
    )));

    let mut short = source();
    rest_on_right_edge(&mut short, ms(0));
    settle_push(&mut short, Point::new(1.0, 0.0), ms(0));
    let pushed = move_horizontally(&mut short, PUSH_THROUGH_DISTANCE - 1.0, settled);
    assert_eq!(pushed.failure, None);
    assert!(pushed.effects.is_empty());
    assert_eq!(short.mode(), SourceMode::Local);

    let mut full = source();
    rest_on_right_edge(&mut full, ms(0));
    settle_push(&mut full, Point::new(1.0, 0.0), ms(0));
    assert!(sends_activation(&move_horizontally(
        &mut full,
        PUSH_THROUGH_DISTANCE,
        settled
    )));
}

#[test]
fn leaving_the_edge_inward_starts_the_push_over() {
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    settle_push(&mut local, Point::new(1.0, 0.0), ms(0));
    let settled = PUSH_THROUGH_SETTLE;
    assert!(
        move_horizontally(&mut local, PUSH_THROUGH_DISTANCE - 1.0, settled)
            .effects
            .is_empty()
    );
    move_to(&mut local, Point::new(97.0, 50.0), settled + ms(1));
    move_to(&mut local, Point::new(99.0, 50.0), settled + ms(2));
    settle_push(&mut local, Point::new(1.0, 0.0), settled + ms(2));
    let again = settled + ms(2) + PUSH_THROUGH_SETTLE;
    assert!(
        move_horizontally(&mut local, PUSH_THROUGH_DISTANCE - 1.0, again)
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(&mut local, 1.0, again)));

    let mut remote = source();
    activate_remote(&mut remote);
    stay_alive(&mut remote, 3, after_crossing(0));
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    settle_push(&mut remote, Point::new(-1.0, 0.0), after_crossing(3));
    let settled = after_crossing(3) + PUSH_THROUGH_SETTLE;
    let pushed = move_horizontally(&mut remote, 1.0 - PUSH_THROUGH_DISTANCE, settled);
    assert!(pushed.effects.is_empty());
    // Two px inward is off the edge; coming back is a new arrival.
    move_horizontally(&mut remote, 2.0, settled + ms(1));
    move_horizontally(&mut remote, -2.0, settled + ms(2));
    settle_push(&mut remote, Point::new(-1.0, 0.0), settled + ms(3));
    let again = settled + ms(3) + PUSH_THROUGH_SETTLE;
    assert!(!sends_release_all(&move_horizontally(
        &mut remote,
        1.0 - PUSH_THROUGH_DISTANCE,
        again
    )));
    assert!(sends_release_all(&move_horizontally(
        &mut remote,
        -1.0,
        again
    )));
}

#[test]
fn a_pause_in_the_push_starts_it_over() {
    let mut paused = source();
    rest_on_right_edge(&mut paused, ms(0));
    settle_push(&mut paused, Point::new(1.0, 0.0), ms(0));
    let settled = PUSH_THROUGH_SETTLE;
    assert!(
        move_horizontally(&mut paused, PUSH_THROUGH_DISTANCE - 1.0, settled)
            .effects
            .is_empty()
    );
    // After the pause even the full distance at once is a fresh push that has yet to settle.
    let late = settled + PUSH_THROUGH_RESET;
    assert!(
        move_horizontally(&mut paused, PUSH_THROUGH_DISTANCE, late)
            .effects
            .is_empty()
    );
    stay_alive(&mut paused, 3, late);
    settle_push(&mut paused, Point::new(1.0, 0.0), late);
    assert!(sends_activation(&move_horizontally(
        &mut paused,
        PUSH_THROUGH_DISTANCE,
        late + PUSH_THROUGH_SETTLE
    )));

    let mut brief = source();
    rest_on_right_edge(&mut brief, ms(0));
    settle_push(&mut brief, Point::new(1.0, 0.0), ms(0));
    assert!(
        move_horizontally(&mut brief, PUSH_THROUGH_DISTANCE - 1.0, settled)
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(
        &mut brief,
        1.0,
        settled + PUSH_THROUGH_RESET - ms(1)
    )));
}

#[test]
fn sliding_along_the_edge_keeps_the_push() {
    let half = PUSH_THROUGH_DISTANCE / 2.0;
    let along = |dx: f64, dy: f64| NormalizedInput::RelativeMotion(Point::new(dx, dy));
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    settle_push(&mut local, Point::new(1.0, 0.0), ms(0));
    let settled = PUSH_THROUGH_SETTLE;
    assert!(
        capture_on_route(&mut local, along(half, 0.0), settled)
            .effects
            .is_empty()
    );
    move_to(&mut local, Point::new(99.0, 70.0), settled + ms(1));
    assert!(
        capture_on_route(&mut local, along(0.0, 10.0), settled + ms(1))
            .effects
            .is_empty()
    );
    // Only the outward part of a diagonal push counts.
    assert!(
        capture_on_route(&mut local, along(half - 1.0, 10.0), settled + ms(2))
            .effects
            .is_empty()
    );
    let crossing = capture_on_route(&mut local, along(1.0, 0.0), settled + ms(2));
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
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    settle_push(&mut remote, Point::new(-1.0, 0.0), after_crossing(3));
    let settled = after_crossing(3) + PUSH_THROUGH_SETTLE;
    let slid = capture_on_route(&mut remote, along(0.0, 10.0), settled);
    assert_frame(
        slid.effects.iter().next().unwrap(),
        4,
        3,
        Message::Motion(Motion::Absolute(Point::new(0.0, 60.0))),
    );
    assert!(
        capture_on_route(&mut remote, along(-half, 0.0), settled)
            .effects
            .is_empty()
    );
    // Here too only the diagonal's outward part counts, while the pointer slides on.
    let diagonal = capture_on_route(&mut remote, along(1.0 - half, 10.0), settled);
    assert!(!sends_release_all(&diagonal));
    assert_frame(
        diagonal.effects.iter().next().unwrap(),
        4,
        4,
        Message::Motion(Motion::Absolute(Point::new(0.0, 70.0))),
    );
    assert!(sends_release_all(&capture_on_route(
        &mut remote,
        along(-1.0, 0.0),
        settled
    )));
    assert_eq!(
        complete_return(&mut remote, 1, settled + ms(1)),
        (DisplayId(1), Point::new(99.0, 70.0))
    );
}

#[test]
fn returning_home_takes_the_same_push_while_the_remote_pointer_stays_on_the_edge() {
    let mut source = source();
    let mut bridge = ReceiverBridge::new(destination_topology(&[DisplayId(2), DisplayId(3)]));
    move_to(&mut source, Point::new(99.0, 50.0), ms(0));
    let enter = push_through(&mut source, Point::new(1.0, 0.0), ms(0));
    bridge.pump(&mut source, enter, after_crossing(0)).unwrap();
    for (at, dx) in [(1, ENTRY_GUARD_DISTANCE), (2, -ENTRY_GUARD_DISTANCE - 3.0)] {
        let moved = move_horizontally(&mut source, dx, after_crossing(at));
        bridge.pump(&mut source, moved, after_crossing(at)).unwrap();
    }
    settle_push(&mut source, Point::new(-1.0, 0.0), after_crossing(3));
    let settled = after_crossing(3) + PUSH_THROUGH_SETTLE;
    for _ in 1..PUSH_THROUGH_DISTANCE as u32 {
        let pushed = move_horizontally(&mut source, -1.0, settled);
        assert!(!sends_release_all(&pushed));
        bridge.pump(&mut source, pushed, settled).unwrap();
        assert_eq!(source.mode(), SourceMode::Remote);
    }
    let through = move_horizontally(&mut source, -1.0, settled);
    bridge.pump(&mut source, through, settled).unwrap();
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
fn absolute_samples_push_through_by_how_far_they_go_past_where_the_push_settled() {
    // Space outside the layout past display 4, which the pointer arrives in 30 px deep and pushes
    // on through, drifting down a little.
    let mut deep = source();
    move_to(&mut deep, Point::new(150.0, 50.0), ms(0));
    let settled = ms(1) + PUSH_THROUGH_SETTLE;
    let last = creep(
        Point::new(230.0, 50.0),
        Point::new(5.0, 2.0),
        ms(1),
        settled,
        |point, now| move_to(&mut deep, point, now),
    );
    move_to(
        &mut deep,
        Point::new(last.x + PUSH_THROUGH_DISTANCE - 1.0, 60.0),
        settled,
    );
    let crossing = capture_on_route(
        &mut deep,
        NormalizedInput::AbsoluteMotion(Point::new(last.x + PUSH_THROUGH_DISTANCE, 60.0)),
        settled + ms(1),
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
}

#[test]
fn a_guarded_entry_edge_takes_no_push_until_the_pointer_moves_away() {
    let mut source = source();
    activate_remote(&mut source);
    stay_alive(&mut source, 3, after_crossing(0));
    let shove = Point::new(-2.0 * PUSH_THROUGH_DISTANCE, 0.0);
    assert!(!sustained_push_crosses(
        &mut source,
        shove,
        after_crossing(1)
    ));
    let shoved = after_crossing(1) + PUSH_THROUGH_SETTLE;
    stay_alive(&mut source, 4, shoved + ms(1));
    // 22 px in and back keeps the guard, so a sustained push still takes nothing.
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE - 2.0, shoved + ms(2));
    move_horizontally(&mut source, 2.0 - ENTRY_GUARD_DISTANCE, shoved + ms(3));
    assert!(!sustained_push_crosses(&mut source, shove, shoved + ms(4)));
    assert_eq!(source.mode(), SourceMode::Remote);
    // 24 px in releases it; back on the edge a full push returns.
    let shoved = shoved + ms(4) + PUSH_THROUGH_SETTLE;
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, shoved + ms(1));
    move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE, shoved + ms(2));
    settle_push(&mut source, Point::new(-1.0, 0.0), shoved + ms(3));
    let settled = shoved + ms(3) + PUSH_THROUGH_SETTLE;
    assert!(
        move_horizontally(&mut source, 1.0 - PUSH_THROUGH_DISTANCE, settled)
            .effects
            .is_empty()
    );
    assert!(sends_release_all(&move_horizontally(
        &mut source,
        -1.0,
        settled
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
