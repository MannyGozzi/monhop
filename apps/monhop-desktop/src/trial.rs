//! A one-use native test window. Preparation never opens a socket or owns input.

use crate::{
    lifecycle::AppController,
    sharing::{LayoutRequest, SharingController, SharingView},
};
use monhop_core::{Point, RevocationSignal};
use monhop_transport::session_trial::{TrialAuthorization, TrialProgress, TrialStopReason};
use serde::Serialize;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use tauri::{
    Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    webview::NewWindowResponse,
};

pub const WINDOW_LABEL: &str = "controlled-trial";

/// Insets of the test pad inside the window, in logical points. macOS measures the near edge
/// from the bottom of the content view; Windows measures it from the top.
const PAD_SIDE: f64 = 24.0;
const PAD_NEAR: f64 = 36.0;
const PAD_FAR: f64 = 110.0;
/// Half a logical point absorbs physical-pixel rounding at any scale factor.
const GEOMETRY_TOLERANCE: f64 = 0.5;

#[derive(Default)]
pub struct TrialController(Mutex<State>);
#[derive(Default)]
struct State {
    ticket: Option<(String, LayoutRequest)>,
    retry_ticket: Option<(String, LayoutRequest)>,
    open: bool,
    started: bool,
    attempt_active: bool,
    launch_pending: bool,
    closing: bool,
    /// Set once Start is committed and never cleared, so only a test that ran is recorded.
    ran: bool,
    recorded: bool,
    sends_input: bool,
    peer_name: String,
    seam_hint: Option<String>,
    last_result: Option<LastTrialView>,
    focus: Arc<AtomicBool>,
    geometry: Option<TrialGeometry>,
    cancel: Option<RevocationSignal>,
    progress: Option<TrialProgress>,
    preflight_error: Option<String>,
    error: Option<String>,
    sent: String,
    received: String,
    #[cfg(target_os = "macos")]
    menu: Option<tauri::menu::Menu<tauri::Wry>>,
    #[cfg(target_os = "macos")]
    arrivals: Option<Arc<crate::trial_dispatch::ArrivalCounters>>,
}

/// Logical geometry, so a display scale change is not mistaken for a moved window.
#[derive(Clone, Copy, Debug)]
struct TrialGeometry {
    position: (f64, f64),
    size: (f64, f64),
}

enum GeometryEvent {
    Moved(PhysicalPosition<i32>),
    Resized(PhysicalSize<u32>),
}

struct StartPermit {
    geometry: TrialGeometry,
    authorization: TrialAuthorization,
    #[cfg(target_os = "macos")]
    arrivals: Option<Arc<crate::trial_dispatch::ArrivalCounters>>,
}

const FOCUS_LOST_REASON: &str =
    "The test stopped because the window lost focus. Open a new test to try again.";
const GEOMETRY_CHANGED_REASON: &str =
    "The test stopped because the window moved or changed size. Open a new test to try again.";
const WAITING_FOR_WINDOW: &str =
    "Click the test window to bring it back in front, then the test continues.";
/// Display names come from the peer's dashboard entry, so they are bounded before any use.
const MAX_PEER_NAME: usize = 48;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialView {
    pub(crate) phase: &'static str,
    message: String,
    remaining_seconds: u64,
    sent_events: String,
    received_events: String,
    busy: bool,
    retryable_start: bool,
    preflight_failure: bool,
    sends_input: bool,
    peer_name: String,
    native_arrivals: ArrivalView,
}

/// The outcome of the most recent test, kept after its window closes so it can be read and copied.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastTrialView {
    outcome: &'static str,
    message: String,
    sends_input: bool,
    peer_name: String,
    sent_events: String,
    received_events: String,
    recorded_at: u64,
}

impl LastTrialView {
    pub(crate) fn report(&self) -> String {
        let role = if self.sends_input {
            "sends input to"
        } else {
            "receives input from"
        };
        format!(
            "MonHop test result\nOutcome: {}\nThis computer {role} {}\nMessage: {}\nSent {} · received {}\nRecorded at (unix seconds): {}",
            self.outcome,
            self.peer_name,
            self.message,
            self.sent_events,
            self.received_events,
            self.recorded_at
        )
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) fn bounded_peer_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_PEER_NAME)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "the other computer".to_owned()
    } else {
        name.to_owned()
    }
}

#[derive(Default, Serialize)]
struct ArrivalView {
    keyboard: u64,
    button: u64,
    movement: u64,
    scroll: u64,
}
fn native_arrivals(state: &State) -> ArrivalView {
    #[cfg(target_os = "macos")]
    if let Some(arrivals) = &state.arrivals {
        let value = arrivals.snapshot();
        return ArrivalView {
            keyboard: value.keyboard,
            button: value.button,
            movement: value.movement,
            scroll: value.scroll,
        };
    }
    let _ = state;
    ArrivalView::default()
}

impl TrialController {
    pub(crate) fn is_open(&self) -> bool {
        lock(&self.0).open
    }

    #[cfg(test)]
    pub(crate) fn test_activate_for_late_event(&self) {
        let mut state = lock(&self.0);
        state.started = true;
        state.attempt_active = true;
        state.cancel = Some(RevocationSignal::default());
    }

