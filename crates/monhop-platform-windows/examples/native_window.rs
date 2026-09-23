//! Explicit Windows input-delivery readiness diagnostic.
//!
//! This example does nothing unless invoked with exactly `--run` or `--run-source`.
//! Windows `SendInput` is global and cannot be confined to this process, so neither diagnostic
//! calls it. `--run` records window-message categories and aggregate Raw Input counts.
//! `--run-source` additionally uses the bounded passive native source capture path.

#[cfg(any(test, windows))]
use std::time::{Duration, Instant};
use std::{
    ffi::{OsStr, OsString},
    process::ExitCode,
};

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageCategory {
    Keyboard,
    Pointer,
    Scroll,
}

#[cfg(any(test, windows))]
const PASSIVE_RUN_DURATION: Duration = Duration::from_secs(14);
#[cfg(any(test, windows))]
const RESULT_VISIBLE_DURATION: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticMode {
    PassiveReadiness,
    PassiveSource,
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservedCategories {
    keyboard: bool,
    pointer: bool,
    scroll: bool,
}

#[cfg(any(test, windows))]
impl ObservedCategories {
    fn record(&mut self, category: MessageCategory) {
        match category {
            MessageCategory::Keyboard => self.keyboard = true,
            MessageCategory::Pointer => self.pointer = true,
            MessageCategory::Scroll => self.scroll = true,
        }
    }

    const fn passed(self) -> bool {
        self.keyboard && self.pointer && self.scroll
    }

    const fn summary(self) -> &'static str {
        match (self.keyboard, self.pointer, self.scroll) {
            (true, true, true) => "keyboard,pointer,scroll",
            (true, true, false) => "keyboard,pointer",
            (true, false, true) => "keyboard,scroll",
            (false, true, true) => "pointer,scroll",
            (true, false, false) => "keyboard",
            (false, true, false) => "pointer",
            (false, false, true) => "scroll",
            (false, false, false) => "none",
        }
    }
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    PassiveResult,
    StartRejectedForFocus,
    Cancelled,
    SetupFailure,
    NativeSourceFailure,
    MessageLoopFailure,
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy)]
enum PassivePhase {
    AwaitingStart,
    Running {
        started_at: Instant,
        deadline: Instant,
    },
    ShowingResult {
        deadline: Instant,
    },
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusLossSource {
    WmActivateAppFalse,
    ForegroundNull,
    ForegroundOther,
}

#[cfg(any(test, windows))]
impl FocusLossSource {
    const fn category(self) -> &'static str {
        match self {
            Self::WmActivateAppFalse => "wm_activateapp_false",
            Self::ForegroundNull => "foreground_null",
            Self::ForegroundOther => "foreground_other",
        }
    }
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundState {
    ThisWindow,
    Null,
    Other,
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FocusLoss {
    source: FocusLossSource,
    elapsed_ms: u64,
    categories: ObservedCategories,
}

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadinessAction {
    None,
    ArmRunTimer,
    ArmResultTimer,
    Exit,
}

/// Models only message categories and monotonic deadlines; it retains no input values.
#[cfg(any(test, windows))]
struct PassiveReadiness {
    phase: PassivePhase,
    completion: Option<Completion>,
    categories: ObservedCategories,
    first_focus_loss: Option<FocusLoss>,
}

#[cfg(any(test, windows))]
impl Default for PassiveReadiness {
    fn default() -> Self {
        Self {
            phase: PassivePhase::AwaitingStart,
            completion: None,
            categories: ObservedCategories::default(),
            first_focus_loss: None,
        }
    }
}

#[cfg(any(test, windows))]
impl PassiveReadiness {
    const fn is_running(&self) -> bool {
        matches!(self.phase, PassivePhase::Running { .. })
    }

    fn start(&mut self, now: Instant, window_is_foreground: bool) -> ReadinessAction {
        if !matches!(self.phase, PassivePhase::AwaitingStart) {
            return ReadinessAction::None;
        }
        if !window_is_foreground {
            self.completion = Some(Completion::StartRejectedForFocus);
            self.phase = PassivePhase::ShowingResult {
                deadline: now + RESULT_VISIBLE_DURATION,
            };
            return ReadinessAction::ArmResultTimer;
        }
        self.phase = PassivePhase::Running {
            started_at: now,
            deadline: now + PASSIVE_RUN_DURATION,
        };
        ReadinessAction::ArmRunTimer
    }

    fn observe(
        &mut self,
        now: Instant,
        foreground: ForegroundState,
        category: MessageCategory,
    ) -> ReadinessAction {
        let action = self.advance(now);
        if action != ReadinessAction::None {
            return action;
        }
        if !self.is_running() {
            return ReadinessAction::None;
        }
        match foreground {
            ForegroundState::ThisWindow => self.categories.record(category),
            ForegroundState::Null => self.note_focus_loss(now, FocusLossSource::ForegroundNull),
            ForegroundState::Other => self.note_focus_loss(now, FocusLossSource::ForegroundOther),
        }
        ReadinessAction::None
    }

    fn observe_source(
        &mut self,
        now: Instant,
        foreground: ForegroundState,
        category: MessageCategory,
    ) {
        if !self.is_running() {
            return;
        }
        match foreground {
            ForegroundState::ThisWindow => self.categories.record(category),
            ForegroundState::Null => self.note_focus_loss(now, FocusLossSource::ForegroundNull),
            ForegroundState::Other => self.note_focus_loss(now, FocusLossSource::ForegroundOther),
        }
    }

    fn note_focus_loss(&mut self, now: Instant, source: FocusLossSource) {
        let PassivePhase::Running { started_at, .. } = self.phase else {
            return;
        };
        self.first_focus_loss.get_or_insert(FocusLoss {
            source,
            elapsed_ms: duration_millis(now.saturating_duration_since(started_at)),
            categories: self.categories,
        });
    }

    fn advance(&mut self, now: Instant) -> ReadinessAction {
        match self.phase {
            PassivePhase::AwaitingStart => ReadinessAction::None,
            PassivePhase::Running { deadline, .. } if now >= deadline => self.finish(now),
            PassivePhase::ShowingResult { deadline } if now >= deadline => ReadinessAction::Exit,
            PassivePhase::Running { .. } | PassivePhase::ShowingResult { .. } => {
                ReadinessAction::None
            }
        }
    }

    fn fail_setup(&mut self, now: Instant) -> ReadinessAction {
        self.completion = Some(Completion::SetupFailure);
        self.phase = PassivePhase::ShowingResult {
            deadline: now + RESULT_VISIBLE_DURATION,
        };
        ReadinessAction::ArmResultTimer
    }

    fn fail_native_source(&mut self, now: Instant) -> ReadinessAction {
        if self.completion.is_some() {
            return self.advance(now);
        }
        self.completion = Some(Completion::NativeSourceFailure);
        self.phase = PassivePhase::ShowingResult {
            deadline: now + RESULT_VISIBLE_DURATION,
        };
        ReadinessAction::ArmResultTimer
    }

    fn finish_source(&mut self, now: Instant) -> ReadinessAction {
        if self.is_running() {
            self.finish(now)
        } else {
            self.advance(now)
        }
    }

    fn cancel(&mut self) {
        if self.completion.is_none() {
            self.completion = Some(Completion::Cancelled);
        }
    }

    fn fail_message_loop(&mut self) {
        self.completion = Some(Completion::MessageLoopFailure);
    }

    fn completion(&self) -> Option<Completion> {
        self.completion
    }

    const fn passive_passed(&self) -> bool {
        self.categories.passed() && self.first_focus_loss.is_none()
    }

    const fn categories(&self) -> ObservedCategories {
        self.categories
    }

    const fn first_focus_loss(&self) -> Option<FocusLoss> {
        self.first_focus_loss
    }

    fn run_started_at(&self) -> Option<Instant> {
        match self.phase {
            PassivePhase::Running { started_at, .. } => Some(started_at),
            PassivePhase::AwaitingStart | PassivePhase::ShowingResult { .. } => None,
        }
    }

    #[cfg(test)]
    fn run_elapsed_ms(&self, now: Instant) -> Option<u64> {
        self.run_started_at()
            .map(|started_at| duration_millis(now.saturating_duration_since(started_at)))
    }

    fn finish(&mut self, now: Instant) -> ReadinessAction {
        self.completion = Some(Completion::PassiveResult);
        self.phase = PassivePhase::ShowingResult {
            deadline: now + RESULT_VISIBLE_DURATION,
        };
        ReadinessAction::ArmResultTimer
    }
}

#[cfg(any(test, windows))]
fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn explicit_run_mode(arguments: impl IntoIterator<Item = OsString>) -> Option<DiagnosticMode> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    match (arguments.next(), arguments.next()) {
        (Some(argument), None) if argument == OsStr::new("--run") => {
            Some(DiagnosticMode::PassiveReadiness)
        }
        (Some(argument), None) if argument == OsStr::new("--run-source") => {
            Some(DiagnosticMode::PassiveSource)
        }
        _ => None,
    }
}

#[cfg(test)]
fn explicit_run_argument(arguments: impl IntoIterator<Item = OsString>) -> bool {
    matches!(
        explicit_run_mode(arguments),
        Some(DiagnosticMode::PassiveReadiness)
    )
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    let _ = explicit_run_mode(std::env::args_os());
    eprintln!("native_window status=UNSUPPORTED platform=non_windows");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    let Some(mode) = explicit_run_mode(std::env::args_os()) else {
        eprintln!("native_window status=NOT_STARTED required=--run_or_--run-source");
        return ExitCode::from(2);
    };
    windows::run(mode)
}

#[cfg(windows)]
mod windows {
    use std::{
        cell::{Cell, RefCell},
        mem,
        process::ExitCode,
        ptr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use monhop_platform_windows::{
        CaptureStats, InputError,
        capture::{CaptureEvent, StopReason},
        capture_counts,
        native_capture::{NativeCapture, NativeCaptureError},
    };

    use windows_sys::Win32::{
        Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{EnableWindow, SetFocus},
            WindowsAndMessaging::{
                BN_CLICKED, BS_PUSHBUTTON, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW,
                DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
                GetForegroundWindow, GetMessageW, GetWindowLongPtrW, IsWindow, KillTimer, MSG,
                PostQuitMessage, RegisterClassW, SW_SHOW, SetTimer, SetWindowLongPtrW,
                SetWindowTextW, ShowWindow, UnregisterClassW, WM_ACTIVATEAPP, WM_CLOSE, WM_COMMAND,
                WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEHWHEEL,
                WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONDOWN,
                WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_CHILD, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW,
                WS_TABSTOP, WS_THICKFRAME, WS_VISIBLE,
            },
        },
    };

