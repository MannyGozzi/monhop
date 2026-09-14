//! Explicit macOS Quartz capture lifetime.
//!
//! A capture exists only after the caller has enabled MonHop locally. Its event tap and run loop
//! are confined to one owned thread. Callback work is limited to copied-field decoding, fixed
//! queue admission, physical-ledger updates, and atomic failure latching.

use std::{
    cell::RefCell,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use monhop_core::{
    MouseButton, Point, RevocationSignal,
    capture::{
        CaptureConsumer, CaptureEvent, CaptureProducer, CaptureStop, CapturedEvent,
        MAX_SUPPRESSION_TTL, NativeInputOwnership, StopReason, SuppressionLease, capture_channel,
    },
    capture_control::{
        CaptureCommand, ControlCompletion, ControlError, ControlReader, ControlWriter,
        control_channel,
    },
    capture_physical::{LocalTransfer, PhysicalCapture},
};

use crate::{
    ContinuousInstant, MacError, PowerWatch, SYNTHETIC_EVENT_MARKER,
    capture_decode::{
        ActiveDisplayBounds, CG_EVENT_FLAGS_CHANGED, CG_EVENT_KEY_DOWN, CG_EVENT_KEY_UP,
        CG_EVENT_SCROLL_WHEEL, DecodedInput, DecodedPointer, EventSourceMetadata,
        LocalModifierState, PointerFields, decode_keyboard, decode_pointer, decode_scroll,
        should_ignore_source, should_keep_quarantine_tap,
    },
    enumerate_active_displays,
    event_tap::{
        CFRelease, CG_EVENT_SOURCE_USER_DATA, CG_EVENT_TAP_OPTION_DEFAULT,
        CG_EVENT_TAP_OPTION_LISTEN_ONLY, CG_HEAD_INSERT_EVENT_TAP, CG_HID_EVENT_TAP, CGEventField,
        CGEventGetIntegerValueField, CGEventPost, CGEventRef, CGEventSetIntegerValueField,
        CGEventTapProxy, CGEventType, EventTap, EventTapInstallError, finish_and_post_event,
        full_input_event_mask, tap_disabled,
    },
    hid_to_mac_virtual_key, mac_virtual_key_to_hid, preflight_permissions,
    trial_window::{ForegroundSample, TrialWindow},
};

type CGDirectDisplayID = u32;
type CGDisplayChangeSummaryFlags = u32;
type CGError = i32;
type CGKeyCode = u16;
type CGEventSourceStateID = i32;
type CGMouseButton = u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

const CG_ERROR_SUCCESS: CGError = 0;
const CG_MOUSE_EVENT_BUTTON_NUMBER: CGEventField = 3;
const CG_MOUSE_EVENT_DELTA_X: CGEventField = 4;
const CG_MOUSE_EVENT_DELTA_Y: CGEventField = 5;
const CG_KEYBOARD_EVENT_KEYCODE: CGEventField = 9;
const CG_EVENT_SOURCE_STATE_ID: CGEventField = 45;
const CG_SCROLL_WHEEL_EVENT_FIXED_PT_DELTA_AXIS_1: CGEventField = 93;
const CG_SCROLL_WHEEL_EVENT_FIXED_PT_DELTA_AXIS_2: CGEventField = 94;
const CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS: CGEventField = 88;
const CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1: CGEventField = 96;
const CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2: CGEventField = 97;
const CG_EVENT_SOURCE_STATE_HID_SYSTEM: CGEventSourceStateID = 1;
const RUN_LOOP_SLICE: Duration = Duration::from_millis(10);
const MAX_OWNER_THREAD_GAP: Duration = Duration::from_secs(5);
const OWNED_RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

// SAFETY: These declarations match the current ApplicationServices SDK declarations. They are
// kept here because this adapter owns its separate active tap and does not alter native.rs. The
// shared event-tap plumbing itself lives in event_tap.rs.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: CGKeyCode,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: CGEventType,
        mouse_cursor_position: CGPoint,
        mouse_button: CGMouseButton,
    ) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetDoubleValueField(event: CGEventRef, field: CGEventField) -> f64;
    fn CGEventSourceKeyState(state_id: CGEventSourceStateID, key: CGKeyCode) -> bool;
    fn CGEventSourceButtonState(state_id: CGEventSourceStateID, button: CGMouseButton) -> bool;
    fn CGDisplayRegisterReconfigurationCallback(
        callback: Option<
            unsafe extern "C" fn(CGDirectDisplayID, CGDisplayChangeSummaryFlags, *mut c_void),
        >,
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: Option<
            unsafe extern "C" fn(CGDirectDisplayID, CGDisplayChangeSummaryFlags, *mut c_void),
        >,
        user_info: *mut c_void,
    ) -> CGError;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCaptureError {
    AlreadyActive,
    TrialWindowNotForeground,
    InvalidDuration,
    StartFailed,
    StartupTimeout,
    CleanupPending,
    ControlPending,
    ControlFailed,
    WorkerPanicked,
    Stopped(StopReason),
    Mac(MacError),
    Clock(crate::ClockError),
    Power(crate::PowerWatchError),
}

struct Shared {
    origin: ContinuousInstant,
    generation: u64,
    stop: CaptureStop,
    revocation: RevocationSignal,
    ready: AtomicBool,
    remote_active: AtomicBool,
    interception_failed: AtomicBool,
    progress_ms: AtomicU64,
    allows_suppression: bool,
    /// Present only for a controlled trial, whose foreground window is its whole confinement.
    trial_window: Option<TrialWindow>,
}

/// The caller must establish local enablement, a paired peer, and topology before starting.
/// Dropping this owner requests local restoration; it neither requests permission nor reconnects.
pub struct NativeCapture {
    shared: Arc<Shared>,
    consumer: CaptureConsumer,
    worker: Option<JoinHandle<Result<(), NativeCaptureError>>>,
    control: ControlWriter,
    completion: Option<Result<(), NativeCaptureError>>,
}

