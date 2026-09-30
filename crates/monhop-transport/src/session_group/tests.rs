//! Hub core routing scenarios. Each computer is a hub over a fake native capture and a
//! destination actor emulated synchronously on a real receiver set; one queue carries every
//! frame between them, on a clock the tests advance by hand.

use super::*;
use crate::{
    session_receiver::{DestinationAction, DestinationFailure, InputDestination, InputReceiver},
    session_receiver_set::{ReceiverSet, SlotFailure, SlotResult},
    session_source::PUSH_THROUGH_DISTANCE,
};
use monhop_core::{
    Display, Edge, EdgeLink, LogicalSize, Machine, ModifierState, NativeSize, NormalizedSpan,
    Platform,
    capture::{CaptureEvent, CaptureStop, StopReason, capture_channel},
    capture_physical::PhysicalCapture,
};
use monhop_protocol::{DeclineReason, DisplayDescription};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub(super) const LETTER_A: u16 = 0x04;
const LETTER_B: u16 = 0x05;
pub(super) const SHIFT: u16 = 0xe1;

fn device(id: u8) -> DeviceId {
    DeviceId([id; 16])
}

fn display(id: u8) -> DisplayId {
    DisplayId(u64::from(id))
}

/// One 100 px display per computer, whose id is the computer's, and a link each way per seam.
fn topology(computers: u8, seams: &[(u8, Edge, u8, Edge)]) -> Topology {
    let full = NormalizedSpan::new(0.0, 1.0).unwrap();
    let link = |from, from_edge, to, to_edge| {
        EdgeLink::new(
            display(from),
            from_edge,
            full,
            display(to),
            to_edge,
            full,
            1.0,
        )
        .unwrap()
    };
    Topology::new(
        (1..=computers)
            .map(|id| Machine::new(device(id), Platform::MacOs))
            .collect(),
        (1..=computers)
            .map(|id| {
                Display::new(
                    display(id),
                    device(id),
                    format!("display-{id}"),
                    NativeSize::new(100, 100),
                    LogicalSize::new(100.0, 100.0),
                    Point::new(0.0, 0.0),
                    1.0,
                    None,
                    true,
                )
            })
            .collect(),
        seams
            .iter()
            .flat_map(|&(from, from_edge, to, to_edge)| {
                [
                    link(from, from_edge, to, to_edge),
                    link(to, to_edge, from, from_edge),
                ]
            })
            .collect(),
    )
    .unwrap()
}

/// Two computers side by side, as a pairwise session sees them.
pub(super) fn pair() -> Topology {
    topology(2, &[(1, Edge::Right, 2, Edge::Left)])
}

/// Each computer's right edge leads to the next one's left edge, and the last back to the first.
pub(super) fn ring(computers: u8) -> Topology {
    let seams: Vec<_> = (1..=computers)
        .map(|id| (id, Edge::Right, id % computers + 1, Edge::Left))
        .collect();
    topology(computers, &seams)
}

fn announced(id: u8) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: display(id),
        name: format!("display-{id}"),
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

#[derive(Clone, Default)]
pub(super) struct Clock(Arc<AtomicU64>);