    #[cfg(test)]
    pub(crate) fn test_late_focus_after_worker(&self, sharing: &SharingController) -> bool {
        self.stop_for_reason(FOCUS_LOST_REASON, sharing.status().busy)
    }

    pub fn prepare(
        &self,
        revision: String,
        layout: LayoutRequest,
        peer_name: String,
    ) -> Result<(), String> {
        let mut state = lock(&self.0);
        if state.open {
            return Err("Close the existing test window first.".into());
        }
        *state = State {
            ticket: Some((revision, layout)),
            open: true,
            peer_name: bounded_peer_name(&peer_name),
            last_result: state.last_result.take(),
            ..State::default()
        };
        Ok(())
    }

    pub(crate) fn last_result(&self) -> Option<LastTrialView> {
        lock(&self.0).last_result.clone()
    }

    /// Setup checks that do not need the window, so their rejections keep the test startable.
    fn preflight(&self, controller: &AppController) -> Result<(), String> {
        let ticket = lock(&self.0)
            .ticket
            .clone()
            .ok_or("Open a new test window to start another test.")?;
        controller.metadata_action(|| {
            if !controller.pairing.shutdown_ready() {
                return Err("Finish pairing on both computers, then press Start again.".to_owned());
            }
            controller.validate_trial_setup(&ticket.0, &ticket.1)
        })
    }