    use super::{
        Completion, DiagnosticMode, FocusLossSource, ForegroundState, MessageCategory,
        PASSIVE_RUN_DURATION, PassiveReadiness, ReadinessAction, duration_millis,
    };

    const WINDOW_CLASS: &[u16] = &[
        76, 111, 99, 97, 108, 75, 77, 87, 105, 110, 100, 111, 119, 115, 82, 101, 97, 100, 105, 110,
        101, 115, 115, 0,
    ];
    const START_BUTTON_ID: usize = 1;
    const RUN_TIMER_ID: usize = 1;
    const RESULT_TIMER_ID: usize = 2;
    const TIMER_POLL_INTERVAL_MS: u32 = 100;
    const SOURCE_CAPTURE_PUMP_LIMIT: usize = 128;
    const SOURCE_CAPTURE_PUMP_INTERVAL: Duration = Duration::from_millis(1);

    #[derive(Clone, Copy)]
    struct RawCaptureTiming {
        start_offset_ms: u64,
        elapsed_ms: u64,
    }

    #[derive(Clone, Copy)]
    struct RawCaptureComplete {
        stats: CaptureStats,
        timing: RawCaptureTiming,
    }

    struct RawCaptureFinished {
        result: Result<CaptureStats, InputError>,
        timing: RawCaptureTiming,
    }

    enum RawCapture {
        NotStarted,
        Running(JoinHandle<RawCaptureFinished>),
        Complete(RawCaptureComplete),
        Failed(Option<RawCaptureTiming>),
    }

    #[derive(Clone, Copy)]
    enum RawCaptureSummary {
        NotStarted,
        Unfinished,
        Complete(RawCaptureComplete),
        Failed(Option<RawCaptureTiming>),
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct SourceCategoryCounts {
        keyboard: u64,
        pointer: u64,
        scroll: u64,
    }

    impl SourceCategoryCounts {
        fn record(&mut self, event: CaptureEvent) {
            match event {
                CaptureEvent::Key { .. } => self.keyboard = self.keyboard.saturating_add(1),
                CaptureEvent::Button { .. }
                | CaptureEvent::AbsoluteMotion { .. }
                | CaptureEvent::RelativeMotion { .. }
                | CaptureEvent::LogicalAbsoluteMotion { .. }
                | CaptureEvent::LogicalRelativeMotion { .. } => {
                    self.pointer = self.pointer.saturating_add(1);
                }
                CaptureEvent::Scroll { .. } | CaptureEvent::LogicalScroll { .. } => {
                    self.scroll = self.scroll.saturating_add(1);
                }
                CaptureEvent::RouteChanged { .. } => {}
            }
        }
    }

    #[derive(Clone, Copy)]
    struct SourceCaptureComplete {
        counts: SourceCategoryCounts,
        timing: RawCaptureTiming,
    }

    #[derive(Clone, Copy)]
    enum SourceCaptureFailureCategory {
        Cancelled,
        Startup,
        Stop(StopReason),
        Cleanup,
        WorkerPanicked,
    }

    impl SourceCaptureFailureCategory {
        const fn name(self) -> &'static str {
            match self {
                Self::Cancelled => "cancelled",
                Self::Startup => "startup",
                Self::Stop(StopReason::Requested) => "unexpected_requested_stop",
                Self::Stop(StopReason::QueueFull) => "queue_full",
                Self::Stop(StopReason::QueueContended) => "queue_contended",
                Self::Stop(StopReason::ConsumerDropped) => "consumer_dropped",
                Self::Stop(StopReason::LeaseExpired) => "lease_expired",
                Self::Stop(StopReason::ClockRegression) => "clock_regression",
                Self::Stop(StopReason::InvalidInput) => "invalid_input",
                Self::Stop(StopReason::EmergencyEscape) => "emergency_escape",
                Self::Stop(StopReason::NativeFailure) => "native_failure",
                Self::Stop(StopReason::DisplaysChanged) => "displays_changed",
                Self::Cleanup => "cleanup",
                Self::WorkerPanicked => "worker_panicked",
            }
        }
    }