impl NativeCapture {
    pub fn start_after_local_enable(
        generation: u64,
        revocation: RevocationSignal,
    ) -> Result<Self, NativeCaptureError> {
        Self::start(generation, revocation, None, None)
    }

    /// The trial window replaces local enablement as the confinement, so it must already be in
    /// front. Refusing revokes without waking, because no capture thread exists to clean up yet.
    pub fn start_controlled_trial(
        generation: u64,
        revocation: RevocationSignal,
        window: TrialWindow,
    ) -> Result<Self, NativeCaptureError> {
        if !window.is_foreground() {
            revocation.mark_revoked_without_wake();
            return Err(NativeCaptureError::TrialWindowNotForeground);
        }
        Self::start(generation, revocation, None, Some(window))
    }

    pub fn start_diagnostic(duration: Duration) -> Result<Self, NativeCaptureError> {
        if duration.is_zero() || duration > crate::MAX_DIAGNOSTIC_DURATION {
            return Err(NativeCaptureError::InvalidDuration);
        }
        Self::start(1, RevocationSignal::default(), Some(duration), None)
    }

    fn start(
        generation: u64,
        revocation: RevocationSignal,
        diagnostic_duration: Option<Duration>,
        trial_window: Option<TrialWindow>,
    ) -> Result<Self, NativeCaptureError> {
        if generation == 0 || revocation.is_revoked() {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        let origin = ContinuousInstant::try_now().map_err(NativeCaptureError::Clock)?;
        let native_input_ownership =
            NativeInputOwnership::claim().ok_or(NativeCaptureError::AlreadyActive)?;
        let stop = CaptureStop::default();
        let power_watch =
            PowerWatch::start_after_local_enable(revocation.clone(), Some(stop.clone()))
                .map_err(NativeCaptureError::Power)?;
        if revocation.is_revoked() {
            return Err(NativeCaptureError::Stopped(
                stop.reason().unwrap_or(StopReason::Requested),
            ));
        }
        let (producer, consumer) = capture_channel(stop.clone());
        let (control, control_reader) = control_channel();
        let shared = Arc::new(Shared {
            origin,
            generation,
            stop,
            revocation,
            ready: AtomicBool::new(false),
            remote_active: AtomicBool::new(false),
            interception_failed: AtomicBool::new(false),
            progress_ms: AtomicU64::new(0),
            allows_suppression: diagnostic_duration.is_none(),
            trial_window,
        });
        let thread_shared = Arc::clone(&shared);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("monhop-macos-capture".into())
            .spawn(move || {
                if let Err(code) = crate::threads::mark_time_sensitive() {
                    log::warn!("native capture keeps normal priority (code {code})");
                }
                // This remains live through final local release retries. A destination cannot
                // start injecting while the prior capture owner still has ledger cleanup work.
                let native_input_ownership = native_input_ownership;
                let power_watch = power_watch;
                let result = run(
                    thread_shared.clone(),
                    producer,
                    control_reader,
                    diagnostic_duration,
                    started_tx,
                );
                if result.is_err() && !thread_shared.stop.is_stopped() {
                    thread_shared.stop.stop(StopReason::NativeFailure);
                }
                let stopped = thread_shared.stop.is_stopped();
                drop(native_input_ownership);
                drop(power_watch);
                if stopped {
                    log::warn!("native capture stopped on its own; revoking the session");
                    // All native guards are gone before an arbitrary observer may wake. Native
                    // callbacks use the atomic no-wake method during capture and cleanup.
                    thread_shared.revocation.revoke();
                }
                result
            }) {
            Ok(worker) => worker,
            Err(_) => return Err(NativeCaptureError::StartFailed),
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

    /// Runs `waker` on the tap thread after every queued event; see `CaptureConsumer::set_waker`.
    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.consumer.set_waker(waker)
    }

    /// Transfers an already-ready local route to the paired peer. The owner thread first releases
    /// only ledger-recorded local holds, then starts the bounded lease and publishes its barrier.
    pub fn activate_remote(
        &mut self,
        generation: u64,
        ttl: Duration,
    ) -> Result<u64, NativeCaptureError> {
        if !self.shared.allows_suppression
            || !self.shared.ready.load(Ordering::Acquire)
            || self.shared.remote_active.load(Ordering::Acquire)
            || ttl.is_zero()
            || ttl > MAX_SUPPRESSION_TTL
        {
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

    /// Renews the owner-thread suppression lease. The queue remains local until the corresponding
    /// route barrier has been admitted and the control completion reports `Applied`.
    pub fn renew_suppression(
        &mut self,
        generation: u64,
        ttl: Duration,
    ) -> Result<u64, NativeCaptureError> {
        if !self.shared.allows_suppression
            || !self.shared.ready.load(Ordering::Acquire)
            || !self.shared.remote_active.load(Ordering::Acquire)
        {
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

    /// Restores the local route at a verified logical macOS point. The owner injects that cursor
    /// position before restoring only ledger-recorded modifiers and buttons, then emits the local
    /// route barrier.
    pub fn restore_local_at(
        &mut self,
        generation: u64,
        point: Point,
    ) -> Result<u64, NativeCaptureError> {
        if !self.shared.remote_active.load(Ordering::Acquire) {
            return Err(NativeCaptureError::Stopped(StopReason::InvalidInput));
        }
        self.submit_control(generation, CaptureCommand::LocalAt(point))
    }

    pub fn restore_local(&mut self, generation: u64) -> Result<u64, NativeCaptureError> {
        self.submit_control(generation, CaptureCommand::Local)
    }

    fn submit_control(
        &mut self,
        generation: u64,
        command: CaptureCommand,
    ) -> Result<u64, NativeCaptureError> {
        if generation != self.shared.generation {
            self.shared.stop.stop(StopReason::InvalidInput);
        }
        if let Some(reason) = self.shared.stop.reason() {
            return Err(NativeCaptureError::Stopped(reason));
        }
        self.control.submit(command).map_err(|error| match error {
            ControlError::Pending => NativeCaptureError::ControlPending,
            ControlError::Failed => NativeCaptureError::ControlFailed,
            ControlError::Exhausted
            | ControlError::InvalidDeadline
            | ControlError::InvalidPoint => {
                self.shared.stop.stop(StopReason::InvalidInput);
                NativeCaptureError::Stopped(StopReason::InvalidInput)
            }
        })
    }

    pub fn completed_control_revision(&self) -> Result<u64, NativeCaptureError> {
        self.control
            .completed_revision()
            .map_err(|_| NativeCaptureError::ControlFailed)
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

    /// Returns pending while already-suppressed presses drain. The event tap remains quarantined
    /// until their physical releases arrive, unless interception itself has failed.
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
    lease: SuppressionLease,
    route_remote: bool,
    pointer_position: Option<Point>,
    active_display_bounds: ActiveDisplayBounds,
    local_modifiers: LocalModifierState,
}

thread_local! {
    static CALLBACK: RefCell<Option<CallbackState>> = const { RefCell::new(None) };
    static CALLBACK_SHARED: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

fn latch_callback_failure(reason: StopReason) {
    CALLBACK_SHARED.with(|slot| {
        if let Ok(slot) = slot.try_borrow()
            && let Some(shared) = slot.as_ref()
        {
            shared.interception_failed.store(true, Ordering::Release);
            shared.stop.stop(reason);
            shared.revocation.mark_revoked_without_wake();
        }
    });
}

/// What one window-server reading permits a capture to do with the event that prompted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrialGate {
    /// No trial window: ordinary sharing capture is never foreground-gated.
    Ungated,
    /// The trial window is still in front, so the ordinary suppression decision stands.
    Confined,
    /// Foreground lost or unreadable: stop the capture and let the event reach local apps.
    Released,
}

/// Only a confirmed front reading keeps a trial suppressing; Unknown is not that proof.
fn trial_gate(sample: Option<ForegroundSample>) -> TrialGate {
    match sample {
        None => TrialGate::Ungated,
        Some(ForegroundSample::Front) => TrialGate::Confined,
        Some(ForegroundSample::Behind | ForegroundSample::Unknown) => TrialGate::Released,
    }
}

/// Reads the window server once and latches the result. Callers must run this before admitting
/// a tapped event, so a released gate is recorded before the suppression decision reads stop.
fn latch_trial_focus_loss(shared: &Shared) -> bool {
    latch_focus_loss(
        trial_gate(
            shared
                .trial_window
                .as_ref()
                .map(TrialWindow::foreground_sample),
        ),
        &shared.stop,
        &shared.revocation,
    )
}

/// Native callbacks may not wake an arbitrary observer, so revocation is marked without a wake.
fn latch_focus_loss(gate: TrialGate, stop: &CaptureStop, revocation: &RevocationSignal) -> bool {
    if gate != TrialGate::Released {
        return false;
    }
    stop.stop(StopReason::NativeFailure);
    revocation.mark_revoked_without_wake();
    true
}

/// An earlier terminal reason outranks a focus loss first seen while seeding local state.
fn startup_completion_error(
    previous_stop: Option<StopReason>,
    trial_focus_lost: bool,
    later_stop: impl FnOnce() -> Option<StopReason>,
) -> Option<NativeCaptureError> {
    if let Some(reason) = previous_stop {
        return Some(NativeCaptureError::Stopped(reason));
    }
    if trial_focus_lost {
        return Some(NativeCaptureError::TrialWindowNotForeground);
    }
    later_stop().map(NativeCaptureError::Stopped)
}

fn with_callback(action: impl FnOnce(&mut CallbackState) -> bool) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| {
        CALLBACK.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                latch_callback_failure(StopReason::NativeFailure);
                return false;
            };
            slot.as_mut().is_some_and(action)
        })
    }));
    match result {
        Ok(result) => result,
        Err(payload) => {
            // A panic payload can panic while dropping. It must not unwind over Core Graphics.
            std::mem::forget(payload);
            latch_callback_failure(StopReason::NativeFailure);
            false
        }
    }
}

impl CallbackState {
    fn mark_stopped(&self) {
        if self.shared.stop.is_stopped() {
            self.shared.revocation.mark_revoked_without_wake();
        }
    }

