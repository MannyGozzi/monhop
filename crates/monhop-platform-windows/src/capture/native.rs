//! Explicit Windows capture lifetime. Callbacks only decode, admit, and latch failures.

use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use monhop_core::{
    CapturePermit, FloorState, MouseButton, NativeSessionClaim, Point, RevocationSignal,
    TakeBackGate,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
            MapVirtualKeyW, SendInput,
        },
        Input::{
            GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_REMOVE,
            RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::*,
    },
};

use crate::{
    capture::{
        CaptureConsumer, CaptureEvent, CaptureProducer, CaptureStop, CapturedEvent,
        MAX_SUPPRESSION_TTL, StopReason, SuppressionLease, capture_channel,
    },
    capture_control::{
        CaptureCommand, ControlCompletion, ControlError, ControlReader, ControlWriter,
        control_channel,
    },
    capture_decode::{DecodedInput, decode_keyboard, decode_mouse},
    capture_physical::{LocalTransfer, PhysicalCapture},
    desktop_state::ordinary_desktop_is_active,
    input::{
        MONHOP_INJECTED_MARKER, RawCaptureOwnership, VirtualDesktop,
        absolute_send_input_coordinates,
    },
    keymap::set1_from_hid_usage,
};

const CLASS_NAME: &[u16] = &[
    76, 111, 99, 97, 108, 75, 77, 67, 97, 112, 116, 117, 114, 101, 0,
];
static ACTIVE: AtomicBool = AtomicBool::new(false);
const CLOSE_ATTEMPTS: usize = 3;
const KEYBOARD_INJECTED_FLAGS: u32 = 0x12;
const MOUSE_INJECTED_FLAGS: u32 = 0x03;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCaptureError {
    AlreadyActive,
    DesktopUnavailable,
    InvalidDuration,
    StartFailed,
    StartupTimeout,
    CleanupPending,
    ControlPending,
    ControlFailed,
    WorkerPanicked,
    Stopped(StopReason),
    Windows { operation: &'static str, code: u32 },
}

/// Reads physical screen coordinates, matching MSLLHOOKSTRUCT.pt regardless of caller DPI context.
pub fn current_pointer_position() -> Option<Point> {
    let mut position = POINT::default();
    // SAFETY: the writable POINT lives through this read-only call; no hook or state is installed.
    if unsafe { GetPhysicalCursorPos(&mut position) } == 0 {
        return None;
    }
    Some(Point::new(f64::from(position.x), f64::from(position.y)))
}

struct Shared {
    origin: Instant,
    generation: u64,
    stop: CaptureStop,
    ready: AtomicBool,
    progress_ms: AtomicU64,
    allows_suppression: bool,
    device_removed: AtomicBool,
    revocation: RevocationSignal,
}

enum CaptureMode {
    Session(TakeBackGate),
    Diagnostic(Duration),
}

/// What the hook thread's callback state takes ownership of when capture starts.
struct CallbackParts {
    producer: CaptureProducer,
    physical: PhysicalCapture,
    take_back: Option<TakeBackGate>,
}

/// The caller must validate the authenticated peer and topology before a session capture.
/// Dropping this owner stops forwarding. Only already-suppressed presses may keep draining.
pub struct NativeCapture {
    shared: Arc<Shared>,
    consumer: CaptureConsumer,
    worker: Option<JoinHandle<Result<(), NativeCaptureError>>>,
    control: ControlWriter,
    completion: Option<Result<(), NativeCaptureError>>,
}

impl NativeCapture {
    /// Physical input while `gate`'s floor is Receiving takes this computer back.
    pub fn start_for_session(
        generation: u64,
        revocation: RevocationSignal,
        permit: CapturePermit,
        gate: TakeBackGate,
    ) -> Result<Self, NativeCaptureError> {
        Self::start(generation, revocation, permit, CaptureMode::Session(gate))
    }

    /// Claims native input for a passive, bounded capture that never suppresses or takes back.
    pub fn start_diagnostic(duration: Duration) -> Result<Self, NativeCaptureError> {
        if duration.is_zero() || duration > Duration::from_secs(30) {
            return Err(NativeCaptureError::InvalidDuration);
        }
        let (permit, injection) = NativeSessionClaim::claim()
            .ok_or(NativeCaptureError::AlreadyActive)?
            .split();
        drop(injection);
        Self::start(
            1,
            RevocationSignal::default(),
            permit,
            CaptureMode::Diagnostic(duration),
        )
    }

    fn start(
        generation: u64,
        revocation: RevocationSignal,
        permit: CapturePermit,
        mode: CaptureMode,
    ) -> Result<Self, NativeCaptureError> {
        if generation == 0 || revocation.is_stopping() {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        if ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeCaptureError::AlreadyActive);
        }
        let ownership = match RawCaptureOwnership::claim() {
            Ok(ownership) => ownership,
            Err(_) => {
                ACTIVE.store(false, Ordering::Release);
                return Err(NativeCaptureError::AlreadyActive);
            }
        };
        let stop = CaptureStop::default();
        let (producer, consumer) = capture_channel(stop.clone());
        let (control, control_reader) = control_channel();
        let (physical, take_back, duration) = match mode {
            CaptureMode::Session(gate) => (
                PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone()),
                Some(gate),
                None,
            ),
            CaptureMode::Diagnostic(duration) => (
                PhysicalCapture::new_passive(Duration::ZERO),
                None,
                Some(duration),
            ),
        };
        let parts = CallbackParts {
            producer,
            physical,
            take_back,
        };
        let shared = Arc::new(Shared {
            origin: Instant::now(),
            generation,
            stop,
            ready: AtomicBool::new(false),
            progress_ms: AtomicU64::new(0),
            allows_suppression: duration.is_none(),
            device_removed: AtomicBool::new(false),
            revocation: revocation.clone(),
        });
        let thread_shared = shared.clone();
        let spawn_failure_revocation = revocation.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("monhop-capture".into())
            .spawn(move || {
                if let Err(code) = crate::threads::mark_time_sensitive() {
                    log::warn!("native capture keeps normal priority (code {code})");
                }
                let wake_after_cleanup = revocation.clone();
                let result = {
                    // Held through final local release retries, so no later session can claim
                    // native input while this capture still owes cleanup.
                    let _permit = permit;
                    struct ActiveGuard;
                    impl Drop for ActiveGuard {
                        fn drop(&mut self) {
                            ACTIVE.store(false, Ordering::Release);
                        }
                    }
                    let _active = ActiveGuard;
                    let mut ownership = ownership;
                    let result = run(
                        thread_shared.clone(),
                        parts,
                        control_reader,
                        revocation,
                        duration,
                        started_tx,
                        &mut ownership,
                    );
                    if let Err(error) = &result {
                        log::warn!("native capture worker failed: {error:?}");
                        thread_shared.stop.stop(StopReason::NativeFailure);
                    }
                    result
                };
                if wake_after_cleanup.is_revoked() {
                    wake_after_cleanup.revoke();
                }
                result
            }) {
            Ok(worker) => worker,
            Err(error) => {
                log::warn!("native capture worker could not start: {error}");
                ACTIVE.store(false, Ordering::Release);
                spawn_failure_revocation.mark_revoked_without_wake();
                spawn_failure_revocation.revoke();
                return Err(NativeCaptureError::StartFailed);
            }
        };
        let owner = Self {
            shared,
            consumer,
            worker: Some(worker),
            control,
            completion: None,
        };
        match started_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(owner),
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(NativeCaptureError::StartupTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(NativeCaptureError::StartFailed),
        }
    }

    pub fn try_next_event(&mut self) -> Result<Option<CapturedEvent>, StopReason> {
        self.consumer.try_pop_tagged()
    }

    /// False while a key or button held at capture start keeps suppression refused.
    pub fn is_ready_for_suppression(&self) -> bool {
        self.shared.allows_suppression && self.shared.ready.load(Ordering::Acquire)
    }

    /// Runs `waker` on the hook thread after every queued event; see `CaptureConsumer::set_waker`.
    pub fn set_waker(&self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) -> bool {
        self.consumer.set_waker(waker)
    }

    pub fn renew_suppression(
        &mut self,
        generation: u64,
        ttl: Duration,
    ) -> Result<u64, NativeCaptureError> {
        if !self.shared.allows_suppression || !self.shared.ready.load(Ordering::Acquire) {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        let age = self
            .shared
            .origin
            .elapsed()
            .as_millis()
            .saturating_sub(u128::from(self.shared.progress_ms.load(Ordering::Acquire)));
        if age >= 60 {
            self.shared.stop.stop(StopReason::NativeFailure);
            return Err(NativeCaptureError::Stopped(StopReason::NativeFailure));
        }
        if ttl.is_zero() || ttl > MAX_SUPPRESSION_TTL {
            self.shared.stop.stop(StopReason::InvalidInput);
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        let deadline = self
            .shared
            .origin
            .elapsed()
            .checked_add(ttl)
            .ok_or(NativeCaptureError::Stopped(StopReason::InvalidInput))?;
        self.submit_control(generation, CaptureCommand::RemoteUntil(deadline))
    }

    /// Requests an atomic transfer of locally delivered held input before remote capture begins.
    pub fn activate_remote(
        &mut self,
        generation: u64,
        ttl: Duration,
    ) -> Result<u64, NativeCaptureError> {
        if !self.shared.allows_suppression || !self.shared.ready.load(Ordering::Acquire) {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        if ttl.is_zero() || ttl > MAX_SUPPRESSION_TTL {
            self.shared.stop.stop(StopReason::InvalidInput);
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        let deadline = self
            .shared
            .origin
            .elapsed()
            .checked_add(ttl)
            .ok_or(NativeCaptureError::Stopped(StopReason::InvalidInput))?;
        self.submit_control(generation, CaptureCommand::ActivateRemote { deadline })
    }

    pub fn restore_local(&mut self, generation: u64) -> Result<u64, NativeCaptureError> {
        self.submit_control(generation, CaptureCommand::Local)
    }

    /// Requests an atomic local cursor placement and held-input restoration on the capture thread.
    pub fn restore_local_at(
        &mut self,
        generation: u64,
        position: Point,
    ) -> Result<u64, NativeCaptureError> {
        self.submit_control(generation, CaptureCommand::LocalAt(position))
    }

    fn submit_control(
        &mut self,
        generation: u64,
        command: CaptureCommand,
    ) -> Result<u64, NativeCaptureError> {
        latch_session_stop(&self.shared);
        if generation != self.shared.generation {
            self.shared.stop.stop(StopReason::InvalidInput);
        }
        if let Some(reason) = self.shared.stop.reason() {
            return Err(NativeCaptureError::Stopped(reason));
        }
        self.control.submit(command).map_err(|error| match error {
            ControlError::Pending => NativeCaptureError::ControlPending,
            ControlError::Failed => self
                .shared
                .stop
                .reason()
                .map(NativeCaptureError::Stopped)
                .unwrap_or(NativeCaptureError::ControlFailed),
            ControlError::Exhausted
            | ControlError::InvalidDeadline
            | ControlError::InvalidPoint => {
                self.shared.stop.stop(StopReason::InvalidInput);
                NativeCaptureError::Stopped(StopReason::InvalidInput)
            }
        })
    }

    pub fn completed_control_revision(&self) -> Result<u64, ControlError> {
        self.control.completed_revision()
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        self.shared.stop.reason()
    }

    pub fn request_stop(&self) {
        self.shared.stop.stop(StopReason::Requested);
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Never waits on a held physical key. The caller can poll until terminal draining finishes.
    pub fn finish(&mut self) -> Result<(), NativeCaptureError> {
        self.request_stop();
        if !self.is_finished() {
            return Err(NativeCaptureError::CleanupPending);
        }
        if let Some(worker) = self.worker.take() {
            self.completion = Some(
                worker
                    .join()
                    .unwrap_or(Err(NativeCaptureError::WorkerPanicked)),
            );
        }
        self.completion.unwrap_or(Ok(()))
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        self.request_stop();
    }
}

struct CallbackState {
    shared: Arc<Shared>,
    producer: CaptureProducer,
    physical: PhysicalCapture,
    take_back: Option<TakeBackGate>,
    lease: SuppressionLease,
    route_remote: bool,
    press_revision: u64,
    ignored: IgnoredInputCounts,
    held_virtual_keys: [Option<u32>; 256],
    unsupported_presses_seen: [bool; 256],
}

#[derive(Default)]
struct IgnoredInputCounts {
    unsupported_events: u64,
    unsourced_motion_samples: u64,
    unmapped_held_keys: u64,
    recovered_releases: u64,
}

impl Drop for CallbackState {
    fn drop(&mut self) {
        log::info!(
            "ignored {} unsupported events, {} unsourced motion samples, {} unmapped held keys; recovered {} releases",
            self.ignored.unsupported_events,
            self.ignored.unsourced_motion_samples,
            self.ignored.unmapped_held_keys,
            self.ignored.recovered_releases,
        );
    }
}

thread_local! {
    static CALLBACK: RefCell<Option<CallbackState>> = const { RefCell::new(None) };
    static CALLBACK_STOP: RefCell<Option<CaptureStop>> = const { RefCell::new(None) };
    static CALLBACK_FAILURE: Cell<Option<CallbackFailure>> = const { Cell::new(None) };
    static CALLBACK_SITE: Cell<Option<u32>> = const { Cell::new(None) };
    static CALLBACK_REENTRY: Cell<Option<(Option<u32>, u32)>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallbackFailure {
    ReentrantCallback,
    CallbackPanicked,
    DisplayChanged,
    PowerBroadcast(usize),
    RawInputRead,
}

impl CallbackFailure {
    const fn stop_reason(self) -> StopReason {
        match self {
            Self::DisplayChanged => StopReason::DisplaysChanged,
            _ => StopReason::NativeFailure,
        }
    }
}

#[track_caller]
fn latch_failure(failure: CallbackFailure) {
    let stop = CALLBACK_STOP.with(|slot| slot.try_borrow().ok().and_then(|slot| slot.clone()));
    if let Some(stop) = stop {
        CALLBACK_FAILURE.with(|first| {
            if first.get().is_none() {
                first.set(Some(failure));
            }
        });
        stop.stop(failure.stop_reason());
    }
}

fn latch_session_stop(shared: &Shared) {
    if shared.revocation.is_stopping() {
        shared.stop.stop(StopReason::Requested);
    }
}

fn propagate_capture_stop(shared: &Shared, wake_attempted: &mut bool) {
    if !shared.stop.is_stopped() {
        return;
    }
    if !shared.revocation.is_stopping() {
        log::warn!(
            "native capture stopped: {:?} at {}; callback failure: {:?}; callback sites (active, incoming): {:?}; requesting session stop",
            shared.stop.reason(),
            shared.stop.origin().map_or_else(
                || "?".to_owned(),
                |at| format!("{}:{}", at.file(), at.line())
            ),
            CALLBACK_FAILURE.get(),
            CALLBACK_REENTRY.get(),
        );
    }
    // CaptureStop already blocks forwarding and suppression. Leave the socket alive for its close.
    // request_stop only sets atomics; it cannot run a waker on this capture thread.
    shared.revocation.request_stop();
    if shared.revocation.is_revoked() && !*wake_attempted {
        // A hard safety stop must wake observers even while suppressed presses drain.
        *wake_attempted = dispatch_revocation_wake(&shared.revocation);
    }
}

fn startup_environment_error(ordinary_desktop_active: bool) -> Option<NativeCaptureError> {
    (!ordinary_desktop_active).then_some(NativeCaptureError::DesktopUnavailable)
}

fn startup_completion_error(
    previous_stop: Option<StopReason>,
    environment: impl FnOnce() -> Option<NativeCaptureError>,
    later_stop: impl FnOnce() -> Option<StopReason>,
) -> Option<NativeCaptureError> {
    if let Some(reason) = previous_stop {
        return Some(NativeCaptureError::Stopped(reason));
    }
    environment().or_else(|| later_stop().map(NativeCaptureError::Stopped))
}

#[track_caller]
fn with_callback(action: impl FnOnce(&mut CallbackState) -> bool) -> bool {
    let incoming = std::panic::Location::caller().line();
    let result = catch_unwind(AssertUnwindSafe(|| {
        CALLBACK.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                if CALLBACK_REENTRY.get().is_none() {
                    CALLBACK_REENTRY.set(Some((CALLBACK_SITE.get(), incoming)));
                }
                latch_failure(CallbackFailure::ReentrantCallback);
                return false;
            };
            struct SiteGuard(Option<u32>);
            impl Drop for SiteGuard {
                fn drop(&mut self) {
                    CALLBACK_SITE.set(self.0);
                }
            }
            let _site = SiteGuard(CALLBACK_SITE.replace(Some(incoming)));
            slot.as_mut().is_some_and(action)
        })
    }));
    match result {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            latch_failure(CallbackFailure::CallbackPanicked);
            false
        }
    }
}

impl CallbackState {
    fn remote(&mut self) -> bool {
        latch_session_stop(&self.shared);
        self.lease.is_suppressing(self.shared.origin.elapsed())
    }

    fn renew_remote(
        &mut self,
        deadline: Duration,
        now: Duration,
    ) -> Result<(), NativeCaptureError> {
        if !self.route_remote || !self.physical.is_ready_for_suppression() {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        self.renew_until(deadline, now)
    }

    fn restore_local(&mut self, revision: u64, now: Duration) -> Result<(), NativeCaptureError> {
        self.release_lease(now)?;
        self.publish_route(revision, false)
    }

    fn renew_until(&mut self, deadline: Duration, now: Duration) -> Result<(), NativeCaptureError> {
        let ttl = deadline
            .checked_sub(now)
            .filter(|ttl| !ttl.is_zero())
            .ok_or(NativeCaptureError::Stopped(StopReason::LeaseExpired))?;
        self.lease
            .renew(self.shared.generation, now, ttl)
            .map_err(NativeCaptureError::Stopped)
    }

    fn release_lease(&mut self, now: Duration) -> Result<(), NativeCaptureError> {
        self.lease
            .release(self.shared.generation, now)
            .map_err(NativeCaptureError::Stopped)
    }

    fn release_injected(&mut self) -> Result<(), NativeCaptureError> {
        let plan = self.physical.release_injected_plan();
        for transfer in plan.iter() {
            match catch_unwind(AssertUnwindSafe(|| send_local_transfer(transfer))) {
                Ok(Ok(())) => self.physical.apply_transfer_success(transfer),
                Ok(Err(error)) => return Err(error),
                Err(payload) => {
                    std::mem::forget(payload);
                    return Err(NativeCaptureError::Stopped(StopReason::NativeFailure));
                }
            }
        }
        Ok(())
    }

    fn publish_route(&mut self, revision: u64, remote: bool) -> Result<(), NativeCaptureError> {
        if remote == self.route_remote {
            return Ok(());
        }
        let event = CapturedEvent {
            event: CaptureEvent::RouteChanged { remote, revision },
            routing_revision: revision,
            remote,
            floor_generation: self
                .take_back
                .as_ref()
                .map_or(0, |gate| gate.floor().snapshot().generation),
        };
        self.producer
            .try_push_tagged(event)
            .map_err(NativeCaptureError::Stopped)?;
        self.route_remote = remote;
        self.physical.set_routing_revision(revision);
        Ok(())
    }

    fn event(&mut self, event: CaptureEvent) -> bool {
        self.event_with_travel(event, None)
    }

    fn event_with_travel(&mut self, event: CaptureEvent, travel: Option<(f64, f64)>) -> bool {
        if matches!(
            event,
            CaptureEvent::Key { .. } | CaptureEvent::Button { .. }
        ) {
            match self.press_revision.checked_add(1) {
                Some(revision) => self.press_revision = revision,
                None => self.shared.stop.stop(StopReason::NativeFailure),
            }
        }
        let now = self.shared.origin.elapsed();
        let remote = self.remote();
        let result = self.physical.process_with_travel(
            event,
            travel,
            remote,
            now,
            &mut self.producer,
            &self.shared.stop,
        );
        self.shared
            .ready
            .store(self.physical.is_ready_for_suppression(), Ordering::Release);
        result
    }

    /// The low-level hook is the only point that can withhold before delivery, so it measures a
    /// physical move's travel from `cursor_before`, read only while take-back can fire.
    fn hook_mouse(
        &mut self,
        decoded: DecodedInput,
        cursor_before: impl FnOnce() -> Option<Point>,
    ) -> bool {
        let DecodedInput::Event(event @ CaptureEvent::AbsoluteMotion { x, y }) = decoded else {
            return self.decoded(decoded);
        };
        let receiving = self
            .take_back
            .as_ref()
            .is_some_and(|gate| gate.floor().snapshot().state == FloorState::Receiving);
        let travel = receiving
            .then(cursor_before)
            .flatten()
            .map(|before| (f64::from(x) - before.x, f64::from(y) - before.y));
        self.event_with_travel(event, travel)
    }

    fn decoded(&mut self, event: DecodedInput) -> bool {
        match event {
            DecodedInput::Ignored => false,
            DecodedInput::Unsupported => {
                self.ignored.unsupported_events = self.ignored.unsupported_events.saturating_add(1);
                false
            }
            DecodedInput::Malformed => {
                self.shared.stop.stop(StopReason::InvalidInput);
                false
            }
            DecodedInput::Event(event) => self.event(event),
        }
    }

    fn keyboard(
        &mut self,
        message: u32,
        virtual_key: u32,
        scan_code: u32,
        flags: u32,
        extra_info: usize,
    ) -> bool {
        let decoded = decode_keyboard(message, virtual_key, scan_code, flags, extra_info);
        match decoded {
            DecodedInput::Event(CaptureEvent::Key { usage, pressed, .. }) => {
                self.held_virtual_keys[usize::from(usage.0)] = pressed.then_some(virtual_key);
            }
            DecodedInput::Unsupported if flags & 0x80 != 0 => {
                // Recover only a physical release with one known press. Enter and keypad Enter
                // can share a virtual key, so an ambiguous release must still stop safely.
                let mut held = self
                    .held_virtual_keys
                    .iter()
                    .enumerate()
                    .filter(|(_, key)| **key == Some(virtual_key));
                if let Some((usage, _)) = held.next() {
                    if held.next().is_some()
                        || self
                            .unsupported_presses_seen
                            .get(virtual_key as usize)
                            .copied()
                            .unwrap_or(true)
                    {
                        return self.decoded(DecodedInput::Malformed);
                    }
                    self.held_virtual_keys[usage] = None;
                    self.ignored.recovered_releases =
                        self.ignored.recovered_releases.saturating_add(1);
                    return self.event(CaptureEvent::Key {
                        usage: monhop_core::HidUsage(usage as u16),
                        pressed: false,
                        repeat: false,
                        modifiers: monhop_core::ModifierState(0),
                    });
                }
            }
            DecodedInput::Unsupported => self.mark_unsupported_press(virtual_key),
            _ => {}
        }
        self.decoded(decoded)
    }

    fn mark_unsupported_press(&mut self, virtual_key: u32) {
        // Keep ambiguity for the capture lifetime: repeats and overlapping unmapped keys cannot
        // be distinguished by virtual key, so an observed release cannot safely clear it.
        if let Some(seen) = self.unsupported_presses_seen.get_mut(virtual_key as usize) {
            *seen = true;
        }
    }

    fn raw_mouse(&mut self, raw: &RAWINPUT) {
        if raw.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        // SAFETY: the reported RAWINPUT type selects the initialized mouse union member.
        let mouse = unsafe { raw.data.mouse };
        if mouse.ulExtraInformation == MONHOP_INJECTED_MARKER as u32 {
            return;
        }
        // These samples cannot supply physical relative motion. Local emergency escape still runs
        // through the keyboard hook and worker timer if the remote pointer cannot move.
        if raw.header.hDevice.is_null() || mouse.usFlags & 1 != 0 {
            self.ignored.unsourced_motion_samples =
                self.ignored.unsourced_motion_samples.saturating_add(1);
            return;
        }
        // WM_INPUT arrives too late to withhold; hook_mouse withholds the same movement.
        if mouse.lLastX != 0 || mouse.lLastY != 0 {
            self.event(CaptureEvent::RelativeMotion {
                dx: mouse.lLastX,
                dy: mouse.lLastY,
            });
        }
    }

    fn seed_held_key(&mut self, virtual_key: u32, scan: u32) {
        // Reuse hook decoding so Pause cannot seed NumLock or an unsupported E1 sequence.
        let flags = match scan >> 8 {
            0 => 0,
            0xe0 => 1,
            _ => {
                self.ignored.unmapped_held_keys = self.ignored.unmapped_held_keys.saturating_add(1);
                self.mark_unsupported_press(virtual_key);
                return;
            }
        };
        if let DecodedInput::Event(CaptureEvent::Key { usage, .. }) =
            decode_keyboard(WM_KEYDOWN, virtual_key, scan & 0xff, flags, 0)
        {
            self.physical.seed_locally_held_key(usage);
            self.held_virtual_keys[usize::from(usage.0)] = Some(virtual_key);
        } else {
            self.ignored.unmapped_held_keys = self.ignored.unmapped_held_keys.saturating_add(1);
            self.mark_unsupported_press(virtual_key);
        }
    }
}

fn control_state<T>(
    action: impl FnOnce(&mut CallbackState) -> Result<T, NativeCaptureError>,
) -> Result<T, NativeCaptureError> {
    let mut result = Err(NativeCaptureError::Stopped(StopReason::NativeFailure));
    with_callback(|state| {
        result = action(state);
        false
    });
    result
}

fn check_control_live(shared: &Shared) -> Result<(), NativeCaptureError> {
    latch_session_stop(shared);
    match shared.stop.reason() {
        Some(reason) => Err(NativeCaptureError::Stopped(reason)),
        None => Ok(()),
    }
}

fn apply_control(revision: u64, command: CaptureCommand) -> ControlCompletion {
    apply_control_with(revision, command, place_local_cursor, send_local_transfer)
}

fn apply_control_with(
    revision: u64,
    command: CaptureCommand,
    place: impl FnOnce(Point) -> Result<(), NativeCaptureError>,
    mut transfer: impl FnMut(LocalTransfer) -> Result<(), NativeCaptureError>,
) -> ControlCompletion {
    let Ok(shared) = control_state(|state| Ok(state.shared.clone())) else {
        return ControlCompletion::Failed;
    };
    let result = (|| {
        check_control_live(&shared)?;
        if matches!(command, CaptureCommand::ActivateRemote { .. }) {
            control_state(|state| {
                if state.route_remote || !state.physical.is_ready_for_suppression() {
                    return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
                }
                Ok(())
            })?;
        }
        if let CaptureCommand::LocalAt(position) = command {
            // SendInput can dispatch physical hooks synchronously. Never hold CALLBACK across it.
            place(position)?;
        }
        let target = match command {
            CaptureCommand::ActivateRemote { .. } => Some(true),
            CaptureCommand::LocalAt(_) => Some(false),
            _ => None,
        };
        if let Some(remote) = target {
            apply_handoff(&shared, remote, &mut transfer)?;
        }
        check_control_live(&shared)?;
        control_state(|state| {
            latch_session_stop(&shared);
            if let Some(reason) = shared.stop.reason() {
                return Err(NativeCaptureError::Stopped(reason));
            }
            let now = shared.origin.elapsed();
            match command {
                CaptureCommand::ActivateRemote { deadline } => {
                    state.renew_until(deadline, now)?;
                    state.publish_route(revision, true)
                }
                CaptureCommand::RemoteUntil(deadline) => state.renew_remote(deadline, now),
                CaptureCommand::LocalAt(_) | CaptureCommand::Local => {
                    state.restore_local(revision, now)
                }
            }
        })
    })();
    if let Err(error) = result {
        shared.stop.stop(stop_reason(error));
        return ControlCompletion::Failed;
    }
    if shared.stop.is_stopped() {
        ControlCompletion::Failed
    } else {
        ControlCompletion::Applied
    }
}

fn apply_handoff(
    shared: &Shared,
    remote: bool,
    send: &mut impl FnMut(LocalTransfer) -> Result<(), NativeCaptureError>,
) -> Result<(), NativeCaptureError> {
    // A hook can run before or after our injected effect inside SendInput. After physical
    // press activity, establish a quiet release before replanning from the current ledger.
    let mut pending_release = None;
    for _ in 0..512 {
        let next = if let Some(release) = pending_release.take() {
            release
        } else {
            check_control_live(shared)?;
            let next =
                control_state(|state| Ok(state.physical.handoff_plan(remote).iter().next()))?;
            let Some(next) = next else { return Ok(()) };
            next
        };
        let before = control_state(|state| Ok(state.press_revision))?;
        send(next)?;
        pending_release = control_state(|state| {
            let raced = state.press_revision != before;
            let pressed = matches!(
                next,
                LocalTransfer::Key { pressed: true, .. }
                    | LocalTransfer::Button { pressed: true, .. }
            );
            let compensate =
                raced || pressed && (shared.revocation.is_stopping() || shared.stop.is_stopped());
            state.physical.apply_transfer_success(next);
            if !compensate {
                return Ok(None);
            }
            let (release, owed_down) = match next {
                LocalTransfer::Key { usage, .. } => (
                    LocalTransfer::Key {
                        usage,
                        pressed: false,
                    },
                    LocalTransfer::Key {
                        usage,
                        pressed: true,
                    },
                ),
                LocalTransfer::Button { button, .. } => (
                    LocalTransfer::Button {
                        button,
                        pressed: false,
                    },
                    LocalTransfer::Button {
                        button,
                        pressed: true,
                    },
                ),
            };
            // Unknown native ordering must retain a cleanup obligation even if retry fails.
            state.physical.apply_transfer_success(owed_down);
            Ok(Some(release))
        })?;
    }
    Err(NativeCaptureError::Stopped(StopReason::NativeFailure))
}

fn stop_reason(error: NativeCaptureError) -> StopReason {
    match error {
        NativeCaptureError::Stopped(reason) => reason,
        _ => StopReason::NativeFailure,
    }
}

fn send_local_transfer(transfer: LocalTransfer) -> Result<(), NativeCaptureError> {
    let input = match transfer {
        LocalTransfer::Key { usage, pressed } => keyboard_transfer_input(usage, pressed)?,
        LocalTransfer::Button { button, pressed } => button_transfer_input(button, pressed),
    };
    // SAFETY: input is one initialized INPUT record with the exact native layout.
    let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(windows_error("SendInput held-input transfer"))
    }
}

fn keyboard_transfer_input(
    usage: monhop_core::HidUsage,
    pressed: bool,
) -> Result<INPUT, NativeCaptureError> {
    let scan_code = set1_from_hid_usage(usage)
        .map_err(|_| NativeCaptureError::Stopped(StopReason::InvalidInput))?;
    let mut flags = KEYEVENTF_SCANCODE;
    if scan_code.is_extended() {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    Ok(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: u16::from(scan_code.make_code),
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MONHOP_INJECTED_MARKER,
            },
        },
    })
}

