//! Source-side input routing with explicit transport and native effects.
//!
//! This controller is transport- and platform-free. Its caller must already have authenticated
//! the peer, obtained explicit sharing authorization, and normalized capture into the logical
//! coordinate spaces documented by [`NormalizedInput`]. The route tag assigned by the native hook
//! thread is authoritative: an event keeps that classification until its FIFO barrier is consumed.
//! The controller does not defer a cross-route event backlog. Its fixed key/button ledger records
//! held state synchronously, and any unexpected tag or unbound barrier fails closed.
//!
//! The negotiated base epoch carries peer-health control traffic for the whole session. Activation
//! epochs carry only activation, input, and release messages. Keeping those sequence spaces apart
//! lets a Ping race an activation without becoming an input-epoch violation.

use std::{fmt, time::Duration};

use monhop_core::{
    DeviceId, DisplayId, Edge, EdgeTransition, FloorOwner, FloorSnapshot, FloorState, HidUsage,
    ModifierState, MouseButton, Platform, Point, PointerOwnership, PointerTarget, SharedFloor,
    Topology, TransitionAcknowledgement,
};
use monhop_protocol::{
    Button, Frame, Key, MAX_LOGICAL_ORIGIN_ABS, Message, Motion, RateLimiter, SequenceGate,
    SessionEpoch,
};

use crate::session_clock::millis_u64;
use crate::session_health::{HOLD_LIMIT, HealthError, PeerHealth, RETREAT_AFTER, hold_stats};
use crate::session_startup::ReadyControl;

/// Maximum effects produced by one controller call.
///
/// At most eight held modifiers and five held mouse buttons transfer at a route barrier. The
/// remaining slots hold required native control effects. Exceeding this bound fails closed.
pub const MAX_SOURCE_EFFECTS: usize = 16;

/// Logical px the pointer must move off the edge a crossing entered through to cross it again.
pub const ENTRY_GUARD_DISTANCE: f64 = 24.0;

/// A platform event converted into source-controller units.
///
/// `AbsoluteMotion` is a topology logical position. `RelativeMotion` is a logical delta in the
/// target display's coordinate system. Adapters convert physical desktop pixels and raw counts
/// before constructing this type. Scroll is in signed wheel detents: positive horizontal is right
/// and positive vertical is up. Windows adapters divide 120-unit detents, while macOS adapters
/// divide 40-point detents after their horizontal inversion; both preserve fractional values.
#[derive(Clone, Copy, PartialEq)]
pub enum NormalizedInput {
    Key {
        usage: HidUsage,
        pressed: bool,
        repeat: bool,
        modifiers: ModifierState,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    AbsoluteMotion(Point),
    RelativeMotion(Point),
    Scroll {
        horizontal: f64,
        vertical: f64,
    },
    /// Native-capture FIFO barrier for a new route.
    RouteChanged {
        remote: bool,
        revision: u64,
    },
}

impl fmt::Debug for NormalizedInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Key { .. } => "Key",
            Self::Button { .. } => "Button",
            Self::AbsoluteMotion(_) => "AbsoluteMotion",
            Self::RelativeMotion(_) => "RelativeMotion",
            Self::Scroll { .. } => "Scroll",
            Self::RouteChanged { .. } => "RouteChanged",
        };
        write!(formatter, "NormalizedInput::{name}([redacted])")
    }
}

impl NormalizedInput {
    fn is_valid(self) -> bool {
        match self {
            Self::Key { usage, .. } => usage.is_valid(),
            Self::Button { .. } => true,
            Self::AbsoluteMotion(point) | Self::RelativeMotion(point) => valid_point(point),
            Self::Scroll {
                horizontal,
                vertical,
            } => {
                horizontal.is_finite()
                    && vertical.is_finite()
                    && horizontal.abs() <= 1_000_000.0
                    && vertical.abs() <= 1_000_000.0
            }
            Self::RouteChanged { revision, .. } => revision != 0,
        }
    }
}

/// A normalized event with the native capture route that admitted it.
#[derive(Clone, Copy, PartialEq)]
pub struct TaggedInput {
    pub event: NormalizedInput,
    pub routing_revision: u64,
    pub remote: bool,
    pub floor_generation: u64,
}

impl fmt::Debug for TaggedInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TaggedInput([redacted])")
    }
}

/// Required work for the authenticated transport owner or native capture owner.
#[derive(Clone, PartialEq)]
pub enum SourceEffect {
    /// Send this already-sequenced frame on the authenticated reliable stream.
    RemoteFrame(Frame),
    /// Atomically release locally delivered input, acquire remote suppression, and enqueue the
    /// remote route barrier. Bind the synchronously returned native ticket with
    /// [`SourceController::bind_capture_route`] before draining the capture queue.
    ActivateRemote { request: RouteRequest },
    /// Atomically release remote suppression, place the local cursor, restore native-owned held
    /// modifiers/buttons, and enqueue the local route barrier after receiver `ReleaseAck`.
    RestoreLocalAt {
        request: RouteRequest,
        display: DisplayId,
        position: Point,
    },
}

impl fmt::Debug for SourceEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteFrame(_) => formatter.write_str("SourceEffect::RemoteFrame([redacted])"),
            Self::ActivateRemote { request } => formatter
                .debug_struct("SourceEffect::ActivateRemote")
                .field("request", request)
                .finish(),
            Self::RestoreLocalAt { .. } => {
                formatter.write_str("SourceEffect::RestoreLocalAt([redacted])")
            }
        }
    }
}

/// Fixed-capacity effects from one controller call.
#[derive(Clone, PartialEq)]
pub struct SourceEffects {
    effects: [Option<SourceEffect>; MAX_SOURCE_EFFECTS],
    len: usize,
}

impl Default for SourceEffects {
    fn default() -> Self {
        Self {
            effects: std::array::from_fn(|_| None),
            len: 0,
        }
    }
}

impl SourceEffects {
    fn push(&mut self, effect: SourceEffect) -> Result<(), ()> {
        if self.len == MAX_SOURCE_EFFECTS {
            return Err(());
        }
        self.effects[self.len] = Some(effect);
        self.len += 1;
        Ok(())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &SourceEffect> {
        self.effects[..self.len].iter().map(|effect| {
            effect
                .as_ref()
                .expect("effects below length are initialized")
        })
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for SourceEffects {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SourceEffects([redacted])")
    }
}

/// Result from one controller operation.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceOutcome {
    pub effects: SourceEffects,
    pub failure: Option<SourceFailure>,
}

/// Construction failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceConfigError {
    UnknownLocalDisplay,
    LocalDisplayBelongsToAnotherMachine,
    Ownership,
}

/// A sticky source-controller failure. No variant includes input values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    ClockRegression,
    PeerHealth(HealthError),
    InvalidCapturedInput,
    InvalidKeyState,
    RouteMismatch,
    UnexpectedRouteBarrier,
    InvalidActivationAcknowledgement,
    InvalidReleaseAcknowledgement,
    InvalidRemoteFrame,
    RateLimited,
    NativeControl,
    Topology,
    Ownership,
    ProtocolEpochExhausted,
    SequenceExhausted,
    RouteRequestExhausted,
    EffectCapacityExceeded,
}

/// Observable source routing mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    Local,
    AwaitActivationAcknowledgement,
    AwaitRemoteCaptureBarrier,
    Remote,
    AwaitRemoteReleaseAcknowledgement,
    AwaitLocalCaptureBarrier,
    Failed,
}

/// An opaque request identity for one source-issued native route command.
///
/// It is not a capture ticket. Native suppression renewals can advance their tickets without a
/// route change, so the runtime must bind the actual returned ticket before reading capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteRequest(u64);

impl RouteRequest {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The stable route's destination coordinate convention for a platform adapter.
///
/// This is read-only. It is `None` while a route change is awaiting a peer acknowledgement or a
/// native FIFO barrier, so an adapter cannot classify a pending route as active.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionTarget {
    pub target: PointerTarget,
    pub platform: Platform,
    pub scale_factor: f64,
}

/// Pure source routing controller.
pub struct SourceController {
    floor: SharedFloor,
    floor_claim: Option<FloorSnapshot>,
    free_generation: u64,
    rebase_pointer: bool,
    /// This computer's own trip ended at a verified cursor, so its release needs no fresh poll.
    anchored_return: bool,
    enabled: bool,
    /// Native capture refuses suppression while input held since its start is down.
    capture_ready: bool,
    take_back: bool,
    declined: Option<(DisplayId, DisplayId, Option<Duration>, Duration)>,
    /// Display and edge the last crossing entered through, so sensor noise cannot bounce it back.
    entry_guard: Option<(DisplayId, Edge)>,
    peer_offset: Point,
    topology: Topology,
    ownership: PointerOwnership,
    home: PointerTarget,
    state: State,
    base_epoch: SessionEpoch,
    input_epoch: SessionEpoch,
    outbound_input_sequence: u64,
    outbound_control_sequence: u64,
    inbound_input_epoch: Option<SessionEpoch>,
    inbound_input_sequence: Option<u64>,
    /// Peer heartbeats arrive as datagrams: newer than the last one counts, gaps are loss.
    inbound_heartbeats: SequenceGate,
    capture_route: CaptureRoute,
    next_route_request: u64,
    failed_route_request: Option<RouteRequest>,
    keys: [KeyState; 256],
    buttons: [ButtonState; 5],
    health: PeerHealth,
    remote_limiter: RateLimiter,
    last_now: Duration,
    remote_target: Option<PointerTarget>,
    failure: Option<SourceFailure>,
    /// Set while the peer has been silent past the retreat point and its barrier is unacknowledged.
    held_since: Option<Duration>,
    /// ReleaseAll frames the receiver still owes an acknowledgement for; the hold's barrier is
    /// the last of them, so the hold ends only once the count reaches zero.
    release_acks_pending: u32,
    hold_pong_seen: bool,
    holds: u32,
    held_max: Duration,
}