impl Clock {
    pub(super) fn now(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::SeqCst))
    }

    pub(super) fn advance(&self, by: Duration) {
        let micros = u64::try_from(by.as_micros()).unwrap();
        self.0.fetch_add(micros, Ordering::SeqCst);
    }

    fn session(&self) -> SessionClock {
        let micros = self.0.clone();
        SessionClock::with_test_reader(move || Duration::from_micros(micros.load(Ordering::SeqCst)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Command {
    Activate,
    Restore(Point),
}

/// Native capture that grants every route change at once and queues its barrier behind the
/// records already captured.
pub(super) struct FakeCapture {
    floor: SharedFloor,
    route: (bool, u64),
    pub(super) queue: VecDeque<CapturedEvent>,
    /// Every route command, with the floor it met.
    pub(super) commands: Vec<(Command, FloorSnapshot)>,
    renewals: u32,
    /// The OS pointer, which local absolute motion and restores move.
    pub(super) pointer: Point,
    /// Presses held since capture started; suppression is refused until none are.
    pub(super) blocking: Vec<HeldInput>,
}

impl FakeCapture {
    fn new(floor: SharedFloor) -> Self {
        Self {
            floor,
            route: (false, 0),
            queue: VecDeque::new(),
            commands: Vec::new(),
            renewals: 0,
            pointer: Point::new(50.0, 50.0),
            blocking: Vec::new(),
        }
    }

    fn capture(&mut self, event: CaptureEvent) {
        if let CaptureEvent::LogicalAbsoluteMotion { x, y } = event
            && !self.route.0
        {
            self.pointer = Point::new(x, y);
        }
        self.queue.push_back(CapturedEvent {
            event,
            routing_revision: self.route.1,
            remote: self.route.0,
            floor_generation: self.floor.snapshot().generation,
        });
    }

    fn reroute(&mut self, command: Command, remote: bool) -> Result<u64, CaptureRefusal> {
        self.commands.push((command, self.floor.snapshot()));
        self.route = (remote, self.route.1 + 1);
        let revision = self.route.1;
        self.capture(CaptureEvent::RouteChanged { remote, revision });
        Ok(revision)
    }

    pub(super) fn commands(&self) -> Vec<Command> {
        self.commands.iter().map(|(command, _)| *command).collect()
    }
}

impl CaptureControl for FakeCapture {
    fn activate_remote(&mut self, _: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        assert!(!ttl.is_zero() && ttl <= MAX_SUPPRESSION_TTL);
        self.reroute(Command::Activate, true)
    }
    fn restore_local_at(&mut self, _: u64, position: Point) -> Result<u64, CaptureRefusal> {
        self.pointer = position;
        self.reroute(Command::Restore(position), false)
    }
    fn renew_suppression(&mut self, _: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        assert!(self.route.0 && !ttl.is_zero() && ttl <= MAX_SUPPRESSION_TTL);
        self.renewals += 1;
        Ok(self.route.1)
    }
    fn completed_control_revision(&self) -> Option<u64> {
        Some(self.route.1)
    }
    fn is_ready_for_suppression(&self) -> bool {
        self.blocking.is_empty()
    }
    fn blocking_presses(&self) -> Vec<HeldInput> {
        self.blocking.clone()
    }
    fn stop_reason(&self) -> Option<StopReason> {
        None
    }
    fn request_stop(&self) {}
}

/// Native injection on one computer: what reached it and what it holds down now.
pub(super) struct Injector {
    gate: TakeBackGate,
    pub(super) actions: Vec<DestinationAction>,
    pub(super) keys: [bool; 256],
    refuse_release: bool,
}

impl InputDestination for Injector {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        if action == DestinationAction::ReleaseAll && self.refuse_release {
            return Err(DestinationFailure);
        }
        let down = matches!(action, DestinationAction::Key { usage, pressed: true }
            if !self.keys[usize::from(usage.0)]);
        if down {
            self.gate.note_injected_press();
        }
        if !action.is_release() && !self.gate.admits_injection() {
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
            DestinationAction::ReleaseAll => {
                self.keys.fill(false);
                self.gate.note_injected_released();
            }
            _ => {}
        }
        Ok(())
    }
}

/// Stands in for a destination the actor already released directly at shutdown.
struct Released;

impl InputDestination for Released {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        if action.is_release() {
            Ok(())
        } else {
            Err(DestinationFailure)
        }
    }
}

/// The hub destination actor's loop, run synchronously: the take-back before input, a tick per
/// live slot, and a leaving slot retried until its release lands.
pub(super) struct FakeActor {
    pub(super) set: ReceiverSet,
    pub(super) injector: Injector,
    displays: DisplayTopology,
    leaving: Vec<FloorPeer>,
    events: VecDeque<(FloorPeer, SlotEvent)>,
    /// Every message submitted, by slot, in order.
    pub(super) received: Vec<(FloorPeer, Message)>,
    /// Every reply, by slot, in order.
    replies: Vec<(FloorPeer, Message)>,
}

impl FakeActor {
    fn new(gate: TakeBackGate, displays: DisplayTopology) -> Self {
        Self {
            set: ReceiverSet::new(gate.clone()),
            injector: Injector {
                gate,
                actions: Vec::new(),
                keys: [false; 256],
                refuse_release: false,
            },
            displays,
            leaving: Vec::new(),
            events: VecDeque::new(),
            received: Vec::new(),
            replies: Vec::new(),
        }
    }

    fn join(&mut self, join: DestinationJoin, now: Duration) {
        let DestinationJoin {
            slot,
            control,
            local_lower,
            enabled,
        } = join;
        let next_heartbeat_out = control.next_heartbeat_out;
        let joined = InputReceiver::after_startup(self.displays.clone(), control, now)
            .map_err(SlotFailure::Receiver)
            .and_then(|receiver| {
                self.set
                    .add(slot, receiver, local_lower, enabled, next_heartbeat_out)
            });
        if let Err(failure) = joined {
            self.events.push_back((slot, SlotEvent::Failed(failure)));
            self.events.push_back((slot, SlotEvent::Left));
        }
    }

    fn submit(&mut self, slot: FloorPeer, frame: Frame, now: Duration) {
        self.take_back();
        if self.leaving.contains(&slot) {
            return;
        }
        self.received.push((slot, frame.message.clone()));
        let received = self.set.receive(slot, &frame, now, &mut self.injector);
        let stopped = received.is_err();
        self.respond(slot, received);
        if !stopped && let Err(failure) = self.set.flush(slot, &mut self.injector) {
            self.fail(slot, failure);
        }
    }

    fn leave(&mut self, slot: FloorPeer) {
        if !self.leaving.contains(&slot) {
            self.leaving.push(slot);
        }
    }

    fn pass(&mut self, now: Duration, tick: bool) {
        self.take_back();
        for slot in self.set.occupied() {
            if self.leaving.contains(&slot) {
                if self.set.remove(slot, &mut self.injector) {
                    self.leaving.retain(|leaving| *leaving != slot);
                    self.events.push_back((slot, SlotEvent::Left));
                }
            } else if tick {
                let ticked = self.set.tick(slot, now, &mut self.injector);
                self.respond(slot, ticked);
            }
        }
    }

    fn take_back(&mut self) {
        if let Some((slot, result)) = self.set.take_back(&mut self.injector)
            && !self.leaving.contains(&slot)
        {
            self.respond(slot, result);
        }
    }

    fn respond(&mut self, slot: FloorPeer, result: SlotResult) {
        match result {
            Ok(Some(frame)) => {
                self.replies.push((slot, frame.message.clone()));
                self.events.push_back((slot, SlotEvent::Response(frame)));
            }
            Ok(None) => {}
            Err(failure) => self.fail(slot, failure),
        }
    }

    fn fail(&mut self, slot: FloorPeer, failure: SlotFailure) {
        if !self.leaving.contains(&slot) {
            self.events.push_back((slot, SlotEvent::Failed(failure)));
            self.leaving.push(slot);
        }
    }

    /// Native input is released once, directly, then every receiver stops as bookkeeping.
    fn shut_down(&mut self) {
        self.injector.gate.open_injection(0);
        self.injector
            .apply(DestinationAction::ReleaseAll)
            .expect("the shutdown release lands");
        for slot in self.set.occupied() {
            self.set.remove(slot, &mut Released);
        }
        self.leaving.clear();
        self.events.clear();
    }
}

pub(super) struct Computer {
    id: u8,
    /// The other computers in slot order.
    members: Vec<u8>,
    pub(super) core: HubCore,
    pub(super) capture: FakeCapture,
    pub(super) actor: FakeActor,
    pub(super) native: bool,
    pub(super) native_stops: u32,
    /// Each link's end as it closed, and each slot freed after it, by peer.
    pub(super) closed: Vec<(u8, LinkEnd)>,
    pub(super) ended: Vec<u8>,
    pub(super) started: Vec<u8>,
    pub(super) floors: Vec<(FloorState, Option<u8>)>,
}

impl Computer {
    pub(super) fn slot(&self, peer: u8) -> FloorPeer {
        let index = self.members.iter().position(|m| *m == peer).unwrap();
        FloorPeer::slot(u8::try_from(index + 1).unwrap()).unwrap()
    }

    fn peer(&self, slot: FloorPeer) -> u8 {
        self.members[usize::from(slot.get()) - 1]
    }

    pub(super) fn source(&self, peer: u8) -> &SourceController {
        self.core.source(self.slot(peer)).expect("a live link")
    }

    pub(super) fn floor(&self) -> (FloorState, Option<u8>) {
        let floor = self.core.floor();
        let peer = (floor.peer != FloorPeer::NONE).then(|| self.peer(floor.peer));
        (floor.state, peer)
    }

    fn note_floor(&mut self) {
        let floor = self.floor();
        if self.floors.last() != Some(&floor) {
            self.floors.push(floor);
        }
    }

    pub(super) fn keys(&self) -> Vec<(u16, bool)> {
        self.actor
            .injector
            .actions
            .iter()
            .filter_map(|action| match action {
                DestinationAction::Key { usage, pressed } => Some((usage.0, *pressed)),
                _ => None,
            })
            .collect()
    }

    pub(super) fn released(&self) -> usize {
        self.actor
            .injector
            .actions
            .iter()
            .filter(|action| **action == DestinationAction::ReleaseAll)
            .count()
    }
}

pub(super) struct Mesh {
    pub(super) clock: Clock,
    computers: Vec<Computer>,
    /// Frames in flight, delivered in order: (to, from, frame).
    pub(super) wire: VecDeque<(u8, u8, Frame)>,
    /// Directions whose frames are lost: (from, to).
    pub(super) cut: Vec<(u8, u8)>,
    /// A computer that has yet to see a link its peer closed: (computer, peer).
    closing: VecDeque<(u8, u8)>,
}

impl Mesh {
    /// Every pair of `group`'s computers linked, and every link started.
    pub(super) fn start(group: Topology) -> Self {
        let count = u8::try_from(group.displays().count()).unwrap();
        let clock = Clock::default();
        let computers = (1..=count)
            .map(|id| {
                let members: Vec<u8> = (1..=count).filter(|member| *member != id).collect();
                let config = HubConfig {
                    local: device(id),
                    group: group.clone(),
                    members: members.iter().map(|member| device(*member)).collect(),
                    generation: 1,
                    double_click: Duration::from_millis(500),
                };
                let core = HubCore::new(config, clock.session()).unwrap();
                let capture = FakeCapture::new(core.gate().floor().clone());
                let actor = FakeActor::new(core.gate().clone(), announced(id));
                let mut computer = Computer {
                    id,
                    members,
                    core,
                    capture,
                    actor,
                    native: false,
                    native_stops: 0,
                    closed: Vec::new(),
                    ended: Vec::new(),
                    started: Vec::new(),
                    floors: Vec::new(),
                };
                computer.note_floor();
                computer
            })
            .collect();
        let mut mesh = Self {
            clock,
            computers,
            wire: VecDeque::new(),
            cut: Vec::new(),
            closing: VecDeque::new(),
        };
        for a in 1..=count {
            for b in a + 1..=count {
                mesh.connect(a, b);
            }
        }
        for _ in 0..200 {
            mesh.tick();
            if mesh
                .computers
                .iter()
                .all(|c| c.members.iter().all(|peer| c.core.is_ready(c.slot(*peer))))
            {
                for computer in &mut mesh.computers {
                    computer.started.sort_unstable();
                    assert_eq!(computer.started, computer.members);
                    assert!(computer.native);
                }
                return mesh;
            }
        }
        panic!("the links never started");
    }

    pub(super) fn connect(&mut self, a: u8, b: u8) {
        for (local, peer) in [(a, b), (b, a)] {
            let computer = self.computer(local);
            let slot = computer
                .core
                .add_link(LinkSetup {
                    peer: device(peer),
                    outbound_enabled: true,
                    inbound_enabled: true,
                    epoch: SessionEpoch::new(10).unwrap(),
                    next_sequence: 3,
                    local_displays: announced(local),
                    peer_displays: announced(peer),
                })
                .unwrap();
            assert_eq!(slot, computer.slot(peer));
        }
    }

    pub(super) fn computer(&mut self, id: u8) -> &mut Computer {
        &mut self.computers[usize::from(id) - 1]
    }

    pub(super) fn at(&self, id: u8) -> &Computer {
        &self.computers[usize::from(id) - 1]
    }

    pub(super) fn tick(&mut self) {
        self.clock.advance(Duration::from_millis(1));
        let now = self.clock.now();
        for computer in &mut self.computers {
            if computer.native {
                computer.actor.pass(now, true);
            }
            let pointer = computer.capture.pointer;
            let capture = computer.native.then_some(&mut computer.capture);
            let ticked = computer.core.on_tick(capture, || Some(pointer));
            assert_eq!(ticked, Ok(()), "computer {} failed", computer.id);
            computer.note_floor();
        }
        self.pump();
    }

    pub(super) fn idle(&mut self, millis: u64) {
        for _ in 0..millis {
            self.tick();
        }
    }

    /// Everything each computer has to do, without delivering a frame.
    pub(super) fn stage(&mut self) {
        for index in 0..self.computers.len() {
            while self.step(index) {}
        }
    }

    pub(super) fn pump(&mut self) {
        self.run(|_| false);
    }

    /// Runs until `done` holds, checked after every step and every delivered frame.
    pub(super) fn pump_until(&mut self, done: impl Fn(&Self) -> bool) {
        assert!(self.run(done), "the mesh settled first");
    }

    /// True once `done` holds, false once nothing is left to do.
    fn run(&mut self, done: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..100_000 {
            let mut busy = false;
            for index in 0..self.computers.len() {
                busy |= self.step(index);
            }
            if done(self) {
                return true;
            }
            if let Some((to, from, frame)) = self.wire.pop_front() {
                self.deliver(to, from, frame);
                busy = true;
                if done(self) {
                    return true;
                }
            }
            if let Some((id, peer)) = self.closing.pop_front() {
                self.end_link(id, peer);
                busy = true;
            }
            if !busy {
                return false;
            }
        }
        panic!("the mesh never settled");
    }

    /// One computer's actions, destination events and captured records; true if it had any.
    pub(super) fn step(&mut self, index: usize) -> bool {
        let now = self.clock.now();
        let actions: Vec<HubAction> = self.computers[index].core.actions().collect();
        let mut busy = !actions.is_empty();
        for action in actions {
            self.execute(index, action);
        }
        let computer = &mut self.computers[index];
        if computer.native {
            computer.actor.pass(now, false);
        }
        while let Some((slot, event)) = computer.actor.events.pop_front() {
            busy = true;
            let capture = computer.native.then_some(&mut computer.capture);
            let handled = computer.core.on_destination(capture, slot, event);
            assert_eq!(handled, Ok(()), "computer {} failed", computer.id);
        }
        while computer.native
            && let Some(record) = computer.capture.queue.pop_front()
        {
            busy = true;
            let captured = computer.core.on_captured(&mut computer.capture, record);
            assert_eq!(captured, Ok(()), "computer {} failed", computer.id);
        }
        computer.note_floor();
        busy
    }

    fn execute(&mut self, index: usize, action: HubAction) {
        let now = self.clock.now();
        let id = self.computers[index].id;
        let computer = &mut self.computers[index];
        match action {
            HubAction::SendOutbound { slot, frame } => self.send(id, slot, frame, true),
            HubAction::SendInbound { slot, frame } => self.send(id, slot, frame, false),
            HubAction::StartNative => {
                computer.native = true;
                computer.core.native_ready();
            }
            HubAction::StopNative => {
                computer.actor.shut_down();
                computer.native = false;
                computer.native_stops += 1;
                computer.core.native_stopped();
            }
            HubAction::JoinDestination(join) => computer.actor.join(*join, now),
            HubAction::Submit { slot, frame } => computer.actor.submit(slot, frame, now),
            HubAction::LeaveDestination { slot } => computer.actor.leave(slot),
            HubAction::CloseLink { slot, end } => {
                let peer = computer.peer(slot);
                computer.closed.push((peer, end));
                // The connection closes: what was in flight is lost, and the peer's reader ends.
                self.wire.retain(|(to, from, _)| {
                    !((*to, *from) == (id, peer) || (*to, *from) == (peer, id))
                });
                self.closing.push_back((peer, id));
            }
            HubAction::LinkEnded { slot } => {
                let peer = computer.peer(slot);
                computer.ended.push(peer);
            }
            HubAction::LinkStarted { slot, peer } => {
                assert_eq!(peer, device(computer.peer(slot)));
                let peer = computer.peer(slot);
                computer.started.push(peer);
            }
        }
    }

    fn send(&mut self, id: u8, slot: FloorPeer, frame: Frame, outbound: bool) {
        let peer = self.at(id).peer(slot);
        let scopes = SessionScopes::new(device(id), device(peer)).unwrap();
        let scope = if outbound {
            scopes.outbound
        } else {
            scopes.inbound
        };
        if !self.cut.contains(&(id, peer)) {
            self.wire.push_back((peer, id, frame.with_scope(scope)));
        }
    }

    pub(super) fn deliver(&mut self, to: u8, from: u8, frame: Frame) {
        let computer = self.computer(to);
        let slot = computer.slot(from);
        let capture = computer.native.then_some(&mut computer.capture);
        let delivered = computer.core.on_frame(capture, slot, frame);
        assert_eq!(delivered, Ok(()), "computer {to} failed");
        computer.note_floor();
    }

    pub(super) fn end_link(&mut self, id: u8, peer: u8) {
        let computer = self.computer(id);
        let slot = computer.slot(peer);
        let capture = computer.native.then_some(&mut computer.capture);
        let ended = computer
            .core
            .remove_link(capture, slot, Err(SessionFailure::Wire));
        assert_eq!(ended, Ok(()), "computer {id} failed");
        computer.note_floor();
    }

    /// Both ends of the link between `a` and `b` see it drop.
    pub(super) fn drop_link(&mut self, a: u8, b: u8) {
        self.end_link(a, b);
        self.end_link(b, a);
    }

    pub(super) fn queue(&mut self, id: u8, event: CaptureEvent) {
        self.computer(id).capture.capture(event);
    }

    pub(super) fn input(&mut self, id: u8, event: CaptureEvent) {
        self.queue(id, event);
        self.pump();
    }

    fn rest(&mut self, id: u8, x: f64, y: f64) {
        self.input(id, rest_at(x, y));
    }

    pub(super) fn moved(&mut self, id: u8, dx: f64) {
        self.input(id, moved(dx, 0.0));
    }

    pub(super) fn key(&mut self, id: u8, usage: u16, pressed: bool, modifiers: u8) {
        self.input(
            id,
            CaptureEvent::Key {
                usage: HidUsage(usage),
                pressed,
                repeat: false,
                modifiers: ModifierState(modifiers),
            },
        );
    }

    pub(super) fn tap(&mut self, id: u8, usage: u16) {
        self.key(id, usage, true, 0);
        self.key(id, usage, false, 0);
    }

    /// Rests on the right edge and pushes through it.
    pub(super) fn cross_right(&mut self, id: u8) {
        self.rest(id, 99.0, 50.0);
        self.input(id, push(1.0, 0.0));
    }

    /// Rests on the left edge and pushes through it.
    pub(super) fn cross_left(&mut self, id: u8) {
        self.rest(id, 0.0, 50.0);
        self.input(id, push(-1.0, 0.0));
    }

    /// From a left-edge entry, over to the right edge and through it.
    pub(super) fn push_on(&mut self, id: u8) {
        self.moved(id, 98.0);
        self.input(id, push(1.0, 0.0));
    }

    pub(super) fn request_local(&mut self, id: u8) {
        let computer = self.computer(id);
        let capture = computer.native.then_some(&mut computer.capture);
        assert_eq!(computer.core.request_local(capture), Ok(()));
        self.pump();
    }

    /// Local motion on `id` while another computer controls it.
    pub(super) fn take_back(&mut self, id: u8) {
        let now = self.clock.now();
        let gate = self.computer(id).core.gate().clone();
        let stop = CaptureStop::new();
        let (mut producer, _consumer) = capture_channel(stop.clone());
        PhysicalCapture::new(now).with_take_back(gate).process(
            CaptureEvent::LogicalRelativeMotion { dx: 6.0, dy: 0.0 },
            false,
            now,
            &mut producer,
            &stop,
        );
        self.pump();
    }
}

pub(super) fn rest_at(x: f64, y: f64) -> CaptureEvent {
    CaptureEvent::LogicalAbsoluteMotion { x, y }
}

pub(super) fn moved(dx: f64, dy: f64) -> CaptureEvent {
    CaptureEvent::LogicalRelativeMotion { dx, dy }
}

/// One record carrying the full push along a unit direction.
pub(super) fn push(x: f64, y: f64) -> CaptureEvent {
    moved(x * PUSH_THROUGH_DISTANCE, y * PUSH_THROUGH_DISTANCE)
}

/// Where a trip that left display 1's right edge at mid-height comes home to.
pub(super) const TRIP_START: Point = Point::new(99.0, 50.0);

/// Every other computer around 1: 2 on its right, 3 on its left and 4 above it.
fn star() -> Topology {
    topology(
        4,
        &[
            (1, Edge::Right, 2, Edge::Left),
            (1, Edge::Left, 3, Edge::Right),
            (1, Edge::Top, 4, Edge::Bottom),
        ],
    )
}

/// 1's bottom-right corner joins 2's display on the right and 3's below.
fn corner() -> Topology {
    topology(
        3,
        &[
            (1, Edge::Right, 2, Edge::Left),
            (1, Edge::Bottom, 3, Edge::Top),
        ],
    )
}

fn declined(id: u8, reason: DeclineReason) -> Message {
    Message::ActivationDeclined {
        display_id: display(id),
        reason,
    }
}

fn is_activation(frame: &Frame) -> bool {
    matches!(frame.message, Message::ActivateDisplayAt { .. })
}

#[test]
fn three_peers_each_take_turns_controlling_the_other_two() {
    let mut mesh = Mesh::start(ring(3));
    for (home, first, second) in [(1, 2, 3), (2, 3, 1), (3, 1, 2)] {
        mesh.idle(20);
        let typed = [mesh.at(first).keys().len(), mesh.at(second).keys().len()];
        let released = [mesh.at(first).released(), mesh.at(second).released()];

        mesh.cross_right(home);
        assert_eq!(mesh.at(home).floor(), (FloorState::Sending, Some(first)));
        assert_eq!(mesh.at(first).floor(), (FloorState::Receiving, Some(home)));
        assert_eq!(mesh.at(home).source(first).mode(), SourceMode::Remote);
        mesh.tap(home, LETTER_A);

        // Pushed on through the first peer's far edge, the pointer goes straight to the second.
        mesh.push_on(home);
        assert_eq!(mesh.at(home).floor(), (FloorState::Sending, Some(second)));
        assert_eq!(mesh.at(first).floor(), (FloorState::Free, None));
        assert_eq!(mesh.at(second).floor(), (FloorState::Receiving, Some(home)));
        assert_eq!(mesh.at(first).released(), released[0] + 1);
        assert_eq!(mesh.at(home).source(first).mode(), SourceMode::Local);
        assert_eq!(mesh.at(home).source(second).mode(), SourceMode::Remote);
        mesh.tap(home, LETTER_B);

        // Through the second peer's far edge it comes home.
        mesh.push_on(home);
        for id in [home, first, second] {
            assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
        }
        assert_eq!(mesh.at(second).released(), released[1] + 1);
        assert_eq!(
            mesh.at(first).keys()[typed[0]..],
            [(LETTER_A, true), (LETTER_A, false)]
        );
        assert_eq!(
            mesh.at(second).keys()[typed[1]..],
            [(LETTER_B, true), (LETTER_B, false)]
        );
        let commands = mesh.at(home).capture.commands();
        let [Command::Activate, Command::Restore(back)] = commands[..] else {
            panic!("computer {home} made one trip: {commands:?}");
        };
        assert!(back.x < 2.0 && back.y == 50.0, "home through its left edge");
        assert!(!mesh.at(home).core.route().0);
        for peer in [first, second] {
            assert_eq!(mesh.at(home).source(peer).mode(), SourceMode::Local);
        }
    }
    for computer in &mesh.computers {
        assert!(computer.closed.is_empty());
    }
}

#[test]
fn handover_from_x_to_y_carries_a_held_modifier_and_never_restores_local_between() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    mesh.key(1, SHIFT, true, ModifierState::LEFT_SHIFT);
    assert_eq!(mesh.at(2).keys(), [(SHIFT, true)]);

    mesh.push_on(1);
    // X let go of everything it injected; Y takes the pointer with Shift already down.
    assert_eq!(mesh.at(2).released(), 1);
    assert!(!mesh.at(2).actor.injector.keys[usize::from(SHIFT)]);
    assert_eq!(
        mesh.at(3).actor.injector.actions[0],
        DestinationAction::MoveTo(Point::new(1.0, 50.0))
    );
    assert_eq!(mesh.at(3).keys(), [(SHIFT, true)]);
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(3)));
    assert_eq!(mesh.at(1).capture.commands(), [Command::Activate]);
    assert_eq!(mesh.at(1).core.route(), (true, 1));

    // Y's controller keeps the lease X's controller no longer owns.
    let renewals = mesh.at(1).capture.renewals;
    mesh.idle(40);
    assert!(mesh.at(1).capture.renewals > renewals);
    assert!(mesh.at(1).source(3).owns_capture_route());
    assert!(!mesh.at(1).source(2).owns_capture_route());

    mesh.key(1, SHIFT, false, 0);
    assert_eq!(mesh.at(3).keys(), [(SHIFT, true), (SHIFT, false)]);
    assert_eq!(mesh.at(2).keys(), [(SHIFT, true)]);

    mesh.push_on(1);
    let commands = mesh.at(1).capture.commands();
    assert!(matches!(
        commands[..],
        [Command::Activate, Command::Restore(_)]
    ));
    assert_eq!(
        mesh.at(1).floors,
        [
            (FloorState::Free, None),
            (FloorState::Requesting, Some(2)),
            (FloorState::Sending, Some(2)),
            (FloorState::Returning, Some(2)),
            (FloorState::Sending, Some(3)),
            (FloorState::Returning, Some(3)),
            (FloorState::Free, None),
        ]
    );
}

