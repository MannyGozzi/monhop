//! Quartz event decoding for explicit macOS capture, pure but for a button's capture-clock stamp.
//!
//! This module does not touch Core Graphics. The owned capture thread supplies copied fields from
//! its event-tap callback, which keeps the policy testable on non-macOS hosts.

use std::fmt;

use monhop_core::{
    GestureKind, GesturePhase, HidUsage, LogicalRect, ModifierState, MouseButton, Point,
    PointerGesture, SystemGesture, TakeBackGate, capture::CaptureEvent,
    capture_physical::LocalTransfer, clicks::capture_clock, gesture_latch::TouchRecord,
};

use crate::{MacVirtualKey, mac_modifier_flags_from_held_keys, mac_virtual_key_to_hid};

/// Core Graphics `kCGEventKeyDown`.
pub const CG_EVENT_KEY_DOWN: u32 = 10;
/// Core Graphics `kCGEventKeyUp`.
pub const CG_EVENT_KEY_UP: u32 = 11;
/// Core Graphics `kCGEventFlagsChanged`.
pub const CG_EVENT_FLAGS_CHANGED: u32 = 12;
/// Core Graphics `kCGEventScrollWheel`.
pub const CG_EVENT_SCROLL_WHEEL: u32 = 22;
/// Core Graphics `kCGEventMouseMoved`.
pub const CG_EVENT_MOUSE_MOVED: u32 = 5;
/// Core Graphics `kCGEventLeftMouseDown`.
pub const CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
/// Core Graphics `kCGEventLeftMouseUp`.
pub const CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
/// Core Graphics `kCGEventRightMouseDown`.
pub const CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
/// Core Graphics `kCGEventRightMouseUp`.
pub const CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
/// Core Graphics `kCGEventLeftMouseDragged`.
pub const CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
/// Core Graphics `kCGEventRightMouseDragged`.
pub const CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
/// Core Graphics `kCGEventOtherMouseDown`.
pub const CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
/// Core Graphics `kCGEventOtherMouseUp`.
pub const CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
/// Core Graphics `kCGEventOtherMouseDragged`.
pub const CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
/// Private `kCGSEventGesture`: pinch, rotation, smart zoom, pressure and touch-session records.
pub const CG_EVENT_GESTURE: u32 = 29;
/// Private `kCGSEventDockControl`: the Dock's Spaces, Mission Control and Show Desktop swipes.
pub const CG_EVENT_DOCK_CONTROL: u32 = 30;
/// Private `kCGSEventFluidTouchGesture`.
pub const CG_EVENT_FLUID_TOUCH_GESTURE: u32 = 31;

/// Cumulative Dock-swipe progress at which the swipe fires its system gesture, mid-swipe.
pub const DOCK_SWIPE_COMMIT_PROGRESS: f64 = 0.15;
/// Axis velocity that still commits a swipe lifted short of its progress. Unverified units.
pub const DOCK_SWIPE_COMMIT_VELOCITY: f64 = 0.5;
// Progress sign (field 124) meaning the first gesture of each motion's pair; the opposite sign
// means the second. Unverified on macOS 27, where these flipped before: flip them here.
/// Horizontal: `DesktopNext`, else `DesktopPrevious`.
pub const DOCK_SWIPE_HORIZONTAL_NEXT_SIGN: f64 = -1.0;
/// Vertical: `Overview` (fingers up), else `AppWindows`.
pub const DOCK_SWIPE_VERTICAL_UP_SIGN: f64 = -1.0;
/// Thumb and three fingers: `ShowDesktop` (spread), else `Launcher`.
pub const DOCK_SWIPE_SCALE_SPREAD_SIGN: f64 = 1.0;

// Field 110, the IOHID event type behind a gesture record.
const HID_ROTATION: i64 = 5;
const HID_SCROLL: i64 = 6;
const HID_ZOOM: i64 = 8;
const HID_ZOOM_TOGGLE: i64 = 22;
const HID_DOCK_SWIPE: i64 = 23;
const HID_FORCE: i64 = 32;
const HID_TOUCH_STARTED: i64 = 61;
const HID_TOUCH_ENDED: i64 = 62;

// Field 132.
const NATIVE_PHASE_BEGAN: i64 = 1;
const NATIVE_PHASE_CHANGED: i64 = 2;
const NATIVE_PHASE_ENDED: i64 = 4;
const NATIVE_PHASE_CANCELLED: i64 = 8;
const NATIVE_PHASE_MAY_BEGIN: i64 = 128;

// Field 123 on a Dock swipe.
const MOTION_HORIZONTAL: i64 = 1;
const MOTION_VERTICAL: i64 = 2;
const MOTION_SCALE: i64 = 3;

/// Field 143 once a press goes deep enough for Look Up.
const FORCE_CLICK_STAGE: i64 = 2;

/// Core Graphics `kCGEventSourceStateHIDSystemState`.
pub const CG_EVENT_SOURCE_STATE_HID_SYSTEM: i64 = 1;

/// Caps Lock is the one flags-changed key that is not a held modifier.
const HID_CAPS_LOCK: HidUsage = HidUsage(0x39);

/// MonHop's fixed conversion for a discrete Quartz scroll line.
///
/// Quartz flags a non-continuous scroll as line-based. MonHop normalizes that line to the same
/// logical-point unit that continuous Quartz point deltas use, so downstream conversion to signed
/// detents is well defined without treating the 16.16 fixed-point value as pixels.
pub const QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE: f64 = 40.0;

/// Copied event-source metadata used to reject synthetic input before callback-local state.
#[derive(Clone, Copy)]
pub struct EventSourceMetadata {
    pub user_data: i64,
    pub state_id: i64,
}

/// A copied Quartz record that cannot affect the physical-input ledger.
pub enum DecodedInput {
    /// Synthetic, or a record MonHop does not track: local apps receive it on either route.
    Ignored,
    /// A physical record with no delta to forward, such as a sub-pixel move or a scroll phase.
    /// Withheld while remote, where it would move the pinned cursor or scroll the app under it.
    Empty,
    /// A physical key or button MonHop cannot forward: counted, and local apps receive it.
    Unsupported,
    /// Field values no physical record carries; capture stops.
    Malformed,
    Event(CaptureEvent),
}

/// One decoded pointer record and the accepted logical cursor position to retain locally.
pub struct DecodedPointer {
    /// The local-route cursor anchor. The native callback admits this before the relative delta.
    pub local_absolute: Option<CaptureEvent>,
    pub input: DecodedInput,
    pub position: Option<Point>,
}

/// Copied Core Graphics fields for one pointer callback. Deltas come from the double-valued
/// fields, so any sub-point motion Quartz reports survives.
#[derive(Clone, Copy)]
pub struct PointerFields {
    pub location: Point,
    pub delta_x: f64,
    pub delta_y: f64,
    pub button_number: i64,
}

/// Active logical display rectangles cached by the capture owner thread.
///
/// This is never read or mutated by the event callback. The owner refreshes it immediately before
/// posting a local restore point, so a stale topology cannot produce an unchecked cursor warp.
#[derive(Clone)]
pub struct ActiveDisplayBounds {
    rectangles: Vec<LogicalRect>,
}

impl ActiveDisplayBounds {
    pub fn from_rectangles(rectangles: impl IntoIterator<Item = LogicalRect>) -> Option<Self> {
        let rectangles: Vec<_> = rectangles.into_iter().collect();
        if rectangles.is_empty()
            || rectangles
                .iter()
                .any(|rectangle| !valid_rectangle(*rectangle))
        {
            return None;
        }
        Some(Self { rectangles })
    }

    /// Uses the same half-open desktop convention as the topology and Core Graphics display bounds.
    pub fn contains(&self, point: Point) -> bool {
        self.rectangles
            .iter()
            .any(|rectangle| rectangle.contains_half_open(point))
    }
}

/// Locally delivered modifier state used only to set flags on owner-thread transfer posting.
///
/// The bits retain left and right HID usages independently, while Quartz flags combine each pair.
#[derive(Clone, Copy, Default)]
pub struct LocalModifierState {
    held: u8,
}