#[derive(Clone, Copy)]
enum State {
    Local {
        target: PointerTarget,
        cursor: Option<Point>,
    },
    AwaitActivation {
        target: PointerTarget,
        entry: Point,
        return_target: PointerTarget,
        return_position: Point,
        ownership_epoch: u64,
        capture_already_remote: bool,
    },
    AwaitRemoteBarrier {
        target: PointerTarget,
        position: Point,
        return_target: PointerTarget,
        return_position: Point,
        request: RouteRequest,
        ticket: Option<u64>,
    },
    Remote {
        target: PointerTarget,
        position: Point,
        return_target: PointerTarget,
        return_position: Point,
    },
    AwaitReleaseAcknowledgement {
        local: PointerTarget,
        local_position: Point,
        ownership_epoch: Option<u64>,
    },
    AwaitLocalBarrier {
        local: PointerTarget,
        position: Point,
        request: RouteRequest,
        ticket: Option<u64>,
    },
    Failed,
}

#[derive(Clone, Copy)]
struct CaptureRoute {
    remote: bool,
    revision: u64,
}

#[derive(Clone, Copy, Default)]
struct KeyState {
    physical: bool,
    remote: bool,
}

#[derive(Clone, Copy, Default)]
struct ButtonState {
    physical: bool,
    remote: bool,
}

fn heartbeat_gate(epoch: SessionEpoch, last: Option<u64>) -> SequenceGate {
    let mut gate = SequenceGate::default();
    gate.activate_epoch(epoch).expect("fresh heartbeat gate");
    gate.seed_heartbeat(last);
    gate
}

impl SourceController {
    /// Creates a controller after core topology validation and authenticated session setup.
    ///
    /// The first remote activation uses `session_epoch + 1`, matching the receiver's v2 anchored
    /// activation rule. `session_epoch` itself remains the control Ping/Pong epoch forever.
    /// `next_control_sequence` is the first unused sequence after the authenticated handshake.
    pub fn new(
        topology: Topology,
        local_machine: DeviceId,
        local_display: DisplayId,
        session_epoch: SessionEpoch,
        next_control_sequence: u64,
        now: Duration,
    ) -> Result<Self, SourceConfigError> {
        let display = topology
            .display(local_display)
            .map_err(|_| SourceConfigError::UnknownLocalDisplay)?;
        if display.machine != local_machine {
            return Err(SourceConfigError::LocalDisplayBelongsToAnotherMachine);
        }
        let home = PointerTarget::new(local_machine, local_display);
        let ownership =
            PointerOwnership::new(local_machine, home).map_err(|_| SourceConfigError::Ownership)?;
        Ok(Self {
            floor: SharedFloor::new(),
            floor_claim: None,
            free_generation: 1,
            rebase_pointer: false,
            anchored_return: false,
            enabled: true,
            capture_ready: true,
            take_back: false,
            declined: None,
            entry_guard: None,
            peer_offset: Point::default(),
            topology,
            ownership,
            home,
            state: State::Local {
                target: home,
                cursor: None,
            },
            base_epoch: session_epoch,
            input_epoch: session_epoch,
            outbound_input_sequence: 0,
            outbound_control_sequence: next_control_sequence,
            inbound_input_epoch: None,
            inbound_input_sequence: None,
            inbound_heartbeats: heartbeat_gate(session_epoch, next_control_sequence.checked_sub(1)),
            capture_route: CaptureRoute {
                remote: false,
                revision: 0,
            },
            next_route_request: 1,
            failed_route_request: None,
            keys: [KeyState::default(); 256],
            buttons: [ButtonState::default(); 5],
            health: PeerHealth::new(now),
            remote_limiter: RateLimiter::new(20_000).expect("bounded remote control rate"),
            last_now: now,
            remote_target: None,
            failure: None,
            held_since: None,
            release_acks_pending: 0,
            hold_pong_seen: false,
            holds: 0,
            held_max: Duration::ZERO,
        })
    }

    /// Both directional controllers share the same owner-tagged floor.
    pub fn with_floor(mut self, floor: SharedFloor, enabled: bool) -> Self {
        self.free_generation = floor.snapshot().generation;
        self.floor = floor;
        self.enabled = enabled;
        self
    }

    pub fn floor_generation(&self) -> u64 {
        self.floor.snapshot().generation
    }

    pub(crate) fn set_peer_offset(&mut self, offset: Point) {
        self.peer_offset = offset;
    }

    /// Input seen during readiness updates only the physical ledger.
    pub(crate) fn inherit_bookkeeping(&mut self, earlier: &Self) {
        self.keys = earlier.keys;
        self.buttons = earlier.buttons;
        self.capture_ready = earlier.capture_ready;
    }

    /// Every seam is a wall until native capture can suppress; local tracking continues meanwhile.
    pub fn set_capture_ready(&mut self, ready: bool) {
        self.capture_ready = ready;
    }

