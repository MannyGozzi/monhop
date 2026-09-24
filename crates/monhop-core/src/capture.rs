//! Bounded, callback-safe capture primitives shared by the explicit native adapters.
//!
//! This module does not install hooks, suppress input, start a thread, or retain a global
//! instance. The native adapter owns those effects and decides whether a successfully queued
//! event should be suppressed before it returns from the callback that produced it.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::{
    GestureKind, GesturePhase, HidUsage, ModifierState, MouseButton, PointerGesture,
    RevocationSignal, SystemGesture,
};

/// The fixed number of events retained by a capture channel.
pub const CAPTURE_QUEUE_CAPACITY: usize = 512;

/// Windows wheel units per detent. [`CaptureEvent::Scroll`] stores values in these units.
pub const WHEEL_UNITS_PER_DETENT: i32 = 120;

/// A suppression lease may be renewed for at most this long.
pub const MAX_SUPPRESSION_TTL: Duration = Duration::from_millis(120);

/// A seam whose crossing the peer declined is retried only after this much continued pressure.
pub const DECLINE_RETRY_AFTER: Duration = Duration::from_millis(50);

static NATIVE_INPUT_OWNED: AtomicBool = AtomicBool::new(false);

/// Clears the process-wide flag when the claim and every permit split from it have dropped.
struct ClaimInner;

impl Drop for ClaimInner {
    fn drop(&mut self) {
        NATIVE_INPUT_OWNED.store(false, Ordering::Release);
    }
}

/// Process-wide ownership of native input effects for one session.
///
/// Capture and injection each hold their permit from before their first native side effect until
/// all input they suppressed or injected is known to be released.
#[must_use]
pub struct NativeSessionClaim {
    inner: Arc<ClaimInner>,
}

impl fmt::Debug for NativeSessionClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSessionClaim")
    }
}

impl NativeSessionClaim {
    pub fn is_claimed() -> bool {
        NATIVE_INPUT_OWNED.load(Ordering::Acquire)
    }

    /// Claims exclusive native input ownership for this process.
    pub fn claim() -> Option<Self> {
        NATIVE_INPUT_OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                inner: Arc::new(ClaimInner),
            })
    }

    pub fn split(self) -> (CapturePermit, InjectionPermit) {
        (
            CapturePermit {
                _claim: Arc::clone(&self.inner),
            },
            InjectionPermit { _claim: self.inner },
        )
    }
}

/// The native capture's share of a [`NativeSessionClaim`].
#[must_use]
pub struct CapturePermit {
    _claim: Arc<ClaimInner>,
}

impl fmt::Debug for CapturePermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturePermit")
    }
}

/// The native injector's share of a [`NativeSessionClaim`].
#[must_use]
pub struct InjectionPermit {
    _claim: Arc<ClaimInner>,
}

impl fmt::Debug for InjectionPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InjectionPermit")
    }
}

/// A copied physical input event.
///
/// `RelativeMotion` is expressed in unscaled raw-input counts. `AbsoluteMotion` is expressed in
/// physical virtual-desktop pixels. The two coordinate systems are never converted here.
/// `Scroll` is expressed in Windows wheel units, where one detent is 120 units. A `Button` is
/// stamped `at` [`crate::clicks::capture_clock`] when the capture thread received it.
///
/// The `Logical*` variants preserve macOS Quartz logical desktop points. `LogicalScroll` is
/// normalized to logical points by the macOS adapter: continuous point deltas pass through and
/// discrete line deltas use its explicit line-to-point conversion. They are intentionally distinct
/// from the Windows integer variants; no adapter may round one representation into the other here.
///
/// `Gesture` holds a [`PointerGesture`]'s parts and is valid exactly when they form one.
#[derive(Clone, Copy, PartialEq)]
pub enum CaptureEvent {
    Key {
        usage: HidUsage,
        pressed: bool,
        repeat: bool,
        modifiers: ModifierState,
    },
    Button {
        button: MouseButton,
        pressed: bool,
        at: Duration,
    },
    AbsoluteMotion {
        x: i32,
        y: i32,
    },
    RelativeMotion {
        dx: i32,
        dy: i32,
    },
    Scroll {
        horizontal: i32,
        vertical: i32,
    },
    /// A macOS Quartz cursor position in logical desktop points.
    LogicalAbsoluteMotion {
        x: f64,
        y: f64,
    },
    /// A macOS Quartz cursor delta in logical desktop points.
    LogicalRelativeMotion {
        dx: f64,
        dy: f64,
    },
    /// A macOS Quartz scroll delta in normalized logical desktop points.
    LogicalScroll {
        horizontal: f64,
        vertical: f64,
    },
    /// FIFO cutover barrier. The producer admits this before events for the new route.
    RouteChanged {
        remote: bool,
        revision: u64,
    },
    Gesture {
        kind: GestureKind,
        phase: Option<GesturePhase>,
        value: f64,
    },
    SystemGesture(SystemGesture),
}

