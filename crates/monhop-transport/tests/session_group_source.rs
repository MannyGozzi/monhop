use std::{collections::VecDeque, time::Duration};

use monhop_core::{
    DeviceId, Display, DisplayId, Edge, EdgeLink, FloorPeer, FloorState, HidUsage, LogicalSize,
    Machine, ModifierState, MouseButton, NativeSize, NormalizedSpan, Platform, Point,
    PointerTarget, SharedFloor, TakeBackGate, Topology,
};
use monhop_protocol::{DisplayDescription, DisplayTopology, Frame, Message, SessionEpoch};
use monhop_transport::session_health::PEER_LIVENESS;
use monhop_transport::session_receiver::{
    DestinationAction, DestinationFailure, InputDestination, InputReceiver,
};
use monhop_transport::session_source::{
    Handover, NormalizedInput, PUSH_THROUGH_DISTANCE, RouteRequest, SourceController, SourceEffect,
    SourceFailure, SourceMode, SourceOutcome, TaggedInput,
};

const HOME: u8 = 1;
const PEER_X: u8 = 2;
const PEER_Y: u8 = 3;
const SHIFT: u16 = 0xe1;
const CONTROL: u16 = 0xe0;
const LETTER_A: u16 = 0x04;

fn device(machine: u8) -> DeviceId {
    DeviceId([machine; 16])
}

fn slot(machine: u8) -> FloorPeer {
    FloorPeer::slot(machine - 1).unwrap()
}

/// Each machine has one 100 px display whose id is the machine's.
fn display(machine: u8) -> Display {
    Display::new(
        DisplayId(u64::from(machine)),
        device(machine),
        format!("display-{machine}"),
        NativeSize::new(100, 100),
        LogicalSize::new(100.0, 100.0),
        Point::new(0.0, 0.0),
        1.0,
        None,
        true,
    )
}

fn link(from: u8, from_edge: Edge, to: u8, to_edge: Edge) -> EdgeLink {
    let full = NormalizedSpan::new(0.0, 1.0).unwrap();
    EdgeLink::new(
        DisplayId(u64::from(from)),
        from_edge,
        full,
        DisplayId(u64::from(to)),
        to_edge,
        full,
        1.0,
    )
    .unwrap()
}

/// This computer's display 1 has Y's display 3 on its left and X's display 2 on its right, and
/// X's display 2 goes on right onto Y's display 3.
fn group_topology() -> Topology {
    Topology::new(
        vec![
            Machine::new(device(HOME), Platform::Windows),
            Machine::new(device(PEER_X), Platform::MacOs),
            Machine::new(device(PEER_Y), Platform::Windows),
        ],
        vec![display(HOME), display(PEER_X), display(PEER_Y)],
        vec![
            link(HOME, Edge::Right, PEER_X, Edge::Left),
            link(PEER_X, Edge::Left, HOME, Edge::Right),
            link(PEER_X, Edge::Right, PEER_Y, Edge::Left),
            link(PEER_Y, Edge::Left, PEER_X, Edge::Right),
            link(HOME, Edge::Left, PEER_Y, Edge::Right),
            link(PEER_Y, Edge::Right, HOME, Edge::Left),
        ],
    )
    .unwrap()
}

fn ungrouped_controller(floor: &SharedFloor) -> SourceController {
    SourceController::new(
        group_topology(),
        device(HOME),
        DisplayId(1),
        SessionEpoch::new(3).unwrap(),
        3,
        Duration::ZERO,
    )
    .unwrap()
    .with_floor(floor.clone(), true)
}

fn controller(peer: u8, floor: &SharedFloor) -> SourceController {
    ungrouped_controller(floor)
        .with_peer(device(peer), slot(peer))
        .unwrap()
}

#[derive(Debug, PartialEq)]
enum Injected {
    Move(Point),
    Key(HidUsage, bool),
    Button(MouseButton, bool, u8),
    ReleaseAll,
}

#[derive(Default)]
struct Recording(Vec<Injected>);

impl InputDestination for Recording {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let injected = match action {
            DestinationAction::MoveTo(point) => Injected::Move(point),
            DestinationAction::Key { usage, pressed } => Injected::Key(usage, pressed),
            DestinationAction::Button {
                button,
                pressed,
                click_count,
            } => Injected::Button(button, pressed, click_count),
            DestinationAction::ReleaseAll => Injected::ReleaseAll,
            _ => return Ok(()),
        };
        self.0.push(injected);
        Ok(())
    }
}