fn button_transfer_input(button: MouseButton, pressed: bool) -> INPUT {
    let (flags, mouse_data) = match (button, pressed) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MONHOP_INJECTED_MARKER,
            },
        },
    }
}

fn place_local_cursor(position: Point) -> Result<(), NativeCaptureError> {
    if !ordinary_desktop_is_active() {
        return Err(NativeCaptureError::Stopped(StopReason::NativeFailure));
    }
    let displays = crate::enumerate_displays(monhop_core::DeviceId([0; 16]))
        .map_err(|_| NativeCaptureError::Stopped(StopReason::NativeFailure))?;
    if !position.is_finite()
        || !displays
            .iter()
            .any(|display| display.bounds().contains(position))
    {
        return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
    }
    let desktop = current_virtual_desktop()?;
    let (x, y) = local_cursor_coordinates(position, desktop)?;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: MONHOP_INJECTED_MARKER,
            },
        },
    };
    // SAFETY: input is one initialized, marked absolute virtual-desktop mouse record.
    if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } != 1 {
        return Err(windows_error("SendInput local cursor placement"));
    }
    Ok(())
}

fn current_virtual_desktop() -> Result<VirtualDesktop, NativeCaptureError> {
    // SAFETY: these metrics query the active virtual desktop without changing native state.
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    // SAFETY: these metrics query the active virtual desktop without changing native state.
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    // SAFETY: these metrics query the active virtual desktop without changing native state.
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    // SAFETY: these metrics query the active virtual desktop without changing native state.
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let width =
        u32::try_from(width).map_err(|_| NativeCaptureError::Stopped(StopReason::NativeFailure))?;
    let height = u32::try_from(height)
        .map_err(|_| NativeCaptureError::Stopped(StopReason::NativeFailure))?;
    VirtualDesktop::new(left, top, width, height)
        .map_err(|_| NativeCaptureError::Stopped(StopReason::NativeFailure))
}

