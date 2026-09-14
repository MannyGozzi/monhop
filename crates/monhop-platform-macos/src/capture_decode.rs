//! Pure Quartz event decoding for explicit macOS capture.
//!
//! This module does not touch Core Graphics. The owned capture thread supplies copied fields from
//! its event-tap callback, which keeps the policy testable on non-macOS hosts.

use monhop_core::{
    HidUsage, LogicalRect, ModifierState, MouseButton, Point, capture::CaptureEvent,
    capture_physical::LocalTransfer,
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

/// Core Graphics `kCGEventSourceStateHIDSystemState`.
pub const CG_EVENT_SOURCE_STATE_HID_SYSTEM: i64 = 1;

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
    Ignored,
    Unsupported,
    Event(CaptureEvent),
}

/// One decoded pointer record and the accepted logical cursor position to retain locally.
pub struct DecodedPointer {
    /// The local-route cursor anchor. The native callback admits this before the relative delta.
    pub local_absolute: Option<CaptureEvent>,
    pub input: DecodedInput,
    pub position: Option<Point>,
}

/// Copied Core Graphics fields for one pointer callback.
#[derive(Clone, Copy)]
pub struct PointerFields {
    pub location: Point,
    pub delta_x: i64,
    pub delta_y: i64,
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
        point.is_finite()
            && self.rectangles.iter().any(|rectangle| {
                point.x >= rectangle.origin.x
                    && point.x < rectangle.origin.x + rectangle.size.width
                    && point.y >= rectangle.origin.y
                    && point.y < rectangle.origin.y + rectangle.size.height
            })
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
        let Some(bit) = modifier_bit(usage) else {
            return;
        };
        if pressed {
            self.held |= bit;
        } else {
            self.held &= !bit;
        }
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
/// `flags_changed_pressed` must come from the side-specific HID-state query for a
/// `kCGEventFlagsChanged` record. The shared physical ledger derives repeat and modifiers after
/// the key is admitted, preventing generic Quartz modifier flags from collapsing left and right.
pub fn decode_keyboard(
    event_type: u32,
    virtual_key: u16,
    flags_changed_pressed: bool,
    source: EventSourceMetadata,
    synthetic_marker: i64,
) -> DecodedInput {
    if should_ignore_source(source, synthetic_marker) {
        return DecodedInput::Ignored;
    }
    let pressed = match event_type {
        CG_EVENT_KEY_DOWN => true,
        CG_EVENT_KEY_UP => false,
        CG_EVENT_FLAGS_CHANGED => flags_changed_pressed,
        _ => return DecodedInput::Ignored,
    };
    let Ok(usage) = mac_virtual_key_to_hid(MacVirtualKey(virtual_key)) else {
        return DecodedInput::Unsupported;
    };
    DecodedInput::Event(CaptureEvent::Key {
        usage,
        pressed,
        repeat: false,
        modifiers: ModifierState::default(),
    })
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
            input: DecodedInput::Unsupported,
            position: previous_position,
        };
    }
    let position = Some(fields.location);
    let (local_absolute, input) = match event_type {
        CG_EVENT_MOUSE_MOVED
        | CG_EVENT_LEFT_MOUSE_DRAGGED
        | CG_EVENT_RIGHT_MOUSE_DRAGGED
        | CG_EVENT_OTHER_MOUSE_DRAGGED => {
            let Ok(dx) = i32::try_from(fields.delta_x) else {
                return DecodedPointer {
                    local_absolute: None,
                    input: DecodedInput::Unsupported,
                    position: previous_position,
                };
            };
            let Ok(dy) = i32::try_from(fields.delta_y) else {
                return DecodedPointer {
                    local_absolute: None,
                    input: DecodedInput::Unsupported,
                    position: previous_position,
                };
            };
            let input = if dx == 0 && dy == 0 {
                DecodedInput::Ignored
            } else {
                DecodedInput::Event(CaptureEvent::LogicalRelativeMotion {
                    dx: f64::from(dx),
                    dy: f64::from(dy),
                })
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
        return DecodedInput::Unsupported;
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
        return DecodedInput::Unsupported;
    }
    if horizontal == 0.0 && vertical == 0.0 {
        return DecodedInput::Ignored;
    }
    DecodedInput::Event(CaptureEvent::LogicalScroll {
        horizontal,
        vertical,
    })
}

fn button(button: MouseButton, pressed: bool) -> DecodedInput {
    DecodedInput::Event(CaptureEvent::Button { button, pressed })
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

fn modifier_bit(usage: HidUsage) -> Option<u8> {
    (0xe0..=0xe7)
        .contains(&usage.0)
        .then_some(1_u8 << (usage.0 - 0xe0))
}
