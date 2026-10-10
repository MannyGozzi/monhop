//! Pure, synchronous routing core for one computer sharing with several paired peers at once.
//!
//! [`HubCore`] holds every peer link's startup controls and source controller, the floor, the
//! capture route with its issuer, the one pending route command and the physical key ledger. It
//! performs no I/O and starts no thread. Frames and destination-actor commands come back as
//! [`HubAction`]s, drained with [`HubCore::actions`], which the shell executes in order after each
//! call. Route commands go straight to the [`CaptureControl`] passed in, because a controller binds
//! its ticket before the next captured record is read. Every controller call reads the hub clock
//! the core was built with, as the source-runtime helpers do, so no caller can hand a controller
//! a stale time. An `Err` from any entry point is hub-fatal: the shell stops capture and injection,
//! which restores local input without the network, and ends every link.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the share hub shell is its first caller")
)]

use crate::{
    session::{DISPLAY_CHECK_INTERVAL, SESSION_QUEUE_CAPACITY, SessionFailure, SessionScopes},
    session_clock::SessionClock,
    session_hub_actor::SlotEvent,
    session_native::pointer_confined,
    session_runtime::POINTER_POLL_INTERVAL,
    session_source::{
        Handover, MotionTarget, NormalizedInput, SourceController, SourceEffect, SourceEffects,
        SourceFailure, SourceMode, SourceOutcome, TaggedInput,
    },
    session_source_runtime::{
        CaptureControl, CaptureRefusal, OutboundFrames, apply_effects, blocking_presses, normalize,
        renew_lease, retry_pending, settle_submitted,
    },
    session_startup::{ReadyControl, StartupControl, StartupError, startup_failure},
    session_wire::is_heartbeat,
};
use monhop_core::{
    DeviceId, DisplayId, FloorOwner, FloorPeer, FloorSnapshot, FloorState, HidUsage,
    MAX_GROUP_PEERS, MouseButton, Point, SharedFloor, TakeBackGate, Topology,
    capture::{CapturedEvent, MAX_SUPPRESSION_TTL},
    capture_physical::HeldInput,
};
use monhop_protocol::{DisplayTopology, Frame, FrameScope, Message, SessionEpoch};
use std::{cell::RefCell, collections::VecDeque, fmt, time::Duration};

mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;

/// What a hub is built from. Slots follow `members` order: the first member is slot 1.
pub(crate) struct HubConfig {
    pub local: DeviceId,
    pub group: Topology,
    /// The group's other computers, at most [`MAX_GROUP_PEERS`].
    pub members: Vec<DeviceId>,
    /// The sharing generation native capture routes under.
    pub generation: u64,
    /// This computer's double-click interval, read once per hub.
    pub double_click: Duration,
}

