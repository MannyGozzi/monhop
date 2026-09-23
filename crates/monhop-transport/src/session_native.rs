//! Native destination adapters. Construction is explicit and never enables capture.

use crate::{
    session::check_read_displays,
    session_actor::WatchedDestination,
    session_receiver::{DestinationAction, DestinationFailure, InputDestination},
};
use monhop_core::{DeviceId, DisplayId, InjectionPermit, Point, RevocationSignal, TakeBackGate};
use monhop_protocol::{DisplayDescription, DisplayTopology};

/// Scroll wire values use signed wheel detents: positive right and positive up.
/// A macOS pixel-scroll detent is forty points, with residual fractions retained by the injector.
pub const MAC_POINTS_PER_DETENT: f64 = 40.0;
pub const WINDOWS_UNITS_PER_DETENT: f64 = 120.0;

/// The last native destination check that failed in this process, as a small code for diagnostics.
static LAST_DESTINATION_STEP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn last_destination_step() -> u8 {
    LAST_DESTINATION_STEP.load(std::sync::atomic::Ordering::Acquire)
}

fn note_step(step: u8) -> DestinationFailure {
    LAST_DESTINATION_STEP.store(step, std::sync::atomic::Ordering::Release);
    DestinationFailure
}

pub fn current_displays(device: DeviceId) -> Result<DisplayTopology, DestinationFailure> {
    #[cfg(windows)]
    let displays =
        monhop_platform_windows::enumerate_displays(device).map_err(|_| DestinationFailure)?;
    #[cfg(target_os = "macos")]
    let displays = monhop_platform_macos::enumerate_active_displays()
        .map_err(|_| DestinationFailure)?
        .into_iter()
        .map(|d| {
            let name = d
                .name
                .clone()
                .unwrap_or_else(|| format!("Display {}", d.id.0));
            d.into_core_display(device, name)
        })
        .collect::<Vec<_>>();
    #[cfg(not(any(windows, target_os = "macos")))]
    let displays: Vec<monhop_core::Display> = {
        let _ = device;
        return Err(DestinationFailure);
    };
    DisplayTopology::new(
        displays
            .into_iter()
            .map(|d| DisplayDescription {
                id: namespaced_display_id(device, d.id),
                name: d.name,
                native_width: d.native_size.width,
                native_height: d.native_size.height,
                logical_origin: d.origin,
                logical_size: Point::new(d.logical_size.width, d.logical_size.height),
                scale_factor: d.scale_factor as f32,
                is_primary: d.primary,
                monitor: d.monitor,
            })
            .collect(),
    )
    .map_err(|_| DestinationFailure)
}