#[test]
fn losing_the_peer_that_holds_the_floor_restores_local_input_before_freeing_it() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_right(1);
    mesh.key(1, LETTER_A, true, 0);
    assert_eq!(mesh.at(2).keys(), [(LETTER_A, true)]);
    let held = mesh.at(1).core.floor();
    assert_eq!(
        (held.state, held.peer),
        (FloorState::Sending, mesh.at(1).slot(2))
    );

    // X's link drops: local input comes home at once, and the floor stays X's until it has.
    mesh.end_link(1, 2);
    let computer = mesh.at(1);
    assert_eq!(
        computer.capture.commands.last(),
        Some(&(Command::Restore(TRIP_START), held))
    );
    assert_eq!(computer.core.floor(), held);
    assert_eq!(computer.core.route(), (true, 1));

    mesh.pump();
    let computer = mesh.at(1);
    assert_eq!(computer.floor(), (FloorState::Free, None));
    assert_eq!(computer.core.route(), (false, 2));
    assert_eq!(computer.source(3).capture_route(), (false, 2));
    let [(peer, end)] = &computer.closed[..] else {
        panic!("one link closed");
    };
    assert_eq!((*peer, end.peer), (2, device(2)));
    assert_eq!(end.result, Err(SessionFailure::Wire));
    assert!(end.started_at.is_some());
    assert_eq!(end.holds, (0, Duration::ZERO));
    assert_eq!(computer.ended, [2]);
    assert!(computer.core.source(computer.slot(2)).is_none());
    // X released what it injected once its own end of the link closed.
    assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
    assert!(!mesh.at(2).actor.injector.keys[usize::from(LETTER_A)]);
    assert_eq!(mesh.at(2).ended, [1]);

    // The rest of the group runs on.
    mesh.key(1, LETTER_A, false, 0);
    mesh.idle(20);
    mesh.cross_left(1);
    assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(3)));
    assert_eq!(mesh.at(3).floor(), (FloorState::Receiving, Some(1)));
}