impl fmt::Debug for CaptureEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Key { .. } => "Key",
            Self::Button { .. } => "Button",
            Self::AbsoluteMotion { .. } => "AbsoluteMotion",
            Self::RelativeMotion { .. } => "RelativeMotion",
            Self::Scroll { .. } => "Scroll",
            Self::LogicalAbsoluteMotion { .. } => "LogicalAbsoluteMotion",
            Self::LogicalRelativeMotion { .. } => "LogicalRelativeMotion",
            Self::LogicalScroll { .. } => "LogicalScroll",
            Self::RouteChanged { .. } => "RouteChanged",
            Self::Gesture { .. } => "Gesture",
            Self::SystemGesture(_) => "SystemGesture",
        };
        write!(formatter, "CaptureEvent::{name}([redacted])")
    }
}

impl CaptureEvent {
    /// Returns whether this event has a representable physical value.
    pub const fn is_valid(self) -> bool {
        match self {
            Self::Key { usage, .. } => usage.is_valid(),
            Self::Button { .. } | Self::AbsoluteMotion { .. } => true,
            Self::RelativeMotion { dx, dy } => dx != 0 || dy != 0,
            Self::Scroll {
                horizontal,
                vertical,
            } => horizontal != 0 || vertical != 0,
            Self::LogicalAbsoluteMotion { x, y } => x.is_finite() && y.is_finite(),
            Self::LogicalRelativeMotion { dx, dy } => {
                dx.is_finite() && dy.is_finite() && (dx != 0.0 || dy != 0.0)
            }
            Self::LogicalScroll {
                horizontal,
                vertical,
            } => {
                horizontal.is_finite()
                    && vertical.is_finite()
                    && (horizontal != 0.0 || vertical != 0.0)
            }
            Self::RouteChanged { .. } | Self::SystemGesture(_) => true,
            Self::Gesture { kind, phase, value } => {
                PointerGesture::from_parts(kind, phase, value).is_ok()
            }
        }
    }

    pub const fn gesture(gesture: PointerGesture) -> Self {
        Self::Gesture {
            kind: gesture.kind(),
            phase: gesture.phase(),
            value: gesture.value(),
        }
    }
}

/// The first terminal condition observed by capture. It never changes afterward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Requested,
    QueueFull,
    QueueContended,
    ConsumerDropped,
    LeaseExpired,
    ClockRegression,
    InvalidInput,
    EmergencyEscape,
    NativeFailure,
    /// The OS reported a completed display reconfiguration.
    DisplaysChanged,
}

impl StopReason {
    /// False only for `Requested`, whose recorder already decides the session's fate.
    pub const fn ends_session(self) -> bool {
        !matches!(self, Self::Requested)
    }

    const fn encoded(self) -> u8 {
        match self {
            Self::Requested => 1,
            Self::QueueFull => 2,
            Self::QueueContended => 3,
            Self::ConsumerDropped => 4,
            Self::LeaseExpired => 5,
            Self::ClockRegression => 6,
            Self::InvalidInput => 7,
            Self::EmergencyEscape => 8,
            Self::NativeFailure => 9,
            Self::DisplaysChanged => 10,
        }
    }

    const fn from_encoded(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Requested),
            2 => Some(Self::QueueFull),
            3 => Some(Self::QueueContended),
            4 => Some(Self::ConsumerDropped),
            5 => Some(Self::LeaseExpired),
            6 => Some(Self::ClockRegression),
            7 => Some(Self::InvalidInput),
            8 => Some(Self::EmergencyEscape),
            9 => Some(Self::NativeFailure),
            10 => Some(Self::DisplaysChanged),
            _ => None,
        }
    }
}

/// Shared, first-reason-wins capture termination state.
///
/// Calling [`CaptureStop::stop`] does not invoke a waker, callback, or other arbitrary code.
#[derive(Clone)]
pub struct CaptureStop {
    reason: Arc<AtomicU8>,
    // The first stop wins; the call site is the only record of why a capture ended.
    origin: Arc<std::sync::OnceLock<&'static std::panic::Location<'static>>>,
}

impl fmt::Debug for CaptureStop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureStop")
            .field("reason", &self.reason())
            .finish()
    }
}