fn local_cursor_coordinates(
    position: Point,
    desktop: VirtualDesktop,
) -> Result<(i32, i32), NativeCaptureError> {
    let x = cursor_coordinate(position.x)?;
    let y = cursor_coordinate(position.y)?;
    absolute_send_input_coordinates(desktop, x, y)
        .map_err(|_| NativeCaptureError::Stopped(StopReason::InvalidInput))
}

fn cursor_coordinate(value: f64) -> Result<i32, NativeCaptureError> {
    let floored = value.floor();
    if !floored.is_finite() || floored < f64::from(i32::MIN) || floored > f64::from(i32::MAX) {
        return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
    }
    Ok(floored as i32)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        // SAFETY: Windows supplies KBDLLHOOKSTRUCT for a nonnegative low-level hook callback.
        let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if is_injected_keyboard(key.flags, key.dwExtraInfo) {
            // SAFETY: injected input is never admitted or borrowed from callback-local state.
            return unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) };
        }
        if with_callback(|state| {
            state.keyboard(
                wparam as u32,
                key.vkCode,
                key.scanCode,
                key.flags,
                key.dwExtraInfo,
            )
        }) {
            return 1;
        }
    }
    // SAFETY: chaining uses the original hook arguments, including negative hook codes.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        // SAFETY: Windows supplies MSLLHOOKSTRUCT for a nonnegative low-level mouse hook.
        let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if is_injected_mouse(mouse.flags, mouse.dwExtraInfo) {
            // SAFETY: injected input is never admitted or borrowed from callback-local state.
            return unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) };
        }
        // The system moves the cursor only after this hook returns, so the cursor position is the
        // one this record moves from, including any injected travel since the last physical record.
        if with_callback(|state| {
            state.hook_mouse(
                decode_mouse(
                    wparam as u32,
                    mouse.flags,
                    mouse.dwExtraInfo,
                    mouse.mouseData,
                    mouse.pt.x,
                    mouse.pt.y,
                ),
                current_pointer_position,
            )
        }) {
            return 1;
        }
    }
    // SAFETY: forwards unconsumed input with the original hook arguments.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

fn is_injected_keyboard(flags: u32, extra_info: usize) -> bool {
    flags & KEYBOARD_INJECTED_FLAGS != 0 || extra_info == MONHOP_INJECTED_MARKER
}