#[test]
fn losing_the_peer_this_computer_receives_from_releases_only_its_injection() {
    let mut mesh = Mesh::start(star());
    mesh.cross_left(2);
    assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(2)));
    mesh.key(2, LETTER_A, true, 0);
    assert_eq!(mesh.at(1).keys(), [(LETTER_A, true)]);
    let gate = mesh.at(1).core.gate().clone();
    assert!(gate.injected_held());

    // A bystander's link ending releases nothing the owner injected.
    mesh.drop_link(1, 4);
    mesh.pump();
    assert_eq!(mesh.at(1).ended, [4]);
    assert_eq!(mesh.at(1).released(), 0);
    assert!(gate.injected_held() && gate.admits_injection());
    assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(2)));

    // The owner's does, and its floor stays taken until that release lands.
    mesh.computer(1).actor.injector.refuse_release = true;
    mesh.drop_link(1, 2);
    mesh.pump();
    let closed: Vec<u8> = mesh.at(1).closed.iter().map(|(peer, _)| *peer).collect();
    assert_eq!(closed, [4, 2]);
    assert_eq!(mesh.at(1).ended, [4]);
    assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(2)));
    assert!(!gate.admits_injection());
    mesh.cross_right(3);
    let slot = mesh.at(1).slot(3);
    let busy = declined(1, DeclineReason::Busy);
    assert!(mesh.at(1).actor.replies.contains(&(slot, busy)));
    assert_eq!(mesh.at(3).floor(), (FloorState::Free, None));

    mesh.computer(1).actor.injector.refuse_release = false;
    mesh.tick();
    assert_eq!(mesh.at(1).ended, [4, 2]);
    assert_eq!(mesh.at(1).floor(), (FloorState::Free, None));
    assert_eq!(mesh.at(1).released(), 1);
    assert!(!mesh.at(1).actor.injector.keys[usize::from(LETTER_A)]);
    assert!(!gate.injected_held());

    // The peer still linked is admitted once it presses on past its decline's retry delay.
    let mut presses = 0;
    while mesh.at(1).floor() != (FloorState::Receiving, Some(3)) {
        assert!(presses < 6, "the decline is retried");
        presses += 1;
        mesh.idle(20);
        mesh.input(3, push(1.0, 0.0));
    }
    assert!(presses > 1, "the first press waits out the retry delay");
}