impl Default for CaptureStop {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureStop {
    pub fn new() -> Self {
        Self {
            reason: Arc::new(AtomicU8::new(0)),
            origin: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Records `reason` only when no previous terminal reason has been recorded.
    #[track_caller]
    pub fn stop(&self, reason: StopReason) {
        if self
            .reason
            .compare_exchange(0, reason.encoded(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.origin.set(std::panic::Location::caller());
        }
    }

    pub fn reason(&self) -> Option<StopReason> {
        StopReason::from_encoded(self.reason.load(Ordering::Acquire))
    }

    /// The call site that recorded the terminal reason, if any.
    pub fn origin(&self) -> Option<&'static std::panic::Location<'static>> {
        self.origin.get().copied()
    }

    pub fn is_stopped(&self) -> bool {
        self.reason().is_some()
    }

    /// True once stopped for any reason but `Requested`, which leaves the session to its recorder.
    pub fn ends_session(&self) -> bool {
        self.reason().is_some_and(StopReason::ends_session)
    }

    /// Asks `session` to stop for an end it did not request; returns the reason when this call
    /// asked. A requested stop leaves `session` alone: its signal can outlive it to serve the next.
    #[track_caller]
    pub fn request_session_stop(&self, session: &RevocationSignal) -> Option<StopReason> {
        let reason = self.reason().filter(|reason| reason.ends_session())?;
        if session.is_stopping() {
            return None;
        }
        session.request_stop();
        Some(reason)
    }

    fn stopped_or(&self, requested: StopReason) -> StopReason {
        self.stop(requested);
        self.reason()
            .expect("CaptureStop stores a valid terminal reason")
    }
}

/// Cuts the capture owner thread's wait short so a submitted command or stop applies at once
/// instead of at its next poll. The native adapter registers how its own thread is woken.
#[derive(Clone, Default)]
pub struct OwnerWake(Arc<std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>>);

impl fmt::Debug for OwnerWake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnerWake")
    }
}

impl OwnerWake {
    /// Registers the one wake; a second registration is refused. It must only signal, never block.
    pub fn register(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.0.set(wake).is_ok()
    }

    /// A no-op until a wake is registered: the owner's bounded poll still picks the change up.
    pub fn wake(&self) {
        if let Some(wake) = self.0.get() {
            wake();
        }
    }
}

/// An event tagged with the hook thread's route that was active when it was admitted.
#[derive(Clone, Copy, PartialEq)]
pub struct CapturedEvent {
    pub event: CaptureEvent,
    pub routing_revision: u64,
    pub remote: bool,
    /// The session floor's generation when the event was captured; 0 without a take-back gate.
    pub floor_generation: u64,
}

impl fmt::Debug for CapturedEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturedEvent([redacted])")
    }
}

/// Creates a fixed, lock-free SPSC capture queue. Its slots are allocated exactly once.
///
/// `CaptureProducer` and `CaptureConsumer` expose only `&mut self` methods and cannot be cloned,
/// which makes their single-producer/single-consumer ownership explicit. A slot's publish counter
/// is release-stored after its payload atomics and acquire-loaded before they are read. The
/// consumer release-stores the next free counter only after copying the payload, so a wrapped slot
/// is never read and written concurrently.
pub fn capture_channel(stop: CaptureStop) -> (CaptureProducer, CaptureConsumer) {
    let queue = Arc::new(CaptureQueue {
        stop,
        slots: std::array::from_fn(|index| QueueSlot::new(index as u64)),
        waker: std::sync::OnceLock::new(),
    });
    (
        CaptureProducer {
            queue: Arc::clone(&queue),
            tail: 0,
            routing_revision: 0,
            remote: false,
        },
        CaptureConsumer { queue, head: 0 },
    )
}

struct CaptureQueue {
    stop: CaptureStop,
    slots: [QueueSlot; CAPTURE_QUEUE_CAPACITY],
    /// Called by the producer after every publish so the consumer can sleep instead of polling.
    waker: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

/// Seven atomic words keep slot publication and every copied field data-race-free without unsafe
/// storage. Only `publish` establishes ownership; payload words are read after its acquire load.
struct QueueSlot {
    publish: AtomicU64,
    event_header: AtomicU64,
    event_payload: AtomicU64,
    event_revision: AtomicU64,
    routing_revision: AtomicU64,
    routing_remote: AtomicU64,
    floor_generation: AtomicU64,
}

impl QueueSlot {
    const fn new(position: u64) -> Self {
        Self {
            publish: AtomicU64::new(position),
            event_header: AtomicU64::new(0),
            event_payload: AtomicU64::new(0),
            event_revision: AtomicU64::new(0),
            routing_revision: AtomicU64::new(0),
            routing_remote: AtomicU64::new(0),
            floor_generation: AtomicU64::new(0),
        }
    }
}

/// The one callback-thread producer for a bounded SPSC capture queue.
pub struct CaptureProducer {
    queue: Arc<CaptureQueue>,
    tail: u64,
    routing_revision: u64,
    remote: bool,
}

impl fmt::Debug for CaptureProducer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureProducer")
    }
}

