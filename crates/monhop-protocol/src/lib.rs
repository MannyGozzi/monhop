#![forbid(unsafe_code)]

//! A small, strict binary codec for authenticated MonHop sessions.
//!
//! The transport owns encryption and delivery. This crate only turns complete,
//! bounded frames into typed messages and rejects malformed peer input.

use std::error::Error;
use std::fmt;

use monhop_core::{
    DeviceId, DisplayId, HidUsage, ModifierState, MonitorIdentity, MouseButton, Platform, Point,
};

pub const MAGIC: [u8; 4] = *b"LKM!";
/// Bumped whenever the wire changes shape; both computers must run the same build.
pub const PROTOCOL_VERSION: u16 = 10;
pub const HEADER_LEN: usize = 28;
pub const MAX_FRAME_LEN: usize = 8_192;
pub use monhop_core::MAX_DISPLAYS;
pub const MAX_DISPLAY_NAME_BYTES: usize = 96;
pub const MAX_EVENTS_PER_SECOND: u32 = 100_000;
pub const MAX_NATIVE_DIMENSION: u32 = 65_535;
pub const MAX_LOGICAL_SIZE: f64 = 1_000_000.0;
pub const MAX_LOGICAL_ORIGIN_ABS: f64 = 1_000_000_000.0;
pub const MIN_SCALE_FACTOR: f32 = 0.1;
pub const MAX_SCALE_FACTOR: f32 = 16.0;

const TOPOLOGY_PREFIX_LEN: usize = 4;
const DISPLAY_FIXED_LEN: usize = 64;
const MODIFIER_MASK: u16 = u8::MAX as u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionEpoch(u64);

impl SessionEpoch {
    pub fn new(value: u64) -> Result<Self, EpochError> {
        if value == 0 {
            return Err(EpochError::Zero);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochError {
    Zero,
}

impl fmt::Display for EpochError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session epoch must be non-zero")
    }
}

impl Error for EpochError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities(u32);

impl Capabilities {
    pub const RELATIVE_MOTION: u32 = 1 << 0;
    pub const HORIZONTAL_SCROLL: u32 = 1 << 1;
    pub const DISPLAY_TOPOLOGY: u32 = 1 << 2;
    pub const KNOWN_BITS: u32 =
        Self::RELATIVE_MOTION | Self::HORIZONTAL_SCROLL | Self::DISPLAY_TOPOLOGY;

