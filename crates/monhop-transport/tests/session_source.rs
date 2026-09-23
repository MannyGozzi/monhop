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
    PUSH_THROUGH_RECENT, PUSH_THROUGH_RESET, SourceController, SourceEffect, SourceFailure,
    SourceMode, SourceOutcome, TaggedInput,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// `value` ms after a crossing at zero, as [`activate_remote`]'s one-record push makes.
fn after_crossing(value: u64) -> Duration {
    ms(value)
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
    assert_frame(
        edge.effects.iter().next().unwrap(),
        4,
        0,
        Message::ActivateDisplayAt {
            display_id: DisplayId(2),
            position: entry,
        },
    );
    complete_activation(source, DisplayId(2), after_crossing(0));
}

/// Acknowledges the first activation, of `display`, and completes its native capture barrier.
fn complete_activation(source: &mut SourceController, display: DisplayId, now: Duration) {
    let acknowledged = source.on_remote_frame(&activation_ack_for(4, 0, display), now);
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

/// From a pointer resting on an edge: one record carrying the full distance along the unit `push`
/// at `now`, whose outcome it yields.
fn push_through(source: &mut SourceController, push: Point, now: Duration) -> SourceOutcome {
    capture_on_route(
        source,
        NormalizedInput::RelativeMotion(scaled(push, PUSH_THROUGH_DISTANCE)),
        now,
    )
}

/// From a pointer resting on an edge: ten records 10 ms apart along the unit `push`, each a tenth
/// of the full distance, so only the last, at `now + 90 ms`, can cross; yields its outcome.
fn flick(source: &mut SourceController, push: Point, now: Duration) -> SourceOutcome {
    let tenth = NormalizedInput::RelativeMotion(scaled(push, PUSH_THROUGH_DISTANCE / 10.0));
    for record in 0..9 {
        let held = capture_on_route(source, tenth, now + ms(10 * record));
        assert_eq!(held.failure, None);
        assert!(!sends_activation(&held) && !sends_release_all(&held));
    }
    capture_on_route(source, tenth, now + ms(90))
}

/// How long [`sustained_push_crosses`] pushes.
const SUSTAINED: Duration = Duration::from_millis(100);

/// Pushes `by` every 10 ms from `from` and last at `from + SUSTAINED`, far enough to cross any open
/// edge; true if any of it crossed.
fn sustained_push_crosses(source: &mut SourceController, by: Point, from: Duration) -> bool {
    let last = from + SUSTAINED;
    let mut crossed = false;
    let mut now = from;
    loop {
        let pushed = capture_on_route(source, NormalizedInput::RelativeMotion(by), now);
        assert_eq!(pushed.failure, None);
        crossed |= sends_activation(&pushed) || sends_release_all(&pushed);
        if now == last {
            return crossed;
        }
        now = (now + ms(10)).min(last);
    }
}

/// The px [`rest_on_right_edge`]'s arrival pushes outward, which the push counts.
const ARRIVAL: f64 = 1.0;

/// Brings the local pointer from inside display 1 onto its linked right edge, and delivers that
/// arrival's relative half, [`ARRIVAL`] px on.
fn rest_on_right_edge(source: &mut SourceController, now: Duration) {
    move_to(source, Point::new(50.0, 50.0), now);
    move_to(source, Point::new(99.0, 50.0), now);
    let overshoot = move_horizontally(source, ARRIVAL, now);
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

/// A local absolute sample the full distance on from `from` along the unit `push` at `now`, whose
/// outcome it yields.
fn absolute_push_through(
    source: &mut SourceController,
    from: Point,
    push: Point,
    now: Duration,
) -> SourceOutcome {
    let by = scaled(push, PUSH_THROUGH_DISTANCE);
    capture_on_route(
        source,
        NormalizedInput::AbsoluteMotion(Point::new(from.x + by.x, from.y + by.y)),
        now,
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
    let back = after_crossing(1);
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
    let short = move_horizontally(&mut source, 1.0 - PUSH_THROUGH_DISTANCE, after_crossing(55));
    assert!(short.effects.is_empty());
    assert!(sends_release_all(&move_horizontally(
        &mut source,
        -1.0,
        after_crossing(56)
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
            crossed_in_budget(crossed, 1.0),
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
    let on_seam = ms(406);
    // The next challenge goes out during the hold; its reply is fresh, but the barrier is still owed.
    let ping = source.tick(on_seam + ms(4));
    assert_frame(ping.effects.iter().next().unwrap(), 3, 4, Message::Ping(2));
    let fresh = Frame::new(SessionEpoch::new(3).unwrap(), 4, Message::Pong(2));
    assert!(
        source
            .on_remote_frame(&fresh, on_seam + ms(14))
            .failure
            .is_none()
    );
    assert!(source.held_since().is_some());
    // Pushing through the seam after that fresh reply still keeps input local.
    let wall = move_horizontally(&mut source, PUSH_THROUGH_DISTANCE, on_seam + ms(19));
    assert_eq!(wall.failure, None);
    assert!(
        wall.effects.is_empty(),
        "{:?}",
        wall.effects.iter().collect::<Vec<_>>()
    );
    assert_eq!(source.mode(), SourceMode::Local);
    // The receiver's acknowledgement of the barrier ends the hold.
    let acknowledged = on_seam + ms(24);
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
        .pump(&mut windows_source, returned, after_crossing(1))
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
        .pump(&mut mac_source, returned, after_crossing(1))
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
        .pump(&mut source, return_edge, after_crossing(1))
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
        after_crossing(2),
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
    // Slow samples on through it push through once the full distance past where the push began.
    let crept = ms(201);
    creep(
        Point::new(203.0, 50.0),
        Point::new(10.0, 0.0),
        ms(51),
        crept,
        |point, now| move_to(&mut source, point, now),
    );
    move_to(
        &mut source,
        Point::new(200.5 + PUSH_THROUGH_DISTANCE - 0.5, 50.0),
        crept,
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            200.5 + PUSH_THROUGH_DISTANCE,
            50.0,
        ))),
        crept + ms(1),
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
    let pushed = ms(101);
    let arrived = Point::new(230.0, 60.0);
    creep(
        arrived,
        Point::new(5.0, 0.0),
        ms(1),
        pushed,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            arrived.x + PUSH_THROUGH_DISTANCE,
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
    // Resting there long past a pause, the push on starts afresh from where the pointer rests.
    let pushed = ms(300);
    creep(
        Point::new(255.0, 80.0),
        Point::new(5.0, 0.0),
        ms(200),
        pushed,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            250.0 + PUSH_THROUGH_DISTANCE,
            80.0,
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
    let pushed = ms(102);
    creep(
        Point::new(255.0, 50.0),
        Point::new(5.0, 0.0),
        ms(2),
        pushed,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            250.0 + PUSH_THROUGH_DISTANCE,
            50.0,
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
    let crept = ms(101);
    let arrived = Point::new(230.0, 60.0);
    creep(arrived, Point::new(5.0, 0.0), ms(1), crept, |point, now| {
        let pushed = source.observe_pointer(point, now);
        assert_eq!(pushed.failure, None);
        assert!(pushed.effects.is_empty());
    });
    let crossing =
        source.observe_pointer(Point::new(arrived.x + PUSH_THROUGH_DISTANCE, 60.0), crept);
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
    let ignored = source.observe_pointer(Point::new(150.0, 50.0), crept + ms(1));
    assert_eq!(ignored.failure, None);
    assert!(ignored.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::AwaitActivationAcknowledgement);
}

#[test]
fn polled_positions_wiggling_past_an_edge_never_add_up_to_a_push() {
    // Only polled positions, as from a source without relative input: out on display 5 the
    // pushing pointer goes 30 px further and back again and again, giving the depth back each time.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    let crept = ms(101);
    let mut observe = |x: f64, at: Duration| {
        let observed = source.observe_pointer(Point::new(x, 50.0), at);
        assert_eq!(observed.failure, None);
        sends_activation(&observed)
    };
    let arrived = 230.0;
    let last = creep(
        Point::new(arrived, 50.0),
        Point::new(5.0, 0.0),
        ms(1),
        crept,
        |point, now| assert!(!observe(point.x, now)),
    )
    .x;
    let wiggles = (PUSH_THROUGH_DISTANCE / 30.0) as u64 + 1;
    for wiggle in 0..wiggles {
        let at = crept + ms(20 * wiggle);
        assert!(!observe(last + 30.0, at), "wiggle {wiggle} out");
        assert!(!observe(last, at + ms(10)), "wiggle {wiggle} back");
    }
    // Still pushing, the full distance from where the push began crosses, and no less.
    let pushed = crept + ms(20 * wiggles);
    assert!(!observe(arrived + PUSH_THROUGH_DISTANCE - 1.0, pushed));
    assert!(observe(arrived + PUSH_THROUGH_DISTANCE, pushed + ms(10)));
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
    let pushed = ms(101);
    let arrived = Point::new(250.0, 50.0);
    creep(
        arrived,
        Point::new(5.0, 0.0),
        ms(1),
        pushed,
        |point, now| move_to(&mut source, point, now),
    );
    let crossing = source.fresh_capture(
        local(NormalizedInput::AbsoluteMotion(Point::new(
            arrived.x + PUSH_THROUGH_DISTANCE,
            50.0,
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
        complete_return(&mut source, 1, after_crossing(5)),
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
    let back = after_crossing(3);
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
    let rested = back + ms(2) + SUSTAINED;
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
    let rested = rested + ms(4) + SUSTAINED;
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
        shove + SUSTAINED + ms(10),
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
    after_crossing(3) + ms(value)
}

#[test]
fn a_guarded_home_edge_releases_once_the_pointer_is_that_far_onto_a_display_not_in_use() {
    // Display 5, past display 4's right edge line at x = 200, is not in use: the OS cursor moves
    // onto it while the tracked cursor stays pinned inside display 4.
    let right_edge = 200.0;
    let right = Point::new(1.0, 0.0);
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
        let push = absolute_push_through(&mut source, beyond, right, after_return(1));
        assert_eq!(push.failure, None);
        assert_eq!(sends_activation(&push), crosses, "{past} px past the edge");
        if !crosses {
            let released = Point::new(right_edge + ENTRY_GUARD_DISTANCE, 50.0);
            move_to(&mut source, released, after_return(2));
            assert!(
                sends_activation(&absolute_push_through(
                    &mut source,
                    released,
                    right,
                    after_return(2)
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
        let moved = after_crossing(2) + SUSTAINED;
        push_across(&mut remote, entered, -ENTRY_GUARD_DISTANCE, moved + ms(1));
        assert!(!sends_release_all(&push_across(
            &mut remote,
            entered,
            ENTRY_GUARD_DISTANCE + 3.0,
            moved + ms(2)
        )));
        let short = push_across(
            &mut remote,
            entered,
            PUSH_THROUGH_DISTANCE - 1.0,
            moved + ms(3),
        );
        assert!(short.effects.is_empty(), "{entered:?}");
        assert!(
            sends_release_all(&push_across(&mut remote, entered, 1.0, moved + ms(4))),
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
        let rested = after_return(1) + SUSTAINED;
        move_to(
            &mut relative,
            edge_point(home_edge, ENTRY_GUARD_DISTANCE),
            rested + ms(2),
        );
        move_to(&mut relative, at, rested + ms(3));
        let crossing = push_through(&mut relative, push, rested + ms(4));
        assert!(sends_activation(&crossing), "{entered:?}");

        // Local absolute samples past the home edge: 3 px is held, 24 px releases the guard and
        // starts a push that crosses the full distance on from there.
        let mut absolute = source_on(topology_linked_on_every_edge(), DisplayId(1));
        round_trip(&mut absolute, at, push, entry);
        stay_alive(&mut absolute, 3, after_return(0));
        move_to(&mut absolute, edge_point(home_edge, -3.0), after_return(1));
        let released = edge_point(home_edge, -ENTRY_GUARD_DISTANCE);
        move_to(&mut absolute, released, after_return(2));
        let beyond = |distance: f64| {
            let by = across(home_edge, distance);
            Point::new(released.x + by.x, released.y + by.y)
        };
        move_to(
            &mut absolute,
            beyond(PUSH_THROUGH_DISTANCE - 1.0),
            after_return(3),
        );
        let crossing = capture_on_route(
            &mut absolute,
            NormalizedInput::AbsoluteMotion(beyond(PUSH_THROUGH_DISTANCE)),
            after_return(4),
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
    // Letting go of the seam ends the streak; pressing it again waits before it retries.
    let pause = declined + retry * 2 + ms(10);
    stay_alive(&mut seam, 3, pause);
    move_to(&mut seam, Point::new(50.0, 50.0), pause);
    move_to(&mut seam, Point::new(99.0, 50.0), pause + ms(1));
    let pressed = pause + ms(1);
    assert!(press(&mut seam, pressed).effects.is_empty());
    assert!(sends_activation(&press(&mut seam, pressed + retry)));
    decline(&mut seam, 7, DeclineReason::Disabled, pressed + retry);
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
    // The flick that crossed logs once the other computer has the pointer, and the flick back
    // home once the return begins, each with the travel the rule counted.
    let mut source = source();
    move_to(&mut source, Point::new(99.0, 50.0), ms(0));
    assert!(sends_activation(&flick(
        &mut source,
        Point::new(1.0, 0.0),
        ms(0)
    )));
    complete_activation(&mut source, DisplayId(2), ms(90));
    let crossed = format!("run_ms=90 travel={PUSH_THROUGH_DISTANCE:.1} off_axis=0 end=Crossed");
    assert_eq!(
        logged(&format!(
            "push-through: edge=seam-out platform=Windows {crossed}"
        )),
        1
    );
    stay_alive(&mut source, 3, ms(90));
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, ms(91));
    move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE - 3.0, ms(91));
    assert!(sends_release_all(&flick(
        &mut source,
        Point::new(-1.0, 0.0),
        ms(92)
    )));
    assert_eq!(
        logged(&format!(
            "push-through: edge=seam-back platform=Windows {crossed}"
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
    assert_eq!(logged("run_ms=60 travel=7.0 off_axis=1 end=Inward"), 1);
    for at in (300..=360).step_by(10) {
        step(&mut source, out, at);
    }
    let resumed = 360 + PUSH_THROUGH_RESET.as_millis() as u64;
    step(&mut source, out, resumed);
    assert_eq!(logged("run_ms=60 travel=7.0 off_axis=0 end=Pause"), 1);
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
    let full = 3 + PUSH_THROUGH_DISTANCE as u64;
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

/// A push at a count a ms, the slowest these tests hold to it, crosses within this many ms.
const CROSSING_BUDGET_MS: u64 = 350;

/// True if a push of up to `speed` outward a ms crossed `elapsed` after setting off: no sooner
/// than it could carry the full distance, and within [`CROSSING_BUDGET_MS`].
fn crossed_in_budget(elapsed: Option<Duration>, speed: f64) -> bool {
    let soonest = Duration::from_secs_f64(PUSH_THROUGH_DISTANCE / speed / 1000.0);
    elapsed.is_some_and(|elapsed| (soonest..=ms(CROSSING_BUDGET_MS)).contains(&elapsed))
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
fn a_windows_flick_crosses_on_the_report_that_carries_it_the_full_distance() {
    // A 1000 Hz mouse at 1600 DPI flicked fast: 30 raw counts in every 1 ms report. Three reach
    // the edge, the third's counts already pushing, then the flick carries on for `carry_on` ms.
    let counted = |carry_on: u64| 30.0 * (carry_on + 1) as f64;
    for carry_on in [5, 7, 8, 20] {
        for hook_past_edge in [false, true] {
            let mut source = windows_source();
            move_to(&mut source, Point::new(10.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
            let crossed = (1..=3 + carry_on).find(|report| {
                windows_report(
                    &mut source,
                    &mut cursor,
                    Point::new(30.0, 0.0),
                    hook_past_edge,
                    ms(*report),
                )
            });
            let reaches = (0..=carry_on).find(|on| counted(*on) >= PUSH_THROUGH_DISTANCE);
            assert_eq!(
                crossed,
                reaches.map(|on| 3 + on),
                "{carry_on} ms, hook past the edge: {hook_past_edge}"
            );
        }
    }
}

#[test]
fn a_mac_trackpad_flick_crosses_on_the_record_that_carries_it_the_full_distance() {
    // Trackpad records every 8 to 16 ms, slowing as the finger stops. The first reaches the edge
    // from 90 points in, its whole delta already pushing.
    for interval in [8, 12, 16] {
        for (deltas, crossing) in [
            ([-150.0, -120.0, -100.0, -80.0, -50.0], Some(2)),
            ([-100.0, -60.0, -40.0, -50.0, -10.0], Some(4)),
            ([-100.0, -60.0, -40.0, -20.0, -10.0], None),
        ] {
            let mut source = mac_source_controller();
            move_to(&mut source, Point::new(90.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(90.0, 50.0));
            let crossed = (1..).zip(deltas).find_map(|(record, dx)| {
                mac_event(
                    &mut source,
                    &mut cursor,
                    Point::new(dx, 0.0),
                    ms(record * interval),
                )
                .then_some(record)
            });
            assert_eq!(crossed, crossing, "{deltas:?} every {interval} ms");
        }
    }
}

#[test]
fn a_bump_that_slows_to_a_stop_past_the_edge_never_crosses() {
    for tail in [20, 30] {
        // Windows: 3 counts a ms reach the edge, then slow to a stop within 50 counts.
        for hook_past_edge in [false, true] {
            let mut source = windows_source();
            move_to(&mut source, Point::new(90.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(90.0, 50.0));
            let mut crossed = false;
            for report in 1..=3 {
                crossed |= windows_report(
                    &mut source,
                    &mut cursor,
                    Point::new(3.0, 0.0),
                    hook_past_edge,
                    ms(report),
                );
            }
            let slowing = reports(tail, |t| Point::new(throw_tail(3.0, tail as f64, t), 0.0));
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
        // Mac: a record of 10 points reaches the edge, then the rest slows to a stop.
        for interval in [8, 12, 16] {
            let mut source = mac_source_controller();
            move_to(&mut source, Point::new(10.0, 50.0), ms(0));
            let mut cursor = OsCursor::on_square(Point::new(10.0, 50.0));
            let speed = 10.0 / interval as f64;
            let slid = |t: u64| throw_tail(speed, tail as f64, t as f64);
            let mut crossed = mac_event(
                &mut source,
                &mut cursor,
                Point::new(-10.0, 0.0),
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
    // of that: 300 ms down, then 1.5 s scrubbing up and down, whose drift alone would cross.
    let slide = |t: f64| {
        let slid = 1.5 * t;
        Point::new(0.1 * slid, 450.0 - (slid % 900.0 - 450.0).abs())
    };
    let duration = 1800;
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
            crossed_in_budget(turned, 3.0),
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
    assert!(crossed_in_budget(turned, 1.0), "{turned:?}");
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
                crossed_in_budget(crossing.map(|(elapsed, _)| elapsed), 1.0),
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
fn a_push_after_resting_on_the_edge_crosses_once_it_carries_the_full_distance_on_both_platforms() {
    // Arrive, rest past a pause, then push on from here.
    let start = ms(150);
    // Windows at 1000 and 3000 counts a second: whole counts, so the first report whose count
    // since setting off reaches the full distance crosses.
    for speed in [1.0, 3.0] {
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
                .map(|(t, _)| t);
            let reaches = (PUSH_THROUGH_DISTANCE / speed).ceil() as u64;
            assert_eq!(
                crossed_after,
                Some(reaches),
                "at {speed} counts a ms, hook past the edge: {hook_past_edge}"
            );
        }
    }
    // Mac at 1000 and 3000 points a second, records every 8 or 16 ms, timed from the first, which
    // already carries its interval's travel.
    for speed in [1.0, 3.0] {
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
            let step = speed * interval as f64;
            let crossed_after = (0..=CROSSING_BUDGET_MS / interval)
                .map(|record| record * interval)
                .find(|elapsed| {
                    mac_event(
                        &mut source,
                        &mut cursor,
                        Point::new(-step, 0.0),
                        start + ms(*elapsed),
                    )
                });
            let records = (PUSH_THROUGH_DISTANCE / step).ceil() as u64;
            assert_eq!(
                crossed_after,
                Some((records - 1) * interval),
                "at {speed} points a ms, records every {interval} ms"
            );
        }
    }
}

#[test]
fn a_poll_that_sees_the_arrival_first_counts_the_arrivals_travel_as_the_record_would() {
    for polled in [false, true] {
        let mut source = mac_source_controller();
        move_to(&mut source, Point::new(50.0, 50.0), ms(0));
        // The polled OS cursor is on the edge before the tap delivers the record that took it
        // there; unpolled, that record's own position arrives first.
        if polled {
            assert!(
                source
                    .observe_pointer(Point::new(0.0, 50.0), ms(10))
                    .effects
                    .is_empty()
            );
        }
        let mut cursor = OsCursor::on_square(Point::new(50.0, 50.0));
        let left = |dx: f64| Point::new(-dx, 0.0);
        // Either way the push begins before the record's delta, which counts in full.
        assert!(!mac_event(&mut source, &mut cursor, left(120.0), ms(11)));
        assert!(!mac_event(
            &mut source,
            &mut cursor,
            left(PUSH_THROUGH_DISTANCE - 121.0),
            ms(21)
        ));
        assert!(
            mac_event(&mut source, &mut cursor, left(1.0), ms(31)),
            "polled: {polled}"
        );
    }
}

#[test]
fn past_an_edge_an_absolute_sample_and_its_relative_half_count_their_step_once() {
    // Display 5 past display 4's right edge is not in use, so the OS pointer really moves there.
    let mut source = source_on(topology_with_displays_not_in_use(), DisplayId(4));
    move_to(&mut source, Point::new(150.0, 50.0), ms(0));
    move_to(&mut source, Point::new(199.0, 50.0), ms(10));
    let arrived = 49.0;
    move_horizontally(&mut source, arrived, ms(10));
    // 10 px every 10 ms from the arrival at 10 ms, whose relative half carried 49: the relative
    // travel leads the absolute depth, and crosses on its own, never summed with the depth.
    let expected = ((PUSH_THROUGH_DISTANCE - arrived) / 10.0).ceil() as u64;
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
    assert_eq!(crossed, Some((expected, "relative")));
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
    // Arriving 30 px deep with 80 px of relative travel, then 10 px short of crossing.
    assert!(!step(&mut source, 230.0, 80.0, ms(10)));
    let on = PUSH_THROUGH_DISTANCE - 90.0;
    let x = 230.0 + on;
    assert!(!step(&mut source, x, on, ms(20)));
    // A wiggle back in and out again: the push starts over from where it turns out again.
    assert!(!step(&mut source, x - 10.0, -10.0, ms(30)));
    let x = x + 10.0;
    assert!(!step(&mut source, x, 20.0, ms(40)));
    let on = PUSH_THROUGH_DISTANCE - 21.0;
    assert!(!step(&mut source, x + on, on, ms(50)));
    assert!(step(&mut source, x + on + 1.0, 1.0, ms(60)));
}

#[test]
fn a_push_crosses_only_once_it_reaches_the_full_distance() {
    // The arrival's own px counts from the start of the push.
    let rest = PUSH_THROUGH_DISTANCE - ARRIVAL;
    let mut steps = source();
    rest_on_right_edge(&mut steps, ms(0));
    for _ in 1..rest as u32 {
        let step = move_horizontally(&mut steps, 1.0, ms(1));
        assert_eq!(step.failure, None);
        assert!(step.effects.is_empty());
    }
    assert!(sends_activation(&move_horizontally(&mut steps, 1.0, ms(1))));

    let mut short = source();
    rest_on_right_edge(&mut short, ms(0));
    let pushed = move_horizontally(&mut short, rest - 1.0, ms(1));
    assert_eq!(pushed.failure, None);
    assert!(pushed.effects.is_empty());
    assert_eq!(short.mode(), SourceMode::Local);

    let mut full = source();
    rest_on_right_edge(&mut full, ms(0));
    assert!(sends_activation(&move_horizontally(&mut full, rest, ms(1))));
}

#[test]
fn a_quick_flick_that_carries_the_full_distance_crosses() {
    // From rest on the linked edge, 30 counts every 10 ms for 100 ms: 300 in all, reaching the
    // full distance on the ninth record.
    let mut source = source();
    move_to(&mut source, Point::new(50.0, 50.0), ms(0));
    move_to(&mut source, Point::new(99.0, 50.0), ms(1));
    let crossed = (1..=10).find(|record| {
        sends_activation(&move_horizontally(&mut source, 30.0, ms(1 + 10 * record)))
    });
    assert_eq!(crossed, Some(9));
}

#[test]
fn a_bump_of_fifty_never_crosses_and_logs_its_travel() {
    capture_log();
    // Five records of 10 counts against the linked edge, then the hand stops.
    let mut source = source();
    move_to(&mut source, Point::new(50.0, 50.0), ms(0));
    move_to(&mut source, Point::new(99.0, 50.0), ms(1));
    for record in 1..=5 {
        let bumped = move_horizontally(&mut source, 10.0, ms(1 + 10 * record));
        assert_eq!(bumped.failure, None);
        assert!(bumped.effects.is_empty());
    }
    // The next push, after the pause, starts over and ends the bump's episode.
    let later = move_horizontally(&mut source, 10.0, ms(200));
    assert!(later.effects.is_empty());
    assert_eq!(source.mode(), SourceMode::Local);
    assert_eq!(
        logged(
            "push-through: edge=seam-out platform=Windows run_ms=50 travel=50.0 off_axis=0 end=Pause"
        ),
        1
    );
}

/// Display 1 on the Windows computer and display 2 on the Mac, 400 by 100 logical units, linked
/// through 1's right edge and 2's left, at `windows_scale` and `mac_scale` pixels a unit.
fn scaled_pair(windows_scale: f64, mac_scale: f64) -> Topology {
    let windows = Display::new(
        DisplayId(1),
        device(1),
        "windows".into(),
        NativeSize::new(400, 100),
        LogicalSize::new(400.0, 100.0),
        Point::new(0.0, 0.0),
        windows_scale,
        None,
        true,
    );
    let mac = Display::new(
        DisplayId(2),
        device(2),
        "mac".into(),
        NativeSize::new((400.0 * mac_scale) as u32, (100.0 * mac_scale) as u32),
        LogicalSize::new(400.0, 100.0),
        Point::new(0.0, 0.0),
        mac_scale,
        None,
        true,
    );
    two_machine_topology(
        vec![windows, mac],
        vec![
            full_link(1, Edge::Right, 2, Edge::Left),
            full_link(2, Edge::Left, 1, Edge::Right),
        ],
    )
}

/// A source on `machine`'s display of [`scaled_pair`], resting at `at` on the seam.
fn scaled_source(topology: Topology, machine: u8, at: Point) -> SourceController {
    let mut source = SourceController::new(
        topology,
        device(machine),
        DisplayId(u64::from(machine)),
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap();
    move_to(&mut source, at, ms(0));
    source
}

/// [`scaled_source`] controlling the other computer, entered by a one-record push along `out`.
fn controlling_across_scales(
    topology: Topology,
    machine: u8,
    at: Point,
    out: Point,
) -> SourceController {
    let mut source = scaled_source(topology, machine, at);
    assert!(sends_activation(&push_through(&mut source, out, ms(0))));
    complete_activation(&mut source, DisplayId(u64::from(3 - machine)), ms(0));
    source
}

#[test]
fn a_push_home_counts_the_controlling_mouses_own_travel_on_any_display_scale() {
    // Relative motion reaches the source in the controlled display's units: a Windows mouse's
    // counts shrink onto Mac points by the Mac's scale, Mac points grow onto Windows pixels by the
    // Windows scale. The push home takes the same hand travel either way: the 13th record of 20.
    let home_on = |source: &mut SourceController, home: Point, per_unit: f64| {
        let onto = |units: f64| NormalizedInput::RelativeMotion(scaled(home, units * per_unit));
        // Off the entry edge past its guard, then back onto it.
        assert!(!sends_release_all(&capture_on_route(
            source,
            onto(-60.0),
            ms(1)
        )));
        assert!(!sends_release_all(&capture_on_route(
            source,
            onto(66.0),
            ms(2)
        )));
        (1..=20).find(|record| {
            sends_release_all(&capture_on_route(source, onto(20.0), ms(2 + 10 * record)))
        })
    };
    for mac_scale in [1.0, 2.0] {
        let topology = scaled_pair(1.0, mac_scale);
        let mut windows =
            controlling_across_scales(topology, 1, Point::new(399.0, 50.0), Point::new(1.0, 0.0));
        let per_unit = windows
            .motion_target()
            .unwrap()
            .per_captured_unit(Platform::Windows);
        assert_eq!(per_unit, 1.0 / mac_scale);
        let crossed = home_on(&mut windows, Point::new(-1.0, 0.0), per_unit);
        assert_eq!(crossed, Some(13), "Mac at {mac_scale}x");
    }
    for windows_scale in [1.0, 1.5] {
        let topology = scaled_pair(windows_scale, 1.0);
        let mut mac =
            controlling_across_scales(topology, 2, Point::new(0.0, 50.0), Point::new(-1.0, 0.0));
        let per_unit = mac
            .motion_target()
            .unwrap()
            .per_captured_unit(Platform::MacOs);
        assert_eq!(per_unit, windows_scale);
        let crossed = home_on(&mut mac, Point::new(1.0, 0.0), per_unit);
        assert_eq!(crossed, Some(13), "Windows at {windows_scale}x");
    }
}

#[test]
fn the_record_that_carries_the_push_past_the_full_distance_crosses_and_enters_that_far_in() {
    // 200 in, then one record of 120: that record crosses, and the 70 past the full distance
    // carries on past the entry, converted as relative motion onto that display is.
    for (mac_scale, entry_x) in [(1.0, 71.0), (2.0, 36.0)] {
        let mut windows = scaled_source(scaled_pair(1.0, mac_scale), 1, Point::new(399.0, 50.0));
        assert!(
            move_horizontally(&mut windows, 200.0, ms(1))
                .effects
                .is_empty()
        );
        let crossing = move_horizontally(&mut windows, 120.0, ms(2));
        let entered = activation(&crossing);
        assert_eq!(
            entered,
            Some((DisplayId(2), Point::new(entry_x, 50.0))),
            "Mac at {mac_scale}x"
        );
    }
    for (windows_scale, entry_x) in [(1.0, 329.0), (1.5, 294.0)] {
        let mut mac = scaled_source(scaled_pair(windows_scale, 1.0), 2, Point::new(0.0, 50.0));
        assert!(
            move_horizontally(&mut mac, -200.0, ms(1))
                .effects
                .is_empty()
        );
        let crossing = move_horizontally(&mut mac, -120.0, ms(2));
        let entered = activation(&crossing);
        assert_eq!(
            entered,
            Some((DisplayId(1), Point::new(entry_x, 50.0))),
            "Windows at {windows_scale}x"
        );
    }
    // Home from a Mac at 2x the same way, the returning pointer lands 70 px in from the seam.
    let topology = scaled_pair(1.0, 2.0);
    let mut returning =
        controlling_across_scales(topology, 1, Point::new(399.0, 50.0), Point::new(1.0, 0.0));
    let onto = |counts: f64| NormalizedInput::RelativeMotion(Point::new(counts / 2.0, 0.0));
    capture_on_route(&mut returning, onto(60.0), ms(1));
    capture_on_route(&mut returning, onto(-66.0), ms(2));
    let short = capture_on_route(&mut returning, onto(-200.0), ms(3));
    assert!(!sends_release_all(&short));
    assert!(sends_release_all(&capture_on_route(
        &mut returning,
        onto(-120.0),
        ms(4)
    )));
    assert_eq!(
        complete_return(&mut returning, 1, ms(5)),
        (DisplayId(1), Point::new(329.0, 50.0))
    );
}

/// The messages `outcome` sends the other computer.
fn sent(outcome: &SourceOutcome) -> Vec<Message> {
    outcome
        .effects
        .iter()
        .filter_map(|effect| match effect {
            SourceEffect::RemoteFrame(frame) => Some(frame.message.clone()),
            _ => None,
        })
        .collect()
}

/// A key and a mouse button pressed and released at `now`, none of which may reach the peer.
fn tap_key_and_button(source: &mut SourceController, now: Duration) -> Vec<Message> {
    let key = |pressed| NormalizedInput::Key {
        usage: HidUsage(0x04),
        pressed,
        repeat: false,
        modifiers: ModifierState(0),
    };
    let button = |pressed| NormalizedInput::Button {
        button: MouseButton::Left,
        pressed,
        at: now,
    };
    [key(true), key(false), button(true), button(false)]
        .into_iter()
        .flat_map(|event| sent(&capture_on_route(source, event, now)))
        .collect()
}

/// A Windows source pushed through onto a Mac at 2x, awaiting the activation acknowledgement,
/// with no route settled so its relative motion comes in counts.
fn handing_over_to_a_retina_mac() -> SourceController {
    let mut source = scaled_source(scaled_pair(1.0, 2.0), 1, Point::new(399.0, 50.0));
    assert!(sends_activation(&push_through(
        &mut source,
        Point::new(1.0, 0.0),
        ms(0)
    )));
    assert_eq!(source.motion_target(), None);
    source
}

#[test]
fn motion_while_a_crossing_is_acknowledged_lands_on_the_other_screen() {
    // 40 counts right and 8 down while the Mac acknowledges and the route switches: 20 and 4
    // points on from the entry, sent as the first motion once the route is remote.
    let mut source = handing_over_to_a_retina_mac();
    for at in [1, 2] {
        let moved = capture_on_route(
            &mut source,
            NormalizedInput::RelativeMotion(Point::new(10.0, 4.0)),
            ms(at),
        );
        assert!(sent(&moved).is_empty());
    }
    let acknowledged = source.on_remote_frame(&activation_ack_for(4, 0, DisplayId(2)), ms(3));
    assert!(sent(&acknowledged).is_empty());
    let moved = move_horizontally(&mut source, 20.0, ms(4));
    assert!(sent(&moved).is_empty());
    let request = route_request(&acknowledged);
    assert_eq!(source.bind_capture_route(request, 1, ms(5)).failure, None);
    let barrier = source.fresh_capture(
        routed(
            NormalizedInput::RouteChanged {
                remote: true,
                revision: 1,
            },
            true,
            1,
        ),
        ms(5),
    );
    assert_eq!(
        sent(&barrier),
        [Message::Motion(Motion::Absolute(Point::new(21.0, 54.0)))]
    );
    // The pointer model moves on from there.
    assert_eq!(
        moved_to(&move_horizontally(&mut source, 1.0, ms(6))),
        Some(Point::new(22.0, 54.0))
    );
}

#[test]
fn keys_and_buttons_while_a_crossing_is_acknowledged_never_reach_the_other_screen() {
    let mut source = handing_over_to_a_retina_mac();
    let mut replayed = tap_key_and_button(&mut source, ms(1));
    let acknowledged = source.on_remote_frame(&activation_ack_for(4, 0, DisplayId(2)), ms(2));
    replayed.extend(sent(&acknowledged));
    replayed.extend(tap_key_and_button(&mut source, ms(3)));
    source.bind_capture_route(route_request(&acknowledged), 1, ms(4));
    let barrier = source.fresh_capture(
        routed(
            NormalizedInput::RouteChanged {
                remote: true,
                revision: 1,
            },
            true,
            1,
        ),
        ms(4),
    );
    replayed.extend(sent(&barrier));
    assert_eq!(source.mode(), SourceMode::Remote);
    assert!(
        replayed
            .iter()
            .all(|message| !matches!(message, Message::Key(_) | Message::Button(_))),
        "{replayed:?}"
    );
}

#[test]
fn motion_while_a_crossing_is_acknowledged_is_dropped_if_the_crossing_does_not_happen() {
    let wiggle = |source: &mut SourceController, at: u64| {
        let moved = capture_on_route(
            source,
            NormalizedInput::RelativeMotion(Point::new(30.0, 6.0)),
            ms(at),
        );
        assert!(sent(&moved).is_empty());
    };
    // Declined: the pointer stays home, and the next crossing enters with nothing carried.
    let mut declined = handing_over_to_a_retina_mac();
    wiggle(&mut declined, 1);
    let decline = declined.on_remote_frame(
        &Frame::new(
            SessionEpoch::new(4).unwrap(),
            0,
            Message::ActivationDeclined {
                display_id: DisplayId(2),
                reason: DeclineReason::Busy,
            },
        ),
        ms(2),
    );
    assert!(sent(&decline).is_empty());
    assert_eq!(declined.mode(), SourceMode::Local);
    let retry = ms(2) + DECLINE_RETRY_AFTER;
    assert!(sends_activation(&move_horizontally(
        &mut declined,
        PUSH_THROUGH_DISTANCE,
        retry
    )));
    let acknowledged = declined.on_remote_frame(&activation_ack_for(5, 0, DisplayId(2)), retry);
    declined.bind_capture_route(route_request(&acknowledged), 1, retry);
    let barrier = declined.fresh_capture(
        routed(
            NormalizedInput::RouteChanged {
                remote: true,
                revision: 1,
            },
            true,
            1,
        ),
        retry,
    );
    assert_eq!(declined.mode(), SourceMode::Remote);
    assert!(sent(&barrier).is_empty(), "{:?}", sent(&barrier));
    // Refused by this computer's capture once acknowledged: control comes home, nothing moves.
    let mut refused = handing_over_to_a_retina_mac();
    wiggle(&mut refused, 1);
    let acknowledged = refused.on_remote_frame(&activation_ack_for(4, 0, DisplayId(2)), ms(2));
    wiggle(&mut refused, 3);
    let refusal = refused.refuse_capture_activation(route_request(&acknowledged), ms(4));
    assert_eq!(sent(&refusal), [Message::ReleaseAll]);
    let released = refused.on_remote_frame(
        &Frame::new(SessionEpoch::new(4).unwrap(), 1, Message::ReleaseAck),
        ms(5),
    );
    assert!(released.effects.is_empty());
    assert_eq!(refused.mode(), SourceMode::Local);
}

#[test]
fn motion_while_a_return_is_acknowledged_lands_on_this_screen() {
    // Home from a Mac at 2x through the seam; 40 counts left and 6 down while the Mac
    // acknowledges the release move the restored pointer on from where the return entered.
    let returned = |edge_return: bool| {
        let topology = scaled_pair(1.0, 2.0);
        let mut source =
            controlling_across_scales(topology, 1, Point::new(399.0, 50.0), Point::new(1.0, 0.0));
        let onto = |counts: f64| NormalizedInput::RelativeMotion(Point::new(counts / 2.0, 0.0));
        capture_on_route(&mut source, onto(60.0), ms(1));
        capture_on_route(&mut source, onto(-66.0), ms(2));
        let release = if edge_return {
            capture_on_route(&mut source, onto(-PUSH_THROUGH_DISTANCE), ms(3))
        } else {
            source.request_local(ms(3))
        };
        assert!(sends_release_all(&release));
        assert_eq!(source.motion_target(), None);
        for (delta, at) in [(Point::new(-30.0, 6.0), 4), (Point::new(-10.0, 0.0), 5)] {
            let moved =
                capture_on_route(&mut source, NormalizedInput::RelativeMotion(delta), ms(at));
            assert!(moved.effects.is_empty());
        }
        complete_return(&mut source, 1, ms(6))
    };
    assert_eq!(returned(true), (DisplayId(1), Point::new(359.0, 56.0)));
    // A return asked for on this computer lands where it was sent, carrying nothing.
    assert_eq!(returned(false), (DisplayId(1), Point::new(399.0, 50.0)));
}

#[test]
fn motion_leaning_more_along_the_edge_than_half_its_push_never_crosses() {
    let pushed_through = |source: &mut SourceController, out: f64, along: f64| {
        (1..=20).find(|record| {
            let moved = capture_on_route(
                source,
                NormalizedInput::RelativeMotion(Point::new(out, along)),
                ms(10 * record),
            );
            sends_activation(&moved)
        })
    };
    // 25 out and 13 along a record, 500 out in all: too sideways to push at all.
    let mut sideways = source();
    rest_on_right_edge(&mut sideways, ms(0));
    assert_eq!(pushed_through(&mut sideways, 25.0, 13.0), None);
    assert_eq!(sideways.mode(), SourceMode::Local);
    // 25 out and 12.5 along is straight enough: the tenth record reaches the full distance.
    let mut straight = source();
    rest_on_right_edge(&mut straight, ms(0));
    assert_eq!(pushed_through(&mut straight, 25.0, 12.5), Some(10));
}

#[test]
fn leaving_the_edge_inward_starts_the_push_over() {
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    let short = PUSH_THROUGH_DISTANCE - 1.0;
    assert!(
        move_horizontally(&mut local, short - ARRIVAL, ms(1))
            .effects
            .is_empty()
    );
    move_to(&mut local, Point::new(97.0, 50.0), ms(2));
    move_to(&mut local, Point::new(99.0, 50.0), ms(3));
    assert!(
        move_horizontally(&mut local, short, ms(4))
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(&mut local, 1.0, ms(5))));

    let mut remote = source();
    activate_remote(&mut remote);
    stay_alive(&mut remote, 3, after_crossing(0));
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    let pushed = move_horizontally(&mut remote, -short, after_crossing(3));
    assert!(pushed.effects.is_empty());
    // Two px inward is off the edge; coming back is a new arrival.
    move_horizontally(&mut remote, 2.0, after_crossing(4));
    move_horizontally(&mut remote, -2.0, after_crossing(5));
    assert!(!sends_release_all(&move_horizontally(
        &mut remote,
        -short,
        after_crossing(6)
    )));
    assert!(sends_release_all(&move_horizontally(
        &mut remote,
        -1.0,
        after_crossing(7)
    )));
}

#[test]
fn a_pause_in_the_push_starts_it_over() {
    // 240 in, a pause, then 240 more: two pushes, neither the full distance.
    let most = 240.0;
    let mut paused = source();
    rest_on_right_edge(&mut paused, ms(0));
    assert!(
        move_horizontally(&mut paused, most - ARRIVAL, ms(10))
            .effects
            .is_empty()
    );
    let late = ms(10) + PUSH_THROUGH_RESET;
    assert!(
        move_horizontally(&mut paused, most, late)
            .effects
            .is_empty()
    );
    assert_eq!(paused.mode(), SourceMode::Local);
    // The fresh push still crosses once it has the full distance of its own.
    assert!(sends_activation(&move_horizontally(
        &mut paused,
        PUSH_THROUGH_DISTANCE - most,
        late + ms(10)
    )));

    // A gap just short of the pause keeps the push going.
    let mut brief = source();
    rest_on_right_edge(&mut brief, ms(0));
    assert!(
        move_horizontally(&mut brief, most - ARRIVAL, ms(10))
            .effects
            .is_empty()
    );
    assert!(sends_activation(&move_horizontally(
        &mut brief,
        PUSH_THROUGH_DISTANCE - most,
        ms(10) + PUSH_THROUGH_RESET - ms(1)
    )));
}

#[test]
fn sliding_along_the_edge_keeps_the_push() {
    let half = PUSH_THROUGH_DISTANCE / 2.0;
    let along = |dx: f64, dy: f64| NormalizedInput::RelativeMotion(Point::new(dx, dy));
    let mut local = source();
    rest_on_right_edge(&mut local, ms(0));
    assert!(
        capture_on_route(&mut local, along(half, 0.0), ms(1))
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
        capture_on_route(&mut local, along(half - 1.0 - ARRIVAL, 10.0), ms(3))
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
    move_horizontally(&mut remote, ENTRY_GUARD_DISTANCE, after_crossing(1));
    move_horizontally(&mut remote, -ENTRY_GUARD_DISTANCE - 3.0, after_crossing(2));
    let pushed = after_crossing(3);
    let slid = capture_on_route(&mut remote, along(0.0, 10.0), pushed);
    assert_frame(
        slid.effects.iter().next().unwrap(),
        4,
        3,
        Message::Motion(Motion::Absolute(Point::new(0.0, 60.0))),
    );
    assert!(
        capture_on_route(&mut remote, along(-half, 0.0), pushed)
            .effects
            .is_empty()
    );
    // Here too only the diagonal's outward part counts, while the pointer slides on.
    let diagonal = capture_on_route(&mut remote, along(1.0 - half, 10.0), pushed);
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
        pushed
    )));
    assert_eq!(
        complete_return(&mut remote, 1, pushed + ms(1)),
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
    let pushing = after_crossing(3);
    for _ in 1..PUSH_THROUGH_DISTANCE as u32 {
        let pushed = move_horizontally(&mut source, -1.0, pushing);
        assert!(!sends_release_all(&pushed));
        bridge.pump(&mut source, pushed, pushing).unwrap();
        assert_eq!(source.mode(), SourceMode::Remote);
    }
    let through = move_horizontally(&mut source, -1.0, pushing);
    bridge.pump(&mut source, through, pushing).unwrap();
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
fn absolute_samples_push_through_by_how_far_they_go_past_where_the_push_began() {
    // Space outside the layout past display 4, which the pointer arrives in 30 px deep and pushes
    // on through, drifting down a little.
    let mut deep = source();
    move_to(&mut deep, Point::new(150.0, 50.0), ms(0));
    let arrived = 230.0;
    move_to(&mut deep, Point::new(arrived, 50.0), ms(1));
    move_to(
        &mut deep,
        Point::new(arrived + PUSH_THROUGH_DISTANCE - 1.0, 60.0),
        ms(11),
    );
    let crossing = capture_on_route(
        &mut deep,
        NormalizedInput::AbsoluteMotion(Point::new(arrived + PUSH_THROUGH_DISTANCE, 60.0)),
        ms(21),
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
    let shoved = after_crossing(1) + SUSTAINED;
    stay_alive(&mut source, 4, shoved + ms(1));
    // 22 px in and back keeps the guard, so a sustained push still takes nothing.
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE - 2.0, shoved + ms(2));
    move_horizontally(&mut source, 2.0 - ENTRY_GUARD_DISTANCE, shoved + ms(3));
    assert!(!sustained_push_crosses(&mut source, shove, shoved + ms(4)));
    assert_eq!(source.mode(), SourceMode::Remote);
    // 24 px in releases it; back on the edge a full push returns.
    let shoved = shoved + ms(4) + SUSTAINED;
    move_horizontally(&mut source, ENTRY_GUARD_DISTANCE, shoved + ms(1));
    move_horizontally(&mut source, -ENTRY_GUARD_DISTANCE, shoved + ms(2));
    assert!(
        move_horizontally(&mut source, 1.0 - PUSH_THROUGH_DISTANCE, shoved + ms(3))
            .effects
            .is_empty()
    );
    assert!(sends_release_all(&move_horizontally(
        &mut source,
        -1.0,
        shoved + ms(4)
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