/// The receiving half on one peer, answering exactly as its session would.
struct Peer {
    receiver: InputReceiver,
    injected: Recording,
    reply_epoch: SessionEpoch,
    reply_sequence: u64,
    heartbeat_sequence: u64,
}

impl Peer {
    fn new(machine: u8, accepts_control: bool) -> Self {
        let displays = DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(u64::from(machine)),
            name: format!("display-{machine}"),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::new(0.0, 0.0),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap();
        let epoch = SessionEpoch::new(3).unwrap();
        let receiver = InputReceiver::new(displays, epoch, Duration::ZERO).with_floor(
            TakeBackGate::new(SharedFloor::new()),
            false,
            accepts_control,
        );
        Self {
            receiver,
            injected: Recording::default(),
            reply_epoch: epoch,
            reply_sequence: 0,
            heartbeat_sequence: 3,
        }
    }

    fn respond(&mut self, frame: &Frame, now: Duration) -> Option<Frame> {
        let reply = self
            .receiver
            .receive(frame, now, &mut self.injected)
            .expect("the peer accepts the frame");
        self.receiver
            .flush(&mut self.injected)
            .expect("the peer delivers it");
        let message = reply?;
        if matches!(message, Message::Ping(_) | Message::Pong(_)) {
            let sequence = self.heartbeat_sequence;
            self.heartbeat_sequence += 1;
            return Some(Frame::new(self.receiver.control_epoch(), sequence, message));
        }
        let epoch = self.receiver.epoch();
        if epoch != self.reply_epoch {
            self.reply_epoch = epoch;
            self.reply_sequence = 0;
        }
        let sequence = self.reply_sequence;
        self.reply_sequence += 1;
        Some(Frame::new(epoch, sequence, message))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    X,
    Y,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::X => Self::Y,
            Self::Y => Self::X,
        }
    }
}

fn one_of(first: Option<Handover>, second: Option<Handover>) -> Option<Handover> {
    assert!(
        first.is_none() || second.is_none(),
        "one handover at a time"
    );
    first.or(second)
}

/// This computer's two controllers over one floor and one native capture route, as the hub
/// drives them: every captured record reaches both, a route barrier reaches its issuer while
/// the other adopts it, and frames wait in each link's outbox until delivered.
struct Group {
    floor: SharedFloor,
    sources: [SourceController; 2],
    peers: [Peer; 2],
    outbox: [VecDeque<Frame>; 2],
    route: (bool, u64),
    floors: Vec<(FloorState, FloorPeer)>,
    restores: Vec<(DisplayId, Point)>,
    now: Duration,
}

impl Group {
    fn new() -> Self {
        Self::with_peers(Peer::new(PEER_X, true), Peer::new(PEER_Y, true))
    }

    fn with_peers(x: Peer, y: Peer) -> Self {
        let floor = SharedFloor::new();
        let sources = [controller(PEER_X, &floor), controller(PEER_Y, &floor)];
        let mut group = Self {
            floor,
            sources,
            peers: [x, y],
            outbox: [VecDeque::new(), VecDeque::new()],
            route: (false, 0),
            floors: Vec::new(),
            restores: Vec::new(),
            now: Duration::ZERO,
        };
        group.note_floor();
        group
    }

    /// Both peers are live, so each controller may hand over to the other.
    fn reach(&mut self) {
        self.source(Side::X).set_reachable(&[device(PEER_Y)]);
        self.source(Side::Y).set_reachable(&[device(PEER_X)]);
    }

    fn source(&mut self, side: Side) -> &mut SourceController {
        &mut self.sources[side as usize]
    }

    fn injected(&self, side: Side) -> &[Injected] {
        &self.peers[side as usize].injected.0
    }

    fn queued(&self, side: Side) -> Vec<Message> {
        self.outbox[side as usize]
            .iter()
            .map(|frame| frame.message.clone())
            .collect()
    }

    fn floor_owner(&self) -> (FloorState, FloorPeer) {
        let floor = self.floor.snapshot();
        (floor.state, floor.peer)
    }