    pub fn new(bits: u32) -> Result<Self, CapabilityError> {
        if bits & !Self::KNOWN_BITS != 0 {
            return Err(CapabilityError::UnknownBits);
        }
        Ok(Self(bits))
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, capability: u32) -> bool {
        self.0 & capability == capability
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityError {
    UnknownBits,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("capabilities contain unknown bits")
    }
}

impl Error for CapabilityError {}

#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    pub device_id: DeviceId,
    pub platform: Platform,
    pub protocol_version: u16,
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayDescription {
    pub id: DisplayId,
    pub name: String,
    pub native_width: u32,
    pub native_height: u32,
    pub logical_origin: Point,
    pub logical_size: Point,
    pub scale_factor: f32,
    pub is_primary: bool,
    /// Present when the platform read the monitor's EDID identity.
    pub monitor: Option<MonitorIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayTopology {
    displays: Vec<DisplayDescription>,
}

impl DisplayTopology {
    pub fn new(displays: Vec<DisplayDescription>) -> Result<Self, TopologyError> {
        let topology = Self { displays };
        topology.validate()?;
        Ok(topology)
    }

    pub fn displays(&self) -> &[DisplayDescription] {
        &self.displays
    }

    /// Display labels and enumeration order are cosmetic, not routing identity.
    pub fn same_geometry(&self, other: &Self) -> bool {
        self.displays.len() == other.displays.len()
            && self.displays.iter().all(|display| {
                other.displays.iter().any(|candidate| {
                    display.id == candidate.id
                        && display.native_width == candidate.native_width
                        && display.native_height == candidate.native_height
                        && display.logical_origin == candidate.logical_origin
                        && display.logical_size == candidate.logical_size
                        && display.scale_factor == candidate.scale_factor
                        && display.is_primary == candidate.is_primary
                })
            })
    }

    pub fn into_displays(self) -> Vec<DisplayDescription> {
        self.displays
    }

    fn validate(&self) -> Result<(), TopologyError> {
        if self.displays.is_empty() {
            return Err(TopologyError::Empty);
        }
        if self.displays.len() > MAX_DISPLAYS {
            return Err(TopologyError::TooManyDisplays);
        }

        let mut primary_count = 0_u8;
        for (index, display) in self.displays.iter().enumerate() {
            if display.name.len() > MAX_DISPLAY_NAME_BYTES {
                return Err(TopologyError::DisplayNameTooLong);
            }
            if display.native_width == 0
                || display.native_height == 0
                || display.native_width > MAX_NATIVE_DIMENSION
                || display.native_height > MAX_NATIVE_DIMENSION
            {
                return Err(TopologyError::InvalidNativeSize);
            }
            if !display.logical_origin.is_finite() || !display.logical_size.is_finite() {
                return Err(TopologyError::NonFiniteGeometry);
            }
            let logical_end_x = display.logical_origin.x + display.logical_size.x;
            let logical_end_y = display.logical_origin.y + display.logical_size.y;
            if !logical_end_x.is_finite() || !logical_end_y.is_finite() {
                return Err(TopologyError::GeometryOverflow);
            }
            if display.logical_origin.x.abs() > MAX_LOGICAL_ORIGIN_ABS
                || display.logical_origin.y.abs() > MAX_LOGICAL_ORIGIN_ABS
            {
                return Err(TopologyError::LogicalOriginOutOfRange);
            }
            if display.logical_size.x <= 0.0
                || display.logical_size.y <= 0.0
                || display.logical_size.x > MAX_LOGICAL_SIZE
                || display.logical_size.y > MAX_LOGICAL_SIZE
            {
                return Err(TopologyError::InvalidLogicalSize);
            }
            if !display.scale_factor.is_finite()
                || !(MIN_SCALE_FACTOR..=MAX_SCALE_FACTOR).contains(&display.scale_factor)
            {
                return Err(TopologyError::InvalidScaleFactor);
            }
            if display.is_primary {
                primary_count += 1;
            }
            if self.displays[..index]
                .iter()
                .any(|previous| previous.id == display.id)
            {
                return Err(TopologyError::DuplicateDisplayId);
            }
        }
        if primary_count != 1 {
            return Err(TopologyError::PrimaryDisplayCount);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyError {
    Empty,
    TooManyDisplays,
    DisplayNameTooLong,
    DuplicateDisplayId,
    InvalidNativeSize,
    NonFiniteGeometry,
    GeometryOverflow,
    LogicalOriginOutOfRange,
    InvalidLogicalSize,
    InvalidScaleFactor,
    PrimaryDisplayCount,
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid display topology")
    }
}

impl Error for TopologyError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Motion {
    /// An absolute logical virtual-desktop position.
    Absolute(Point),
    /// A relative logical movement delta from the current destination cursor.
    Relative(Point),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Button {
    pub button: MouseButton,
    pub is_down: bool,
    /// The source OS's count for this press and its release: 1 single, 2 double, 3 triple; never 0.
    pub click_count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scroll {
    pub horizontal: f64,
    pub vertical: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub usage: HidUsage,
    pub is_down: bool,
    pub repeat: bool,
    pub modifiers: ModifierState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectCode {
    Requested,
    TransportLost,
    ProtocolViolation,
    SessionExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    MalformedFrame,
    UnsupportedVersion,
    SessionExpired,
    RateLimited,
    ProtocolViolation,
}

/// The mutually authenticated authority requested for a session: bidirectional sharing gated per
/// direction by `control`, or the setup link (topology/arrangement exchange only, scope 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPurpose {
    Share,
    Setup,
}

/// Which directions a paired session may carry input, agreed during the handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlPermissions {
    pub lower_controls_higher: bool,
    pub higher_controls_lower: bool,
}

impl ControlPermissions {
    pub const BOTH: Self = Self {
        lower_controls_higher: true,
        higher_controls_lower: true,
    };

    pub const fn to_wire(self) -> u8 {
        (self.lower_controls_higher as u8) | ((self.higher_controls_lower as u8) << 1)
    }

    pub fn from_wire(value: u8) -> Result<Self, DecodeError> {
        if value == 0 || value > 0b11 {
            return Err(DecodeError::InvalidControl);
        }
        Ok(Self {
            lower_controls_higher: value & 0b01 != 0,
            higher_controls_lower: value & 0b10 != 0,
        })
    }

    /// `Connection` never carries input, so no scope allows it there.
    pub const fn allows(self, scope: FrameScope) -> bool {
        match scope {
            FrameScope::Connection => false,
            FrameScope::LowerControlsHigher => self.lower_controls_higher,
            FrameScope::HigherControlsLower => self.higher_controls_lower,
        }
    }
}

/// The bilateral setup proposal carried in the final handshake frame. `Setup` must carry
/// `ControlPermissions::BOTH`; `control` is otherwise nonzero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSetup {
    pub purpose: SessionPurpose,
    pub control: ControlPermissions,
}

/// Why the inbound half refused an `ActivateDisplayAt`; it never injects on this path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineReason {
    Contended = 1,
    Busy = 2,
    Disabled = 3,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Hello(Hello),
    /// Proposes purpose and control permissions. Accepted only once both sides agree.
    SessionSetup(SessionSetup),
    /// The local native input worker is ready. Only the startup phase may admit this message.
    SessionReady,
    DisplayTopology(DisplayTopology),
    Motion(Motion),
    Button(Button),
    Scroll(Scroll),
    Key(Key),
    Modifiers(ModifierState),
    ActivateDisplay(DisplayId),
    /// Atomically anchors the destination pointer before subsequent reliable input is delivered.
    /// The enclosing frame epoch and reliable sequence provide its transition ordering.
    ActivateDisplayAt {
        display_id: DisplayId,
        position: Point,
    },
    /// Confirms a completed activation. An acknowledgement is not authority by itself; the session
    /// controller must validate it against the active trusted session.
    ActivationAck(DisplayId),
    /// Answers an `ActivateDisplayAt` without injecting; consumes that epoch like an ack.
    ActivationDeclined {
        display_id: DisplayId,
        reason: DeclineReason,
    },
    ReleaseAll,
    ReleaseAck,
    /// Local input has already been restored; this never itself moves the pointer.
    TakeBack,
    Ping(u64),
    Pong(u64),
    Disconnect(DisconnectCode),
    Error(ErrorCode),
}

impl Message {
    pub const fn delivery(&self) -> DeliveryClass {
        match self {
            Self::Motion(Motion::Absolute(_)) => DeliveryClass::MotionDatagram,
            Self::Hello(_)
            | Self::SessionSetup(_)
            | Self::SessionReady
            | Self::DisplayTopology(_)
            | Self::Motion(Motion::Relative(_))
            | Self::Button(_)
            | Self::Scroll(_)
            | Self::Key(_)
            | Self::Modifiers(_)
            | Self::ActivateDisplay(_)
            | Self::ActivateDisplayAt { .. }
            | Self::ActivationAck(_)
            | Self::ActivationDeclined { .. }
            | Self::ReleaseAll
            | Self::ReleaseAck
            | Self::TakeBack
            | Self::Ping(_)
            | Self::Pong(_)
            | Self::Disconnect(_)
            | Self::Error(_) => DeliveryClass::Reliable,
        }
    }

    const fn kind(&self) -> MessageKind {
        match self {
            Self::Hello(_) => MessageKind::Hello,
            Self::SessionSetup(_) => MessageKind::SessionSetup,
            Self::SessionReady => MessageKind::SessionReady,
            Self::DisplayTopology(_) => MessageKind::DisplayTopology,
            Self::Motion(_) => MessageKind::Motion,
            Self::Button(_) => MessageKind::Button,
            Self::Scroll(_) => MessageKind::Scroll,
            Self::Key(_) => MessageKind::Key,
            Self::Modifiers(_) => MessageKind::Modifiers,
            Self::ActivateDisplay(_) => MessageKind::ActivateDisplay,
            Self::ActivateDisplayAt { .. } => MessageKind::ActivateDisplayAt,
            Self::ActivationAck(_) => MessageKind::ActivationAck,
            Self::ActivationDeclined { .. } => MessageKind::ActivationDeclined,
            Self::ReleaseAll => MessageKind::ReleaseAll,
            Self::ReleaseAck => MessageKind::ReleaseAck,
            Self::TakeBack => MessageKind::TakeBack,
            Self::Ping(_) => MessageKind::Ping,
            Self::Pong(_) => MessageKind::Pong,
            Self::Disconnect(_) => MessageKind::Disconnect,
            Self::Error(_) => MessageKind::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryClass {
    Reliable,
    MotionDatagram,
}

/// Which directional half of the connection a frame belongs to. Header byte 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameScope {
    Connection,
    LowerControlsHigher,
    HigherControlsLower,
}

impl FrameScope {
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Connection => 0,
            Self::LowerControlsHigher => 1,
            Self::HigherControlsLower => 2,
        }
    }

    pub fn from_wire(value: u8) -> Result<Self, DecodeError> {
        match value {
            0 => Ok(Self::Connection),
            1 => Ok(Self::LowerControlsHigher),
            2 => Ok(Self::HigherControlsLower),
            _ => Err(DecodeError::InvalidScope),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub epoch: SessionEpoch,
    pub sequence: u64,
    pub message: Message,
    pub scope: FrameScope,
}

impl Frame {
    /// Scope defaults to `Connection`; use `with_scope` for a directional half.
    pub const fn new(epoch: SessionEpoch, sequence: u64, message: Message) -> Self {
        Self {
            epoch,
            sequence,
            message,
            scope: FrameScope::Connection,
        }
    }

    pub fn with_scope(self, scope: FrameScope) -> Self {
        Self { scope, ..self }
    }

    pub fn delivery(&self) -> DeliveryClass {
        self.message.delivery()
    }

    pub fn encoded_len(&self) -> Result<usize, EncodeError> {
        let body_len = body_len(&self.message)?;
        HEADER_LEN
            .checked_add(body_len)
            .filter(|length| *length <= MAX_FRAME_LEN)
            .ok_or(EncodeError::FrameTooLarge)
    }

    /// Clears and reuses `output`, allocating only if its capacity is too small.
    pub fn encode_into(&self, output: &mut Vec<u8>) -> Result<(), EncodeError> {
        encode_into(self, output)
    }
}

pub fn encode_into(frame: &Frame, output: &mut Vec<u8>) -> Result<(), EncodeError> {
    let total_len = frame.encoded_len()?;
    let body_len = total_len - HEADER_LEN;
    let body_len = u16::try_from(body_len).map_err(|_| EncodeError::FrameTooLarge)?;

    output.clear();
    output.reserve(total_len);
    output.extend_from_slice(&MAGIC);
    write_u16(output, PROTOCOL_VERSION);
    output.push(frame.message.kind() as u8);
    output.push(frame.scope.to_wire());
    write_u16(output, body_len);
    write_u16(output, 0);
    write_u64(output, frame.epoch.get());
    write_u64(output, frame.sequence);
    encode_body(&frame.message, output)?;
    debug_assert_eq!(output.len(), total_len);
    Ok(())
}

pub fn decode(input: &[u8]) -> Result<Frame, DecodeError> {
    if input.len() > MAX_FRAME_LEN {
        return Err(DecodeError::FrameTooLarge);
    }
    if input.len() < HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    if input[..4] != MAGIC {
        return Err(DecodeError::InvalidMagic);
    }

    let mut header = Cursor::new(&input[4..HEADER_LEN]);
    let version = header.read_u16()?;
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedVersion);
    }
    let kind = MessageKind::from_wire(header.read_u8()?)?;
    let scope = FrameScope::from_wire(header.read_u8()?)?;
    let body_len = usize::from(header.read_u16()?);
    if header.read_u16()? != 0 {
        return Err(DecodeError::NonZeroReservedField);
    }
    let expected_len = HEADER_LEN
        .checked_add(body_len)
        .ok_or(DecodeError::InvalidLength)?;
    if expected_len != input.len() {
        return Err(DecodeError::InvalidLength);
    }
    let epoch = SessionEpoch::new(header.read_u64()?).map_err(|_| DecodeError::InvalidEpoch)?;
    let sequence = header.read_u64()?;
    debug_assert_eq!(header.remaining(), 0);

    let mut body = Cursor::new(&input[HEADER_LEN..]);
    let message = decode_body(kind, &mut body)?;
    if body.remaining() != 0 {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Frame::new(epoch, sequence, message).with_scope(scope))
}

/// Decodes a transport datagram and rejects any state-changing message. Only
/// absolute motion is safe to drop because newer positions supersede it.
pub fn decode_datagram(input: &[u8]) -> Result<Frame, DatagramDecodeError> {
    let frame = decode(input).map_err(DatagramDecodeError::Decode)?;
    if frame.delivery() != DeliveryClass::MotionDatagram {
        return Err(DatagramDecodeError::ReliableMessage);
    }
    Ok(frame)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    FrameTooLarge,
    InvalidHelloVersion,
    InvalidCapabilities,
    InvalidTopology,
    NonFiniteCoordinate,
    CoordinateOutOfRange,
    InvalidKeyUsage,
    InvalidKeyState,
    InvalidClickCount,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("frame cannot be encoded")
    }
}

impl Error for EncodeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    FrameTooLarge,
    Truncated,
    InvalidMagic,
    UnsupportedVersion,
    UnknownMessageType,
    NonZeroReservedField,
    InvalidLength,
    InvalidEpoch,
    TrailingBytes,
    InvalidBoolean,
    InvalidEnum,
    InvalidCapabilities,
    InvalidTopology,
    InvalidUtf8,
    NonFiniteCoordinate,
    CoordinateOutOfRange,
    InvalidKeyUsage,
    InvalidModifierMask,
    InvalidKeyState,
    InvalidScope,
    InvalidControl,
    InvalidDeclineReason,
    InvalidClickCount,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid protocol frame")
    }
}

impl Error for DecodeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramDecodeError {
    Decode(DecodeError),
    ReliableMessage,
}

impl fmt::Display for DatagramDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid motion datagram")
    }
}