#[test]
fn simultaneous_activation_from_two_peers_admits_exactly_one_and_declines_the_other_busy() {
    for reversed in [false, true] {
        let mut mesh = Mesh::start(ring(3));
        // 2 crosses onto 1's right edge while 3 crosses onto its left edge.
        for event in [rest_at(0.0, 50.0), push(-1.0, 0.0)] {
            mesh.queue(2, event);
        }
        for event in [rest_at(99.0, 50.0), push(1.0, 0.0)] {
            mesh.queue(3, event);
        }
        mesh.stage();
        let requests: Vec<u8> = mesh
            .wire
            .iter()
            .filter(|(to, _, frame)| *to == 1 && is_activation(frame))
            .map(|(_, from, _)| *from)
            .collect();
        assert_eq!(requests, [2, 3]);
        if reversed {
            mesh.wire.make_contiguous().reverse();
        }
        mesh.pump();

        let (winner, loser) = if reversed { (3, 2) } else { (2, 3) };
        assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(winner)));
        assert_eq!(mesh.at(winner).floor(), (FloorState::Sending, Some(1)));
        assert_eq!(mesh.at(winner).source(1).mode(), SourceMode::Remote);
        assert_eq!(mesh.at(loser).floor(), (FloorState::Free, None));
        assert_eq!(mesh.at(loser).source(1).mode(), SourceMode::Local);
        let replies = &mesh.at(1).actor.replies;
        let (won, lost) = (mesh.at(1).slot(winner), mesh.at(1).slot(loser));
        assert!(replies.contains(&(won, Message::ActivationAck(display(1)))));
        assert!(replies.contains(&(lost, declined(1, DeclineReason::Busy))));
        assert!(!replies.contains(&(lost, Message::ActivationAck(display(1)))));

        mesh.tap(loser, LETTER_A);
        mesh.tap(winner, LETTER_B);
        assert_eq!(mesh.at(1).keys(), [(LETTER_B, true), (LETTER_B, false)]);
    }
}