impl LocalModifierState {
    pub fn record_local_key(&mut self, usage: HidUsage, pressed: bool) {
        set_modifier(&mut self.held, usage, pressed);
    }

    /// Returns the exact flags after `transfer`, without committing it before submission succeeds.
    pub fn flags_after_transfer(&self, transfer: LocalTransfer) -> u64 {
        let mut after = *self;
        after.apply_transfer_success(transfer);
        after.flags()
    }

    /// Commits only an event that Core Graphics accepted for submission.
    pub fn apply_transfer_success(&mut self, transfer: LocalTransfer) {
        if let LocalTransfer::Key { usage, pressed } = transfer {
            self.record_local_key(usage, pressed);
        }
    }

    pub fn flags(self) -> u64 {
        mac_modifier_flags_from_held_keys((0_u16..8).filter_map(|offset| {
            (self.held & (1_u8 << offset) != 0).then_some(HidUsage(0xe0 + offset))
        }))
    }
}

/// HID system state copied for one flags-changed record.
#[derive(Clone, Copy, Default)]
pub struct HidKeyState {
    /// `CGEventSourceKeyState(HIDSystemState)` for the record's key.
    pub down: bool,
    /// Injected input was down or changed during the read, so `down` can count an injected copy.
    pub injection_held: bool,
}

impl HidKeyState {
    /// Runs the HID read between two samples of `gate`'s injection bookkeeping. The read is
    /// physical only if nothing injected was held at its start and nothing changed until its end.
    pub fn sample(gate: Option<&TakeBackGate>, read: impl FnOnce() -> bool) -> Self {
        let Some(gate) = gate else {
            return Self {
                down: read(),
                injection_held: false,
            };
        };
        let changes = gate.injection_changes();
        let held = gate.injected_held();
        let down = read();
        Self {
            down,
            injection_held: held || gate.injection_changes() != changes,
        }
    }
}

/// Physical side-specific modifier state for flags-changed records.
///
/// HID system state also counts injected copies of a key. While no injected input overlaps the
/// read, a record takes HID state and resyncs its bit; otherwise the record toggles the bit,
/// seeded at capture start.
#[derive(Clone, Copy, Default)]
pub struct PhysicalModifierLedger {
    held: u8,
    /// Modifiers MonHop itself posted down and has not posted up.
    posted: u8,
}

impl PhysicalModifierLedger {
    /// Capture start only. Usages other than the eight side-specific modifiers are ignored.
    pub fn seed_held(&mut self, usage: HidUsage) {
        set_modifier(&mut self.held, usage, true);
    }

    /// Records a local transfer the capture posted, whose copy HID state then also counts.
    pub fn record_posted(&mut self, transfer: LocalTransfer) {
        if let LocalTransfer::Key { usage, pressed } = transfer {
            set_modifier(&mut self.posted, usage, pressed);
        }
    }

    /// The modifier's state after one physical flags-changed record; `None` if it is not tracked.
    fn flags_changed(&mut self, usage: HidUsage, hid: HidKeyState) -> Option<bool> {
        let bit = modifier_bit(usage)?;
        let down = if hid.injection_held || self.posted & bit != 0 {
            self.held & bit == 0
        } else {
            hid.down
        };
        set_modifier(&mut self.held, usage, down);
        Some(down)
    }
}

/// Returns whether the callback must pass an event through before acquiring callback-local state.
///
/// The MonHop marker rejects this process's injection. Requiring the HID-system source rejects
/// private and combined-session event sources, which conservatively excludes normal Quartz
/// synthetic input from other applications as well.
pub fn should_ignore_source(source: EventSourceMetadata, synthetic_marker: i64) -> bool {
    source.user_data == synthetic_marker || source.state_id != CG_EVENT_SOURCE_STATE_HID_SYSTEM
}

/// Keeps the event tap only for releases of presses that MonHop already suppressed.
///
/// `interception_failed` is distinct from the terminal stop reason: a topology or posting
/// failure can stop routing while Quartz still delivers releases, whereas a disabled tap cannot.
pub const fn should_keep_quarantine_tap(
    has_suppressed_presses: bool,
    interception_failed: bool,
) -> bool {
    has_suppressed_presses && !interception_failed
}

/// Decodes a physical Quartz key record without inferring modifier state or repeat state.
///
/// Marked and non-HID records return before `modifiers` changes. Flags-changed records other than
/// the eight side-specific modifiers and Caps Lock are ignored, never unsupported. The shared
/// physical ledger derives repeat and modifiers after the key is admitted.
pub fn decode_keyboard(
    event_type: u32,
    virtual_key: u16,
    modifiers: &mut PhysicalModifierLedger,
    hid: HidKeyState,
    source: EventSourceMetadata,
    synthetic_marker: i64,
) -> DecodedInput {
    if should_ignore_source(source, synthetic_marker) {
        return DecodedInput::Ignored;
    }
    let key_pressed = match event_type {
        CG_EVENT_KEY_DOWN => Some(true),
        CG_EVENT_KEY_UP => Some(false),
        CG_EVENT_FLAGS_CHANGED => None,
        _ => return DecodedInput::Ignored,
    };
    let Ok(usage) = mac_virtual_key_to_hid(MacVirtualKey(virtual_key)) else {
        return match key_pressed {
            Some(_) => DecodedInput::Unsupported,
            None => DecodedInput::Ignored,
        };
    };
    let pressed = match key_pressed {
        Some(pressed) => {
            set_modifier(&mut modifiers.held, usage, pressed);
            pressed
        }
        None => match modifiers.flags_changed(usage, hid) {
            Some(down) => down,
            None if usage == HID_CAPS_LOCK => hid.down,
            None => return DecodedInput::Ignored,
        },
    };
    DecodedInput::Event(CaptureEvent::Key {
        usage,
        pressed,
        repeat: false,
        modifiers: ModifierState::default(),
    })
}

/// Moved and dragged records: the pointer types that carry a relative delta.
pub const fn is_pointer_motion(event_type: u32) -> bool {
    matches!(
        event_type,
        CG_EVENT_MOUSE_MOVED
            | CG_EVENT_LEFT_MOUSE_DRAGGED
            | CG_EVENT_RIGHT_MOUSE_DRAGGED
            | CG_EVENT_OTHER_MOUSE_DRAGGED
    )
}

/// Decodes a physical Quartz pointer record.
///
/// Quartz locations are logical macOS desktop points. Every accepted movement returns a local
/// absolute anchor plus an independent `kCGMouseEventDeltaX/Y` delta. The native callback queues
/// the anchor before the delta only while local, so source edge routing sees the clamped position
/// and its outward intent without adding the motion twice.
pub fn decode_pointer(
    event_type: u32,
    fields: PointerFields,
    previous_position: Option<Point>,
    source: EventSourceMetadata,
    synthetic_marker: i64,
) -> DecodedPointer {
    if should_ignore_source(source, synthetic_marker) {
        return DecodedPointer {
            local_absolute: None,
            input: DecodedInput::Ignored,
            position: previous_position,
        };
    }
    if !fields.location.is_finite() {
        return DecodedPointer {
            local_absolute: None,
            input: DecodedInput::Malformed,
            position: previous_position,
        };
    }
    let position = Some(fields.location);
    let (local_absolute, input) = match event_type {
        motion if is_pointer_motion(motion) => {
            let (dx, dy) = (fields.delta_x, fields.delta_y);
            if !dx.is_finite() || !dy.is_finite() {
                return DecodedPointer {
                    local_absolute: None,
                    input: DecodedInput::Malformed,
                    position: previous_position,
                };
            }
            let input = if dx == 0.0 && dy == 0.0 {
                DecodedInput::Empty
            } else {
                DecodedInput::Event(CaptureEvent::LogicalRelativeMotion { dx, dy })
            };
            (
                Some(CaptureEvent::LogicalAbsoluteMotion {
                    x: fields.location.x,
                    y: fields.location.y,
                }),
                input,
            )
        }
        CG_EVENT_LEFT_MOUSE_DOWN => (None, button(MouseButton::Left, true)),
        CG_EVENT_LEFT_MOUSE_UP => (None, button(MouseButton::Left, false)),
        CG_EVENT_RIGHT_MOUSE_DOWN => (None, button(MouseButton::Right, true)),
        CG_EVENT_RIGHT_MOUSE_UP => (None, button(MouseButton::Right, false)),
        CG_EVENT_OTHER_MOUSE_DOWN => (None, mouse_button(fields.button_number, true)),
        CG_EVENT_OTHER_MOUSE_UP => (None, mouse_button(fields.button_number, false)),
        _ => {
            return DecodedPointer {
                local_absolute: None,
                input: DecodedInput::Ignored,
                position: previous_position,
            };
        }
    };
    DecodedPointer {
        local_absolute,
        input,
        position,
    }
}

