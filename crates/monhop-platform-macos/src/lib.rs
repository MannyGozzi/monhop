//! Native macOS display and input adapter.
//!
//! Native calls are compiled only for macOS. Other targets retain the pure key
//! mapping APIs and return an explicit unsupported-platform error for OS work.

#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use monhop_core::{
    DeviceId, Display, DisplayId, HidUsage, LogicalRect, LogicalSize, MouseButton, NativeSize,
    Point,
};

pub use monhop_core::{capture, capture_control, capture_physical};
pub mod capture_decode;
#[cfg(target_os = "macos")]
mod continuous_clock;
#[cfg(target_os = "macos")]
pub use continuous_clock::{ClockError, ContinuousInstant};
#[cfg(target_os = "macos")]
mod power_watch;
#[cfg(target_os = "macos")]
pub use power_watch::{PowerWatch, PowerWatchError};
#[cfg(target_os = "macos")]
pub mod display_names;
#[cfg(target_os = "macos")]
#[path = "capture.rs"]
pub mod native_capture;

mod keymap;
pub use keymap::{KeyMapError, MacVirtualKey, hid_to_mac_virtual_key, mac_virtual_key_to_hid};

#[cfg(target_os = "macos")]
mod cf_owned;
#[cfg(target_os = "macos")]
mod event_tap;
#[cfg(target_os = "macos")]
pub mod identity;
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
pub mod network;
#[cfg(target_os = "macos")]
pub mod network_watch;
#[cfg(target_os = "macos")]
pub mod threads;
pub mod trial_window;
#[cfg(target_os = "macos")]
pub mod udp_receive;
#[cfg(not(target_os = "macos"))]
mod unsupported;
#[cfg(target_os = "macos")]
mod wifi_attachment;

#[cfg(target_os = "macos")]
use native as backend;
#[cfg(not(target_os = "macos"))]
use unsupported as backend;

pub const SYNTHETIC_EVENT_MARKER: i64 = 0x4C4B_4D01;
pub const MAX_DIAGNOSTIC_DURATION: Duration = Duration::from_secs(30);
pub const MAX_ABSOLUTE_POINTER_COORDINATE: f64 = 1_000_000_000.0;
pub const MAX_RELATIVE_POINTER_DELTA: f64 = 1_000_000.0;
pub const MAC_EVENT_FLAG_SHIFT: u64 = 0x0002_0000;
pub const MAC_EVENT_FLAG_CONTROL: u64 = 0x0004_0000;
pub const MAC_EVENT_FLAG_OPTION: u64 = 0x0008_0000;
pub const MAC_EVENT_FLAG_COMMAND: u64 = 0x0010_0000;

#[derive(Clone, Debug, PartialEq)]
pub struct MacDisplay {
    pub id: DisplayId,
    /// Core Graphics does not expose a user-facing display name. The caller may
    /// add one from a separately authorized source instead of accepting a fake.
    pub name: Option<String>,
    pub native_size: NativeSize,
    pub logical_bounds: LogicalRect,
    pub scale_factor: f64,
    pub refresh_rate_hz: Option<f64>,
    pub primary: bool,
    pub monitor: Option<monhop_core::MonitorIdentity>,
}