    fn remote(&mut self) -> bool {
        let remote = self.lease.is_suppressing(self.shared.origin.elapsed());
        self.mark_stopped();
        remote
    }

    fn apply_control(&mut self, revision: u64, command: CaptureCommand) -> ControlCompletion {
        // Suppression may never be started or extended for a window that went behind.
        latch_trial_focus_loss(&self.shared);
        if self.shared.stop.is_stopped() {
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        match command {
            CaptureCommand::ActivateRemote { deadline } => self.activate_remote(revision, deadline),
            CaptureCommand::RemoteUntil(deadline) => self.renew_remote(revision, deadline),
            CaptureCommand::LocalAt(point) => self.restore_local_at(revision, point),
            CaptureCommand::Local => self.restore_local_without_transfer(revision),
        }
    }

    fn activate_remote(&mut self, revision: u64, deadline: Duration) -> ControlCompletion {
        if self.route_remote || !self.physical.is_ready_for_suppression() {
            self.shared.stop.stop(StopReason::InvalidInput);
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        let plan = self.physical.handoff_plan(true);
        if !self.apply_transfer_plan(plan.iter()) {
            return ControlCompletion::Failed;
        }
        if !self.renew_lease(deadline) {
            return ControlCompletion::Failed;
        }
        self.publish_route(revision, true)
    }

    fn renew_remote(&mut self, revision: u64, deadline: Duration) -> ControlCompletion {
        if !self.route_remote || !self.renew_lease(deadline) {
            return ControlCompletion::Failed;
        }
        self.publish_route(revision, true)
    }

    fn restore_local_at(&mut self, revision: u64, point: Point) -> ControlCompletion {
        if !self.route_remote || !point.is_finite() {
            self.shared.stop.stop(StopReason::NativeFailure);
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        if !self.refresh_active_display_bounds() || !self.active_display_bounds.contains(point) {
            self.shared.stop.stop(StopReason::NativeFailure);
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        if !post_cursor(point, self.local_modifiers.flags()) {
            self.shared.stop.stop(StopReason::NativeFailure);
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        self.pointer_position = Some(point);
        let plan = self.physical.handoff_plan(false);
        if !self.apply_transfer_plan(plan.iter()) {
            return ControlCompletion::Failed;
        }
        if self
            .lease
            .release(self.shared.generation, self.shared.origin.elapsed())
            .is_err()
        {
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        self.publish_route(revision, false)
    }

    fn restore_local_without_transfer(&mut self, revision: u64) -> ControlCompletion {
        if self
            .lease
            .release(self.shared.generation, self.shared.origin.elapsed())
            .is_err()
        {
            self.mark_stopped();
            return ControlCompletion::Failed;
        }
        self.publish_route(revision, false)
    }

    fn renew_lease(&mut self, deadline: Duration) -> bool {
        let now = self.shared.origin.elapsed();
        let result = match deadline.checked_sub(now) {
            Some(ttl) if !ttl.is_zero() => self.lease.renew(self.shared.generation, now, ttl),
            _ => {
                self.shared.stop.stop(StopReason::LeaseExpired);
                Err(StopReason::LeaseExpired)
            }
        };
        if result.is_err() {
            self.mark_stopped();
            return false;
        }
        true
    }

    fn apply_transfer_plan(&mut self, plan: impl Iterator<Item = LocalTransfer>) -> bool {
        for transfer in plan {
            let flags = self.local_modifiers.flags_after_transfer(transfer);
            if !post_local_transfer(transfer, self.pointer_position, flags) {
                self.shared.stop.stop(StopReason::NativeFailure);
                self.mark_stopped();
                return false;
            }
            // CGEventPost is void. Commit only after this owner created, preflighted, and
            // submitted the event, never as a claim that another process observed delivery.
            self.physical.apply_transfer_success(transfer);
            self.local_modifiers.apply_transfer_success(transfer);
        }
        true
    }

    fn publish_route(&mut self, revision: u64, remote: bool) -> ControlCompletion {
        if remote != self.route_remote {
            let barrier = CapturedEvent {
                event: CaptureEvent::RouteChanged { remote, revision },
                routing_revision: revision,
                remote,
            };
            if self.producer.try_push_tagged(barrier).is_err() {
                self.mark_stopped();
                return ControlCompletion::Failed;
            }
            self.route_remote = remote;
            self.physical.set_routing_revision(revision);
        }
        self.shared.remote_active.store(remote, Ordering::Release);
        ControlCompletion::Applied
    }

    fn release_injected_locally(&mut self) -> bool {
        let plan = self.physical.release_injected_plan();
        self.apply_transfer_plan(plan.iter())
    }

    fn event_for_route(&mut self, event: CaptureEvent, remote: bool) -> bool {
        let now = self.shared.origin.elapsed();
        let suppress =
            self.physical
                .process(event, remote, now, &mut self.producer, &self.shared.stop);
        if let CaptureEvent::Key { usage, pressed, .. } = event
            && !remote
            && !suppress
        {
            self.local_modifiers.record_local_key(usage, pressed);
        }
        self.shared
            .ready
            .store(self.physical.is_ready_for_suppression(), Ordering::Release);
        self.mark_stopped();
        suppress
    }

    fn decoded(&mut self, decoded: DecodedInput) -> bool {
        let remote = self.remote();
        self.decoded_for_route(decoded, remote)
    }

    fn decoded_for_route(&mut self, decoded: DecodedInput, remote: bool) -> bool {
        match decoded {
            DecodedInput::Ignored => false,
            DecodedInput::Unsupported => {
                self.shared.stop.stop(StopReason::InvalidInput);
                self.mark_stopped();
                false
            }
            DecodedInput::Event(event) => self.event_for_route(event, remote),
        }
    }

    fn refresh_active_display_bounds(&mut self) -> bool {
        let Ok(bounds) = current_active_display_bounds() else {
            return false;
        };
        self.active_display_bounds = bounds;
        !self.shared.stop.is_stopped()
    }

    fn native_event(
        &mut self,
        event_type: CGEventType,
        event: CGEventRef,
        source: EventSourceMetadata,
    ) -> bool {
        // One window-server reading per tapped event: a latched stop makes the ledger below
        // refuse suppression, so nothing is withheld once the trial window is behind.
        latch_trial_focus_loss(&self.shared);
        match event_type {
            CG_EVENT_KEY_DOWN | CG_EVENT_KEY_UP | CG_EVENT_FLAGS_CHANGED => {
                // SAFETY: keyboard events expose kCGKeyboardEventKeycode as an integer field.
                let raw_key =
                    unsafe { CGEventGetIntegerValueField(event, CG_KEYBOARD_EVENT_KEYCODE) };
                let Ok(key) = u16::try_from(raw_key) else {
                    return self.decoded(DecodedInput::Unsupported);
                };
                let changed_pressed = if event_type == CG_EVENT_FLAGS_CHANGED {
                    // SAFETY: key is copied from the callback event and the HID state query has no
                    // ownership or allocation side effect. It preserves the modifier side.
                    unsafe { CGEventSourceKeyState(CG_EVENT_SOURCE_STATE_HID_SYSTEM, key) }
                } else {
                    false
                };
                self.decoded(decode_keyboard(
                    event_type,
                    key,
                    changed_pressed,
                    source,
                    SYNTHETIC_EVENT_MARKER,
                ))
            }
            CG_EVENT_SCROLL_WHEEL => {
                // SAFETY: the live callback scroll event exposes the continuous-scroll flag.
                let continuous = unsafe {
                    CGEventGetIntegerValueField(event, CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0
                };
                let (vertical, horizontal) = if continuous {
                    // SAFETY: the live scroll event exposes both point-delta fields.
                    unsafe {
                        (
                            CGEventGetDoubleValueField(
                                event,
                                CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
                            ),
                            CGEventGetDoubleValueField(
                                event,
                                CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
                            ),
                        )
                    }
                } else {
                    // SAFETY: the live scroll event exposes both fixed-point line-delta fields.
                    unsafe {
                        (
                            CGEventGetDoubleValueField(
                                event,
                                CG_SCROLL_WHEEL_EVENT_FIXED_PT_DELTA_AXIS_1,
                            ),
                            CGEventGetDoubleValueField(
                                event,
                                CG_SCROLL_WHEEL_EVENT_FIXED_PT_DELTA_AXIS_2,
                            ),
                        )
                    }
                };
                self.decoded(decode_scroll(
                    horizontal,
                    vertical,
                    continuous,
                    source,
                    SYNTHETIC_EVENT_MARKER,
                ))
            }
            _ => {
                // SAFETY: these getters borrow the event, which remains live for the callback.
                let (point, button_number, delta_x, delta_y) = unsafe {
                    (
                        CGEventGetLocation(event),
                        CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_BUTTON_NUMBER),
                        CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_DELTA_X),
                        CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_DELTA_Y),
                    )
                };
                let remote = self.remote();
                let DecodedPointer {
                    local_absolute,
                    input,
                    position,
                } = decode_pointer(
                    event_type,
                    PointerFields {
                        location: Point::new(point.x, point.y),
                        delta_x,
                        delta_y,
                        button_number,
                    },
                    self.pointer_position,
                    source,
                    SYNTHETIC_EVENT_MARKER,
                );
                self.pointer_position = position;
                if !remote && let Some(absolute) = local_absolute {
                    self.event_for_route(absolute, false);
                    if self.shared.stop.is_stopped() {
                        return false;
                    }
                }
                self.decoded_for_route(input, remote)
            }
        }
    }
}

unsafe extern "C" fn event_callback(
    _: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    _: *mut c_void,
) -> CGEventRef {
    if tap_disabled(event_type) {
        latch_callback_failure(StopReason::NativeFailure);
        return event;
    }
    if event.is_null() {
        latch_callback_failure(StopReason::NativeFailure);
        return event;
    }
    // SAFETY: Core Graphics supplies a live event for the callback duration. This filter runs
    // before any thread-local borrow, so MonHop's own marked injection cannot reenter capture.
    let source = unsafe {
        EventSourceMetadata {
            user_data: CGEventGetIntegerValueField(event, CG_EVENT_SOURCE_USER_DATA),
            state_id: CGEventGetIntegerValueField(event, CG_EVENT_SOURCE_STATE_ID),
        }
    };
    if should_ignore_source(source, SYNTHETIC_EVENT_MARKER) {
        return event;
    }
    if with_callback(|state| state.native_event(event_type, event, source)) {
        ptr::null()
    } else {
        event
    }
}

unsafe extern "C" fn display_reconfiguration_callback(
    _: CGDirectDisplayID,
    _: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
) {
    if user_info.is_null() {
        return;
    }
    // SAFETY: DisplayWatch retains one Arc for the entire registered callback lifetime.
    let shared = unsafe { &*user_info.cast::<Shared>() };
    shared.stop.stop(StopReason::NativeFailure);
    shared.revocation.mark_revoked_without_wake();
}

fn run(
    shared: Arc<Shared>,
    producer: CaptureProducer,
    mut control: ControlReader,
    diagnostic_duration: Option<Duration>,
    started: mpsc::SyncSender<Result<(), NativeCaptureError>>,
) -> Result<(), NativeCaptureError> {
    let active_display_bounds = match read_while_capture_active(
        &shared.stop,
        &shared.revocation,
        current_active_display_bounds,
    ) {
        Ok(bounds) => bounds,
        Err(error) => {
            let _ = started.send(Err(error));
            return Err(error);
        }
    };
    CALLBACK_SHARED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&shared)));
    CALLBACK.with(|slot| {
        *slot.borrow_mut() = Some(CallbackState {
            shared: Arc::clone(&shared),
            producer,
            physical: if diagnostic_duration.is_some() {
                PhysicalCapture::new_passive(Duration::ZERO)
            } else {
                PhysicalCapture::new(Duration::ZERO)
            },
            lease: SuppressionLease::new(shared.generation, Duration::ZERO, shared.stop.clone()),
            route_remote: false,
            pointer_position: None,
            active_display_bounds,
            local_modifiers: LocalModifierState::default(),
        });
    });

    // No tap is installed behind a trial window that already lost the foreground.
    if latch_trial_focus_loss(&shared) {
        let error = NativeCaptureError::TrialWindowNotForeground;
        let _ = started.send(Err(error));
        clear_callback();
        return Err(error);
    }
    let mut resources = match Resources::install(
        diagnostic_duration.is_some(),
        &shared.stop,
        &shared.revocation,
    ) {
        Ok(resources) => resources,
        Err(error) => {
            let _ = started.send(Err(error));
            clear_callback();
            return Err(error);
        }
    };
    let mut display_watch = match DisplayWatch::install(&shared) {
        Ok(watch) => watch,
        Err(error) => {
            let _ = started.send(Err(error));
            let cleanup = resources.close();
            clear_callback();
            return cleanup.and(Err(error));
        }
    };
    seed_initial_state();
    let seeded_stop = shared.stop.reason();
    let trial_focus_lost = latch_trial_focus_loss(&shared);
    if let Some(error) =
        startup_completion_error(seeded_stop, trial_focus_lost, || shared.stop.reason())
    {
        let _ = started.send(Err(error));
        let cleanup = close_native_resources(&mut resources, &mut display_watch);
        clear_callback();
        return cleanup.and(Err(error));
    }
    update_progress(&shared);
    let _ = started.send(Ok(()));

    let mut previous_tick = shared.origin.elapsed();
    loop {
        let now = shared.origin.elapsed();
        if now.saturating_sub(previous_tick) >= MAX_OWNER_THREAD_GAP {
            shared.interception_failed.store(true, Ordering::Release);
            shared.stop.stop(StopReason::NativeFailure);
        }
        previous_tick = now;
        update_progress(&shared);
        if shared.revocation.is_revoked()
            || diagnostic_duration.is_some_and(|duration| now >= duration)
        {
            shared.stop.stop(StopReason::Requested);
        }
        if !permissions_still_sufficient(diagnostic_duration.is_some()) {
            shared.interception_failed.store(true, Ordering::Release);
            shared.stop.stop(StopReason::NativeFailure);
        }
        if let Some((revision, command)) = control.pending() {
            let completion = if with_callback(|state| {
                matches!(
                    state.apply_control(revision, command),
                    ControlCompletion::Applied
                )
            }) {
                ControlCompletion::Applied
            } else {
                ControlCompletion::Failed
            };
            // Failed is terminal in the mailbox. In particular, a full queue, a stopped lease, or
            // a rejected barrier can never be reported as a completed route change.
            control.complete(revision, completion);
        }

        let mut draining = false;
        with_callback(|state| {
            state.physical.tick(shared.origin.elapsed(), &shared.stop);
            state.remote();
            draining = state.physical.has_suppressed_presses();
            state.mark_stopped();
            false
        });
        if shared.stop.is_stopped()
            && !should_keep_quarantine_tap(
                draining,
                shared.interception_failed.load(Ordering::Acquire),
            )
        {
            break;
        }

        resources.run_once();
    }

    // Remove the tap before cleanup so this loop can only submit fixed ledger-owned local
    // releases. A revoked permission leaves the failed item in the ledger for the next retry.
    let cleanup = close_native_resources(&mut resources, &mut display_watch);
    release_injected_until_complete();
    shared.remote_active.store(false, Ordering::Release);
    clear_callback();
    cleanup
}

fn update_progress(shared: &Shared) {
    shared.progress_ms.store(
        shared
            .origin
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        Ordering::Release,
    );
}

fn release_injected_until_complete() {
    while !with_callback(|state| state.release_injected_locally()) {
        thread::sleep(OWNED_RELEASE_RETRY_INTERVAL);
    }
}

fn close_native_resources(
    resources: &mut Resources,
    display_watch: &mut DisplayWatch,
) -> Result<(), NativeCaptureError> {
    // Always remove the tap even if display-watch unregistration fails. The latter can retain
    // its Arc defensively, but must not leave a suppressing callback active during cleanup.
    let resources_result = resources.close();
    let display_result = display_watch.close();
    resources_result.and(display_result)
}

fn permissions_still_sufficient(passive: bool) -> bool {
    preflight_permissions().is_ok_and(|permissions| {
        permissions.listen_events && (passive || permissions.accessibility)
    })
}

fn seed_initial_state() {
    with_callback(|state| {
        for key in 0_u16..=u16::from(u8::MAX) {
            if let Ok(usage) = mac_virtual_key_to_hid(crate::MacVirtualKey(key)) {
                // SAFETY: the state query reads the current HID key state and has no callback or
                // ownership side effect.
                if unsafe { CGEventSourceKeyState(CG_EVENT_SOURCE_STATE_HID_SYSTEM, key) } {
                    state.physical.seed_locally_held_key(usage);
                    state.local_modifiers.record_local_key(usage, true);
                }
            }
        }
        for (button, quartz_button) in [
            (MouseButton::Left, 0),
            (MouseButton::Right, 1),
            (MouseButton::Middle, 2),
            (MouseButton::Back, 3),
            (MouseButton::Forward, 4),
        ] {
            // SAFETY: Quartz mouse-button identifiers are the fixed values used by the decoder.
            if unsafe { CGEventSourceButtonState(CG_EVENT_SOURCE_STATE_HID_SYSTEM, quartz_button) }
            {
                state.physical.seed_locally_held_button(button);
            }
        }
        state.pointer_position =
            current_pointer_position().filter(|point| state.active_display_bounds.contains(*point));
        state
            .shared
            .ready
            .store(state.physical.is_ready_for_suppression(), Ordering::Release);
        false
    });
}

fn clear_callback() {
    CALLBACK.with(|slot| *slot.borrow_mut() = None);
    CALLBACK_SHARED.with(|slot| *slot.borrow_mut() = None);
}

/// Posts exactly one ledger-owned local transfer. This deliberately bypasses `MacInjector`: its
/// held-state guard is appropriate for remote payload injection, while this path must release or
/// restore each fixed physical-plan item on the capture owner thread before its route barrier.
fn post_local_transfer(transfer: LocalTransfer, position: Option<Point>, flags: u64) -> bool {
    match transfer {
        LocalTransfer::Key { usage, pressed } => {
            let Ok(key) = hid_to_mac_virtual_key(usage) else {
                return false;
            };
            // SAFETY: the key came from the fixed HID mapping and a null source requests Quartz's
            // default source. The owned event is released by post_marked_event.
            let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), key.0, pressed) };
            post_marked_event(event, flags)
        }
        LocalTransfer::Button { button, pressed } => {
            let Some(point) = position else {
                return false;
            };
            post_button(button, pressed, point, flags)
        }
    }
}

