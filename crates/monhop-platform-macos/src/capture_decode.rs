//! Pure Quartz event decoding for explicit macOS capture.
//!
//! This module does not touch Core Graphics. The owned capture thread supplies copied fields from
//! its event-tap callback, which keeps the policy testable on non-macOS hosts.

use monhop_core::{
    HidUsage, LogicalRect, ModifierState, MouseButton, Point, TakeBackGate, capture::CaptureEvent,
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
    Ignored,
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
        CG_EVENT_MOUSE_MOVED
        | CG_EVENT_LEFT_MOUSE_DRAGGED
        | CG_EVENT_RIGHT_MOUSE_DRAGGED
        | CG_EVENT_OTHER_MOUSE_DRAGGED => {
            let Ok(dx) = i32::try_from(fields.delta_x) else {
                return DecodedPointer {
                    local_absolute: None,
                    input: DecodedInput::Malformed,
                    position: previous_position,
                };
            };
            let Ok(dy) = i32::try_from(fields.delta_y) else {
                return DecodedPointer {
                    local_absolute: None,
                    input: DecodedInput::Malformed,
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
}