impl CaptureProducer {
    /// Enqueues one copied event without allocation or blocking.
    ///
    /// This compatibility wrapper tags a local revision-zero event. Native capture uses
    /// [`Self::try_push_tagged`] so route metadata is retained.
    pub fn try_push(&mut self, event: CaptureEvent) -> Result<(), StopReason> {
        self.try_push_tagged(CapturedEvent {
            event,
            routing_revision: 0,
            remote: false,
            floor_generation: 0,
        })
    }

    /// Enqueues one event with the hook thread's active route metadata.
    ///
    /// A `RouteChanged` record must carry the next strictly increasing revision and exactly match
    /// its tag. The producer advances its local route only after that barrier is published, so a
    /// new-route physical event cannot enter the queue before its barrier.
    pub fn try_push_tagged(&mut self, record: CapturedEvent) -> Result<(), StopReason> {
        if let Some(reason) = self.queue.stop.reason() {
            return Err(reason);
        }
        if !record.event.is_valid() || !self.accepts_route(record) {
            return Err(self.queue.stop.stopped_or(StopReason::InvalidInput));
        }

        if let Some(reason) = self.queue.stop.reason() {
            return Err(reason);
        }
        let position = self.tail;
        let slot = &self.queue.slots[slot_index(position)];
        if slot.publish.load(Ordering::Acquire) != position {
            return Err(self.queue.stop.stopped_or(StopReason::QueueFull));
        }

        let encoded = EncodedEvent::from(record.event);
        slot.event_header.store(encoded.header, Ordering::Relaxed);
        slot.event_payload.store(encoded.payload, Ordering::Relaxed);
        slot.event_revision
            .store(encoded.revision, Ordering::Relaxed);
        slot.routing_revision
            .store(record.routing_revision, Ordering::Relaxed);
        slot.routing_remote
            .store(u64::from(record.remote), Ordering::Relaxed);
        slot.floor_generation
            .store(record.floor_generation, Ordering::Relaxed);
        slot.publish
            .store(position.wrapping_add(1), Ordering::Release);
        self.tail = position.wrapping_add(1);
        if let CaptureEvent::RouteChanged { remote, revision } = record.event {
            self.remote = remote;
            self.routing_revision = revision;
        }
        if let Some(wake) = self.queue.waker.get() {
            wake();
        }

        self.queue.stop.reason().map_or(Ok(()), Err)
    }

    fn accepts_route(&self, record: CapturedEvent) -> bool {
        match record.event {
            CaptureEvent::RouteChanged { remote, revision } => {
                revision > self.routing_revision
                    && record.routing_revision == revision
                    && record.remote == remote
            }
            _ => record.routing_revision == self.routing_revision && record.remote == self.remote,
        }
    }
}

/// The sole consumer for a capture queue. Dropping it terminates all producers.
pub struct CaptureConsumer {
    queue: Arc<CaptureQueue>,
    head: u64,
}

impl fmt::Debug for CaptureConsumer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureConsumer")
    }
}

impl CaptureConsumer {
    /// Registers the one callback the producer runs after each publish; a second registration is
    /// refused. The callback runs on the hook thread and must only signal, never block.
    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.queue.waker.set(waker).is_ok()
    }

    /// Removes one copied event without allocation or blocking.
    ///
    /// Once stopped, this method never returns a queued payload.
    pub fn try_pop(&mut self) -> Result<Option<CaptureEvent>, StopReason> {
        self.try_pop_tagged()
            .map(|record| record.map(|record| record.event))
    }

    /// Removes one copied event and its routing metadata without allocation or blocking.
    ///
    /// Once stopped, this method never returns a queued payload.
    pub fn try_pop_tagged(&mut self) -> Result<Option<CapturedEvent>, StopReason> {
        if let Some(reason) = self.queue.stop.reason() {
            return Err(reason);
        }
        let position = self.head;
        let slot = &self.queue.slots[slot_index(position)];
        if slot.publish.load(Ordering::Acquire) != position.wrapping_add(1) {
            return Ok(None);
        }

        let record = CapturedEvent::decode(
            slot.event_header.load(Ordering::Relaxed),
            slot.event_payload.load(Ordering::Relaxed),
            slot.event_revision.load(Ordering::Relaxed),
            slot.routing_revision.load(Ordering::Relaxed),
            slot.routing_remote.load(Ordering::Relaxed),
            slot.floor_generation.load(Ordering::Relaxed),
        );
        slot.publish.store(
            position.wrapping_add(CAPTURE_QUEUE_CAPACITY as u64),
            Ordering::Release,
        );
        self.head = position.wrapping_add(1);

        let record = match record {
            Some(record) => record,
            None => return Err(self.queue.stop.stopped_or(StopReason::NativeFailure)),
        };

        self.queue.stop.reason().map_or(Ok(Some(record)), Err)
    }
}