impl MacDisplay {
    /// Builds the core display model using an explicit caller-provided label.
    pub fn into_core_display(self, machine: DeviceId, name: String) -> Display {
        Display::new(
            self.id,
            machine,
            name,
            self.native_size,
            LogicalSize::new(
                self.logical_bounds.size.width,
                self.logical_bounds.size.height,
            ),
            self.logical_bounds.origin,
            self.scale_factor,
            self.refresh_rate_hz,
            self.primary,
        )
        .with_monitor(self.monitor)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermissionState {
    pub accessibility: bool,
    pub listen_events: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassiveDiagnosticCounts {
    pub keyboard_events: u64,
    pub pointer_events: u64,
    pub scroll_events: u64,
    pub synthetic_events_filtered: u64,
    pub tap_disabled_events: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacError {
    UnsupportedPlatform,
    AccessibilityPermissionRequired,
    ListenEventPermissionRequired,
    DisplayEnumerationFailed,
    DisplayLimitExceeded,
    InvalidDisplayData,
    EventTapUnavailable,
    EventTapSourceUnavailable,
    EventTapDisabled,
    DiagnosticDurationZero,
    DiagnosticDurationTooLong,
    DiagnosticCancelled,
    InvalidPoint,
    PointOutOfRange,
    ScrollOutOfRange,
    CursorPositionUnknown,
    PointerBoundsUnavailable,
    UnsupportedHidUsage,
    NativeEventCreationFailed,
}

impl fmt::Display for MacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedPlatform => "macOS native APIs are unavailable on this platform",
            Self::AccessibilityPermissionRequired => "macOS Accessibility permission is required",
            Self::ListenEventPermissionRequired => "macOS listen-event permission is required",
            Self::DisplayEnumerationFailed => "macOS display enumeration failed",
            Self::DisplayLimitExceeded => "macOS reported more displays than the supported limit",
            Self::InvalidDisplayData => "macOS returned invalid display metadata",
            Self::EventTapUnavailable => "macOS could not create the passive event tap",
            Self::EventTapSourceUnavailable => {
                "macOS could not create the event-tap run-loop source"
            }
            Self::EventTapDisabled => "macOS disabled the passive event tap",
            Self::DiagnosticDurationZero => "passive diagnostic duration must be non-zero",
            Self::DiagnosticDurationTooLong => "passive diagnostic duration exceeds thirty seconds",
            Self::DiagnosticCancelled => "passive diagnostic was cancelled",
            Self::InvalidPoint => "input point must be finite",
            Self::PointOutOfRange => "input point exceeds the supported native range",
            Self::ScrollOutOfRange => "scroll value exceeds the native event range",
            Self::CursorPositionUnknown => {
                "pointer position is unknown; send an absolute move before pointer input"
            }
            Self::PointerBoundsUnavailable => {
                "no valid display bounds are available for pointer input"
            }
            Self::UnsupportedHidUsage => "USB HID usage has no macOS physical key mapping",
            Self::NativeEventCreationFailed => "macOS could not create an input event",
        })
    }
}

impl Error for MacError {}

impl From<KeyMapError> for MacError {
    fn from(_: KeyMapError) -> Self {
        Self::UnsupportedHidUsage
    }
}

/// Enumerates active Core Graphics displays. This does not prompt or change any
/// system state.
pub fn enumerate_active_displays() -> Result<Vec<MacDisplay>, MacError> {
    backend::enumerate_active_displays()
}

/// Reads the current permission state without prompting the user.
pub fn preflight_permissions() -> Result<PermissionState, MacError> {
    backend::preflight_permissions()
}

/// Explicitly asks macOS to show its standard permission prompts when needed.
pub fn request_permissions() -> Result<PermissionState, MacError> {
    backend::request_permissions()
}

/// Runs a bounded, listen-only diagnostic on the calling thread. The callback
/// counts categories only and always returns the original event unchanged.
pub fn run_passive_diagnostic(duration: Duration) -> Result<PassiveDiagnosticCounts, MacError> {
    run_passive_diagnostic_with_cancel(duration, &AtomicBool::new(false))
}

/// Runs a listen-only diagnostic that tears down its tap when cancelled.
/// Cancellation is checked between bounded run-loop slices, including under input load.
pub fn run_passive_diagnostic_with_cancel(
    duration: Duration,
    cancelled: &AtomicBool,
) -> Result<PassiveDiagnosticCounts, MacError> {
    if duration.is_zero() {
        return Err(MacError::DiagnosticDurationZero);
    }
    if duration > MAX_DIAGNOSTIC_DURATION {
        return Err(MacError::DiagnosticDurationTooLong);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(MacError::DiagnosticCancelled);
    }
    backend::run_passive_diagnostic(duration, SYNTHETIC_EVENT_MARKER, cancelled)
}