fn is_injected_mouse(flags: u32, extra_info: usize) -> bool {
    flags & MOUSE_INJECTED_FLAGS != 0 || extra_info == MONHOP_INJECTED_MARKER
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: WM_INPUT supplies a live HRAWINPUT; our fixed buffer fits mouse records.
        let read = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                (&mut raw as *mut RAWINPUT).cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || read < size_of::<RAWINPUTHEADER>() as u32 || raw.header.dwSize > read
        {
            latch_failure(CallbackFailure::RawInputRead);
        } else {
            with_callback(|state| {
                state.raw_mouse(&raw);
                false
            });
        }
    } else if message == WM_INPUT_DEVICE_CHANGE {
        if device_change_is_loss(wparam) {
            with_callback(|state| {
                state.shared.device_removed.store(true, Ordering::Release);
                state.shared.stop.stop(StopReason::NativeFailure);
                false
            });
        }
    } else if message == WM_DISPLAYCHANGE {
        latch_failure(CallbackFailure::DisplayChanged);
    } else if message == WM_POWERBROADCAST {
        latch_failure(CallbackFailure::PowerBroadcast(wparam));
    }
    // SAFETY: WM_INPUT requires default cleanup, and other messages use their original arguments.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Registration replays an arrival for every attached device; only a removal is a loss.
fn device_change_is_loss(wparam: usize) -> bool {
    wparam == GIDC_REMOVAL as usize
}

fn run(
    shared: Arc<Shared>,
    parts: CallbackParts,
    mut control: ControlReader,
    revocation: RevocationSignal,
    duration: Option<Duration>,
    started: mpsc::SyncSender<Result<(), NativeCaptureError>>,
    ownership: &mut RawCaptureOwnership,
) -> Result<(), NativeCaptureError> {
    CALLBACK_STOP.with(|slot| *slot.borrow_mut() = Some(shared.stop.clone()));
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(CallbackState {
            shared: shared.clone(),
            producer: parts.producer,
            physical: parts.physical,
            take_back: parts.take_back,
            lease: SuppressionLease::new(shared.generation, Duration::ZERO, shared.stop.clone()),
            route_remote: false,
            ignored: IgnoredInputCounts::default(),
            held_virtual_keys: [None; 256],
            unsupported_presses_seen: [false; 256],
            press_revision: 0,
        })
    });
    if let Some(error) = startup_environment_error(ordinary_desktop_is_active()) {
        shared.stop.stop(stop_reason(error));
        revocation.mark_revoked_without_wake();
        let _ = started.send(Err(error));
        clear_callback();
        return Err(error);
    }
    let mut resources = match Resources::install(ownership) {
        Ok(resources) => resources,
        Err(error) => {
            shared.stop.stop(StopReason::NativeFailure);
            revocation.mark_revoked_without_wake();
            let _ = started.send(Err(error));
            clear_callback();
            return Err(error);
        }
    };
    seed_initial_state();
    if let Some(error) = startup_completion_error(
        shared.stop.reason(),
        || startup_environment_error(ordinary_desktop_is_active()),
        || shared.stop.reason(),
    ) {
        shared.stop.stop(stop_reason(error));
        revocation.mark_revoked_without_wake();
        let _ = dispatch_revocation_wake(&revocation);
        let _ = started.send(Err(error));
        let cleanup = resources.close_with_retries();
        clear_callback();
        return cleanup.and(Err(error));
    }
    shared.progress_ms.store(
        shared
            .origin
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        Ordering::Release,
    );
    let _ = started.send(Ok(()));
    let mut source_loss_at = None;
    let mut last_ownership_check = Duration::ZERO;
    let mut revocation_wake_attempted = false;
    loop {
        shared.progress_ms.store(
            shared
                .origin
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            Ordering::Release,
        );
        latch_session_stop(&shared);
        if duration.is_some_and(|limit| shared.origin.elapsed() >= limit) {
            shared.stop.stop(StopReason::Requested);
        }
        let command = control.pending();
        let now = shared.origin.elapsed();
        if command.is_some()
            || now.saturating_sub(last_ownership_check) >= Duration::from_millis(30)
        {
            let owns_raw = ordinary_desktop_is_active()
                && registered_mouse().is_ok_and(|entry| {
                    entry.is_some_and(|device| owns_mouse_registration(&device, resources.hwnd))
                });
            if !owns_raw {
                shared.stop.stop(StopReason::NativeFailure);
            }
            last_ownership_check = now;
        }
        if let Some((revision, command)) = command {
            let mut completion = ControlCompletion::Failed;
            if !shared.stop.is_stopped() {
                completion = apply_control(revision, command);
            }
            if completion == ControlCompletion::Failed {
                shared.stop.stop(StopReason::NativeFailure);
            }
            control.complete(revision, completion);
        }
        let mut draining = false;
        with_callback(|state| {
            let now = shared.origin.elapsed();
            state.physical.tick(now, &shared.stop);
            state.remote();
            draining = state.physical.has_suppressed_presses();
            false
        });
        if shared.stop.is_stopped() {
            propagate_capture_stop(&shared, &mut revocation_wake_attempted);
            if shared.device_removed.load(Ordering::Acquire) {
                source_loss_at.get_or_insert_with(Instant::now);
            }
            let source_lost = source_loss_at
                .is_some_and(|start: Instant| start.elapsed() >= Duration::from_millis(250));
            if !draining || source_lost {
                break;
            }
        }
        let mut message = MSG::default();
        for _ in 0..64 {
            // SAFETY: message is writable and this pump owns its thread's windows and hooks.
            if unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                break;
            }
            if message.message == WM_QUIT {
                shared.stop.stop(StopReason::Requested);
                break;
            }
            // SAFETY: the message was initialized by PeekMessageW on this thread.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // SAFETY: zero handles waits only for messages, with a finite callback watchdog interval.
        if unsafe {
            MsgWaitForMultipleObjectsEx(0, ptr::null(), 5, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        } == u32::MAX
        {
            shared.stop.stop(StopReason::NativeFailure);
        }
    }
    let result = resources.close_with_retries();
    let mut callback = take_callback_until_available(&shared.stop);
    clear_callback_stop();
    release_injected_until_complete(&mut callback, &shared.stop);
    result
}

fn dispatch_revocation_wake(revocation: &RevocationSignal) -> bool {
    let signal = revocation.clone();
    thread::Builder::new()
        .name("monhop-revoke".into())
        .spawn(move || signal.revoke())
        .is_ok()
}

fn clear_callback() {
    CALLBACK.with(|slot| *slot.borrow_mut() = None);
    clear_callback_stop();
}

fn clear_callback_stop() {
    CALLBACK_FAILURE.set(None);
    CALLBACK_REENTRY.set(None);
    CALLBACK_STOP.with(|slot| *slot.borrow_mut() = None);
}

fn take_callback_until_available(stop: &CaptureStop) -> CallbackState {
    loop {
        if let Some(state) = CALLBACK.with(|slot| slot.try_borrow_mut().ok()?.take()) {
            return state;
        }
        stop.stop(StopReason::NativeFailure);
        thread::sleep(Duration::from_millis(25));
    }
}

fn release_injected_until_complete(state: &mut CallbackState, stop: &CaptureStop) {
    loop {
        match catch_unwind(AssertUnwindSafe(|| state.release_injected())) {
            Ok(Ok(())) => return,
            Ok(Err(_)) => {}
            Err(payload) => std::mem::forget(payload),
        }
        stop.stop(StopReason::NativeFailure);
        thread::sleep(Duration::from_millis(25));
    }
}

fn seed_initial_state() {
    with_callback(|state| {
        for vk in 8..=254 {
            if matches!(vk, 0x10..=0x12) {
                continue;
            }
            // SAFETY: these APIs read current keyboard state and map a virtual key without input effects.
            let held = unsafe { GetAsyncKeyState(vk) } < 0;
            if !held {
                continue;
            }
            // SAFETY: MAPVK_VK_TO_VSC_EX preserves the extended scan-code prefix.
            let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
            state.seed_held_key(vk as u32, scan);
        }
        for (vk, button) in [
            (1, MouseButton::Left),
            (2, MouseButton::Right),
            (4, MouseButton::Middle),
            (5, MouseButton::Back),
            (6, MouseButton::Forward),
        ] {
            // SAFETY: reads each mouse button's current held state without synthesizing input.
            if unsafe { GetAsyncKeyState(vk) } < 0 {
                state.physical.seed_locally_held_button(button);
            }
        }
        state
            .shared
            .ready
            .store(state.physical.is_ready_for_suppression(), Ordering::Release);
        false
    });
}

struct Resources<'a> {
    ownership: &'a mut RawCaptureOwnership,
    module: HINSTANCE,
    hwnd: HWND,
    keyboard: HHOOK,
    mouse: HHOOK,
    registered: bool,
    raw: bool,
    raw_state_uncertain: bool,
    previous_mouse: Option<RAWINPUTDEVICE>,
    cleanup_finalized: bool,
    cleanup_error: Option<NativeCaptureError>,
}

impl<'a> Resources<'a> {
    fn install(ownership: &'a mut RawCaptureOwnership) -> Result<Self, NativeCaptureError> {
        // SAFETY: a null module name obtains the current process module without loading code.
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        if module.is_null() {
            return Err(windows_error("GetModuleHandleW"));
        }
        let mut result = Self {
            ownership,
            module,
            hwnd: ptr::null_mut(),
            keyboard: ptr::null_mut(),
            mouse: ptr::null_mut(),
            registered: false,
            raw: false,
            raw_state_uncertain: false,
            previous_mouse: None,
            cleanup_finalized: false,
            cleanup_error: None,
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module,
            lpszClassName: CLASS_NAME.as_ptr(),
            ..Default::default()
        };
        // SAFETY: the class data and static callback remain valid for the registered class lifetime.
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(result.rollback(windows_error("RegisterClassW")));
        }
        result.registered = true;
        // SAFETY: creates an owned hidden top-level window to receive input and system-change messages.
        result.hwnd = unsafe {
            CreateWindowExW(
                0,
                CLASS_NAME.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                module,
                ptr::null(),
            )
        };
        if result.hwnd.is_null() {
            return Err(result.rollback(windows_error("CreateWindowExW")));
        }
        result.previous_mouse = match registered_mouse() {
            Ok(previous) => previous,
            Err(error) => return Err(result.rollback(error)),
        };
        if let Err(error) = register_mouse(RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
            hwndTarget: result.hwnd,
        }) {
            return Err(result.rollback(error));
        }
        result.raw = true;
        // SAFETY: callbacks have static lifetime and run on this dedicated message-loop thread.
        result.keyboard =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) };
        if result.keyboard.is_null() {
            return Err(result.rollback(windows_error("SetWindowsHookExW keyboard")));
        }
        // SAFETY: the low-level mouse callback has the same owned lifetime as the keyboard callback.
        result.mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
        if result.mouse.is_null() {
            return Err(result.rollback(windows_error("SetWindowsHookExW mouse")));
        }
        Ok(result)
    }

    fn rollback(&mut self, startup_error: NativeCaptureError) -> NativeCaptureError {
        if self.close_with_retries().is_err() {
            NativeCaptureError::CleanupPending
        } else {
            startup_error
        }
    }

    fn close_with_retries(&mut self) -> Result<(), NativeCaptureError> {
        if self.cleanup_finalized {
            return self.cleanup_error.map_or(Ok(()), Err);
        }
        let mut result = self.close_once();
        for _ in 1..CLOSE_ATTEMPTS {
            if result.is_ok() {
                break;
            }
            result = self.close_once();
        }
        self.cleanup_error = result.err();
        self.cleanup_finalized = true;
        if self.cleanup_error.is_some() {
            self.ownership.poison();
        }
        self.cleanup_error.map_or(Ok(()), Err)
    }

    fn close_once(&mut self) -> Result<(), NativeCaptureError> {
        let mut error = None;
        for hook in [&mut self.keyboard, &mut self.mouse] {
            if !hook.is_null() {
                // SAFETY: each hook handle belongs to this thread and is removed before callback state.
                if unsafe { UnhookWindowsHookEx(*hook) } == 0 {
                    error.get_or_insert_with(|| windows_error("UnhookWindowsHookEx"));
                } else {
                    *hook = ptr::null_mut();
                }
            }
        }
        if self.raw {
            match registered_mouse() {
                Ok(Some(current)) if current.hwndTarget == self.hwnd => {
                    let previous = self.previous_mouse.unwrap_or(RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 2,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: ptr::null_mut(),
                    });
                    if let Err(failure) = register_mouse(previous) {
                        error.get_or_insert(failure);
                    } else {
                        self.raw = false;
                    }
                }
                Ok(_) => {
                    self.raw = false;
                    self.raw_state_uncertain = true;
                    error.get_or_insert(NativeCaptureError::StartFailed);
                }
                Err(failure) => {
                    error.get_or_insert(failure);
                }
            }
        }
        if !self.raw && !self.hwnd.is_null() {
            // SAFETY: the window is owned by this thread and has no external state pointer.
            if unsafe { DestroyWindow(self.hwnd) } == 0 {
                error.get_or_insert_with(|| windows_error("DestroyWindow"));
            } else {
                self.hwnd = ptr::null_mut();
            }
        }
        if self.registered && self.hwnd.is_null() {
            // SAFETY: all windows of this class are gone before removing its registration.
            if unsafe { UnregisterClassW(CLASS_NAME.as_ptr(), self.module) } == 0 {
                error.get_or_insert_with(|| windows_error("UnregisterClassW"));
            } else {
                self.registered = false;
            }
        }
        if self.raw_state_uncertain {
            error.get_or_insert(NativeCaptureError::StartFailed);
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for Resources<'_> {
    fn drop(&mut self) {
        let _ = self.close_with_retries();
    }
}