    fn begin(
        &self,
        window: &WebviewWindow,
    ) -> Result<(String, LayoutRequest, TrialAuthorization), String> {
        let mut state = lock(&self.0);
        if !state.open || state.closing || state.started {
            return Err("Open a new test window to start another test.".into());
        }
        let receives_input = !state.sends_input;
        let permit = (|| {
            let (id, bounds) = window_identity(window, receives_input)?;
            let geometry = window_geometry(window)?;
            state.focus.store(true, Ordering::Release);
            let authorization =
                TrialAuthorization::for_current_window(id, state.focus.clone(), bounds)
                    .map_err(|error| error.to_string())?;
            #[cfg(target_os = "macos")]
            // Only a destination sees marked arrivals, so a source never gates local events.
            let arrivals = receives_input
                .then(|| {
                    crate::trial_dispatch::arm(
                        objc2::MainThreadMarker::new()
                            .ok_or("Check the test window on the main thread.")?,
                        id,
                        state.focus.clone(),
                        authorization.revocation(),
                        bounds.ok_or("The native test area is unavailable.")?,
                    )
                })
                .transpose()?;
            Ok(StartPermit {
                geometry,
                authorization,
                #[cfg(target_os = "macos")]
                arrivals,
            })
        })();
        match commit_begin(&mut state, permit) {
            Ok(started) => Ok(started),
            Err(error) => {
                state.focus.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
    pub fn revoke(&self) {
        let mut state = lock(&self.0);
        state.attempt_active = false;
        revoke_state(&mut state, None);
    }
    fn preflight_rejected(&self, error: String) {
        let mut state = lock(&self.0);
        if state.open && !state.closing && !state.started && state.ticket.is_some() {
            state.preflight_error = Some(error);
        }
    }

    /// Nothing connected, so the spent ticket comes back and Start stays available.
    fn launch_rejected(&self, error: String) {
        let mut state = lock(&self.0);
        state.launch_pending = false;
        state.attempt_active = false;
        if state.retry_ticket.is_none() || state.closing || !state.open {
            revoke_state(&mut state, None);
            state.error.get_or_insert(error);
            return;
        }
        if let Some(cancel) = state.cancel.take() {
            cancel.revoke();
        }
        state.focus.store(false, Ordering::Release);
        state.started = false;
        state.geometry = None;
        state.progress = None;
        #[cfg(target_os = "macos")]
        {
            state.arrivals = None;
        }
        state.ticket = state.retry_ticket.take();
        state.preflight_error = Some(error);
    }

    /// Focus loss before the active phase pauses startup instead of ending the test.
    fn window_left_front(&self, sharing_busy: bool) -> bool {
        {
            let state = lock(&self.0);
            if !state.started {
                return false;
            }
            if !state.closing
                && state
                    .progress
                    .as_ref()
                    .is_some_and(|progress| !progress.is_active())
            {
                state.focus.store(false, Ordering::Release);
                return false;
            }
        }
        self.stop_for_reason(FOCUS_LOST_REASON, sharing_busy)
    }

    fn stop_for_reason(&self, reason: &'static str, sharing_busy: bool) -> bool {
        let mut state = lock(&self.0);
        if !state.started {
            return false;
        }
        let report = state.attempt_active && (state.launch_pending || sharing_busy);
        state.attempt_active = false;
        revoke_state(&mut state, report.then_some(reason));
        report
    }
    fn stop_for_geometry_event(
        &self,
        event: GeometryEvent,
        scale: Option<f64>,
        sharing_busy: bool,
    ) -> bool {
        let mut state = lock(&self.0);
        if !state.started || geometry_matches(state.geometry, event, scale) {
            return false;
        }
        let report = state.attempt_active && (state.launch_pending || sharing_busy);
        state.attempt_active = false;
        revoke_state(&mut state, report.then_some(GEOMETRY_CHANGED_REASON));
        report
    }
    fn finish_start(&self, result: &Result<(), String>) {
        let mut state = lock(&self.0);
        state.launch_pending = false;
        if let Err(error) = result {
            state.attempt_active = false;
            let outcome = outcome_message(&state);
            revoke_state(&mut state, None);
            state
                .error
                .get_or_insert_with(|| outcome.unwrap_or(error).to_owned());
        }
    }
    pub fn view(&self, sharing: &SharingController) -> TrialView {
        self.view_from(sharing.status(), sharing)
    }

    /// Same as `view`, but for a caller that already has a fresh `SharingView` and would
    /// otherwise pay for a second status lock and clone to get one.
    pub fn view_from(&self, view: SharingView, sharing: &SharingController) -> TrialView {
        let mut state = lock(&self.0);
        if view.diagnostics.sent_events != "0" {
            state.sent = view.diagnostics.sent_events;
        }
        if view.diagnostics.received_events != "0" {
            state.received = view.diagnostics.received_events;
        }
        let busy = state.launch_pending || (state.started && !sharing.shutdown_ready());
        let retryable_start = state.open
            && !state.closing
            && !state.started
            && state.ticket.is_some()
            && state.error.is_none();
        let waiting = state
            .progress
            .as_ref()
            .is_some_and(TrialProgress::is_waiting_for_window);
        let (phase, message) = if state.closing {
            ("stopping", "Releasing held input before closing.".into())
        } else if let Some(error) = &state.error {
            (if busy { "stopping" } else { "error" }, error.clone())
        } else if !state.started && state.ticket.is_some() {
            let message = state.preflight_error.as_ref().map_or_else(
                || {
                    format!(
                        "Press Start here and on {}, in either order. Each computer waits up to two minutes for the other. Nothing is shared yet.",
                        state.peer_name
                    )
                },
                |error| {
                    format!(
                        "The test did not start: {error} You can correct it and try Start again."
                    )
                },
            );
            ("prepared", message)
        } else if busy && view.phase == "sharing" {
            let message = if state.sends_input {
                match &state.seam_hint {
                    Some(edge) => format!(
                        "Test active. Move the pointer off {edge}. It continues in the test window on {}.",
                        state.peer_name
                    ),
                    None => format!(
                        "Test active. Move the pointer across the seam into the test window on {}.",
                        state.peer_name
                    ),
                }
            } else {
                format!(
                    "Test active. {} is sending. Keep this window in front.",
                    state.peer_name
                )
            };
            ("active", message)
        } else if busy && (state.launch_pending || view.phase == "starting") {
            let message = if waiting {
                WAITING_FOR_WINDOW.into()
            } else {
                format!(
                    "Waiting for {} to press Start. Keep this window in front.",
                    state.peer_name
                )
            };
            ("starting", message)
        } else if busy {
            (
                "stopping",
                "Stopping the test and releasing held input.".into(),
            )
        } else if view.phase == "error" && state.started {
            let message = outcome_message(&state).map_or(view.message, str::to_owned);
            ("error", message)
        } else {
            (
                "finished",
                "Test ended. Input is local. Open a new test window to try again.".into(),
            )
        };
        if matches!(phase, "error" | "finished") && state.ran && !state.recorded {
            state.recorded = true;
            state.last_result = Some(LastTrialView {
                outcome: if phase == "error" {
                    "failed"
                } else {
                    "finished"
                },
                message: message.clone(),
                sends_input: state.sends_input,
                peer_name: state.peer_name.clone(),
                sent_events: if state.sent.is_empty() {
                    "0".into()
                } else {
                    state.sent.clone()
                },
                received_events: if state.received.is_empty() {
                    "0".into()
                } else {
                    state.received.clone()
                },
                recorded_at: unix_seconds(),
            });
        }
        TrialView {
            phase,
            message,
            remaining_seconds: state
                .progress
                .as_ref()
                .map_or(60, TrialProgress::remaining_seconds),
            sent_events: if state.sent.is_empty() {
                "0".into()
            } else {
                state.sent.clone()
            },
            received_events: if state.received.is_empty() {
                "0".into()
            } else {
                state.received.clone()
            },
            busy,
            retryable_start,
            preflight_failure: retryable_start && state.preflight_error.is_some(),
            sends_input: state.sends_input,
            peer_name: state.peer_name.clone(),
            native_arrivals: native_arrivals(&state),
        }
    }
}

fn outcome_message(state: &State) -> Option<&'static str> {
    state
        .progress
        .as_ref()
        .and_then(TrialProgress::stop_reason)
        .map(TrialStopReason::message)
}

fn revoke_state(state: &mut State, reason: Option<&'static str>) {
    state.ticket = None;
    state.retry_ticket = None;
    state.preflight_error = None;
    state.focus.store(false, Ordering::Release);
    if let Some(reason) = reason {
        state.error.get_or_insert_with(|| reason.into());
    }
    if let Some(cancel) = &state.cancel {
        cancel.revoke();
    }
}

fn commit_begin(
    state: &mut State,
    permit: Result<StartPermit, String>,
) -> Result<(String, LayoutRequest, TrialAuthorization), String> {
    let permit = permit?;
    let (revision, layout) = state.ticket.take().ok_or("This test was already used.")?;
    #[cfg(target_os = "macos")]
    let StartPermit {
        geometry,
        authorization,
        arrivals,
    } = permit;
    #[cfg(not(target_os = "macos"))]
    let StartPermit {
        geometry,
        authorization,
    } = permit;
    state.started = true;
    state.ran = true;
    state.attempt_active = true;
    state.preflight_error = None;
    state.geometry = Some(geometry);
    #[cfg(target_os = "macos")]
    {
        state.arrivals = arrivals;
    }
    state.cancel = Some(authorization.revocation());
    state.progress = Some(authorization.progress());
    state.retry_ticket = Some((revision.clone(), layout.clone()));
    state.launch_pending = true;
    Ok((revision, layout, authorization))
}

fn geometry_matches(
    baseline: Option<TrialGeometry>,
    event: GeometryEvent,
    scale: Option<f64>,
) -> bool {
    let (Some(baseline), Some(scale)) = (baseline, scale.filter(|s| s.is_finite() && *s > 0.0))
    else {
        return false;
    };
    let (observed, expected) = match event {
        GeometryEvent::Moved(position) => (
            (f64::from(position.x) / scale, f64::from(position.y) / scale),
            baseline.position,
        ),
        GeometryEvent::Resized(size) => (
            (
                f64::from(size.width) / scale,
                f64::from(size.height) / scale,
            ),
            baseline.size,
        ),
    };
    (observed.0 - expected.0).abs() <= GEOMETRY_TOLERANCE
        && (observed.1 - expected.1).abs() <= GEOMETRY_TOLERANCE
}

fn window_scale(window: &WebviewWindow) -> Result<f64, String> {
    let scale = window
        .scale_factor()
        .map_err(|_| "The native test window scale is unavailable.")?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err("The native test window scale is invalid.".into());
    }
    Ok(scale)
}

fn window_geometry(window: &WebviewWindow) -> Result<TrialGeometry, String> {
    let scale = window_scale(window)?;
    let position = window
        .outer_position()
        .map_err(|_| "The native test window position is unavailable.")?;
    let size = window
        .inner_size()
        .map_err(|_| "The native test window size is unavailable.")?;
    if size.width == 0 || size.height == 0 {
        return Err("The native test window has no usable size.".into());
    }
    Ok(TrialGeometry {
        position: (f64::from(position.x) / scale, f64::from(position.y) / scale),
        size: (
            f64::from(size.width) / scale,
            f64::from(size.height) / scale,
        ),
    })
}

pub fn open(
    app: &tauri::AppHandle,
    revision: String,
    layout: LayoutRequest,
    peer_name: String,
) -> Result<(), String> {
    let controller = app.state::<Arc<AppController>>();
    controller.prepare_trial(revision, layout, peer_name)?;
    let result = (|| {
        {
            let saved = saved_setup(app)?;
            let mut state = lock(&controller.trial.0);
            state.sends_input = saved.source_side() == "local";
            state.seam_hint = saved.source_seam_hint();
        }
        #[cfg(target_os = "macos")]
        crate::trial_dispatch::install(
            objc2::MainThreadMarker::new().ok_or("Open the test window on the main thread.")?,
        )?;
        #[cfg(target_os = "macos")]
        {
            lock(&controller.trial.0).menu = app
                .remove_menu()
                .map_err(|_| "The app menu could not be disabled for testing.")?;
        }
        let window =
            WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("trial.html".into()))
                .title("MonHop · Controlled test")
                .initialization_script(format!(
                    "window.__MONHOP_PLATFORM__ = {:?};",
                    crate::window_platform()
                ))
                .inner_size(860.0, 640.0)
                .min_inner_size(760.0, 560.0)
                .resizable(false)
                .maximizable(false)
                .visible(false)
                .incognito(true)
                .devtools(false)
                .disable_drag_drop_handler()
                .browser_extensions_enabled(false)
                .general_autofill_enabled(false)
                .on_navigation(local_navigation)
                .on_new_window(|_, _| NewWindowResponse::Deny)
                .on_download(|_, _| false);
        #[cfg(windows)]
        let window = window.with_environment(
            crate::constrained_webview2_environment(
                &app.path().app_local_data_dir().map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?,
        );
        let window = window.build().map_err(|e| e.to_string())?;
        let main = app
            .get_webview_window("main")
            .ok_or("The setup window is unavailable.")?;
        main.hide().map_err(|e| e.to_string())?;
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })();
    if result.is_err() {
        controller.trial.revoke();
        lock(&controller.trial.0).open = false;
        restore_menu(app, &controller.trial);
        if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
            let _ = window.destroy();
        }
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
        }
    }
    result
}