    #[derive(Clone, Copy)]
    struct SourceCaptureFailure {
        category: SourceCaptureFailureCategory,
        counts: SourceCategoryCounts,
        timing: RawCaptureTiming,
    }

    struct SourceCaptureFinished {
        result: Result<SourceCaptureComplete, SourceCaptureFailure>,
    }

    enum SourceCapture {
        NotStarted,
        Running {
            worker: JoinHandle<SourceCaptureFinished>,
            cancellation: Arc<AtomicBool>,
            started_at: Instant,
            run_started_at: Instant,
        },
        Complete(SourceCaptureComplete),
        Failed(SourceCaptureFailure),
    }

    #[derive(Clone, Copy)]
    enum SourceCaptureSummary {
        NotStarted,
        Unfinished,
        Complete(SourceCaptureComplete),
        Failed(SourceCaptureFailure),
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SourceCapturePoll {
        NotStarted,
        Pending,
        Complete,
        Failed,
    }

    struct WindowState {
        mode: DiagnosticMode,
        model: RefCell<PassiveReadiness>,
        start_button: Cell<HWND>,
        status_label: Cell<HWND>,
        raw_capture: RefCell<RawCapture>,
        source_capture: RefCell<SourceCapture>,
    }

    impl WindowState {
        fn new(mode: DiagnosticMode) -> Self {
            Self {
                mode,
                model: RefCell::new(PassiveReadiness::default()),
                start_button: Cell::new(ptr::null_mut()),
                status_label: Cell::new(ptr::null_mut()),
                raw_capture: RefCell::new(RawCapture::NotStarted),
                source_capture: RefCell::new(SourceCapture::NotStarted),
            }
        }
        fn set_status(&self, message: &str) {
            let status_label = self.status_label.get();
            if status_label.is_null() {
                return;
            }
            let message = wide(message);
            // SAFETY: status_label is a child of our live window and message is NUL-terminated.
            unsafe {
                SetWindowTextW(status_label, message.as_ptr());
            }
        }

        fn start(&self, hwnd: HWND) {
            // SAFETY: GetForegroundWindow only reads the current foreground window handle.
            let foreground = unsafe { GetForegroundWindow() } == hwnd;
            let action = {
                let mut model = self.model.borrow_mut();
                model.start(Instant::now(), foreground)
            };
            self.apply_or_quit(hwnd, action);
        }

        fn record(&self, hwnd: HWND, category: MessageCategory) {
            let foreground = foreground_state(hwnd);
            let (action, focus_just_lost) = {
                let mut model = self.model.borrow_mut();
                let focus_was_lost = model.first_focus_loss().is_some();
                let now = Instant::now();
                let action = match self.mode {
                    DiagnosticMode::PassiveReadiness => model.observe(now, foreground, category),
                    DiagnosticMode::PassiveSource => {
                        model.observe_source(now, foreground, category);
                        ReadinessAction::None
                    }
                };
                (
                    action,
                    !focus_was_lost && model.first_focus_loss().is_some(),
                )
            };
            if focus_just_lost {
                self.set_status("Passive check failed: this window lost foreground focus.");
            }
            self.apply_or_quit(hwnd, action);
        }

        fn note_focus_loss(&self) {
            let focus_just_lost = {
                let mut model = self.model.borrow_mut();
                let focus_was_lost = model.first_focus_loss().is_some();
                model.note_focus_loss(Instant::now(), FocusLossSource::WmActivateAppFalse);
                !focus_was_lost && model.first_focus_loss().is_some()
            };
            if focus_just_lost {
                self.set_status("Passive check failed: this window lost foreground focus.");
            }
        }

        fn advance(&self, hwnd: HWND) -> bool {
            let now = Instant::now();
            let action = match self.mode {
                DiagnosticMode::PassiveReadiness => self.model.borrow_mut().advance(now),
                DiagnosticMode::PassiveSource => match self.poll_source_capture() {
                    SourceCapturePoll::NotStarted => self.model.borrow_mut().advance(now),
                    SourceCapturePoll::Pending => ReadinessAction::None,
                    SourceCapturePoll::Complete => self.model.borrow_mut().finish_source(now),
                    SourceCapturePoll::Failed => self.model.borrow_mut().fail_native_source(now),
                },
            };
            self.apply_action(hwnd, action)
        }

        fn cancel(&self) {
            self.model.borrow_mut().cancel();
        }

        fn fail_message_loop(&self) {
            self.model.borrow_mut().fail_message_loop();
        }

        fn join_raw_capture(&self) {
            let worker = {
                let mut capture = self.raw_capture.borrow_mut();
                match mem::replace(&mut *capture, RawCapture::NotStarted) {
                    RawCapture::Running(worker) => worker,
                    capture_state => {
                        *capture = capture_state;
                        return;
                    }
                }
            };
            let completion = match worker.join() {
                Ok(RawCaptureFinished {
                    result: Ok(stats),
                    timing,
                }) => RawCapture::Complete(RawCaptureComplete { stats, timing }),
                Ok(RawCaptureFinished {
                    result: Err(_),
                    timing,
                }) => RawCapture::Failed(Some(timing)),
                Err(_) => RawCapture::Failed(None),
            };
            *self.raw_capture.borrow_mut() = completion;
        }

        fn start_source_capture(&self, run_started_at: Instant) {
            if !matches!(*self.source_capture.borrow(), SourceCapture::NotStarted) {
                return;
            }
            let cancellation = Arc::new(AtomicBool::new(false));
            let worker_cancellation = cancellation.clone();
            let started_at = Instant::now();
            let capture = match thread::Builder::new()
                .name("monhop-native-source-check".to_owned())
                .spawn(move || run_source_capture_diagnostic(run_started_at, worker_cancellation))
            {
                Ok(worker) => SourceCapture::Running {
                    worker,
                    cancellation,
                    started_at,
                    run_started_at,
                },
                Err(_) => SourceCapture::Failed(SourceCaptureFailure {
                    category: SourceCaptureFailureCategory::Startup,
                    counts: SourceCategoryCounts::default(),
                    timing: source_capture_timing(started_at, run_started_at, Instant::now()),
                }),
            };
            *self.source_capture.borrow_mut() = capture;
        }