fn post_cursor(point: Point, flags: u64) -> bool {
    if !point.is_finite() {
        return false;
    }
    // SAFETY: point is finite logical macOS desktop coordinates. The event remains owned until
    // post_marked_event releases it.
    let event = unsafe {
        CGEventCreateMouseEvent(
            ptr::null(),
            5,
            CGPoint {
                x: point.x,
                y: point.y,
            },
            0,
        )
    };
    post_marked_event(event, flags)
}

fn post_button(button: MouseButton, pressed: bool, point: Point, flags: u64) -> bool {
    if !point.is_finite() {
        return false;
    }
    let (event_type, button_number) = match (button, pressed) {
        (MouseButton::Left, true) => (1, 0),
        (MouseButton::Left, false) => (2, 0),
        (MouseButton::Right, true) => (3, 1),
        (MouseButton::Right, false) => (4, 1),
        (MouseButton::Middle, true) => (25, 2),
        (MouseButton::Middle, false) => (26, 2),
        (MouseButton::Back, true) => (25, 3),
        (MouseButton::Back, false) => (26, 3),
        (MouseButton::Forward, true) => (25, 4),
        (MouseButton::Forward, false) => (26, 4),
    };
    // SAFETY: event type and button number are selected from the fixed enum; Core Graphics uses
    // a three-button creation value, then the documented field preserves Back and Forward.
    let event = unsafe {
        CGEventCreateMouseEvent(
            ptr::null(),
            event_type,
            CGPoint {
                x: point.x,
                y: point.y,
            },
            button_number.min(2),
        )
    };
    if event.is_null() {
        return false;
    }
    // SAFETY: event is owned and non-null until post_marked_event releases it.
    unsafe {
        CGEventSetIntegerValueField(
            event,
            CG_MOUSE_EVENT_BUTTON_NUMBER,
            i64::from(button_number),
        );
    }
    post_marked_event(event, flags)
}