/// The facts of one authenticated Share session the core needs; the shell keeps its connection.
pub(crate) struct LinkSetup {
    pub peer: DeviceId,
    /// The session's permissions let this computer control the peer.
    pub outbound_enabled: bool,
    /// The session's permissions let the peer control this computer.
    pub inbound_enabled: bool,
    pub epoch: SessionEpoch,
    /// The first unused control sequence after the handshake, the same in both directions.
    pub next_sequence: u64,
    /// The displays each side announced in the handshake.
    pub local_displays: DisplayTopology,
    pub peer_displays: DisplayTopology,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkRefusal {
    /// The peer is not one of the hub's members.
    NotMember,
    /// The peer's slot still holds a link that has not finished ending.
    Occupied,
    /// Native input is stopping after the last link left; add the link once it has stopped.
    Stopping,
    /// The session's displays disagree with the group topology.
    Layout,
}

/// A link's end, reported when it closes; its destination slot may still be leaving.
pub(crate) struct LinkEnd {
    pub peer: DeviceId,
    pub result: Result<(), SessionFailure>,
    /// When both directions finished startup, on the hub clock.
    pub started_at: Option<Duration>,
    /// Holds the link's source survived and the longest one.
    pub holds: (u32, Duration),
}

/// A peer's receiver, fresh from startup, for the destination actor.
pub(crate) struct DestinationJoin {
    pub slot: FloorPeer,
    pub control: ReadyControl,
    pub local_lower: bool,
    pub enabled: bool,
}

/// Work the shell performs, in order, after each [`HubCore`] call.
pub(crate) enum HubAction {
    /// Send on `slot`'s link in the direction this computer controls.
    SendOutbound { slot: FloorPeer, frame: Frame },
    /// Send on `slot`'s link in the direction its peer controls.
    SendInbound { slot: FloorPeer, frame: Frame },
    /// Claim native input, start capture, then the destination actor; report
    /// [`HubCore::native_ready`] once both are ready.
    StartNative,
    /// Stop capture and the destination actor, releasing native input; report
    /// [`HubCore::native_stopped`] once finished.
    StopNative,
    /// Run the joining peer's receiver in the destination actor.
    JoinDestination(Box<DestinationJoin>),
    /// Hand an inbound frame to `slot`'s receiver in the destination actor.
    Submit { slot: FloorPeer, frame: Frame },
    /// Stop `slot`'s receiver; the core waits for its [`SlotEvent::Left`].
    LeaveDestination { slot: FloorPeer },
    /// Close `slot`'s link: nothing more is sent or read on it.
    CloseLink { slot: FloorPeer, end: LinkEnd },
    /// `slot` is free for a new link: its peer's end can be reported.
    LinkEnded { slot: FloorPeer },
    /// Both directions of `slot`'s link finished startup.
    LinkStarted { slot: FloorPeer, peer: DeviceId },
}

impl fmt::Debug for HubAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, slot) = match self {
            Self::SendOutbound { slot, .. } => ("SendOutbound", Some(slot)),
            Self::SendInbound { slot, .. } => ("SendInbound", Some(slot)),
            Self::StartNative => ("StartNative", None),
            Self::StopNative => ("StopNative", None),
            Self::JoinDestination(join) => ("JoinDestination", Some(&join.slot)),
            Self::Submit { slot, .. } => ("Submit", Some(slot)),
            Self::LeaveDestination { slot } => ("LeaveDestination", Some(slot)),
            Self::CloseLink { slot, .. } => ("CloseLink", Some(slot)),
            Self::LinkEnded { slot } => ("LinkEnded", Some(slot)),
            Self::LinkStarted { slot, .. } => ("LinkStarted", Some(slot)),
        };
        match slot {
            Some(slot) => write!(formatter, "HubAction::{name}(slot {})", slot.get()),
            None => write!(formatter, "HubAction::{name}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Native {
    Idle,
    Starting,
    Ready,
    Stopping,
}

enum Slot {
    Empty,
    Linked(Box<PeerLink>),
    /// The link ended; its receiver is leaving the destination actor.
    Leaving,
}

struct PeerLink {
    peer: DeviceId,
    scopes: SessionScopes,
    outbound_enabled: bool,
    inbound_enabled: bool,
    local_lower: bool,
    peer_offset: Point,
    phase: Phase,
    /// While starting, it keeps only the physical ledger; once ready, it routes.
    source: SourceController,
    /// Inbound input that arrived after the inbound half was ready but before the link was.
    early_input: VecDeque<Frame>,
    started_at: Option<Duration>,
    /// Its receiver runs in the destination actor, so its end waits for that slot's Left.
    joined: bool,
}

enum Phase {
    Starting(Box<Controls>),
    Ready,
}

/// The startup control of each direction.
struct Controls {
    outbound: StartupControl,
    inbound: StartupControl,
}

enum Startup {
    Waiting,
    NeedsNative,
    Ready,
}

impl PeerLink {
    fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }

    /// The pairwise runtime's startup frame handling: inbound input that beats the link's
    /// readiness waits in `early_input`.
    fn startup_frame(
        &mut self,
        actions: &mut Vec<HubAction>,
        slot: FloorPeer,
        outbound: bool,
        frame: Frame,
        now: Duration,
    ) -> Result<(), SessionFailure> {
        let Phase::Starting(controls) = &mut self.phase else {
            return Ok(());
        };
        let control = if outbound {
            &mut controls.outbound
        } else {
            &mut controls.inbound
        };
        if !outbound
            && !is_heartbeat(&frame.message)
            && !matches!(frame.message, Message::SessionReady)
            && control.is_ready(now).map_err(startup_failure)?
        {
            if self.early_input.len() == SESSION_QUEUE_CAPACITY {
                return Err(SessionFailure::QueueFull);
            }
            self.early_input.push_back(frame);
            return Ok(());
        }
        if let Some(response) = control.receive(&frame, now).map_err(startup_failure)? {
            actions.push(if outbound {
                HubAction::SendOutbound {
                    slot,
                    frame: response,
                }
            } else {
                HubAction::SendInbound {
                    slot,
                    frame: response,
                }
            });
        }
        Ok(())
    }

    /// Heartbeats both startup controls; readiness is announced only once native input is.
    fn startup_tick(
        &mut self,
        actions: &mut Vec<HubAction>,
        slot: FloorPeer,
        native: Native,
        now: Duration,
    ) -> Result<Startup, StartupError> {
        let Phase::Starting(controls) = &mut self.phase else {
            return Ok(Startup::Waiting);
        };
        let Controls { outbound, inbound } = &mut **controls;
        if let Some(frame) = outbound.poll(now)? {
            actions.push(HubAction::SendOutbound { slot, frame });
        }
        if let Some(frame) = inbound.poll(now)? {
            actions.push(HubAction::SendInbound { slot, frame });
        }
        if native != Native::Ready {
            let confirmed = outbound.health_confirmed(now)? && inbound.health_confirmed(now)?;
            return Ok(if confirmed {
                Startup::NeedsNative
            } else {
                Startup::Waiting
            });
        }
        if inbound.can_announce_ready(now)? {
            let frame = inbound.announce_ready(now)?;
            actions.push(HubAction::SendInbound { slot, frame });
        }
        if outbound.can_announce_ready(now)? {
            let frame = outbound.announce_ready(now)?;
            actions.push(HubAction::SendOutbound { slot, frame });
        }
        Ok(if outbound.is_ready(now)? && inbound.is_ready(now)? {
            Startup::Ready
        } else {
            Startup::Waiting
        })
    }
}

/// A local restore the hub completes for a floor whose owning controller is gone or refused it.
#[derive(Clone, Copy)]
struct Orphan {
    /// Held until the restore's barrier, then freed with `release_peer`.
    claim: FloorSnapshot,
    position: Point,
    /// None while native control is still busy with an earlier command.
    ticket: Option<u64>,
    since: Duration,
}

/// A route command whose barrier is still in the capture queue.
#[derive(Clone, Copy)]
struct Issued {
    slot: FloorPeer,
    ticket: u64,
    /// A local restore rather than a remote activation.
    local: bool,
}

/// Routes every peer link of one computer through one floor, one native capture and one
/// destination actor.
pub(crate) struct HubCore {
    local: DeviceId,
    group: Topology,
    initial_display: DisplayId,
    members: [Option<DeviceId>; MAX_GROUP_PEERS],
    generation: u64,
    double_click: Duration,
    origin: SessionClock,
    floor: SharedFloor,
    gate: TakeBackGate,
    native: Native,
    capture_ready: bool,
    /// What keeps `capture_ready` false, as of the last tick.
    blocking: Vec<HeldInput>,
    slots: [Slot; MAX_GROUP_PEERS],
    /// The last route barrier dispatched to the controllers.
    route: (bool, u64),
    /// The controller whose route command's barrier is still in the capture queue.
    issuer: Option<Issued>,
    /// The one route command native control was busy for, and the slot that issued it.
    pending: Option<(SourceEffect, Duration)>,
    pending_slot: Option<FloorPeer>,
    submitted: Option<(u64, Duration)>,
    orphan: Option<Orphan>,
    /// Input was asked home while the current trip was still in transition.
    local_requested: bool,
    /// Physically held keys and buttons, from every captured record.
    keys: [bool; 256],
    buttons: [bool; 5],
    last_renewed: Duration,
    last_pointer_poll: Duration,
    actions: Vec<HubAction>,
}