/// Returns the Quartz modifier flags represented by the supplied held physical
/// HID keys. Left and right sides use distinct key codes but share each Quartz
/// modifier bit until both sides are released.
pub fn mac_modifier_flags_from_held_keys(usages: impl IntoIterator<Item = HidUsage>) -> u64 {
    usages.into_iter().fold(0, |flags, usage| {
        flags
            | match usage.0 {
                0xE0 | 0xE4 => MAC_EVENT_FLAG_CONTROL,
                0xE1 | 0xE5 => MAC_EVENT_FLAG_SHIFT,
                0xE2 | 0xE6 => MAC_EVENT_FLAG_OPTION,
                0xE3 | 0xE7 => MAC_EVENT_FLAG_COMMAND,
                _ => 0,
            }
    })
}

/// Selects the drag button sent with a pointer movement. Quartz supports one
/// primary drag event at a time, so this uses a fixed left/right/other priority.
pub fn drag_button_from_held_buttons(
    buttons: impl IntoIterator<Item = MouseButton>,
) -> Option<MouseButton> {
    let mut held = [false; 5];
    for button in buttons {
        held[mouse_button_index(button)] = true;
    }
    [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ]
    .into_iter()
    .find(|button| held[mouse_button_index(*button)])
}

/// Tracks submitted input and clamps pointer motion to a snapshot of display bounds.
/// Recreate after display changes; posting does not acknowledge delivery.
#[derive(Debug)]
pub struct MacInjector {
    destination: backend::PostingDestination,
    held_keys: BTreeSet<HidUsage>,
    held_buttons: BTreeSet<MouseButton>,
    scroll_residual: ScrollResidual,
    cursor: CursorState,
}

impl MacInjector {
    pub fn new() -> Result<Self, MacError> {
        Self::with_destination(backend::system_destination())
    }

    /// Creates an injector whose events can only be delivered to this process.
    /// It is intended for bounded native fixture checks, not application input.
    pub fn for_current_process() -> Result<Self, MacError> {
        Self::with_destination(backend::current_process_destination()?)
    }

    fn with_destination(destination: backend::PostingDestination) -> Result<Self, MacError> {
        backend::ensure_injection_permission()?;
        let cursor = CursorState::new(
            enumerate_active_displays()?
                .into_iter()
                .map(|display| display.logical_bounds),
        )?;
        Ok(Self {
            destination,
            held_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            scroll_residual: ScrollResidual::default(),
            cursor,
        })
    }