fn post_marked_event(event: CGEventRef, flags: u64) -> bool {
    if event.is_null() {
        return false;
    }
    // This is called only by the owner thread. CGEventPost is void, so permission and event
    // creation prove submission eligibility, not downstream delivery.
    if !preflight_permissions().is_ok_and(|permissions| permissions.accessibility) {
        // SAFETY: event is owned by this function even when permission was revoked.
        unsafe { CFRelease(event) };
        return false;
    }
    // The marker is set before callback TLS is borrowed, so this local handoff injection cannot
    // alter physical capture state.
    finish_and_post_event(event, SYNTHETIC_EVENT_MARKER, flags, |event| {
        // SAFETY: event remains owned until finish_and_post_event releases it after this call.
        unsafe { CGEventPost(CG_HID_EVENT_TAP, event) };
    });
    true
}

fn require_capture_active(
    stop: &CaptureStop,
    revocation: &RevocationSignal,
) -> Result<(), NativeCaptureError> {
    if revocation.is_revoked() {
        stop.stop(StopReason::Requested);
    }
    match stop.reason() {
        Some(reason) => Err(NativeCaptureError::Stopped(reason)),
        None => Ok(()),
    }
}

fn read_while_capture_active<T>(
    stop: &CaptureStop,
    revocation: &RevocationSignal,
    read: impl FnOnce() -> Result<T, NativeCaptureError>,
) -> Result<T, NativeCaptureError> {
    require_capture_active(stop, revocation)?;
    let result = read()?;
    require_capture_active(stop, revocation)?;
    Ok(result)
}