impl Drop for CaptureConsumer {
    fn drop(&mut self) {
        self.queue.stop.stop(StopReason::ConsumerDropped);
    }
}

const EVENT_KEY: u64 = 1;
const EVENT_BUTTON: u64 = 2;
const EVENT_ABSOLUTE_MOTION: u64 = 3;
const EVENT_RELATIVE_MOTION: u64 = 4;
const EVENT_SCROLL: u64 = 5;
const EVENT_ROUTE_CHANGED: u64 = 6;
const EVENT_LOGICAL_ABSOLUTE_MOTION: u64 = 7;
const EVENT_LOGICAL_RELATIVE_MOTION: u64 = 8;
const EVENT_LOGICAL_SCROLL: u64 = 9;
const EVENT_GESTURE: u64 = 10;
const EVENT_SYSTEM_GESTURE: u64 = 11;
const EVENT_KIND_MASK: u64 = 0xff;
/// A gesture's kind and phase each take four header bits; phase 0 is none.
const GESTURE_FIELD_MASK: u64 = 0xf;

struct EncodedEvent {
    header: u64,
    payload: u64,
    revision: u64,
}

impl From<CaptureEvent> for EncodedEvent {
    fn from(event: CaptureEvent) -> Self {
        match event {
            CaptureEvent::Key {
                usage,
                pressed,
                repeat,
                modifiers,
            } => Self {
                header: EVENT_KEY
                    | (u64::from(usage.0) << 8)
                    | (u64::from(pressed) << 24)
                    | (u64::from(repeat) << 25)
                    | (u64::from(modifiers.0) << 26),
                payload: 0,
                revision: 0,
            },
            CaptureEvent::Button {
                button,
                pressed,
                at,
            } => Self {
                header: EVENT_BUTTON | ((button.index() as u64) << 8) | (u64::from(pressed) << 11),
                payload: u64::try_from(at.as_nanos()).unwrap_or(u64::MAX),
                revision: 0,
            },
            CaptureEvent::AbsoluteMotion { x, y } => Self {
                header: EVENT_ABSOLUTE_MOTION,
                payload: pack_i32_pair(x, y),
                revision: 0,
            },
            CaptureEvent::RelativeMotion { dx, dy } => Self {
                header: EVENT_RELATIVE_MOTION,
                payload: pack_i32_pair(dx, dy),
                revision: 0,
            },
            CaptureEvent::Scroll {
                horizontal,
                vertical,
            } => Self {
                header: EVENT_SCROLL,
                payload: pack_i32_pair(horizontal, vertical),
                revision: 0,
            },
            CaptureEvent::LogicalAbsoluteMotion { x, y } => Self {
                header: EVENT_LOGICAL_ABSOLUTE_MOTION,
                payload: x.to_bits(),
                revision: y.to_bits(),
            },
            CaptureEvent::LogicalRelativeMotion { dx, dy } => Self {
                header: EVENT_LOGICAL_RELATIVE_MOTION,
                payload: dx.to_bits(),
                revision: dy.to_bits(),
            },
            CaptureEvent::LogicalScroll {
                horizontal,
                vertical,
            } => Self {
                header: EVENT_LOGICAL_SCROLL,
                payload: horizontal.to_bits(),
                revision: vertical.to_bits(),
            },
            CaptureEvent::RouteChanged { remote, revision } => Self {
                header: EVENT_ROUTE_CHANGED | (u64::from(remote) << 8),
                payload: 0,
                revision,
            },
            CaptureEvent::Gesture { kind, phase, value } => Self {
                header: EVENT_GESTURE
                    | (u64::from(kind.to_wire()) << 8)
                    | (u64::from(phase.map_or(0, GesturePhase::to_wire)) << 12),
                payload: value.to_bits(),
                revision: 0,
            },
            CaptureEvent::SystemGesture(gesture) => Self {
                header: EVENT_SYSTEM_GESTURE | (u64::from(gesture.to_wire()) << 8),
                payload: 0,
                revision: 0,
            },
        }
    }
}