    fn note_floor(&mut self) {
        let seen = self.floor_owner();
        if self.floors.last() != Some(&seen) {
            self.floors.push(seen);
        }
    }

    fn tag(&self, event: NormalizedInput) -> TaggedInput {
        TaggedInput {
            event,
            routing_revision: self.route.1,
            remote: self.route.0,
            floor_generation: self.floor.snapshot().generation,
        }
    }

    fn apply(&mut self, side: Side, outcome: SourceOutcome) -> Option<Handover> {
        assert_eq!(outcome.failure, None);
        self.note_floor();
        let mut handed = outcome.handover;
        for effect in outcome.effects.iter().cloned() {
            let routed = match effect {
                SourceEffect::RemoteFrame(frame) => {
                    self.outbox[side as usize].push_back(frame);
                    None
                }
                SourceEffect::ActivateRemote { request } => self.reroute(side, request, true),
                SourceEffect::RestoreLocalAt {
                    request,
                    display,
                    position,
                } => {
                    self.restores.push((display, position));
                    self.reroute(side, request, false)
                }
            };
            handed = one_of(handed, routed);
        }
        handed
    }

    fn reroute(&mut self, side: Side, request: RouteRequest, remote: bool) -> Option<Handover> {
        let ticket = self.route.1 + 1;
        let now = self.now;
        let bound = self.source(side).bind_capture_route(request, ticket, now);
        assert_eq!(bound.failure, None);
        assert!(bound.effects.is_empty());
        self.route = (remote, ticket);
        let barrier = self.tag(NormalizedInput::RouteChanged {
            remote,
            revision: ticket,
        });
        let crossed = self.source(side).on_captured(barrier, now);
        let handed = self.apply(side, crossed);
        let adopted = self
            .source(side.other())
            .observe_foreign_route(remote, ticket, now);
        one_of(handed, self.apply(side.other(), adopted))
    }

    fn capture(&mut self, event: NormalizedInput) -> Option<Handover> {
        let record = self.tag(event);
        let now = self.now;
        let mut handed = None;
        for side in [Side::X, Side::Y] {
            let outcome = self.source(side).on_captured(record, now);
            handed = one_of(handed, self.apply(side, outcome));
        }
        handed
    }

    fn deliver(&mut self, side: Side) -> Option<Handover> {
        let mut handed = None;
        while let Some(frame) = self.outbox[side as usize].pop_front() {
            let now = self.now;
            if let Some(reply) = self.peers[side as usize].respond(&frame, now) {
                let outcome = self.source(side).on_remote_frame(&reply, now);
                handed = one_of(handed, self.apply(side, outcome));
            }
        }
        handed
    }

    fn accept(&mut self, handover: Handover, side: Side) {
        let now = self.now;
        let accepted = self.source(side).accept_handover(handover, now);
        assert!(accepted.handover.is_none(), "the handover is taken");
        assert!(self.apply(side, accepted).is_none());
    }
}

fn moved(dx: f64) -> NormalizedInput {
    NormalizedInput::RelativeMotion(Point::new(dx, 0.0))
}

/// One record carrying the full push along `direction`, 1 right or -1 left.
fn push(direction: f64) -> NormalizedInput {
    moved(direction * PUSH_THROUGH_DISTANCE)
}

fn rest_at(x: f64) -> NormalizedInput {
    NormalizedInput::AbsoluteMotion(Point::new(x, 50.0))
}

fn key(usage: u16, pressed: bool, modifiers: u8) -> NormalizedInput {
    NormalizedInput::Key {
        usage: HidUsage(usage),
        pressed,
        repeat: false,
        modifiers: ModifierState(modifiers),
    }
}

fn left_button(pressed: bool) -> NormalizedInput {
    NormalizedInput::Button {
        button: MouseButton::Left,
        pressed,
        at: Duration::ZERO,
    }
}

fn activation(display: u8, x: f64) -> Message {
    Message::ActivateDisplayAt {
        display_id: DisplayId(u64::from(display)),
        position: Point::new(x, 50.0),
    }
}

/// Where a trip that left display 1 through its right edge comes home to.
fn trip_start() -> (PointerTarget, Point) {
    (
        PointerTarget::new(device(HOME), DisplayId(1)),
        Point::new(99.0, 50.0),
    )
}