fn current_active_display_bounds() -> Result<ActiveDisplayBounds, NativeCaptureError> {
    let displays = enumerate_active_displays().map_err(NativeCaptureError::Mac)?;
    ActiveDisplayBounds::from_rectangles(displays.into_iter().map(|display| display.logical_bounds))
        .ok_or(NativeCaptureError::Mac(MacError::InvalidDisplayData))
}

/// The pointer in the same global coordinates the event tap reports, for a poll between samples.
pub fn current_pointer_position() -> Option<Point> {
    // SAFETY: a null source requests the current Quartz event state. The snapshot event is owned
    // and never posted.
    let event = unsafe { CGEventCreate(ptr::null()) };
    if event.is_null() {
        return None;
    }
    // SAFETY: event remains owned until the matching CFRelease below.
    let point = unsafe { CGEventGetLocation(event) };
    // SAFETY: event was created by this function and was never transferred.
    unsafe { CFRelease(event) };
    (point.x.is_finite() && point.y.is_finite()).then_some(Point::new(point.x, point.y))
}

struct Resources {
    tap: EventTap,
}

impl Resources {
    fn install(
        passive: bool,
        stop: &CaptureStop,
        revocation: &RevocationSignal,
    ) -> Result<Self, NativeCaptureError> {
        let permissions = read_while_capture_active(stop, revocation, || {
            preflight_permissions().map_err(NativeCaptureError::Mac)
        })?;
        if !permissions.listen_events {
            return Err(NativeCaptureError::Mac(
                MacError::ListenEventPermissionRequired,
            ));
        }
        if !passive && !permissions.accessibility {
            return Err(NativeCaptureError::Mac(
                MacError::AccessibilityPermissionRequired,
            ));
        }
        let options = if passive {
            CG_EVENT_TAP_OPTION_LISTEN_ONLY
        } else {
            CG_EVENT_TAP_OPTION_DEFAULT
        };
        // The event mask contains only documented input event constants and the callback uses
        // thread-local state established before this explicit tap is created.
        let tap = EventTap::install(
            CG_HEAD_INSERT_EVENT_TAP,
            options,
            full_input_event_mask(),
            Some(event_callback),
            ptr::null_mut(),
        )
        .map_err(|error| {
            NativeCaptureError::Mac(match error {
                EventTapInstallError::TapUnavailable => MacError::EventTapUnavailable,
                EventTapInstallError::SourceUnavailable => MacError::EventTapSourceUnavailable,
            })
        })?;
        Ok(Self { tap })
    }