impl CapturedEvent {
    fn decode(
        header: u64,
        payload: u64,
        event_revision: u64,
        routing_revision: u64,
        routing_remote: u64,
        floor_generation: u64,
    ) -> Option<Self> {
        let remote = match routing_remote {
            0 => false,
            1 => true,
            _ => return None,
        };
        let event = match header & EVENT_KIND_MASK {
            EVENT_KEY => CaptureEvent::Key {
                usage: HidUsage(((header >> 8) & u64::from(u16::MAX)) as u16),
                pressed: header & (1 << 24) != 0,
                repeat: header & (1 << 25) != 0,
                modifiers: ModifierState(((header >> 26) & u64::from(u8::MAX)) as u8),
            },
            EVENT_BUTTON => CaptureEvent::Button {
                button: MouseButton::from_index(((header >> 8) & 0x07) as usize)?,
                pressed: header & (1 << 11) != 0,
                at: Duration::from_nanos(payload),
            },
            EVENT_ABSOLUTE_MOTION => {
                let (x, y) = unpack_i32_pair(payload);
                CaptureEvent::AbsoluteMotion { x, y }
            }
            EVENT_RELATIVE_MOTION => {
                let (dx, dy) = unpack_i32_pair(payload);
                CaptureEvent::RelativeMotion { dx, dy }
            }
            EVENT_SCROLL => {
                let (horizontal, vertical) = unpack_i32_pair(payload);
                CaptureEvent::Scroll {
                    horizontal,
                    vertical,
                }
            }
            EVENT_LOGICAL_ABSOLUTE_MOTION => CaptureEvent::LogicalAbsoluteMotion {
                x: f64::from_bits(payload),
                y: f64::from_bits(event_revision),
            },
            EVENT_LOGICAL_RELATIVE_MOTION => CaptureEvent::LogicalRelativeMotion {
                dx: f64::from_bits(payload),
                dy: f64::from_bits(event_revision),
            },
            EVENT_LOGICAL_SCROLL => CaptureEvent::LogicalScroll {
                horizontal: f64::from_bits(payload),
                vertical: f64::from_bits(event_revision),
            },
            EVENT_ROUTE_CHANGED => CaptureEvent::RouteChanged {
                remote: header & (1 << 8) != 0,
                revision: event_revision,
            },
            EVENT_GESTURE => CaptureEvent::Gesture {
                kind: GestureKind::from_wire(((header >> 8) & GESTURE_FIELD_MASK) as u8)?,
                phase: match (header >> 12) & GESTURE_FIELD_MASK {
                    0 => None,
                    phase => Some(GesturePhase::from_wire(phase as u8)?),
                },
                value: f64::from_bits(payload),
            },
            EVENT_SYSTEM_GESTURE => CaptureEvent::SystemGesture(SystemGesture::from_wire(
                ((header >> 8) & u64::from(u8::MAX)) as u8,
            )?),
            _ => return None,
        };
        if matches!(event, CaptureEvent::RouteChanged { remote: event_remote, revision }
            if event_remote != remote || revision != routing_revision)
        {
            return None;
        }
        event.is_valid().then_some(Self {
            event,
            routing_revision,
            remote,
            floor_generation,
        })
    }
}

const fn slot_index(position: u64) -> usize {
    (position as usize) % CAPTURE_QUEUE_CAPACITY
}

const fn pack_i32_pair(first: i32, second: i32) -> u64 {
    (first as u32 as u64) | ((second as u32 as u64) << 32)
}

const fn unpack_i32_pair(packed: u64) -> (i32, i32) {
    (packed as u32 as i32, (packed >> 32) as u32 as i32)
}

/// A generation-bound suppression deadline. It is deliberately local and not thread-safe.
pub struct SuppressionLease {
    generation: u64,
    deadline: Option<Duration>,
    last_now: Duration,
    stop: CaptureStop,
}

impl fmt::Debug for SuppressionLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SuppressionLease([redacted])")
    }
}

impl SuppressionLease {
    pub fn new(generation: u64, now: Duration, stop: CaptureStop) -> Self {
        Self {
            generation,
            deadline: None,
            last_now: now,
            stop,
        }
    }

    /// Activates or extends this lease. An already-expired deadline is terminal even if no timer
    /// observed it first.
    pub fn renew(
        &mut self,
        generation: u64,
        now: Duration,
        ttl: Duration,
    ) -> Result<(), StopReason> {
        self.validate_generation_and_time(generation, now)?;
        self.reject_expired(now)?;
        if ttl.is_zero() || ttl > MAX_SUPPRESSION_TTL {
            return Err(self.stop.stopped_or(StopReason::InvalidInput));
        }
        let deadline = now
            .checked_add(ttl)
            .ok_or_else(|| self.stop.stopped_or(StopReason::InvalidInput))?;
        self.deadline = Some(deadline);
        Ok(())
    }