impl Error for DatagramDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::ReliableMessage => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceGateError {
    NotHeartbeat,
    EpochNotActivated,
    EpochNotNew,
    EpochMismatch,
    StaleOrReplay,
}

impl fmt::Display for SequenceGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("frame does not belong to the active session sequence")
    }
}

impl Error for SequenceGateError {}

/// Keeps session activation explicit and sequence spaces separate for the
/// reliable stream and lossy motion datagrams.
#[derive(Clone, Debug, Default)]
pub struct SequenceGate {
    active_epoch: Option<SessionEpoch>,
    reliable_sequence: Option<u64>,
    motion_sequence: Option<u64>,
    /// Heartbeats travel as datagrams in their own sequence space; loss leaves gaps, never replays.
    heartbeat_sequence: Option<u64>,
}

impl SequenceGate {
    pub const fn active_epoch(&self) -> Option<SessionEpoch> {
        self.active_epoch
    }

    pub const fn last_heartbeat(&self) -> Option<u64> {
        self.heartbeat_sequence
    }

    /// Continues the heartbeat space from a previous phase of the same session.
    pub fn seed_heartbeat(&mut self, last: Option<u64>) {
        self.heartbeat_sequence = last;
    }

    /// Activating a later epoch invalidates every delayed packet from prior
    /// sessions. An observed packet never activates an epoch implicitly.
    pub fn activate_epoch(&mut self, epoch: SessionEpoch) -> Result<(), SequenceGateError> {
        if let Some(active_epoch) = self.active_epoch
            && epoch <= active_epoch
        {
            return Err(SequenceGateError::EpochNotNew);
        }
        self.active_epoch = Some(epoch);
        self.reliable_sequence = None;
        self.motion_sequence = None;
        self.heartbeat_sequence = None;
        Ok(())
    }