/// Decodes Quartz scroll values into logical macOS points without axis-sign conversion.
///
/// Continuous events use Quartz's pixel/point fields directly. Discrete events arrive as 16.16
/// fixed-point line values, so they are explicitly normalized to logical points instead of being
/// mislabeled as pixels. The runtime owns any destination-specific signed-detent conversion.
pub fn decode_scroll(
    horizontal: f64,
    vertical: f64,
    continuous: bool,
    source: EventSourceMetadata,
    synthetic_marker: i64,
) -> DecodedInput {
    if should_ignore_source(source, synthetic_marker) {
        return DecodedInput::Ignored;
    }
    if !horizontal.is_finite() || !vertical.is_finite() {
        return DecodedInput::Malformed;
    }
    let (horizontal, vertical) = if continuous {
        (horizontal, vertical)
    } else {
        (
            horizontal * QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE,
            vertical * QUARTZ_POINTS_PER_DISCRETE_SCROLL_LINE,
        )
    };
    if !horizontal.is_finite() || !vertical.is_finite() {
        return DecodedInput::Malformed;
    }
    if horizontal == 0.0 && vertical == 0.0 {
        return DecodedInput::Empty;
    }
    DecodedInput::Event(CaptureEvent::LogicalScroll {
        horizontal,
        vertical,
    })
}

/// Copied fields of one private gesture record. Every one is private and may change meaning, so
/// no value here can make decoding fail: an unknown one only makes the record unmapped.
#[derive(Clone, Copy, Debug, Default)]
pub struct GestureFields {
    pub event_type: u32,
    /// Field 110: the IOHID event type behind the record.
    pub hid_type: i64,
    /// Field 132.
    pub phase: i64,
    /// Field 113: the pinch delta; the view's scale multiplies by `1 + zoom`. It shares its slot
    /// with field 114, so each is meaningful only on its own record kind.
    pub zoom: f64,
    /// Field 114: degrees, counterclockwise positive.
    pub rotation: f64,
    /// Field 123: the Dock swipe's axis.
    pub motion: i64,
    /// Field 124: cumulative Dock-swipe progress, signed by direction.
    pub progress: f64,
    /// Fields 129 and 130.
    pub velocity_x: f64,
    pub velocity_y: f64,
    /// Field 143: the pressure stage of a click.
    pub stage: i64,
}

/// What one gesture record means for local apps and for the other computer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecodedGesture {
    /// How the record relates to the touch session the capture latch keeps on one route.
    pub record: TouchRecord,
    pub output: GestureOutput,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureOutput {
    /// Latch only: a session boundary, a scroll companion, or a swipe short of its commit.
    Quiet,
    /// Valid by construction: gestures come only from [`PointerGesture::from_parts`].
    Event(CaptureEvent),
    /// A native gesture MonHop cannot map. Withheld while remote and counted, never a failure.
    Unmapped,
}

/// Why a Dock swipe or pinch ended, for its diagnostic line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureEnd {
    Lifted,
    Cancelled,
    /// A new one began before this one's end arrived.
    Lost,
}

impl fmt::Display for GestureEnd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Lifted => "lift",
            Self::Cancelled => "cancel",
            Self::Lost => "lost end",
        })
    }
}

/// One finished Dock swipe or pinch, logged once so a hardware run can pin the unverified signs
/// and thresholds. Values only: no position or key identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureSummary {
    DockSwipe {
        motion: i64,
        progress: f64,
        peak_velocity_x: f64,
        peak_velocity_y: f64,
        fired: Option<SystemGesture>,
        end: GestureEnd,
    },
    Pinch {
        records: u32,
        scale: f64,
        end: GestureEnd,
    },
}