/// Pushes through display 1's right edge and lands on X's display 2 at x 1.
fn cross_to_x(group: &mut Group) {
    assert!(group.capture(rest_at(99.0)).is_none());
    assert!(group.capture(push(1.0)).is_none());
    assert_eq!(group.queued(Side::X), [activation(PEER_X, 1.0)]);
    assert!(group.deliver(Side::X).is_none());
    assert_eq!(group.source(Side::X).mode(), SourceMode::Remote);
    assert_eq!(group.route, (true, 1));
}

/// From X's entry, moves onto display 2's right edge, the seam with Y, and pushes through it.
fn push_on_from_x(group: &mut Group) -> Option<Handover> {
    assert!(group.capture(moved(98.0)).is_none());
    group.capture(push(1.0))
}

/// Crosses to X and on to Y's seam, and yields the handover X's release acknowledgement makes.
fn handed_over_by_x(group: &mut Group) -> Handover {
    group.reach();
    cross_to_x(group);
    assert!(push_on_from_x(group).is_none());
    group
        .deliver(Side::X)
        .expect("the release acknowledgement hands the pointer over")
}

#[test]
fn a_local_crossing_onto_another_peers_display_is_left_to_that_peers_controller() {
    for (rest, direction, crossing, other, display, entry) in [
        (99.0, 1.0, Side::X, Side::Y, PEER_X, 1.0),
        (0.0, -1.0, Side::Y, Side::X, PEER_Y, 99.0),
    ] {
        let mut group = Group::new();
        assert!(group.capture(rest_at(rest)).is_none());
        assert!(group.capture(push(direction)).is_none());
        assert_eq!(group.queued(crossing), [activation(display, entry)]);
        assert!(group.queued(other).is_empty());
        assert_eq!(group.source(other).mode(), SourceMode::Local);
        assert_eq!(group.floor_owner(), (FloorState::Requesting, slot(display)));
    }

    // A group controller whose peer was never named crosses no seam at all.
    let floor = SharedFloor::new();
    let mut unnamed = ungrouped_controller(&floor);
    for (rest, direction) in [(99.0, 1.0), (0.0, -1.0)] {
        for event in [rest_at(rest), push(direction)] {
            let record = TaggedInput {
                event,
                routing_revision: 0,
                remote: false,
                floor_generation: floor.snapshot().generation,
            };
            let outcome = unnamed.on_captured(record, Duration::ZERO);
            assert_eq!(outcome.failure, None);
            assert!(outcome.effects.is_empty());
        }
    }
    assert_eq!(unnamed.mode(), SourceMode::Local);
    assert_eq!(floor.snapshot().state, FloorState::Free);
}

#[test]
fn a_remote_crossing_onto_a_live_third_peer_hands_over_after_release_ack() {
    let mut group = Group::new();
    group.reach();
    cross_to_x(&mut group);
    assert!(push_on_from_x(&mut group).is_none());
    assert_eq!(group.queued(Side::X).last(), Some(&Message::ReleaseAll));
    assert_eq!(
        group.source(Side::X).mode(),
        SourceMode::AwaitRemoteReleaseAcknowledgement
    );
    assert_eq!(group.floor_owner(), (FloorState::Returning, slot(PEER_X)));

    let handover = group
        .deliver(Side::X)
        .expect("the release acknowledgement hands the pointer over");
    assert_eq!(
        handover.to(),
        PointerTarget::new(device(PEER_Y), DisplayId(3))
    );
    assert_eq!(handover.return_at(), trip_start());
    assert_eq!(handover.claim(), group.floor.snapshot());
    assert_eq!(handover.route_revision(), 1);
    // X released everything; input stays captured and the floor stays held for the next peer.
    assert_eq!(group.injected(Side::X).last(), Some(&Injected::ReleaseAll));
    assert!(group.restores.is_empty());
    assert_eq!(group.route, (true, 1));
    assert_eq!(group.floor_owner(), (FloorState::Returning, slot(PEER_X)));
    assert_eq!(group.source(Side::X).mode(), SourceMode::Local);
    assert_eq!(group.source(Side::X).capture_route(), (true, 1));
}