impl HubCore {
    pub(crate) fn new(config: HubConfig, origin: SessionClock) -> Result<Self, SessionFailure> {
        let HubConfig {
            local,
            group,
            members,
            generation,
            double_click,
        } = config;
        let local_display = |primary: bool| {
            group
                .displays()
                .find(|d| d.machine == local && d.in_use && (d.primary || !primary))
                .map(|d| d.id)
        };
        let initial_display = local_display(true)
            .or_else(|| local_display(false))
            .ok_or(SessionFailure::InvalidLayout)?;
        if members.is_empty() || members.len() > MAX_GROUP_PEERS {
            return Err(SessionFailure::InvalidLayout);
        }
        let mut slots = [None; MAX_GROUP_PEERS];
        for (index, member) in members.iter().enumerate() {
            let known = group.displays().any(|d| d.machine == *member);
            if *member == local || !known || members[..index].contains(member) {
                return Err(SessionFailure::InvalidLayout);
            }
            slots[index] = Some(*member);
        }
        let floor = SharedFloor::new();
        let now = origin.elapsed();
        Ok(Self {
            local,
            group,
            initial_display,
            members: slots,
            generation,
            double_click,
            origin,
            gate: TakeBackGate::new(floor.clone()),
            floor,
            native: Native::Idle,
            capture_ready: false,
            blocking: Vec::new(),
            slots: std::array::from_fn(|_| Slot::Empty),
            route: (false, 0),
            issuer: None,
            pending: None,
            pending_slot: None,
            submitted: None,
            orphan: None,
            local_requested: false,
            keys: [false; 256],
            buttons: [false; 5],
            last_renewed: now,
            last_pointer_poll: now,
            actions: Vec::new(),
        })
    }

    /// The gate native capture and the destination actor start on.
    pub(crate) fn gate(&self) -> &TakeBackGate {
        &self.gate
    }

    pub(crate) fn floor(&self) -> FloorSnapshot {
        self.floor.snapshot()
    }

    /// The last route barrier dispatched: whether input is remote, and its revision.
    pub(crate) fn route(&self) -> (bool, u64) {
        self.route
    }

    /// `slot`'s source controller while its link is up.
    pub(crate) fn source(&self, slot: FloorPeer) -> Option<&SourceController> {
        self.link(slot).map(|link| &link.source)
    }

    pub(crate) fn is_ready(&self, slot: FloorPeer) -> bool {
        self.link(slot).is_some_and(PeerLink::is_ready)
    }

    /// The presses keeping every seam a wall; empty once capture can suppress.
    pub(crate) fn blocking_presses(&self) -> &[HeldInput] {
        &self.blocking
    }