/// The OS pointer in the same coordinates the capture hook reports, or None where no query exists.
pub(crate) fn current_pointer_position() -> Option<Point> {
    #[cfg(target_os = "macos")]
    {
        monhop_platform_macos::native_capture::current_pointer_position()
    }
    #[cfg(windows)]
    {
        monhop_platform_windows::native_capture::current_pointer_position()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// Two computers can report the same native display id (two Macs, two Windows PCs), so every
/// id carries its owning computer; both sides derive the peer's ids from the same rule.
pub fn namespaced_display_id(device: DeviceId, native: DisplayId) -> DisplayId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in device.0.iter().chain(native.0.to_le_bytes().iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    DisplayId(hash)
}

pub(crate) struct NativeDestination {
    device: DeviceId,
    displays: DisplayTopology,
    revocation: RevocationSignal,
    #[cfg(windows)]
    injector: monhop_platform_windows::Injector,
    #[cfg(windows)]
    desktop: monhop_platform_windows::VirtualDesktop,
    #[cfg(windows)]
    scroll: WheelResidual,
    #[cfg(target_os = "macos")]
    injector: monhop_platform_macos::MacInjector,
    /// The macOS permission query is a TCC round trip; the injection thread asks it at most every
    /// PERMISSION_RECHECK while displays are still compared on every call.
    #[cfg(target_os = "macos")]
    permission_checked: Option<std::time::Instant>,
    _ownership: InjectionPermit,
    admission: InjectionLedger,
    displays_changed: bool,
}

impl NativeDestination {
    pub(crate) fn new_after_local_enable(
        device: DeviceId,
        displays: DisplayTopology,
        ownership: InjectionPermit,
        revocation: RevocationSignal,
        gate: TakeBackGate,
    ) -> Result<Self, DestinationFailure> {
        if revocation.is_stopping() {
            return Err(note_step(1));
        }
        #[cfg(windows)]
        let (left, top, right, bottom) = displays.displays().iter().fold(
            (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ),
            |(l, t, r, b), d| {
                (
                    l.min(d.logical_origin.x),
                    t.min(d.logical_origin.y),
                    r.max(d.logical_origin.x + d.logical_size.x),
                    b.max(d.logical_origin.y + d.logical_size.y),
                )
            },
        );
        let result = Self {
            _ownership: ownership,
            admission: InjectionLedger::new(gate),
            displays_changed: false,
            device,
            displays,
            revocation,
            #[cfg(windows)]
            injector: monhop_platform_windows::Injector::new(),
            #[cfg(windows)]
            desktop: monhop_platform_windows::VirtualDesktop::new(
                left as i32,
                top as i32,
                (right - left) as u32,
                (bottom - top) as u32,
            )
            .map_err(|_| DestinationFailure)?,
            #[cfg(windows)]
            scroll: WheelResidual::default(),
            #[cfg(target_os = "macos")]
            injector: monhop_platform_macos::MacInjector::new().map_err(|_| note_step(4))?,
            #[cfg(target_os = "macos")]
            permission_checked: None,
        };
        Ok(result)
    }
}

impl WatchedDestination for NativeDestination {
    fn local_displays_changed(&self) -> bool {
        self.displays_changed
    }
    fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
        if self.revocation.is_stopping() {
            return Err(note_step(5));
        }
        #[cfg(windows)]
        if !monhop_platform_windows::desktop_state::ordinary_desktop_is_active() {
            return Err(note_step(6));
        }
        #[cfg(target_os = "macos")]
        if self
            .permission_checked
            .is_none_or(|checked| checked.elapsed() >= PERMISSION_RECHECK)
        {
            if !monhop_platform_macos::preflight_permissions()
                .map_err(|_| note_step(7))?
                .accessibility
            {
                return Err(note_step(6));
            }
            self.permission_checked = Some(std::time::Instant::now());
        }
        if check_read_displays(&self.displays, current_displays(self.device)).is_err() {
            self.displays_changed = true;
            return Err(note_step(8));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
const PERMISSION_RECHECK: std::time::Duration = std::time::Duration::from_millis(250);

impl InputDestination for NativeDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        require_action_authorization(&self.revocation, action)?;
        if !self.admission.begin(action) {
            return Ok(());
        }
        let result = self.post(action);
        if result.is_ok() {
            self.admission.complete(action);
        }
        result
    }
}

impl NativeDestination {
    fn post(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        #[cfg(windows)]
        {
            use monhop_platform_windows::InjectionOperation as Op;
            let operation = match action {
                DestinationAction::MoveTo(point) => {
                    if !point.is_finite()
                        || !self.displays.displays().iter().any(|d| {
                            point.x >= d.logical_origin.x
                                && point.y >= d.logical_origin.y
                                && point.x < d.logical_origin.x + d.logical_size.x
                                && point.y < d.logical_origin.y + d.logical_size.y
                        })
                    {
                        return Err(DestinationFailure);
                    }
                    // Windows topology already uses physical pixels, including negative origins.
                    // Floor avoids rounding an interior fractional point into the next display.
                    Op::AbsoluteMove {
                        x: point.x.floor() as i32,
                        y: point.y.floor() as i32,
                        desktop: self.desktop,
                    }
                }
                DestinationAction::Key { usage, pressed } => Op::Key { usage, pressed },
                DestinationAction::Button {
                    button,
                    pressed,
                    click_count,
                } => Op::Button {
                    button,
                    pressed,
                    click_count,
                },
                DestinationAction::Scroll {
                    horizontal,
                    vertical,
                } => {
                    let (x, y, next) = self.scroll.convert(horizontal, vertical)?;
                    if x != 0 || y != 0 {
                        self.injector
                            .inject(Op::Scroll {
                                horizontal: x,
                                vertical: y,
                            })
                            .map_err(|_| DestinationFailure)?;
                    }
                    self.scroll = next;
                    return Ok(());
                }
                DestinationAction::ReleaseAll => {
                    self.scroll = WheelResidual::default();
                    return self.injector.release_all().map_err(|_| DestinationFailure);
                }
            };
            self.injector
                .inject(operation)
                .map_err(|_| DestinationFailure)
        }
        #[cfg(target_os = "macos")]
        {
            let step = match action {
                DestinationAction::MoveTo(_) => 10,
                DestinationAction::Key { .. } => 11,
                DestinationAction::Button { .. } => 12,
                DestinationAction::Scroll { .. } => 13,
                DestinationAction::ReleaseAll => 14,
            };
            match action {
                DestinationAction::MoveTo(point) => self.injector.move_to(point),
                DestinationAction::Key { usage, pressed } => self.injector.key(usage, pressed),
                DestinationAction::Button {
                    button,
                    pressed,
                    click_count,
                } => self.injector.button(button, pressed, click_count),
                DestinationAction::Scroll {
                    horizontal,
                    vertical,
                } => self.injector.scroll(
                    -horizontal * MAC_POINTS_PER_DETENT,
                    vertical * MAC_POINTS_PER_DETENT,
                ),
                DestinationAction::ReleaseAll => self.injector.release_all(),
            }
            .map_err(|_| note_step(step))
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = action;
            Err(DestinationFailure)
        }
    }
}

/// Tracks native transitions, not wire repeats, so the gate includes posts still in flight.
struct InjectionLedger {
    gate: TakeBackGate,
    keys: [bool; 256],
    buttons: [bool; 5],
}

impl InjectionLedger {
    fn new(gate: TakeBackGate) -> Self {
        Self {
            gate,
            keys: [false; 256],
            buttons: [false; 5],
        }
    }

    fn begin(&self, action: DestinationAction) -> bool {
        if matches!(action, DestinationAction::ReleaseAll) {
            return true;
        }
        let press = match action {
            DestinationAction::Key {
                usage,
                pressed: true,
            } => !self.keys[usize::from(usage.0)],
            DestinationAction::Button {
                button,
                pressed: true,
                ..
            } => !self.buttons[button.index()],
            _ => false,
        };
        if press {
            self.gate.note_injected_press();
        }
        if !self.gate.admits_injection() {
            if press {
                self.gate.note_injected_up();
            }
            return false;
        }
        true
    }

    fn complete(&mut self, action: DestinationAction) {
        match action {
            DestinationAction::Key { usage, pressed } => {
                let key = &mut self.keys[usize::from(usage.0)];
                if !pressed && *key {
                    self.gate.note_injected_up();
                }
                *key = pressed;
            }
            DestinationAction::Button {
                button, pressed, ..
            } => {
                let held = &mut self.buttons[button.index()];
                if !pressed && *held {
                    self.gate.note_injected_up();
                }
                *held = pressed;
            }
            DestinationAction::ReleaseAll => {
                self.keys.fill(false);
                self.buttons.fill(false);
                self.gate.note_injected_released();
            }
            _ => {}
        }
    }
}

fn require_action_authorization(
    revocation: &RevocationSignal,
    action: DestinationAction,
) -> Result<(), DestinationFailure> {
    // A stop or revocation forbids new input, but never ledger-owned release cleanup.
    if revocation.is_stopping() && !matches!(action, DestinationAction::ReleaseAll) {
        Err(note_step(9))
    } else {
        Ok(())
    }
}

#[cfg(any(windows, test))]
#[derive(Default)]
struct WheelResidual {
    horizontal: f64,
    vertical: f64,
}
#[cfg(any(windows, test))]
impl WheelResidual {
    fn convert(
        &self,
        horizontal: f64,
        vertical: f64,
    ) -> Result<(i32, i32, Self), DestinationFailure> {
        let x = self.horizontal + horizontal * WINDOWS_UNITS_PER_DETENT;
        let y = self.vertical + vertical * WINDOWS_UNITS_PER_DETENT;
        if !x.is_finite()
            || !y.is_finite()
            || x.abs() > f64::from(i32::MAX)
            || y.abs() > f64::from(i32::MAX)
        {
            return Err(DestinationFailure);
        }
        Ok((
            x.trunc() as i32,
            y.trunc() as i32,
            Self {
                horizontal: x.fract(),
                vertical: y.fract(),
            },
        ))
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn display_ids_are_namespaced_by_owning_computer() {
        use monhop_core::{DeviceId, DisplayId};
        let a = DeviceId([1; 16]);
        let b = DeviceId([2; 16]);
        assert_ne!(
            super::namespaced_display_id(a, DisplayId(1)),
            super::namespaced_display_id(b, DisplayId(1))
        );
        assert_ne!(
            super::namespaced_display_id(a, DisplayId(1)),
            super::namespaced_display_id(a, DisplayId(2))
        );
        assert_eq!(
            super::namespaced_display_id(a, DisplayId(1)),
            super::namespaced_display_id(a, DisplayId(1))
        );
    }

    use super::*;

    #[test]
    fn power_or_network_revocation_blocks_input_but_not_managed_release() {
        let signal = RevocationSignal::default();
        let movement = DestinationAction::MoveTo(Point::new(1.0, 1.0));
        assert_eq!(require_action_authorization(&signal, movement), Ok(()));
        signal.mark_revoked_without_wake();
        for action in [
            movement,
            DestinationAction::Key {
                usage: monhop_core::HidUsage(4),
                pressed: true,
            },
            DestinationAction::Key {
                usage: monhop_core::HidUsage(4),
                pressed: false,
            },
            DestinationAction::Button {
                button: monhop_core::MouseButton::Left,
                pressed: true,
                click_count: 2,
            },
            DestinationAction::Scroll {
                horizontal: 0.5,
                vertical: -0.5,
            },
        ] {
            assert_eq!(
                require_action_authorization(&signal, action),
                Err(DestinationFailure)
            );
        }
        assert_eq!(
            require_action_authorization(&signal, DestinationAction::ReleaseAll),
            Ok(())
        );
        assert!(signal.is_revoked());
    }

    #[test]
    fn high_resolution_scroll_keeps_fractional_signed_remainders() {
        let mut wheel = WheelResidual::default();
        let mut emitted = (0, 0);
        for _ in 0..10 {
            let (x, y, next) = wheel.convert(0.001, -0.001).unwrap();
            emitted.0 += x;
            emitted.1 += y;
            wheel = next;
        }
        assert_eq!(emitted, (1, -1));
        assert!((wheel.horizontal - 0.2).abs() < 1e-10);
        assert!((wheel.vertical + 0.2).abs() < 1e-10);
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use monhop_core::{FloorState, HidUsage, SharedFloor};
    #[test]
    fn pending_down_counts_before_admission_and_repeats_do_not_recount() {
        let floor = SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        let mut ledger = InjectionLedger::new(gate.clone());
        let down = DestinationAction::Key {
            usage: HidUsage(4),
            pressed: true,
        };
        assert!(ledger.begin(down));
        assert!(gate.injected_held());
        ledger.complete(down);
        assert!(ledger.begin(down));
        ledger.complete(down);
        let up = DestinationAction::Key {
            usage: HidUsage(4),
            pressed: false,
        };
        assert!(ledger.begin(up));
        ledger.complete(up);
        assert!(!gate.injected_held());
    }
    #[test]
    fn closed_admission_uncounts_a_down_and_release_is_still_allowed() {
        let gate = TakeBackGate::new(monhop_core::SharedFloor::new());
        let mut ledger = InjectionLedger::new(gate.clone());
        assert!(!ledger.begin(DestinationAction::Button {
            button: monhop_core::MouseButton::Left,
            pressed: true,
            click_count: 1,
        }));
        assert!(!gate.injected_held());
        gate.note_injected_press();
        assert!(ledger.begin(DestinationAction::ReleaseAll));
        ledger.complete(DestinationAction::ReleaseAll);
        assert!(!gate.injected_held());
    }
}