#[test]
fn an_unreachable_third_peer_seam_is_a_wall() {
    let mut group = Group::new();
    cross_to_x(&mut group);
    assert!(push_on_from_x(&mut group).is_none());
    assert!(!group.queued(Side::X).contains(&Message::ReleaseAll));
    assert_eq!(group.source(Side::X).mode(), SourceMode::Remote);
    assert_eq!(group.floor_owner(), (FloorState::Sending, slot(PEER_X)));

    // The same push goes through once Y's controller is live.
    group.source(Side::X).set_reachable(&[device(PEER_Y)]);
    assert!(group.capture(push(1.0)).is_none());
    assert_eq!(group.queued(Side::X).last(), Some(&Message::ReleaseAll));
    assert_eq!(
        group.source(Side::X).mode(),
        SourceMode::AwaitRemoteReleaseAcknowledgement
    );
}

#[test]
fn accept_handover_transfers_modifiers_and_buttons_only() {
    let mut group = Group::new();
    group.reach();
    cross_to_x(&mut group);
    for event in [
        key(SHIFT, true, ModifierState::LEFT_SHIFT),
        key(LETTER_A, true, ModifierState::LEFT_SHIFT),
        left_button(true),
    ] {
        assert!(group.capture(event).is_none());
    }
    assert!(push_on_from_x(&mut group).is_none());
    let handover = group.deliver(Side::X).expect("a handover");
    assert_eq!(group.injected(Side::X).last(), Some(&Injected::ReleaseAll));

    group.accept(handover, Side::Y);
    assert_eq!(group.queued(Side::Y), [activation(PEER_Y, 1.0)]);
    assert_eq!(group.floor_owner(), (FloorState::Sending, slot(PEER_Y)));
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(group.source(Side::Y).mode(), SourceMode::Remote);
    assert_eq!(
        group.injected(Side::Y),
        [
            Injected::Move(Point::new(1.0, 50.0)),
            Injected::Key(HidUsage(SHIFT), true),
            Injected::Button(MouseButton::Left, true, 1),
        ]
    );

    // The ordinary key's release is dropped; the modifier's and the button's reach Y.
    for event in [
        key(LETTER_A, false, ModifierState::LEFT_SHIFT),
        key(SHIFT, false, 0),
        left_button(false),
    ] {
        assert!(group.capture(event).is_none());
    }
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(
        group.injected(Side::Y)[3..],
        [
            Injected::Key(HidUsage(SHIFT), false),
            Injected::Button(MouseButton::Left, false, 1),
        ]
    );
}

#[test]
fn a_non_owner_adopts_foreign_barriers_and_never_restores() {
    let mut group = Group::new();
    cross_to_x(&mut group);
    assert_eq!(group.source(Side::Y).capture_route(), (true, 1));
    // Keys reach both ledgers under X's route without a mismatch.
    for event in [
        key(CONTROL, true, ModifierState::LEFT_CONTROL),
        key(CONTROL, false, 0),
    ] {
        assert!(group.capture(event).is_none());
    }
    let now = group.now;
    let home = group.source(Side::X).request_local(now);
    assert!(group.apply(Side::X, home).is_none());
    assert!(group.deliver(Side::X).is_none());
    assert_eq!(group.restores, [(DisplayId(1), trip_start().1)]);
    assert_eq!(group.source(Side::Y).capture_route(), (false, 2));
    assert_eq!(group.floor.snapshot().state, FloorState::Free);
    let stale = group.source(Side::Y).observe_foreign_route(true, 2, now);
    assert_eq!(stale.failure, Some(SourceFailure::UnexpectedRouteBarrier));

    // Past the liveness deadline both links hold; only the route's owner restores local input.
    let restores = |outcome: &SourceOutcome| {
        outcome
            .effects
            .iter()
            .filter(|effect| matches!(effect, SourceEffect::RestoreLocalAt { .. }))
            .count()
    };
    let mut group = Group::new();
    cross_to_x(&mut group);
    let owner = group.source(Side::X).tick(PEER_LIVENESS);
    assert_eq!(owner.failure, None);
    assert_eq!(restores(&owner), 1);
    let other = group.source(Side::Y).tick(PEER_LIVENESS);
    assert_eq!(other.failure, None);
    assert!(group.source(Side::Y).held_since().is_some());
    assert_eq!(restores(&other), 0);
    let ended = group.source(Side::Y).abandon(PEER_LIVENESS);
    assert_eq!(ended.failure, Some(SourceFailure::LinkEnded));
    assert!(ended.effects.is_empty());

    // An owner whose link ends yields the restore for its caller to complete.
    let mut group = Group::new();
    cross_to_x(&mut group);
    let ended = group.source(Side::X).abandon(now);
    assert_eq!(ended.failure, Some(SourceFailure::LinkEnded));
    assert!(ended.effects.iter().any(|effect| matches!(
        effect,
        SourceEffect::RestoreLocalAt { display, position, .. }
            if (*display, *position) == (DisplayId(1), trip_start().1)
    )));
}

