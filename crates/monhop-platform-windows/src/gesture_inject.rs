//! Pinch and system-gesture input planned as whole `SendInput` batches without touching Windows,
//! so a batch never leaves a key down and the pinch's own Ctrl never combines with wire input.

use std::{collections::BTreeSet, iter};

use monhop_core::{
    HID_LEFT_CONTROL, HID_RIGHT_CONTROL, HidUsage, MAX_PINCH_STEPS_PER_RECORD, MouseButton,
    Platform, SystemGesture,
    capture::WHEEL_UNITS_PER_DETENT,
    chord::{chord_plan, system_chord},
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{INPUT, MOUSEEVENTF_WHEEL};

use crate::input::{InputError, button_input, keyboard_input, mouse_input};

/// A pinch posts one wheel notch each time the scale changes by this factor.
pub(crate) const PINCH_SCALE_PER_NOTCH: f64 = 1.1;
/// Wheel units in every pinch wheel event. Several apps zoom one step per wheel message whatever
/// its size, so a smaller message would zoom them too fast.
pub(crate) const PINCH_WHEEL_QUANTUM: i32 = WHEEL_UNITS_PER_DETENT;

/// One `INPUT` record of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stroke {
    Key {
        usage: HidUsage,
        pressed: bool,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Wheel {
        units: i32,
    },
    /// Left Ctrl the injector holds for a pinch, tracked apart from the wire's keys.
    SyntheticCtrl {
        pressed: bool,
    },
}

/// Input the injector holds down on its own rather than for the wire.
#[derive(Debug, Default)]
pub(crate) struct Synthetic {
    ctrl: bool,
    /// Releases still owed for keys and buttons a short batch left down when releasing them failed
    /// too; every later batch retries them first.
    stranded: Vec<Stroke>,
}

impl Synthetic {
    pub(crate) const fn new() -> Self {
        Self {
            ctrl: false,
            stranded: Vec::new(),
        }
    }

    fn landed(&mut self, strokes: &[Stroke]) {
        for stroke in strokes {
            match *stroke {
                Stroke::SyntheticCtrl { pressed } => self.ctrl = pressed,
                other => self.stranded.retain(|owed| *owed != other),
            }
        }
    }

    fn strand(&mut self, releases: &[Stroke]) {
        for release in releases {
            if !self.stranded.contains(release) {
                self.stranded.push(*release);
            }
        }
    }
}

/// What is down before a batch.
#[derive(Clone, Copy)]
pub(crate) struct Held<'a> {
    pub(crate) keys: &'a BTreeSet<HidUsage>,
    pub(crate) buttons: &'a BTreeSet<MouseButton>,
    pub(crate) synthetic: &'a Synthetic,
}

