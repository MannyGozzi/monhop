//! A source controller on one computer and a receiver on the other, each running its own health
//! loop, joined by a real quinn link: what the receiver injects after a stall, and whether the
//! session lives through it.

use super::link_tests::{Pair, negotiated_pair};
use super::*;
use crate::{
    session_health::{PEER_LIVENESS, RETREAT_AFTER},
    session_receiver::{DestinationAction, DestinationFailure, InputDestination, InputReceiver},
    session_source::{
        NormalizedInput, PUSH_THROUGH_DISTANCE, SourceController, SourceEffect, SourceMode,
        SourceOutcome, TaggedInput,
    },
    session_wire::{WRITE_DEADLINE, is_heartbeat},
};
use monhop_core::{
    Display, DisplayId, Edge, EdgeLink, HidUsage, LogicalSize, Machine, ModifierState, MouseButton,
    NativeSize, NormalizedSpan, Platform, Point, Topology,
};
use monhop_protocol::{DisplayDescription, DisplayTopology, Motion, SessionEpoch};
use tokio::time::{Instant, MissedTickBehavior, timeout};

const HELD_KEY: HidUsage = HidUsage(0x04);
const TYPED_KEY: HidUsage = HidUsage(0x05);
/// Long past the receiver's deadline, well inside `HOLD_LIMIT`.
const LONG_STALL: Duration = Duration::from_millis(1_500);
/// Far past the old 60 ms write deadline, yet 200 ms under `RETREAT_AFTER` so a loaded test
/// machine cannot make either side hold.
const SHORT_STALL: Duration = Duration::from_millis(180);
/// 44-byte scroll frames that more than fill the 16 KiB stream window at once, so the writer waits
/// on credit for the whole stall.
pub(super) const SCROLL_BURST: usize = 400;
/// Tick lateness on a loaded machine; a retreat that waited for `PEER_LIVENESS` still fails.
const RETREAT_SLACK: Duration = Duration::from_millis(80);

fn epoch() -> SessionEpoch {
    SessionEpoch::new(3).unwrap()
}

/// The controlling computer's display 1, whose right edge leads to the controlled computer's 2.
fn topology() -> Topology {
    let display = |id: u64, machine: u8| {
        Display::new(
            DisplayId(id),
            DeviceId([machine; 16]),
            format!("display-{id}"),
            NativeSize::new(100, 100),
            LogicalSize::new(100.0, 100.0),
            Point::new(0.0, 0.0),
            1.0,
            None,
            true,
        )
    };
    let link = |from: u64, from_edge, to: u64, to_edge| {
        let span = NormalizedSpan::new(0.0, 1.0).unwrap();
        EdgeLink::new(
            DisplayId(from),
            from_edge,
            span,
            DisplayId(to),
            to_edge,
            span,
            1.0,
        )
        .unwrap()
    };
    Topology::new(
        vec![
            Machine::new(DeviceId([1; 16]), Platform::Windows),
            Machine::new(DeviceId([2; 16]), Platform::MacOs),
        ],
        vec![display(1, 1), display(2, 2)],
        vec![
            link(1, Edge::Right, 2, Edge::Left),
            link(2, Edge::Left, 1, Edge::Right),
        ],
    )
    .unwrap()
}

fn controlled_displays() -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(2),
        name: "display-2".into(),
        native_width: 100,
        native_height: 100,
        logical_origin: Point::new(0.0, 0.0),
        logical_size: Point::new(100.0, 100.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .unwrap()
}

#[derive(Default)]
struct Injected(Vec<DestinationAction>);

impl InputDestination for Injected {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        self.0.push(action);
        Ok(())
    }
}

/// The controlling computer, with a native capture that grants every route change at once.
struct Controller {
    source: SourceController,
    link: LinkIo,
    reader: LinkReader,
    route: (bool, u64),
    sent: Vec<Message>,
}

impl Controller {
    fn apply(&mut self, outcome: SourceOutcome, now: Duration) {
        assert_eq!(outcome.failure, None, "the source controller failed");
        for effect in outcome.effects.iter() {
            let (request, remote) = match effect {
                SourceEffect::RemoteFrame(frame) => {
                    self.sent.push(frame.message.clone());
                    assert_eq!(self.link.send_outbound(frame.clone()), Ok(()));
                    continue;
                }
                SourceEffect::ActivateRemote { request } => (*request, true),
                SourceEffect::RestoreLocalAt { request, .. } => (*request, false),
            };
            self.route = (remote, self.route.1 + 1);
            let bound = self.source.bind_capture_route(request, self.route.1, now);
            assert!(bound.failure.is_none() && bound.effects.is_empty());
            let revision = self.route.1;
            self.capture(NormalizedInput::RouteChanged { remote, revision }, now);
        }
    }