        fn poll_source_capture(&self) -> SourceCapturePoll {
            let (worker, started_at, run_started_at) = {
                let mut capture = self.source_capture.borrow_mut();
                match mem::replace(&mut *capture, SourceCapture::NotStarted) {
                    SourceCapture::Running {
                        worker,
                        cancellation: _,
                        started_at,
                        run_started_at,
                    } if worker.is_finished() => (worker, started_at, run_started_at),
                    capture_state => {
                        *capture = capture_state;
                        return match &*capture {
                            SourceCapture::Complete(_) => SourceCapturePoll::Complete,
                            SourceCapture::Failed(_) => SourceCapturePoll::Failed,
                            SourceCapture::NotStarted => SourceCapturePoll::NotStarted,
                            SourceCapture::Running { .. } => SourceCapturePoll::Pending,
                        };
                    }
                }
            };
            let completion = match worker.join() {
                Ok(SourceCaptureFinished {
                    result: Ok(capture),
                }) => SourceCapture::Complete(capture),
                Ok(SourceCaptureFinished {
                    result: Err(failure),
                }) => SourceCapture::Failed(failure),
                Err(_) => SourceCapture::Failed(SourceCaptureFailure {
                    category: SourceCaptureFailureCategory::WorkerPanicked,
                    counts: SourceCategoryCounts::default(),
                    timing: source_capture_timing(started_at, run_started_at, Instant::now()),
                }),
            };
            let poll = match completion {
                SourceCapture::Complete(_) => SourceCapturePoll::Complete,
                SourceCapture::Failed(_) => SourceCapturePoll::Failed,
                SourceCapture::NotStarted => SourceCapturePoll::NotStarted,
                SourceCapture::Running { .. } => SourceCapturePoll::Pending,
            };
            *self.source_capture.borrow_mut() = completion;
            poll
        }

        fn join_source_capture(&self) {
            let source = {
                let mut capture = self.source_capture.borrow_mut();
                mem::replace(&mut *capture, SourceCapture::NotStarted)
            };
            let completion = match source {
                SourceCapture::Running {
                    worker,
                    cancellation,
                    started_at,
                    run_started_at,
                } => {
                    cancellation.store(true, Ordering::Release);
                    match worker.join() {
                        Ok(SourceCaptureFinished {
                            result: Ok(capture),
                        }) => SourceCapture::Complete(capture),
                        Ok(SourceCaptureFinished {
                            result: Err(failure),
                        }) => SourceCapture::Failed(failure),
                        Err(_) => SourceCapture::Failed(SourceCaptureFailure {
                            category: SourceCaptureFailureCategory::WorkerPanicked,
                            counts: SourceCategoryCounts::default(),
                            timing: source_capture_timing(
                                started_at,
                                run_started_at,
                                Instant::now(),
                            ),
                        }),
                    }
                }
                other => other,
            };
            *self.source_capture.borrow_mut() = completion;
        }

        fn apply_or_quit(&self, hwnd: HWND, action: ReadinessAction) {
            if self.apply_action(hwnd, action) {
                // SAFETY: posts a quit message only for this process message loop.
                unsafe {
                    PostQuitMessage(0);
                }
            }
        }

        fn apply_action(&self, hwnd: HWND, action: ReadinessAction) -> bool {
            match action {
                ReadinessAction::None => false,
                ReadinessAction::ArmRunTimer => {
                    // SAFETY: hwnd is our live window and this timer has no callback.
                    if unsafe { SetTimer(hwnd, RUN_TIMER_ID, TIMER_POLL_INTERVAL_MS, None) } == 0 {
                        let action = {
                            let mut model = self.model.borrow_mut();
                            model.fail_setup(Instant::now())
                        };
                        return self.apply_action(hwnd, action);
                    }
                    let start_button = self.start_button.get();
                    if !start_button.is_null() {
                        // SAFETY: start_button is our live child control and this test starts once.
                        unsafe {
                            EnableWindow(start_button, 0);
                        }
                    }
                    // SAFETY: moves focus from the Start child back to our own window before input.
                    unsafe {
                        SetFocus(hwnd);
                    }
                    self.set_status(match self.mode {
                        DiagnosticMode::PassiveReadiness => {
                            "Running: use this window. SendInput is disabled; Raw Input is aggregate only."
                        }
                        DiagnosticMode::PassiveSource => {
                            "Running: passive source capture only. SendInput and suppression are not permitted."
                        }
                    });
                    println!("native_window phase=CAPTURING");
                    if let Some(run_started_at) = self.model.borrow().run_started_at() {
                        match self.mode {
                            DiagnosticMode::PassiveReadiness => {
                                self.start_raw_capture(run_started_at);
                            }
                            DiagnosticMode::PassiveSource => {
                                self.start_source_capture(run_started_at);
                            }
                        }
                    }
                    false
                }
                ReadinessAction::ArmResultTimer => {
                    // SAFETY: hwnd is our live window and the run timer may be absent.
                    unsafe {
                        KillTimer(hwnd, RUN_TIMER_ID);
                    }
                    let status = self.result_status();
                    self.set_status(status);
                    // SAFETY: hwnd is our live window and this timer has no callback.
                    unsafe { SetTimer(hwnd, RESULT_TIMER_ID, TIMER_POLL_INTERVAL_MS, None) == 0 }
                }
                ReadinessAction::Exit => true,
            }
        }

        fn start_raw_capture(&self, run_started_at: Instant) {
            if !matches!(*self.raw_capture.borrow(), RawCapture::NotStarted) {
                return;
            }
            let capture = match thread::Builder::new()
                .name("monhop-raw-input-check".to_owned())
                .spawn(move || run_raw_input_diagnostic(run_started_at))
            {
                Ok(worker) => RawCapture::Running(worker),
                Err(_) => RawCapture::Failed(None),
            };
            *self.raw_capture.borrow_mut() = capture;
        }

        fn result_status(&self) -> &'static str {
            let model = self.model.borrow();
            if self.mode == DiagnosticMode::PassiveSource {
                return match model.completion() {
                    Some(Completion::PassiveResult) if model.passive_passed() => {
                        "Passive source capture completed. Injection is NOT_VERIFIED; suppression was NOT_PERMITTED."
                    }
                    Some(Completion::PassiveResult) => {
                        "Passive source capture completed with window-category or focus failure."
                    }
                    Some(Completion::StartRejectedForFocus) => {
                        "Start rejected: make this test window foreground, then retry."
                    }
                    Some(Completion::SetupFailure) => "Setup failed before passive source capture.",
                    Some(Completion::NativeSourceFailure) => {
                        "Native source capture failed before a passive result."
                    }
                    Some(Completion::Cancelled) | Some(Completion::MessageLoopFailure) | None => {
                        "Passive source capture ended before a result."
                    }
                };
            }
            match model.completion() {
                Some(Completion::PassiveResult) if model.passive_passed() => {
                    "Window categories passed. Injection remains NOT_VERIFIED."
                }
                Some(Completion::PassiveResult) => {
                    "Window categories failed. Injection remains NOT_VERIFIED."
                }
                Some(Completion::StartRejectedForFocus) => {
                    "Start rejected: make this test window foreground, then retry."
                }
                Some(Completion::SetupFailure) => "Setup failed before the passive check.",
                Some(Completion::NativeSourceFailure) => {
                    "Native source capture failed before a passive result."
                }
                Some(Completion::Cancelled) | Some(Completion::MessageLoopFailure) | None => {
                    "Passive check ended before a result."
                }
            }
        }

        fn raw_capture_summary(&self) -> RawCaptureSummary {
            match &*self.raw_capture.borrow() {
                RawCapture::NotStarted => RawCaptureSummary::NotStarted,
                RawCapture::Running(_) => RawCaptureSummary::Unfinished,
                RawCapture::Complete(capture) => RawCaptureSummary::Complete(*capture),
                RawCapture::Failed(timing) => RawCaptureSummary::Failed(*timing),
            }
        }