    pub fn accept(&mut self, frame: &Frame) -> Result<(), SequenceGateError> {
        self.check_epoch(frame)?;
        let last_sequence = match frame.delivery() {
            DeliveryClass::Reliable => &mut self.reliable_sequence,
            DeliveryClass::MotionDatagram => &mut self.motion_sequence,
        };
        Self::advance(last_sequence, frame.sequence)
    }

    /// Admits a Ping or Pong that arrived as a datagram: newer than the last one, gaps allowed.
    pub fn accept_heartbeat(&mut self, frame: &Frame) -> Result<(), SequenceGateError> {
        if !matches!(frame.message, Message::Ping(_) | Message::Pong(_)) {
            return Err(SequenceGateError::NotHeartbeat);
        }
        self.check_epoch(frame)?;
        Self::advance(&mut self.heartbeat_sequence, frame.sequence)
    }

    fn check_epoch(&self, frame: &Frame) -> Result<(), SequenceGateError> {
        if self.active_epoch.is_none() {
            return Err(SequenceGateError::EpochNotActivated);
        }
        if self.active_epoch != Some(frame.epoch) {
            return Err(SequenceGateError::EpochMismatch);
        }
        Ok(())
    }

    fn advance(last_sequence: &mut Option<u64>, sequence: u64) -> Result<(), SequenceGateError> {
        if last_sequence.is_some_and(|last| sequence <= last) {
            return Err(SequenceGateError::StaleOrReplay);
        }
        *last_sequence = Some(sequence);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitConfigError {
    ZeroRate,
    RateTooHigh,
}

impl fmt::Display for RateLimitConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid rate limiter configuration")
    }
}