    fn capture(&mut self, event: NormalizedInput, now: Duration) {
        let record = TaggedInput {
            event,
            routing_revision: self.route.1,
            remote: self.route.0,
            floor_generation: self.source.floor_generation(),
        };
        let outcome = self.source.on_captured(record, now);
        self.apply(outcome, now);
    }

    fn key(&mut self, usage: HidUsage, pressed: bool, now: Duration) {
        let modifiers = ModifierState::default();
        let repeat = false;
        let key = NormalizedInput::Key {
            usage,
            pressed,
            repeat,
            modifiers,
        };
        self.capture(key, now);
    }

    fn wiggle(&mut self, step: u128, now: Duration) {
        let dy = if step.is_multiple_of(2) { 1.0 } else { -1.0 };
        self.capture(NormalizedInput::RelativeMotion(Point::new(0.0, dy)), now);
    }

    fn scroll(&mut self, now: Duration) {
        let scroll = NormalizedInput::Scroll {
            horizontal: 0.0,
            vertical: 1.0,
        };
        self.capture(scroll, now);
    }

    fn last_position(&self) -> Option<Point> {
        self.sent.iter().rev().find_map(|message| match message {
            Message::Motion(Motion::Absolute(point)) => Some(*point),
            _ => None,
        })
    }
}

fn moves(messages: &[Message]) -> usize {
    let is_move = |message: &&Message| matches!(message, Message::Motion(_));
    messages.iter().filter(is_move).count()
}

fn scrolls(messages: &[Message]) -> usize {
    let is_scroll = |message: &&Message| matches!(message, Message::Scroll(_));
    messages.iter().filter(is_scroll).count()
}

/// The controlled computer, framing its receiver's replies as the destination actor does.
struct Controlled {
    receiver: InputReceiver,
    injected: Injected,
    link: LinkIo,
    reader: LinkReader,
    reply_epoch: SessionEpoch,
    reply_sequence: u64,
    heartbeat_sequence: u64,
    received: Vec<Message>,
}

impl Controlled {
    fn reply(&mut self, message: Message) {
        let frame = if is_heartbeat(&message) {
            self.heartbeat_sequence += 1;
            Frame::new(
                self.receiver.control_epoch(),
                self.heartbeat_sequence,
                message,
            )
        } else {
            if self.receiver.epoch() != self.reply_epoch {
                self.reply_epoch = self.receiver.epoch();
                self.reply_sequence = 0;
            }
            self.reply_sequence += 1;
            Frame::new(self.reply_epoch, self.reply_sequence - 1, message)
        };
        assert_eq!(self.link.send_inbound(frame), Ok(()));
    }

    fn receive(&mut self, frame: &Frame, now: Duration) {
        self.received.push(frame.message.clone());
        let reply = self.receiver.receive(frame, now, &mut self.injected);
        let reply = reply.expect("the receiver accepts the frame");
        self.receiver.flush(&mut self.injected).expect("delivered");
        if let Some(message) = reply {
            self.reply(message);
        }
    }

    fn tick(&mut self, now: Duration) {
        let reply = self.receiver.tick(now, &mut self.injected);
        if let Some(message) = reply.expect("the receiver stays up") {
            self.reply(message);
        }
    }

    fn injected_since(&self, from: usize) -> &[DestinationAction] {
        &self.injected.0[from..]
    }
}

/// What a computer takes from the link.
#[derive(Clone, Copy)]
enum Reads {
    Everything,
    /// Starves the stream. This build sends no datagrams, so it reads nothing: a regression trap
    /// for heartbeats that would ride datagrams past the input they vouch for.
    DatagramsOnly,
    Nothing,
}

/// Both computers read everything: the link flows.
const FLOWING: (Reads, Reads) = (Reads::Everything, Reads::Everything);

async fn read(reader: &mut LinkReader, reads: Reads) -> Result<Frame, SessionFailure> {
    match reads {
        Reads::Everything => reader.next_frame().await,
        Reads::DatagramsOnly => next_datagram(reader).await,
        Reads::Nothing => std::future::pending().await,
    }
}