/// The active computer's applied setup on disk decides the role: platform alone cannot, now
/// that both computers may run either side.
fn saved_setup(
    app: &tauri::AppHandle,
) -> Result<crate::sharing_preferences::SharingPreferences, String> {
    let path = crate::sharing_setup_path(app)?;
    crate::sharing_preferences::SetupFile::load(&path)
        .map_err(|_| "The saved setup could not be read.".to_owned())?
        .active_computer()
        .cloned()
        .ok_or_else(|| "Apply a layout on both computers before testing.".to_owned())
}

pub async fn start(window: WebviewWindow) -> Result<TrialView, String> {
    require_window(&window)?;
    let app = window.app_handle().clone();
    let controller = app.state::<Arc<AppController>>().inner().clone();
    // Setup rejections must stay retryable, so they run before the ticket is spent.
    if let Err(error) = controller.trial.preflight(&controller) {
        controller.trial.preflight_rejected(error);
        return Ok(controller.trial.view(&controller.sharing));
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let main_controller = controller.clone();
    app.run_on_main_thread(move || {
        let result = main_controller.trial.begin(&window);
        let _ = sender.send(result);
    })
    .map_err(|_| "The test window could not be checked.".to_owned())?;
    let started = receiver
        .await
        .map_err(|_| "The test window check stopped.".to_owned())?;
    let (revision, layout, authorization) = match started {
        Ok(started) => started,
        Err(error) => {
            controller.trial.preflight_rejected(error);
            return Ok(controller.trial.view(&controller.sharing));
        }
    };
    let worker = controller.clone();
    let launched = tauri::async_runtime::spawn_blocking(move || {
        worker.start_trial(revision, layout, authorization)
    })
    .await
    .map_err(|_| "The test could not start.".to_owned())?;
    if let Err(error) = launched {
        // The launch was refused before any connection attempt, so the test stays ready.
        controller.trial.launch_rejected(error);
        disarm_dispatch(&app);
        return Ok(controller.trial.view(&controller.sharing));
    }
    controller.trial.finish_start(&Ok(()));
    Ok(controller.trial.view(&controller.sharing))
}

pub fn stop(window: &WebviewWindow) -> Result<TrialView, String> {
    require_window(window)?;
    let controller = window.state::<Arc<AppController>>();
    controller.trial.revoke();
    controller.sharing.stop();
    Ok(controller.trial.view(&controller.sharing))
}

pub fn close(app: &tauri::AppHandle) {
    let controller = app.state::<Arc<AppController>>().inner().clone();
    {
        let mut state = lock(&controller.trial.0);
        if !state.open || state.closing {
            return;
        }
        state.closing = true;
    }
    // The closing poll may never arrive, so the outcome is recorded before anything is released.
    let _ = controller.trial.view(&controller.sharing);
    controller.trial.revoke();
    controller.sharing.stop();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while !controller.sharing.shutdown_ready() || lock(&controller.trial.0).launch_pending {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let app_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            #[cfg(target_os = "macos")]
            if let Some(marker) = objc2::MainThreadMarker::new() {
                // A closed test must stop consuming marked events for the rest of the process.
                crate::trial_dispatch::disarm(marker);
            }
            if let Some(window) = app_main.get_webview_window(WINDOW_LABEL) {
                let _ = window.destroy();
            }
            restore_menu(&app_main, &controller.trial);
            {
                let mut state = lock(&controller.trial.0);
                *state = State {
                    last_result: state.last_result.take(),
                    ..State::default()
                };
            }
            if let Some(window) = app_main.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        });
    });
}