    pub(crate) fn bookkeeping(&mut self, record: TaggedInput) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        match record.event {
            NormalizedInput::Key { .. } | NormalizedInput::Button { .. } => {
                self.apply_local(record.event, &mut effects)
            }
            _ => {}
        }
        self.outcome(effects)
    }

    fn local_crossing_allowed(&mut self) -> bool {
        let floor = self.floor.snapshot();
        if floor.state != FloorState::Free {
            return false;
        }
        if floor.generation != self.free_generation {
            self.free_generation = floor.generation;
            self.rebase_pointer = true;
        }
        self.enabled && !self.rebase_pointer
    }

    fn release_floor(&mut self) {
        let anchored = std::mem::take(&mut self.anchored_return);
        if let Some(owned) = self.floor_claim.take() {
            match self.floor.transition(owned, FloorState::Free) {
                // No inbound claim came between, so the pointer needs no rebase from a poll.
                Ok(free) if anchored => self.free_generation = free.generation,
                Ok(_) => {}
                Err(_) => {
                    self.floor.release(FloorOwner::Outbound, owned.generation);
                }
            }
        }
    }

    pub(crate) fn after_startup(
        topology: Topology,
        source: DeviceId,
        initial_display: DisplayId,
        mut control: ReadyControl,
    ) -> Result<Self, SourceFailure> {
        control
            .health
            .check(control.now)
            .map_err(SourceFailure::PeerHealth)?;
        let mut controller = Self::new(
            topology,
            source,
            initial_display,
            control.epoch,
            control.next_outgoing,
            control.now,
        )
        .map_err(|_| SourceFailure::Topology)?;
        controller.finish_startup(control)?;
        Ok(controller)
    }

    pub(crate) fn finish_startup(&mut self, control: ReadyControl) -> Result<(), SourceFailure> {
        self.outbound_control_sequence = control.next_heartbeat_out;
        self.inbound_heartbeats = heartbeat_gate(control.epoch, control.last_heartbeat_in);
        self.health = control.health;
        self.remote_limiter = control.limiter;
        self.last_now = control.now;
        Ok(())
    }

    pub fn mode(&self) -> SourceMode {
        match self.state {
            State::Local { .. } => SourceMode::Local,
            State::AwaitActivation { .. } => SourceMode::AwaitActivationAcknowledgement,
            State::AwaitRemoteBarrier { .. } => SourceMode::AwaitRemoteCaptureBarrier,
            State::Remote { .. } => SourceMode::Remote,
            State::AwaitReleaseAcknowledgement { .. } => {
                SourceMode::AwaitRemoteReleaseAcknowledgement
            }
            State::AwaitLocalBarrier { .. } => SourceMode::AwaitLocalCaptureBarrier,
            State::Failed => SourceMode::Failed,
        }
    }

    pub fn failure(&self) -> Option<SourceFailure> {
        self.failure
    }

    /// When the current hold began: input is local and the session waits for the link.
    pub fn held_since(&self) -> Option<Duration> {
        self.held_since
    }

    /// How many holds this session survived and the longest one, including one still running.
    pub fn hold_stats(&self, now: Duration) -> (u32, Duration) {
        hold_stats(self.holds, self.held_max, self.held_since, now)
    }

    /// Returns the remaining fresh-peer time available for native suppression.
    ///
    /// The runtime must grant a lease shorter than this value, with its own scheduling margin. A
    /// received input frame or Ping does not refresh this budget. Only a validated `Pong` does.
    /// An error does not emit cleanup effects because this query is intended for the native lease
    /// owner; it must decline renewal and let the bounded lease expire, while the next controller
    /// operation emits the normal fail-closed cleanup effects.
    pub fn suppression_budget(&mut self, now: Duration) -> Result<Duration, SourceFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        if now < self.last_now {
            return Err(SourceFailure::ClockRegression);
        }
        self.last_now = now;
        if self.held_since.is_some() {
            return Err(SourceFailure::PeerHealth(HealthError::DeadlineExpired));
        }
        self.health
            .remaining(now)
            .map_err(SourceFailure::PeerHealth)
    }

    pub fn capture_route(&self) -> (bool, u64) {
        (self.capture_route.remote, self.capture_route.revision)
    }

    /// Returns the stable active display and its native motion conversion details.
    ///
    /// Runtime adapters use this only for relative motion tagged with the active route. Local
    /// absolute positions are already in the local topology's coordinate space. A Windows source
    /// divides raw counts for a macOS target by `scale_factor`; a macOS source multiplies logical
    /// deltas for a Windows target by it.
    pub fn motion_target(&self) -> Option<MotionTarget> {
        let target = match self.state {
            State::Local { target, .. } | State::Remote { target, .. } => target,
            State::AwaitActivation { .. }
            | State::AwaitRemoteBarrier { .. }
            | State::AwaitReleaseAcknowledgement { .. }
            | State::AwaitLocalBarrier { .. }
            | State::Failed => return None,
        };
        let display = self.topology.display(target.display).ok()?;
        let platform = self
            .topology
            .machine_for_display(target.display)
            .ok()?
            .platform;
        Some(MotionTarget {
            target,
            platform,
            scale_factor: display.scale_factor,
        })
    }

    /// Binds a source route request to the actual ticket returned by native control.
    ///
    /// Call synchronously after native command submission and before reading another capture
    /// record. Lease renewals can advance native ticket values independently, so this ticket must
    /// never be inferred from the request identity.
    pub fn bind_capture_route(
        &mut self,
        request: RouteRequest,
        ticket: u64,
        now: Duration,
    ) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() && self.failed_route_request != Some(request) {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects) {
            return self.outcome(effects);
        }
        if ticket == 0 || ticket <= self.capture_route.revision {
            self.fail(SourceFailure::RouteMismatch, &mut effects);
            return self.outcome(effects);
        }
        let mut accepted = false;
        match &mut self.state {
            State::AwaitRemoteBarrier {
                request: expected,
                ticket: expected_ticket,
                ..
            }
            | State::AwaitLocalBarrier {
                request: expected,
                ticket: expected_ticket,
                ..
            } if *expected == request && expected_ticket.is_none() => {
                *expected_ticket = Some(ticket);
                accepted = true;
            }
            State::Failed if self.failed_route_request == Some(request) => {
                self.failed_route_request = None;
                accepted = true;
            }
            _ => {}
        }
        if !accepted {
            self.fail(SourceFailure::RouteMismatch, &mut effects);
        }
        self.outcome(effects)
    }

    /// Records that native control rejected a requested route change.
    pub fn fail_capture_route_submission(
        &mut self,
        request: RouteRequest,
        now: Duration,
    ) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() && self.failed_route_request != Some(request) {
            return self.outcome(effects);
        }
        if self.observe_time(now, &mut effects) {
            self.fail(SourceFailure::NativeControl, &mut effects);
        }
        self.outcome(effects)
    }

    /// Native capture refused the activation because it cannot suppress yet: the peer that already
    /// acknowledged is released and the pointer stays home, as after a decline.
    pub fn refuse_capture_activation(
        &mut self,
        request: RouteRequest,
        now: Duration,
    ) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() || !self.observe_time(now, &mut effects) {
            return self.outcome(effects);
        }
        match self.state {
            State::AwaitRemoteBarrier {
                target,
                return_target,
                return_position,
                request: expected,
                ticket: None,
                ..
            } if expected == request => {
                self.capture_ready = false;
                self.take_back = false;
                self.declined = Some((return_target.display, target.display, Some(now), now));
                log::info!("return: the other computer declined control");
                self.begin_return_to(return_target, return_position, None, &mut effects);
            }
            _ => self.fail(SourceFailure::NativeControl, &mut effects),
        }
        self.outcome(effects)
    }

    /// Applies one capture event in native FIFO order.
    pub fn on_captured(&mut self, record: TaggedInput, now: Duration) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects)
            || !self.check_health(now, &mut effects)
            || !record.event.is_valid()
        {
            if self.failure.is_none() {
                self.fail(SourceFailure::InvalidCapturedInput, &mut effects);
            }
            return self.outcome(effects);
        }
        if let NormalizedInput::RouteChanged { remote, revision } = record.event {
            if record.remote != remote || record.routing_revision != revision {
                self.fail(SourceFailure::RouteMismatch, &mut effects);
            } else {
                self.on_route_changed(remote, revision, &mut effects);
            }
            return self.outcome(effects);
        }
        if record.remote != self.capture_route.remote
            || record.routing_revision != self.capture_route.revision
        {
            self.fail(SourceFailure::RouteMismatch, &mut effects);
            return self.outcome(effects);
        }
        if matches!(
            record.event,
            NormalizedInput::AbsoluteMotion(_) | NormalizedInput::RelativeMotion(_)
        ) {
            let floor = self.floor.snapshot();
            let permitted = if self.capture_route.remote {
                floor.state == FloorState::Sending && record.floor_generation == floor.generation
            } else {
                self.local_crossing_allowed() && record.floor_generation == self.free_generation
            };
            if !permitted {
                return self.outcome(effects);
            }
        }
        if self.capture_route.remote {
            self.apply_remote(record.event, &mut effects);
        } else {
            self.apply_local(record.event, &mut effects);
        }
        self.outcome(effects)
    }

    /// Applies an authenticated, decoded frame from the paired receiver.
    pub fn on_remote_frame(&mut self, frame: &Frame, now: Duration) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects) || !self.check_health(now, &mut effects) {
            return self.outcome(effects);
        }
        if self.failure.is_some() {
            return self.outcome(effects);
        }
        if self.remote_limiter.allow_at(millis_u64(now)).is_err() {
            self.fail(SourceFailure::RateLimited, &mut effects);
            return self.outcome(effects);
        }
        match frame.message {
            // Datagrams may arrive out of order: a stale heartbeat is dropped, never answered or trusted.
            Message::Ping(token) => {
                if self.inbound_heartbeats.accept_heartbeat(frame).is_ok() {
                    self.push_control(Message::Pong(token), &mut effects);
                }
                return self.outcome(effects);
            }
            Message::Pong(token) => {
                if self.inbound_heartbeats.accept_heartbeat(frame).is_ok() {
                    self.on_pong(token, now, &mut effects);
                }
                return self.outcome(effects);
            }
            // The receiver acknowledges every ReleaseAll in order; outside the ordinary return
            // path each one is a barrier acknowledgement.
            Message::ReleaseAck
                if self.release_acks_pending > 0
                    && !matches!(self.state, State::AwaitReleaseAcknowledgement { .. }) =>
            {
                // The receiver may never have been activated for this epoch: the barrier ack
                // is then the first frame of the epoch and opens it.
                let epoch_open = self.inbound_input_epoch == Some(self.input_epoch)
                    || self.activate_input_epoch(frame.epoch);
                if frame.epoch != self.input_epoch
                    || !epoch_open
                    || !self.accept_input_sequence(frame.sequence)
                {
                    self.fail(SourceFailure::InvalidReleaseAcknowledgement, &mut effects);
                } else {
                    self.release_acks_pending -= 1;
                    self.try_resume(now);
                }
                return self.outcome(effects);
            }
            _ => {}
        }
        if matches!(frame.message, Message::TakeBack) {
            // A take-back may precede an activation acknowledgement in the response epoch.
            let previous_reanchor = matches!(
                self.state,
                State::AwaitActivation {
                    capture_already_remote: true,
                    ..
                }
            ) && self.inbound_input_epoch == Some(frame.epoch)
                && frame.epoch < self.input_epoch;
            let accepted = if previous_reanchor {
                let fresh = self
                    .inbound_input_sequence
                    .is_none_or(|last| frame.sequence > last);
                if fresh {
                    self.inbound_input_sequence = Some(frame.sequence);
                }
                fresh
            } else {
                let opened = self.inbound_input_epoch == Some(frame.epoch)
                    || (matches!(self.state, State::AwaitActivation { .. })
                        && frame.epoch == self.input_epoch
                        && self.activate_input_epoch(frame.epoch));
                frame.epoch == self.input_epoch
                    && opened
                    && self.accept_input_sequence(frame.sequence)
            };
            if !accepted && !self.late_take_back(frame.epoch) {
                self.fail(SourceFailure::InvalidRemoteFrame, &mut effects);
            } else if accepted
                && !matches!(
                    self.state,
                    State::Local { .. }
                        | State::AwaitLocalBarrier { .. }
                        | State::AwaitReleaseAcknowledgement { .. }
                )
            {
                self.take_back = true;
            }
            return self.outcome(effects);
        }
        if self.held_since.is_some() {
            // Anything else while held is from before the barrier and proves nothing.
            return self.outcome(effects);
        }
        match self.state {
            State::AwaitActivation {
                target,
                entry,
                return_target,
                return_position,
                ownership_epoch,
                capture_already_remote,
            } => {
                let declined = matches!(frame.message, Message::ActivationDeclined { display_id, .. } if display_id == target.display);
                let valid = frame.epoch == self.input_epoch
                    && (declined
                        || matches!(frame.message, Message::ActivationAck(display) if display == target.display))
                    && (self.inbound_input_epoch == Some(frame.epoch)
                        || self.activate_input_epoch(frame.epoch))
                    && self.accept_input_sequence(frame.sequence);
                if !valid {
                    self.fail(
                        SourceFailure::InvalidActivationAcknowledgement,
                        &mut effects,
                    );
                    return self.outcome(effects);
                }
                if declined {
                    if !self.recover_ownership_at(return_target) {
                        self.fail(SourceFailure::Ownership, &mut effects);
                        return self.outcome(effects);
                    }
                    self.remote_target = None;
                    self.clear_remote_delivery();
                    self.take_back = false;
                    self.entry_guard = None;
                    self.declined = Some((return_target.display, target.display, Some(now), now));
                    log::info!("return: the other computer declined control");
                    if capture_already_remote {
                        if let Some(owned) = self.floor_claim {
                            match self.floor.transition(owned, FloorState::Returning) {
                                Ok(returning) => self.floor_claim = Some(returning),
                                Err(_) => {
                                    self.fail(SourceFailure::Ownership, &mut effects);
                                    return self.outcome(effects);
                                }
                            }
                        }
                        self.push_input(Message::ReleaseAll, &mut effects);
                        self.state = State::AwaitReleaseAcknowledgement {
                            local: return_target,
                            local_position: return_position,
                            ownership_epoch: None,
                        };
                    } else {
                        self.state = State::Local {
                            target: return_target,
                            cursor: Some(return_position),
                        };
                        self.anchored_return = true;
                        self.release_floor();
                    }
                    return self.outcome(effects);
                }
                if !capture_already_remote {
                    let Some(owned) = self.floor_claim else {
                        self.fail(SourceFailure::Ownership, &mut effects);
                        return self.outcome(effects);
                    };
                    match self.floor.transition(owned, FloorState::Sending) {
                        Ok(sending) => self.floor_claim = Some(sending),
                        Err(_) => {
                            self.fail(SourceFailure::Ownership, &mut effects);
                            return self.outcome(effects);
                        }
                    }
                }
                if self
                    .ownership
                    .acknowledge_transition(TransitionAcknowledgement {
                        target,
                        epoch: ownership_epoch,
                    })
                    .is_err()
                {
                    self.fail(SourceFailure::Ownership, &mut effects);
                    return self.outcome(effects);
                }
                self.remote_target = Some(target);
                if capture_already_remote {
                    self.synchronize_reanchored_remote_input(&mut effects);
                    if self.failure.is_none() {
                        self.state = State::Remote {
                            target,
                            position: entry,
                            return_target,
                            return_position,
                        };
                    }
                } else {
                    let Some(request) = self.allocate_route_request(&mut effects) else {
                        return self.outcome(effects);
                    };
                    if !self.push_effect(SourceEffect::ActivateRemote { request }, &mut effects) {
                        return self.outcome(effects);
                    }
                    self.state = State::AwaitRemoteBarrier {
                        target,
                        position: entry,
                        return_target,
                        return_position,
                        request,
                        ticket: None,
                    };
                }
            }
            State::AwaitReleaseAcknowledgement { .. } => {
                if frame.epoch != self.input_epoch
                    || !matches!(frame.message, Message::ReleaseAck)
                    || !self.accept_input_sequence(frame.sequence)
                {
                    self.fail(SourceFailure::InvalidReleaseAcknowledgement, &mut effects);
                } else {
                    self.complete_remote_release(&mut effects);
                }
            }
            _ => self.fail(SourceFailure::InvalidRemoteFrame, &mut effects),
        }
        self.outcome(effects)
    }

    /// Once held or home, a TakeBack the stall delayed past a reanchor is stale, not hostile. It
    /// is ignored when its epoch is neither older than the last one opened nor one never begun.
    fn late_take_back(&self, epoch: SessionEpoch) -> bool {
        (self.held_since.is_some()
            || matches!(
                self.state,
                State::Local { .. } | State::AwaitLocalBarrier { .. }
            ))
            && epoch <= self.input_epoch
            && self
                .inbound_input_epoch
                .is_none_or(|opened| epoch >= opened)
    }

    /// Sends `ReleaseAll` and waits for the receiver's post-delivery `ReleaseAck`.
    ///
    /// Native local routing remains disabled until that acknowledgement has been accepted.
    pub fn request_local(&mut self, now: Duration) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects) || !self.check_health(now, &mut effects) {
            return self.outcome(effects);
        }
        let (local, local_position) = match self.state {
            State::Remote {
                return_target,
                return_position,
                ..
            } => (return_target, return_position),
            _ => return self.outcome(effects),
        };
        log::info!("return: requested on this computer");
        self.begin_return_to(local, local_position, None, &mut effects);
        self.outcome(effects)
    }

    /// Opens an ownership transition and yields its epoch; a refusal fails the session.
    fn begin_ownership_transition(
        &mut self,
        target: PointerTarget,
        effects: &mut SourceEffects,
    ) -> Option<u64> {
        match self.ownership.begin_transition(target) {
            Ok(monhop_core::OwnershipCommand::BeginTransition { epoch, .. }) => Some(epoch),
            _ => {
                self.fail(SourceFailure::Ownership, effects);
                None
            }
        }
    }

    /// `entry_guard` is the home edge an edge return lands on; every other return leaves none.
    fn begin_return_to(
        &mut self,
        local: PointerTarget,
        local_position: Point,
        entry_guard: Option<(DisplayId, Edge)>,
        effects: &mut SourceEffects,
    ) {
        if let Some(owned) = self.floor_claim {
            match self.floor.transition(owned, FloorState::Returning) {
                Ok(returning) => self.floor_claim = Some(returning),
                Err(_) => {
                    self.fail(SourceFailure::Ownership, effects);
                    return;
                }
            }
        }
        let Some(ownership_epoch) = self.begin_ownership_transition(local, effects) else {
            return;
        };
        self.push_input(Message::ReleaseAll, effects);
        if self.failure.is_none() {
            self.entry_guard = entry_guard;
            self.state = State::AwaitReleaseAcknowledgement {
                local,
                local_position,
                ownership_epoch: Some(ownership_epoch),
            };
        }
    }

    /// Drives the base-epoch health challenge independently of pointer activation epochs.
    pub fn tick(&mut self, now: Duration) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects) {
            return self.outcome(effects);
        }
        if let Some(since) = self.held_since {
            if now.saturating_sub(since) >= HOLD_LIMIT {
                let failure = SourceFailure::PeerHealth(HealthError::DeadlineExpired);
                self.fail(failure, &mut effects);
                return self.outcome(effects);
            }
            match self.health.poll_while_held(now) {
                Ok(Some(token)) => self.push_control(Message::Ping(token), &mut effects),
                Ok(None) => {}
                Err(error) => self.fail(SourceFailure::PeerHealth(error), &mut effects),
            }
            return self.outcome(effects);
        }
        if self.should_retreat(now) {
            self.hold(now, &mut effects);
            return self.outcome(effects);
        }
        match self.health.poll(now) {
            Ok(Some(token)) => self.push_control(Message::Ping(token), &mut effects),
            Ok(None) => {}
            Err(HealthError::DeadlineExpired) => self.hold(now, &mut effects),
            Err(error) => self.fail(SourceFailure::PeerHealth(error), &mut effects),
        }
        self.outcome(effects)
    }

    fn on_pong(&mut self, token: u64, now: Duration, effects: &mut SourceEffects) {
        if self.held_since.is_some() {
            match self.health.receive_pong_while_held(token, now) {
                Ok(true) => {
                    self.hold_pong_seen = true;
                    self.try_resume(now);
                }
                Ok(false) => {}
                Err(error) => self.fail(SourceFailure::PeerHealth(error), effects),
            }
        } else if let Err(error) = self.health.receive_pong(token, now) {
            self.fail(SourceFailure::PeerHealth(error), effects);
        }
    }

    /// Remote input that has heard nothing fresh for RETREAT_AFTER comes home while the native
    /// lease is still valid, so a stall can never end in a terminal lease expiry.
    fn should_retreat(&self, now: Duration) -> bool {
        (self.remote_may_hold() || self.native_may_be_remote())
            && now.saturating_sub(self.health.last_response()) >= RETREAT_AFTER
    }

    /// The peer went silent: release everything it may hold, bring the pointer home, send the
    /// barrier, and wait for the link instead of ending the session.
    fn hold(&mut self, now: Duration, effects: &mut SourceEffects) {
        if let Err(error) = self.health.hold(now) {
            self.fail(SourceFailure::PeerHealth(error), effects);
            return;
        }
        log::warn!(
            "source held: no fresh reply for {} ms (mode {:?}, capture route remote={}); input returns local",
            now.saturating_sub(self.health.last_response()).as_millis(),
            self.mode(),
            self.capture_route.remote
        );
        let restore_pending = matches!(self.state, State::AwaitLocalBarrier { .. });
        let native_may_be_remote = self.native_may_be_remote();
        let restore = self.failure_restore_target();
        // A ReleaseAll the ordinary return path already sent is acknowledged before the barrier.
        let owed = u32::from(matches!(
            self.state,
            State::AwaitReleaseAcknowledgement { .. }
        ));
        if !self.recover_ownership_at(restore.0) {
            self.fail(SourceFailure::Ownership, effects);
            return;
        }
        self.push_input(Message::ReleaseAll, effects);
        if self.failure.is_some() {
            return;
        }
        self.release_acks_pending = self.release_acks_pending.saturating_add(owed + 1);
        self.remote_target = None;
        self.clear_remote_delivery();
        if restore_pending {
            // The native restore already in flight completes the return.
        } else if native_may_be_remote {
            let Some(request) = self.allocate_route_request(effects) else {
                return;
            };
            if !self.push_effect(
                SourceEffect::RestoreLocalAt {
                    request,
                    display: restore.0.display,
                    position: restore.1,
                },
                effects,
            ) {
                return;
            }
            self.state = State::AwaitLocalBarrier {
                local: restore.0,
                position: restore.1,
                request,
                ticket: None,
            };
        } else {
            self.state = State::Local {
                target: restore.0,
                cursor: Some(restore.1),
            };
        }
        self.held_since = Some(now);
        self.hold_pong_seen = false;
        self.holds = self.holds.saturating_add(1);
    }

    /// Ownership records the retreat as a timeout and settles on the restore target at once, so
    /// the next seam crossing after the hold can begin a transition.
    fn recover_ownership_at(&mut self, target: PointerTarget) -> bool {
        let Ok(plan) = self.ownership.on_timeout() else {
            return false;
        };
        if self.ownership.complete_recovery(plan.epoch).is_err() {
            return false;
        }
        match self.ownership.begin_transition(target) {
            Ok(monhop_core::OwnershipCommand::BeginTransition { epoch, .. }) => self
                .ownership
                .acknowledge_transition(TransitionAcknowledgement { target, epoch })
                .is_ok(),
            Err(monhop_core::OwnershipError::SameTarget) => true,
            _ => false,
        }
    }

    /// The hold ends once the receiver has consumed the barrier and answered a challenge sent
    /// after the hold began: both directions are proven, in order.
    fn try_resume(&mut self, now: Duration) {
        let Some(since) = self.held_since else {
            return;
        };
        if self.release_acks_pending > 0 || !self.hold_pong_seen {
            return;
        }
        if let Err(error) = self.health.resume(now) {
            self.failure.get_or_insert(SourceFailure::PeerHealth(error));
            return;
        }
        let held = now.saturating_sub(since);
        self.held_max = self.held_max.max(held);
        self.held_since = None;
        self.hold_pong_seen = false;
        log::info!("source resumed after {} ms", held.as_millis());
    }

    fn remote_may_hold(&self) -> bool {
        self.remote_target.is_some()
            || matches!(
                self.state,
                State::AwaitActivation { .. }
                    | State::AwaitRemoteBarrier { .. }
                    | State::Remote { .. }
                    | State::AwaitReleaseAcknowledgement { .. }
            )
    }

    fn native_may_be_remote(&self) -> bool {
        self.capture_route.remote
            || matches!(
                self.state,
                State::AwaitRemoteBarrier { .. }
                    | State::Remote { .. }
                    | State::AwaitReleaseAcknowledgement { .. }
                    | State::AwaitLocalBarrier { .. }
            )
    }

    fn on_route_changed(&mut self, remote: bool, revision: u64, effects: &mut SourceEffects) {
        match self.state {
            State::AwaitRemoteBarrier {
                target,
                position,
                return_target,
                return_position,
                ticket: Some(expected),
                ..
            } if remote && revision == expected => {
                self.capture_route = CaptureRoute { remote, revision };
                self.transfer_to_remote(effects);
                if self.failure.is_none() {
                    self.state = State::Remote {
                        target,
                        position,
                        return_target,
                        return_position,
                    };
                }
            }
            State::AwaitLocalBarrier {
                local,
                position,
                ticket: Some(expected),
                ..
            } if !remote && revision == expected => {
                self.capture_route = CaptureRoute { remote, revision };
                // The native restore placed the cursor at `position` before this barrier.
                self.anchored_return = true;
                self.state = State::Local {
                    target: local,
                    cursor: Some(position),
                };
            }
            _ => self.fail(SourceFailure::UnexpectedRouteBarrier, effects),
        }
    }

    fn apply_local(&mut self, input: NormalizedInput, effects: &mut SourceEffects) {
        match input {
            NormalizedInput::Key {
                usage,
                pressed,
                repeat,
                modifiers,
            } => match self.update_key(usage, pressed, repeat, modifiers) {
                Some(_) => {}
                None => self.fail(SourceFailure::InvalidKeyState, effects),
            },
            NormalizedInput::Button { button, pressed } => {
                self.update_button(button, pressed);
            }
            NormalizedInput::AbsoluteMotion(current) => self.apply_local_motion(current, effects),
            NormalizedInput::RelativeMotion(delta) => self.apply_local_relative(delta, effects),
            NormalizedInput::Scroll { .. } => {}
            NormalizedInput::RouteChanged { .. } => {
                unreachable!("route records are dispatched first")
            }
        }
    }

    fn apply_remote(&mut self, input: NormalizedInput, effects: &mut SourceEffects) {
        let forwarding = matches!(self.state, State::Remote { .. });
        match input {
            NormalizedInput::Key {
                usage,
                pressed,
                repeat,
                modifiers,
            } => {
                let Some(change) = self.update_key(usage, pressed, repeat, modifiers) else {
                    self.fail(SourceFailure::InvalidKeyState, effects);
                    return;
                };
                if !forwarding {
                    return;
                }
                let index = usize::from(usage.0);
                if pressed && !change.was_physical {
                    self.keys[index].remote = true;
                    let modifiers = self.remote_modifiers();
                    self.push_input(
                        Message::Key(Key {
                            usage,
                            is_down: true,
                            repeat: false,
                            modifiers,
                        }),
                        effects,
                    );
                } else if pressed && self.keys[index].remote && repeat {
                    self.push_input(
                        Message::Key(Key {
                            usage,
                            is_down: true,
                            repeat: true,
                            modifiers: self.remote_modifiers(),
                        }),
                        effects,
                    );
                } else if !pressed && change.was_remote {
                    self.keys[index].remote = false;
                    let modifiers = self.remote_modifiers();
                    self.push_input(
                        Message::Key(Key {
                            usage,
                            is_down: false,
                            repeat: false,
                            modifiers,
                        }),
                        effects,
                    );
                }
            }
            NormalizedInput::Button { button, pressed } => {
                let change = self.update_button(button, pressed);
                if !forwarding {
                    return;
                }
                let index = button.index();
                if pressed && !change.was_physical {
                    self.buttons[index].remote = true;
                    self.push_input(
                        Message::Button(Button {
                            button,
                            is_down: true,
                        }),
                        effects,
                    );
                } else if !pressed && change.was_remote {
                    self.buttons[index].remote = false;
                    self.push_input(
                        Message::Button(Button {
                            button,
                            is_down: false,
                        }),
                        effects,
                    );
                }
            }
            NormalizedInput::RelativeMotion(delta) if forwarding => {
                self.apply_remote_motion(delta, effects)
            }
            NormalizedInput::Scroll {
                horizontal,
                vertical,
            } if forwarding && (horizontal != 0.0 || vertical != 0.0) => self.push_input(
                Message::Scroll(monhop_protocol::Scroll {
                    horizontal,
                    vertical,
                }),
                effects,
            ),
            NormalizedInput::AbsoluteMotion(_)
            | NormalizedInput::RelativeMotion(_)
            | NormalizedInput::Scroll { .. } => {}
            NormalizedInput::RouteChanged { .. } => {
                unreachable!("route records are dispatched first")
            }
        }
    }

    /// Local input is anchored on an in-use display. A sample outside every in-use display (a
    /// display the layout marks not in use, or space past a clamped edge) counts as past the
    /// nearest edge of the display the pointer left, at the pointer's lateral position: a linked
    /// edge there crosses to the other computer, any other edge keeps input local at that edge.
    fn apply_local_motion(&mut self, current: Point, effects: &mut SourceEffects) {
        let State::Local { target, cursor } = self.state else {
            return;
        };
        let guarded = cursor.and_then(|cursor| self.guarded_entry_edge(target.display, cursor));
        if let Some((from, to, _, last)) = self.declined {
            let pressing = self.topology.display(from).is_ok_and(|display| {
                self.topology.links().iter().any(|link| {
                    link.from_display == from
                        && link.to_display == to
                        && at_physical_edge(display, current, link.from_edge)
                })
            });
            if !pressing {
                self.declined = Some((from, to, None, last));
            }
        }
        let local_target = match self.local_target_at(current) {
            Ok(target) => target,
            Err(()) => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        if let Some(current_target) =
            local_target.filter(|current_target| *current_target != target)
        {
            self.adopt_local_target(current_target, current, effects);
            return;
        }
        let (previous, anchor) = match local_target {
            Some(_) => (cursor, current),
            None => {
                let edge = self
                    .inside_display(target.display, current)
                    .unwrap_or(current);
                (Some(edge), edge)
            }
        };
        let transition = match previous {
            Some(previous) => {
                let current = self.held_at_entry_edge(target.display, current, guarded);
                match self
                    .topology
                    .transition_for_motion(target.display, previous, current)
                {
                    Ok(value) => value,
                    Err(_) => {
                        self.fail(SourceFailure::Topology, effects);
                        return;
                    }
                }
            }
            None => None,
        };
        match transition {
            Some(transition) => self.begin_edge_transition(target, transition, effects),
            None => {
                self.state = State::Local {
                    target,
                    cursor: Some(anchor),
                };
            }
        }
    }

    /// A polled OS pointer position is local input like a hook sample, taken only while input is
    /// routed locally on a settled display; in every other state it is ignored.
    pub fn observe_pointer(&mut self, point: Point, now: Duration) -> SourceOutcome {
        let mut effects = SourceEffects::default();
        if self.failure.is_some() || !point.is_finite() {
            return self.outcome(effects);
        }
        if !self.observe_time(now, &mut effects) || !self.check_health(now, &mut effects) {
            return self.outcome(effects);
        }
        if !self.capture_route.remote && matches!(self.state, State::Local { .. }) {
            let allowed = self.local_crossing_allowed();
            if self.floor.snapshot().state == FloorState::Free && self.rebase_pointer {
                // Only a fresh native poll rearms crossings after the inbound half lets go.
                if let Ok(Some(target)) = self.local_target_at(point) {
                    if let State::Local { target: old, .. } = self.state {
                        if old != target {
                            self.adopt_local_target(target, point, &mut effects);
                        } else {
                            self.state = State::Local {
                                target,
                                cursor: Some(point),
                            };
                        }
                    }
                } else if let State::Local { target, .. } = self.state {
                    self.state = State::Local {
                        target,
                        cursor: self.inside_display(target.display, point),
                    };
                }
                self.rebase_pointer = false;
            } else if allowed {
                self.apply_local_motion(point, &mut effects);
            }
        }
        self.outcome(effects)
    }

    /// Resolves only in-use displays hosted by this machine, using half-open bounds. Remote
    /// displays commonly reuse logical origins, so considering them here would silently
    /// misclassify a local absolute sample. Overlapping local displays are rejected, not guessed.
    fn local_target_at(&self, point: Point) -> Result<Option<PointerTarget>, ()> {
        let mut found = None;
        for display in self
            .topology
            .displays()
            .filter(|display| display.machine == self.home.machine && display.in_use)
        {
            if !display.bounds().contains_half_open(point) {
                continue;
            }
            if found.is_some() {
                return Err(());
            }
            found = Some(PointerTarget::new(self.home.machine, display.id));
        }
        Ok(found)
    }

    fn adopt_local_target(
        &mut self,
        target: PointerTarget,
        cursor: Point,
        effects: &mut SourceEffects,
    ) {
        let Some(ownership_epoch) = self.begin_ownership_transition(target, effects) else {
            return;
        };
        if self
            .ownership
            .acknowledge_transition(TransitionAcknowledgement {
                target,
                epoch: ownership_epoch,
            })
            .is_err()
        {
            self.fail(SourceFailure::Ownership, effects);
            return;
        }
        self.state = State::Local {
            target,
            cursor: Some(cursor),
        };
    }

    fn begin_edge_transition(
        &mut self,
        from: PointerTarget,
        transition: EdgeTransition,
        effects: &mut SourceEffects,
    ) {
        let machine = match self.topology.machine_for_display(transition.to_display) {
            Ok(machine) => machine.id,
            Err(_) => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        let target = PointerTarget::new(machine, transition.to_display);
        if target.machine == from.machine {
            self.adopt_local_target(target, transition.entry_point, effects);
            return;
        }
        let return_position = match self.inside_display(from.display, transition.crossing_point) {
            Some(position) => position,
            None => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        // The other computer cannot take input while the link is down, and capture that cannot
        // suppress yet cannot hand input over: either way the seam is a wall.
        if self.held_since.is_some() || !self.capture_ready {
            self.state = State::Local {
                target: from,
                cursor: Some(return_position),
            };
            return;
        }
        if !self.local_crossing_allowed() {
            return;
        }
        if let Some((from_id, to_id, since, last)) = self.declined
            && from_id == from.display
            && to_id == target.display
        {
            let retry = monhop_core::capture::DECLINE_RETRY_AFTER;
            let since = if self.last_now.saturating_sub(last) > retry {
                self.last_now
            } else {
                since.unwrap_or(self.last_now)
            };
            self.declined = Some((from_id, to_id, Some(since), self.last_now));
            if self.last_now.saturating_sub(since) < retry {
                return;
            }
        }
        let snapshot = self.floor.snapshot();
        match self.floor.transition(snapshot, FloorState::Requesting) {
            Ok(requesting) => self.floor_claim = Some(requesting),
            Err(_) => return,
        }
        self.declined = None;
        self.begin_remote_activation(target, transition, from, return_position, false, effects);
    }

    fn begin_remote_activation(
        &mut self,
        target: PointerTarget,
        transition: EdgeTransition,
        return_target: PointerTarget,
        return_position: Point,
        capture_already_remote: bool,
        effects: &mut SourceEffects,
    ) {
        let Some(ownership_epoch) = self.begin_ownership_transition(target, effects) else {
            return;
        };
        let Some(next_epoch) = self.next_input_epoch() else {
            self.fail(SourceFailure::ProtocolEpochExhausted, effects);
            return;
        };
        self.input_epoch = next_epoch;
        self.outbound_input_sequence = 0;
        let entry = transition.entry_point;
        self.entry_guard = Some((target.display, transition.to_edge));
        self.state = State::AwaitActivation {
            target,
            entry,
            return_target,
            return_position,
            ownership_epoch,
            capture_already_remote,
        };
        self.push_input(
            Message::ActivateDisplayAt {
                display_id: target.display,
                position: entry,
            },
            effects,
        );
    }

    fn apply_local_relative(&mut self, delta: Point, effects: &mut SourceEffects) {
        let State::Local {
            target,
            cursor: Some(anchor),
        } = self.state
        else {
            return;
        };
        let guarded = self.guarded_entry_edge(target.display, anchor);
        let transition = match self.relative_edge_transition(target.display, anchor, delta, guarded)
        {
            Ok(transition) => transition,
            Err(()) => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        if let Some(transition) = transition {
            self.begin_edge_transition(target, transition, effects);
        }
    }

    fn relative_edge_transition(
        &self,
        display_id: DisplayId,
        anchor: Point,
        delta: Point,
        guarded: Option<Edge>,
    ) -> Result<Option<EdgeTransition>, ()> {
        let display = self.topology.display(display_id).map_err(|_| ())?;
        let bounds = display.bounds();
        for edge in [Edge::Right, Edge::Left, Edge::Bottom, Edge::Top] {
            if guarded == Some(edge)
                || !outward(edge, delta)
                || !at_physical_edge(display, anchor, edge)
            {
                continue;
            }
            let hysteresis = self
                .topology
                .links()
                .iter()
                .filter(|link| link.from_display == display_id && link.from_edge == edge)
                .map(|link| link.hysteresis)
                .fold(None, |largest: Option<f64>, value| {
                    Some(largest.map_or(value, |previous| previous.max(value)))
                });
            let Some(hysteresis) = hysteresis else {
                continue;
            };
            let projected = project_past_edge(bounds, anchor, edge, hysteresis);
            if let Some(transition) = self
                .topology
                .transition_for_motion(display_id, anchor, projected)
                .map_err(|_| ())?
            {
                return Ok(Some(transition));
            }
        }
        Ok(None)
    }

    /// The entry edge `position` may not cross yet. The guard is dropped for good once the
    /// pointer is on another display or has been [`ENTRY_GUARD_DISTANCE`] away from that edge.
    fn guarded_entry_edge(&mut self, display_id: DisplayId, position: Point) -> Option<Edge> {
        let (guarded, edge) = self.entry_guard?;
        let holding = guarded == display_id
            && self.topology.display(display_id).is_ok_and(|display| {
                distance_inside(display.bounds(), position, edge) < ENTRY_GUARD_DISTANCE
            });
        if !holding {
            self.entry_guard = None;
        }
        holding.then_some(edge)
    }

    /// Stops `point` on a guarded edge line, so only motion through the other edges can cross.
    fn held_at_entry_edge(
        &self,
        display_id: DisplayId,
        point: Point,
        guarded: Option<Edge>,
    ) -> Point {
        let (Some(edge), Ok(display)) = (guarded, self.topology.display(display_id)) else {
            return point;
        };
        let bounds = display.bounds();
        match edge {
            Edge::Left => Point::new(point.x.max(bounds.origin.x), point.y),
            Edge::Right => Point::new(point.x.min(bounds.max_x()), point.y),
            Edge::Top => Point::new(point.x, point.y.max(bounds.origin.y)),
            Edge::Bottom => Point::new(point.x, point.y.min(bounds.max_y())),
        }
    }

    fn inside_display(&self, display_id: DisplayId, point: Point) -> Option<Point> {
        let display = self.topology.display(display_id).ok()?;
        let bounds = display.bounds();
        // A local native cursor never reaches the half-open logical maximum. Preserve a position
        // that its OS can restore: the last physical pixel, expressed in topology coordinates.
        let max_x = bounds.max_x() - bounds.size.width / f64::from(display.native_size.width);
        let max_y = bounds.max_y() - bounds.size.height / f64::from(display.native_size.height);
        Some(Point::new(
            point.x.clamp(bounds.origin.x, max_x),
            point.y.clamp(bounds.origin.y, max_y),
        ))
    }

    fn apply_remote_motion(&mut self, delta: Point, effects: &mut SourceEffects) {
        let State::Remote {
            target,
            position,
            return_target,
            return_position,
        } = self.state
        else {
            return;
        };
        let display = match self.topology.display(target.display) {
            Ok(display) => display,
            Err(_) => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        let bounds = display.bounds();
        let desired = Point::new(position.x + delta.x, position.y + delta.y);
        if !desired.is_finite() {
            self.fail(SourceFailure::InvalidCapturedInput, effects);
            return;
        }
        let guarded = self.guarded_entry_edge(target.display, position);
        let desired = self.held_at_entry_edge(target.display, desired, guarded);
        let transition =
            match self
                .topology
                .transition_for_motion(target.display, position, desired)
            {
                Ok(transition) => transition,
                Err(_) => {
                    self.fail(SourceFailure::Topology, effects);
                    return;
                }
            };
        if let Some(transition) = transition {
            self.apply_remote_edge(target, return_target, return_position, transition, effects);
            return;
        }
        let next = Point::new(
            desired.x.clamp(bounds.origin.x, bounds.max_x().next_down()),
            desired.y.clamp(bounds.origin.y, bounds.max_y().next_down()),
        );
        self.state = State::Remote {
            target,
            position: next,
            return_target,
            return_position,
        };
        // The receiver gets the tracked position itself: a relative echo re-added to its own
        // anchor can round one ulp past the edge this side just clamped to.
        if next != position {
            self.push_input(Message::Motion(Motion::Absolute(next)), effects);
        }
    }

    fn apply_remote_edge(
        &mut self,
        from: PointerTarget,
        return_target: PointerTarget,
        return_position: Point,
        transition: EdgeTransition,
        effects: &mut SourceEffects,
    ) {
        let machine = match self.topology.machine_for_display(transition.to_display) {
            Ok(machine) => machine.id,
            Err(_) => {
                self.fail(SourceFailure::Topology, effects);
                return;
            }
        };
        let target = PointerTarget::new(machine, transition.to_display);
        if target.machine == self.home.machine {
            log::info!("return: the pointer crossed back over the edge");
            self.begin_return_to(
                target,
                transition.entry_point,
                Some((target.display, transition.to_edge)),
                effects,
            );
        } else if target.machine == from.machine {
            self.begin_remote_activation(
                target,
                transition,
                return_target,
                return_position,
                true,
                effects,
            );
        } else {
            self.fail(SourceFailure::Topology, effects);
        }
    }

    fn transfer_to_remote(&mut self, effects: &mut SourceEffects) {
        for index in 0xe0..=0xe7 {
            if self.keys[index].physical && !self.keys[index].remote {
                self.keys[index].remote = true;
                let modifiers = self.remote_modifiers();
                self.push_input(
                    Message::Key(Key {
                        usage: HidUsage(index as u16),
                        is_down: true,
                        repeat: false,
                        modifiers,
                    }),
                    effects,
                );
                if self.failure.is_some() {
                    return;
                }
            }
        }
        for index in 0..self.buttons.len() {
            if self.buttons[index].physical && !self.buttons[index].remote {
                self.buttons[index].remote = true;
                self.push_input(
                    Message::Button(Button {
                        button: button_from_index(index),
                        is_down: true,
                    }),
                    effects,
                );
                if self.failure.is_some() {
                    return;
                }
            }
        }
    }

    /// Applies only changes that arrived while a same-peer display activation was in flight.
    ///
    /// The receiver deliberately preserves its existing pressed state across this activation.
    /// Releases must therefore be sent in the new epoch, while already-delivered held input is
    /// left untouched. A bounded preflight turns an unrepresentable batch into one fail-closed
    /// cleanup path before any partial release is emitted.
    fn synchronize_reanchored_remote_input(&mut self, effects: &mut SourceEffects) {
        let key_changes = self
            .keys
            .iter()
            .filter(|state| state.remote != state.physical)
            .count();
        let button_changes = self
            .buttons
            .iter()
            .filter(|state| state.remote != state.physical)
            .count();
        if key_changes + button_changes > MAX_SOURCE_EFFECTS - effects.len {
            self.fail(SourceFailure::EffectCapacityExceeded, effects);
            return;
        }

        // Release ordinary keys before modifiers, then use post-event modifier state for every
        // modifier release. That matches the receiver's strict post-event mask validation.
        for index in 0..self.keys.len() {
            if (0xe0..=0xe7).contains(&index)
                || !self.keys[index].remote
                || self.keys[index].physical
            {
                continue;
            }
            self.keys[index].remote = false;
            self.push_input(
                Message::Key(Key {
                    usage: HidUsage(index as u16),
                    is_down: false,
                    repeat: false,
                    modifiers: self.remote_modifiers(),
                }),
                effects,
            );
        }
        for index in 0xe0..=0xe7 {
            if !self.keys[index].remote || self.keys[index].physical {
                continue;
            }
            self.keys[index].remote = false;
            self.push_input(
                Message::Key(Key {
                    usage: HidUsage(index as u16),
                    is_down: false,
                    repeat: false,
                    modifiers: self.remote_modifiers(),
                }),
                effects,
            );
        }
        for index in 0..self.buttons.len() {
            if !self.buttons[index].remote || self.buttons[index].physical {
                continue;
            }
            self.buttons[index].remote = false;
            self.push_input(
                Message::Button(Button {
                    button: button_from_index(index),
                    is_down: false,
                }),
                effects,
            );
        }

        // A new press while the previous remote route was awaiting its acknowledgement is still
        // a fresh remote press. Add modifiers first so each subsequent key reports the receiver's
        // actual delivered modifier state.
        for index in 0xe0..=0xe7 {
            if !self.keys[index].physical || self.keys[index].remote {
                continue;
            }
            self.keys[index].remote = true;
            self.push_input(
                Message::Key(Key {
                    usage: HidUsage(index as u16),
                    is_down: true,
                    repeat: false,
                    modifiers: self.remote_modifiers(),
                }),
                effects,
            );
        }
        for index in 0..self.keys.len() {
            if (0xe0..=0xe7).contains(&index)
                || !self.keys[index].physical
                || self.keys[index].remote
            {
                continue;
            }
            self.keys[index].remote = true;
            self.push_input(
                Message::Key(Key {
                    usage: HidUsage(index as u16),
                    is_down: true,
                    repeat: false,
                    modifiers: self.remote_modifiers(),
                }),
                effects,
            );
        }
        for index in 0..self.buttons.len() {
            if !self.buttons[index].physical || self.buttons[index].remote {
                continue;
            }
            self.buttons[index].remote = true;
            self.push_input(
                Message::Button(Button {
                    button: button_from_index(index),
                    is_down: true,
                }),
                effects,
            );
        }
    }

    fn complete_remote_release(&mut self, effects: &mut SourceEffects) {
        let State::AwaitReleaseAcknowledgement {
            local,
            local_position,
            ownership_epoch,
            ..
        } = self.state
        else {
            self.fail(SourceFailure::InvalidReleaseAcknowledgement, effects);
            return;
        };
        if let Some(epoch) = ownership_epoch
            && self
                .ownership
                .acknowledge_transition(TransitionAcknowledgement {
                    target: local,
                    epoch,
                })
                .is_err()
        {
            self.fail(SourceFailure::Ownership, effects);
            return;
        }
        self.remote_target = None;
        self.clear_remote_delivery();
        if self.capture_route.remote {
            let Some(request) = self.allocate_route_request(effects) else {
                return;
            };
            if !self.push_effect(
                SourceEffect::RestoreLocalAt {
                    request,
                    display: local.display,
                    position: local_position,
                },
                effects,
            ) {
                return;
            }
            self.state = State::AwaitLocalBarrier {
                local,
                position: local_position,
                request,
                ticket: None,
            };
        } else {
            self.state = State::Local {
                target: local,
                cursor: Some(local_position),
            };
        }
    }

    fn clear_remote_delivery(&mut self) {
        for state in &mut self.keys {
            state.remote = false;
        }
        for state in &mut self.buttons {
            state.remote = false;
        }
    }

    fn update_key(
        &mut self,
        usage: HidUsage,
        pressed: bool,
        repeat: bool,
        modifiers: ModifierState,
    ) -> Option<KeyChange> {
        if !usage.is_valid() {
            return None;
        }
        let index = usize::from(usage.0);
        let state = &mut self.keys[index];
        let change = KeyChange {
            was_physical: state.physical,
            was_remote: state.remote,
        };
        if pressed {
            if state.physical {
                if !repeat {
                    return None;
                }
            } else {
                if repeat {
                    return None;
                }
                state.physical = true;
            }
        } else if repeat {
            return None;
        } else if state.physical {
            state.physical = false;
        } else {
            return Some(KeyChange {
                was_physical: false,
                was_remote: false,
            });
        }
        (self.physical_modifiers() == modifiers).then_some(change)
    }

    fn update_button(&mut self, button: MouseButton, pressed: bool) -> ButtonChange {
        let state = &mut self.buttons[button.index()];
        let change = ButtonChange {
            was_physical: state.physical,
            was_remote: state.remote,
        };
        state.physical = pressed;
        change
    }

    fn physical_modifiers(&self) -> ModifierState {
        let mut mask = 0;
        for index in 0xe0..=0xe7 {
            if self.keys[index].physical {
                mask |= modifier_mask(index as u16);
            }
        }
        ModifierState(mask)
    }

    fn remote_modifiers(&self) -> ModifierState {
        let mut mask = 0;
        for index in 0xe0..=0xe7 {
            if self.keys[index].remote {
                mask |= modifier_mask(index as u16);
            }
        }
        ModifierState(mask)
    }

    fn observe_time(&mut self, now: Duration, effects: &mut SourceEffects) -> bool {
        if now < self.last_now {
            self.fail(SourceFailure::ClockRegression, effects);
            return false;
        }
        self.last_now = now;
        true
    }

    fn check_health(&mut self, now: Duration, effects: &mut SourceEffects) -> bool {
        if self.held_since.is_some() {
            return true;
        }
        match self.health.check(now) {
            Ok(()) => true,
            Err(HealthError::DeadlineExpired) => {
                self.hold(now, effects);
                self.failure.is_none()
            }
            Err(error) => {
                self.fail(SourceFailure::PeerHealth(error), effects);
                false
            }
        }
    }

    fn activate_input_epoch(&mut self, epoch: SessionEpoch) -> bool {
        if self
            .inbound_input_epoch
            .is_some_and(|active| epoch <= active)
        {
            return false;
        }
        self.inbound_input_epoch = Some(epoch);
        self.inbound_input_sequence = None;
        true
    }

    fn accept_input_sequence(&mut self, sequence: u64) -> bool {
        if self.inbound_input_epoch != Some(self.input_epoch)
            || self
                .inbound_input_sequence
                .is_some_and(|last| sequence <= last)
        {
            return false;
        }
        self.inbound_input_sequence = Some(sequence);
        true
    }

    fn next_input_epoch(&self) -> Option<SessionEpoch> {
        self.input_epoch
            .get()
            .checked_add(1)
            .and_then(|value| SessionEpoch::new(value).ok())
    }

    fn allocate_route_request(&mut self, effects: &mut SourceEffects) -> Option<RouteRequest> {
        let Some(request) = advance_counter(&mut self.next_route_request) else {
            self.fail(SourceFailure::RouteRequestExhausted, effects);
            return None;
        };
        Some(RouteRequest(request))
    }

    fn push_input(&mut self, mut message: Message, effects: &mut SourceEffects) {
        match &mut message {
            Message::ActivateDisplayAt { position, .. }
            | Message::Motion(Motion::Absolute(position)) => {
                position.x -= self.peer_offset.x;
                position.y -= self.peer_offset.y;
            }
            _ => {}
        }
        let Some(sequence) = advance_counter(&mut self.outbound_input_sequence) else {
            self.fail(SourceFailure::SequenceExhausted, effects);
            return;
        };
        self.push_effect(
            SourceEffect::RemoteFrame(Frame::new(self.input_epoch, sequence, message)),
            effects,
        );
    }

    fn push_control(&mut self, message: Message, effects: &mut SourceEffects) {
        let Some(sequence) = advance_counter(&mut self.outbound_control_sequence) else {
            self.fail(SourceFailure::SequenceExhausted, effects);
            return;
        };
        self.push_effect(
            SourceEffect::RemoteFrame(Frame::new(self.base_epoch, sequence, message)),
            effects,
        );
    }

    fn push_effect(&mut self, effect: SourceEffect, effects: &mut SourceEffects) -> bool {
        if effects.push(effect).is_err() {
            self.fail(SourceFailure::EffectCapacityExceeded, effects);
            return false;
        }
        true
    }

    fn fail(&mut self, failure: SourceFailure, effects: &mut SourceEffects) {
        if self.failure.is_some() {
            return;
        }
        let remote_may_hold = self.remote_may_hold();
        let native_may_be_remote = self.native_may_be_remote();
        let restore = self.failure_restore_target();
        log::warn!(
            "source controller failed: {failure:?} (mode {:?}, capture route remote={})",
            self.mode(),
            self.capture_route.remote
        );
        self.failure = Some(failure);
        self.state = State::Failed;
        self.entry_guard = None;
        let _ = self.ownership.on_timeout();
        if remote_may_hold {
            let sequence = self.outbound_input_sequence;
            if let Some(next) = sequence.checked_add(1) {
                self.outbound_input_sequence = next;
                let _ = effects.push(SourceEffect::RemoteFrame(Frame::new(
                    self.input_epoch,
                    sequence,
                    Message::ReleaseAll,
                )));
            }
        }
        self.remote_target = None;
        self.clear_remote_delivery();
        if native_may_be_remote {
            let request = self.next_route_request;
            if let Some(next) = request.checked_add(1) {
                self.next_route_request = next;
                self.failed_route_request = Some(RouteRequest(request));
                let _ = effects.push(SourceEffect::RestoreLocalAt {
                    request: RouteRequest(request),
                    display: restore.0.display,
                    position: restore.1,
                });
            }
        }
    }

    fn failure_restore_target(&self) -> (PointerTarget, Point) {
        match self.state {
            State::Local { target, cursor } => (
                target,
                cursor.unwrap_or_else(|| self.display_origin(target)),
            ),
            State::AwaitActivation {
                return_target,
                return_position,
                ..
            }
            | State::AwaitRemoteBarrier {
                return_target,
                return_position,
                ..
            }
            | State::Remote {
                return_target,
                return_position,
                ..
            } => (return_target, return_position),
            State::AwaitReleaseAcknowledgement {
                local,
                local_position,
                ..
            } => (local, local_position),
            State::AwaitLocalBarrier {
                local, position, ..
            } => (local, position),
            State::Failed => (self.home, self.display_origin(self.home)),
        }
    }

    fn display_origin(&self, target: PointerTarget) -> Point {
        self.topology
            .display(target.display)
            .map(|display| display.origin)
            .unwrap_or_default()
    }

    fn outcome(&mut self, mut effects: SourceEffects) -> SourceOutcome {
        if self.failure.is_none() {
            if self.take_back {
                if let State::Remote {
                    return_target,
                    return_position,
                    ..
                } = self.state
                {
                    self.take_back = false;
                    log::info!("return: the other computer took control back");
                    self.begin_return_to(return_target, return_position, None, &mut effects);
                } else if matches!(self.state, State::Local { .. }) {
                    self.take_back = false;
                }
            }
            if matches!(self.state, State::Local { .. }) && self.release_acks_pending == 0 {
                self.release_floor();
            }
        }
        // Motion is untracked while the floor is held, so an anchor is fresh only within its call.
        self.anchored_return = false;
        SourceOutcome {
            effects,
            failure: self.failure,
        }
    }
}

#[derive(Clone, Copy)]
struct KeyChange {
    was_physical: bool,
    was_remote: bool,
}

#[derive(Clone, Copy)]
struct ButtonChange {
    was_physical: bool,
    was_remote: bool,
}

fn valid_point(point: Point) -> bool {
    point.is_finite()
        && point.x.abs() <= MAX_LOGICAL_ORIGIN_ABS
        && point.y.abs() <= MAX_LOGICAL_ORIGIN_ABS
}

/// Yields the value to use and advances the counter, leaving it untouched once exhausted.
fn advance_counter(counter: &mut u64) -> Option<u64> {
    let current = *counter;
    *counter = current.checked_add(1)?;
    Some(current)
}

fn outward(edge: Edge, delta: Point) -> bool {
    match edge {
        Edge::Left => delta.x < 0.0,
        Edge::Right => delta.x > 0.0,
        Edge::Top => delta.y < 0.0,
        Edge::Bottom => delta.y > 0.0,
    }
}

fn at_physical_edge(display: &monhop_core::Display, point: Point, edge: Edge) -> bool {
    let bounds = display.bounds();
    let x_step = bounds.size.width / f64::from(display.native_size.width);
    let y_step = bounds.size.height / f64::from(display.native_size.height);
    match edge {
        Edge::Left => point.x <= bounds.origin.x + x_step,
        Edge::Right => point.x >= bounds.max_x() - x_step,
        Edge::Top => point.y <= bounds.origin.y + y_step,
        Edge::Bottom => point.y >= bounds.max_y() - y_step,
    }
}

/// Perpendicular distance from `edge`'s line into the display; negative past it.
fn distance_inside(bounds: monhop_core::LogicalRect, point: Point, edge: Edge) -> f64 {
    match edge {
        Edge::Left => point.x - bounds.origin.x,
        Edge::Right => bounds.max_x() - point.x,
        Edge::Top => point.y - bounds.origin.y,
        Edge::Bottom => bounds.max_y() - point.y,
    }
}

fn project_past_edge(
    bounds: monhop_core::LogicalRect,
    anchor: Point,
    edge: Edge,
    hysteresis: f64,
) -> Point {
    let beyond = hysteresis + 1.0;
    match edge {
        Edge::Left => Point::new(bounds.origin.x - beyond, anchor.y),
        Edge::Right => Point::new(bounds.max_x() + beyond, anchor.y),
        Edge::Top => Point::new(anchor.x, bounds.origin.y - beyond),
        Edge::Bottom => Point::new(anchor.x, bounds.max_y() + beyond),
    }
}

fn modifier_mask(usage: u16) -> u8 {
    match usage {
        0xe0 => ModifierState::LEFT_CONTROL,
        0xe1 => ModifierState::LEFT_SHIFT,
        0xe2 => ModifierState::LEFT_ALT,
        0xe3 => ModifierState::LEFT_META,
        0xe4 => ModifierState::RIGHT_CONTROL,
        0xe5 => ModifierState::RIGHT_SHIFT,
        0xe6 => ModifierState::RIGHT_ALT,
        0xe7 => ModifierState::RIGHT_META,
        _ => 0,
    }
}

fn button_from_index(index: usize) -> MouseButton {
    MouseButton::from_index(index).expect("five fixed button slots")
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use monhop_core::{Display, LogicalSize, Machine, NativeSize, Platform};

    fn topology() -> Topology {
        Topology::new(
            vec![Machine::new(DeviceId([1; 16]), Platform::Windows)],
            vec![Display::new(
                DisplayId(1),
                DeviceId([1; 16]),
                "fixture".into(),
                NativeSize::new(100, 100),
                LogicalSize::new(100.0, 100.0),
                Point::default(),
                1.0,
                None,
                true,
            )],
            Vec::new(),
        )
        .unwrap()
    }
    fn control(now: Duration) -> ReadyControl {
        let mut health = PeerHealth::new(Duration::ZERO);
        health.poll(Duration::ZERO).unwrap();
        health.receive_pong(1, Duration::ZERO).unwrap();
        health.poll(Duration::from_millis(30)).unwrap();
        ReadyControl {
            epoch: SessionEpoch::new(3).unwrap(),
            next_incoming: 7,
            next_outgoing: 9,
            next_heartbeat_out: 9,
            last_heartbeat_in: Some(6),
            health,
            limiter: RateLimiter::new(2).unwrap(),
            now,
        }
    }

    #[test]
    fn source_continuation_keeps_pending_challenge_sequences_and_rate_credit() {
        let now = Duration::from_millis(31);
        let mut source = SourceController::after_startup(
            topology(),
            DeviceId([1; 16]),
            DisplayId(1),
            control(now),
        )
        .unwrap();
        let epoch = SessionEpoch::new(3).unwrap();
        assert!(
            source
                .on_remote_frame(&Frame::new(epoch, 7, Message::Pong(2)), now)
                .failure
                .is_none()
        );
        let outcome = source.on_remote_frame(&Frame::new(epoch, 8, Message::Ping(55)), now);
        assert!(outcome.failure.is_none());
        assert!(outcome.effects.iter().any(|effect| matches!(effect, SourceEffect::RemoteFrame(frame) if frame.sequence == 9 && frame.message == Message::Pong(55))));
        assert_eq!(
            source
                .on_remote_frame(&Frame::new(epoch, 9, Message::Ping(56)), now)
                .failure,
            Some(SourceFailure::RateLimited)
        );
    }

    #[test]
    fn source_continuation_cannot_refresh_expired_health_or_replay_a_heartbeat() {
        assert!(matches!(
            SourceController::after_startup(
                topology(),
                DeviceId([1; 16]),
                DisplayId(1),
                control(crate::session_health::PEER_LIVENESS)
            ),
            Err(SourceFailure::PeerHealth(HealthError::DeadlineExpired))
        ));
        let now = Duration::from_millis(31);
        let mut source = SourceController::after_startup(
            topology(),
            DeviceId([1; 16]),
            DisplayId(1),
            control(now),
        )
        .unwrap();
        let epoch = SessionEpoch::new(3).unwrap();
        let replay = source.on_remote_frame(&Frame::new(epoch, 6, Message::Pong(2)), now);
        assert_eq!(replay.failure, None);
        assert!(replay.effects.is_empty());
        let deadline = now + crate::session_health::PEER_LIVENESS;
        let held = source.tick(deadline);
        assert_eq!(held.failure, None);
        assert_eq!(source.held_since(), Some(deadline));
        assert!(held.effects.iter().any(|effect| matches!(
            effect,
            SourceEffect::RemoteFrame(Frame {
                message: Message::ReleaseAll,
                ..
            })
        )));
    }
}