async fn next_datagram(reader: &mut LinkReader) -> Result<Frame, SessionFailure> {
    let bytes = reader
        .connection
        .read_datagram()
        .await
        .map_err(|_| SessionFailure::Wire)?;
    let frame = decode_datagram(&bytes).map_err(|_| SessionFailure::UnexpectedStream)?;
    reader.scopes.validate(&frame)?;
    Ok(frame)
}

struct Session {
    controller: Controller,
    controlled: Controlled,
    origin: Instant,
    _endpoints: [quinn::Endpoint; 2],
}

impl Session {
    async fn new() -> Self {
        let Pair {
            client,
            server,
            _endpoints,
        } = negotiated_pair().await;
        let (reader, link) = LinkIo::split(client, SessionProgress::default());
        let source = SourceController::new(
            topology(),
            DeviceId([1; 16]),
            DisplayId(1),
            epoch(),
            3,
            Duration::ZERO,
        )
        .unwrap();
        let controller = Controller {
            source,
            link,
            reader,
            route: (false, 0),
            sent: Vec::new(),
        };
        let (reader, link) = LinkIo::split(server, SessionProgress::default());
        let controlled = Controlled {
            receiver: InputReceiver::new(controlled_displays(), epoch(), Duration::ZERO),
            injected: Injected::default(),
            link,
            reader,
            reply_epoch: epoch(),
            reply_sequence: 0,
            heartbeat_sequence: 2,
            received: Vec::new(),
        };
        Self {
            controller,
            controlled,
            origin: Instant::now(),
            _endpoints,
        }
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    /// Runs both computers for `length`, each reading as its `Reads` says, and calls `script`
    /// with the time since the run began once per millisecond.
    async fn run(
        &mut self,
        length: Duration,
        (controller_reads, controlled_reads): (Reads, Reads),
        mut script: impl FnMut(&mut Controller, Duration, Duration),
    ) {
        enum Event {
            Tick,
            Controller(Result<Frame, SessionFailure>),
            Controlled(Result<Frame, SessionFailure>),
        }
        let started = self.now();
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        while self.now() < started + length {
            let event = tokio::select! {
                biased;
                _ = tick.tick() => Event::Tick,
                frame = read(&mut self.controller.reader, controller_reads) => Event::Controller(frame),
                frame = read(&mut self.controlled.reader, controlled_reads) => Event::Controlled(frame),
            };
            let now = self.now();
            match event {
                Event::Tick => {
                    script(&mut self.controller, now, now - started);
                    let outcome = self.controller.source.tick(now);
                    self.controller.apply(outcome, now);
                    self.controlled.tick(now);
                    assert_eq!(self.controller.link.check(), Ok(()));
                    assert_eq!(self.controlled.link.check(), Ok(()));
                }
                Event::Controller(frame) => {
                    let frame = frame.expect("the controlling computer's link stays up");
                    let outcome = self.controller.source.on_remote_frame(&frame, now);
                    self.controller.apply(outcome, now);
                }
                Event::Controlled(frame) => {
                    let frame = frame.expect("the controlled computer's link stays up");
                    self.controlled.receive(&frame, now);
                }
            }
        }
    }

    /// Lets the heartbeats settle, pushes the pointer onto the controlled display and presses
    /// `HELD_KEY` there.
    async fn cross_and_hold_a_key(&mut self) {
        self.run(Duration::from_millis(100), FLOWING, |_, _, _| {})
            .await;
        let crossing: [(u64, Step); 3] = [
            (0, |controller, now| {
                let at_the_edge = Point::new(99.0, 50.0);
                controller.capture(NormalizedInput::AbsoluteMotion(at_the_edge), now);
            }),
            (1, |controller, now| {
                let push = Point::new(PUSH_THROUGH_DISTANCE, 0.0);
                controller.capture(NormalizedInput::RelativeMotion(push), now);
            }),
            (100, |controller, now| controller.key(HELD_KEY, true, now)),
        ];
        let mut next = 0;
        self.run(
            Duration::from_millis(200),
            FLOWING,
            |controller, now, elapsed| run_due(&crossing, &mut next, controller, now, elapsed),
        )
        .await;
        assert_eq!(self.controller.source.mode(), SourceMode::Remote);
        let pressed = DestinationAction::Key {
            usage: HELD_KEY,
            pressed: true,
        };
        assert!(self.controlled.injected.0.contains(&pressed));
    }
}

type Step = fn(&mut Controller, Duration);

/// Fires each step whose millisecond the run has reached, however late the tick.
fn run_due(
    schedule: &[(u64, Step)],
    next: &mut usize,
    controller: &mut Controller,
    now: Duration,
    elapsed: Duration,
) {
    while let Some((at, step)) = schedule.get(*next)
        && elapsed >= Duration::from_millis(*at)
    {
        step(controller, now);
        *next += 1;
    }
}

/// Moves the pointer `per_ms` times a millisecond, catching up after a late tick, and runs
/// `schedule` on top.
fn wiggling(
    per_ms: u128,
    schedule: &[(u64, Step)],
) -> impl FnMut(&mut Controller, Duration, Duration) + '_ {
    let (mut next, mut steps) = (0, 0);
    move |controller, now, elapsed| {
        while steps < per_ms * (elapsed.as_millis() + 1) {
            controller.wiggle(steps, now);
            steps += 1;
        }
        run_due(schedule, &mut next, controller, now, elapsed);
    }
}