#[test]
fn a_handover_to_a_peer_that_left_during_the_release_restores_local_input() {
    for giver_leaves in [false, true] {
        // 1 keeps its link to 4 throughout, so native input runs on.
        let mut mesh = Mesh::start(ring(4));
        mesh.cross_right(1);
        mesh.moved(1, 98.0);
        mesh.queue(1, push(1.0, 0.0));
        mesh.stage();
        let returning = mesh.at(1).core.floor();
        assert_eq!(mesh.at(1).floor(), (FloorState::Returning, Some(2)));
        assert!(mesh.wire.iter().any(|(to, from, frame)| {
            (*to, *from) == (2, 1) && frame.message == Message::ReleaseAll
        }));

        // Y's link drops before X acknowledges the release that hands the pointer to Y.
        mesh.drop_link(1, 3);
        if giver_leaves {
            // X's link drops too, after the restore went out but before its barrier is read.
            mesh.pump_until(|mesh| mesh.at(1).capture.commands.len() == 2);
            assert_eq!(mesh.at(1).capture.queue.len(), 1);
            mesh.end_link(1, 2);
            assert_eq!(mesh.at(1).core.floor(), returning);
        }
        mesh.pump();
        let computer = mesh.at(1);
        assert_eq!(
            computer.capture.commands.last(),
            Some(&(Command::Restore(TRIP_START), returning))
        );
        assert_eq!(computer.capture.commands.len(), 2);
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
        if giver_leaves {
            assert!(computer.core.source(computer.slot(2)).is_none());
            assert_eq!(computer.ended, [3, 2]);
        } else {
            assert_eq!(computer.source(2).mode(), SourceMode::Local);
            assert_eq!(computer.source(2).capture_route(), (false, 2));
        }
        // X released at the handover, and once more as its receiver stopped with the link.
        let released = if giver_leaves { 2 } else { 1 };
        assert_eq!(mesh.at(2).released(), released);
        assert_eq!(mesh.at(2).floor(), (FloorState::Free, None));
        // Y was never asked to take the pointer; its leaving receiver only released, as ever.
        let y = mesh.at(3);
        assert_eq!(y.floors, [(FloorState::Free, None)]);
        assert!(
            y.actor
                .received
                .iter()
                .all(|(_, message)| !matches!(message, Message::ActivateDisplayAt { .. }))
        );
        assert!(
            y.actor
                .injector
                .actions
                .iter()
                .all(|action| action.is_release())
        );
    }
}