impl Held<'_> {
    fn any_ctrl(self) -> bool {
        self.synthetic.ctrl
            || self.keys.contains(&HID_LEFT_CONTROL)
            || self.keys.contains(&HID_RIGHT_CONTROL)
    }

    fn stranded(self) -> impl Iterator<Item = Stroke> {
        self.synthetic.stranded.iter().copied()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operation {
    /// A wire key or button stroke.
    Wire(Stroke),
    PinchWheel {
        quanta: i32,
    },
    EndGestures,
    System(SystemGesture),
}

/// The batch `operation` posts over `held`, empty when it posts nothing. Each batch first retries
/// stranded releases, and every batch but a pinch step releases the synthetic Ctrl.
pub(crate) fn plan(held: Held<'_>, operation: Operation) -> Vec<Stroke> {
    let body = match operation {
        Operation::PinchWheel { quanta } => return pinch_step(held, quanta),
        Operation::Wire(stroke) => vec![stroke],
        Operation::EndGestures => Vec::new(),
        Operation::System(gesture) => system_strokes(held, gesture),
    };
    let ctrl = held
        .synthetic
        .ctrl
        .then_some(Stroke::SyntheticCtrl { pressed: false });
    held.stranded().chain(ctrl).chain(body).collect()
}

/// `quanta` wheel events under Ctrl. The synthetic Ctrl goes down with the first of them, so a
/// pinch smaller than one quantum never taps Ctrl.
fn pinch_step(held: Held<'_>, quanta: i32) -> Vec<Stroke> {
    if quanta == 0 {
        return Vec::new();
    }
    let press = (!held.any_ctrl()).then_some(Stroke::SyntheticCtrl { pressed: true });
    let wheel = Stroke::Wheel {
        units: PINCH_WHEEL_QUANTUM * quanta.signum(),
    };
    held.stranded()
        .chain(press)
        .chain(iter::repeat_n(wheel, quanta.unsigned_abs() as usize))
        .collect()
}

/// `gesture`'s chord, or its navigation click; empty when the wire holds the key or button that
/// would be pressed.
fn system_strokes(held: Held<'_>, gesture: SystemGesture) -> Vec<Stroke> {
    if let Some(chord) = system_chord(Platform::Windows, gesture) {
        return chord_plan(held.keys.iter().copied(), chord)
            .steps()
            .iter()
            .map(|step| Stroke::Key {
                usage: step.usage,
                pressed: step.pressed,
            })
            .collect();
    }
    let button = match gesture {
        SystemGesture::NavigateBack => MouseButton::Back,
        SystemGesture::NavigateForward => MouseButton::Forward,
        _ => return Vec::new(),
    };
    if held.buttons.contains(&button) {
        return Vec::new();
    }
    vec![
        Stroke::Button {
            button,
            pressed: true,
        },
        Stroke::Button {
            button,
            pressed: false,
        },
    ]
}

/// Adds a magnify `delta` to `residual` wheel units and splits the sum into whole quanta to post,
/// positive when the fingers spread, and the sub-quantum remainder to carry. Quanta past
/// [`MAX_PINCH_STEPS_PER_RECORD`] are dropped, not carried.
pub(crate) fn pinch_quanta(residual: f64, delta: f64) -> (i32, f64) {
    let quantum = f64::from(PINCH_WHEEL_QUANTUM);
    let units =
        residual + delta.ln_1p() * f64::from(WHEEL_UNITS_PER_DETENT) / PINCH_SCALE_PER_NOTCH.ln();
    let quanta = (units / quantum).trunc();
    let limit = f64::from(MAX_PINCH_STEPS_PER_RECORD);
    (quanta.clamp(-limit, limit) as i32, units - quanta * quantum)
}

fn encode(strokes: &[Stroke]) -> Result<Vec<INPUT>, InputError> {
    strokes
        .iter()
        .map(|stroke| match *stroke {
            Stroke::Key { usage, pressed } => keyboard_input(usage, pressed),
            Stroke::SyntheticCtrl { pressed } => keyboard_input(HID_LEFT_CONTROL, pressed),
            Stroke::Button { button, pressed } => Ok(button_input(button, pressed)),
            // Windows reads the unsigned field as a signed wheel delta.
            Stroke::Wheel { units } => Ok(mouse_input(0, 0, MOUSEEVENTF_WHEEL, units as u32)),
        })
        .collect()
}

/// Releases, newest first, whatever `landed` pressed and left down. The synthetic Ctrl is left to
/// [`Synthetic`], which tracks it.
fn unwind(landed: &[Stroke]) -> Vec<Stroke> {
    let mut releases = Vec::new();
    for stroke in landed {
        match *stroke {
            Stroke::Key {
                usage,
                pressed: true,
            } => releases.push(Stroke::Key {
                usage,
                pressed: false,
            }),
            Stroke::Button {
                button,
                pressed: true,
            } => releases.push(Stroke::Button {
                button,
                pressed: false,
            }),
            release @ (Stroke::Key { .. } | Stroke::Button { .. }) => {
                releases.retain(|pending| *pending != release);
            }
            Stroke::Wheel { .. } | Stroke::SyntheticCtrl { .. } => {}
        }
    }
    releases.reverse();
    releases
}

/// Sends `strokes` as one call and counts how many landed.
fn send_counted(
    strokes: &[Stroke],
    send: &mut impl FnMut(&[INPUT]) -> Result<(), InputError>,
) -> (Result<(), InputError>, usize) {
    if strokes.is_empty() {
        return (Ok(()), 0);
    }
    let result = encode(strokes).and_then(|inputs| send(&inputs));
    let landed = match &result {
        Ok(()) => strokes.len(),
        Err(InputError::PartialSendInput { sent, .. }) => (*sent).min(strokes.len()),
        Err(_) => 0,
    };
    (result, landed)
}

/// Posts `strokes` through `send` as one batch and keeps `synthetic` in step with what landed.
/// After a short batch it releases what the landed part left down, strands whatever that release
/// could not land, and still fails.
pub(crate) fn send_batch(
    strokes: &[Stroke],
    synthetic: &mut Synthetic,
    mut send: impl FnMut(&[INPUT]) -> Result<(), InputError>,
) -> Result<(), InputError> {
    let (result, landed) = send_counted(strokes, &mut send);
    synthetic.landed(&strokes[..landed]);
    if result.is_err() {
        let releases = unwind(&strokes[..landed]);
        let (_, released) = send_counted(&releases, &mut send);
        synthetic.strand(&releases[released..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use monhop_core::{MAX_MAGNIFY_DELTA, MIN_MAGNIFY_DELTA};
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            KEYEVENTF_SCANCODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
        },
        WindowsAndMessaging::{XBUTTON1, XBUTTON2},
    };

    use super::*;
    use crate::input::MONHOP_INJECTED_MARKER;

    /// A set-1 make code and whether it carries the E0 extended prefix.
    type Key = (u16, bool);
    const CONTROL: Key = (0x1D, false);
    const SHIFT: Key = (0x2A, false);
    const ALT: Key = (0x38, false);
    const WIN: Key = (0x5B, true);
    const TAB: Key = (0x0F, false);
    const LEFT: Key = (0x4B, true);
    const RIGHT: Key = (0x4D, true);
    const D: Key = (0x20, false);
    const N: Key = (0x31, false);
    const S: Key = (0x1F, false);
    const A: Key = (0x1E, false);

    const HID_A: HidUsage = HidUsage(0x04);
    const HID_TAB: u16 = 0x2B;
    const PRESS_A: Operation = Operation::Wire(Stroke::Key {
        usage: HID_A,
        pressed: true,
    });

    #[derive(Debug, PartialEq, Eq)]
    enum Record {
        Key { scan: u16, flags: u32 },
        Mouse { flags: u32, data: i32 },
    }

    fn key((scan, extended): Key, pressed: bool) -> Record {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        Record::Key { scan, flags }
    }

    fn down(scan: Key) -> Record {
        key(scan, true)
    }

    fn up(scan: Key) -> Record {
        key(scan, false)
    }

    fn wheel(units: i32) -> Record {
        Record::Mouse {
            flags: MOUSEEVENTF_WHEEL,
            data: units,
        }
    }

    fn x_click(button: u16) -> [Record; 2] {
        let data = i32::from(button);
        [
            Record::Mouse {
                flags: MOUSEEVENTF_XDOWN,
                data,
            },
            Record::Mouse {
                flags: MOUSEEVENTF_XUP,
                data,
            },
        ]
    }

    /// A tap of `key` inside presses of `modifiers` and their releases in reverse.
    fn chord(modifiers: &[Key], key: Key) -> Vec<Record> {
        let mut records: Vec<_> = modifiers.iter().map(|modifier| down(*modifier)).collect();
        records.extend([down(key), up(key)]);
        records.extend(modifiers.iter().rev().map(|modifier| up(*modifier)));
        records
    }

    /// Asserts every record is a marked scan-code key or a marked relative mouse event.
    fn decode(inputs: &[INPUT]) -> Vec<Record> {
        inputs
            .iter()
            .map(|input| match input.r#type {
                INPUT_KEYBOARD => {
                    // SAFETY: the type tag names the keyboard member as the initialized one.
                    let key = unsafe { input.Anonymous.ki };
                    assert_eq!((key.wVk, key.dwExtraInfo), (0, MONHOP_INJECTED_MARKER));
                    Record::Key {
                        scan: key.wScan,
                        flags: key.dwFlags,
                    }
                }
                INPUT_MOUSE => {
                    // SAFETY: the type tag names the mouse member as the initialized one.
                    let mouse = unsafe { input.Anonymous.mi };
                    assert_eq!(
                        (mouse.dx, mouse.dy, mouse.dwExtraInfo),
                        (0, 0, MONHOP_INJECTED_MARKER)
                    );
                    Record::Mouse {
                        flags: mouse.dwFlags,
                        data: mouse.mouseData as i32,
                    }
                }
                other => panic!("unexpected input type {other}"),
            })
            .collect()
    }

    fn holding_ctrl() -> Synthetic {
        Synthetic {
            ctrl: true,
            stranded: Vec::new(),
        }
    }

    fn strokes(
        keys: &[u16],
        buttons: &[MouseButton],
        synthetic: &Synthetic,
        operation: Operation,
    ) -> Vec<Stroke> {
        let keys: BTreeSet<_> = keys.iter().map(|usage| HidUsage(*usage)).collect();
        let buttons: BTreeSet<_> = buttons.iter().copied().collect();
        let held = Held {
            keys: &keys,
            buttons: &buttons,
            synthetic,
        };
        plan(held, operation)
    }

    /// The records `operation` posts while the wire holds `keys` and `buttons`.
    fn planned(
        keys: &[u16],
        buttons: &[MouseButton],
        synthetic: &Synthetic,
        operation: Operation,
    ) -> Vec<Record> {
        decode(&encode(&strokes(keys, buttons, synthetic, operation)).unwrap())
    }

    fn system(gesture: SystemGesture) -> Vec<Record> {
        planned(&[], &[], &Synthetic::new(), Operation::System(gesture))
    }

    /// Posts `operation` through a fake `SendInput` whose call `i` lands `landed[i]` records, and
    /// every record once the script runs out; returns the result and each call's records.
    fn scripted(
        synthetic: &mut Synthetic,
        operation: Operation,
        landed: &[usize],
    ) -> (Result<(), InputError>, Vec<Vec<Record>>) {
        let strokes = strokes(&[], &[], synthetic, operation);
        let mut calls = Vec::new();
        let result = send_batch(&strokes, synthetic, |inputs| {
            let attempted = inputs.len();
            let sent = landed.get(calls.len()).copied().unwrap_or(attempted);
            calls.push(decode(inputs));
            match sent {
                0 => Err(InputError::SendInputBlockedOrFailed { attempted, code: 5 }),
                sent if sent < attempted => Err(InputError::PartialSendInput {
                    sent,
                    attempted,
                    code: 5,
                }),
                _ => Ok(()),
            }
        });
        (result, calls)
    }

    #[test]
    fn each_chord_posts_scan_codes_in_order_and_extends_win_and_arrows() {
        for (gesture, expected) in [
            (SystemGesture::Overview, chord(&[WIN], TAB)),
            (SystemGesture::AppWindows, chord(&[WIN], TAB)),
            (SystemGesture::DesktopPrevious, chord(&[CONTROL, WIN], LEFT)),
            (SystemGesture::DesktopNext, chord(&[CONTROL, WIN], RIGHT)),
            (SystemGesture::ShowDesktop, chord(&[WIN], D)),
            (SystemGesture::Launcher, chord(&[], WIN)),
            (SystemGesture::NotificationCenter, chord(&[WIN], N)),
            (SystemGesture::Search, chord(&[WIN], S)),
            (SystemGesture::SwitchAppNext, chord(&[ALT], TAB)),
            (SystemGesture::SwitchAppPrevious, chord(&[SHIFT, ALT], TAB)),
        ] {
            assert_eq!(system(gesture), expected, "{gesture:?}");
        }
    }

    #[test]
    fn back_and_forward_click_the_x_buttons_unless_the_wire_holds_that_button() {
        assert_eq!(system(SystemGesture::NavigateBack), x_click(XBUTTON1));
        assert_eq!(system(SystemGesture::NavigateForward), x_click(XBUTTON2));
        let held_back = [MouseButton::Back];
        let back = Operation::System(SystemGesture::NavigateBack);
        assert!(planned(&[], &held_back, &Synthetic::new(), back).is_empty());
        let forward = Operation::System(SystemGesture::NavigateForward);
        assert_eq!(
            planned(&[], &held_back, &Synthetic::new(), forward),
            x_click(XBUTTON2)
        );
    }

    #[test]
    fn a_system_gesture_releases_the_pinch_ctrl_before_its_chord() {
        let show_desktop = Operation::System(SystemGesture::ShowDesktop);
        let mut expected = vec![up(CONTROL)];
        expected.extend(chord(&[WIN], D));
        assert_eq!(planned(&[], &[], &holding_ctrl(), show_desktop), expected);

        let next = Operation::System(SystemGesture::DesktopNext);
        let mut expected = vec![up(CONTROL)];
        expected.extend(chord(&[CONTROL, WIN], RIGHT));
        assert_eq!(planned(&[], &[], &holding_ctrl(), next), expected);

        let back = Operation::System(SystemGesture::NavigateBack);
        let mut expected = vec![up(CONTROL)];
        expected.extend(x_click(XBUTTON1));
        assert_eq!(planned(&[], &[], &holding_ctrl(), back), expected);

        let switch = Operation::System(SystemGesture::SwitchAppNext);
        assert_eq!(
            planned(&[HID_TAB], &[], &holding_ctrl(), switch),
            [up(CONTROL)],
            "a chord skipped for a wire-held key still ends the pinch"
        );
    }

    #[test]
    fn the_first_pinch_step_presses_left_ctrl_in_its_own_batch_when_no_ctrl_is_down() {
        let one = Operation::PinchWheel { quanta: 1 };
        assert_eq!(
            planned(&[], &[], &Synthetic::new(), one),
            [down(CONTROL), wheel(120)]
        );
        assert_eq!(planned(&[], &[], &holding_ctrl(), one), [wheel(120)]);
        for wire_ctrl in [HID_LEFT_CONTROL, HID_RIGHT_CONTROL] {
            assert_eq!(
                planned(&[wire_ctrl.0], &[], &Synthetic::new(), one),
                [wheel(120)]
            );
        }
    }

    #[test]
    fn a_pinch_that_never_reaches_a_step_injects_nothing() {
        let (quanta, _) = pinch_quanta(0.0, PINCH_SCALE_PER_NOTCH.powf(0.9) - 1.0);
        assert_eq!(quanta, 0);
        let none = Operation::PinchWheel { quanta: 0 };
        assert!(planned(&[], &[], &Synthetic::new(), none).is_empty());
        assert!(
            planned(&[], &[], &Synthetic::new(), Operation::EndGestures).is_empty(),
            "Ended releases no Ctrl that was never pressed"
        );
    }

    #[test]
    fn ending_gestures_releases_only_the_injectors_own_ctrl() {
        assert_eq!(
            planned(&[], &[], &holding_ctrl(), Operation::EndGestures),
            [up(CONTROL)]
        );
        for wire_ctrl in [HID_LEFT_CONTROL, HID_RIGHT_CONTROL] {
            let end = Operation::EndGestures;
            assert!(planned(&[wire_ctrl.0], &[], &Synthetic::new(), end).is_empty());
        }
    }

    #[test]
    fn the_pinch_ctrl_is_released_before_a_wire_key_or_button() {
        assert_eq!(
            planned(&[], &[], &holding_ctrl(), PRESS_A),
            [up(CONTROL), down(A)]
        );
        assert_eq!(planned(&[], &[], &Synthetic::new(), PRESS_A), [down(A)]);
        let button = Operation::Wire(Stroke::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        let left_down = Record::Mouse {
            flags: MOUSEEVENTF_LEFTDOWN,
            data: 0,
        };
        assert_eq!(
            planned(&[], &[], &holding_ctrl(), button),
            [up(CONTROL), left_down]
        );
    }

    #[test]
    fn pinch_wheel_events_carry_one_quantum_each_and_the_remainder_carries_over() {
        // 1.3 notches of scale per step: 156 wheel units against a 120-unit quantum.
        let delta = PINCH_SCALE_PER_NOTCH.powf(1.3) - 1.0;
        let mut residual = 0.0;
        for (expected_quanta, expected_residual) in [(1, 36.0), (1, 72.0), (1, 108.0), (2, 24.0)] {
            let (quanta, carried) = pinch_quanta(residual, delta);
            assert_eq!(quanta, expected_quanta);
            assert!((carried - expected_residual).abs() < 1e-9, "{carried}");
            residual = carried;
        }
        let two = Operation::PinchWheel { quanta: 2 };
        assert_eq!(
            planned(&[], &[], &holding_ctrl(), two),
            [wheel(120), wheel(120)]
        );
    }

    #[test]
    fn spreading_posts_positive_wheel_and_pinching_posts_negative() {
        let (quanta, _) = pinch_quanta(0.0, PINCH_SCALE_PER_NOTCH.powf(1.3) - 1.0);
        assert_eq!(quanta, 1);
        let (quanta, carried) = pinch_quanta(0.0, PINCH_SCALE_PER_NOTCH.powf(-1.3) - 1.0);
        assert_eq!(quanta, -1);
        assert!((carried + 36.0).abs() < 1e-9, "{carried}");
        let (quanta, _) = pinch_quanta(carried, PINCH_SCALE_PER_NOTCH.powf(-0.8) - 1.0);
        assert_eq!(
            quanta, -1,
            "a negative remainder carries into the next zoom out"
        );
        let out = Operation::PinchWheel { quanta: -2 };
        assert_eq!(
            planned(&[], &[], &holding_ctrl(), out),
            [wheel(-120), wheel(-120)]
        );
    }

    #[test]
    fn one_pinch_record_posts_at_most_the_step_cap_and_carries_only_the_remainder() {
        let cap = MAX_PINCH_STEPS_PER_RECORD as i32;
        let quantum = f64::from(PINCH_WHEEL_QUANTUM);
        // 385 quanta out and 16 in, before the cap.
        for (delta, expected) in [
            (MIN_MAGNIFY_DELTA.next_up(), -cap),
            (MAX_MAGNIFY_DELTA, cap),
        ] {
            let (quanta, carried) = pinch_quanta(0.0, delta);
            assert_eq!(quanta, expected, "{delta}");
            assert!(carried.abs() < quantum, "the excess is dropped: {carried}");
        }
        // A 5 % change is about 61 units: carried whole until a second one crosses a quantum.
        let (quanta, carried) = pinch_quanta(0.0, 0.05);
        assert_eq!(quanta, 0);
        assert!((carried - 61.43).abs() < 0.01, "{carried}");
        let (quanta, carried) = pinch_quanta(carried, 0.05);
        assert_eq!(quanta, 1);
        assert!((carried - 2.86).abs() < 0.01, "{carried}");
    }

    #[test]
    fn a_short_chord_releases_what_landed_down_newest_first_and_still_fails() {
        let previous = Operation::System(SystemGesture::DesktopPrevious);
        for (landed, releases) in [
            (1, vec![up(CONTROL)]),
            (2, vec![up(WIN), up(CONTROL)]),
            (3, vec![up(LEFT), up(WIN), up(CONTROL)]),
            (4, vec![up(WIN), up(CONTROL)]),
            (5, vec![up(CONTROL)]),
        ] {
            let mut synthetic = Synthetic::new();
            let (result, calls) = scripted(&mut synthetic, previous, &[landed]);
            assert_eq!(
                result,
                Err(InputError::PartialSendInput {
                    sent: landed,
                    attempted: 6,
                    code: 5
                })
            );
            assert_eq!(calls, [chord(&[CONTROL, WIN], LEFT), releases]);
            assert!(synthetic.stranded.is_empty());
        }
        let back = Operation::System(SystemGesture::NavigateBack);
        let (result, calls) = scripted(&mut Synthetic::new(), back, &[1]);
        assert!(result.is_err());
        let [_, x_up] = x_click(XBUTTON1);
        assert_eq!(calls[1], [x_up]);
    }

    #[test]
    fn keys_a_failed_release_strands_are_released_before_any_later_input_and_by_ending() {
        let previous = Operation::System(SystemGesture::DesktopPrevious);
        let mut synthetic = Synthetic::new();
        let (result, calls) = scripted(&mut synthetic, previous, &[3, 1]);
        assert!(result.is_err());
        assert_eq!(calls[1], [up(LEFT), up(WIN), up(CONTROL)]);
        assert_eq!(
            planned(&[], &[], &synthetic, PRESS_A),
            [up(WIN), up(CONTROL), down(A)],
            "Win and Ctrl stay down on Windows, so they go up before the next key"
        );

        let (result, calls) = scripted(&mut synthetic, Operation::EndGestures, &[0]);
        assert!(result.is_err());
        assert_eq!(calls, [vec![up(WIN), up(CONTROL)]]);
        let (result, calls) = scripted(&mut synthetic, Operation::EndGestures, &[]);
        assert_eq!(result, Ok(()));
        assert_eq!(calls, [vec![up(WIN), up(CONTROL)]]);
        assert!(planned(&[], &[], &synthetic, Operation::EndGestures).is_empty());

        let (_, calls) = scripted(&mut synthetic, previous, &[2, 0]);
        assert_eq!(calls[1], [up(WIN), up(CONTROL)]);
        let one = Operation::PinchWheel { quanta: 1 };
        assert_eq!(
            planned(&[], &[], &synthetic, one),
            [up(WIN), up(CONTROL), down(CONTROL), wheel(120)],
            "a pinch step retries them too, before its own Ctrl"
        );
    }

    #[test]
    fn the_synthetic_ctrl_follows_only_what_landed() {
        let mut synthetic = holding_ctrl();
        let (result, calls) = scripted(&mut synthetic, PRESS_A, &[1]);
        assert!(result.is_err());
        assert!(!synthetic.ctrl, "the Ctrl release landed");
        assert_eq!(calls.len(), 1, "the key that never landed needs no release");

        let mut synthetic = holding_ctrl();
        let (result, calls) = scripted(&mut synthetic, PRESS_A, &[0]);
        assert!(result.is_err());
        assert!(synthetic.ctrl, "nothing landed");
        assert_eq!(calls.len(), 1);

        let mut synthetic = Synthetic::new();
        let one = Operation::PinchWheel { quanta: 1 };
        let (result, calls) = scripted(&mut synthetic, one, &[1]);
        assert!(result.is_err());
        assert!(synthetic.ctrl, "the Ctrl went down before its wheel failed");
        assert_eq!(
            calls.len(),
            1,
            "the pinch Ctrl is released by ending, not unwound"
        );
        let (result, _) = scripted(&mut synthetic, Operation::EndGestures, &[]);
        assert_eq!(result, Ok(()));
        assert!(!synthetic.ctrl);
    }
}
