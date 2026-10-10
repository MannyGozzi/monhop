//! Native destination adapters. Construction is explicit and never enables capture.

use crate::{
    session::check_read_displays,
    session_actor::{EnvironmentFailure, WatchedDestination, WatchedEnvironment},
    session_receiver::{DestinationAction, DestinationFailure, InputDestination},
};
use monhop_core::{
    DeviceId, DisplayId, FloorPeer, GesturePhase, InjectionPermit, MAX_GROUP_PEERS, Platform,
    Point, PointerGesture, RevocationSignal, TakeBackGate,
    clicks::{ClickLanding, FALLBACK_DOUBLE_CLICK_INTERVAL},
    pointer_mark::{PageDirection, PageTurn},
};
use monhop_protocol::{DisplayDescription, DisplayTopology};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

/// Scroll wire values use signed wheel detents: positive right and positive up.
/// A macOS pixel-scroll detent is forty points, with residual fractions retained by the injector.
pub const MAC_POINTS_PER_DETENT: f64 = 40.0;
pub const WINDOWS_UNITS_PER_DETENT: f64 = 120.0;

/// The last native destination check that failed in this process, as a small code for diagnostics.
static LAST_DESTINATION_STEP: AtomicU8 = AtomicU8::new(0);

pub fn last_destination_step() -> u8 {
    LAST_DESTINATION_STEP.load(Ordering::Acquire)
}

/// Off until the app applies the saved switch, so nothing autoscrolls before it decides.
static AUTOSCROLL: AtomicBool = AtomicBool::new(false);
static AUTOSCROLL_MARKER: MarkSink<AutoscrollMarker> = MarkSink::new();
static PAGE_TURN_MARK: MarkSink<PageTurn> = MarkSink::new();

/// A pointer mark's one app-registered sink. It runs on the injection thread, so it must hand the
/// change to its own thread and return without waiting.
struct MarkSink<T>(OnceLock<Box<dyn Fn(T) + Send + Sync>>);

impl<T> MarkSink<T> {
    const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// A second registration is refused.
    fn set(&self, sink: impl Fn(T) + Send + Sync + 'static) -> bool {
        self.0.set(Box::new(sink)).is_ok()
    }

    fn signal(&self, mark: T) {
        if let Some(sink) = self.0.get() {
            sink(mark);
        }
    }
}

/// Where the Mac's autoscroll origin marker goes: sent as an episode starts and as it ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoscrollMarker {
    Show(Point),
    Hide,
}

/// Whether a Windows mouse's middle button autoscrolls this Mac; off also ends a running episode.
pub fn set_autoscroll_enabled(enabled: bool) {
    AUTOSCROLL.store(enabled, Ordering::Release);
}

/// Registers the autoscroll marker's sink; see `MarkSink`.
pub fn set_autoscroll_marker(sink: impl Fn(AutoscrollMarker) + Send + Sync + 'static) -> bool {
    AUTOSCROLL_MARKER.set(sink)
}

/// Registers the sink told about each page turn a swipe from another computer sends; see `MarkSink`.
pub fn set_page_turn_mark(sink: impl Fn(PageTurn) + Send + Sync + 'static) -> bool {
    PAGE_TURN_MARK.set(sink)
}

/// The platform of the peer in each floor slot, so a destination knows which computer drives it.
#[derive(Clone, Default)]
pub(crate) struct SourcePlatforms(Arc<[AtomicU8; MAX_GROUP_PEERS]>);

impl SourcePlatforms {
    pub(crate) fn sole(platform: Platform) -> Self {
        let sources = Self::default();
        sources.set(FloorPeer::SOLE, Some(platform));
        sources
    }

    pub(crate) fn set(&self, slot: FloorPeer, platform: Option<Platform>) {
        if let Some(cell) = self.cell(slot) {
            let code = match platform {
                None => 0,
                Some(Platform::Windows) => 1,
                Some(Platform::MacOs) => 2,
            };
            cell.store(code, Ordering::Release);
        }
    }

    /// None for an unknown or empty slot, which never starts autoscroll.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn of(&self, slot: FloorPeer) -> Option<Platform> {
        match self.cell(slot)?.load(Ordering::Acquire) {
            1 => Some(Platform::Windows),
            2 => Some(Platform::MacOs),
            _ => None,
        }
    }

    fn cell(&self, slot: FloorPeer) -> Option<&AtomicU8> {
        crate::session_receiver_set::index(slot).map(|index| &self.0[index])
    }
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