impl Error for RateLimitConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitError {
    ClockMovedBackwards,
    Exceeded,
}

impl fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("rate limit rejected an event")
    }
}

impl Error for RateLimitError {}

/// A caller-supplied monotonic millisecond token bucket. A new limiter starts
/// full and holds at most one second of event credit.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    rate_per_second: u32,
    credits_milli: u64,
    last_timestamp_ms: Option<u64>,
}

impl RateLimiter {
    pub fn new(rate_per_second: u32) -> Result<Self, RateLimitConfigError> {
        if rate_per_second == 0 {
            return Err(RateLimitConfigError::ZeroRate);
        }
        if rate_per_second > MAX_EVENTS_PER_SECOND {
            return Err(RateLimitConfigError::RateTooHigh);
        }
        Ok(Self {
            rate_per_second,
            credits_milli: u64::from(rate_per_second) * 1_000,
            last_timestamp_ms: None,
        })
    }

    pub const fn rate_per_second(&self) -> u32 {
        self.rate_per_second
    }

    pub fn allow_at(&mut self, timestamp_ms: u64) -> Result<(), RateLimitError> {
        if let Some(last_timestamp_ms) = self.last_timestamp_ms {
            if timestamp_ms < last_timestamp_ms {
                return Err(RateLimitError::ClockMovedBackwards);
            }
            let capacity = u64::from(self.rate_per_second) * 1_000;
            let refill = timestamp_ms
                .saturating_sub(last_timestamp_ms)
                .saturating_mul(u64::from(self.rate_per_second));
            self.credits_milli = self.credits_milli.saturating_add(refill).min(capacity);
        }
        self.last_timestamp_ms = Some(timestamp_ms);
        if self.credits_milli < 1_000 {
            return Err(RateLimitError::Exceeded);
        }
        self.credits_milli -= 1_000;
        Ok(())
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageKind {
    Hello = 1,
    DisplayTopology = 2,
    Motion = 3,
    Button = 4,
    Scroll = 5,
    Key = 6,
    Modifiers = 7,
    ActivateDisplay = 8,
    ReleaseAll = 9,
    Ping = 10,
    Pong = 11,
    Disconnect = 12,
    Error = 13,
    SessionSetup = 14,
    ActivateDisplayAt = 15,
    ActivationAck = 16,
    ReleaseAck = 17,
    SessionReady = 18,
    ActivationDeclined = 19,
    TakeBack = 20,
}

impl MessageKind {
    fn from_wire(value: u8) -> Result<Self, DecodeError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::DisplayTopology),
            3 => Ok(Self::Motion),
            4 => Ok(Self::Button),
            5 => Ok(Self::Scroll),
            6 => Ok(Self::Key),
            7 => Ok(Self::Modifiers),
            8 => Ok(Self::ActivateDisplay),
            9 => Ok(Self::ReleaseAll),
            10 => Ok(Self::Ping),
            11 => Ok(Self::Pong),
            12 => Ok(Self::Disconnect),
            13 => Ok(Self::Error),
            14 => Ok(Self::SessionSetup),
            15 => Ok(Self::ActivateDisplayAt),
            16 => Ok(Self::ActivationAck),
            17 => Ok(Self::ReleaseAck),
            18 => Ok(Self::SessionReady),
            19 => Ok(Self::ActivationDeclined),
            20 => Ok(Self::TakeBack),
            _ => Err(DecodeError::UnknownMessageType),
        }
    }
}

fn body_len(message: &Message) -> Result<usize, EncodeError> {
    let length = match message {
        Message::Hello(hello) => {
            if hello.protocol_version != PROTOCOL_VERSION {
                return Err(EncodeError::InvalidHelloVersion);
            }
            if Capabilities::new(hello.capabilities.bits()).is_err() {
                return Err(EncodeError::InvalidCapabilities);
            }
            24
        }
        Message::SessionSetup(_) => 8,
        Message::DisplayTopology(topology) => {
            topology
                .validate()
                .map_err(|_| EncodeError::InvalidTopology)?;
            topology
                .displays
                .iter()
                .try_fold(TOPOLOGY_PREFIX_LEN, |total, display| {
                    total
                        .checked_add(DISPLAY_FIXED_LEN)
                        .and_then(|value| value.checked_add(display.name.len()))
                        .ok_or(EncodeError::FrameTooLarge)
                })?
        }
        Message::Motion(motion) => {
            let point = match motion {
                Motion::Absolute(point) | Motion::Relative(point) => point,
            };
            validate_input_point(*point).map_err(InputPointError::encode_error)?;
            20
        }
        Message::Button(button) => {
            if button.click_count == 0 {
                return Err(EncodeError::InvalidClickCount);
            }
            4
        }
        Message::Scroll(scroll) => {
            if !scroll.horizontal.is_finite() || !scroll.vertical.is_finite() {
                return Err(EncodeError::NonFiniteCoordinate);
            }
            16
        }
        Message::Key(key) => {
            if !key.usage.is_valid() {
                return Err(EncodeError::InvalidKeyUsage);
            }
            if !key.is_down && key.repeat {
                return Err(EncodeError::InvalidKeyState);
            }
            8
        }
        Message::Modifiers(_) => 4,
        Message::ActivateDisplay(_) => 8,
        Message::ActivateDisplayAt { position, .. } => {
            validate_input_point(*position).map_err(InputPointError::encode_error)?;
            24
        }
        Message::ActivationAck(_) => 8,
        Message::ActivationDeclined { .. } => 16,
        Message::ReleaseAll | Message::ReleaseAck | Message::SessionReady | Message::TakeBack => 0,
        Message::Ping(_) | Message::Pong(_) => 8,
        Message::Disconnect(_) | Message::Error(_) => 4,
    };
    Ok(length)
}