#[test]
fn a_declined_handover_goes_home() {
    let mut group = Group::with_peers(Peer::new(PEER_X, true), Peer::new(PEER_Y, false));
    let handover = handed_over_by_x(&mut group);
    group.accept(handover, Side::Y);
    // Y declines; its controller releases, is acknowledged, and restores where the trip began.
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(group.restores, [(DisplayId(1), trip_start().1)]);
    assert_eq!(group.source(Side::Y).mode(), SourceMode::Local);
    assert_eq!(group.source(Side::X).capture_route(), (false, 2));
    assert_eq!(
        group.floors,
        [
            (FloorState::Free, FloorPeer::NONE),
            (FloorState::Requesting, slot(PEER_X)),
            (FloorState::Sending, slot(PEER_X)),
            (FloorState::Returning, slot(PEER_X)),
            (FloorState::Sending, slot(PEER_Y)),
            (FloorState::Returning, slot(PEER_Y)),
            (FloorState::Free, FloorPeer::NONE),
        ]
    );
}

#[test]
fn a_handover_passes_the_floor_from_one_peer_to_the_next_without_ever_freeing_it() {
    let mut group = Group::new();
    group.reach();
    cross_to_x(&mut group);
    assert!(
        group
            .capture(key(SHIFT, true, ModifierState::LEFT_SHIFT))
            .is_none()
    );
    assert!(push_on_from_x(&mut group).is_none());
    // Motion while X acknowledges the release, then while Y acknowledges its activation,
    // carries on from Y's entry at x 1; the button pressed between lands where the hand was.
    assert!(group.capture(moved(5.0)).is_none());
    assert!(group.capture(left_button(true)).is_none());
    let handover = group.deliver(Side::X).expect("a handover");
    group.accept(handover, Side::Y);
    assert!(group.capture(moved(3.0)).is_none());
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(group.source(Side::Y).mode(), SourceMode::Remote);
    assert_eq!(
        group.injected(Side::Y),
        [
            Injected::Move(Point::new(1.0, 50.0)),
            Injected::Move(Point::new(6.0, 50.0)),
            Injected::Key(HidUsage(SHIFT), true),
            Injected::Button(MouseButton::Left, true, 1),
            Injected::Move(Point::new(9.0, 50.0)),
        ]
    );
    // Suppression never lapsed: the route stayed remote and the floor never came free.
    assert_eq!(
        group.floors,
        [
            (FloorState::Free, FloorPeer::NONE),
            (FloorState::Requesting, slot(PEER_X)),
            (FloorState::Sending, slot(PEER_X)),
            (FloorState::Returning, slot(PEER_X)),
            (FloorState::Sending, slot(PEER_Y)),
        ]
    );
    assert_eq!(group.route, (true, 1));
    assert!(group.restores.is_empty());

    assert!(group.capture(moved(1.0)).is_none());
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(
        group.injected(Side::Y).last(),
        Some(&Injected::Move(Point::new(10.0, 50.0)))
    );

    // Coming home from Y is the one local restore, where the trip began.
    let now = group.now;
    let home = group.source(Side::Y).request_local(now);
    assert!(group.apply(Side::Y, home).is_none());
    assert!(group.deliver(Side::Y).is_none());
    assert_eq!(group.restores, [(DisplayId(1), trip_start().1)]);
    assert_eq!(group.floor_owner(), (FloorState::Free, FloorPeer::NONE));
    assert_eq!(group.injected(Side::Y).last(), Some(&Injected::ReleaseAll));
    assert_eq!(group.source(Side::X).capture_route(), (false, 2));
    assert_eq!(group.source(Side::X).mode(), SourceMode::Local);
    assert_eq!(group.source(Side::Y).mode(), SourceMode::Local);
}
