//! Native destination adapters. Construction is explicit and never enables capture.

#[cfg(any(windows, target_os = "macos"))]
use crate::session_trial::TrialInputGuard;
use crate::{
    session_actor::WatchedDestination,
    session_receiver::{DestinationAction, DestinationFailure, InputDestination},
};
#[cfg(any(windows, target_os = "macos"))]
use monhop_core::HidUsage;
use monhop_core::{DeviceId, DisplayId, Point, RevocationSignal, capture::NativeInputOwnership};
use monhop_protocol::{DisplayDescription, DisplayTopology};
#[cfg(any(windows, target_os = "macos"))]
use std::collections::BTreeSet;

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
    _ownership: NativeInputOwnership,
}

impl NativeDestination {
    pub(crate) fn new_after_local_enable(
        device: DeviceId,
        displays: DisplayTopology,
        ownership: NativeInputOwnership,
        revocation: RevocationSignal,
    ) -> Result<Self, DestinationFailure> {
        if revocation.is_stopping() {
            return Err(note_step(1));
        }
        if !current_displays(device)
            .map_err(|_| note_step(2))?
            .same_geometry(&displays)
        {
            return Err(note_step(3));
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
        let mut result = Self {
            _ownership: ownership,
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
        result.validate_environment()?;
        Ok(result)
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct TrialDestination {
    device: DeviceId,
    displays: DisplayTopology,
    point_map: TrialPointMap,
    guard: TrialInputGuard,
    keys: TrialKeyGate,
    #[cfg(target_os = "macos")]
    injector: monhop_platform_macos::MacInjector,
    #[cfg(windows)]
    injector: monhop_platform_windows::Injector,
    #[cfg(windows)]
    desktop: monhop_platform_windows::VirtualDesktop,
    #[cfg(windows)]
    scroll: WheelResidual,
    _ownership: NativeInputOwnership,
}

#[cfg(any(windows, target_os = "macos"))]
impl TrialDestination {
    // macOS posts only to this process, so a stray event cannot reach another app. Windows has no
    // per-process posting: SendInput goes to whatever is in front, so the trial window being the
    // foreground window IS the confinement, and losing it revokes before the next event is sent.
    pub(crate) fn new_after_local_enable(
        device: DeviceId,
        displays: DisplayTopology,
        receiver_bounds: (Point, Point),
        ownership: NativeInputOwnership,
        guard: TrialInputGuard,
    ) -> Result<Self, DestinationFailure> {
        if !guard.is_live() {
            return Err(DestinationFailure);
        }
        let point_map = TrialPointMap::new(&displays, receiver_bounds)?;
        #[cfg(windows)]
        let desktop = virtual_desktop(&displays)?;
        let mut result = Self {
            device,
            displays,
            point_map,
            guard,
            keys: TrialKeyGate::default(),
            #[cfg(target_os = "macos")]
            injector: monhop_platform_macos::MacInjector::for_current_process()
                .map_err(|_| DestinationFailure)?,
            #[cfg(windows)]
            injector: monhop_platform_windows::Injector::new(),
            #[cfg(windows)]
            desktop,
            #[cfg(windows)]
            scroll: WheelResidual::default(),
            _ownership: ownership,
        };
        result.validate_environment()?;
        Ok(result)
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl WatchedDestination for TrialDestination {
    fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
        // A paused startup is still live; only a revoked trial fails the environment.
        if !self.guard.is_live() {
            self.guard.revoke();
            return Err(DestinationFailure);
        }
        #[cfg(target_os = "macos")]
        let platform_ready = monhop_platform_macos::preflight_permissions()
            .map_err(|_| DestinationFailure)?
            .accessibility;
        #[cfg(windows)]
        let platform_ready = monhop_platform_windows::desktop_state::ordinary_desktop_is_active();
        if !platform_ready || !current_displays(self.device)?.same_geometry(&self.displays) {
            self.guard.revoke();
            return Err(DestinationFailure);
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl InputDestination for TrialDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        if matches!(action, DestinationAction::ReleaseAll) {
            self.keys.clear();
            #[cfg(windows)]
            {
                self.scroll = WheelResidual::default();
            }
            return self.injector.release_all().map_err(|_| DestinationFailure);
        }
        if !self.guard.allows_new_input() {
            return Err(DestinationFailure);
        }
        if let DestinationAction::Key { usage, pressed } = action
            && self.keys.allows(usage, pressed).is_err()
        {
            self.guard.reject_system_shortcut();
            return Err(DestinationFailure);
        }
        #[cfg(target_os = "macos")]
        let result = match action {
            DestinationAction::MoveTo(point) => self
                .injector
                .move_to(self.point_map.map(point)?)
                .map_err(|_| DestinationFailure),
            DestinationAction::Key { usage, pressed } => self
                .injector
                .key(usage, pressed)
                .map_err(|_| DestinationFailure),
            DestinationAction::Button { button, pressed } => self
                .injector
                .button(button, pressed)
                .map_err(|_| DestinationFailure),
            DestinationAction::Scroll {
                horizontal,
                vertical,
            } => self
                .injector
                .scroll(
                    -horizontal * MAC_POINTS_PER_DETENT,
                    vertical * MAC_POINTS_PER_DETENT,
                )
                .map_err(|_| DestinationFailure),
            DestinationAction::ReleaseAll => unreachable!(),
        };
        #[cfg(windows)]
        let result = {
            use monhop_platform_windows::InjectionOperation as Op;
            let operation = match action {
                DestinationAction::MoveTo(point) => {
                    let mapped = self.point_map.map(point)?;
                    Op::AbsoluteMove {
                        x: mapped.x.floor() as i32,
                        y: mapped.y.floor() as i32,
                        desktop: self.desktop,
                    }
                }
                DestinationAction::Key { usage, pressed } => Op::Key { usage, pressed },
                DestinationAction::Button { button, pressed } => Op::Button { button, pressed },
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
                DestinationAction::ReleaseAll => unreachable!(),
            };
            self.injector
                .inject(operation)
                .map_err(|_| DestinationFailure)
        };
        result?;
        if let DestinationAction::Key { usage, pressed } = action {
            self.keys.record(usage, pressed);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn virtual_desktop(
    displays: &DisplayTopology,
) -> Result<monhop_platform_windows::VirtualDesktop, DestinationFailure> {
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
    monhop_platform_windows::VirtualDesktop::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
    .map_err(|_| DestinationFailure)
}

#[cfg(any(windows, target_os = "macos"))]
#[derive(Default)]
struct TrialKeyGate {
    ordinary: BTreeSet<HidUsage>,
    control_or_command: BTreeSet<HidUsage>,
}

#[cfg(any(windows, target_os = "macos"))]
impl TrialKeyGate {
    fn allows(&self, usage: HidUsage, pressed: bool) -> Result<(), DestinationFailure> {
        if !is_trial_key(usage) || is_windows_start_key(usage) {
            return Err(DestinationFailure);
        }
        if !pressed {
            return Ok(());
        }
        if is_trial_ordinary_key(usage) && !self.control_or_command.is_empty() {
            return Err(DestinationFailure);
        }
        if is_control_or_command(usage) && !self.ordinary.is_empty() {
            return Err(DestinationFailure);
        }
        Ok(())
    }

    fn record(&mut self, usage: HidUsage, pressed: bool) {
        if is_trial_ordinary_key(usage) {
            update_held(&mut self.ordinary, usage, pressed);
        }
        if is_control_or_command(usage) {
            update_held(&mut self.control_or_command, usage, pressed);
        }
    }

    fn clear(&mut self) {
        self.ordinary.clear();
        self.control_or_command.clear();
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn is_trial_key(usage: HidUsage) -> bool {
    is_trial_ordinary_key(usage) || (0xe0..=0xe7).contains(&usage.0)
}

#[cfg(any(windows, target_os = "macos"))]
fn is_trial_ordinary_key(usage: HidUsage) -> bool {
    (0x04..=0x39).contains(&usage.0)
}

#[cfg(any(windows, target_os = "macos"))]
fn is_control_or_command(usage: HidUsage) -> bool {
    // Alt drives Windows system shortcuts (Alt+Tab, Alt+Esc, Alt+Space); Option only types on macOS.
    matches!(usage.0, 0xe0 | 0xe3 | 0xe4 | 0xe7)
        || (cfg!(windows) && matches!(usage.0, 0xe2 | 0xe6))
}

/// A lone Windows key opens Start, so the GUI keys never reach SendInput.
#[cfg(any(windows, target_os = "macos"))]
fn is_windows_start_key(usage: HidUsage) -> bool {
    cfg!(windows) && matches!(usage.0, 0xe3 | 0xe7)
}

#[cfg(any(windows, target_os = "macos"))]
fn update_held(held: &mut BTreeSet<HidUsage>, usage: HidUsage, pressed: bool) {
    if pressed {
        held.insert(usage);
    } else {
        held.remove(&usage);
    }
}

#[cfg(any(windows, target_os = "macos"))]
struct TrialPointMap {
    source_rectangles: Vec<(Point, Point)>,
    source_origin: Point,
    destination_origin: Point,
    scale: f64,
}

#[cfg(any(windows, target_os = "macos"))]
impl TrialPointMap {
    fn new(
        displays: &DisplayTopology,
        (destination_origin, destination_size): (Point, Point),
    ) -> Result<Self, DestinationFailure> {
        if !destination_origin.is_finite()
            || !destination_size.is_finite()
            || destination_size.x <= 0.0
            || destination_size.y <= 0.0
        {
            return Err(DestinationFailure);
        }
        let mut left = f64::INFINITY;
        let mut top = f64::INFINITY;
        let mut right = f64::NEG_INFINITY;
        let mut bottom = f64::NEG_INFINITY;
        let mut source_rectangles = Vec::with_capacity(displays.displays().len());
        for display in displays.displays() {
            let origin = display.logical_origin;
            let max = Point::new(
                origin.x + display.logical_size.x,
                origin.y + display.logical_size.y,
            );
            if !origin.is_finite()
                || !max.is_finite()
                || display.logical_size.x <= 0.0
                || display.logical_size.y <= 0.0
            {
                return Err(DestinationFailure);
            }
            left = left.min(origin.x);
            top = top.min(origin.y);
            right = right.max(max.x);
            bottom = bottom.max(max.y);
            source_rectangles.push((origin, max));
        }
        let source_size = Point::new(right - left, bottom - top);
        if !source_size.is_finite() || source_size.x <= 0.0 || source_size.y <= 0.0 {
            return Err(DestinationFailure);
        }
        let scale = (destination_size.x / source_size.x).min(destination_size.y / source_size.y);
        if !scale.is_finite() || scale <= 0.0 {
            return Err(DestinationFailure);
        }
        let mapped_size = Point::new(source_size.x * scale, source_size.y * scale);
        let destination_origin = Point::new(
            destination_origin.x + (destination_size.x - mapped_size.x) / 2.0,
            destination_origin.y + (destination_size.y - mapped_size.y) / 2.0,
        );
        if !destination_origin.is_finite() {
            return Err(DestinationFailure);
        }
        Ok(Self {
            source_rectangles,
            source_origin: Point::new(left, top),
            destination_origin,
            scale,
        })
    }

    fn map(&self, point: Point) -> Result<Point, DestinationFailure> {
        if !point.is_finite()
            || !self.source_rectangles.iter().any(|(origin, max)| {
                point.x >= origin.x && point.y >= origin.y && point.x < max.x && point.y < max.y
            })
        {
            return Err(DestinationFailure);
        }
        let mapped = Point::new(
            self.destination_origin.x + (point.x - self.source_origin.x) * self.scale,
            self.destination_origin.y + (point.y - self.source_origin.y) * self.scale,
        );
        mapped
            .is_finite()
            .then_some(mapped)
            .ok_or(DestinationFailure)
    }
}

impl WatchedDestination for NativeDestination {
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
        if !current_displays(self.device)
            .map_err(|_| note_step(7))?
            .same_geometry(&self.displays)
        {
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
                DestinationAction::Button { button, pressed } => Op::Button { button, pressed },
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
                DestinationAction::Button { button, pressed } => {
                    self.injector.button(button, pressed)
                }
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

    #[cfg(target_os = "macos")]
    #[test]
    fn trial_points_map_uniformly_into_the_receiver_pad() {
        let displays = DisplayTopology::new(vec![DisplayDescription {
            id: monhop_core::DisplayId(7),
            name: "Receiver".into(),
            native_width: 200,
            native_height: 100,
            logical_origin: Point::new(0.0, 0.0),
            logical_size: Point::new(200.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .expect("valid receiver topology");
        let map = TrialPointMap::new(
            &displays,
            (Point::new(10.0, 20.0), Point::new(100.0, 100.0)),
        )
        .expect("valid receiver pad");

        assert_eq!(map.map(Point::new(0.0, 0.0)), Ok(Point::new(10.0, 45.0)));
        assert_eq!(map.map(Point::new(100.0, 50.0)), Ok(Point::new(60.0, 70.0)));
        assert_eq!(map.map(Point::new(200.0, 50.0)), Err(DestinationFailure));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trial_key_gate_allows_only_bounded_non_shortcut_input() {
        for usage in 0x04..=0x39 {
            let key = HidUsage(usage);
            let mut gate = TrialKeyGate::default();
            assert_eq!(gate.allows(key, true), Ok(()));
            gate.record(key, true);
            assert_eq!(gate.allows(key, false), Ok(()));
            gate.record(key, false);
        }
        for usage in 0xe0..=0xe7 {
            let key = HidUsage(usage);
            let mut gate = TrialKeyGate::default();
            assert_eq!(gate.allows(key, true), Ok(()));
            gate.record(key, true);
            assert_eq!(gate.allows(key, false), Ok(()));
            gate.record(key, false);
        }
        for usage in [0x3a, 0x3b, 0x45, 0x4f, 0x52] {
            assert_eq!(
                TrialKeyGate::default().allows(HidUsage(usage), true),
                Err(DestinationFailure)
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trial_key_gate_keeps_option_combinations_and_lone_command_on_macos() {
        for option in [HidUsage(0xe2), HidUsage(0xe6)] {
            let mut gate = TrialKeyGate::default();
            assert_eq!(gate.allows(option, true), Ok(()));
            gate.record(option, true);
            assert_eq!(gate.allows(HidUsage(0x2b), true), Ok(()));
        }
        assert_eq!(TrialKeyGate::default().allows(HidUsage(0xe3), true), Ok(()));
    }

    #[cfg(windows)]
    #[test]
    fn trial_key_gate_rejects_alt_combinations_and_the_windows_key() {
        for alt in [HidUsage(0xe2), HidUsage(0xe6)] {
            let mut alt_first = TrialKeyGate::default();
            assert_eq!(alt_first.allows(alt, true), Ok(()));
            alt_first.record(alt, true);
            for key in [0x2b, 0x29, 0x2c] {
                assert_eq!(
                    alt_first.allows(HidUsage(key), true),
                    Err(DestinationFailure)
                );
            }
            let mut key_first = TrialKeyGate::default();
            assert_eq!(key_first.allows(HidUsage(0x2b), true), Ok(()));
            key_first.record(HidUsage(0x2b), true);
            assert_eq!(key_first.allows(alt, true), Err(DestinationFailure));
        }
        for gui in [HidUsage(0xe3), HidUsage(0xe7)] {
            assert_eq!(
                TrialKeyGate::default().allows(gui, true),
                Err(DestinationFailure)
            );
            assert_eq!(
                TrialKeyGate::default().allows(gui, false),
                Err(DestinationFailure)
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trial_key_gate_rejects_command_or_control_in_both_orders() {
        for modifier in [
            HidUsage(0xe0),
            HidUsage(0xe3),
            HidUsage(0xe4),
            HidUsage(0xe7),
        ] {
            let mut modifier_first = TrialKeyGate::default();
            assert_eq!(modifier_first.allows(modifier, true), Ok(()));
            modifier_first.record(modifier, true);
            assert_eq!(
                modifier_first.allows(HidUsage(0x35), true),
                Err(DestinationFailure)
            );

            let mut key_first = TrialKeyGate::default();
            assert_eq!(key_first.allows(HidUsage(0x04), true), Ok(()));
            key_first.record(HidUsage(0x04), true);
            assert_eq!(key_first.allows(modifier, true), Err(DestinationFailure));
        }

        let mut control = TrialKeyGate::default();
        assert_eq!(control.allows(HidUsage(0xe0), true), Ok(()));
        control.record(HidUsage(0xe0), true);
        assert_eq!(
            control.allows(HidUsage(0x3b), true),
            Err(DestinationFailure)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trial_key_gate_clears_held_state_for_cleanup() {
        let mut gate = TrialKeyGate::default();
        gate.record(HidUsage(0x04), true);
        gate.record(HidUsage(0xe3), true);
        gate.clear();
        assert_eq!(gate.allows(HidUsage(0xe3), true), Ok(()));
        assert_eq!(gate.allows(HidUsage(0x04), true), Ok(()));
    }

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