    /// What the last calls produced, for the shell to execute in order.
    pub(crate) fn actions(&mut self) -> std::vec::Drain<'_, HubAction> {
        self.actions.drain(..)
    }

    /// Starts a link to a member in its fixed slot. The peer's display block may only be
    /// translated from its own coordinates, and this computer's must be untranslated.
    pub(crate) fn add_link(&mut self, setup: LinkSetup) -> Result<FloorPeer, LinkRefusal> {
        if self.native == Native::Stopping {
            return Err(LinkRefusal::Stopping);
        }
        let index = self
            .members
            .iter()
            .position(|member| *member == Some(setup.peer))
            .ok_or(LinkRefusal::NotMember)?;
        if !matches!(self.slots[index], Slot::Empty) {
            return Err(LinkRefusal::Occupied);
        }
        let slot = slot_at(index);
        let scopes =
            SessionScopes::new(self.local, setup.peer).map_err(|_| LinkRefusal::NotMember)?;
        let peer_offset = validate_link(
            &self.group,
            (self.local, &setup.local_displays),
            (setup.peer, &setup.peer_displays),
        )
        .map_err(|_| LinkRefusal::Layout)?;
        let now = self.now();
        let mut source = SourceController::new(
            self.group.clone(),
            self.local,
            self.initial_display,
            setup.epoch,
            setup.next_sequence,
            now,
        )
        .map_err(|_| LinkRefusal::Layout)?
        .with_floor(self.floor.clone(), setup.outbound_enabled)
        .with_pointer_confinement(pointer_confined)
        .with_peer(setup.peer, slot)
        .map_err(|_| LinkRefusal::Layout)?;
        // Its ledger follows every key from here on, so it must start from what is held now.
        let (keys, buttons) = self.held();
        source
            .seed_from_group(&keys, &buttons, self.route, self.capture_ready)
            .map_err(|_| LinkRefusal::Layout)?;
        self.slots[index] = Slot::Linked(Box::new(PeerLink {
            peer: setup.peer,
            scopes,
            outbound_enabled: setup.outbound_enabled,
            inbound_enabled: setup.inbound_enabled,
            local_lower: self.local < setup.peer,
            peer_offset,
            phase: Phase::Starting(Box::new(Controls {
                outbound: StartupControl::new(setup.epoch, setup.next_sequence, true, now),
                inbound: StartupControl::new(setup.epoch, setup.next_sequence, false, now),
            })),
            source,
            early_input: VecDeque::new(),
            started_at: None,
            joined: false,
        }));
        Ok(slot)
    }

    /// Ends `slot`'s link. Local input comes home first if that peer held the floor outbound;
    /// a receiving slot's floor is freed by its own receiver, before its Left.
    pub(crate) fn remove_link<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        slot: FloorPeer,
        result: Result<(), SessionFailure>,
    ) -> Result<(), SessionFailure> {
        match index_of(slot) {
            Some(index) => self.end_link(capture, index, result, None),
            None => Ok(()),
        }
    }

    /// One frame read from `slot`'s link: this computer's controller takes the outbound scope
    /// and the destination actor the inbound one.
    pub(crate) fn on_frame<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        slot: FloorPeer,
        frame: Frame,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        let Some(index) = index_of(slot) else {
            return Ok(());
        };
        let Slot::Linked(link) = &mut self.slots[index] else {
            return Ok(());
        };
        if frame.scope == FrameScope::Connection {
            let result = if matches!(frame.message, Message::Disconnect(_)) {
                Ok(())
            } else {
                Err(SessionFailure::Wire)
            };
            return self.end_link(capture, index, result, None);
        }
        let outbound = frame.scope == link.scopes.outbound;
        if !outbound && frame.scope != link.scopes.inbound {
            return self.end_link(capture, index, Err(SessionFailure::UnexpectedStream), None);
        }
        if !link.is_ready() {
            return match link.startup_frame(&mut self.actions, slot, outbound, frame, now) {
                Ok(()) => Ok(()),
                Err(failure) => self.end_link(capture, index, Err(failure), None),
            };
        }
        if !outbound {
            self.actions.push(HubAction::Submit { slot, frame });
            return Ok(());
        }
        let outcome = link.source.on_remote_frame(&frame, now);
        self.settle(capture, index, outcome)
    }

    /// One record from native capture, in FIFO order. It is normalized once, for the floor
    /// owner's display, then reaches every controller: a route barrier goes to the controller
    /// that issued it and is adopted by every other.
    pub(crate) fn on_captured<C: CaptureControl>(
        &mut self,
        capture: &mut C,
        record: CapturedEvent,
    ) -> Result<(), SessionFailure> {
        let Some(record) = normalize(record, self.motion_target())? else {
            return Ok(());
        };
        self.capture_ready = capture.is_ready_for_suppression();
        self.note_physical(record.event);
        if let NormalizedInput::RouteChanged { remote, revision } = record.event {
            return self.dispatch_barrier(capture, record, remote, revision);
        }
        for index in 0..MAX_GROUP_PEERS {
            let now = self.now();
            let Slot::Linked(link) = &mut self.slots[index] else {
                continue;
            };
            if link.is_ready() {
                link.source.set_capture_ready(self.capture_ready);
                let outcome = link.source.on_captured(record, now);
                self.settle(Some(&mut *capture), index, outcome)?;
            } else if let Some(failure) = link.source.bookkeeping(record).failure {
                let result = Err(SessionFailure::SourceController(failure));
                self.end_link(Some(&mut *capture), index, result, None)?;
            }
        }
        Ok(())
    }

    /// One event from the destination actor for `slot`.
    pub(crate) fn on_destination<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        slot: FloorPeer,
        event: SlotEvent,
    ) -> Result<(), SessionFailure> {
        let Some(index) = index_of(slot) else {
            return Ok(());
        };
        // Only a receiver the actor runs for this link reports on it: anything else is left over
        // from an earlier receiver in the slot and must not end a link still starting.
        let joined = match &self.slots[index] {
            Slot::Linked(link) => link.joined,
            Slot::Leaving => true,
            Slot::Empty => false,
        };
        if !joined {
            return Ok(());
        }
        match event {
            SlotEvent::Response(frame) => {
                if matches!(self.slots[index], Slot::Linked(_)) {
                    self.actions.push(HubAction::SendInbound { slot, frame });
                }
                Ok(())
            }
            SlotEvent::Failed(_) => {
                self.end_link(capture, index, Err(SessionFailure::Destination), None)
            }
            SlotEvent::Left => {
                self.end_link(capture, index, Err(SessionFailure::Destination), None)?;
                if matches!(self.slots[index], Slot::Leaving) {
                    self.slots[index] = Slot::Empty;
                    self.actions.push(HubAction::LinkEnded { slot });
                }
                Ok(())
            }
        }
    }

    /// Housekeeping every poll interval: link startup and health, the route command native
    /// control was busy for, the orphan restore, pointer polls and the floor owner's lease.
    /// `pointer` reads the OS pointer and is called only when a poll is due.
    pub(crate) fn on_tick<C: CaptureControl>(
        &mut self,
        mut capture: Option<&mut C>,
        pointer: impl FnOnce() -> Option<Point>,
    ) -> Result<(), SessionFailure> {
        if let Some(capture) = capture.as_deref() {
            self.capture_ready = capture.is_ready_for_suppression();
            self.blocking = blocking_presses(capture);
        }
        for index in 0..MAX_GROUP_PEERS {
            let now = self.now();
            let native = self.native;
            let Slot::Linked(link) = &mut self.slots[index] else {
                continue;
            };
            if link.is_ready() {
                link.source.set_capture_ready(self.capture_ready);
                let outcome = link.source.tick(now);
                self.settle(capture.as_deref_mut(), index, outcome)?;
                continue;
            }
            match link.startup_tick(&mut self.actions, slot_at(index), native, now) {
                Ok(Startup::Waiting) => {}
                Ok(Startup::NeedsNative) => {
                    if self.native == Native::Idle {
                        self.native = Native::Starting;
                        self.actions.push(HubAction::StartNative);
                    }
                }
                Ok(Startup::Ready) => self.promote(capture.as_deref_mut(), index)?,
                Err(error) => {
                    let result = Err(startup_failure(error));
                    self.end_link(capture.as_deref_mut(), index, result, None)?;
                }
            }
        }
        // A stopping capture restores local input itself: nothing more is submitted or timed.
        let Some(capture) = capture.filter(|_| self.native != Native::Stopping) else {
            return Ok(());
        };
        let now = self.now();
        settle_submitted(capture, &mut self.submitted, now)?;
        self.retry_pending_route(capture)?;
        self.submit_orphan(Some(&mut *capture))?;
        if self
            .orphan
            .is_some_and(|orphan| self.now().saturating_sub(orphan.since) >= MAX_SUPPRESSION_TTL)
        {
            return Err(SessionFailure::Native);
        }
        self.poll_pointer(capture, pointer)?;
        self.renew_owner_lease(capture)
    }

    /// Brings input home from whichever peer this computer controls. Asked while that trip is
    /// still activating or handing over, it applies once the trip is Remote, and lapses with a
    /// trip that ends first.
    pub(crate) fn request_local<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
    ) -> Result<(), SessionFailure> {
        if self.outbound_owner().is_none() {
            return Ok(());
        }
        self.local_requested = true;
        self.follow_local_request(capture)
    }

    /// Capture runs and the destination actor is native-ready.
    pub(crate) fn native_ready(&mut self) {
        if self.native == Native::Starting {
            self.native = Native::Ready;
        }
    }

    /// Native input stopped after [`HubAction::StopNative`]: every leaving slot is free, no claim
    /// outlives it, and the next capture starts from a fresh route and an empty ledger. A report
    /// outside that stop is stale and changes nothing.
    pub(crate) fn native_stopped(&mut self) {
        if self.native != Native::Stopping {
            return;
        }
        self.floor.reset();
        self.native = Native::Idle;
        self.capture_ready = false;
        self.blocking.clear();
        self.route = (false, 0);
        self.issuer = None;
        self.pending = None;
        self.pending_slot = None;
        self.submitted = None;
        self.orphan = None;
        self.local_requested = false;
        self.keys = [false; 256];
        self.buttons = [false; 5];
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if matches!(slot, Slot::Leaving) {
                *slot = Slot::Empty;
                self.actions.push(HubAction::LinkEnded {
                    slot: slot_at(index),
                });
            }
        }
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn link(&self, slot: FloorPeer) -> Option<&PeerLink> {
        match &self.slots[index_of(slot)?] {
            Slot::Linked(link) => Some(link),
            _ => None,
        }
    }

    /// The ready link whose controller holds the floor outbound.
    fn outbound_owner(&self) -> Option<usize> {
        let floor = self.floor.snapshot();
        let index = index_of(floor.peer)?;
        (floor.state.owner() == Some(FloorOwner::Outbound)
            && matches!(&self.slots[index], Slot::Linked(link) if link.is_ready()))
        .then_some(index)
    }

    /// The floor owner's display; with the floor free or inbound every controller is local, so
    /// any gives the same conversion.
    fn motion_target(&self) -> Option<MotionTarget> {
        if self.floor.snapshot().state.owner() == Some(FloorOwner::Outbound) {
            let Slot::Linked(link) = &self.slots[self.outbound_owner()?] else {
                return None;
            };
            return link.source.motion_target();
        }
        self.slots.iter().find_map(|slot| match slot {
            Slot::Linked(link) => link.source.motion_target(),
            _ => None,
        })
    }

    fn note_physical(&mut self, event: NormalizedInput) {
        match event {
            NormalizedInput::Key { usage, pressed, .. } if usage.is_valid() => {
                self.keys[usize::from(usage.0)] = pressed;
            }
            NormalizedInput::Button {
                button, pressed, ..
            } => self.buttons[button.index()] = pressed,
            _ => {}
        }
    }

    fn held(&self) -> (Vec<HidUsage>, Vec<MouseButton>) {
        let keys = (0..self.keys.len())
            .filter(|&usage| self.keys[usage])
            .map(|usage| HidUsage(usage as u16))
            .collect();
        let buttons = MouseButton::ALL
            .into_iter()
            .filter(|button| self.buttons[button.index()])
            .collect();
        (keys, buttons)
    }

    fn dispatch_barrier<C: CaptureControl>(
        &mut self,
        capture: &mut C,
        record: TaggedInput,
        remote: bool,
        revision: u64,
    ) -> Result<(), SessionFailure> {
        self.route = (remote, revision);
        let issuer = self
            .issuer
            .filter(|issued| issued.ticket == revision)
            .map(|issued| issued.slot);
        if issuer.is_some() {
            self.issuer = None;
        }
        for index in 0..MAX_GROUP_PEERS {
            let now = self.now();
            let Slot::Linked(link) = &mut self.slots[index] else {
                continue;
            };
            if !link.is_ready() {
                continue;
            }
            let outcome = if issuer == Some(slot_at(index)) {
                link.source.on_captured(record, now)
            } else {
                link.source.observe_foreign_route(remote, revision, now)
            };
            self.settle(Some(&mut *capture), index, outcome)?;
        }
        if let Some(orphan) = self.orphan.filter(|orphan| orphan.ticket == Some(revision)) {
            self.orphan = None;
            // Nothing else can move a floor held for a link that is gone.
            if !self
                .floor
                .release_peer(orphan.claim.peer, orphan.claim.generation)
            {
                return Err(SessionFailure::Source);
            }
        }
        Ok(())
    }

    /// Applies one controller outcome for the link in `index`, then a local request that waited
    /// for the trip's owner to be Remote. Every controller outcome passes here, so a trip that
    /// ends drops the request before another trip can begin.
    fn settle<C: CaptureControl>(
        &mut self,
        mut capture: Option<&mut C>,
        index: usize,
        outcome: SourceOutcome,
    ) -> Result<(), SessionFailure> {
        self.settle_outcome(capture.as_deref_mut(), index, outcome)?;
        self.follow_local_request(capture)
    }

    fn follow_local_request<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
    ) -> Result<(), SessionFailure> {
        if !self.local_requested {
            return Ok(());
        }
        let Some(index) = self.outbound_owner() else {
            self.local_requested = false;
            return Ok(());
        };
        let now = self.now();
        let Slot::Linked(link) = &mut self.slots[index] else {
            return Ok(());
        };
        if link.source.mode() != SourceMode::Remote {
            return Ok(());
        }
        self.local_requested = false;
        let outcome = link.source.request_local(now);
        self.settle_outcome(capture, index, outcome)
    }

    /// A handover goes on, within this call, to the controller of the peer it names, so
    /// suppression never lapses between them.
    fn settle_outcome<C: CaptureControl>(
        &mut self,
        mut capture: Option<&mut C>,
        index: usize,
        outcome: SourceOutcome,
    ) -> Result<(), SessionFailure> {
        let SourceOutcome {
            effects,
            failure,
            handover,
        } = outcome;
        let Some(handover) = handover else {
            return self.settle_effects(capture, index, &effects, failure);
        };
        if failure.is_some() {
            let (claim, position) = (handover.claim(), handover.return_at().1);
            self.orphan_restore(capture.as_deref_mut(), claim, position)?;
            return self.settle_effects(capture, index, &effects, failure);
        }
        self.settle_effects(capture.as_deref_mut(), index, &effects, failure)?;
        self.deliver_handover(capture, index, handover)
    }

    /// A failing outcome's effects are never applied: its link ends, and a local restore it asked
    /// for becomes the hub's own.
    fn settle_effects<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        index: usize,
        effects: &SourceEffects,
        failure: Option<SourceFailure>,
    ) -> Result<(), SessionFailure> {
        if let Some(failure) = failure {
            let result = Err(SessionFailure::SourceController(failure));
            return self.end_link(capture, index, result, restore_point(effects));
        }
        if effects.is_empty() {
            return Ok(());
        }
        let slot = slot_at(index);
        let Slot::Linked(link) = &mut self.slots[index] else {
            return Ok(());
        };
        let out = SlotOutbox {
            slot,
            actions: RefCell::new(&mut self.actions),
        };
        let Some(capture) = capture else {
            for effect in effects.iter() {
                let SourceEffect::RemoteFrame(frame) = effect else {
                    return Err(SessionFailure::Native);
                };
                out.send_outbound(frame.clone())?;
            }
            return Ok(());
        };
        let mut issuing = Issuing {
            capture,
            issued: None,
        };
        apply_effects(
            effects,
            &mut link.source,
            &mut issuing,
            &out,
            self.generation,
            &self.origin,
            &mut self.pending,
            &mut self.submitted,
        )?;
        if let Some((ticket, local)) = issuing.issued {
            self.issuer = Some(Issued {
                slot,
                ticket,
                local,
            });
        }
        if self.pending.is_some() && self.pending_slot.is_none() {
            self.pending_slot = Some(slot);
        }
        Ok(())
    }

    /// The controller of the peer `handover` names takes it; with none ready, or a refusal,
    /// input comes home and the handed floor is held until that restore's barrier.
    fn deliver_handover<C: CaptureControl>(
        &mut self,
        mut capture: Option<&mut C>,
        from: usize,
        handover: Handover,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        let to = handover.to().machine;
        let target = self.slots.iter_mut().enumerate().find(|(index, slot)| {
            *index != from
                && matches!(slot, Slot::Linked(link) if link.peer == to && link.is_ready())
        });
        let (index, outcome) = match target {
            Some((index, Slot::Linked(link))) => {
                (index, link.source.accept_handover(handover, now))
            }
            _ => {
                log::info!("handover: the next computer's link is not up; input returns here");
                let (claim, position) = (handover.claim(), handover.return_at().1);
                return self.orphan_restore(capture, claim, position);
            }
        };
        let SourceOutcome {
            effects,
            failure,
            handover: refused,
        } = outcome;
        if let Some(refused) = refused {
            let (claim, position) = (refused.claim(), refused.return_at().1);
            self.orphan_restore(capture.as_deref_mut(), claim, position)?;
        }
        self.settle_effects(capture, index, &effects, failure)
    }

    fn orphan_restore<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        claim: FloorSnapshot,
        position: Point,
    ) -> Result<(), SessionFailure> {
        self.hold_orphan(claim, position, None)?;
        self.submit_orphan(capture)
    }

    /// The trip ends here: the claim is held for the hub's own restore, and a local request made
    /// during the trip lapses with it.
    fn hold_orphan(
        &mut self,
        claim: FloorSnapshot,
        position: Point,
        ticket: Option<u64>,
    ) -> Result<(), SessionFailure> {
        // One floor has one owner, so a second orphan means the core lost track of it.
        if self.orphan.is_some() {
            return Err(SessionFailure::Source);
        }
        self.orphan = Some(Orphan {
            claim,
            position,
            ticket,
            since: self.now(),
        });
        self.local_requested = false;
        Ok(())
    }

    fn submit_orphan<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        let Some(orphan) = self
            .orphan
            .as_mut()
            .filter(|orphan| orphan.ticket.is_none())
        else {
            return Ok(());
        };
        let capture = capture.ok_or(SessionFailure::Native)?;
        match capture.restore_local_at(self.generation, orphan.position) {
            Ok(ticket) => {
                orphan.ticket = Some(ticket);
                self.submitted = Some((ticket, now));
                Ok(())
            }
            Err(CaptureRefusal::Pending) => Ok(()),
            Err(_) => Err(SessionFailure::Native),
        }
    }

    fn end_link<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        index: usize,
        result: Result<(), SessionFailure>,
        restore: Option<Point>,
    ) -> Result<(), SessionFailure> {
        if !matches!(self.slots[index], Slot::Linked(_)) {
            return Ok(());
        }
        let Slot::Linked(mut link) = std::mem::replace(&mut self.slots[index], Slot::Empty) else {
            return Ok(());
        };
        let slot = slot_at(index);
        let now = self.now();
        let abandoned = link.source.abandon(now);
        let restore = restore
            .or_else(|| restore_point(&abandoned.effects))
            .or_else(|| abandoned.handover.map(|handover| handover.return_at().1));
        if self.pending_slot == Some(slot) {
            self.pending = None;
            self.pending_slot = None;
        }
        let issued = self.issuer.filter(|issued| issued.slot == slot);
        if issued.is_some() {
            self.issuer = None;
        }
        let floor = self.floor.snapshot();
        // An orphan restore already holds this claim until its own barrier.
        let orphaned = self.orphan.is_some_and(|orphan| orphan.claim == floor);
        if !orphaned && floor.peer == slot && floor.state.owner() == Some(FloorOwner::Outbound) {
            match issued {
                // The link's own restore is already queued: its barrier brings input home.
                Some(Issued {
                    ticket,
                    local: true,
                    ..
                }) => {
                    let position = restore.unwrap_or_else(|| self.home_origin());
                    self.hold_orphan(floor, position, Some(ticket))?;
                }
                _ => {
                    // Native input is or is about to be remote: a restore the controller never
                    // yielded brings the pointer to the home display instead.
                    let remote = self.route.0 || issued.is_some();
                    match restore.or_else(|| remote.then(|| self.home_origin())) {
                        Some(position) => self.orphan_restore(capture, floor, position)?,
                        None => {
                            self.local_requested = false;
                            self.floor.release_peer(slot, floor.generation);
                        }
                    }
                }
            }
        }
        let end = LinkEnd {
            peer: link.peer,
            result,
            started_at: link.started_at,
            holds: link.source.hold_stats(now),
        };
        self.actions.push(HubAction::CloseLink { slot, end });
        if link.joined {
            self.slots[index] = Slot::Leaving;
            self.actions.push(HubAction::LeaveDestination { slot });
        } else {
            self.actions.push(HubAction::LinkEnded { slot });
        }
        self.refresh_reachable();
        if self.native != Native::Idle
            && self.native != Native::Stopping
            && !self
                .slots
                .iter()
                .any(|slot| matches!(slot, Slot::Linked(_)))
        {
            // Stopping capture restores local input and native_stopped frees the floor, so no
            // restore or route command outlives native input.
            self.native = Native::Stopping;
            self.orphan = None;
            self.pending = None;
            self.pending_slot = None;
            self.submitted = None;
            self.local_requested = false;
            self.actions.push(HubAction::StopNative);
        }
        Ok(())
    }

    /// Where input comes home when no controller named a place: the home display's origin.
    fn home_origin(&self) -> Point {
        self.group
            .display(self.initial_display)
            .map(|display| display.origin)
            .unwrap_or_default()
    }

    /// Both directions are ready: the routing controller replaces the startup one, seeded from
    /// the ledger and the last dispatched route, and the receiver joins the destination actor.
    fn promote<C: CaptureControl>(
        &mut self,
        capture: Option<&mut C>,
        index: usize,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        let slot = slot_at(index);
        let (keys, buttons) = self.held();
        let Slot::Linked(link) = &mut self.slots[index] else {
            return Ok(());
        };
        let Phase::Starting(controls) = std::mem::replace(&mut link.phase, Phase::Ready) else {
            return Ok(());
        };
        let Controls { outbound, inbound } = *controls;
        let ready = (|| {
            let control = outbound.into_ready(now).map_err(startup_failure)?;
            let mut source = SourceController::after_startup(
                self.group.clone(),
                self.local,
                self.initial_display,
                control,
            )
            .map_err(SessionFailure::SourceController)?
            .with_floor(self.floor.clone(), link.outbound_enabled)
            .with_peer(link.peer, slot)
            .map_err(|_| SessionFailure::InvalidLayout)?;
            source.set_peer_offset(link.peer_offset);
            source.set_double_click_interval(self.double_click);
            source
                .seed_from_group(&keys, &buttons, self.route, self.capture_ready)
                .map_err(SessionFailure::SourceController)?;
            let control = inbound.into_ready(now).map_err(startup_failure)?;
            Ok((source, control))
        })();
        let (source, control) = match ready {
            Ok(ready) => ready,
            Err(failure) => return self.end_link(capture, index, Err(failure), None),
        };
        link.source = source;
        link.joined = true;
        link.started_at = Some(now);
        self.actions
            .push(HubAction::JoinDestination(Box::new(DestinationJoin {
                slot,
                control,
                local_lower: link.local_lower,
                enabled: link.inbound_enabled,
            })));
        for frame in link.early_input.drain(..) {
            self.actions.push(HubAction::Submit { slot, frame });
        }
        self.actions.push(HubAction::LinkStarted {
            slot,
            peer: link.peer,
        });
        self.refresh_reachable();
        Ok(())
    }

    /// Each ready controller may hand over to every other ready peer this computer may control.
    fn refresh_reachable(&mut self) {
        let mut ready = [None; MAX_GROUP_PEERS];
        for (entry, slot) in ready.iter_mut().zip(&self.slots) {
            if let Slot::Linked(link) = slot
                && link.is_ready()
                && link.outbound_enabled
            {
                *entry = Some(link.peer);
            }
        }
        let mut peers = [DeviceId::default(); MAX_GROUP_PEERS];
        for slot in &mut self.slots {
            let Slot::Linked(link) = slot else {
                continue;
            };
            if !link.is_ready() {
                continue;
            }
            let mut count = 0;
            for peer in ready.iter().flatten().filter(|peer| **peer != link.peer) {
                peers[count] = *peer;
                count += 1;
            }
            link.source.set_reachable(&peers[..count]);
        }
    }

    fn retry_pending_route<C: CaptureControl>(
        &mut self,
        capture: &mut C,
    ) -> Result<(), SessionFailure> {
        let Some(slot) = self.pending_slot else {
            return Ok(());
        };
        let Some(Slot::Linked(link)) = index_of(slot).map(|index| &mut self.slots[index]) else {
            self.pending = None;
            self.pending_slot = None;
            return Ok(());
        };
        let out = SlotOutbox {
            slot,
            actions: RefCell::new(&mut self.actions),
        };
        let mut issuing = Issuing {
            capture,
            issued: None,
        };
        retry_pending(
            &mut self.pending,
            &mut link.source,
            &mut issuing,
            &out,
            self.generation,
            &self.origin,
            &mut self.submitted,
        )?;
        if let Some((ticket, local)) = issuing.issued {
            self.issuer = Some(Issued {
                slot,
                ticket,
                local,
            });
        }
        if self.pending.is_none() {
            self.pending_slot = None;
        }
        Ok(())
    }

    /// While every route is local and the floor free, the OS pointer reaches each local
    /// controller in slot order; the first to claim the floor walls every later one.
    fn poll_pointer<C: CaptureControl>(
        &mut self,
        capture: &mut C,
        pointer: impl FnOnce() -> Option<Point>,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        let settled = self.floor.snapshot().state == FloorState::Free
            && !self.route.0
            && self.pending.is_none()
            && self.orphan.is_none();
        let local = |slot: &Slot| {
            matches!(slot, Slot::Linked(link)
                if link.is_ready() && link.source.mode() == SourceMode::Local)
        };
        if !settled
            || !self.slots.iter().any(local)
            || now.saturating_sub(self.last_pointer_poll) < POINTER_POLL_INTERVAL
        {
            return Ok(());
        }
        self.last_pointer_poll = now;
        let Some(point) = pointer() else {
            return Ok(());
        };
        for index in 0..MAX_GROUP_PEERS {
            let now = self.now();
            if !local(&self.slots[index]) {
                continue;
            }
            let Slot::Linked(link) = &mut self.slots[index] else {
                continue;
            };
            let outcome = link.source.observe_pointer(point, now);
            self.settle(Some(&mut *capture), index, outcome)?;
        }
        Ok(())
    }

    /// Only the controller holding the floor outbound, and owning the remote route, renews
    /// suppression: its lease follows the floor from peer to peer.
    fn renew_owner_lease<C: CaptureControl>(
        &mut self,
        capture: &mut C,
    ) -> Result<(), SessionFailure> {
        let now = self.now();
        if self.pending.is_some()
            || self.submitted.is_some()
            || self.orphan.is_some()
            || now.saturating_sub(self.last_renewed) < DISPLAY_CHECK_INTERVAL
        {
            return Ok(());
        }
        let Some(index) = self.outbound_owner() else {
            return Ok(());
        };
        let Slot::Linked(link) = &mut self.slots[index] else {
            return Ok(());
        };
        let source = &link.source;
        if source.held_since().is_some()
            || !source.owns_capture_route()
            || !source.capture_route().0
        {
            return Ok(());
        }
        match renew_lease(
            &mut link.source,
            capture,
            self.generation,
            &self.origin,
            &mut self.submitted,
        ) {
            Ok(true) => {
                self.last_renewed = self.now();
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(SessionFailure::Native) => Err(SessionFailure::Native),
            // The owner's own link cannot back the lease: it ends and input comes home.
            Err(failure) => self.end_link(Some(capture), index, Err(failure), None),
        }
    }
}