fn typed_during_the_stall() -> [(u64, Step); 4] {
    [
        (100, |controller, now| controller.key(TYPED_KEY, true, now)),
        (130, |controller, now| controller.key(TYPED_KEY, false, now)),
        (160, |controller, now| click(controller, true, now)),
        (190, |controller, now| click(controller, false, now)),
    ]
}

fn click(controller: &mut Controller, pressed: bool, at: Duration) {
    let button = MouseButton::Left;
    controller.capture(
        NormalizedInput::Button {
            button,
            pressed,
            at,
        },
        at,
    );
}

/// The input stream to the controlled computer starves: both sides hold, the controlling computer
/// brings input home, nothing typed during the stall is injected once the stream moves again, and
/// the key held before it is released there.
#[tokio::test]
async fn a_starved_input_stream_holds_both_sides_and_injects_nothing_typed_during_it() {
    timeout(Duration::from_secs(20), async {
        let mut session = Session::new().await;
        session.cross_and_hold_a_key().await;
        let before = session.controlled.injected.0.len();
        let stalled = session.now();

        let typed = typed_during_the_stall();
        let starved = (Reads::Everything, Reads::DatagramsOnly);
        session.run(LONG_STALL, starved, wiggling(1, &typed)).await;
        let controller_held = session.controller.source.held_since();
        let controlled_held = session.controlled.receiver.held_since();
        assert!(RETREAT_AFTER + RETREAT_SLACK < PEER_LIVENESS);
        assert!(
            controller_held.is_some_and(|at| at - stalled < RETREAT_AFTER + RETREAT_SLACK),
            "input did not come home on its own: {controller_held:?} after {stalled:?}"
        );
        assert!(
            controlled_held.is_some(),
            "the receiver never held, so late input would be injected"
        );
        assert!(
            session
                .controlled
                .injected_since(before)
                .contains(&DestinationAction::ReleaseAll),
            "the held key was not released"
        );

        session
            .run(Duration::from_secs(1), FLOWING, |_, _, _| {})
            .await;
        let now = session.now();
        session.controller.key(HELD_KEY, false, now);
        session
            .run(Duration::from_millis(200), FLOWING, |_, _, _| {})
            .await;

        let late = session.controlled.injected_since(before);
        assert!(
            late.iter().all(|action| action.is_release()),
            "input from the stall was injected: {late:?}"
        );
        assert_eq!(session.controller.source.mode(), SourceMode::Local);
        assert_eq!(session.controller.source.held_since(), None);
        assert_eq!(session.controlled.receiver.held_since(), None);
    })
    .await
    .expect("the starved-stream test overran");
}