fn disarm_dispatch(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.run_on_main_thread(|| {
        if let Some(marker) = objc2::MainThreadMarker::new() {
            crate::trial_dispatch::disarm(marker);
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

fn restore_menu(app: &tauri::AppHandle, trial: &TrialController) {
    #[cfg(target_os = "macos")]
    if let Some(menu) = lock(&trial.0).menu.take() {
        let _ = app.set_menu(menu);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, trial);
}

pub fn focus_lost_or_destroyed(app: &tauri::AppHandle) {
    let controller = app.state::<Arc<AppController>>();
    if controller
        .trial
        .window_left_front(controller.sharing.status().busy)
    {
        controller.sharing.stop();
    }
}

pub fn moved(app: &tauri::AppHandle, position: PhysicalPosition<i32>) {
    let controller = app.state::<Arc<AppController>>();
    if controller.trial.stop_for_geometry_event(
        GeometryEvent::Moved(position),
        current_scale(app),
        controller.sharing.status().busy,
    ) {
        controller.sharing.stop();
    }
}

pub fn resized(app: &tauri::AppHandle, size: PhysicalSize<u32>) {
    let controller = app.state::<Arc<AppController>>();
    if controller.trial.stop_for_geometry_event(
        GeometryEvent::Resized(size),
        current_scale(app),
        controller.sharing.status().busy,
    ) {
        controller.sharing.stop();
    }
}

fn current_scale(app: &tauri::AppHandle) -> Option<f64> {
    app.get_webview_window(WINDOW_LABEL)
        .and_then(|window| window_scale(&window).ok())
}

pub fn require_window(window: &WebviewWindow) -> Result<(), String> {
    if window.label() != WINDOW_LABEL {
        return Err("This action is only available in the test window.".into());
    }
    Ok(())
}
fn local_navigation(url: &tauri::Url) -> bool {
    crate::bundled_origin(url)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/trial.html"
}

#[cfg(target_os = "macos")]
fn window_identity(
    window: &WebviewWindow,
    needs_bounds: bool,
) -> Result<(usize, Option<(Point, Point)>), String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSScreen, NSWindow};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let marker = MainThreadMarker::new().ok_or("Check the test window on the main thread.")?;
    let pointer = window
        .ns_window()
        .map_err(|_| "The native test window is unavailable.")?;
    // SAFETY: Tauri owns this live NSWindow and this function runs only on the AppKit main thread.
    let native = unsafe { (pointer as *const NSWindow).as_ref() }
        .ok_or("The native test window is unavailable.")?;
    if !native.isKeyWindow() {
        return Err("Bring the test window to the front first.".into());
    }
    let id =
        usize::try_from(native.windowNumber()).map_err(|_| "The test window ID is invalid.")?;
    if !needs_bounds {
        return Ok((id, None));
    }
    let bounds = native
        .contentView()
        .ok_or("The test window has no content.")?
        .bounds();
    let pad = NSRect::new(
        NSPoint::new(PAD_SIDE, PAD_NEAR),
        NSSize::new(
            bounds.size.width - PAD_SIDE * 2.0,
            bounds.size.height - (PAD_NEAR + PAD_FAR),
        ),
    );
    if pad.size.width <= 0.0 || pad.size.height <= 0.0 {
        return Err("The test window is too small.".into());
    }
    let screen = native.convertRectToScreen(pad);
    let primary = NSScreen::screens(marker)
        .firstObject()
        .ok_or("The primary display is unavailable.")?;
    let top = primary.frame().size.height - (screen.origin.y + screen.size.height);
    Ok((
        id,
        Some((
            Point::new(screen.origin.x, top),
            Point::new(screen.size.width, screen.size.height),
        )),
    ))
}
#[cfg(windows)]
fn window_identity(
    window: &WebviewWindow,
    needs_bounds: bool,
) -> Result<(usize, Option<(Point, Point)>), String> {
    if !window
        .is_focused()
        .map_err(|_| "The test window focus is unavailable.")?
    {
        return Err("Bring the test window to the front first.".into());
    }
    let id = window
        .hwnd()
        .map_err(|_| "The native test window is unavailable.")?
        .0 as usize;
    if !needs_bounds {
        return Ok((id, None));
    }
    // The Windows desktop topology is in physical pixels, so the pad rectangle must be too.
    let scale = window_scale(window)?;
    let origin = window
        .inner_position()
        .map_err(|_| "The native test window position is unavailable.")?;
    let size = window
        .inner_size()
        .map_err(|_| "The native test window size is unavailable.")?;
    let pad = Point::new(
        f64::from(size.width) - PAD_SIDE * 2.0 * scale,
        f64::from(size.height) - (PAD_NEAR + PAD_FAR) * scale,
    );
    if pad.x <= 0.0 || pad.y <= 0.0 {
        return Err("The test window is too small.".into());
    }
    Ok((
        id,
        Some((
            Point::new(
                f64::from(origin.x) + PAD_SIDE * scale,
                f64::from(origin.y) + PAD_NEAR * scale,
            ),
            pad,
        )),
    ))
}
#[cfg(not(any(windows, target_os = "macos")))]
fn window_identity(_: &WebviewWindow, _: bool) -> Result<(usize, Option<(Point, Point)>), String> {
    Err("Native tests require Windows or macOS.".into())
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layout() -> LayoutRequest {
        LayoutRequest {
            source_display: "1".into(),
            links: vec![],
            arrangement: None,
        }
    }
    fn baseline() -> TrialGeometry {
        TrialGeometry {
            position: (120.0, 240.0),
            size: (860.0, 640.0),
        }
    }
    #[test]
    fn prepared_test_is_inert_and_revocation_removes_start_ticket() {
        let trial = TrialController::default();
        trial
            .prepare("1".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        let state = lock(&trial.0);
        assert!(!state.started);
        assert!(state.cancel.is_none());
        assert!(!state.focus.load(Ordering::Acquire));
        drop(state);
        assert!(
            trial
                .prepare("2".into(), layout(), "Noctua Windows PC".into())
                .is_err()
        );
        trial.revoke();
        assert!(lock(&trial.0).ticket.is_none());
    }

    #[test]
    fn the_last_outcome_is_kept_across_closing_and_reopening_the_window() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        trial
            .prepare("1".into(), layout(), " Noctua Windows PC \u{7}".into())
            .unwrap();
        assert!(trial.last_result().is_none());
        assert_eq!(trial.view(&sharing).peer_name, "Noctua Windows PC");
        {
            let mut state = lock(&trial.0);
            state.ran = true;
            state.sends_input = false;
            state.error =
                Some("The connection to the other computer ended. [Session: Wire]".into());
        }
        assert_eq!(trial.view(&sharing).phase, "error");
        let last = trial.last_result().unwrap();
        assert_eq!(last.outcome, "failed");
        assert!(last.message.ends_with("[Session: Wire]"));
        assert!(
            last.report()
                .contains("This computer receives input from Noctua Windows PC")
        );
        trial.revoke();
        {
            let mut state = lock(&trial.0);
            *state = State {
                last_result: state.last_result.take(),
                ..State::default()
            };
        }
        trial
            .prepare("2".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        assert!(
            trial
                .last_result()
                .is_some_and(|last| last.outcome == "failed")
        );
        // A prepared but never started test leaves the earlier record alone.
        assert_eq!(trial.view(&sharing).phase, "prepared");
        assert!(
            trial
                .last_result()
                .is_some_and(|last| last.outcome == "failed")
        );
    }

    #[test]
    fn failed_begin_validation_leaves_the_test_prepared() {
        let trial = TrialController::default();
        trial
            .prepare("1".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        let mut state = lock(&trial.0);
        match commit_begin(
            &mut state,
            Err::<StartPermit, _>("window validation failed".into()),
        ) {
            Err(error) => assert_eq!(error, "window validation failed"),
            Ok(_) => panic!("failed validation must not start the test"),
        }
        assert!(state.ticket.is_some());
        assert!(!state.started);
        assert!(!state.launch_pending);
        assert!(state.geometry.is_none());
        assert!(state.cancel.is_none());
        assert!(!state.focus.load(Ordering::Acquire));
    }

    #[test]
    fn a_refused_launch_returns_the_ticket_and_keeps_start_retryable() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        trial
            .prepare("7".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        let signal = RevocationSignal::default();
        {
            // Stand in for commit_begin, which needs a real window to build an authorization.
            let mut state = lock(&trial.0);
            let ticket = state.ticket.take().unwrap();
            state.retry_ticket = Some(ticket);
            state.started = true;
            state.attempt_active = true;
            state.launch_pending = true;
            state.geometry = Some(baseline());
            state.cancel = Some(signal.clone());
        }

        trial.launch_rejected("Finish pairing on both computers, then press Start again.".into());

        let state = lock(&trial.0);
        assert_eq!(
            state.ticket.as_ref().map(|ticket| ticket.0.as_str()),
            Some("7")
        );
        assert!(state.retry_ticket.is_none());
        assert!(!state.started);
        assert!(!state.launch_pending);
        assert!(state.error.is_none());
        assert!(signal.is_revoked());
        drop(state);

        let view = trial.view(&sharing);
        assert_eq!(view.phase, "prepared");
        assert!(view.retryable_start);
        assert!(view.preflight_failure);
        assert_eq!(view.remaining_seconds, 60);
    }

    #[test]
    fn a_refused_launch_after_a_stop_keeps_the_stop() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        trial
            .prepare("7".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        {
            let mut state = lock(&trial.0);
            state.retry_ticket = state.ticket.take();
            state.started = true;
            state.attempt_active = true;
            state.launch_pending = true;
            state.cancel = Some(RevocationSignal::default());
        }
        trial.revoke();

        trial.launch_rejected("The test could not start.".into());

        let state = lock(&trial.0);
        assert!(state.ticket.is_none());
        assert_eq!(state.error.as_deref(), Some("The test could not start."));
        drop(state);
        assert!(!trial.view(&sharing).retryable_start);
    }

    #[test]
    fn preflight_rejection_is_retryable_and_separate_from_terminal_error() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        trial
            .prepare("1".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        trial.preflight_rejected("Bring the test window to the front first.".into());

        let view = trial.view(&sharing);
        assert_eq!(view.phase, "prepared");
        assert!(view.retryable_start);
        assert!(view.preflight_failure);
        assert!(view.message.contains("did not start"));

        let state = lock(&trial.0);
        assert!(state.ticket.is_some());
        assert!(!state.started);
        assert!(state.error.is_none());
    }

    #[test]
    fn stopping_after_a_preflight_rejection_clears_the_retryable_outcome() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        trial
            .prepare("1".into(), layout(), "Noctua Windows PC".into())
            .unwrap();
        trial.preflight_rejected("Bring the test window to the front first.".into());
        trial.revoke();

        let view = trial.view(&sharing);
        assert_eq!(view.phase, "finished");
        assert!(!view.retryable_start);
        assert!(!view.preflight_failure);
    }

    #[test]
    fn native_focus_revocation_is_sticky() {
        let trial = TrialController::default();
        let signal = RevocationSignal::default();
        {
            let mut state = lock(&trial.0);
            state.cancel = Some(signal.clone());
            state.focus.store(true, Ordering::Release);
        }
        trial.revoke();
        trial.revoke();
        assert!(signal.is_revoked());
        assert!(!lock(&trial.0).focus.load(Ordering::Acquire));
    }

    #[test]
    fn duplicate_window_events_match_the_authorized_geometry() {
        assert!(geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(120, 240)),
            Some(1.0),
        ));
        assert!(geometry_matches(
            Some(baseline()),
            GeometryEvent::Resized(PhysicalSize::new(860, 640)),
            Some(1.0),
        ));
    }

    #[test]
    fn a_scale_factor_change_alone_is_not_a_moved_or_resized_window() {
        assert!(geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(240, 480)),
            Some(2.0),
        ));
        assert!(geometry_matches(
            Some(baseline()),
            GeometryEvent::Resized(PhysicalSize::new(1720, 1280)),
            Some(2.0),
        ));
        // A fractional logical origin rounds to whole physical pixels within the tolerance.
        let fractional = TrialGeometry {
            position: (120.4, 240.0),
            ..baseline()
        };
        assert!(geometry_matches(
            Some(fractional),
            GeometryEvent::Moved(PhysicalPosition::new(181, 360)),
            Some(1.5),
        ));
        // One physical pixel at 1.5x is a real move, not rounding.
        assert!(!geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(181, 360)),
            Some(1.5),
        ));
        // A real one-point move at the same scale still stops the test.
        assert!(!geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(242, 480)),
            Some(2.0),
        ));
        // An unreadable scale cannot confirm the window is where it was authorized.
        assert!(!geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(120, 240)),
            None,
        ));
    }

    #[test]
    fn changed_or_missing_window_geometry_stops_and_keeps_its_reason() {
        assert!(!geometry_matches(
            Some(baseline()),
            GeometryEvent::Moved(PhysicalPosition::new(121, 240)),
            Some(1.0),
        ));
        assert!(!geometry_matches(
            Some(baseline()),
            GeometryEvent::Resized(PhysicalSize::new(861, 640)),
            Some(1.0),
        ));
        assert!(!geometry_matches(
            None,
            GeometryEvent::Moved(PhysicalPosition::new(120, 240)),
            Some(1.0),
        ));

        let trial = TrialController::default();
        let signal = RevocationSignal::default();
        {
            let mut state = lock(&trial.0);
            state.started = true;
            state.attempt_active = true;
            state.geometry = Some(baseline());
            state.focus.store(true, Ordering::Release);
            state.cancel = Some(signal.clone());
        }
        assert!(!trial.stop_for_geometry_event(
            GeometryEvent::Moved(PhysicalPosition::new(120, 240)),
            Some(1.0),
            true,
        ));
        assert!(!signal.is_revoked());
        assert!(trial.stop_for_geometry_event(
            GeometryEvent::Resized(PhysicalSize::new(861, 640)),
            Some(1.0),
            true,
        ));
        trial.finish_start(&Err("The sharing worker stopped.".into()));
        let state = lock(&trial.0);
        assert!(signal.is_revoked());
        assert_eq!(state.error.as_deref(), Some(GEOMETRY_CHANGED_REASON));
    }

    #[test]
    fn worker_failure_is_recorded_before_late_window_events() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        let signal = RevocationSignal::default();
        {
            let mut state = lock(&trial.0);
            state.started = true;
            state.attempt_active = true;
            state.launch_pending = true;
            state.geometry = Some(baseline());
            state.cancel = Some(signal.clone());
        }

        trial.finish_start(&Err("The sharing worker stopped.".into()));
        assert!(!trial.stop_for_reason(FOCUS_LOST_REASON, false));
        assert!(!trial.stop_for_geometry_event(
            GeometryEvent::Moved(PhysicalPosition::new(121, 240)),
            Some(1.0),
            false,
        ));

        assert!(signal.is_revoked());
        let state = lock(&trial.0);
        assert!(!state.launch_pending);
        assert_eq!(state.error.as_deref(), Some("The sharing worker stopped."));
        drop(state);
        assert_eq!(trial.view(&sharing).phase, "error");
    }

    #[test]
    fn late_window_events_revoke_finished_attempts_without_replacing_completion() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        let signal = RevocationSignal::default();
        trial.test_activate_for_late_event();
        {
            let mut state = lock(&trial.0);
            state.geometry = Some(baseline());
            state.cancel = Some(signal.clone());
        }

        assert!(!trial.test_late_focus_after_worker(&sharing));
        assert!(!trial.stop_for_geometry_event(
            GeometryEvent::Resized(PhysicalSize::new(861, 640)),
            Some(1.0),
            false,
        ));

        assert!(signal.is_revoked());
        assert!(lock(&trial.0).error.is_none());
        assert_eq!(trial.view(&sharing).phase, "finished");
    }

    #[test]
    fn manual_stop_keeps_late_window_events_from_replacing_the_stop_result() {
        let trial = TrialController::default();
        let signal = RevocationSignal::default();
        {
            let mut state = lock(&trial.0);
            state.started = true;
            state.attempt_active = true;
            state.cancel = Some(signal.clone());
        }

        trial.revoke();
        assert!(!trial.stop_for_reason(FOCUS_LOST_REASON, true));

        assert!(signal.is_revoked());
        assert!(!lock(&trial.0).attempt_active);
        assert!(lock(&trial.0).error.is_none());
    }

    #[test]
    fn pending_start_is_never_reported_as_completed_cleanup() {
        let trial = TrialController::default();
        let sharing = SharingController::default();
        {
            let mut state = lock(&trial.0);
            state.started = true;
            state.launch_pending = true;
        }
        let view = trial.view(&sharing);
        assert_eq!(view.phase, "starting");
        assert!(view.busy);
        trial.revoke();
        assert!(trial.view(&sharing).busy);
        lock(&trial.0).launch_pending = false;
        assert_eq!(trial.view(&sharing).phase, "finished");
    }

    #[test]
    fn trial_navigation_cannot_reach_setup_or_remote_pages() {
        for url in [
            "tauri://localhost/index.html",
            "tauri://localhost/trial.html?x=1",
            "tauri://localhost/trial.html#x",
            "https://example.com/trial.html",
        ] {
            assert!(!local_navigation(&url.parse().unwrap()));
        }
    }
}