/// Frames a controller's effects send, queued as actions for its slot.
struct SlotOutbox<'a> {
    slot: FloorPeer,
    actions: RefCell<&'a mut Vec<HubAction>>,
}

impl OutboundFrames for SlotOutbox<'_> {
    fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure> {
        self.actions.borrow_mut().push(HubAction::SendOutbound {
            slot: self.slot,
            frame,
        });
        Ok(())
    }
}

/// Native capture as one controller's effects reach it, noting the ticket of the route change
/// they submit, and whether it restores local input, so its barrier goes back to that controller.
struct Issuing<'a, C> {
    capture: &'a mut C,
    issued: Option<(u64, bool)>,
}

impl<C: CaptureControl> CaptureControl for Issuing<'_, C> {
    fn activate_remote(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        let ticket = self.capture.activate_remote(generation, ttl)?;
        self.issued = Some((ticket, false));
        Ok(ticket)
    }
    fn restore_local_at(
        &mut self,
        generation: u64,
        position: Point,
    ) -> Result<u64, CaptureRefusal> {
        let ticket = self.capture.restore_local_at(generation, position)?;
        self.issued = Some((ticket, true));
        Ok(ticket)
    }
    fn renew_suppression(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        self.capture.renew_suppression(generation, ttl)
    }
    fn completed_control_revision(&self) -> Option<u64> {
        self.capture.completed_control_revision()
    }
    fn is_ready_for_suppression(&self) -> bool {
        self.capture.is_ready_for_suppression()
    }
    fn blocking_presses(&self) -> Vec<HeldInput> {
        self.capture.blocking_presses()
    }
    fn stop_reason(&self) -> Option<monhop_core::capture::StopReason> {
        self.capture.stop_reason()
    }
    fn request_stop(&self) {
        self.capture.request_stop();
    }
}