    /// Clears an active local lease only after its generation, clock, and deadline are valid.
    pub fn release(&mut self, generation: u64, now: Duration) -> Result<(), StopReason> {
        self.validate_generation_and_time(generation, now)?;
        self.reject_expired(now)?;
        self.deadline = None;
        Ok(())
    }

    /// Returns whether suppression remains active at `now`. Exact deadlines are expired.
    pub fn is_suppressing(&mut self, now: Duration) -> bool {
        if self.observe_time(now).is_err() || self.stop.is_stopped() {
            return false;
        }
        self.reject_expired(now).is_ok() && self.deadline.is_some()
    }

    fn validate_generation_and_time(
        &mut self,
        generation: u64,
        now: Duration,
    ) -> Result<(), StopReason> {
        self.observe_time(now)?;
        if let Some(reason) = self.stop.reason() {
            return Err(reason);
        }
        if generation != self.generation {
            return Err(self.stop.stopped_or(StopReason::InvalidInput));
        }
        Ok(())
    }

    fn observe_time(&mut self, now: Duration) -> Result<(), StopReason> {
        if let Some(reason) = self.stop.reason() {
            return Err(reason);
        }
        if now < self.last_now {
            return Err(self.stop.stopped_or(StopReason::ClockRegression));
        }
        self.last_now = now;
        Ok(())
    }

    fn reject_expired(&mut self, now: Duration) -> Result<(), StopReason> {
        if let Some(reason) = self.stop.reason() {
            return Err(reason);
        }
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return Err(self.stop.stopped_or(StopReason::LeaseExpired));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_producer_wakes_the_registered_consumer_once_per_publish() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (mut producer, consumer) = capture_channel(CaptureStop::new());
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        assert!(consumer.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));
        assert!(
            !consumer.set_waker(Arc::new(|| {})),
            "a second waker is refused"
        );
        let key = |pressed| CaptureEvent::Key {
            usage: HidUsage(0x04),
            pressed,
            repeat: false,
            modifiers: ModifierState::default(),
        };
        producer.try_push(key(true)).unwrap();
        producer.try_push(key(false)).unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_button_keeps_its_capture_time_through_the_ring() {
        let (mut producer, mut consumer) = capture_channel(CaptureStop::new());
        for button in [MouseButton::Left, MouseButton::Forward] {
            for at in [
                Duration::ZERO,
                Duration::from_nanos(1),
                Duration::from_secs(86_400 * 365),
            ] {
                for pressed in [true, false] {
                    let event = CaptureEvent::Button {
                        button,
                        pressed,
                        at,
                    };
                    producer.try_push(event).unwrap();
                    assert!(consumer.try_pop().unwrap() == Some(event));
                }
            }
        }
    }

    #[test]
    fn every_gesture_and_system_gesture_keeps_its_exact_value_through_the_ring() {
        let (mut producer, mut consumer) = capture_channel(CaptureStop::new());
        let mut events = vec![
            CaptureEvent::gesture(PointerGesture::SmartMagnify),
            CaptureEvent::gesture(PointerGesture::ForceClick),
        ];
        for phase in GesturePhase::ALL {
            for delta in [0.0, -0.0, -0.999_999, 4.0, 0.015_625] {
                events.push(CaptureEvent::gesture(PointerGesture::Magnify {
                    phase,
                    delta,
                }));
            }
            for degrees in [-180.0, 0.0, 12.345_678_9, 180.0] {
                events.push(CaptureEvent::gesture(PointerGesture::Rotate {
                    phase,
                    degrees,
                }));
            }
        }
        events.extend(SystemGesture::ALL.map(CaptureEvent::SystemGesture));
        for event in events {
            assert!(event.is_valid());
            producer.try_push(event).unwrap();
            let received = consumer.try_pop().unwrap().unwrap();
            assert!(received == event);
            if let (
                CaptureEvent::Gesture { value, .. },
                CaptureEvent::Gesture {
                    value: received, ..
                },
            ) = (event, received)
            {
                assert_eq!(received.to_bits(), value.to_bits());
            }
        }
    }

    #[test]
    fn an_invalid_gesture_is_refused_before_it_enters_the_ring() {
        let invalid = [
            CaptureEvent::Gesture {
                kind: GestureKind::Magnify,
                phase: None,
                value: 0.5,
            },
            CaptureEvent::Gesture {
                kind: GestureKind::Magnify,
                phase: Some(GesturePhase::Changed),
                value: -1.0,
            },
            CaptureEvent::Gesture {
                kind: GestureKind::Rotate,
                phase: Some(GesturePhase::Changed),
                value: f64::NAN,
            },
            CaptureEvent::Gesture {
                kind: GestureKind::ForceClick,
                phase: Some(GesturePhase::Began),
                value: 0.0,
            },
        ];
        for event in invalid {
            assert!(!event.is_valid());
            let stop = CaptureStop::new();
            let (mut producer, _consumer) = capture_channel(stop.clone());
            assert_eq!(producer.try_push(event), Err(StopReason::InvalidInput));
        }
    }