    fn run_once(&self) {
        // A bounded timeout services stop and control polling without a callback waker.
        self.tap.run_once(RUN_LOOP_SLICE);
    }

    fn close(&mut self) -> Result<(), NativeCaptureError> {
        // Disabling before removal prevents new suppression; close then ends delivery before TLS
        // state is cleared.
        self.tap.disable();
        self.tap.close();
        Ok(())
    }
}

struct DisplayWatch {
    shared: *const Shared,
    registered: bool,
}

impl DisplayWatch {
    fn install(shared: &Arc<Shared>) -> Result<Self, NativeCaptureError> {
        let retained = Arc::into_raw(Arc::clone(shared));
        // SAFETY: retained stays live until a successful unregistration releases it below.
        let status = unsafe {
            CGDisplayRegisterReconfigurationCallback(
                Some(display_reconfiguration_callback),
                retained.cast_mut().cast(),
            )
        };
        if status != CG_ERROR_SUCCESS {
            // SAFETY: registration did not retain this pointer, so this balances into_raw.
            unsafe { drop(Arc::from_raw(retained)) };
            return Err(NativeCaptureError::StartFailed);
        }
        Ok(Self {
            shared: retained,
            registered: true,
        })
    }

    fn close(&mut self) -> Result<(), NativeCaptureError> {
        if !self.registered {
            return Ok(());
        }
        // SAFETY: shared is the same stable pointer passed during registration.
        let status = unsafe {
            CGDisplayRemoveReconfigurationCallback(
                Some(display_reconfiguration_callback),
                self.shared.cast_mut().cast(),
            )
        };
        if status != CG_ERROR_SUCCESS {
            // The callback may still race after an unsuccessful removal. Retain this one Arc to
            // prevent use-after-free; capture is already terminal and never reconnects itself.
            self.registered = false;
            return Err(NativeCaptureError::StartFailed);
        }
        self.registered = false;
        // SAFETY: a successful removal guarantees this Arc is no longer reachable by the callback.
        unsafe { drop(Arc::from_raw(self.shared)) };
        Ok(())
    }
}