fn encode_body(message: &Message, output: &mut Vec<u8>) -> Result<(), EncodeError> {
    match message {
        Message::Hello(hello) => {
            output.extend_from_slice(&hello.device_id.0);
            output.push(platform_to_wire(hello.platform));
            output.push(0);
            write_u16(output, hello.protocol_version);
            write_u32(output, hello.capabilities.bits());
        }
        Message::SessionSetup(setup) => {
            output.push(session_purpose_to_wire(setup.purpose));
            output.push(setup.control.to_wire());
            output.extend_from_slice(&[0; 6]);
        }
        Message::DisplayTopology(topology) => {
            output.push(topology.displays.len() as u8);
            output.extend_from_slice(&[0; 3]);
            for display in &topology.displays {
                write_u64(output, display.id.0);
                output.push(display.name.len() as u8);
                output.push(u8::from(display.is_primary));
                write_u16(output, 0);
                write_u32(output, display.native_width);
                write_u32(output, display.native_height);
                write_f64(output, display.logical_origin.x);
                write_f64(output, display.logical_origin.y);
                write_f64(output, display.logical_size.x);
                write_f64(output, display.logical_size.y);
                write_f32(output, display.scale_factor);
                output.extend_from_slice(display.name.as_bytes());
                let monitor = display.monitor.unwrap_or(MonitorIdentity {
                    vendor: 0,
                    product: 0,
                    serial: 0,
                });
                write_u16(output, monitor.vendor);
                write_u16(output, monitor.product);
                write_u32(output, monitor.serial);
            }
        }
        Message::Motion(motion) => {
            let (representation, point) = match motion {
                Motion::Absolute(point) => (1, point),
                Motion::Relative(point) => (2, point),
            };
            output.push(representation);
            output.extend_from_slice(&[0; 3]);
            write_f64(output, point.x);
            write_f64(output, point.y);
        }
        Message::Button(button) => {
            output.push(button_to_wire(button.button));
            output.push(u8::from(button.is_down));
            output.push(button.click_count);
            output.push(0);
        }
        Message::Scroll(scroll) => {
            write_f64(output, scroll.horizontal);
            write_f64(output, scroll.vertical);
        }
        Message::Key(key) => {
            write_u16(output, key.usage.0);
            output.push(u8::from(key.is_down));
            output.push(u8::from(key.repeat));
            write_u16(output, u16::from(key.modifiers.0));
            write_u16(output, 0);
        }
        Message::Modifiers(modifiers) => {
            write_u16(output, u16::from(modifiers.0));
            write_u16(output, 0);
        }
        Message::ActivateDisplay(display_id) => write_u64(output, display_id.0),
        Message::ActivateDisplayAt {
            display_id,
            position,
        } => {
            write_u64(output, display_id.0);
            write_f64(output, position.x);
            write_f64(output, position.y);
        }
        Message::ActivationAck(display_id) => write_u64(output, display_id.0),
        Message::ActivationDeclined { display_id, reason } => {
            write_u64(output, display_id.0);
            output.push(decline_reason_to_wire(*reason));
            output.extend_from_slice(&[0; 7]);
        }
        Message::ReleaseAll | Message::ReleaseAck | Message::SessionReady | Message::TakeBack => {}
        Message::Ping(nonce) | Message::Pong(nonce) => write_u64(output, *nonce),
        Message::Disconnect(code) => {
            output.push(disconnect_to_wire(*code));
            output.extend_from_slice(&[0; 3]);
        }
        Message::Error(code) => {
            output.push(error_to_wire(*code));
            output.extend_from_slice(&[0; 3]);
        }
    }
    Ok(())
}