#[test]
fn a_non_owner_hold_barrier_never_releases_the_owners_injected_input() {
    let mut mesh = Mesh::start(ring(3));
    mesh.cross_left(2);
    assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(2)));
    mesh.key(2, LETTER_A, true, 0);
    assert_eq!(mesh.at(1).keys(), [(LETTER_A, true)]);
    let owned = mesh.at(1).core.floor();

    // 1's replies stop reaching 3, whose controller for 1 holds and sends its barrier.
    mesh.cut.push((1, 3));
    mesh.idle(520);
    assert!(mesh.at(3).source(1).held_since().is_some());
    let slot = mesh.at(1).slot(3);
    assert!(
        mesh.at(1)
            .actor
            .received
            .contains(&(slot, Message::ReleaseAll))
    );
    assert!(mesh.at(1).source(3).held_since().is_some());

    let computer = mesh.at(1);
    assert_eq!(computer.keys(), [(LETTER_A, true)]);
    assert_eq!(computer.released(), 0);
    assert!(computer.actor.injector.keys[usize::from(LETTER_A)]);
    assert_eq!(computer.core.floor(), owned);
    assert!(computer.core.gate().admits_injection());
    mesh.key(2, LETTER_A, false, 0);
    assert_eq!(mesh.at(1).keys(), [(LETTER_A, true), (LETTER_A, false)]);
}