        fn source_capture_summary(&self) -> SourceCaptureSummary {
            match &*self.source_capture.borrow() {
                SourceCapture::NotStarted => SourceCaptureSummary::NotStarted,
                SourceCapture::Running { .. } => SourceCaptureSummary::Unfinished,
                SourceCapture::Complete(capture) => SourceCaptureSummary::Complete(*capture),
                SourceCapture::Failed(failure) => SourceCaptureSummary::Failed(*failure),
            }
        }
    }

    fn drain_source_events(
        capture: &mut NativeCapture,
        counts: &mut SourceCategoryCounts,
    ) -> Result<(), StopReason> {
        for _ in 0..SOURCE_CAPTURE_PUMP_LIMIT {
            match capture.try_next_event() {
                Ok(Some(record)) => counts.record(record.event),
                Ok(None) => return Ok(()),
                Err(reason) => return Err(reason),
            }
        }
        Ok(())
    }

    fn run_source_capture_diagnostic(
        run_started_at: Instant,
        cancellation: Arc<AtomicBool>,
    ) -> SourceCaptureFinished {
        let native_started_at = Instant::now();
        let mut counts = SourceCategoryCounts::default();
        let mut capture = match NativeCapture::start_diagnostic(PASSIVE_RUN_DURATION) {
            Ok(capture) => capture,
            Err(_) => {
                return SourceCaptureFinished {
                    result: Err(SourceCaptureFailure {
                        category: SourceCaptureFailureCategory::Startup,
                        counts,
                        timing: source_capture_timing(
                            native_started_at,
                            run_started_at,
                            Instant::now(),
                        ),
                    }),
                };
            }
        };

        let mut failure = None;
        loop {
            if cancellation.load(Ordering::Acquire) {
                capture.request_stop();
                failure = Some(SourceCaptureFailureCategory::Cancelled);
            }

            match drain_source_events(&mut capture, &mut counts) {
                Ok(()) => {}
                Err(StopReason::Requested) if failure.is_none() => {}
                Err(reason) if failure.is_none() => {
                    failure = Some(SourceCaptureFailureCategory::Stop(reason));
                }
                Err(_) => {}
            }

            if failure.is_some() || capture.is_finished() {
                let terminal_reason = capture.stop_reason();
                let cleanup = finish_source_capture(&mut capture);
                let completed_at = Instant::now();
                let timing = source_capture_timing(native_started_at, run_started_at, completed_at);
                let requested_at_diagnostic_deadline = !cancellation.load(Ordering::Acquire)
                    && terminal_reason == Some(StopReason::Requested)
                    && completed_at.saturating_duration_since(native_started_at)
                        >= PASSIVE_RUN_DURATION;
                let result = match failure {
                    Some(category) => Err(SourceCaptureFailure {
                        category,
                        counts,
                        timing,
                    }),
                    None if requested_at_diagnostic_deadline && cleanup.is_ok() => {
                        Ok(SourceCaptureComplete { counts, timing })
                    }
                    None if cleanup.is_err() => Err(SourceCaptureFailure {
                        category: SourceCaptureFailureCategory::Cleanup,
                        counts,
                        timing,
                    }),
                    None => Err(SourceCaptureFailure {
                        category: terminal_reason.map_or(
                            SourceCaptureFailureCategory::Cleanup,
                            SourceCaptureFailureCategory::Stop,
                        ),
                        counts,
                        timing,
                    }),
                };
                return SourceCaptureFinished { result };
            }

            thread::sleep(SOURCE_CAPTURE_PUMP_INTERVAL);
        }
    }