/// The replies to the controlling computer starve while its input still flows: it brings input
/// home, its barrier releases the held key and nothing follows the barrier there, and both sides
/// resume once the replies move again.
#[tokio::test]
async fn starved_replies_bring_input_home_and_nothing_follows_the_barrier() {
    timeout(Duration::from_secs(20), async {
        let mut session = Session::new().await;
        session.cross_and_hold_a_key().await;
        let before = session.controlled.injected.0.len();
        let stalled = session.now();

        let typed_at_home: [(u64, Step); 2] = [
            (600, |controller, now| controller.key(TYPED_KEY, true, now)),
            (630, |controller, now| controller.key(TYPED_KEY, false, now)),
        ];
        let starved = (Reads::DatagramsOnly, Reads::Everything);
        session
            .run(LONG_STALL, starved, wiggling(1, &typed_at_home))
            .await;
        let controller_held = session.controller.source.held_since();
        assert!(
            controller_held.is_some_and(|at| at - stalled < RETREAT_AFTER + RETREAT_SLACK),
            "input did not come home on its own: {controller_held:?} after {stalled:?}"
        );
        assert_eq!(session.controller.source.mode(), SourceMode::Local);

        session
            .run(Duration::from_secs(1), FLOWING, |_, _, _| {})
            .await;
        let now = session.now();
        session.controller.key(HELD_KEY, false, now);
        session
            .run(Duration::from_millis(200), FLOWING, |_, _, _| {})
            .await;

        let late = session.controlled.injected_since(before);
        let barrier = late
            .iter()
            .position(|action| *action == DestinationAction::ReleaseAll);
        assert!(
            barrier.is_some_and(|at| late[at..].iter().all(|action| action.is_release())),
            "the held key was not released, or input followed the barrier: {late:?}"
        );
        assert_eq!(session.controller.source.mode(), SourceMode::Local);
        assert_eq!(session.controller.source.held_since(), None);
        assert_eq!(session.controlled.receiver.held_since(), None);
    })
    .await
    .expect("the starved-replies test overran");
}

/// A short stall of everything survives with no hold: the writer waits on credit past the old
/// deadline instead of ending the link, backed-up positions collapse, every scroll and key arrives
/// in order, and the pointer ends where the controlling computer last sent it.
#[tokio::test]
async fn a_short_full_stall_survives_without_a_hold() {
    timeout(Duration::from_secs(20), async {
        let mut session = Session::new().await;
        session.cross_and_hold_a_key().await;
        let before = session.controlled.injected.0.len();
        let sent_before = session.controller.sent.len();
        let received_before = session.controlled.received.len();

        let typed = typed_during_the_stall();
        let schedule: [(u64, Step); 3] = [
            (0, |controller, now| {
                (0..SCROLL_BURST).for_each(|_| controller.scroll(now));
            }),
            typed[0],
            typed[1],
        ];
        let mut wiggle = wiggling(1, &schedule);
        let mut longest_wait = Duration::ZERO;
        let stalled = (Reads::Everything, Reads::Nothing);
        session
            .run(SHORT_STALL, stalled, |controller, now, elapsed| {
                wiggle(controller, now, elapsed);
                if let Some(since) = controller.link.outbox.waiting_since() {
                    longest_wait = longest_wait.max(since.elapsed());
                }
            })
            .await;
        session
            .run(Duration::from_millis(300), FLOWING, |_, _, _| {})
            .await;

        assert!(
            longest_wait > WRITE_DEADLINE,
            "the writer never waited on credit: {longest_wait:?}"
        );
        let now = session.now();
        assert_eq!(session.controller.source.hold_stats(now).0, 0);
        assert_eq!(session.controlled.receiver.hold_stats(now).0, 0);
        assert_eq!(session.controller.source.mode(), SourceMode::Remote);
        let sent = &session.controller.sent[sent_before..];
        let received = &session.controlled.received[received_before..];
        assert_eq!(scrolls(sent), SCROLL_BURST);
        assert_eq!(scrolls(received), SCROLL_BURST);
        assert!(
            moves(received) < moves(sent),
            "no backed-up position collapsed: {} of {} arrived",
            moves(received),
            moves(sent)
        );
        let late = session.controlled.injected_since(before);
        let typed: Vec<_> = late
            .iter()
            .filter(|action| matches!(action, DestinationAction::Key { .. }))
            .collect();
        assert_eq!(
            typed,
            [
                &DestinationAction::Key {
                    usage: TYPED_KEY,
                    pressed: true
                },
                &DestinationAction::Key {
                    usage: TYPED_KEY,
                    pressed: false
                },
            ]
        );
        let last_move = late.iter().rev().find_map(|action| match action {
            DestinationAction::MoveTo(point) => Some(*point),
            _ => None,
        });
        assert_eq!(last_move, session.controller.last_position());
        assert!(RETREAT_AFTER > SHORT_STALL);
    })
    .await
    .expect("the short-stall test overran");
}