    #[test]
    fn an_unknown_gesture_slot_is_a_native_failure_not_an_event() {
        let decode = |header, payload| CapturedEvent::decode(header, payload, 0, 0, 0, 0);
        assert!(decode(EVENT_GESTURE | (1 << 8) | (2 << 12), 0).is_some());
        for header in [
            EVENT_GESTURE | (5 << 8) | (2 << 12),
            EVENT_GESTURE | (1 << 8) | (5 << 12),
            EVENT_SYSTEM_GESTURE,
            EVENT_SYSTEM_GESTURE | (13 << 8),
        ] {
            assert!(decode(header, 0).is_none());
        }
        assert!(
            decode(EVENT_GESTURE | (3 << 8), 1.0_f64.to_bits()).is_none(),
            "a discrete kind with a value is invalid"
        );
    }

    #[test]
    fn gesture_debug_never_emits_input_values() {
        let debug = format!(
            "{:?}",
            CaptureEvent::gesture(PointerGesture::Rotate {
                phase: GesturePhase::Began,
                degrees: 42.5,
            })
        );
        assert_eq!(debug, "CaptureEvent::Gesture([redacted])");
        assert_eq!(
            format!("{:?}", CaptureEvent::SystemGesture(SystemGesture::Search)),
            "CaptureEvent::SystemGesture([redacted])"
        );
    }

    #[test]
    fn a_display_change_stop_keeps_its_own_reason() {
        let stop = CaptureStop::new();
        stop.stop(StopReason::DisplaysChanged);
        stop.stop(StopReason::NativeFailure);
        assert_eq!(stop.reason(), Some(StopReason::DisplaysChanged));
    }

    #[test]
    fn only_the_sessions_own_request_leaves_the_session_running() {
        let reasons: Vec<_> = (1..=u8::MAX).filter_map(StopReason::from_encoded).collect();
        assert_eq!(reasons.len(), 10);
        for reason in reasons {
            assert_eq!(reason.ends_session(), reason != StopReason::Requested);
            let stop = CaptureStop::new();
            assert!(!stop.ends_session(), "a running capture ends nothing");
            stop.stop(reason);
            assert_eq!(stop.ends_session(), reason.ends_session());
        }
    }

    #[test]
    fn a_requested_stop_leaves_the_session_signal_clean_for_the_next_capture() {
        let session = RevocationSignal::default();
        let first = CaptureStop::new();
        first.stop(StopReason::Requested);
        assert_eq!(first.request_session_stop(&session), None);
        assert!(!session.is_stopping());
        assert!(session.origin().is_none());

        let second = CaptureStop::new();
        assert_eq!(second.request_session_stop(&session), None);
        assert!(!second.is_stopped(), "the next capture starts unstopped");
        second.stop(StopReason::NativeFailure);
        let asked_at = line!() + 1;
        let asked = second.request_session_stop(&session);
        assert_eq!(asked, Some(StopReason::NativeFailure));
        assert!(session.is_stopping());
        assert!(
            !session.is_revoked(),
            "a soft stop leaves the close its socket"
        );
        assert_eq!(session.origin().map(|at| at.line()), Some(asked_at));
        assert_eq!(second.request_session_stop(&session), None, "asks once");
    }

    #[test]
    fn every_clone_of_an_owner_wake_runs_the_one_registered_wake() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let wake = OwnerWake::default();
        wake.wake();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let control_side = wake.clone();
        assert!(wake.register(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));
        assert!(
            !control_side.register(Arc::new(|| {})),
            "a second wake is refused"
        );
        control_side.wake();
        wake.wake();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_lease_expiry_asks_the_session_to_stop_and_leaves_its_socket_open() {
        let session = RevocationSignal::default();
        let stop = CaptureStop::new();
        stop.stop(StopReason::LeaseExpired);
        assert_eq!(
            stop.request_session_stop(&session),
            Some(StopReason::LeaseExpired)
        );
        assert!(session.is_stopping());
        assert!(
            !session.is_revoked(),
            "the failure close still has a socket"
        );
    }

    #[test]
    fn a_session_already_stopping_is_not_asked_again() {
        let session = RevocationSignal::default();
        session.request_stop();
        let stop = CaptureStop::new();
        stop.stop(StopReason::DisplaysChanged);
        assert_eq!(stop.request_session_stop(&session), None);
        assert!(session.is_stopping());
    }
}