    fn finish_source_capture(capture: &mut NativeCapture) -> Result<(), NativeCaptureError> {
        loop {
            match capture.finish() {
                Ok(()) => return Ok(()),
                Err(NativeCaptureError::CleanupPending) => {
                    thread::sleep(SOURCE_CAPTURE_PUMP_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn source_capture_timing(
        native_started_at: Instant,
        run_started_at: Instant,
        completed_at: Instant,
    ) -> RawCaptureTiming {
        RawCaptureTiming {
            start_offset_ms: duration_millis(
                native_started_at.saturating_duration_since(run_started_at),
            ),
            elapsed_ms: duration_millis(completed_at.saturating_duration_since(native_started_at)),
        }
    }

    struct RegisteredClass {
        module: HINSTANCE,
    }

    impl RegisteredClass {
        fn register(module: HINSTANCE) -> Result<Self, SetupError> {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module,
                lpszClassName: WINDOW_CLASS.as_ptr(),
                ..Default::default()
            };
            // SAFETY: class contains initialized values that remain valid for this call.
            if unsafe { RegisterClassW(&class) } == 0 {
                return Err(SetupError::RegisterClass);
            }
            Ok(Self { module })
        }
    }

    impl Drop for RegisteredClass {
        fn drop(&mut self) {
            // SAFETY: all windows of this class are destroyed before this guard drops.
            unsafe {
                UnregisterClassW(WINDOW_CLASS.as_ptr(), self.module);
            }
        }
    }

    struct ControlledWindow {
        hwnd: HWND,
    }

    impl ControlledWindow {
        fn create(module: HINSTANCE, state: &WindowState) -> Result<Self, SetupError> {
            let title = wide(match state.mode {
                DiagnosticMode::PassiveReadiness => "MonHop Windows readiness diagnostic",
                DiagnosticMode::PassiveSource => "MonHop Windows passive source diagnostic",
            });
            // SAFETY: the class is registered and state remains alive until this window is dropped.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    WINDOW_CLASS.as_ptr(),
                    title.as_ptr(),
                    (WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX)) | WS_VISIBLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    560,
                    270,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    module,
                    (state as *const WindowState).cast(),
                )
            };
            if hwnd.is_null() {
                return Err(SetupError::CreateWindow);
            }
            let window = Self { hwnd };
            window.create_controls(module, state)?;
            // SAFETY: hwnd is our visible top-level window.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            Ok(window)
        }

        fn create_controls(
            &self,
            module: HINSTANCE,
            state: &WindowState,
        ) -> Result<(), SetupError> {
            let (line_one, line_two, line_three, line_four, button_label) = match state.mode {
                DiagnosticMode::PassiveReadiness => (
                    "This is a passive readiness check. It never calls SendInput.",
                    "Click Start, then use keyboard, pointer, and scroll input in this window.",
                    "Window categories are local; Raw Input counts are system-wide. Provenance is not verified.",
                    "Deadline checks run between messages; titlebar moves can delay UI dispatch.",
                    "Start passive check",
                ),
                DiagnosticMode::PassiveSource => (
                    "This is a passive source-capture diagnostic. It never calls SendInput.",
                    "Click Start, then use keyboard, pointer, and scroll input in this window.",
                    "Native capture is bounded and passive: suppression and injection are not permitted.",
                    "Source categories are not window-scoped; focus and physical provenance are not verified.",
                    "Start source diagnostic",
                ),
            };
            create_label(self.hwnd, module, line_one, 16)?;
            create_label(self.hwnd, module, line_two, 44)?;
            create_label(self.hwnd, module, line_three, 72)?;
            create_label(self.hwnd, module, line_four, 100)?;
            state.status_label.set(create_label(
                self.hwnd,
                module,
                "Waiting for the explicit Start action.",
                128,
            )?);

            let button_class = wide("BUTTON");
            let button_label = wide(button_label);
            // SAFETY: all strings are NUL-terminated and the button is a child of our live window.
            let button = unsafe {
                CreateWindowExW(
                    0,
                    button_class.as_ptr(),
                    button_label.as_ptr(),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                    16,
                    168,
                    180,
                    32,
                    self.hwnd,
                    START_BUTTON_ID as *mut core::ffi::c_void,
                    module,
                    ptr::null(),
                )
            };
            if button.is_null() {
                return Err(SetupError::CreateControl);
            }
            state.start_button.set(button);
            Ok(())
        }
    }

    impl Drop for ControlledWindow {
        fn drop(&mut self) {
            // SAFETY: timers belong to this window; IsWindow avoids destroying a user-closed window.
            unsafe {
                KillTimer(self.hwnd, RUN_TIMER_ID);
                KillTimer(self.hwnd, RESULT_TIMER_ID);
                if IsWindow(self.hwnd) != 0 {
                    DestroyWindow(self.hwnd);
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum SetupError {
        ModuleHandle,
        RegisterClass,
        CreateWindow,
        CreateControl,
    }

    impl SetupError {
        const fn category(self) -> &'static str {
            match self {
                Self::ModuleHandle => "module_handle",
                Self::RegisterClass => "register_class",
                Self::CreateWindow => "create_window",
                Self::CreateControl => "create_control",
            }
        }
    }

    pub(super) fn run(mode: DiagnosticMode) -> ExitCode {
        let module = match module_handle() {
            Ok(module) => module,
            Err(error) => return setup_failure(error),
        };
        let class = match RegisteredClass::register(module) {
            Ok(class) => class,
            Err(error) => return setup_failure(error),
        };
        let state = Box::new(WindowState::new(mode));
        let window = match ControlledWindow::create(module, state.as_ref()) {
            Ok(window) => window,
            Err(error) => return setup_failure(error),
        };

        println!("native_window phase=WAITING_FOR_START");
        let loop_failed = run_message_loop(window.hwnd, state.as_ref());
        if loop_failed {
            state.fail_message_loop();
        }
        match mode {
            DiagnosticMode::PassiveReadiness => state.join_raw_capture(),
            DiagnosticMode::PassiveSource => state.join_source_capture(),
        }
        drop(window);
        drop(class);
        println!("native_window phase=FINISHED");
        report(state.as_ref())
    }

    fn create_label(
        parent: HWND,
        module: HINSTANCE,
        text: &str,
        y: i32,
    ) -> Result<HWND, SetupError> {
        let class = wide("STATIC");
        let text = wide(text);
        // SAFETY: all strings are NUL-terminated and the label is a child of our live window.
        let label = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE,
                16,
                y,
                520,
                24,
                parent,
                ptr::null_mut(),
                module,
                ptr::null(),
            )
        };
        if label.is_null() {
            return Err(SetupError::CreateControl);
        }
        Ok(label)
    }

    fn module_handle() -> Result<HINSTANCE, SetupError> {
        // SAFETY: null requests the current module handle without loading a library.
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        if module.is_null() {
            return Err(SetupError::ModuleHandle);
        }
        Ok(module)
    }

    fn setup_failure(error: SetupError) -> ExitCode {
        eprintln!(
            "native_window status=SETUP_FAILED category={}",
            error.category()
        );
        ExitCode::from(1)
    }

    fn run_message_loop(hwnd: HWND, state: &WindowState) -> bool {
        let mut message = MSG::default();
        loop {
            if state.advance(hwnd) {
                return false;
            }
            // SAFETY: message is writable and hwnd is our valid top-level window.
            let result = unsafe { GetMessageW(&mut message, hwnd, 0, 0) };
            if result == -1 {
                // SAFETY: GetLastError only reads this thread's error state.
                let _ = unsafe { GetLastError() };
                return true;
            }
            if result == 0 {
                return false;
            }
            if state.advance(hwnd) {
                return false;
            }
            if message.message == WM_TIMER {
                continue;
            }
            // SAFETY: message was initialized by GetMessageW and targets this process window.
            unsafe {
                DispatchMessageW(&message);
            }
        }
    }

    fn report(state: &WindowState) -> ExitCode {
        match state.mode {
            DiagnosticMode::PassiveReadiness => report_passive_readiness(state),
            DiagnosticMode::PassiveSource => report_passive_source(state),
        }
    }

    fn report_passive_readiness(state: &WindowState) -> ExitCode {
        let (completion, passive_passed, categories, focus_loss) = {
            let model = state.model.borrow();
            (
                model.completion().unwrap_or(Completion::Cancelled),
                model.passive_passed(),
                model.categories(),
                model.first_focus_loss(),
            )
        };
        let exit_code = match completion {
            Completion::PassiveResult => {
                let passive = if passive_passed { "PASS" } else { "FAIL" };
                let focus = focus_loss.map_or("held", |_| "lost");
                println!(
                    "native_window status=NOT_VERIFIED injection=not_attempted marker=not_observed passive={passive} window_categories={} window_message_provenance=not_verified focus={focus} focus_loss_category={} focus_loss_elapsed_ms={} focus_loss_window_categories={} deadline_scope=message_loop",
                    categories.summary(),
                    focus_loss.map_or("none", |loss| loss.source.category()),
                    focus_loss
                        .map_or_else(|| "none".to_owned(), |loss| loss.elapsed_ms.to_string()),
                    focus_loss.map_or("none", |loss| loss.categories.summary()),
                );
                ExitCode::from(2)
            }
            Completion::StartRejectedForFocus => {
                println!(
                    "native_window status=NOT_VERIFIED injection=not_attempted marker=not_observed passive=FAIL category=focus"
                );
                ExitCode::from(2)
            }
            Completion::Cancelled => {
                println!("native_window status=ABORTED category=user_cancelled");
                ExitCode::from(2)
            }
            Completion::SetupFailure => {
                println!("native_window status=SETUP_FAILED category=timer");
                ExitCode::from(1)
            }
            Completion::NativeSourceFailure => {
                println!("native_window status=SETUP_FAILED category=native_source");
                ExitCode::from(1)
            }
            Completion::MessageLoopFailure => {
                println!("native_window status=SETUP_FAILED category=message_loop");
                ExitCode::from(1)
            }
        };
        print_raw_capture(state.raw_capture_summary());
        exit_code
    }

    fn report_passive_source(state: &WindowState) -> ExitCode {
        let (completion, passive_passed, categories, focus_loss) = {
            let model = state.model.borrow();
            (
                model.completion().unwrap_or(Completion::Cancelled),
                model.passive_passed(),
                model.categories(),
                model.first_focus_loss(),
            )
        };
        let exit_code = match completion {
            Completion::PassiveResult => {
                let passive = if passive_passed { "PASS" } else { "FAIL" };
                let focus = focus_loss.map_or("held", |_| "lost");
                println!(
                    "native_window status=NOT_VERIFIED injection=not_attempted suppression=not_permitted marker=not_observed passive={passive} window_categories={} window_message_provenance=not_verified focus={focus} focus_loss_category={} focus_loss_elapsed_ms={} focus_loss_window_categories={} deadline_scope=message_loop",
                    categories.summary(),
                    focus_loss.map_or("none", |loss| loss.source.category()),
                    focus_loss
                        .map_or_else(|| "none".to_owned(), |loss| loss.elapsed_ms.to_string()),
                    focus_loss.map_or("none", |loss| loss.categories.summary()),
                );
                ExitCode::from(2)
            }
            Completion::StartRejectedForFocus => {
                println!(
                    "native_window status=NOT_VERIFIED injection=not_attempted suppression=not_permitted marker=not_observed passive=FAIL category=focus"
                );
                ExitCode::from(2)
            }
            Completion::Cancelled => {
                println!("native_window status=ABORTED category=user_cancelled");
                ExitCode::from(2)
            }
            Completion::SetupFailure => {
                println!(
                    "native_window status=SETUP_FAILED category=timer injection=not_attempted suppression=not_permitted marker=not_observed"
                );
                ExitCode::from(1)
            }
            Completion::NativeSourceFailure => {
                println!(
                    "native_window status=SETUP_FAILED category=native_source injection=not_attempted suppression=not_permitted marker=not_observed"
                );
                ExitCode::from(1)
            }
            Completion::MessageLoopFailure => {
                println!("native_window status=SETUP_FAILED category=message_loop");
                ExitCode::from(1)
            }
        };
        print_source_capture(state.source_capture_summary());
        exit_code
    }

    fn print_raw_capture(capture: RawCaptureSummary) {
        match capture {
            RawCaptureSummary::NotStarted => {
                println!("native_window raw_input=not_started raw_input_scope=system_wide")
            }
            RawCaptureSummary::Unfinished => {
                println!("native_window raw_input=unfinished raw_input_scope=system_wide")
            }
            RawCaptureSummary::Complete(capture) => println!(
                "native_window raw_input=complete raw_input_scope=system_wide start_offset_ms={} elapsed_ms={} keyboard_events={} mouse_events={} motion_events={} button_events={} scroll_events={} monhop_marked_events={} provenance=not_verified",
                capture.timing.start_offset_ms,
                capture.timing.elapsed_ms,
                capture.stats.keyboard_events,
                capture.stats.mouse_events,
                capture.stats.mouse_motion_events,
                capture.stats.button_events,
                capture.stats.scroll_events,
                capture.stats.filtered_injected_events,
            ),
            RawCaptureSummary::Failed(Some(timing)) => println!(
                "native_window raw_input=failed raw_input_scope=system_wide start_offset_ms={} elapsed_ms={}",
                timing.start_offset_ms, timing.elapsed_ms,
            ),
            RawCaptureSummary::Failed(None) => {
                println!(
                    "native_window raw_input=failed raw_input_scope=system_wide timing=unavailable"
                )
            }
        }
    }

    fn print_source_capture(capture: SourceCaptureSummary) {
        match capture {
            SourceCaptureSummary::NotStarted => println!(
                "native_window source_capture=not_started source_capture_scope=not_window_scoped source_capture_focus_provenance=not_verified"
            ),
            SourceCaptureSummary::Unfinished => println!(
                "native_window source_capture=unfinished source_capture_scope=not_window_scoped source_capture_focus_provenance=not_verified provenance=not_verified"
            ),
            SourceCaptureSummary::Complete(capture) => println!(
                "native_window source_capture=complete source_capture_scope=not_window_scoped source_capture_focus_provenance=not_verified start_offset_ms={} elapsed_ms={} keyboard_events={} pointer_events={} scroll_events={} provenance=not_verified",
                capture.timing.start_offset_ms,
                capture.timing.elapsed_ms,
                capture.counts.keyboard,
                capture.counts.pointer,
                capture.counts.scroll,
            ),
            SourceCaptureSummary::Failed(failure) => println!(
                "native_window source_capture=failed category={} source_capture_scope=not_window_scoped source_capture_focus_provenance=not_verified start_offset_ms={} elapsed_ms={} keyboard_events={} pointer_events={} scroll_events={} provenance=not_verified",
                failure.category.name(),
                failure.timing.start_offset_ms,
                failure.timing.elapsed_ms,
                failure.counts.keyboard,
                failure.counts.pointer,
                failure.counts.scroll,
            ),
        }
    }

    fn run_raw_input_diagnostic(run_started_at: Instant) -> RawCaptureFinished {
        let capture_started_at = Instant::now();
        let result = capture_counts(PASSIVE_RUN_DURATION);
        RawCaptureFinished {
            result,
            timing: RawCaptureTiming {
                start_offset_ms: duration_millis(
                    capture_started_at.saturating_duration_since(run_started_at),
                ),
                elapsed_ms: duration_millis(capture_started_at.elapsed()),
            },
        }
    }

    fn foreground_state(hwnd: HWND) -> ForegroundState {
        // SAFETY: GetForegroundWindow only reads the current foreground window handle.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground == hwnd {
            ForegroundState::ThisWindow
        } else if foreground.is_null() {
            ForegroundState::Null
        } else {
            ForegroundState::Other
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            // SAFETY: WM_NCCREATE supplies the CREATESTRUCTW from our CreateWindowExW call.
            let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
            // SAFETY: state outlives this window and is cleared by window destruction.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            // SAFETY: preserves normal non-client initialization after recording our state pointer.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }

        if message == WM_NCDESTROY {
            // SAFETY: no callback may retain state after this window's final non-client teardown.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            // SAFETY: forwards the final teardown message with its original arguments.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }

        // SAFETY: WM_NCCREATE stored this pointer until WM_NCDESTROY clears it.
        let state =
            unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState).as_ref() };
        if let Some(state) = state {
            match message {
                WM_COMMAND
                    if low_word(wparam) == START_BUTTON_ID as u16
                        && high_word(wparam) == BN_CLICKED as u16
                        && lparam as HWND == state.start_button.get() =>
                {
                    state.start(hwnd);
                    return 0;
                }
                WM_KEYDOWN | WM_KEYUP => state.record(hwnd, MessageCategory::Keyboard),
                WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP => {
                    state.record(hwnd, MessageCategory::Pointer);
                }
                WM_MOUSEWHEEL | WM_MOUSEHWHEEL => state.record(hwnd, MessageCategory::Scroll),
                WM_ACTIVATEAPP if wparam == 0 => state.note_focus_loss(),
                WM_CLOSE => {
                    state.cancel();
                    // SAFETY: exits only our message loop; window destruction waits for Raw Input join.
                    unsafe {
                        PostQuitMessage(0);
                    }
                    return 0;
                }
                WM_DESTROY => {
                    // SAFETY: posts a quit message only for this process message loop.
                    unsafe {
                        PostQuitMessage(0);
                    }
                    return 0;
                }
                _ => {}
            }
        }

        // SAFETY: forwards unhandled messages to Windows with the original arguments.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    const fn low_word(value: WPARAM) -> u16 {
        value as u16
    }

    const fn high_word(value: WPARAM) -> u16 {
        (value >> 16) as u16
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Completion, DiagnosticMode, FocusLoss, FocusLossSource, ForegroundState, MessageCategory,
        ObservedCategories, PASSIVE_RUN_DURATION, PassiveReadiness, RESULT_VISIBLE_DURATION,
        ReadinessAction, explicit_run_argument, explicit_run_mode,
    };
    use std::{
        ffi::OsString,
        time::{Duration, Instant},
    };

    #[test]
    fn foreground_window_categories_pass_at_the_monotonic_deadline() {
        let start = Instant::now();
        let mut model = PassiveReadiness::default();
        assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
        for category in [
            MessageCategory::Keyboard,
            MessageCategory::Pointer,
            MessageCategory::Scroll,
        ] {
            assert_eq!(
                model.observe(start, ForegroundState::ThisWindow, category),
                ReadinessAction::None
            );
        }
        assert_eq!(
            model.advance(start + PASSIVE_RUN_DURATION),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(model.completion(), Some(Completion::PassiveResult));
        assert!(model.passive_passed());
        assert_eq!(
            model.advance(start + PASSIVE_RUN_DURATION + RESULT_VISIBLE_DURATION),
            ReadinessAction::Exit
        );
    }

    #[test]
    fn first_focus_loss_preserves_source_timing_and_categories() {
        let start = Instant::now();
        let mut model = PassiveReadiness::default();
        assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
        assert_eq!(
            model.observe(
                start + Duration::from_millis(3),
                ForegroundState::ThisWindow,
                MessageCategory::Keyboard,
            ),
            ReadinessAction::None
        );
        model.note_focus_loss(
            start + Duration::from_millis(7),
            FocusLossSource::WmActivateAppFalse,
        );
        assert_eq!(
            model.observe(
                start + Duration::from_millis(11),
                ForegroundState::Other,
                MessageCategory::Pointer,
            ),
            ReadinessAction::None
        );
        assert_eq!(
            model.first_focus_loss(),
            Some(FocusLoss {
                source: FocusLossSource::WmActivateAppFalse,
                elapsed_ms: 7,
                categories: ObservedCategories {
                    keyboard: true,
                    pointer: false,
                    scroll: false,
                },
            })
        );
        assert_eq!(
            model.advance(start + PASSIVE_RUN_DURATION),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(model.completion(), Some(Completion::PassiveResult));
        assert!(!model.passive_passed());
    }

    #[test]
    fn foreground_loss_categories_are_recorded_without_window_identity() {
        let start = Instant::now();
        for (foreground, source, category) in [
            (
                ForegroundState::Null,
                FocusLossSource::ForegroundNull,
                "foreground_null",
            ),
            (
                ForegroundState::Other,
                FocusLossSource::ForegroundOther,
                "foreground_other",
            ),
        ] {
            let mut model = PassiveReadiness::default();
            assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
            assert_eq!(
                model.observe(
                    start + Duration::from_millis(5),
                    foreground,
                    MessageCategory::Scroll
                ),
                ReadinessAction::None
            );
            assert_eq!(model.first_focus_loss().unwrap().source, source);
            assert_eq!(
                model.first_focus_loss().unwrap().source.category(),
                category
            );
            assert_eq!(model.first_focus_loss().unwrap().elapsed_ms, 5);
            assert_eq!(
                model.first_focus_loss().unwrap().categories.summary(),
                "none"
            );
        }
    }

    #[test]
    fn deadline_precedes_a_late_window_message() {
        let start = Instant::now();
        let mut model = PassiveReadiness::default();
        assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
        assert_eq!(
            model.observe(
                start + PASSIVE_RUN_DURATION,
                ForegroundState::ThisWindow,
                MessageCategory::Keyboard
            ),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(model.categories().summary(), "none");
    }

    #[test]
    fn accepted_start_excludes_long_wait_from_run_elapsed() {
        let created_at = Instant::now();
        let accepted_start = created_at + Duration::from_secs(3_600);
        let mut model = PassiveReadiness::default();
        assert_eq!(
            model.start(accepted_start, true),
            ReadinessAction::ArmRunTimer
        );
        assert_eq!(
            model.run_elapsed_ms(accepted_start + Duration::from_millis(14_000)),
            Some(14_000)
        );
    }

    #[test]
    fn source_observation_waits_for_native_completion_after_the_ui_deadline() {
        let start = Instant::now();
        let mut model = PassiveReadiness::default();
        assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
        model.observe_source(
            start + PASSIVE_RUN_DURATION + Duration::from_secs(1),
            ForegroundState::ThisWindow,
            MessageCategory::Keyboard,
        );
        assert!(model.is_running());
        assert_eq!(model.completion(), None);
        assert_eq!(
            model.finish_source(start + PASSIVE_RUN_DURATION + Duration::from_secs(1)),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(model.completion(), Some(Completion::PassiveResult));
    }

    #[test]
    fn native_source_failure_is_terminal_and_uses_the_result_deadline() {
        let start = Instant::now();
        let mut model = PassiveReadiness::default();
        assert_eq!(model.start(start, true), ReadinessAction::ArmRunTimer);
        assert_eq!(
            model.fail_native_source(start + Duration::from_millis(4)),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(model.completion(), Some(Completion::NativeSourceFailure));
        assert_eq!(
            model.fail_native_source(start + Duration::from_millis(4) + RESULT_VISIBLE_DURATION),
            ReadinessAction::Exit
        );
    }

    #[test]
    fn terminal_states_do_not_claim_a_passive_result() {
        let now = Instant::now();

        let mut cancelled = PassiveReadiness::default();
        cancelled.cancel();
        assert_eq!(cancelled.completion(), Some(Completion::Cancelled));

        let mut setup_failure = PassiveReadiness::default();
        assert_eq!(
            setup_failure.fail_setup(now),
            ReadinessAction::ArmResultTimer
        );
        assert_eq!(setup_failure.completion(), Some(Completion::SetupFailure));

        let mut message_loop_failure = PassiveReadiness::default();
        message_loop_failure.fail_message_loop();
        assert_eq!(
            message_loop_failure.completion(),
            Some(Completion::MessageLoopFailure)
        );
    }

    #[test]
    fn exact_run_argument_rejects_extra_arguments() {
        assert!(explicit_run_argument([
            OsString::from("native_window"),
            OsString::from("--run")
        ]));
        assert!(!explicit_run_argument([
            OsString::from("native_window"),
            OsString::from("--run"),
            OsString::from("extra"),
        ]));
    }

    #[test]
    fn explicit_modes_are_exact_and_keep_the_passive_receipt_mode() {
        assert_eq!(
            explicit_run_mode([OsString::from("native_window"), OsString::from("--run")]),
            Some(DiagnosticMode::PassiveReadiness)
        );
        assert_eq!(
            explicit_run_mode([
                OsString::from("native_window"),
                OsString::from("--run-source"),
            ]),
            Some(DiagnosticMode::PassiveSource)
        );
        assert_eq!(explicit_run_mode([OsString::from("native_window")]), None);
        assert_eq!(
            explicit_run_mode([
                OsString::from("native_window"),
                OsString::from("--run-source"),
                OsString::from("extra"),
            ]),
            None
        );
    }
}