fn owns_mouse_registration(device: &RAWINPUTDEVICE, window: HWND) -> bool {
    // GetRegisteredRawInputDevices can omit DEVNOTIFY from its readback. Still require
    // our exact window and input mode; notification delivery does not define ownership.
    !window.is_null()
        && device.hwndTarget == window
        && device.usUsagePage == 1
        && device.usUsage == 2
        && device.dwFlags & !RIDEV_DEVNOTIFY == RIDEV_INPUTSINK
}

fn registered_mouse() -> Result<Option<RAWINPUTDEVICE>, NativeCaptureError> {
    let mut count = 0;
    // SAFETY: the null buffer queries the required number of raw-input registrations.
    if unsafe {
        GetRegisteredRawInputDevices(
            ptr::null_mut(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    } == u32::MAX
    {
        return Err(windows_error("GetRegisteredRawInputDevices count"));
    }
    if count == 0 {
        return Ok(None);
    }
    if count > 4096 {
        return Err(NativeCaptureError::StartFailed);
    }
    let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
    // SAFETY: the buffer has room for count entries, and the native function receives that capacity.
    let read = unsafe {
        GetRegisteredRawInputDevices(
            devices.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if read == u32::MAX || read as usize > devices.len() {
        return Err(windows_error("GetRegisteredRawInputDevices"));
    }
    Ok(devices
        .into_iter()
        .take(read as usize)
        .find(|device| device.usUsagePage == 1 && device.usUsage == 2))
}

fn register_mouse(device: RAWINPUTDEVICE) -> Result<(), NativeCaptureError> {
    // SAFETY: the single initialized registration lives for this synchronous native call.
    if unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        Err(windows_error("RegisterRawInputDevices"))
    } else {
        Ok(())
    }
}

fn windows_error(operation: &'static str) -> NativeCaptureError {
    NativeCaptureError::Windows {
        operation,
        // SAFETY: reads the calling thread's last error immediately after the failing native call.
        code: unsafe { GetLastError() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback_fixture() -> (CallbackState, CaptureConsumer) {
        callback_fixture_with(None)
    }

    fn callback_fixture_with(take_back: Option<TakeBackGate>) -> (CallbackState, CaptureConsumer) {
        let stop = CaptureStop::default();
        let (producer, consumer) = capture_channel(stop.clone());
        let shared = Arc::new(Shared {
            origin: Instant::now(),
            generation: 1,
            stop: stop.clone(),
            ready: AtomicBool::new(true),
            progress_ms: AtomicU64::new(0),
            allows_suppression: true,
            device_removed: AtomicBool::new(false),
            revocation: RevocationSignal::default(),
        });
        let physical = match &take_back {
            Some(gate) => PhysicalCapture::new(Duration::ZERO).with_take_back(gate.clone()),
            None => PhysicalCapture::new(Duration::ZERO),
        };
        (
            CallbackState {
                shared,
                producer,
                physical,
                take_back,
                lease: SuppressionLease::new(1, Duration::ZERO, stop),
                route_remote: false,
                ignored: IgnoredInputCounts::default(),
                held_virtual_keys: [None; 256],
                unsupported_presses_seen: [false; 256],
                press_revision: 0,
            },
            consumer,
        )
    }

    fn raw_motion(has_device: bool, flags: u16) -> RAWINPUT {
        let mut raw = RAWINPUT::default();
        raw.header.dwType = RIM_TYPEMOUSE;
        raw.header.hDevice = if has_device {
            ptr::without_provenance_mut(1)
        } else {
            ptr::null_mut()
        };
        raw.data.mouse = windows_sys::Win32::UI::Input::RAWMOUSE {
            usFlags: flags,
            lLastX: 7,
            lLastY: -3,
            ..Default::default()
        };
        raw
    }

    #[test]
    fn unsupported_hook_events_neither_stop_nor_enter_the_physical_ledger() {
        let (mut state, mut consumer) = callback_fixture();
        for event in [
            decode_keyboard(WM_KEYDOWN, 0x13, 0x45, 0, 0),
            decode_keyboard(WM_KEYDOWN, 0, 0, 0, 0),
            decode_keyboard(WM_KEYDOWN, 0, 0x100, 0, 0),
            decode_keyboard(WM_KEYDOWN, 0, 0xff, 0, 0),
            decode_mouse(WM_XBUTTONDOWN, 0, 0, 3 << 16, 0, 0),
            decode_mouse(0xffff, 0, 0, 0, 0, 0),
        ] {
            assert!(!state.decoded(event));
        }
        assert_eq!(state.ignored.unsupported_events, 6);
        assert_eq!(state.shared.stop.reason(), None);
        assert!(consumer.try_pop().unwrap().is_none());
        assert!(state.physical.is_ready_for_suppression());
        assert!(state.shared.ready.load(Ordering::Acquire));
        assert!(!state.physical.has_suppressed_presses());
        assert_eq!(state.physical.handoff_plan(true).iter().len(), 0);
        assert!(!state.decoded(decode_keyboard(WM_KEYDOWN, 0, 0x1e, 0, 0)));
        assert!(matches!(
            consumer.try_pop().unwrap(),
            Some(CaptureEvent::Key {
                modifiers: monhop_core::ModifierState(0),
                repeat: false,
                ..
            })
        ));
    }

    #[test]
    fn malformed_hook_metadata_still_stops_capture() {
        let (mut state, _consumer) = callback_fixture();
        assert!(!state.decoded(decode_keyboard(WM_KEYUP, 0, 0x1e, 0, 0)));
        assert_eq!(state.shared.stop.reason(), Some(StopReason::InvalidInput));
        assert_eq!(state.ignored.unsupported_events, 0);
    }

    #[test]
    fn unsupported_raw_samples_are_counted_without_forwarding() {
        let (mut state, mut consumer) = callback_fixture();
        for (has_device, flags) in [(false, 0), (true, 1), (false, 1)] {
            state.raw_mouse(&raw_motion(has_device, flags));
            assert_eq!(state.shared.stop.reason(), None);
            assert!(consumer.try_pop().unwrap().is_none());
            assert!(state.physical.is_ready_for_suppression());
            assert!(!state.physical.has_suppressed_presses());
        }
        assert_eq!(state.ignored.unsourced_motion_samples, 3);
        state.raw_mouse(&raw_motion(true, 0));
        assert!(matches!(
            consumer.try_pop().unwrap(),
            Some(CaptureEvent::RelativeMotion { dx: 7, dy: -3 })
        ));
    }

    #[test]
    fn injected_input_is_excluded_from_unsupported_counts() {
        let (mut state, mut consumer) = callback_fixture();
        assert!(!state.decoded(decode_keyboard(WM_KEYUP, 0x13, 0, 0x10, 0)));
        assert!(!state.decoded(decode_mouse(WM_XBUTTONDOWN, 1, 0, 0, 0, 0)));
        let mut raw = raw_motion(false, 1);
        raw.data.mouse = windows_sys::Win32::UI::Input::RAWMOUSE {
            usFlags: 1,
            ulExtraInformation: MONHOP_INJECTED_MARKER as u32,
            lLastX: 7,
            ..Default::default()
        };
        state.raw_mouse(&raw);
        assert_eq!(state.ignored.unsupported_events, 0);
        assert_eq!(state.ignored.unsourced_motion_samples, 0);
        assert_eq!(state.shared.stop.reason(), None);
        assert!(consumer.try_pop().unwrap().is_none());
    }

    #[test]
    fn unmapped_initial_keys_do_not_seed_aliases_or_block_mapped_key_release() {
        let (mut state, mut consumer) = callback_fixture();
        for (vk, scan) in [(0x13, 0x45), (0x13, 0xe11d), (0, 0), (0, 0xff), (0, 0x101)] {
            state.seed_held_key(vk, scan);
        }
        assert_eq!(state.ignored.unmapped_held_keys, 5);
        assert!(state.physical.is_ready_for_suppression());
        assert_eq!(state.shared.stop.reason(), None);
        assert_eq!(state.physical.handoff_plan(true).iter().len(), 0);

        state.seed_held_key(0xa3, 0xe01d);
        assert!(!state.physical.is_ready_for_suppression());
        assert!(!state.decoded(DecodedInput::Unsupported));
        assert!(!state.physical.is_ready_for_suppression());
        assert!(!state.decoded(decode_keyboard(WM_KEYUP, 0xa3, 0x1d, 0x81, 0)));
        assert!(state.physical.is_ready_for_suppression());
        assert!(consumer.try_pop().unwrap().is_none());
        assert_eq!(state.shared.stop.reason(), None);
    }

    #[test]
    fn supported_presses_remain_balanced_around_unsupported_input() {
        for (down, up) in [
            (
                decode_keyboard(WM_KEYDOWN, 0, 0x1d, 0, 0),
                decode_keyboard(WM_KEYUP, 0, 0x1d, 0x80, 0),
            ),
            (
                decode_mouse(WM_LBUTTONDOWN, 0, 0, 0, 0, 0),
                decode_mouse(WM_LBUTTONUP, 0, 0, 0, 0, 0),
            ),
        ] {
            let (mut state, mut consumer) = callback_fixture();
            state.publish_route(1, true).unwrap();
            assert!(matches!(
                consumer.try_pop().unwrap(),
                Some(CaptureEvent::RouteChanged {
                    remote: true,
                    revision: 1
                })
            ));
            let (DecodedInput::Event(down), DecodedInput::Event(up)) = (down, up) else {
                panic!("expected supported press and release");
            };
            assert!(state.physical.process(
                down,
                true,
                Duration::ZERO,
                &mut state.producer,
                &state.shared.stop
            ));
            assert!(state.physical.has_suppressed_presses());
            assert!(consumer.try_pop().unwrap().is_some());
            assert!(!state.decoded(DecodedInput::Unsupported));
            state.raw_mouse(&raw_motion(false, 0));
            assert!(state.physical.has_suppressed_presses());
            assert!(consumer.try_pop().unwrap().is_none());
            // A release still belongs to its suppressed press after routing returns local.
            state.publish_route(2, false).unwrap();
            assert!(matches!(
                consumer.try_pop().unwrap(),
                Some(CaptureEvent::RouteChanged {
                    remote: false,
                    revision: 2
                })
            ));
            assert!(state.physical.process(
                up,
                false,
                Duration::ZERO,
                &mut state.producer,
                &state.shared.stop
            ));
            assert!(!state.physical.has_suppressed_presses());
            assert!(matches!(
                consumer.try_pop().unwrap(),
                Some(
                    CaptureEvent::Key {
                        pressed: false,
                        modifiers: monhop_core::ModifierState(0),
                        ..
                    } | CaptureEvent::Button { pressed: false, .. }
                )
            ));
            assert_eq!(state.shared.stop.reason(), None);
        }
    }

    #[test]
    fn emergency_escape_restores_local_motion_after_unsupported_raw_input() {
        let (mut state, _consumer) = callback_fixture();
        state.publish_route(1, true).unwrap();
        state.raw_mouse(&raw_motion(false, 1));
        for usage in [0xe0, 0xe4, 0x29] {
            let event = CaptureEvent::Key {
                usage: monhop_core::HidUsage(usage),
                pressed: true,
                repeat: false,
                modifiers: monhop_core::ModifierState(0),
            };
            assert!(state.physical.process(
                event,
                true,
                Duration::ZERO,
                &mut state.producer,
                &state.shared.stop
            ));
        }
        let now = monhop_core::EmergencyEscape::HOLD_DURATION;
        state.physical.tick(now, &state.shared.stop);
        assert_eq!(
            state.shared.stop.reason(),
            Some(StopReason::EmergencyEscape)
        );
        assert!(!state.physical.process(
            CaptureEvent::AbsoluteMotion { x: 12, y: 34 },
            true,
            now,
            &mut state.producer,
            &state.shared.stop
        ));
        for usage in [0xe0, 0xe4, 0x29] {
            assert!(state.physical.process(
                CaptureEvent::Key {
                    usage: monhop_core::HidUsage(usage),
                    pressed: false,
                    repeat: false,
                    modifiers: monhop_core::ModifierState(0)
                },
                true,
                now,
                &mut state.producer,
                &state.shared.stop
            ));
        }
        assert!(!state.physical.has_suppressed_presses());
    }

    #[test]
    fn unsupported_physical_release_recovers_only_its_tracked_press() {
        for scan in [0, 0xff, 0x100] {
            let (mut state, mut consumer) = callback_fixture();
            state.publish_route(1, true).unwrap();
            consumer.try_pop().unwrap();
            state
                .lease
                .renew(1, Duration::ZERO, MAX_SUPPRESSION_TTL)
                .unwrap();
            assert!(state.keyboard(WM_KEYDOWN, 0xa2, 0x1d, 0, 0));
            assert!(state.physical.has_suppressed_presses());
            assert!(matches!(
                consumer.try_pop().unwrap(),
                Some(CaptureEvent::Key { pressed: true, .. })
            ));

            assert!(state.keyboard(WM_KEYUP, 0xa2, scan, 0x80, 0));
            assert!(!state.physical.has_suppressed_presses());
            assert!(matches!(
                consumer.try_pop().unwrap(),
                Some(CaptureEvent::Key {
                    usage: monhop_core::HidUsage(0xe0),
                    pressed: false,
                    modifiers: monhop_core::ModifierState(0),
                    ..
                })
            ));
            assert!(!state.keyboard(WM_KEYUP, 0xa2, scan, 0x80, 0));
            assert!(consumer.try_pop().unwrap().is_none());
            assert_eq!(state.shared.stop.reason(), None);
            assert_eq!(state.ignored.recovered_releases, 1);
            assert_eq!(state.ignored.unsupported_events, 1);
            assert!(state.held_virtual_keys.iter().all(Option::is_none));
        }
    }

    #[test]
    fn injected_releases_cannot_clear_a_physical_press() {
        let (mut state, mut consumer) = callback_fixture();
        assert!(!state.keyboard(WM_KEYDOWN, 0x41, 0x1e, 0, 0));
        consumer.try_pop().unwrap();
        for (flags, marker) in [(0x90, 0), (0x82, 0), (0x80, MONHOP_INJECTED_MARKER)] {
            assert!(!state.keyboard(WM_KEYUP, 0x41, 0, flags, marker));
            assert_eq!(state.held_virtual_keys[4], Some(0x41));
            assert!(consumer.try_pop().unwrap().is_none());
        }
        assert_eq!(state.ignored.recovered_releases, 0);
        assert_eq!(state.ignored.unsupported_events, 0);
        assert!(!state.keyboard(WM_KEYUP, 0x41, 0, 0x80, 0));
        assert_eq!(state.ignored.recovered_releases, 1);
        assert!(state.held_virtual_keys.iter().all(Option::is_none));
    }

    #[test]
    fn initially_held_key_can_recover_its_release_without_forwarding() {
        let (mut state, mut consumer) = callback_fixture();
        state.seed_held_key(0xa3, 0xe01d);
        assert!(!state.physical.is_ready_for_suppression());
        assert!(!state.keyboard(WM_KEYUP, 0xa3, 0, 0x80, 0));
        assert!(state.physical.is_ready_for_suppression());
        assert!(consumer.try_pop().unwrap().is_none());
        assert_eq!(state.ignored.recovered_releases, 1);
        assert_eq!(state.shared.stop.reason(), None);
    }

    #[test]
    fn ambiguous_virtual_key_release_stops_without_guessing_which_key() {
        let (mut state, mut consumer) = callback_fixture();
        assert!(!state.keyboard(WM_KEYDOWN, 0x0d, 0x1c, 0, 0));
        assert!(!state.keyboard(WM_KEYDOWN, 0x0d, 0x1c, 1, 0));
        consumer.try_pop().unwrap();
        consumer.try_pop().unwrap();
        assert!(!state.keyboard(WM_KEYUP, 0x0d, 0, 0x80, 0));
        assert_eq!(state.shared.stop.reason(), Some(StopReason::InvalidInput));
        assert_eq!(state.ignored.recovered_releases, 0);
        assert_eq!(
            state
                .held_virtual_keys
                .iter()
                .filter(|key| **key == Some(0x0d))
                .count(),
            2
        );
    }

    #[test]
    fn unmapped_press_or_initial_key_prevents_releasing_a_different_physical_key() {
        for initial in [false, true] {
            let (mut state, mut consumer) = callback_fixture();
            if initial {
                state.seed_held_key(0x0d, 0);
            } else {
                assert!(!state.keyboard(WM_KEYDOWN, 0x0d, 0, 0, 0));
                assert!(!state.keyboard(WM_KEYUP, 0x0d, 0, 0x80, 0));
            }
            // A prior unsupported release cannot prove that all overlapping presses are up.
            assert!(!state.keyboard(WM_KEYDOWN, 0x0d, 0x1c, 0, 0));
            consumer.try_pop().unwrap();
            assert!(!state.keyboard(WM_KEYDOWN, 0x0d, 0, 0, 0));
            assert!(!state.keyboard(WM_KEYUP, 0x0d, 0, 0x80, 0));
            assert_eq!(state.shared.stop.reason(), Some(StopReason::InvalidInput));
            assert_eq!(state.held_virtual_keys[0x28], Some(0x0d));
            assert_eq!(state.ignored.recovered_releases, 0);
        }
    }

    #[test]
    fn mouse_ownership_accepts_windows_readback_without_notification_flag() {
        let window = std::ptr::without_provenance_mut(1);
        for flags in [RIDEV_INPUTSINK, RIDEV_INPUTSINK | RIDEV_DEVNOTIFY] {
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: flags,
                hwndTarget: window,
            };
            assert!(owns_mouse_registration(&device, window));
        }
    }

    #[test]
    fn mouse_ownership_rejects_missing_sink_unexpected_flags_and_other_targets() {
        let window = std::ptr::without_provenance_mut(1);
        let device = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        for flags in [0, RIDEV_DEVNOTIFY, RIDEV_REMOVE, 0x1000, 0x8000_0100, 0x130] {
            assert!(!owns_mouse_registration(
                &RAWINPUTDEVICE {
                    dwFlags: flags,
                    ..device
                },
                window
            ));
        }
        assert!(!owns_mouse_registration(
            &device,
            std::ptr::without_provenance_mut(2)
        ));
        assert!(!owns_mouse_registration(
            &RAWINPUTDEVICE {
                hwndTarget: ptr::null_mut(),
                ..device
            },
            window
        ));
        assert!(!owns_mouse_registration(
            &RAWINPUTDEVICE {
                hwndTarget: ptr::null_mut(),
                ..device
            },
            ptr::null_mut()
        ));
        assert!(!owns_mouse_registration(
            &RAWINPUTDEVICE {
                usUsage: 6,
                ..device
            },
            window
        ));
        assert!(!owns_mouse_registration(
            &RAWINPUTDEVICE {
                usUsagePage: 2,
                ..device
            },
            window
        ));
    }

    #[test]
    fn local_cursor_coordinates_floor_and_stay_within_the_virtual_desktop() {
        let desktop = VirtualDesktop::new(-100, -200, 1_000, 800).expect("valid desktop");

        assert_eq!(
            local_cursor_coordinates(Point::new(-99.1, -199.1), desktop),
            Ok((0, 0))
        );
        assert_eq!(
            local_cursor_coordinates(Point::new(900.0, -200.0), desktop),
            Err(NativeCaptureError::Stopped(StopReason::InvalidInput))
        );
        assert_eq!(
            local_cursor_coordinates(Point::new(f64::NAN, -200.0), desktop),
            Err(NativeCaptureError::Stopped(StopReason::InvalidInput))
        );
    }

    #[test]
    fn device_arrival_replays_do_not_end_capture() {
        assert!(!device_change_is_loss(GIDC_ARRIVAL as usize));
        assert!(device_change_is_loss(GIDC_REMOVAL as usize));
    }

    #[test]
    fn capture_failure_stops_input_immediately_but_allows_the_session_close() {
        let (mut state, mut consumer) = callback_fixture();
        state
            .lease
            .renew(1, Duration::ZERO, MAX_SUPPRESSION_TTL)
            .unwrap();
        assert!(state.remote());
        let mut wake_attempted = false;
        propagate_capture_stop(&state.shared, &mut wake_attempted);
        assert!(!state.shared.revocation.is_stopping());

        state.shared.stop.stop(StopReason::NativeFailure);
        propagate_capture_stop(&state.shared, &mut wake_attempted);

        assert!(!state.remote());
        assert_eq!(consumer.try_pop().unwrap_err(), StopReason::NativeFailure);
        assert!(state.shared.revocation.is_stopping());
        assert!(!state.shared.revocation.is_revoked());
        // Repeated native-loop passes cannot cut short the transport's close grace.
        propagate_capture_stop(&state.shared, &mut wake_attempted);
        assert!(!state.shared.revocation.is_revoked());
        assert!(!wake_attempted);
        state.shared.revocation.revoke();
        assert!(state.shared.revocation.is_revoked());
    }

    #[test]
    fn injected_callback_input_never_takes_back() {
        let floor = monhop_core::SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), monhop_core::FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        let (mut state, mut consumer) = callback_fixture_with(Some(gate.clone()));

        for (flags, marker) in [(0x10, 0), (0x02, 0), (0, MONHOP_INJECTED_MARKER)] {
            assert!(is_injected_keyboard(flags, marker));
            assert!(!state.keyboard(WM_KEYDOWN, 0x41, 0x1e, flags, marker));
        }
        for (flags, marker) in [(0x01, 0), (0x02, 0), (0, MONHOP_INJECTED_MARKER)] {
            assert!(is_injected_mouse(flags, marker));
            assert!(!state.decoded(decode_mouse(WM_LBUTTONDOWN, flags, marker, 0, 0, 0)));
        }
        assert!(!is_injected_keyboard(0, 0) && !is_injected_mouse(0, 0));
        let mut marked = raw_motion(true, 0);
        marked.data.mouse = windows_sys::Win32::UI::Input::RAWMOUSE {
            ulExtraInformation: MONHOP_INJECTED_MARKER as u32,
            lLastX: 40,
            ..Default::default()
        };
        for raw in [marked, raw_motion(false, 0)] {
            state.raw_mouse(&raw);
        }
        assert_eq!(gate.take_triggered(), None);
        assert_eq!(gate.floor().snapshot(), receiving);
        assert!(consumer.try_pop().unwrap().is_none());

        state.raw_mouse(&raw_motion(true, 0));
        assert!(
            gate.take_triggered().is_some(),
            "physical relative motion past the threshold takes back"
        );
    }

    fn receiving_gate_holding_injected_input() -> TakeBackGate {
        let floor = monhop_core::SharedFloor::new();
        let receiving = floor
            .transition(floor.snapshot(), FloorState::Receiving)
            .unwrap();
        let gate = TakeBackGate::new(floor);
        gate.open_injection(receiving.generation);
        gate.note_injected_press();
        gate
    }

    fn physical_move(x: i32) -> DecodedInput {
        decode_mouse(WM_MOUSEMOVE, 0, 0, 0, x, 100)
    }

    #[test]
    fn hook_withholds_the_movement_that_takes_back_while_injected_input_is_down() {
        let gate = receiving_gate_holding_injected_input();
        let (mut state, mut consumer) = callback_fixture_with(Some(gate.clone()));
        let at = |x| move || Some(Point::new(x, 100.0));
        assert!(
            !state.hook_mouse(physical_move(904), at(900.0)),
            "sub-threshold travel is delivered"
        );
        assert_eq!(gate.take_triggered(), None);
        assert!(
            state.hook_mouse(physical_move(907), at(904.0)),
            "the movement that takes back is withheld before delivery"
        );
        assert!(gate.take_triggered().is_some());
        assert!(state.hook_mouse(physical_move(908), || {
            panic!("the cursor is read only while take-back can fire")
        }));
        assert_eq!(state.shared.stop.reason(), None);
        let positions = std::iter::from_fn(|| consumer.try_pop().unwrap())
            .filter(|event| matches!(event, CaptureEvent::AbsoluteMotion { .. }))
            .count();
        assert_eq!(positions, 3, "hook positions still reach local routing");
    }

    #[test]
    fn injected_travel_between_physical_moves_never_takes_back() {
        let gate = receiving_gate_holding_injected_input();
        let (mut state, _consumer) = callback_fixture_with(Some(gate.clone()));
        assert!(!state.hook_mouse(physical_move(100), || Some(Point::new(99.0, 100.0))));
        // The peer's injected motion moved the cursor 800 units before the next physical nudge.
        assert!(!state.hook_mouse(physical_move(901), || Some(Point::new(900.0, 100.0))));
        assert!(!state.hook_mouse(physical_move(902), || None));
        assert_eq!(gate.take_triggered(), None);
    }

    #[test]
    fn route_barriers_carry_the_floor_generation() {
        let gate = TakeBackGate::new(monhop_core::SharedFloor::new());
        let (mut state, mut consumer) = callback_fixture_with(Some(gate.clone()));
        state.publish_route(1, true).unwrap();
        let barrier = consumer.try_pop_tagged().unwrap().unwrap();
        assert!(matches!(barrier.event, CaptureEvent::RouteChanged { .. }));
        assert_eq!(barrier.floor_generation, gate.floor().snapshot().generation);

        let (mut ungated, mut consumer) = callback_fixture();
        ungated.publish_route(1, true).unwrap();
        assert_eq!(
            consumer.try_pop_tagged().unwrap().unwrap().floor_generation,
            0
        );
    }

    #[test]
    fn callbacks_observe_session_stop_before_the_next_worker_iteration() {
        let (mut state, mut consumer) = callback_fixture();
        state.publish_route(1, true).unwrap();
        consumer.try_pop().unwrap();
        state
            .lease
            .renew(1, Duration::ZERO, MAX_SUPPRESSION_TTL)
            .unwrap();
        assert!(state.keyboard(WM_KEYDOWN, 0xa2, 0x1d, 0, 0));
        consumer.try_pop().unwrap();

        state.shared.revocation.request_stop();
        assert!(!state.shared.stop.is_stopped());
        assert!(!state.event(CaptureEvent::AbsoluteMotion { x: 12, y: 34 }));
        assert_eq!(state.shared.stop.reason(), Some(StopReason::Requested));
        assert_eq!(consumer.try_pop().unwrap_err(), StopReason::Requested);
        // The release of an already-suppressed press still drains locally after the stop.
        assert!(state.keyboard(WM_KEYUP, 0xa2, 0x1d, 0x80, 0));
        assert!(!state.physical.has_suppressed_presses());
        assert!(!state.shared.revocation.is_revoked());
    }

    #[test]
    fn pending_control_observes_session_stop_before_applying_a_route() {
        let (state, _consumer) = callback_fixture();
        state.shared.revocation.request_stop();
        let _guard = install_callback(state);
        assert_eq!(
            apply_control_with(
                1,
                CaptureCommand::ActivateRemote {
                    deadline: MAX_SUPPRESSION_TTL
                },
                |_| panic!("stopped control must not place the cursor"),
                |_| panic!("stopped control must not transfer input"),
            ),
            ControlCompletion::Failed
        );
        with_callback(|state| {
            assert_eq!(state.shared.stop.reason(), Some(StopReason::Requested));
            assert!(!state.route_remote);
            assert!(!state.remote());
            false
        });
    }

    struct CallbackGuard;
    impl Drop for CallbackGuard {
        fn drop(&mut self) {
            clear_callback();
        }
    }

    fn install_callback(state: CallbackState) -> CallbackGuard {
        CALLBACK_STOP.with(|slot| *slot.borrow_mut() = Some(state.shared.stop.clone()));
        CALLBACK.with(|slot| *slot.borrow_mut() = Some(state));
        CallbackGuard
    }

    fn remote_callback() -> (CallbackState, CaptureConsumer) {
        let (mut state, mut consumer) = callback_fixture();
        state
            .renew_until(MAX_SUPPRESSION_TTL, Duration::ZERO)
            .unwrap();
        state.publish_route(1, true).unwrap();
        consumer.try_pop().unwrap();
        (state, consumer)
    }

    #[test]
    fn fast_physical_motion_during_cursor_placement_precedes_the_route_barrier() {
        let (state, mut consumer) = remote_callback();
        let stop = state.shared.stop.clone();
        let _guard = install_callback(state);
        assert_eq!(
            apply_control_with(
                2,
                CaptureCommand::LocalAt(Point::new(20.0, 30.0)),
                |_| {
                    assert_eq!(CALLBACK_SITE.get(), None);
                    for x in [3, -500, -2400, -2560] {
                        assert!(with_callback(
                            |state| state.event(CaptureEvent::AbsoluteMotion { x, y: 300 })
                        ));
                    }
                    Ok(())
                },
                |_| panic!("no held input")
            ),
            ControlCompletion::Applied
        );
        for x in [3, -500, -2400, -2560] {
            let event = consumer.try_pop_tagged().unwrap().unwrap();
            assert_eq!(event.event, CaptureEvent::AbsoluteMotion { x, y: 300 });
            assert_eq!(event.routing_revision, 1);
            assert!(event.remote);
        }
        assert_eq!(
            consumer.try_pop().unwrap(),
            Some(CaptureEvent::RouteChanged {
                remote: false,
                revision: 2
            })
        );
        assert!(!with_callback(
            |state| state.event(CaptureEvent::AbsoluteMotion { x: 21, y: 30 })
        ));
        let event = consumer.try_pop_tagged().unwrap().unwrap();
        assert_eq!(event.routing_revision, 2);
        assert!(!event.remote);
        assert_eq!(stop.reason(), None);
        assert_eq!(CALLBACK_FAILURE.get(), None);
    }

    #[test]
    fn physical_release_during_local_restore_balances_the_injected_press() {
        for down in [
            CaptureEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            },
            CaptureEvent::Key {
                usage: monhop_core::HidUsage(0xe0),
                pressed: true,
                repeat: false,
                modifiers: monhop_core::ModifierState(1),
            },
        ] {
            let (mut state, mut consumer) = remote_callback();
            assert!(state.event(down));
            consumer.try_pop().unwrap();
            let stop = state.shared.stop.clone();
            let _guard = install_callback(state);
            let mut effects = Vec::new();
            assert_eq!(
                apply_control_with(
                    2,
                    CaptureCommand::LocalAt(Point::new(20.0, 30.0)),
                    |_| Ok(()),
                    |effect| {
                        assert_eq!(CALLBACK_SITE.get(), None);
                        effects.push(effect);
                        if effects.len() == 1 {
                            let up = match down {
                                CaptureEvent::Button { button, .. } => CaptureEvent::Button {
                                    button,
                                    pressed: false,
                                },
                                CaptureEvent::Key { usage, .. } => CaptureEvent::Key {
                                    usage,
                                    pressed: false,
                                    repeat: false,
                                    modifiers: monhop_core::ModifierState(0),
                                },
                                _ => unreachable!(),
                            };
                            assert!(with_callback(|state| state.event(up)));
                        }
                        Ok(())
                    }
                ),
                ControlCompletion::Applied
            );
            assert_eq!(effects.len(), 2);
            assert!(matches!(
                effects[0],
                LocalTransfer::Key { pressed: true, .. }
                    | LocalTransfer::Button { pressed: true, .. }
            ));
            assert!(matches!(
                effects[1],
                LocalTransfer::Key { pressed: false, .. }
                    | LocalTransfer::Button { pressed: false, .. }
            ));
            let up = consumer.try_pop_tagged().unwrap().unwrap();
            assert_eq!(up.routing_revision, 1);
            assert!(up.remote);
            assert_eq!(
                consumer.try_pop().unwrap(),
                Some(CaptureEvent::RouteChanged {
                    remote: false,
                    revision: 2
                })
            );
            with_callback(|state| {
                assert_eq!(state.physical.release_injected_plan().iter().len(), 0);
                assert_eq!(state.physical.handoff_plan(false).iter().len(), 0);
                assert!(!state.physical.has_suppressed_presses());
                false
            });
            assert_eq!(stop.reason(), None);
        }
    }

    #[test]
    fn remote_handoff_replans_physical_presses_before_publishing_its_barrier() {
        let (mut state, mut consumer) = callback_fixture();
        assert!(!state.event(CaptureEvent::Button {
            button: MouseButton::Left,
            pressed: true
        }));
        consumer.try_pop().unwrap();
        let _guard = install_callback(state);
        let mut effects = Vec::new();
        assert_eq!(
            apply_control_with(
                1,
                CaptureCommand::ActivateRemote {
                    deadline: MAX_SUPPRESSION_TTL
                },
                |_| panic!("no cursor placement"),
                |effect| {
                    assert_eq!(CALLBACK_SITE.get(), None);
                    effects.push(effect);
                    if effects.len() == 1 {
                        assert!(!with_callback(|state| state.event(CaptureEvent::Button {
                            button: MouseButton::Left,
                            pressed: false
                        })));
                        assert!(!with_callback(|state| state.event(CaptureEvent::Button {
                            button: MouseButton::Right,
                            pressed: true
                        })));
                    }
                    Ok(())
                }
            ),
            ControlCompletion::Applied
        );
        assert_eq!(
            effects,
            vec![
                LocalTransfer::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
                LocalTransfer::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
                LocalTransfer::Button {
                    button: MouseButton::Right,
                    pressed: false
                }
            ]
        );
        for _ in 0..2 {
            let event = consumer.try_pop_tagged().unwrap().unwrap();
            assert_eq!(event.routing_revision, 0);
            assert!(!event.remote);
        }
        assert_eq!(
            consumer.try_pop().unwrap(),
            Some(CaptureEvent::RouteChanged {
                remote: true,
                revision: 1
            })
        );
        assert!(with_callback(|state| state.event(CaptureEvent::Button {
            button: MouseButton::Right,
            pressed: false
        })));
    }

    #[test]
    fn release_and_repress_during_native_send_cannot_leave_a_local_button_down() {
        for inject_first in [false, true] {
            let (mut state, _consumer) = callback_fixture();
            assert!(!state.event(CaptureEvent::Button {
                button: MouseButton::Left,
                pressed: true
            }));
            let _guard = install_callback(state);
            let mut local_down = true;
            let mut sends = 0;
            assert_eq!(
                apply_control_with(
                    1,
                    CaptureCommand::ActivateRemote {
                        deadline: MAX_SUPPRESSION_TTL
                    },
                    |_| panic!("no cursor placement"),
                    |effect| {
                        assert_eq!(
                            effect,
                            LocalTransfer::Button {
                                button: MouseButton::Left,
                                pressed: false
                            }
                        );
                        sends += 1;
                        if inject_first {
                            local_down = false;
                        }
                        if sends == 1 {
                            for pressed in [false, true] {
                                let suppressed = with_callback(|state| {
                                    state.event(CaptureEvent::Button {
                                        button: MouseButton::Left,
                                        pressed,
                                    })
                                });
                                if !suppressed {
                                    local_down = pressed;
                                }
                            }
                        }
                        if !inject_first {
                            local_down = false;
                        }
                        Ok(())
                    }
                ),
                ControlCompletion::Applied
            );
            assert_eq!(sends, 2);
            assert!(
                !local_down,
                "the local application must receive a release after the racing press"
            );
            assert!(with_callback(|state| state.event(CaptureEvent::Button {
                button: MouseButton::Left,
                pressed: false
            })));
        }
    }

    #[test]
    fn failed_release_retry_retains_the_obligation_to_release_local_input() {
        let (mut state, _consumer) = callback_fixture();
        assert!(!state.event(CaptureEvent::Button {
            button: MouseButton::Left,
            pressed: true
        }));
        let _guard = install_callback(state);
        let mut sends = 0;
        assert_eq!(
            apply_control_with(
                1,
                CaptureCommand::ActivateRemote {
                    deadline: MAX_SUPPRESSION_TTL
                },
                |_| panic!("no cursor placement"),
                |_| {
                    sends += 1;
                    if sends == 1 {
                        for pressed in [false, true] {
                            with_callback(|state| {
                                state.event(CaptureEvent::Button {
                                    button: MouseButton::Left,
                                    pressed,
                                })
                            });
                        }
                        Ok(())
                    } else {
                        Err(NativeCaptureError::StartFailed)
                    }
                }
            ),
            ControlCompletion::Failed
        );
        with_callback(|state| {
            assert!(!state.route_remote);
            assert!(!state.remote());
            assert_eq!(
                state
                    .physical
                    .release_injected_plan()
                    .iter()
                    .collect::<Vec<_>>(),
                vec![LocalTransfer::Button {
                    button: MouseButton::Left,
                    pressed: false
                }]
            );
            false
        });
    }

    #[test]
    fn stop_compensation_retries_a_physical_repress_after_the_injected_release() {
        let (mut state, _consumer) = remote_callback();
        assert!(state.event(CaptureEvent::Button {
            button: MouseButton::Left,
            pressed: true
        }));
        let signal = state.shared.revocation.clone();
        let _guard = install_callback(state);
        let mut local_down = false;
        let mut sends = 0;
        assert_eq!(
            apply_control_with(
                2,
                CaptureCommand::LocalAt(Point::new(20.0, 30.0)),
                |_| Ok(()),
                |effect| {
                    sends += 1;
                    local_down = matches!(effect, LocalTransfer::Button { pressed: true, .. });
                    signal.request_stop();
                    if sends == 2 {
                        for pressed in [false, true] {
                            if !with_callback(|state| {
                                state.event(CaptureEvent::Button {
                                    button: MouseButton::Left,
                                    pressed,
                                })
                            }) {
                                local_down = pressed;
                            }
                        }
                    }
                    Ok(())
                }
            ),
            ControlCompletion::Failed
        );
        assert_eq!(sends, 3);
        assert!(!local_down);
        with_callback(|state| {
            assert_eq!(state.physical.release_injected_plan().iter().len(), 0);
            assert!(!state.remote());
            false
        });
    }

    #[test]
    fn stop_during_cursor_placement_cannot_publish_a_new_route() {
        let (state, _consumer) = remote_callback();
        let signal = state.shared.revocation.clone();
        let _guard = install_callback(state);
        assert_eq!(
            apply_control_with(
                2,
                CaptureCommand::LocalAt(Point::new(20.0, 30.0)),
                |_| {
                    signal.request_stop();
                    Ok(())
                },
                |_| panic!("stopped control must not transfer input")
            ),
            ControlCompletion::Failed
        );
        with_callback(|state| {
            assert!(state.route_remote);
            assert!(!state.remote());
            assert_eq!(state.shared.stop.reason(), Some(StopReason::Requested));
            false
        });
    }

    #[test]
    fn stop_during_transfer_balances_down_and_retains_failed_cleanup() {
        for fail_compensation in [false, true] {
            let (mut state, _consumer) = remote_callback();
            assert!(state.event(CaptureEvent::Button {
                button: MouseButton::Left,
                pressed: true
            }));
            let signal = state.shared.revocation.clone();
            let _guard = install_callback(state);
            let mut sent = 0;
            assert_eq!(
                apply_control_with(
                    2,
                    CaptureCommand::LocalAt(Point::new(20.0, 30.0)),
                    |_| Ok(()),
                    |effect| {
                        sent += 1;
                        signal.request_stop();
                        if sent == 2 {
                            assert_eq!(
                                effect,
                                LocalTransfer::Button {
                                    button: MouseButton::Left,
                                    pressed: false
                                }
                            );
                            if fail_compensation {
                                return Err(NativeCaptureError::StartFailed);
                            }
                        }
                        Ok(())
                    }
                ),
                ControlCompletion::Failed
            );
            assert_eq!(sent, 2);
            with_callback(|state| {
                assert!(state.route_remote);
                assert!(!state.remote());
                assert_eq!(
                    state.physical.release_injected_plan().iter().len(),
                    usize::from(fail_compensation)
                );
                false
            });
        }
    }

    #[test]
    fn hard_revocation_wakes_observers_before_held_input_finishes_draining() {
        struct Notify(mpsc::SyncSender<()>);
        impl std::task::Wake for Notify {
            fn wake(self: Arc<Self>) {
                let _ = self.0.try_send(());
            }
        }
        let (mut state, mut consumer) = callback_fixture();
        state.publish_route(1, true).unwrap();
        consumer.try_pop().unwrap();
        state
            .lease
            .renew(1, Duration::ZERO, MAX_SUPPRESSION_TTL)
            .unwrap();
        assert!(state.keyboard(WM_KEYDOWN, 0xa2, 0x1d, 0, 0));
        let (tx, rx) = mpsc::sync_channel(1);
        let waker = std::task::Waker::from(Arc::new(Notify(tx)));
        assert!(
            state
                .shared
                .revocation
                .poll_revoked(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );

        state.shared.stop.stop(StopReason::NativeFailure);
        state.shared.revocation.mark_revoked_without_wake();
        let mut wake_attempted = false;
        propagate_capture_stop(&state.shared, &mut wake_attempted);
        rx.recv_timeout(Duration::from_secs(1))
            .expect("hard stop must wake without waiting for release");
        assert!(wake_attempted);
        assert!(state.physical.has_suppressed_presses());
        assert!(state.keyboard(WM_KEYUP, 0xa2, 0x1d, 0x80, 0));
        assert!(!state.physical.has_suppressed_presses());
    }

    #[test]
    fn callback_reentry_stops_capture_and_records_the_first_cause() {
        let (state, _consumer) = callback_fixture();
        let stop = state.shared.stop.clone();
        CALLBACK_STOP.with(|slot| *slot.borrow_mut() = Some(stop.clone()));
        CALLBACK.with(|slot| *slot.borrow_mut() = Some(state));
        assert!(!with_callback(|_| with_callback(|_| true)));
        assert_eq!(stop.reason(), Some(StopReason::NativeFailure));
        assert_eq!(
            CALLBACK_FAILURE.get(),
            Some(CallbackFailure::ReentrantCallback)
        );
        let first_origin = stop.origin().unwrap();
        latch_failure(CallbackFailure::PowerBroadcast(4));
        assert_eq!(
            CALLBACK_FAILURE.get(),
            Some(CallbackFailure::ReentrantCallback)
        );
        assert_eq!(stop.origin(), Some(first_origin));
        clear_callback();
        assert_eq!(CALLBACK_FAILURE.get(), None);
    }

    #[test]
    fn callback_panics_stop_capture_without_crossing_the_native_boundary() {
        let (state, _consumer) = callback_fixture();
        let stop = state.shared.stop.clone();
        CALLBACK_STOP.with(|slot| *slot.borrow_mut() = Some(stop.clone()));
        CALLBACK.with(|slot| *slot.borrow_mut() = Some(state));
        assert!(!with_callback(|_| panic!(
            "simulated native callback failure"
        )));
        assert_eq!(stop.reason(), Some(StopReason::NativeFailure));
        assert_eq!(
            CALLBACK_FAILURE.get(),
            Some(CallbackFailure::CallbackPanicked)
        );
        clear_callback();
    }

    #[test]
    fn callback_failure_origin_names_the_trigger_instead_of_the_shared_latch() {
        let stop = CaptureStop::default();
        CALLBACK_STOP.with(|slot| *slot.borrow_mut() = Some(stop.clone()));
        let expected_line = line!() + 1;
        latch_failure(CallbackFailure::DisplayChanged);
        assert_eq!(stop.origin().unwrap().line(), expected_line);
        assert_eq!(stop.reason(), Some(StopReason::DisplaysChanged));
        clear_callback_stop();
    }

    #[test]
    fn capture_stop_preserves_hard_revocation() {
        let (state, _consumer) = callback_fixture();
        state.shared.stop.stop(StopReason::NativeFailure);
        state.shared.revocation.mark_revoked_without_wake();
        propagate_capture_stop(&state.shared, &mut false);
        assert!(state.shared.revocation.is_revoked());
    }

    #[test]
    fn startup_preflight_reports_an_unavailable_desktop() {
        assert_eq!(startup_environment_error(true), None);
        assert_eq!(
            startup_environment_error(false),
            Some(NativeCaptureError::DesktopUnavailable)
        );
    }

    #[test]
    fn existing_stop_reason_wins_without_latching_a_later_environment_failure() {
        let stop = CaptureStop::default();
        stop.stop(StopReason::InvalidInput);
        let environment_checked = std::cell::Cell::new(false);

        let error = startup_completion_error(
            stop.reason(),
            || {
                environment_checked.set(true);
                startup_environment_error(false)
            },
            || stop.reason(),
        );

        assert_eq!(
            error,
            Some(NativeCaptureError::Stopped(StopReason::InvalidInput))
        );
        assert_eq!(stop.reason(), Some(StopReason::InvalidInput));
        assert!(!environment_checked.get());
    }
}