fn decode_body(kind: MessageKind, body: &mut Cursor<'_>) -> Result<Message, DecodeError> {
    match kind {
        MessageKind::Hello => {
            let mut device_id = [0; 16];
            device_id.copy_from_slice(body.take(16)?);
            let platform = platform_from_wire(body.read_u8()?)?;
            body.require_zero(1)?;
            let hello_version = body.read_u16()?;
            if hello_version != PROTOCOL_VERSION {
                return Err(DecodeError::UnsupportedVersion);
            }
            let capabilities = Capabilities::new(body.read_u32()?)
                .map_err(|_| DecodeError::InvalidCapabilities)?;
            Ok(Message::Hello(Hello {
                device_id: DeviceId(device_id),
                platform,
                protocol_version: hello_version,
                capabilities,
            }))
        }
        MessageKind::SessionSetup => {
            let purpose = session_purpose_from_wire(body.read_u8()?)?;
            let control = ControlPermissions::from_wire(body.read_u8()?)?;
            body.require_zero(6)?;
            if purpose == SessionPurpose::Setup && control != ControlPermissions::BOTH {
                return Err(DecodeError::InvalidControl);
            }
            Ok(Message::SessionSetup(SessionSetup { purpose, control }))
        }
        MessageKind::DisplayTopology => {
            let count = usize::from(body.read_u8()?);
            body.require_zero(3)?;
            if count == 0 || count > MAX_DISPLAYS {
                return Err(DecodeError::InvalidTopology);
            }
            let mut displays = Vec::with_capacity(count);
            for _ in 0..count {
                let id = DisplayId(body.read_u64()?);
                let name_len = usize::from(body.read_u8()?);
                let is_primary = body.read_bool()?;
                body.require_zero(2)?;
                let native_width = body.read_u32()?;
                let native_height = body.read_u32()?;
                let logical_origin = Point::new(body.read_f64()?, body.read_f64()?);
                let logical_size = Point::new(body.read_f64()?, body.read_f64()?);
                let scale_factor = body.read_f32()?;
                if name_len > MAX_DISPLAY_NAME_BYTES {
                    return Err(DecodeError::InvalidTopology);
                }
                let name = std::str::from_utf8(body.take(name_len)?)
                    .map_err(|_| DecodeError::InvalidUtf8)?
                    .to_owned();
                let vendor = body.read_u16()?;
                let product = body.read_u16()?;
                let serial = body.read_u32()?;
                let monitor = MonitorIdentity::new(vendor, product, serial);
                displays.push(DisplayDescription {
                    id,
                    name,
                    native_width,
                    native_height,
                    logical_origin,
                    logical_size,
                    scale_factor,
                    is_primary,
                    monitor,
                });
            }
            let topology =
                DisplayTopology::new(displays).map_err(|_| DecodeError::InvalidTopology)?;
            Ok(Message::DisplayTopology(topology))
        }
        MessageKind::Motion => {
            let representation = body.read_u8()?;
            body.require_zero(3)?;
            let point = Point::new(body.read_f64()?, body.read_f64()?);
            validate_input_point(point).map_err(InputPointError::decode_error)?;
            match representation {
                1 => Ok(Message::Motion(Motion::Absolute(point))),
                2 => Ok(Message::Motion(Motion::Relative(point))),
                _ => Err(DecodeError::InvalidEnum),
            }
        }
        MessageKind::Button => {
            let button = button_from_wire(body.read_u8()?)?;
            let is_down = body.read_bool()?;
            let click_count = body.read_u8()?;
            if click_count == 0 {
                return Err(DecodeError::InvalidClickCount);
            }
            body.require_zero(1)?;
            Ok(Message::Button(Button {
                button,
                is_down,
                click_count,
            }))
        }
        MessageKind::Scroll => Ok(Message::Scroll(Scroll {
            horizontal: body.read_f64()?,
            vertical: body.read_f64()?,
        })),
        MessageKind::Key => {
            let usage = HidUsage(body.read_u16()?);
            if !usage.is_valid() {
                return Err(DecodeError::InvalidKeyUsage);
            }
            let is_down = body.read_bool()?;
            let repeat = body.read_bool()?;
            let modifiers = read_modifiers(body)?;
            body.require_zero(2)?;
            if !is_down && repeat {
                return Err(DecodeError::InvalidKeyState);
            }
            Ok(Message::Key(Key {
                usage,
                is_down,
                repeat,
                modifiers,
            }))
        }
        MessageKind::Modifiers => {
            let modifiers = read_modifiers(body)?;
            body.require_zero(2)?;
            Ok(Message::Modifiers(modifiers))
        }
        MessageKind::ActivateDisplay => Ok(Message::ActivateDisplay(DisplayId(body.read_u64()?))),
        MessageKind::ActivateDisplayAt => {
            let display_id = DisplayId(body.read_u64()?);
            let position = Point::new(body.read_f64()?, body.read_f64()?);
            validate_input_point(position).map_err(InputPointError::decode_error)?;
            Ok(Message::ActivateDisplayAt {
                display_id,
                position,
            })
        }
        MessageKind::ActivationAck => Ok(Message::ActivationAck(DisplayId(body.read_u64()?))),
        MessageKind::ActivationDeclined => {
            let display_id = DisplayId(body.read_u64()?);
            let reason = decline_reason_from_wire(body.read_u8()?)?;
            body.require_zero(7)?;
            Ok(Message::ActivationDeclined { display_id, reason })
        }
        MessageKind::ReleaseAll => Ok(Message::ReleaseAll),
        MessageKind::ReleaseAck => Ok(Message::ReleaseAck),
        MessageKind::SessionReady => Ok(Message::SessionReady),
        MessageKind::TakeBack => Ok(Message::TakeBack),
        MessageKind::Ping => Ok(Message::Ping(body.read_u64()?)),
        MessageKind::Pong => Ok(Message::Pong(body.read_u64()?)),
        MessageKind::Disconnect => {
            let code = disconnect_from_wire(body.read_u8()?)?;
            body.require_zero(3)?;
            Ok(Message::Disconnect(code))
        }
        MessageKind::Error => {
            let code = error_from_wire(body.read_u8()?)?;
            body.require_zero(3)?;
            Ok(Message::Error(code))
        }
    }
}

fn read_modifiers(body: &mut Cursor<'_>) -> Result<ModifierState, DecodeError> {
    let raw = body.read_u16()?;
    if raw & !MODIFIER_MASK != 0 {
        return Err(DecodeError::InvalidModifierMask);
    }
    Ok(ModifierState(raw as u8))
}