#[cfg(test)]
mod trial_gate_tests {
    use super::*;
    use monhop_core::{HidUsage, ModifierState};

    fn key(pressed: bool) -> CaptureEvent {
        CaptureEvent::Key {
            usage: HidUsage(0x04),
            pressed,
            repeat: false,
            modifiers: ModifierState(0),
        }
    }

    #[test]
    fn only_a_confirmed_front_reading_keeps_a_trial_confined() {
        assert_eq!(
            trial_gate(Some(ForegroundSample::Front)),
            TrialGate::Confined
        );
        assert_eq!(
            trial_gate(Some(ForegroundSample::Behind)),
            TrialGate::Released
        );
        assert_eq!(
            trial_gate(Some(ForegroundSample::Unknown)),
            TrialGate::Released
        );
    }

    #[test]
    fn ordinary_sharing_capture_is_never_foreground_gated() {
        assert_eq!(trial_gate(None), TrialGate::Ungated);
        let stop = CaptureStop::default();
        let revocation = RevocationSignal::default();
        assert!(!latch_focus_loss(TrialGate::Ungated, &stop, &revocation));
        assert!(!latch_focus_loss(TrialGate::Confined, &stop, &revocation));
        assert_eq!(stop.reason(), None);
        assert!(!revocation.is_revoked());
    }

    #[test]
    fn a_released_gate_latches_stop_and_revocation() {
        let stop = CaptureStop::default();
        let revocation = RevocationSignal::default();
        assert!(latch_focus_loss(TrialGate::Released, &stop, &revocation));
        assert_eq!(stop.reason(), Some(StopReason::NativeFailure));
        assert!(revocation.is_revoked());
    }

    #[test]
    fn a_released_gate_passes_later_input_through_and_drains_only_its_own_suppression() {
        let stop = CaptureStop::default();
        let (mut producer, _consumer) = capture_channel(stop.clone());
        let mut physical = PhysicalCapture::new(Duration::ZERO);
        // The remote route opens with its barrier, exactly as publish_route does.
        producer
            .try_push_tagged(CapturedEvent {
                event: CaptureEvent::RouteChanged {
                    remote: true,
                    revision: 1,
                },
                routing_revision: 1,
                remote: true,
            })
            .unwrap();
        physical.set_routing_revision(1);
        assert!(physical.process(key(true), true, Duration::ZERO, &mut producer, &stop));

        assert!(latch_focus_loss(
            TrialGate::Released,
            &stop,
            &RevocationSignal::default()
        ));

        // The release of an already-suppressed press still drains: no application may see a key
        // go up that it never saw go down. Anything pressed afterwards reaches the user.
        assert!(physical.process(key(false), true, Duration::ZERO, &mut producer, &stop));
        assert!(!physical.process(key(true), true, Duration::ZERO, &mut producer, &stop));
    }

    #[test]
    fn startup_reports_focus_loss_but_never_over_an_earlier_terminal_reason() {
        assert_eq!(startup_completion_error(None, false, || None), None);
        assert_eq!(
            startup_completion_error(None, true, || Some(StopReason::NativeFailure)),
            Some(NativeCaptureError::TrialWindowNotForeground)
        );
        assert_eq!(
            startup_completion_error(Some(StopReason::EmergencyEscape), true, || panic!(
                "an earlier reason is terminal"
            )),
            Some(NativeCaptureError::Stopped(StopReason::EmergencyEscape))
        );
        assert_eq!(
            startup_completion_error(None, false, || Some(StopReason::Requested)),
            Some(NativeCaptureError::Stopped(StopReason::Requested))
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn stopped_capture_never_enters_native_startup_reads() {
        let stop = CaptureStop::default();
        let signal = RevocationSignal::default();
        signal.mark_revoked_without_wake();
        let result: Result<(), _> = read_while_capture_active(&stop, &signal, || {
            panic!("cancelled startup cannot read native state")
        });
        assert_eq!(
            result,
            Err(NativeCaptureError::Stopped(StopReason::Requested))
        );
    }

    #[test]
    fn stop_or_sleep_during_a_native_read_prevents_later_hook_installation() {
        for power in [false, true] {
            let stop = CaptureStop::default();
            let signal = RevocationSignal::default();
            let result = read_while_capture_active(&stop, &signal, || {
                if power {
                    signal.mark_revoked_without_wake();
                } else {
                    stop.stop(StopReason::Requested);
                }
                Ok(())
            });
            assert_eq!(
                result,
                Err(NativeCaptureError::Stopped(StopReason::Requested))
            );
        }
    }
}