    pub fn held_keys(&self) -> impl ExactSizeIterator<Item = HidUsage> + '_ {
        self.held_keys.iter().copied()
    }

    pub fn held_buttons(&self) -> impl ExactSizeIterator<Item = MouseButton> + '_ {
        self.held_buttons.iter().copied()
    }

    pub fn key(&mut self, usage: HidUsage, is_down: bool) -> Result<(), MacError> {
        if !should_post_held_state_event(is_down, self.held_keys.contains(&usage)) {
            return Ok(());
        }
        let virtual_key = hid_to_mac_virtual_key(usage)?;
        let flags = self.modifier_flags_for_key(usage, is_down);
        backend::post_key(
            self.destination,
            virtual_key,
            is_down,
            SYNTHETIC_EVENT_MARKER,
            flags,
        )?;
        if is_down {
            self.held_keys.insert(usage);
        } else {
            self.held_keys.remove(&usage);
        }
        Ok(())
    }

    /// Submits a button event at the last successful pointer anchor.
    /// It returns CursorPositionUnknown without an anchor and does not acknowledge delivery.
    pub fn button(&mut self, button: MouseButton, is_down: bool) -> Result<(), MacError> {
        if !should_post_held_state_event(is_down, self.held_buttons.contains(&button)) {
            return Ok(());
        }
        let destination = self.destination;
        let flags = self.modifier_flags();
        self.cursor.post_button(|point| {
            backend::post_button(
                destination,
                button,
                is_down,
                SYNTHETIC_EVENT_MARKER,
                flags,
                point,
            )
        })?;
        if is_down {
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
        Ok(())
    }

    pub fn scroll(&mut self, horizontal: f64, vertical: f64) -> Result<(), MacError> {
        backend::ensure_injection_permission()?;
        let flags = self.modifier_flags();
        let point = self.cursor.anchor()?;
        self.scroll_residual.post(horizontal, vertical, |x, y| {
            backend::post_scroll(self.destination, x, y, SYNTHETIC_EVENT_MARKER, flags, point)
        })
    }

    /// Submits an absolute point and anchors pointer state only after submission.
    /// Submission is not a downstream delivery acknowledgement.
    pub fn move_to(&mut self, point: Point) -> Result<(), MacError> {
        let destination = self.destination;
        let flags = self.modifier_flags();
        let drag_button = self.drag_button();
        self.cursor.post_absolute(point, |point| {
            backend::post_motion(
                destination,
                point,
                SYNTHETIC_EVENT_MARKER,
                flags,
                drag_button,
            )
        })
    }

    /// Submits a bounded delta from the last successful pointer anchor.
    /// It returns CursorPositionUnknown until an absolute move creates that anchor.
    /// Submission is not a downstream delivery acknowledgement.
    pub fn move_by(&mut self, delta: Point) -> Result<(), MacError> {
        let destination = self.destination;
        let flags = self.modifier_flags();
        let drag_button = self.drag_button();
        self.cursor.post_relative(delta, |point| {
            backend::post_motion(
                destination,
                point,
                SYNTHETIC_EVENT_MARKER,
                flags,
                drag_button,
            )
        })
    }

    /// Attempts every tracked release, preserving the first error after all
    /// cleanup attempts complete.
    pub fn release_all(&mut self) -> Result<(), MacError> {
        self.scroll_residual = ScrollResidual::default();
        let keys: Vec<_> = self.held_keys.iter().copied().collect();
        let buttons: Vec<_> = self.held_buttons.iter().copied().collect();
        let mut first_error = None;

        for usage in keys {
            if let Err(error) = self.key(usage, false) {
                first_error.get_or_insert(error);
            }
        }
        for button in buttons {
            if let Err(error) = self.button(button, false) {
                first_error.get_or_insert(error);
            }
        }
        clear_cursor_after_button_recovery(&self.held_buttons, &mut self.cursor);

        first_error.map_or(Ok(()), Err)
    }

    fn modifier_flags(&self) -> u64 {
        mac_modifier_flags_from_held_keys(self.held_keys.iter().copied())
    }

    fn modifier_flags_for_key(&self, usage: HidUsage, is_down: bool) -> u64 {
        let mut flags = self.modifier_flags();
        let modifier_flag = mac_modifier_flags_from_held_keys([usage]);
        if modifier_flag == 0 {
            return flags;
        }
        if is_down {
            flags |= modifier_flag;
        } else if !self
            .held_keys
            .iter()
            .copied()
            .any(|held| held != usage && mac_modifier_flags_from_held_keys([held]) == modifier_flag)
        {
            flags &= !modifier_flag;
        }
        flags
    }

    fn drag_button(&self) -> Option<MouseButton> {
        drag_button_from_held_buttons(self.held_buttons.iter().copied())
    }
}

#[derive(Debug)]
struct CursorState {
    bounds: CursorBounds,
    submitted: Option<Point>,
}

impl CursorState {
    fn new(bounds: impl IntoIterator<Item = LogicalRect>) -> Result<Self, MacError> {
        Ok(Self {
            bounds: CursorBounds::new(bounds)?,
            submitted: None,
        })
    }