#[derive(Clone, Copy)]
enum InputPointError {
    NonFinite,
    OutOfRange,
}

impl InputPointError {
    const fn encode_error(self) -> EncodeError {
        match self {
            Self::NonFinite => EncodeError::NonFiniteCoordinate,
            Self::OutOfRange => EncodeError::CoordinateOutOfRange,
        }
    }

    const fn decode_error(self) -> DecodeError {
        match self {
            Self::NonFinite => DecodeError::NonFiniteCoordinate,
            Self::OutOfRange => DecodeError::CoordinateOutOfRange,
        }
    }
}

fn validate_input_point(point: Point) -> Result<(), InputPointError> {
    if !point.is_finite() {
        return Err(InputPointError::NonFinite);
    }
    if point.x.abs() > MAX_LOGICAL_ORIGIN_ABS || point.y.abs() > MAX_LOGICAL_ORIGIN_ABS {
        return Err(InputPointError::OutOfRange);
    }
    Ok(())
}

fn platform_to_wire(platform: Platform) -> u8 {
    match platform {
        Platform::Windows => 1,
        Platform::MacOs => 2,
    }
}

fn platform_from_wire(value: u8) -> Result<Platform, DecodeError> {
    match value {
        1 => Ok(Platform::Windows),
        2 => Ok(Platform::MacOs),
        _ => Err(DecodeError::InvalidEnum),
    }
}

fn session_purpose_to_wire(purpose: SessionPurpose) -> u8 {
    match purpose {
        SessionPurpose::Share => 1,
        SessionPurpose::Setup => 2,
    }
}

fn session_purpose_from_wire(value: u8) -> Result<SessionPurpose, DecodeError> {
    match value {
        1 => Ok(SessionPurpose::Share),
        2 => Ok(SessionPurpose::Setup),
        _ => Err(DecodeError::InvalidEnum),
    }
}

fn decline_reason_to_wire(reason: DeclineReason) -> u8 {
    reason as u8
}

fn decline_reason_from_wire(value: u8) -> Result<DeclineReason, DecodeError> {
    match value {
        1 => Ok(DeclineReason::Contended),
        2 => Ok(DeclineReason::Busy),
        3 => Ok(DeclineReason::Disabled),
        _ => Err(DecodeError::InvalidDeclineReason),
    }
}

fn button_to_wire(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
    }
}

fn button_from_wire(value: u8) -> Result<MouseButton, DecodeError> {
    match value {
        1 => Ok(MouseButton::Left),
        2 => Ok(MouseButton::Right),
        3 => Ok(MouseButton::Middle),
        4 => Ok(MouseButton::Back),
        5 => Ok(MouseButton::Forward),
        _ => Err(DecodeError::InvalidEnum),
    }
}

fn disconnect_to_wire(code: DisconnectCode) -> u8 {
    match code {
        DisconnectCode::Requested => 1,
        DisconnectCode::TransportLost => 2,
        DisconnectCode::ProtocolViolation => 3,
        DisconnectCode::SessionExpired => 4,
    }
}

fn disconnect_from_wire(value: u8) -> Result<DisconnectCode, DecodeError> {
    match value {
        1 => Ok(DisconnectCode::Requested),
        2 => Ok(DisconnectCode::TransportLost),
        3 => Ok(DisconnectCode::ProtocolViolation),
        4 => Ok(DisconnectCode::SessionExpired),
        _ => Err(DecodeError::InvalidEnum),
    }
}

fn error_to_wire(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::MalformedFrame => 1,
        ErrorCode::UnsupportedVersion => 2,
        ErrorCode::SessionExpired => 3,
        ErrorCode::RateLimited => 4,
        ErrorCode::ProtocolViolation => 5,
    }
}

fn error_from_wire(value: u8) -> Result<ErrorCode, DecodeError> {
    match value {
        1 => Ok(ErrorCode::MalformedFrame),
        2 => Ok(ErrorCode::UnsupportedVersion),
        3 => Ok(ErrorCode::SessionExpired),
        4 => Ok(ErrorCode::RateLimited),
        5 => Ok(ErrorCode::ProtocolViolation),
        _ => Err(DecodeError::InvalidEnum),
    }
}

fn write_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn write_f32(output: &mut Vec<u8>, value: f32) {
    write_u32(output, value.to_bits());
}

fn write_f64(output: &mut Vec<u8>, value: f64) {
    write_u64(output, value.to_bits());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(DecodeError::Truncated)?;
        let result = &self.bytes[self.position..end];
        self.position = end;
        Ok(result)
    }

    fn require_zero(&mut self, count: usize) -> Result<(), DecodeError> {
        if self.take(count)?.iter().any(|byte| *byte != 0) {
            return Err(DecodeError::NonZeroReservedField);
        }
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, DecodeError> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn read_f32(&mut self) -> Result<f32, DecodeError> {
        let value = f32::from_bits(self.read_u32()?);
        if !value.is_finite() {
            return Err(DecodeError::NonFiniteCoordinate);
        }
        Ok(value)
    }

    fn read_f64(&mut self) -> Result<f64, DecodeError> {
        let value = f64::from_bits(self.read_u64()?);
        if !value.is_finite() {
            return Err(DecodeError::NonFiniteCoordinate);
        }
        Ok(value)
    }

    fn read_bool(&mut self) -> Result<bool, DecodeError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidBoolean),
        }
    }
}