#[test]
fn a_corner_linked_to_two_peers_hands_the_crossing_to_exactly_one() {
    // Either push crosses on its own.
    for (along, peer) in [((1.0, 0.0), 2), ((0.0, 1.0), 3)] {
        let mut mesh = Mesh::start(corner());
        mesh.rest(1, 99.0, 99.0);
        mesh.input(1, push(along.0, along.1));
        assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(peer)));
    }
    // The hand turns the corner, through the second edge before the first peer answers.
    for (first, second, winner, other) in [
        ((1.0, 0.0), (0.0, 1.0), 2, 3),
        ((0.0, 1.0), (1.0, 0.0), 3, 2),
    ] {
        let mut mesh = Mesh::start(corner());
        mesh.rest(1, 99.0, 99.0);
        mesh.queue(1, push(first.0, first.1));
        mesh.stage();
        // Long enough for the first push's heading to fade, short of a pause.
        mesh.clock.advance(Duration::from_millis(60));
        mesh.queue(1, push(second.0, second.1));
        mesh.stage();
        let requests: Vec<u8> = mesh
            .wire
            .iter()
            .filter(|(_, from, frame)| *from == 1 && is_activation(frame))
            .map(|(to, _, _)| *to)
            .collect();
        assert_eq!(requests, [winner]);
        assert_eq!(mesh.at(1).floor(), (FloorState::Requesting, Some(winner)));

        mesh.pump();
        assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(winner)));
        assert_eq!(mesh.at(1).source(other).mode(), SourceMode::Local);
        assert_eq!(mesh.at(other).floor(), (FloorState::Free, None));
        assert!(
            mesh.at(other)
                .actor
                .received
                .iter()
                .all(|(_, message)| matches!(message, Message::Ping(_) | Message::Pong(_)))
        );
    }
}

#[test]
fn a_single_peer_hub_behaves_like_run_session() {
    // Crossing at once from both sides: the lower device wins in either arrival order.
    for reversed in [false, true] {
        let mut mesh = Mesh::start(pair());
        for event in [rest_at(99.0, 50.0), push(1.0, 0.0)] {
            mesh.queue(1, event);
        }
        for event in [rest_at(0.0, 50.0), push(-1.0, 0.0)] {
            mesh.queue(2, event);
        }
        mesh.stage();
        if reversed {
            mesh.wire.make_contiguous().reverse();
        }
        mesh.pump();
        assert_eq!(mesh.at(1).floor(), (FloorState::Sending, Some(2)));
        assert_eq!(mesh.at(2).floor(), (FloorState::Receiving, Some(1)));
        assert_eq!(mesh.at(2).source(1).mode(), SourceMode::Local);
        let contended = declined(1, DeclineReason::Contended);
        assert!(
            mesh.at(1)
                .actor
                .replies
                .contains(&(FloorPeer::SOLE, contended))
        );
    }

    // A crossing, typing, and a return on request.
    let mut mesh = Mesh::start(pair());
    mesh.cross_right(1);
    mesh.tap(1, LETTER_A);
    assert_eq!(mesh.at(2).keys(), [(LETTER_A, true), (LETTER_A, false)]);
    mesh.request_local(1);
    assert_eq!(mesh.at(1).source(2).mode(), SourceMode::Local);
    assert_eq!(
        mesh.at(1).capture.commands(),
        [Command::Activate, Command::Restore(TRIP_START)]
    );
    for id in [1, 2] {
        assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
    }

    // Local input on the controlled computer takes it back.
    mesh.idle(20);
    mesh.cross_left(2);
    assert_eq!(mesh.at(1).floor(), (FloorState::Receiving, Some(2)));
    mesh.take_back(1);
    assert!(
        mesh.at(1)
            .actor
            .replies
            .contains(&(FloorPeer::SOLE, Message::TakeBack))
    );
    assert_eq!(mesh.at(2).source(1).mode(), SourceMode::Local);
    assert_eq!(
        mesh.at(2).capture.commands(),
        [Command::Activate, Command::Restore(Point::new(0.0, 50.0))]
    );
    for id in [1, 2] {
        assert_eq!(mesh.at(id).floor(), (FloorState::Free, None));
    }

    // Its only link ending stops native input, as a pairwise session's end does, and the next
    // start begins from nothing held and a fresh route.
    mesh.drop_link(1, 2);
    mesh.pump();
    for (id, peer) in [(1, 2), (2, 1)] {
        let computer = mesh.at(id);
        assert!(!computer.native);
        assert_eq!(computer.native_stops, 1);
        assert_eq!(computer.ended, [peer]);
        let core = &computer.core;
        assert_eq!(core.native, Native::Idle);
        assert_eq!(computer.floor(), (FloorState::Free, None));
        assert_eq!(core.route(), (false, 0));
        assert_eq!(core.held(), (Vec::new(), Vec::new()));
        assert!(!core.gate().injected_held());
        assert!(core.orphan.is_none() && core.pending.is_none() && core.submitted.is_none());
        assert!(core.slots.iter().all(|slot| matches!(slot, Slot::Empty)));
    }
}

#[test]
fn presses_held_since_capture_started_are_named_until_released_or_native_input_stops() {
    let mut mesh = Mesh::start(pair());
    let held = [
        HeldInput::Key(HidUsage(0x50)),
        HeldInput::Button(MouseButton::Middle),
    ];
    mesh.computer(1).capture.blocking = held.to_vec();
    mesh.tick();
    assert_eq!(mesh.at(1).core.blocking_presses(), held);
    assert!(mesh.at(2).core.blocking_presses().is_empty());

    mesh.computer(1).capture.blocking.truncate(1);
    mesh.tick();
    assert_eq!(mesh.at(1).core.blocking_presses(), &held[..1]);
    mesh.computer(1).capture.blocking.clear();
    mesh.tick();
    assert!(mesh.at(1).core.blocking_presses().is_empty());

    mesh.computer(1).capture.blocking = held.to_vec();
    mesh.tick();
    mesh.drop_link(1, 2);
    mesh.pump();
    assert!(!mesh.at(1).native);
    assert!(mesh.at(1).core.blocking_presses().is_empty());
}