    fn post_absolute(
        &mut self,
        point: Point,
        post: impl FnOnce(Point) -> Result<(), MacError>,
    ) -> Result<(), MacError> {
        validate_absolute_point(point)?;
        let point = self.bounds.clamp(point)?;
        post(point)?;
        self.submitted = Some(point);
        Ok(())
    }

    fn post_relative(
        &mut self,
        delta: Point,
        post: impl FnOnce(Point) -> Result<(), MacError>,
    ) -> Result<(), MacError> {
        validate_relative_delta(delta)?;
        let point = self.submitted.ok_or(MacError::CursorPositionUnknown)?;
        let point = self
            .bounds
            .clamp(Point::new(point.x + delta.x, point.y + delta.y))?;
        post(point)?;
        self.submitted = Some(point);
        Ok(())
    }

    fn post_button(
        &self,
        post: impl FnOnce(Point) -> Result<(), MacError>,
    ) -> Result<(), MacError> {
        post(self.anchor()?)
    }

    fn anchor(&self) -> Result<Point, MacError> {
        self.submitted.ok_or(MacError::CursorPositionUnknown)
    }

    fn clear(&mut self) {
        self.submitted = None;
    }
}

#[derive(Debug)]
struct CursorBounds {
    rectangles: Vec<PointerRectangle>,
}

impl CursorBounds {
    fn new(bounds: impl IntoIterator<Item = LogicalRect>) -> Result<Self, MacError> {
        let rectangles: Result<Vec<_>, _> = bounds
            .into_iter()
            .map(PointerRectangle::from_logical_bounds)
            .collect();
        let rectangles = rectangles?;
        if rectangles.is_empty() {
            return Err(MacError::PointerBoundsUnavailable);
        }
        Ok(Self { rectangles })
    }

    fn clamp(&self, point: Point) -> Result<Point, MacError> {
        if !point.is_finite() {
            return Err(MacError::InvalidPoint);
        }
        if let Some(rectangle) = self
            .rectangles
            .iter()
            .find(|rectangle| rectangle.contains(point))
        {
            return Ok(rectangle.clamp(point));
        }

        self.rectangles
            .iter()
            .map(|rectangle| {
                let clamped = rectangle.clamp(point);
                (squared_distance(point, clamped), clamped)
            })
            .min_by(|(left, _), (right, _)| left.total_cmp(right))
            .map(|(_, point)| point)
            .ok_or(MacError::PointerBoundsUnavailable)
    }
}

#[derive(Clone, Copy, Debug)]
struct PointerRectangle {
    min: Point,
    max: Point,
}

impl PointerRectangle {
    fn from_logical_bounds(bounds: LogicalRect) -> Result<Self, MacError> {
        let width = bounds.size.width;
        let height = bounds.size.height;
        if !bounds.origin.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
        {
            return Err(MacError::InvalidDisplayData);
        }
        let edge = Point::new(bounds.origin.x + width, bounds.origin.y + height);
        if !edge.is_finite()
            || edge.x <= bounds.origin.x
            || edge.y <= bounds.origin.y
            || edge.x.abs() > MAX_ABSOLUTE_POINTER_COORDINATE
            || edge.y.abs() > MAX_ABSOLUTE_POINTER_COORDINATE
            || bounds.origin.x.abs() > MAX_ABSOLUTE_POINTER_COORDINATE
            || bounds.origin.y.abs() > MAX_ABSOLUTE_POINTER_COORDINATE
        {
            return Err(MacError::InvalidDisplayData);
        }
        let max = Point::new(edge.x.next_down(), edge.y.next_down());
        if max.x < bounds.origin.x || max.y < bounds.origin.y {
            return Err(MacError::InvalidDisplayData);
        }
        Ok(Self {
            min: bounds.origin,
            max,
        })
    }