fn restore_point(effects: &SourceEffects) -> Option<Point> {
    effects.iter().find_map(|effect| match effect {
        SourceEffect::RestoreLocalAt { position, .. } => Some(*position),
        _ => None,
    })
}

/// Slot 1 is index 0; [`FloorPeer::NONE`] has none.
fn index_of(slot: FloorPeer) -> Option<usize> {
    usize::from(slot.get())
        .checked_sub(1)
        .filter(|&index| index < MAX_GROUP_PEERS)
}

fn slot_at(index: usize) -> FloorPeer {
    u8::try_from(index + 1)
        .ok()
        .and_then(FloorPeer::slot)
        .expect("a group slot index")
}

/// This computer's block must match the group untranslated and the peer's may only be
/// translated; each side's displays must be exactly its displays in the group. Yields the
/// peer's translation.
fn validate_link(
    group: &Topology,
    local: (DeviceId, &DisplayTopology),
    peer: (DeviceId, &DisplayTopology),
) -> Result<Point, SessionFailure> {
    let mut offset = None;
    for ((device, native), is_local) in [(local, true), (peer, false)] {
        for d in native.displays() {
            let actual = group
                .display(d.id)
                .map_err(|_| SessionFailure::InvalidLayout)?;
            if actual.machine != device
                || actual.logical_size.width != d.logical_size.x
                || actual.logical_size.height != d.logical_size.y
                || actual.native_size.width != d.native_width
                || actual.native_size.height != d.native_height
                || actual.scale_factor != f64::from(d.scale_factor)
            {
                return Err(SessionFailure::InvalidLayout);
            }
            let delta = Point::new(
                actual.origin.x - d.logical_origin.x,
                actual.origin.y - d.logical_origin.y,
            );
            if is_local && delta != Point::default() {
                return Err(SessionFailure::InvalidLayout);
            }
            if !is_local {
                if offset.is_some_and(|previous| previous != delta) {
                    return Err(SessionFailure::InvalidLayout);
                }
                offset = Some(delta);
            }
        }
        if group.displays().filter(|d| d.machine == device).count() != native.displays().len() {
            return Err(SessionFailure::InvalidLayout);
        }
    }
    offset.ok_or(SessionFailure::InvalidLayout)
}