impl fmt::Display for GestureSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::DockSwipe {
                motion,
                progress,
                peak_velocity_x,
                peak_velocity_y,
                fired,
                end,
            } => {
                write!(
                    formatter,
                    "dock swipe ended ({end}): motion {motion}, final progress {progress:.3}, \
                     peak velocity x {peak_velocity_x:.3} y {peak_velocity_y:.3}, fired "
                )?;
                match fired {
                    Some(gesture) => write!(formatter, "{gesture:?}"),
                    None => formatter.write_str("none"),
                }
            }
            Self::Pinch {
                records,
                scale,
                end,
            } => write!(
                formatter,
                "pinch ended ({end}): {records} records, total scale {scale:.3}"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DockSwipe {
    motion: i64,
    progress: f64,
    /// Along the motion's axis; a Scale swipe has none.
    velocity: f64,
    peak_velocity_x: f64,
    peak_velocity_y: f64,
    fired: Option<SystemGesture>,
}

impl DockSwipe {
    const fn new(motion: i64) -> Self {
        Self {
            motion,
            progress: 0.0,
            velocity: 0.0,
            peak_velocity_x: 0.0,
            peak_velocity_y: 0.0,
            fired: None,
        }
    }

    /// A terminal record may carry zeroed values; the last real ones decide it.
    fn observe(&mut self, fields: GestureFields, terminal: bool) {
        let kept = |value: f64| value.is_finite() && !(terminal && value == 0.0);
        if kept(fields.progress) {
            self.progress = fields.progress;
        }
        let velocity = match self.motion {
            MOTION_HORIZONTAL => fields.velocity_x,
            MOTION_VERTICAL => fields.velocity_y,
            _ => 0.0,
        };
        if kept(velocity) {
            self.velocity = velocity;
        }
        if fields.velocity_x.is_finite() {
            self.peak_velocity_x = self.peak_velocity_x.max(fields.velocity_x.abs());
        }
        if fields.velocity_y.is_finite() {
            self.peak_velocity_y = self.peak_velocity_y.max(fields.velocity_y.abs());
        }
    }

    fn commits(&self, phase: GesturePhase) -> bool {
        match phase {
            GesturePhase::Cancelled => false,
            GesturePhase::Ended if self.progress.abs() < DOCK_SWIPE_COMMIT_PROGRESS => {
                self.progress != 0.0
                    && self.velocity * self.progress.signum() >= DOCK_SWIPE_COMMIT_VELOCITY
            }
            _ => self.progress.abs() >= DOCK_SWIPE_COMMIT_PROGRESS,
        }
    }

    fn gesture(&self) -> SystemGesture {
        let (sign, first, second) = match self.motion {
            MOTION_HORIZONTAL => (
                DOCK_SWIPE_HORIZONTAL_NEXT_SIGN,
                SystemGesture::DesktopNext,
                SystemGesture::DesktopPrevious,
            ),
            MOTION_VERTICAL => (
                DOCK_SWIPE_VERTICAL_UP_SIGN,
                SystemGesture::Overview,
                SystemGesture::AppWindows,
            ),
            _ => (
                DOCK_SWIPE_SCALE_SPREAD_SIGN,
                SystemGesture::ShowDesktop,
                SystemGesture::Launcher,
            ),
        };
        if self.progress * sign > 0.0 {
            first
        } else {
            second
        }
    }

    const fn summary(self, end: GestureEnd) -> GestureSummary {
        GestureSummary::DockSwipe {
            motion: self.motion,
            progress: self.progress,
            peak_velocity_x: self.peak_velocity_x,
            peak_velocity_y: self.peak_velocity_y,
            fired: self.fired,
            end,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Pinch {
    records: u32,
    scale: f64,
}

impl Pinch {
    const fn summary(self, end: GestureEnd) -> GestureSummary {
        GestureSummary::Pinch {
            records: self.records,
            scale: self.scale,
            end,
        }
    }
}

/// Gesture state that spans records: the open Dock swipe and pinch and the current press.
#[derive(Debug, Default)]
pub struct GestureDecoder {
    swipe: Option<DockSwipe>,
    pinch: Option<Pinch>,
    force_clicked: bool,
    finished_swipe: Option<GestureSummary>,
    finished_pinch: Option<GestureSummary>,
}

impl GestureDecoder {
    /// Swipes and pinches finished since the last call, for the owner thread to log.
    pub fn take_finished(&mut self) -> impl Iterator<Item = GestureSummary> + use<> {
        [self.finished_swipe.take(), self.finished_pinch.take()]
            .into_iter()
            .flatten()
    }

    /// A Dock swipe fires its one system gesture at the first record whose progress reaches the
    /// commit, or at a short lift whose velocity agrees; never on Cancelled.
    fn dock_swipe(&mut self, fields: GestureFields) -> GestureOutput {
        let phase = match native_phase(fields.phase) {
            Some(NativePhase::Gesture(phase)) => phase,
            Some(NativePhase::MayBegin) => return GestureOutput::Quiet,
            None => return GestureOutput::Unmapped,
        };
        if phase == GesturePhase::Began || (self.swipe.is_none() && !phase.is_terminal()) {
            // The motion is fixed when a swipe starts; an edge swipe's is one MonHop cannot map.
            if !matches!(
                fields.motion,
                MOTION_HORIZONTAL | MOTION_VERTICAL | MOTION_SCALE
            ) {
                return GestureOutput::Unmapped;
            }
            if let Some(lost) = self.swipe.take() {
                self.finished_swipe = Some(lost.summary(GestureEnd::Lost));
            }
            self.swipe = Some(DockSwipe::new(fields.motion));
        }
        let Some(swipe) = self.swipe.as_mut() else {
            return GestureOutput::Quiet;
        };
        swipe.observe(fields, phase.is_terminal());
        let fired = (swipe.fired.is_none() && swipe.commits(phase)).then(|| swipe.gesture());
        if fired.is_some() {
            swipe.fired = fired;
        }
        if let Some(end) = terminal_end(phase)
            && let Some(done) = self.swipe.take()
        {
            self.finished_swipe = Some(done.summary(end));
        }
        fired.map_or(GestureOutput::Quiet, |gesture| {
            GestureOutput::Event(CaptureEvent::SystemGesture(gesture))
        })
    }

    fn magnify(&mut self, fields: GestureFields) -> GestureOutput {
        let phase = match native_phase(fields.phase) {
            Some(NativePhase::Gesture(phase)) => phase,
            Some(NativePhase::MayBegin) => return GestureOutput::Quiet,
            None => return GestureOutput::Unmapped,
        };
        let Ok(gesture) =
            PointerGesture::from_parts(GestureKind::Magnify, Some(phase), fields.zoom)
        else {
            return GestureOutput::Unmapped;
        };
        if phase == GesturePhase::Began
            && let Some(lost) = self.pinch.take()
        {
            self.finished_pinch = Some(lost.summary(GestureEnd::Lost));
        }
        let pinch = self.pinch.get_or_insert(Pinch {
            records: 0,
            scale: 1.0,
        });
        pinch.records = pinch.records.saturating_add(1);
        pinch.scale *= 1.0 + fields.zoom;
        if let Some(end) = terminal_end(phase)
            && let Some(done) = self.pinch.take()
        {
            self.finished_pinch = Some(done.summary(end));
        }
        GestureOutput::Event(CaptureEvent::gesture(gesture))
    }

    /// Look Up fires once per press, when it first reaches the force-click stage.
    fn force(&mut self, fields: GestureFields) -> DecodedGesture {
        if fields.stage >= FORCE_CLICK_STAGE && !self.force_clicked {
            self.force_clicked = true;
            return DecodedGesture {
                record: TouchRecord::Standalone,
                output: GestureOutput::Event(CaptureEvent::gesture(PointerGesture::ForceClick)),
            };
        }
        let ended = matches!(native_phase(fields.phase), Some(NativePhase::Gesture(phase)) if phase.is_terminal());
        if fields.stage <= 0 || ended {
            self.force_clicked = false;
        }
        continues(GestureOutput::Quiet)
    }
}

enum NativePhase {
    MayBegin,
    Gesture(GesturePhase),
}

fn native_phase(value: i64) -> Option<NativePhase> {
    Some(NativePhase::Gesture(match value {
        NATIVE_PHASE_BEGAN => GesturePhase::Began,
        NATIVE_PHASE_CHANGED => GesturePhase::Changed,
        NATIVE_PHASE_ENDED => GesturePhase::Ended,
        NATIVE_PHASE_CANCELLED => GesturePhase::Cancelled,
        NATIVE_PHASE_MAY_BEGIN => return Some(NativePhase::MayBegin),
        _ => return None,
    }))
}

const fn terminal_end(phase: GesturePhase) -> Option<GestureEnd> {
    match phase {
        GesturePhase::Ended => Some(GestureEnd::Lifted),
        GesturePhase::Cancelled => Some(GestureEnd::Cancelled),
        GesturePhase::Began | GesturePhase::Changed => None,
    }
}

const fn continues(output: GestureOutput) -> DecodedGesture {
    DecodedGesture {
        record: TouchRecord::Continues,
        output,
    }
}

/// Decodes one physical private gesture record (Core Graphics types 29 to 31). Pure but for
/// `decoder`, and total: nothing a private field holds stops capture.
pub fn decode_gesture(fields: GestureFields, decoder: &mut GestureDecoder) -> DecodedGesture {
    let record = |record, output| DecodedGesture { record, output };
    match (fields.event_type, fields.hid_type) {
        (CG_EVENT_GESTURE, HID_TOUCH_STARTED) => record(TouchRecord::Opens, GestureOutput::Quiet),
        (CG_EVENT_GESTURE, HID_TOUCH_ENDED) => record(TouchRecord::Closes, GestureOutput::Quiet),
        (CG_EVENT_GESTURE, HID_SCROLL) => continues(GestureOutput::Quiet),
        (CG_EVENT_GESTURE, HID_ZOOM) => continues(decoder.magnify(fields)),
        (CG_EVENT_GESTURE, HID_ROTATION) => continues(rotate(fields)),
        (CG_EVENT_GESTURE, HID_ZOOM_TOGGLE) => record(
            TouchRecord::Standalone,
            GestureOutput::Event(CaptureEvent::gesture(PointerGesture::SmartMagnify)),
        ),
        (CG_EVENT_GESTURE, HID_FORCE) => decoder.force(fields),
        (CG_EVENT_DOCK_CONTROL, HID_DOCK_SWIPE) => continues(decoder.dock_swipe(fields)),
        _ => continues(GestureOutput::Unmapped),
    }
}

fn rotate(fields: GestureFields) -> GestureOutput {
    let phase = match native_phase(fields.phase) {
        Some(NativePhase::Gesture(phase)) => phase,
        Some(NativePhase::MayBegin) => return GestureOutput::Quiet,
        None => return GestureOutput::Unmapped,
    };
    PointerGesture::from_parts(GestureKind::Rotate, Some(phase), fields.rotation)
        .map_or(GestureOutput::Unmapped, |gesture| {
            GestureOutput::Event(CaptureEvent::gesture(gesture))
        })
}

/// The event-tap callback's decode is where a button is stamped on the shared capture clock.
fn button(button: MouseButton, pressed: bool) -> DecodedInput {
    DecodedInput::Event(CaptureEvent::Button {
        button,
        pressed,
        at: capture_clock(),
    })
}

fn mouse_button(button_number: i64, pressed: bool) -> DecodedInput {
    let mouse_button = match button_number {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => return DecodedInput::Unsupported,
    };
    button(mouse_button, pressed)
}

fn valid_rectangle(rectangle: LogicalRect) -> bool {
    rectangle.origin.is_finite()
        && rectangle.size.width.is_finite()
        && rectangle.size.height.is_finite()
        && rectangle.size.width > 0.0
        && rectangle.size.height > 0.0
        && (rectangle.origin.x + rectangle.size.width).is_finite()
        && (rectangle.origin.y + rectangle.size.height).is_finite()
}

/// Sets or clears one side-specific modifier bit; other usages leave `held` unchanged.
fn set_modifier(held: &mut u8, usage: HidUsage, down: bool) {
    let Some(bit) = modifier_bit(usage) else {
        return;
    };
    if down {
        *held |= bit;
    } else {
        *held &= !bit;
    }
}

fn modifier_bit(usage: HidUsage) -> Option<u8> {
    let offset = usage.0.checked_sub(0xe0).filter(|offset| *offset < 8)?;
    Some(1_u8 << offset)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use monhop_core::{
        FloorState, SharedFloor, TakeBackGate,
        capture::{CaptureStop, capture_channel},
        capture_physical::PhysicalCapture,
    };

    use super::*;
    use crate::SYNTHETIC_EVENT_MARKER;

    const LEFT_CONTROL: u16 = 0x3b;
    const LEFT_SHIFT: u16 = 0x38;
    const RIGHT_SHIFT: u16 = 0x3c;
    const CAPS_LOCK: u16 = 0x39;
    const FN_GLOBE: u16 = 0x3f;

    fn physical() -> EventSourceMetadata {
        EventSourceMetadata {
            user_data: 0,
            state_id: CG_EVENT_SOURCE_STATE_HID_SYSTEM,
        }
    }

    fn injected() -> EventSourceMetadata {
        EventSourceMetadata {
            user_data: SYNTHETIC_EVENT_MARKER,
            ..physical()
        }
    }

    /// HID state read while the peer holds an injected key, so `down` may count that key.
    fn while_injected(down: bool) -> HidKeyState {
        HidKeyState {
            down,
            injection_held: true,
        }
    }

    fn nothing_injected(down: bool) -> HidKeyState {
        HidKeyState {
            down,
            injection_held: false,
        }
    }

    fn flags_changed(
        ledger: &mut PhysicalModifierLedger,
        virtual_key: u16,
        source: EventSourceMetadata,
        hid: HidKeyState,
    ) -> DecodedInput {
        decode_keyboard(
            CG_EVENT_FLAGS_CHANGED,
            virtual_key,
            ledger,
            hid,
            source,
            SYNTHETIC_EVENT_MARKER,
        )
    }

    fn key(decoded: DecodedInput) -> (HidUsage, bool) {
        match decoded {
            DecodedInput::Event(CaptureEvent::Key { usage, pressed, .. }) => (usage, pressed),
            _ => panic!("expected a physical key record"),
        }
    }

    #[test]
    fn physical_modifier_toggles_from_ledger_not_hid_state() {
        let mut ledger = PhysicalModifierLedger::default();
        let left_shift = HidUsage(0xe1);
        for (virtual_key, hid_down, expected) in [
            (LEFT_SHIFT, false, (left_shift, true)),
            (RIGHT_SHIFT, false, (HidUsage(0xe5), true)),
            (LEFT_SHIFT, true, (left_shift, false)),
            (LEFT_SHIFT, false, (left_shift, true)),
        ] {
            assert_eq!(
                key(flags_changed(
                    &mut ledger,
                    virtual_key,
                    physical(),
                    while_injected(hid_down)
                )),
                expected,
                "while injection is held, each side toggles independently of HID state"
            );
        }

        // Caps Lock is not a held modifier; each record keeps reading its HID key state.
        for hid_down in [true, true, false] {
            assert_eq!(
                key(flags_changed(
                    &mut ledger,
                    CAPS_LOCK,
                    physical(),
                    while_injected(hid_down)
                )),
                (HidUsage(0x39), hid_down)
            );
        }
    }

    #[test]
    fn modifier_resyncs_from_hid_state_when_nothing_injected() {
        let left_shift = HidUsage(0xe1);
        // Left Shift went down after the tap existed but before the seed read HID state.
        let mut double_counted = PhysicalModifierLedger::default();
        double_counted.seed_held(left_shift);
        assert_eq!(
            key(flags_changed(
                &mut double_counted,
                LEFT_SHIFT,
                physical(),
                nothing_injected(true)
            )),
            (left_shift, true),
            "with nothing injected the queued press is read from HID state, not toggled"
        );

        let mut ledger = PhysicalModifierLedger::default();
        ledger.seed_held(left_shift);
        assert_eq!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                while_injected(true)
            )),
            (left_shift, false),
            "toggling the double-counted press inverts the ledger"
        );
        assert_eq!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(false)
            )),
            (left_shift, false),
            "the physical release resyncs the ledger from HID state"
        );
        for (hid_down, pressed) in [(false, true), (true, false)] {
            assert_eq!(
                key(flags_changed(
                    &mut ledger,
                    LEFT_SHIFT,
                    physical(),
                    while_injected(hid_down)
                )),
                (left_shift, pressed),
                "the healed ledger toggles correctly under injection"
            );
        }
    }

    #[test]
    fn locally_posted_modifier_is_not_read_from_hid_state() {
        let left_shift = HidUsage(0xe1);
        let mut ledger = PhysicalModifierLedger::default();
        assert!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(true)
            ))
            .1
        );
        // Restoring the local route re-presses the held modifier with a marked post.
        ledger.record_posted(LocalTransfer::Key {
            usage: left_shift,
            pressed: true,
        });
        assert_eq!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(true)
            )),
            (left_shift, false),
            "HID state still counts MonHop's own copy after the physical release"
        );
        ledger.record_posted(LocalTransfer::Key {
            usage: left_shift,
            pressed: false,
        });
        assert!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(true)
            ))
            .1
        );
        assert!(
            !key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(false)
            ))
            .1
        );
    }

    #[test]
    fn injected_same_modifier_does_not_hide_physical_release() {
        let mut ledger = PhysicalModifierLedger::default();
        assert!(
            key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                nothing_injected(true)
            ))
            .1
        );
        for _ in 0..2 {
            assert!(matches!(
                flags_changed(&mut ledger, LEFT_SHIFT, injected(), while_injected(true)),
                DecodedInput::Ignored
            ));
        }
        assert!(
            !key(flags_changed(
                &mut ledger,
                LEFT_SHIFT,
                physical(),
                while_injected(true)
            ))
            .1
        );

        // The peer holds an injected Left Shift when the local user presses and releases theirs.
        let floor = SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        gate.note_injected_press();
        // What the native callback copies: HID state counts the injected copy as down.
        let hid = || HidKeyState {
            down: true,
            injection_held: gate.injected_held(),
        };
        assert!(matches!(
            flags_changed(&mut ledger, LEFT_SHIFT, injected(), hid()),
            DecodedInput::Ignored
        ));
        let stop = CaptureStop::default();
        let (mut producer, mut consumer) = capture_channel(stop.clone());
        let mut capture = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());

        let DecodedInput::Event(press) = flags_changed(&mut ledger, LEFT_SHIFT, physical(), hid())
        else {
            panic!("expected the physical press");
        };
        assert!(
            capture.process(press, false, Duration::ZERO, &mut producer, &stop),
            "the press takes back and is withheld while the injected copy is down"
        );
        assert!(gate.take_triggered().is_some());
        assert!(capture.has_suppressed_presses());

        let DecodedInput::Event(release) =
            flags_changed(&mut ledger, LEFT_SHIFT, physical(), hid())
        else {
            panic!("expected the physical release");
        };
        assert!(matches!(release, CaptureEvent::Key { pressed: false, .. }));
        assert!(capture.process(
            release,
            false,
            Duration::from_millis(1),
            &mut producer,
            &stop
        ));
        assert!(
            !capture.has_suppressed_presses(),
            "the physical release ends the withheld press's quarantine"
        );
        assert!(consumer.try_pop().unwrap().is_none());
        assert_eq!(stop.reason(), None);
    }

    /// The peer's injected input is admitted and one injected key is down.
    fn receiving_gate_holding_injected_input() -> TakeBackGate {
        let floor = SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        gate.note_injected_press();
        gate
    }

    /// One scripted injector action, run before or inside a HID read.
    type InjectorStep = fn(&TakeBackGate);

    /// Samples a scripted HID value while `injector` runs inside the read.
    fn sample_during(
        gate: &TakeBackGate,
        down: bool,
        injector: impl FnOnce(&TakeBackGate),
    ) -> HidKeyState {
        HidKeyState::sample(Some(gate), || {
            injector(gate);
            down
        })
    }

    #[test]
    fn hid_read_is_physical_only_without_overlapping_injection() {
        let quiet = TakeBackGate::new(SharedFloor::new());
        let overlapping: [(&str, InjectorStep, InjectorStep); 5] = [
            ("held throughout", TakeBackGate::note_injected_press, |_| {}),
            (
                "released during the read",
                TakeBackGate::note_injected_press,
                TakeBackGate::note_injected_released,
            ),
            (
                "one up during the read",
                TakeBackGate::note_injected_press,
                TakeBackGate::note_injected_up,
            ),
            (
                "pressed during the read",
                |_| {},
                TakeBackGate::note_injected_press,
            ),
            (
                "pressed and released during the read",
                |_| {},
                |gate| {
                    gate.note_injected_press();
                    gate.note_injected_up();
                },
            ),
        ];
        for (case, before, during) in overlapping {
            let gate = TakeBackGate::new(SharedFloor::new());
            before(&gate);
            assert!(sample_during(&gate, true, during).injection_held, "{case}");
        }

        quiet.note_injected_press();
        quiet.note_injected_up();
        let settled = sample_during(&quiet, true, |_| {});
        assert!(
            settled.down && !settled.injection_held,
            "a read after injection settled is physical"
        );
        let ungated = HidKeyState::sample(None, || true);
        assert!(ungated.down && !ungated.injection_held);
    }

    #[test]
    fn physical_release_racing_an_injected_release_ends_its_quarantine() {
        let left_shift = HidUsage(0xe1);
        let gate = receiving_gate_holding_injected_input();
        let stop = CaptureStop::default();
        let (mut producer, mut consumer) = capture_channel(stop.clone());
        let mut capture = PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone());
        let mut ledger = PhysicalModifierLedger::default();

        let hid = sample_during(&gate, true, |_| {});
        let DecodedInput::Event(press) = flags_changed(&mut ledger, LEFT_SHIFT, physical(), hid)
        else {
            panic!("expected the physical press");
        };
        assert!(
            capture.process(press, false, Duration::ZERO, &mut producer, &stop),
            "the press takes back and is withheld while injected input is down"
        );
        assert!(capture.has_suppressed_presses());

        // HID state still counts the injected copy; the injector releases it before the read ends.
        let hid = sample_during(&gate, true, TakeBackGate::note_injected_released);
        assert!(
            !gate.injected_held(),
            "a held check after the read alone would call this read physical"
        );
        let DecodedInput::Event(release) = flags_changed(&mut ledger, LEFT_SHIFT, physical(), hid)
        else {
            panic!("expected the physical release");
        };
        assert_eq!(key(DecodedInput::Event(release)), (left_shift, false));
        assert!(capture.process(
            release,
            false,
            Duration::from_millis(1),
            &mut producer,
            &stop
        ));
        assert!(
            !capture.has_suppressed_presses(),
            "the physical release ends the withheld press's quarantine"
        );
        assert!(consumer.try_pop().unwrap().is_none());
        assert_eq!(stop.reason(), None);
    }

    #[test]
    fn seeded_modifier_is_initially_held() {
        let mut ledger = PhysicalModifierLedger::default();
        ledger.seed_held(HidUsage(0xe0));
        ledger.seed_held(HidUsage(0x04));
        let stop = CaptureStop::default();
        let (mut producer, mut consumer) = capture_channel(stop.clone());
        let mut capture = PhysicalCapture::new(Duration::ZERO);
        capture.seed_locally_held_key(HidUsage(0xe0));
        assert!(!capture.is_ready_for_suppression());

        let (usage, pressed) = key(flags_changed(
            &mut ledger,
            LEFT_CONTROL,
            physical(),
            while_injected(true),
        ));
        assert_eq!((usage, pressed), (HidUsage(0xe0), false));
        let release = CaptureEvent::Key {
            usage,
            pressed,
            repeat: false,
            modifiers: ModifierState::default(),
        };
        assert!(!capture.process(release, false, Duration::ZERO, &mut producer, &stop));
        assert!(capture.is_ready_for_suppression());
        assert!(consumer.try_pop().unwrap().is_none());

        assert!(
            key(flags_changed(
                &mut ledger,
                LEFT_CONTROL,
                physical(),
                while_injected(false)
            ))
            .1
        );
        assert!(
            matches!(
                decode_keyboard(
                    CG_EVENT_KEY_DOWN,
                    0x00,
                    &mut ledger,
                    HidKeyState::default(),
                    physical(),
                    SYNTHETIC_EVENT_MARKER,
                ),
                DecodedInput::Event(CaptureEvent::Key { pressed: true, .. })
            ),
            "seeding a non-modifier tracks nothing"
        );
    }

    #[test]
    fn only_modifiers_and_caps_lock_decode_from_flags_changed() {
        let mut ledger = PhysicalModifierLedger::default();
        for virtual_key in 0..=u16::from(u8::MAX) {
            for hid in [nothing_injected(true), while_injected(false)] {
                match flags_changed(&mut ledger, virtual_key, physical(), hid) {
                    DecodedInput::Ignored => {}
                    DecodedInput::Event(CaptureEvent::Key { usage, .. }) => assert!(
                        usage.0 == 0x39 || usage.is_modifier(),
                        "only the eight modifiers and Caps Lock reach the ledger"
                    ),
                    _ => panic!("a flags-changed record never stops capture"),
                }
            }
        }
        assert!(matches!(
            flags_changed(&mut ledger, FN_GLOBE, physical(), nothing_injected(true)),
            DecodedInput::Ignored
        ));
        assert!(matches!(
            decode_keyboard(
                CG_EVENT_KEY_DOWN,
                FN_GLOBE,
                &mut ledger,
                HidKeyState::default(),
                physical(),
                SYNTHETIC_EVENT_MARKER,
            ),
            DecodedInput::Unsupported
        ));
    }

    fn record(event_type: u32, hid_type: i64) -> GestureFields {
        GestureFields {
            event_type,
            hid_type,
            ..GestureFields::default()
        }
    }

    fn pinch(phase: i64, zoom: f64) -> GestureFields {
        GestureFields {
            phase,
            zoom,
            ..record(CG_EVENT_GESTURE, HID_ZOOM)
        }
    }

    fn swipe(motion: i64, phase: i64, progress: f64, velocity: (f64, f64)) -> GestureFields {
        GestureFields {
            phase,
            motion,
            progress,
            velocity_x: velocity.0,
            velocity_y: velocity.1,
            ..record(CG_EVENT_DOCK_CONTROL, HID_DOCK_SWIPE)
        }
    }

    fn press(stage: i64) -> GestureFields {
        GestureFields {
            stage,
            ..record(CG_EVENT_GESTURE, HID_FORCE)
        }
    }

    const STILL: (f64, f64) = (0.0, 0.0);

    fn gesture(gesture: PointerGesture) -> GestureOutput {
        GestureOutput::Event(CaptureEvent::gesture(gesture))
    }

    fn system(gesture: SystemGesture) -> GestureOutput {
        GestureOutput::Event(CaptureEvent::SystemGesture(gesture))
    }

    /// The outputs of `records` decoded in order by one decoder.
    fn outputs(
        decoder: &mut GestureDecoder,
        records: impl IntoIterator<Item = GestureFields>,
    ) -> Vec<GestureOutput> {
        records
            .into_iter()
            .map(|fields| decode_gesture(fields, decoder).output)
            .collect()
    }

    fn finished(decoder: &mut GestureDecoder) -> Vec<GestureSummary> {
        decoder.take_finished().collect()
    }

    #[test]
    fn the_decode_table_maps_each_known_record_and_leaves_the_rest_unmapped() {
        let decode = |fields| decode_gesture(fields, &mut GestureDecoder::default());
        let decoded = |record, output| DecodedGesture { record, output };
        let changed = |fields: GestureFields| GestureFields {
            phase: NATIVE_PHASE_CHANGED,
            ..fields
        };
        for (fields, expected) in [
            (
                record(CG_EVENT_GESTURE, HID_TOUCH_STARTED),
                decoded(TouchRecord::Opens, GestureOutput::Quiet),
            ),
            (
                record(CG_EVENT_GESTURE, HID_TOUCH_ENDED),
                decoded(TouchRecord::Closes, GestureOutput::Quiet),
            ),
            (
                record(CG_EVENT_GESTURE, HID_SCROLL),
                decoded(TouchRecord::Continues, GestureOutput::Quiet),
            ),
            (
                pinch(NATIVE_PHASE_CHANGED, 0.125),
                decoded(
                    TouchRecord::Continues,
                    gesture(PointerGesture::Magnify {
                        phase: GesturePhase::Changed,
                        delta: 0.125,
                    }),
                ),
            ),
            (
                changed(GestureFields {
                    rotation: -12.5,
                    ..record(CG_EVENT_GESTURE, HID_ROTATION)
                }),
                decoded(
                    TouchRecord::Continues,
                    gesture(PointerGesture::Rotate {
                        phase: GesturePhase::Changed,
                        degrees: -12.5,
                    }),
                ),
            ),
            (
                record(CG_EVENT_GESTURE, HID_ZOOM_TOGGLE),
                decoded(
                    TouchRecord::Standalone,
                    gesture(PointerGesture::SmartMagnify),
                ),
            ),
            (
                press(FORCE_CLICK_STAGE),
                decoded(TouchRecord::Standalone, gesture(PointerGesture::ForceClick)),
            ),
            (
                press(1),
                decoded(TouchRecord::Continues, GestureOutput::Quiet),
            ),
            (
                swipe(MOTION_VERTICAL, NATIVE_PHASE_BEGAN, 0.0, STILL),
                decoded(TouchRecord::Continues, GestureOutput::Quiet),
            ),
        ] {
            assert_eq!(decode(fields), expected, "{fields:?}");
        }
        for (event_type, hid_type) in [
            (CG_EVENT_GESTURE, 0),
            (CG_EVENT_GESTURE, 16),
            (CG_EVENT_GESTURE, HID_DOCK_SWIPE),
            (CG_EVENT_GESTURE, 27),
            (CG_EVENT_DOCK_CONTROL, 0),
            (CG_EVENT_DOCK_CONTROL, HID_ZOOM),
            (CG_EVENT_FLUID_TOUCH_GESTURE, 16),
            (CG_EVENT_FLUID_TOUCH_GESTURE, 27),
            (CG_EVENT_FLUID_TOUCH_GESTURE, HID_TOUCH_STARTED),
        ] {
            assert_eq!(
                decode(changed(record(event_type, hid_type))),
                decoded(TouchRecord::Continues, GestureOutput::Unmapped),
                "type {event_type} hid {hid_type}"
            );
        }
    }

    #[test]
    fn gesture_phases_map_and_an_unknown_phase_or_value_is_unmapped() {
        let mut decoder = GestureDecoder::default();
        for (native, phase) in [
            (NATIVE_PHASE_BEGAN, GesturePhase::Began),
            (NATIVE_PHASE_CHANGED, GesturePhase::Changed),
            (NATIVE_PHASE_ENDED, GesturePhase::Ended),
            (NATIVE_PHASE_CANCELLED, GesturePhase::Cancelled),
        ] {
            assert_eq!(
                outputs(&mut decoder, [pinch(native, 0.0)]),
                [gesture(PointerGesture::Magnify { phase, delta: 0.0 })]
            );
        }
        assert_eq!(
            outputs(&mut decoder, [pinch(NATIVE_PHASE_MAY_BEGIN, 0.0)]),
            [GestureOutput::Quiet]
        );
        for native in [0, 3, 16, -1] {
            assert_eq!(
                outputs(&mut decoder, [pinch(native, 0.1)]),
                [GestureOutput::Unmapped]
            );
        }
        for zoom in [-1.0, -1.5, 4.001, f64::NAN, f64::INFINITY] {
            assert_eq!(
                outputs(&mut decoder, [pinch(NATIVE_PHASE_CHANGED, zoom)]),
                [GestureOutput::Unmapped]
            );
        }
        let rotation = |rotation| GestureFields {
            phase: NATIVE_PHASE_CHANGED,
            rotation,
            ..record(CG_EVENT_GESTURE, HID_ROTATION)
        };
        for degrees in [180.5, f64::NAN] {
            assert_eq!(
                outputs(&mut decoder, [rotation(degrees)]),
                [GestureOutput::Unmapped]
            );
        }
    }

    #[test]
    fn a_dock_swipe_fires_once_at_the_first_record_reaching_the_commit() {
        let up = DOCK_SWIPE_VERTICAL_UP_SIGN;
        let short = DOCK_SWIPE_COMMIT_PROGRESS * 0.9;
        let mut decoder = GestureDecoder::default();
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_BEGAN, 0.0, STILL),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_CHANGED, up * short, (0.0, up)),
                    swipe(
                        MOTION_VERTICAL,
                        NATIVE_PHASE_CHANGED,
                        up * DOCK_SWIPE_COMMIT_PROGRESS,
                        (0.0, up * 2.0),
                    ),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_CHANGED, up * 0.6, (0.5, up)),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_ENDED, up * 0.9, STILL),
                ]
            ),
            [
                GestureOutput::Quiet,
                GestureOutput::Quiet,
                system(SystemGesture::Overview),
                GestureOutput::Quiet,
                GestureOutput::Quiet,
            ]
        );
        assert_eq!(
            finished(&mut decoder),
            [GestureSummary::DockSwipe {
                motion: MOTION_VERTICAL,
                progress: up * 0.9,
                peak_velocity_x: 0.5,
                peak_velocity_y: 2.0,
                fired: Some(SystemGesture::Overview),
                end: GestureEnd::Lifted,
            }]
        );
        assert!(finished(&mut decoder).is_empty(), "each line is taken once");
    }

    #[test]
    fn a_short_swipe_fires_at_lift_only_when_its_velocity_agrees_fast_enough() {
        let next = DOCK_SWIPE_HORIZONTAL_NEXT_SIGN;
        let short = next * DOCK_SWIPE_COMMIT_PROGRESS * 0.5;
        let lifted = |velocity_x| {
            let mut decoder = GestureDecoder::default();
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_HORIZONTAL, NATIVE_PHASE_BEGAN, 0.0, STILL),
                    swipe(MOTION_HORIZONTAL, NATIVE_PHASE_CHANGED, short, STILL),
                    swipe(
                        MOTION_HORIZONTAL,
                        NATIVE_PHASE_ENDED,
                        short,
                        (velocity_x, 0.0),
                    ),
                ],
            )
            .pop()
        };
        assert_eq!(
            lifted(next * DOCK_SWIPE_COMMIT_VELOCITY),
            Some(system(SystemGesture::DesktopNext))
        );
        for velocity_x in [
            next * DOCK_SWIPE_COMMIT_VELOCITY * 0.9,
            -next * DOCK_SWIPE_COMMIT_VELOCITY * 4.0,
            0.0,
        ] {
            assert_eq!(
                lifted(velocity_x),
                Some(GestureOutput::Quiet),
                "velocity {velocity_x}"
            );
        }
        let mut decoder = GestureDecoder::default();
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_HORIZONTAL, NATIVE_PHASE_BEGAN, 0.0, STILL),
                    swipe(
                        MOTION_HORIZONTAL,
                        NATIVE_PHASE_CHANGED,
                        -short,
                        (-next, 0.0)
                    ),
                    swipe(MOTION_HORIZONTAL, NATIVE_PHASE_ENDED, 0.0, STILL),
                ]
            )
            .pop(),
            Some(system(SystemGesture::DesktopPrevious)),
            "a lift that zeroes its values is judged on the last real ones"
        );
    }

    #[test]
    fn a_cancelled_swipe_never_fires() {
        let spread = DOCK_SWIPE_SCALE_SPREAD_SIGN;
        let mut decoder = GestureDecoder::default();
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_SCALE, NATIVE_PHASE_BEGAN, 0.0, STILL),
                    swipe(MOTION_SCALE, NATIVE_PHASE_CHANGED, spread * 0.1, STILL),
                    swipe(
                        MOTION_SCALE,
                        NATIVE_PHASE_CANCELLED,
                        spread * 0.9,
                        (9.0, 9.0)
                    ),
                ]
            ),
            [GestureOutput::Quiet; 3]
        );
        assert!(matches!(
            finished(&mut decoder)[..],
            [GestureSummary::DockSwipe {
                fired: None,
                end: GestureEnd::Cancelled,
                ..
            }]
        ));
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_SCALE, NATIVE_PHASE_BEGAN, spread * 0.5, STILL),
                    swipe(MOTION_SCALE, NATIVE_PHASE_CANCELLED, 0.0, STILL),
                ]
            ),
            [system(SystemGesture::ShowDesktop), GestureOutput::Quiet],
            "a swipe that already fired cannot take it back"
        );
    }

    #[test]
    fn each_motion_fires_the_gesture_its_sign_constant_names() {
        let commit = DOCK_SWIPE_COMMIT_PROGRESS;
        for (motion, sign, first, second) in [
            (
                MOTION_HORIZONTAL,
                DOCK_SWIPE_HORIZONTAL_NEXT_SIGN,
                SystemGesture::DesktopNext,
                SystemGesture::DesktopPrevious,
            ),
            (
                MOTION_VERTICAL,
                DOCK_SWIPE_VERTICAL_UP_SIGN,
                SystemGesture::Overview,
                SystemGesture::AppWindows,
            ),
            (
                MOTION_SCALE,
                DOCK_SWIPE_SCALE_SPREAD_SIGN,
                SystemGesture::ShowDesktop,
                SystemGesture::Launcher,
            ),
        ] {
            for (progress, expected) in [(sign * commit, first), (-sign * commit, second)] {
                let mut decoder = GestureDecoder::default();
                assert_eq!(
                    outputs(
                        &mut decoder,
                        [swipe(motion, NATIVE_PHASE_BEGAN, progress, STILL)]
                    ),
                    [system(expected)],
                    "motion {motion} progress {progress}"
                );
            }
        }
        for motion in [0, 4, 7, 14] {
            for phase in [NATIVE_PHASE_BEGAN, NATIVE_PHASE_CHANGED] {
                assert_eq!(
                    outputs(
                        &mut GestureDecoder::default(),
                        [swipe(motion, phase, 0.9, STILL)]
                    ),
                    [GestureOutput::Unmapped],
                    "an edge swipe starts nothing"
                );
            }
        }
        let up = DOCK_SWIPE_VERTICAL_UP_SIGN;
        let mut decoder = GestureDecoder::default();
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_BEGAN, 0.0, STILL),
                    swipe(0, NATIVE_PHASE_ENDED, up, STILL),
                ]
            ),
            [GestureOutput::Quiet, system(SystemGesture::Overview)],
            "a started swipe keeps its motion to the end"
        );
    }

    #[test]
    fn a_swipe_starts_without_its_began_and_a_new_began_replaces_a_lost_one() {
        let up = DOCK_SWIPE_VERTICAL_UP_SIGN;
        let mut decoder = GestureDecoder::default();
        assert_eq!(
            outputs(
                &mut decoder,
                [
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_ENDED, up, STILL),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_MAY_BEGIN, up, STILL),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_CHANGED, up * 0.5, STILL),
                    swipe(MOTION_VERTICAL, NATIVE_PHASE_BEGAN, -up * 0.5, STILL),
                ]
            ),
            [
                GestureOutput::Quiet,
                GestureOutput::Quiet,
                system(SystemGesture::Overview),
                system(SystemGesture::AppWindows),
            ],
            "a lift with nothing open and a may-begin start nothing"
        );
        assert!(matches!(
            finished(&mut decoder)[..],
            [GestureSummary::DockSwipe {
                fired: Some(SystemGesture::Overview),
                end: GestureEnd::Lost,
                ..
            }]
        ));
    }

    #[test]
    fn force_click_fires_once_per_press() {
        let mut decoder = GestureDecoder::default();
        let look_up = gesture(PointerGesture::ForceClick);
        let quiet = GestureOutput::Quiet;
        assert_eq!(
            outputs(&mut decoder, [1, 2, 2, 1, 2, 0, 1, 2].map(press)),
            [quiet, look_up, quiet, quiet, quiet, quiet, quiet, look_up]
        );
        let lifted = GestureFields {
            phase: NATIVE_PHASE_ENDED,
            ..press(1)
        };
        assert_eq!(
            outputs(&mut decoder, [lifted, press(2)]),
            [quiet, look_up],
            "an ended press also ends it"
        );
    }

    #[test]
    fn a_pinch_totals_its_scale_across_records() {
        let mut decoder = GestureDecoder::default();
        outputs(
            &mut decoder,
            [
                pinch(NATIVE_PHASE_BEGAN, 0.5),
                pinch(NATIVE_PHASE_CHANGED, 0.5),
                pinch(NATIVE_PHASE_BEGAN, 0.25),
            ],
        );
        assert_eq!(
            finished(&mut decoder),
            [GestureSummary::Pinch {
                records: 2,
                scale: 2.25,
                end: GestureEnd::Lost,
            }]
        );
        outputs(
            &mut decoder,
            [
                pinch(NATIVE_PHASE_CHANGED, -0.5),
                pinch(NATIVE_PHASE_ENDED, 0.0),
            ],
        );
        assert_eq!(
            finished(&mut decoder),
            [GestureSummary::Pinch {
                records: 3,
                scale: 0.625,
                end: GestureEnd::Lifted,
            }]
        );
    }

    #[test]
    fn gesture_summaries_log_values_only() {
        assert_eq!(
            GestureSummary::DockSwipe {
                motion: 2,
                progress: -0.4123,
                peak_velocity_x: 0.0,
                peak_velocity_y: 1.8734,
                fired: Some(SystemGesture::Overview),
                end: GestureEnd::Lifted,
            }
            .to_string(),
            "dock swipe ended (lift): motion 2, final progress -0.412, \
             peak velocity x 0.000 y 1.873, fired Overview"
        );
        assert_eq!(
            GestureSummary::DockSwipe {
                motion: 1,
                progress: 0.05,
                peak_velocity_x: 0.2,
                peak_velocity_y: 0.0,
                fired: None,
                end: GestureEnd::Cancelled,
            }
            .to_string(),
            "dock swipe ended (cancel): motion 1, final progress 0.050, \
             peak velocity x 0.200 y 0.000, fired none"
        );
        assert_eq!(
            GestureSummary::Pinch {
                records: 42,
                scale: 1.5634,
                end: GestureEnd::Lost,
            }
            .to_string(),
            "pinch ended (lost end): 42 records, total scale 1.563"
        );
    }

    #[test]
    fn a_record_without_delta_is_empty_unless_its_source_is_ignored() {
        let still = PointerFields {
            location: Point::new(1.0, 1.0),
            delta_x: 0.0,
            delta_y: -0.0,
            button_number: 0,
        };
        let motion = |source| {
            decode_pointer(
                CG_EVENT_MOUSE_MOVED,
                still,
                None,
                source,
                SYNTHETIC_EVENT_MARKER,
            )
            .input
        };
        for continuous in [true, false] {
            assert!(matches!(
                decode_scroll(0.0, -0.0, continuous, physical(), SYNTHETIC_EVENT_MARKER),
                DecodedInput::Empty
            ));
        }
        assert!(matches!(motion(physical()), DecodedInput::Empty));
        let foreign = EventSourceMetadata {
            state_id: 0,
            ..physical()
        };
        for source in [injected(), foreign] {
            assert!(
                matches!(
                    decode_scroll(0.0, 0.0, true, source, SYNTHETIC_EVENT_MARKER),
                    DecodedInput::Ignored
                ),
                "the source filter decides first"
            );
            assert!(matches!(motion(source), DecodedInput::Ignored));
        }
    }
}