    fn contains(self, point: Point) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }

    fn clamp(self, point: Point) -> Point {
        Point::new(
            point.x.clamp(self.min.x, self.max.x),
            point.y.clamp(self.min.y, self.max.y),
        )
    }
}

fn squared_distance(from: Point, to: Point) -> f64 {
    let x = from.x - to.x;
    let y = from.y - to.y;
    x.mul_add(x, y * y)
}

fn clear_cursor_after_button_recovery(
    held_buttons: &BTreeSet<MouseButton>,
    cursor: &mut CursorState,
) {
    if held_buttons.is_empty() {
        cursor.clear();
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

#[derive(Debug, Default)]
struct ScrollResidual {
    pending: [f64; 2],
}

impl ScrollResidual {
    fn post(
        &mut self,
        horizontal: f64,
        vertical: f64,
        post: impl FnOnce(i32, i32) -> Result<(), MacError>,
    ) -> Result<(), MacError> {
        let total = [self.pending[0] + horizontal, self.pending[1] + vertical];
        if total.iter().any(|value| {
            !value.is_finite() || *value < f64::from(i32::MIN) || *value > f64::from(i32::MAX)
        }) {
            return Err(MacError::ScrollOutOfRange);
        }
        let whole = [total[0].trunc() as i32, total[1].trunc() as i32];
        if whole != [0, 0] {
            post(whole[0], whole[1])?;
        }
        // Quartz takes integer pixels. Preserve fractions only after a successful
        // post, so a failed event can be retried without consuming prior residuals.
        self.pending = [
            total[0] - f64::from(whole[0]),
            total[1] - f64::from(whole[1]),
        ];
        Ok(())
    }
}

pub fn validate_absolute_point(point: Point) -> Result<(), MacError> {
    validate_bounded_point(point, MAX_ABSOLUTE_POINTER_COORDINATE)
}

pub fn validate_relative_delta(delta: Point) -> Result<(), MacError> {
    validate_bounded_point(delta, MAX_RELATIVE_POINTER_DELTA)
}

fn validate_bounded_point(point: Point, limit: f64) -> Result<(), MacError> {
    if !point.is_finite() {
        return Err(MacError::InvalidPoint);
    }
    if point.x.abs() > limit || point.y.abs() > limit {
        return Err(MacError::PointOutOfRange);
    }
    Ok(())
}

const fn should_post_held_state_event(is_down: bool, is_tracked: bool) -> bool {
    is_down || is_tracked
}

const fn mouse_button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use monhop_core::{LogicalRect, LogicalSize, MouseButton, Point};

    use super::{
        CursorState, MacError, ScrollResidual, clear_cursor_after_button_recovery,
        should_post_held_state_event,
    };

    fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect {
            origin: Point::new(x, y),
            size: LogicalSize::new(width, height),
        }
    }

    fn cursor(bounds: &[LogicalRect]) -> CursorState {
        CursorState::new(bounds.iter().copied()).unwrap()
    }

    fn standard_cursor() -> CursorState {
        cursor(&[rect(0.0, 0.0, 1_000.0, 1_000.0)])
    }

    #[test]
    fn fractional_scroll_accumulates_on_both_axes_and_reverses() {
        let mut scroll = ScrollResidual::default();
        let mut posted = Vec::new();
        for _ in 0..4 {
            scroll
                .post(0.25, -0.5, |x, y| {
                    posted.push((x, y));
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(posted, [(0, -1), (1, -1)]);
        scroll
            .post(0.75, -0.75, |_, _| panic!("fraction must be retained"))
            .unwrap();
        scroll
            .post(-0.5, 0.5, |_, _| panic!("opposite fractions must cancel"))
            .unwrap();
        scroll
            .post(-1.25, 1.25, |x, y| {
                assert_eq!((x, y), (-1, 1));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn failed_or_invalid_scroll_does_not_consume_prior_fractions() {
        let mut scroll = ScrollResidual::default();
        scroll
            .post(0.25, 0.25, |_, _| panic!("fraction must be retained"))
            .unwrap();
        assert_eq!(
            scroll.post(1.0, 1.0, |_, _| Err(MacError::NativeEventCreationFailed)),
            Err(MacError::NativeEventCreationFailed)
        );
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from(i32::MAX) + 1.0,
            f64::from(i32::MIN) - 1.0,
        ] {
            assert_eq!(
                scroll.post(0.5, invalid, |_, _| panic!("invalid event must not post")),
                Err(MacError::ScrollOutOfRange)
            );
        }
        scroll
            .post(0.75, 0.75, |x, y| {
                assert_eq!((x, y), (1, 1));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn suppresses_untracked_release_events() {
        assert!(should_post_held_state_event(true, false));
        assert!(should_post_held_state_event(false, true));
        assert!(!should_post_held_state_event(false, false));
    }

    #[test]
    fn relative_motion_accumulates_without_reading_the_os_cursor() {
        let mut cursor = standard_cursor();
        cursor
            .post_absolute(Point::new(100.0, 200.0), |_| Ok(()))
            .unwrap();
        cursor
            .post_relative(Point::new(3.0, -4.0), |point| {
                assert_eq!(point, Point::new(103.0, 196.0));
                Ok(())
            })
            .unwrap();
        cursor
            .post_relative(Point::new(-1.0, 2.0), |point| {
                assert_eq!(point, Point::new(102.0, 198.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn absolute_moves_reanchor_and_buttons_use_the_last_submitted_point() {
        let mut cursor = standard_cursor();
        cursor
            .post_absolute(Point::new(10.0, 20.0), |_| Ok(()))
            .unwrap();
        cursor
            .post_absolute(Point::new(30.0, 40.0), |_| Ok(()))
            .unwrap();
        cursor
            .post_relative(Point::new(2.0, 3.0), |point| {
                assert_eq!(point, Point::new(32.0, 43.0));
                Ok(())
            })
            .unwrap();
        cursor
            .post_button(|point| {
                assert_eq!(point, Point::new(32.0, 43.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn cursor_rejects_invalid_or_empty_display_bounds_and_nonfinite_input() {
        assert!(matches!(
            CursorState::new([]),
            Err(MacError::PointerBoundsUnavailable)
        ));
        for invalid in [
            rect(0.0, 0.0, 0.0, 1.0),
            rect(0.0, 0.0, -1.0, 1.0),
            rect(0.0, 0.0, f64::NAN, 1.0),
        ] {
            assert!(matches!(
                CursorState::new([invalid]),
                Err(MacError::InvalidDisplayData)
            ));
        }

        let mut cursor = standard_cursor();
        assert_eq!(
            cursor.post_relative(Point::new(1.0, 1.0), |_| Ok(())),
            Err(MacError::CursorPositionUnknown)
        );
        assert_eq!(
            cursor.post_absolute(Point::new(f64::NAN, 0.0), |_| Ok(())),
            Err(MacError::InvalidPoint)
        );
        assert_eq!(
            cursor.post_absolute(Point::new(1_000_000_001.0, 0.0), |_| Ok(())),
            Err(MacError::PointOutOfRange)
        );
        assert_eq!(
            cursor.post_button(|_| Ok(())),
            Err(MacError::CursorPositionUnknown)
        );
    }

    #[test]
    fn anchor_is_unavailable_before_motion_and_after_recovery() {
        let mut cursor = standard_cursor();
        assert_eq!(cursor.anchor(), Err(MacError::CursorPositionUnknown));
        let point = Point::new(20.0, 30.0);
        cursor.post_absolute(point, |_| Ok(())).unwrap();
        assert_eq!(cursor.anchor(), Ok(point));
        cursor.clear();
        assert_eq!(cursor.anchor(), Err(MacError::CursorPositionUnknown));
    }

    #[test]
    fn absolute_and_relative_moves_clamp_to_the_display_union() {
        let mut cursor = cursor(&[rect(0.0, 0.0, 100.0, 100.0)]);
        let upper_x = 100.0_f64.next_down();
        let upper_y = 100.0_f64.next_down();
        cursor
            .post_absolute(Point::new(200.0, 150.0), |point| {
                assert_eq!(point, Point::new(upper_x, upper_y));
                Ok(())
            })
            .unwrap();
        cursor
            .post_relative(Point::new(-1.0, -1.0), |point| {
                assert_eq!(point, Point::new(upper_x - 1.0, upper_y - 1.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn edge_overshoot_keeps_the_virtual_anchor_at_the_clamped_point() {
        let mut cursor = cursor(&[rect(0.0, 0.0, 100.0, 100.0)]);
        let upper_x = 100.0_f64.next_down();
        cursor
            .post_absolute(Point::new(99.0, 50.0), |_| Ok(()))
            .unwrap();
        cursor
            .post_relative(Point::new(2.0, 0.0), |point| {
                assert_eq!(point, Point::new(upper_x, 50.0));
                Ok(())
            })
            .unwrap();
        cursor
            .post_relative(Point::new(-1.0, 0.0), |point| {
                assert_eq!(point, Point::new(upper_x - 1.0, 50.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn negative_origin_bounds_are_preserved() {
        let mut cursor = cursor(&[rect(-100.0, -100.0, 100.0, 100.0)]);
        cursor
            .post_absolute(Point::new(-50.0, -50.0), |_| Ok(()))
            .unwrap();
        cursor
            .post_relative(Point::new(-75.0, -75.0), |point| {
                assert_eq!(point, Point::new(-100.0, -100.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn display_gaps_clamp_to_a_member_rectangle_not_a_bounding_box() {
        let mut cursor = cursor(&[rect(0.0, 0.0, 100.0, 100.0), rect(200.0, 0.0, 100.0, 100.0)]);
        cursor
            .post_absolute(Point::new(125.0, 50.0), |point| {
                assert_eq!(point, Point::new(100.0_f64.next_down(), 50.0));
                Ok(())
            })
            .unwrap();
        cursor
            .post_absolute(Point::new(175.0, 50.0), |point| {
                assert_eq!(point, Point::new(200.0, 50.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn failed_posts_leave_the_previous_anchor_for_retry() {
        let mut cursor = standard_cursor();
        cursor
            .post_absolute(Point::new(10.0, 20.0), |_| Ok(()))
            .unwrap();
        assert_eq!(
            cursor.post_absolute(Point::new(30.0, 40.0), |_| Err(
                MacError::NativeEventCreationFailed
            )),
            Err(MacError::NativeEventCreationFailed)
        );
        assert_eq!(
            cursor.post_relative(Point::new(1.0, 2.0), |_| Err(
                MacError::NativeEventCreationFailed
            )),
            Err(MacError::NativeEventCreationFailed)
        );
        cursor
            .post_button(|point| {
                assert_eq!(point, Point::new(10.0, 20.0));
                Ok(())
            })
            .unwrap();
        cursor
            .post_relative(Point::new(1.0, 2.0), |point| {
                assert_eq!(point, Point::new(11.0, 22.0));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn recovery_clears_the_anchor_only_after_button_cleanup_completes() {
        let mut cursor = standard_cursor();
        cursor
            .post_absolute(Point::new(10.0, 20.0), |_| Ok(()))
            .unwrap();
        let mut buttons = BTreeSet::from([MouseButton::Left]);
        clear_cursor_after_button_recovery(&buttons, &mut cursor);
        assert!(cursor.submitted.is_some());

        buttons.clear();
        clear_cursor_after_button_recovery(&buttons, &mut cursor);
        assert_eq!(cursor.submitted, None);
    }
}