/// Whether a program confines the cursor, as a game's mouse-look does; never where no query exists.
pub(crate) fn pointer_confined() -> bool {
    #[cfg(windows)]
    {
        monhop_platform_windows::native_capture::pointer_confined()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// This computer's double-click interval, which numbers the clicks it forwards.
pub(crate) fn double_click_interval() -> std::time::Duration {
    #[cfg(target_os = "macos")]
    let interval = monhop_platform_macos::native_capture::double_click_interval();
    #[cfg(windows)]
    let interval = monhop_platform_windows::native_capture::double_click_interval();
    #[cfg(not(any(windows, target_os = "macos")))]
    let interval = None;
    interval.unwrap_or(FALLBACK_DOUBLE_CLICK_INTERVAL)
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
    native: NativeInput,
    _ownership: InjectionPermit,
    sequencer: Sequencer,
    #[cfg(target_os = "macos")]
    autoscroll: MacAutoscroll,
}

/// A Windows mouse's middle button autoscrolls this Mac while the switch is on.
#[cfg(target_os = "macos")]
struct MacAutoscroll {
    machine: crate::session_autoscroll::Autoscroll,
    sources: SourcePlatforms,
    /// The marker the app was last told about.
    shown: Option<Point>,
}

#[cfg(target_os = "macos")]
impl MacAutoscroll {
    /// The controller when a middle press may start autoscroll: a Windows source holds the floor.
    fn starts(&self, sequencer: &Sequencer) -> Option<u64> {
        let source = sequencer.admission.gate.floor().snapshot().peer;
        (AUTOSCROLL.load(Ordering::Acquire) && self.sources.of(source) == Some(Platform::Windows))
            .then(|| sequencer.controller())
            .flatten()
    }

    /// Tells the app about changes only, so each episode signals once each way.
    fn signal_marker(&mut self) {
        let wanted = self.machine.marker();
        if wanted == self.shown {
            return;
        }
        self.shown = wanted;
        AUTOSCROLL_MARKER.signal(wanted.map_or(AutoscrollMarker::Hide, AutoscrollMarker::Show));
    }
}

/// What must still hold for injection: no revocation, the ordinary desktop on Windows or the
/// Accessibility permission on macOS, and the displays the session started with.
pub(crate) struct NativeEnvironment {
    device: DeviceId,
    displays: DisplayTopology,
    revocation: RevocationSignal,
    /// The macOS permission query is a TCC round trip, asked at most every PERMISSION_RECHECK
    /// while displays are still compared on every check.
    #[cfg(target_os = "macos")]
    permission_checked: Option<std::time::Instant>,
}

/// The platform injector and what its posts need.
struct NativeInput {
    #[cfg(windows)]
    injector: monhop_platform_windows::Injector,
    #[cfg(windows)]
    desktop: monhop_platform_windows::VirtualDesktop,
    #[cfg(windows)]
    scroll: WheelResidual,
    #[cfg(target_os = "macos")]
    injector: monhop_platform_macos::MacInjector,
}

impl NativeDestination {
    pub(crate) fn new_after_local_enable(
        device: DeviceId,
        displays: DisplayTopology,
        ownership: InjectionPermit,
        revocation: RevocationSignal,
        gate: TakeBackGate,
        sources: SourcePlatforms,
    ) -> Result<Self, DestinationFailure> {
        if revocation.is_stopping() {
            return Err(note_step(1));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = sources;
        #[cfg(windows)]
        let (left, top, right, bottom) = displays.displays().iter().fold(
            (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ),
            |(l, t, r, b), d| {
                let bounds = d.logical_bounds();
                (
                    l.min(bounds.origin.x),
                    t.min(bounds.origin.y),
                    r.max(bounds.max_x()),
                    b.max(bounds.max_y()),
                )
            },
        );
        let result = Self {
            _ownership: ownership,
            sequencer: Sequencer::new(gate, revocation, &displays),
            device,
            displays,
            native: NativeInput {
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
            },
            #[cfg(target_os = "macos")]
            autoscroll: MacAutoscroll {
                machine: crate::session_autoscroll::Autoscroll::new(),
                sources,
                shown: None,
            },
        };
        Ok(result)
    }
}

/// A destination that goes away mid-episode still takes its marker down.
#[cfg(target_os = "macos")]
impl Drop for NativeDestination {
    fn drop(&mut self) {
        self.autoscroll.machine.stop(std::time::Instant::now());
        self.autoscroll.signal_marker();
    }
}

impl WatchedDestination for NativeDestination {
    type Environment = NativeEnvironment;
    fn environment(&mut self) -> NativeEnvironment {
        NativeEnvironment {
            device: self.device,
            displays: self.displays.clone(),
            revocation: self.sequencer.revocation.clone(),
            #[cfg(target_os = "macos")]
            permission_checked: None,
        }
    }
    /// Releases again any key or button whose release Windows silently dropped.
    #[cfg(windows)]
    fn tick(&mut self) {
        self.native.injector.tick(std::time::Instant::now());
    }
    /// Posts the autoscroll owed since the last tick, behind the same gates as any other input.
    #[cfg(target_os = "macos")]
    fn tick(&mut self) {
        let now = std::time::Instant::now();
        let (controller, enabled) = (
            self.sequencer.controller(),
            AUTOSCROLL.load(Ordering::Acquire),
        );
        if let Some(step) = self.autoscroll.machine.tick(now, controller, enabled) {
            let native = &mut self.native;
            if self
                .sequencer
                .apply(step.scroll, |post| native.post_scroll_at(post, step.at))
                .is_err()
            {
                self.autoscroll.machine.stop(now);
            }
        }
        self.autoscroll.signal_marker();
    }
}

impl WatchedEnvironment for NativeEnvironment {
    fn validate(&mut self) -> Result<(), EnvironmentFailure> {
        if self.revocation.is_stopping() {
            return Err(note_step(5).into());
        }
        #[cfg(windows)]
        if !monhop_platform_windows::desktop_state::ordinary_desktop_is_active() {
            return Err(note_step(6).into());
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
                return Err(note_step(6).into());
            }
            self.permission_checked = Some(std::time::Instant::now());
        }
        if check_read_displays(&self.displays, current_displays(self.device)).is_err() {
            note_step(8);
            return Err(EnvironmentFailure::DisplaysChanged);
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
const PERMISSION_RECHECK: std::time::Duration = std::time::Duration::from_millis(250);

impl InputDestination for NativeDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let native = &mut self.native;
        let displays = &self.displays;
        let mut turned = None;
        let mut post = |post: DestinationAction| {
            native.post(displays, post)?;
            if let DestinationAction::System(gesture) = post {
                turned = PageDirection::of(gesture);
            }
            Ok(())
        };
        #[cfg(target_os = "macos")]
        let applied = {
            let sequencer = &mut self.sequencer;
            let starts = self.autoscroll.starts(sequencer);
            let routed = self.autoscroll.machine.apply(
                action,
                starts,
                std::time::Instant::now(),
                |action| sequencer.apply(action, &mut post),
            );
            self.autoscroll.signal_marker();
            routed
        };
        #[cfg(not(target_os = "macos"))]
        let applied = self.sequencer.apply(action, &mut post);
        if let Some(direction) = turned
            && let Some(at) = current_pointer_position()
        {
            PAGE_TURN_MARK.signal(PageTurn { direction, at });
        }
        applied
    }
}

/// Authorization, admission, then where a click lands: everything decided before a native post.
struct Sequencer {
    revocation: RevocationSignal,
    admission: InjectionLedger,
    landing: ClickLanding,
}

impl Sequencer {
    fn new(gate: TakeBackGate, revocation: RevocationSignal, displays: &DisplayTopology) -> Self {
        Self {
            revocation,
            admission: InjectionLedger::new(gate, PINCH_HOLDS_MODIFIER),
            landing: ClickLanding::new(
                displays
                    .displays()
                    .iter()
                    .map(DisplayDescription::logical_bounds),
            ),
        }
    }

    /// Posts any snap, the action, then any move back, one native call each. The action is done
    /// once its own post succeeds, and control ending in between drops the posts that follow.
    fn apply(
        &mut self,
        action: DestinationAction,
        mut post: impl FnMut(DestinationAction) -> Result<(), DestinationFailure>,
    ) -> Result<(), DestinationFailure> {
        require_action_authorization(&self.revocation, action)?;
        if !self.admission.begin(action) {
            return Ok(());
        }
        let (before, own, after) = match action {
            DestinationAction::MoveTo(source) => (
                None,
                DestinationAction::MoveTo(self.landing.move_target(source)),
                None,
            ),
            DestinationAction::Button {
                button,
                pressed: true,
                click_count,
            } => (self.landing.press_target(button, click_count), action, None),
            DestinationAction::Button {
                button,
                pressed: false,
                ..
            } => (None, action, self.landing.release_target(button)),
            _ => (None, action, None),
        };
        if let Some(snap) = before {
            post(DestinationAction::MoveTo(snap))?;
            if !self.still_in_control() {
                self.admission.refuse(action);
                return Ok(());
            }
        }
        post(own)?;
        self.admission.complete(action);
        match action {
            DestinationAction::MoveTo(source) => self.landing.moved(source),
            DestinationAction::Button {
                button,
                pressed: true,
                click_count,
            } => self.landing.pressed(button, click_count),
            DestinationAction::Button {
                button,
                pressed: false,
                ..
            } => self.landing.released(button),
            DestinationAction::ReleaseAll => self.landing.clear(),
            DestinationAction::Key { .. }
            | DestinationAction::Scroll { .. }
            | DestinationAction::Gesture(_)
            | DestinationAction::System(_)
            | DestinationAction::EndGestures => {}
        }
        match after {
            Some(source) if self.still_in_control() => post(DestinationAction::MoveTo(source)),
            _ => Ok(()),
        }
    }

    /// False once a take-back or a revocation has ended control since the action's admission.
    fn still_in_control(&self) -> bool {
        !self.revocation.is_stopping() && self.admission.gate.admits_injection()
    }

    /// The floor generation now in control, which a take-back or a new claim changes.
    #[cfg(any(target_os = "macos", test))]
    fn controller(&self) -> Option<u64> {
        self.still_in_control()
            .then(|| self.admission.gate.floor().snapshot().generation)
    }
}

impl NativeInput {
    fn post(
        &mut self,
        #[cfg_attr(not(windows), expect(unused_variables))] displays: &DisplayTopology,
        action: DestinationAction,
    ) -> Result<(), DestinationFailure> {
        #[cfg(windows)]
        {
            use monhop_platform_windows::InjectionOperation as Op;
            let operation = match action {
                DestinationAction::MoveTo(point) => {
                    if !displays
                        .displays()
                        .iter()
                        .any(|d| d.logical_bounds().contains_half_open(point))
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
                    button, pressed, ..
                } => Op::Button { button, pressed },
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
                DestinationAction::Gesture(gesture) => {
                    return self
                        .injector
                        .gesture(gesture)
                        .map_err(|_| DestinationFailure);
                }
                DestinationAction::System(gesture) => {
                    return self
                        .injector
                        .system_gesture(gesture)
                        .map_err(|_| DestinationFailure);
                }
                DestinationAction::EndGestures => {
                    return self.injector.end_gestures().map_err(|_| DestinationFailure);
                }
                DestinationAction::ReleaseAll => {
                    self.scroll = WheelResidual::default();
                    let ended = self.injector.end_gestures();
                    return self
                        .injector
                        .release_all()
                        .and(ended)
                        .map_err(|_| DestinationFailure);
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
                DestinationAction::Gesture(_) => 15,
                DestinationAction::System(_) => 16,
                DestinationAction::EndGestures => 17,
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
                } => {
                    let (horizontal, vertical) = mac_scroll_pixels(horizontal, vertical);
                    self.injector.scroll(horizontal, vertical)
                }
                DestinationAction::Gesture(gesture) => self.injector.gesture(gesture),
                DestinationAction::System(gesture) => self.injector.system_gesture(gesture),
                DestinationAction::EndGestures => self.injector.end_gestures(),
                DestinationAction::ReleaseAll => {
                    let ended = self.injector.end_gestures();
                    self.injector.release_all().and(ended)
                }
            }
            .map_err(|_| note_step(step))
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = action;
            Err(DestinationFailure)
        }
    }

    /// An autoscroll step's scroll, its event placed at the origin rather than the pointer.
    #[cfg(target_os = "macos")]
    fn post_scroll_at(
        &mut self,
        action: DestinationAction,
        at: Point,
    ) -> Result<(), DestinationFailure> {
        let DestinationAction::Scroll {
            horizontal,
            vertical,
        } = action
        else {
            return Err(note_step(13));
        };
        let (horizontal, vertical) = mac_scroll_pixels(horizontal, vertical);
        self.injector
            .scroll_at(horizontal, vertical, at)
            .map_err(|_| note_step(13))
    }
}

/// Wire detents as macOS pixel-scroll deltas, whose horizontal axis runs the other way.
#[cfg(target_os = "macos")]
fn mac_scroll_pixels(horizontal: f64, vertical: f64) -> (f64, f64) {
    (
        -horizontal * MAC_POINTS_PER_DETENT,
        vertical * MAC_POINTS_PER_DETENT,
    )
}

/// Only the Windows injector keeps a key down through a pinch (its Ctrl); the Mac's zoom steps press
/// and release at once.
const PINCH_HOLDS_MODIFIER: bool = cfg!(windows);

/// Tracks native transitions, not wire repeats, so the gate includes posts still in flight.
struct InjectionLedger {
    gate: TakeBackGate,
    keys: [bool; 256],
    buttons: [bool; 5],
    /// A pinch may hold a synthetic modifier from its Began until it ends.
    magnify: bool,
    pinch_holds_modifier: bool,
}

impl InjectionLedger {
    fn new(gate: TakeBackGate, pinch_holds_modifier: bool) -> Self {
        Self {
            gate,
            keys: [false; 256],
            buttons: [false; 5],
            magnify: false,
            pinch_holds_modifier,
        }
    }

    fn begin(&self, action: DestinationAction) -> bool {
        if action.is_release() {
            return true;
        }
        if self.is_new_press(action) {
            self.gate.note_injected_press();
        }
        if !self.gate.admits_injection() {
            self.refuse(action);
            return false;
        }
        true
    }

    /// Un-counts a press [`Self::begin`] counted that will not be posted.
    fn refuse(&self, action: DestinationAction) {
        if self.is_new_press(action) {
            self.gate.note_injected_up();
        }
    }

    fn is_new_press(&self, action: DestinationAction) -> bool {
        match action {
            DestinationAction::Key {
                usage,
                pressed: true,
            } => !self.keys[usize::from(usage.0)],
            DestinationAction::Button {
                button,
                pressed: true,
                ..
            } => !self.buttons[button.index()],
            DestinationAction::Gesture(PointerGesture::Magnify {
                phase: GesturePhase::Began,
                ..
            }) => self.pinch_holds_modifier && !self.magnify,
            DestinationAction::System(_) => true,
            _ => false,
        }
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
            DestinationAction::Gesture(PointerGesture::Magnify { phase, .. }) => {
                if phase == GesturePhase::Began {
                    self.magnify = self.pinch_holds_modifier;
                } else if phase.is_terminal() {
                    self.end_magnify();
                }
            }
            // A chord releases every key it pressed before its post returns.
            DestinationAction::System(_) => self.gate.note_injected_up(),
            DestinationAction::EndGestures => self.end_magnify(),
            DestinationAction::ReleaseAll => {
                self.keys.fill(false);
                self.buttons.fill(false);
                self.magnify = false;
                self.gate.note_injected_released();
            }
            _ => {}
        }
    }

    fn end_magnify(&mut self) {
        if std::mem::take(&mut self.magnify) {
            self.gate.note_injected_up();
        }
    }
}

fn require_action_authorization(
    revocation: &RevocationSignal,
    action: DestinationAction,
) -> Result<(), DestinationFailure> {
    // A stop or revocation forbids new input, but never ledger-owned release cleanup.
    if revocation.is_stopping() && !action.is_release() {
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
    fn each_floor_slot_names_its_source_platform_and_an_unknown_slot_names_none() {
        let sources = SourcePlatforms::sole(Platform::Windows);
        assert_eq!(sources.of(FloorPeer::SOLE), Some(Platform::Windows));
        assert_eq!(sources.of(FloorPeer::NONE), None);
        let second = FloorPeer::slot(2).unwrap();
        assert_eq!(sources.of(second), None);
        sources.clone().set(second, Some(Platform::MacOs));
        assert_eq!(
            sources.of(second),
            Some(Platform::MacOs),
            "clones share slots"
        );
        sources.set(FloorPeer::SOLE, None);
        assert_eq!(sources.of(FloorPeer::SOLE), None);
        sources.set(FloorPeer::NONE, Some(Platform::Windows));
        assert_eq!(sources.of(FloorPeer::NONE), None);
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
                click_count: 2,
            },
            DestinationAction::Scroll {
                horizontal: 0.5,
                vertical: -0.5,
            },
            DestinationAction::Gesture(monhop_core::PointerGesture::Magnify {
                phase: monhop_core::GesturePhase::Began,
                delta: 0.0,
            }),
            DestinationAction::System(monhop_core::SystemGesture::Overview),
        ] {
            assert_eq!(
                require_action_authorization(&signal, action),
                Err(DestinationFailure)
            );
        }
        for release in [
            DestinationAction::EndGestures,
            DestinationAction::ReleaseAll,
        ] {
            assert_eq!(require_action_authorization(&signal, release), Ok(()));
        }
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
        let mut ledger = InjectionLedger::new(gate.clone(), true);
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
        let mut ledger = InjectionLedger::new(gate.clone(), true);
        assert!(!ledger.begin(DestinationAction::Button {
            button: monhop_core::MouseButton::Left,
            pressed: true,
            click_count: 1,
        }));
        assert!(!gate.injected_held());
        assert!(!ledger.begin(DestinationAction::System(
            monhop_core::SystemGesture::Search
        )));
        gate.note_injected_press();
        assert!(ledger.begin(DestinationAction::EndGestures));
        ledger.complete(DestinationAction::EndGestures);
        assert!(
            gate.injected_held(),
            "ending gestures releases no key or button"
        );
        assert!(ledger.begin(DestinationAction::ReleaseAll));
        ledger.complete(DestinationAction::ReleaseAll);
        assert!(!gate.injected_held());
    }

    #[test]
    fn a_pinch_counts_as_held_until_it_ends_and_a_chord_only_while_posting() {
        use GesturePhase::{Began, Changed, Ended};
        let floor = SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        let mut ledger = InjectionLedger::new(gate.clone(), true);
        let pinch =
            |phase| DestinationAction::Gesture(PointerGesture::Magnify { phase, delta: 0.0 });
        for (phase, held) in [
            (Began, true),
            (Changed, true),
            (Began, true),
            (Ended, false),
        ] {
            assert!(ledger.begin(pinch(phase)));
            ledger.complete(pinch(phase));
            assert_eq!(gate.injected_held(), held, "{phase:?}");
        }
        assert!(ledger.begin(pinch(Began)));
        ledger.complete(pinch(Began));
        ledger.complete(DestinationAction::EndGestures);
        assert!(
            !gate.injected_held(),
            "ending gestures ends the pinch's modifier"
        );
        let chord = DestinationAction::System(monhop_core::SystemGesture::Overview);
        assert!(ledger.begin(chord));
        assert!(gate.injected_held(), "a chord counts while it posts");
        ledger.complete(chord);
        assert!(!gate.injected_held());

        let mut mac = InjectionLedger::new(gate.clone(), false);
        assert!(mac.begin(pinch(Began)));
        mac.complete(pinch(Began));
        assert!(
            !gate.injected_held(),
            "a pinch that holds no key counts nothing"
        );
        mac.complete(pinch(Ended));
        assert!(!gate.injected_held());
    }
}

#[cfg(test)]
mod landing_tests {
    use super::*;
    use monhop_core::{FloorState, MouseButton, SharedFloor};

    #[derive(Debug, PartialEq)]
    enum Posted {
        Move(f64, f64),
        Button(MouseButton, bool, u8),
        ReleaseAll,
    }

    fn posted(action: DestinationAction) -> Posted {
        match action {
            DestinationAction::MoveTo(point) => Posted::Move(point.x, point.y),
            DestinationAction::Button {
                button,
                pressed,
                click_count,
            } => Posted::Button(button, pressed, click_count),
            DestinationAction::ReleaseAll => Posted::ReleaseAll,
            DestinationAction::Key { .. }
            | DestinationAction::Scroll { .. }
            | DestinationAction::Gesture(_)
            | DestinationAction::System(_)
            | DestinationAction::EndGestures => {
                unreachable!("not posted here")
            }
        }
    }

    fn display(id: u64, x: f64) -> DisplayDescription {
        DisplayDescription {
            id: DisplayId(id),
            name: format!("Display {id}"),
            native_width: 1920,
            native_height: 1080,
            logical_origin: Point::new(x, 0.0),
            logical_size: Point::new(1920.0, 1080.0),
            scale_factor: 1.0,
            is_primary: id == 1,
            monitor: None,
        }
    }

    /// Two 1920x1080 displays side by side, admitting injection.
    fn sequencer() -> (Sequencer, TakeBackGate) {
        let floor = SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        let displays = DisplayTopology::new(vec![display(1, 0.0), display(2, 1920.0)]).unwrap();
        let sequencer = Sequencer::new(gate.clone(), RevocationSignal::default(), &displays);
        (sequencer, gate)
    }

    /// Applies `action`, answering each post with `outcome`; returns the result and posts tried.
    fn run(
        sequencer: &mut Sequencer,
        action: DestinationAction,
        mut outcome: impl FnMut(usize) -> Result<(), DestinationFailure>,
    ) -> (Result<(), DestinationFailure>, Vec<Posted>) {
        let mut posts = Vec::new();
        let result = sequencer.apply(action, |post| {
            posts.push(posted(post));
            outcome(posts.len() - 1)
        });
        (result, posts)
    }

    /// Applies `action`, failing the post at index `fail`.
    fn apply(
        sequencer: &mut Sequencer,
        action: DestinationAction,
        fail: Option<usize>,
    ) -> (Result<(), DestinationFailure>, Vec<Posted>) {
        run(sequencer, action, |index| {
            if fail == Some(index) {
                Err(DestinationFailure)
            } else {
                Ok(())
            }
        })
    }

    fn posts(sequencer: &mut Sequencer, action: DestinationAction) -> Vec<Posted> {
        let (result, posts) = apply(sequencer, action, None);
        assert_eq!(result, Ok(()));
        posts
    }

    fn move_to(x: f64, y: f64) -> DestinationAction {
        DestinationAction::MoveTo(Point::new(x, y))
    }

    fn left(pressed: bool, click_count: u8) -> DestinationAction {
        DestinationAction::Button {
            button: MouseButton::Left,
            pressed,
            click_count,
        }
    }

    /// A single click at (100, 200), then the source's jittered second press at (106, 195).
    fn snapped_double() -> (Sequencer, TakeBackGate) {
        let (mut sequencer, gate) = sequencer();
        posts(&mut sequencer, move_to(100.0, 200.0));
        posts(&mut sequencer, left(true, 1));
        posts(&mut sequencer, left(false, 1));
        posts(&mut sequencer, move_to(106.0, 195.0));
        assert_eq!(
            posts(&mut sequencer, left(true, 2)),
            [
                Posted::Move(100.0, 200.0),
                Posted::Button(MouseButton::Left, true, 2)
            ],
            "the snap, then the press, each its own post"
        );
        (sequencer, gate)
    }

    #[test]
    fn a_snapped_drag_follows_the_hand_then_moves_back_after_its_release() {
        let (mut sequencer, gate) = snapped_double();
        assert_eq!(
            posts(&mut sequencer, move_to(109.0, 195.0)),
            [Posted::Move(103.0, 200.0)],
            "a 3-unit move moves the cursor 3 units"
        );
        assert_eq!(
            posts(&mut sequencer, left(false, 2)),
            [
                Posted::Button(MouseButton::Left, false, 2),
                Posted::Move(109.0, 195.0)
            ],
            "released at the shifted point, then back to the source's own point"
        );
        assert!(!gate.injected_held());
        assert_eq!(
            posts(&mut sequencer, move_to(110.0, 195.0)),
            [Posted::Move(110.0, 195.0)]
        );
    }

    #[test]
    fn a_failed_move_back_leaves_the_release_recorded() {
        let (mut sequencer, gate) = snapped_double();
        let (result, tried) = apply(&mut sequencer, left(false, 2), Some(1));
        assert_eq!(result, Err(DestinationFailure));
        assert_eq!(
            tried,
            [
                Posted::Button(MouseButton::Left, false, 2),
                Posted::Move(106.0, 195.0)
            ]
        );
        assert!(!sequencer.admission.buttons[MouseButton::Left.index()]);
        assert!(!gate.injected_held(), "ReleaseAll owes no second button-up");
        assert_eq!(
            posts(&mut sequencer, move_to(107.0, 195.0)),
            [Posted::Move(107.0, 195.0)],
            "the released snap no longer shifts"
        );
    }

    #[test]
    fn a_failed_snap_posts_no_press() {
        let (mut sequencer, _gate) = sequencer();
        posts(&mut sequencer, move_to(100.0, 200.0));
        posts(&mut sequencer, left(true, 1));
        posts(&mut sequencer, left(false, 1));
        posts(&mut sequencer, move_to(104.0, 200.0));
        let (result, tried) = apply(&mut sequencer, left(true, 2), Some(0));
        assert_eq!(result, Err(DestinationFailure));
        assert_eq!(tried, [Posted::Move(100.0, 200.0)]);
        assert!(!sequencer.admission.buttons[MouseButton::Left.index()]);
    }

    #[test]
    fn a_refused_press_posts_no_snap_and_release_all_forgets_the_landing() {
        let (mut sequencer, gate) = snapped_double();
        assert_eq!(
            posts(&mut sequencer, DestinationAction::ReleaseAll),
            [Posted::ReleaseAll]
        );
        assert_eq!(
            posts(&mut sequencer, move_to(104.0, 200.0)),
            [Posted::Move(104.0, 200.0)],
            "the release of all input ended the shift"
        );
        assert_eq!(
            posts(&mut sequencer, left(true, 3)),
            [Posted::Button(MouseButton::Left, true, 3)],
            "and forgot the first landing"
        );
        posts(&mut sequencer, left(false, 3));
        posts(&mut sequencer, move_to(108.0, 200.0));
        assert_eq!(
            sequencer.landing.press_target(MouseButton::Left, 2),
            Some(Point::new(104.0, 200.0)),
            "admitted, this press would snap"
        );

        gate.floor().reset();
        assert_eq!(apply(&mut sequencer, left(true, 2), None).1, []);
        assert_eq!(apply(&mut sequencer, move_to(100.0, 200.0), None).1, []);
    }

    #[test]
    fn a_snapped_drag_across_a_seam_drops_its_shift_there() {
        let (mut sequencer, _gate) = sequencer();
        posts(&mut sequencer, move_to(1912.0, 500.0));
        posts(&mut sequencer, left(true, 1));
        posts(&mut sequencer, left(false, 1));
        posts(&mut sequencer, move_to(1919.0, 500.0));
        posts(&mut sequencer, left(true, 2));
        assert_eq!(
            posts(&mut sequencer, move_to(1921.0, 500.0)),
            [Posted::Move(1921.0, 500.0)],
            "on the next display, the source's own point: one jump, no dead zone"
        );
        assert_eq!(
            posts(&mut sequencer, move_to(1915.0, 500.0)),
            [Posted::Move(1915.0, 500.0)],
            "the shift stays dropped"
        );
        assert_eq!(
            posts(&mut sequencer, left(false, 2)),
            [Posted::Button(MouseButton::Left, false, 2)],
            "nothing to move back from"
        );
    }

    /// Ends control the way a local take-back or a revocation does.
    fn end_control(gate: &TakeBackGate, revocation: &RevocationSignal, revoke: bool) {
        if revoke {
            revocation.mark_revoked_without_wake();
        } else {
            gate.floor().reset();
        }
    }

    #[test]
    fn control_ending_after_a_release_skips_its_move_back() {
        for revoke in [false, true] {
            let (mut sequencer, gate) = snapped_double();
            let revocation = sequencer.revocation.clone();
            let (result, tried) = run(&mut sequencer, left(false, 2), |_| {
                end_control(&gate, &revocation, revoke);
                Ok(())
            });
            assert_eq!(result, Ok(()));
            assert_eq!(
                tried,
                [Posted::Button(MouseButton::Left, false, 2)],
                "no warp after control ended"
            );
            assert!(!sequencer.admission.buttons[MouseButton::Left.index()]);
            assert!(!gate.injected_held());
            assert_eq!(
                posts(&mut sequencer, DestinationAction::ReleaseAll),
                [Posted::ReleaseAll]
            );
        }
    }

    #[test]
    fn autoscroll_started_under_control_never_scrolls_once_control_ends() {
        use crate::session_autoscroll::Autoscroll;
        use std::time::{Duration, Instant};
        for revoke in [false, true] {
            let (mut sequencer, gate) = sequencer();
            let revocation = sequencer.revocation.clone();
            let mut autoscroll = Autoscroll::new();
            let start = Instant::now();
            let controller = sequencer.controller();
            assert!(controller.is_some());
            let middle = |pressed| DestinationAction::Button {
                button: MouseButton::Middle,
                pressed,
                click_count: 1,
            };
            let mut delivered = Vec::new();
            for (millis, action) in [
                (0, move_to(100.0, 100.0)),
                (1, middle(true)),
                (2, middle(false)),
                (3, move_to(100.0, 400.0)),
            ] {
                let at = start + Duration::from_millis(millis);
                autoscroll
                    .apply(action, controller, at, |action| {
                        let (result, posts) = apply(&mut sequencer, action, None);
                        delivered.extend(posts);
                        result
                    })
                    .unwrap();
            }
            assert_eq!(
                delivered,
                [
                    Posted::Move(100.0, 100.0),
                    Posted::Button(MouseButton::Middle, true, 1),
                    Posted::Button(MouseButton::Middle, false, 1),
                    Posted::Move(100.0, 400.0)
                ]
            );
            assert!(!gate.injected_held());
            let owed = start + Duration::from_millis(20);
            assert!(
                autoscroll
                    .tick(owed, sequencer.controller(), true)
                    .is_some()
            );

            end_control(&gate, &revocation, revoke);
            assert_eq!(sequencer.controller(), None);
            let later = start + Duration::from_millis(40);
            assert_eq!(autoscroll.tick(later, sequencer.controller(), true), None);
            assert_eq!(autoscroll.marker(), None, "the marker comes down");
            let reclaimed = start + Duration::from_millis(60);
            assert_eq!(
                autoscroll.tick(reclaimed, controller, true),
                None,
                "and stays down"
            );
        }
    }

    #[test]
    fn a_held_back_middle_click_never_drags_a_held_left_button() {
        use crate::session_autoscroll::Autoscroll;
        use std::time::Instant;
        let (mut sequencer, _gate) = sequencer();
        let controller = sequencer.controller();
        let mut autoscroll = Autoscroll::new();
        let middle = |pressed, click_count| DestinationAction::Button {
            button: MouseButton::Middle,
            pressed,
            click_count,
        };
        let mut delivered = Vec::new();
        for action in [
            move_to(100.0, 100.0),
            middle(true, 1),
            middle(false, 1),
            middle(true, 2),
            middle(false, 2),
            move_to(104.0, 100.0),
            middle(true, 3),
            left(true, 1),
            middle(false, 3),
        ] {
            autoscroll
                .apply(action, controller, Instant::now(), |action| {
                    let (result, posts) = apply(&mut sequencer, action, None);
                    delivered.extend(posts);
                    result
                })
                .unwrap();
        }
        assert_eq!(
            delivered,
            [
                Posted::Move(100.0, 100.0),
                Posted::Button(MouseButton::Middle, true, 1),
                Posted::Button(MouseButton::Middle, false, 1),
                Posted::Move(104.0, 100.0),
                Posted::Button(MouseButton::Left, true, 1),
                Posted::Button(MouseButton::Middle, true, 1),
                Posted::Button(MouseButton::Middle, false, 1),
            ],
            "no snap back to an earlier click while left is down"
        );
    }

    #[test]
    fn control_ending_after_a_snap_refuses_its_press() {
        for revoke in [false, true] {
            let (mut sequencer, gate) = sequencer();
            posts(&mut sequencer, move_to(100.0, 200.0));
            posts(&mut sequencer, left(true, 1));
            posts(&mut sequencer, left(false, 1));
            posts(&mut sequencer, move_to(104.0, 200.0));
            let revocation = sequencer.revocation.clone();
            let (result, tried) = run(&mut sequencer, left(true, 2), |_| {
                end_control(&gate, &revocation, revoke);
                Ok(())
            });
            assert_eq!(result, Ok(()));
            assert_eq!(
                tried,
                [Posted::Move(100.0, 200.0)],
                "the press never follows the snap"
            );
            assert!(!sequencer.admission.buttons[MouseButton::Left.index()]);
            assert!(!gate.injected_held(), "the refused press is un-counted");
        }
    }
}
