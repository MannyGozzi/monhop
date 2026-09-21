//! Screenshot-excluded overlays, owned entirely by a dedicated Win32 message loop.

use super::Retry;
use monhop_core::dimming::{Chord, DimLevel};
use monhop_platform_windows::keymap::set1_from_hid_usage;
use std::{
    mem::size_of,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Wdk::System::SystemServices::RtlGetVersion,
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Dwm::DwmIsCompositionEnabled,
            Gdi::{BLACK_BRUSH, EnumDisplayMonitors, GetStockObject, HBRUSH, HDC, HMONITOR},
        },
        System::{
            LibraryLoader::GetModuleHandleW, SystemInformation::OSVERSIONINFOW,
            Threading::GetCurrentThreadId,
        },
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            Input::KeyboardAndMouse::{
                MAPVK_VSC_TO_VK_EX, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
                MapVirtualKeyW, RegisterHotKey, UnregisterHotKey,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, PCWSTR, w},
};

const FADE_TIME: Duration = Duration::from_millis(180);
const RESPONSE_TIME: Duration = Duration::from_secs(1);
/// How long the first retry waits after a chord turns out to be taken, and the longest any later
/// one waits: a chord another app is holding is not an emergency, so the wait doubles up to this.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_LIMIT: Duration = Duration::from_secs(60);
const APPLY: u32 = WM_APP + 41;
const DISPLAYS_CHANGED: u32 = WM_APP + 42;
const HOTKEY_ID: i32 = 1;
const FADE_TIMER: usize = 1;
const RETRY_TIMER: usize = 2;
const CONTROL_CLASS: PCWSTR = w!("MonHop.Dimming.Control");
const OVERLAY_CLASS: PCWSTR = w!("MonHop.Dimming.Overlay");
const UNAVAILABLE: &str = "Screen dimming could not start. Restart MonHop and try again.";
const HELD: &str =
    "The dimming shortcut is held by another app. MonHop keeps trying; Dim now still works.";
const UNUSABLE_KEY: &str = "This key cannot be used for the dimming shortcut.";
type Press = Arc<dyn Fn() + Send + Sync>;
static WORKER: OnceLock<Result<Arc<Worker>, String>> = OnceLock::new();

struct Worker {
    hwnd: AtomicUsize,
    thread: AtomicU32,
    failed: AtomicBool,
    pending: Mutex<Pending>,
}
struct WorkerLifetime(Arc<Worker>);
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        self.0.failed.store(true, Ordering::Release);
        self.0.hwnd.store(0, Ordering::Release);
        if let Some(Some(request)) = lock(&self.0.pending).hotkey.take() {
            request.finish(Err(UNAVAILABLE.into()));
        }
    }
}
#[derive(Default)]
struct Pending {
    wake_posted: bool,
    shown: Option<bool>,
    level: Option<DimLevel>,
    hotkey: Option<Option<Arc<Registration>>>,
}
struct Registration {
    chord: Chord,
    press: Press,
    /// Checked before, and reported after, every retry the worker makes for this chord.
    retry: Retry,
    response: Mutex<Response>,
    ready: Condvar,
}
#[derive(Default)]
struct Response {
    canceled: bool,
    result: Option<Result<(), String>>,
}
/// Why a registration did not take. A chord another app holds is worth asking for again; a key
/// Windows cannot map never becomes registrable, so nothing is gained by asking twice.
enum Failure {
    Held(windows::core::Error),
    Key,
}
impl Failure {
    fn message(&self) -> String {
        match self {
            Self::Held(_) => HELD.to_owned(),
            Self::Key => UNUSABLE_KEY.to_owned(),
        }
    }
    fn chased(&self) -> bool {
        matches!(self, Self::Held(_))
    }
}
/// What an attempt settled, and so what happens to the chase afterwards.
enum Settled {
    /// The chord is in hand: any chase for it is over.
    Registered,
    /// Another app holds the chord: ask again on a widening backoff.
    Held,
    /// Nothing was installed and nothing was lost, so a chase already running keeps running.
    Unchanged,
}
/// A chord another app holds: one `RegisterHotKey` call per wake, never a thread and never a poll,
/// with each wake twice as far out as the last up to `RETRY_LIMIT`.
struct Retrying {
    request: Arc<Registration>,
    attempts: u32,
    started: Instant,
    delay: Duration,
}
impl Retrying {
    fn new(request: &Arc<Registration>) -> Self {
        Self {
            request: Arc::clone(request),
            attempts: 1,
            started: Instant::now(),
            delay: RETRY_FIRST,
        }
    }
    /// Counts the call this wake is about to make.
    fn attempt(&mut self) -> u32 {
        self.attempts += 1;
        self.attempts
    }
    /// Moves the next wake twice as far out, to a minute.
    fn back_off(&mut self) -> Duration {
        self.delay = self.delay.saturating_mul(2).min(RETRY_LIMIT);
        self.delay
    }
}

impl Registration {
    fn finish(&self, result: Result<(), String>) -> bool {
        let mut response = lock(&self.response);
        if response.canceled {
            return false;
        }
        response.result = Some(result);
        self.ready.notify_one();
        true
    }
    fn wait(&self) -> Result<(), String> {
        let deadline = Instant::now() + RESPONSE_TIME;
        let mut response = lock(&self.response);
        loop {
            if let Some(result) = response.result.take() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                response.canceled = true;
                return Err("The dimming shortcut did not respond. Try enabling it again.".into());
            }
            response = self
                .ready
                .wait_timeout(response, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
fn worker() -> Result<&'static Arc<Worker>, String> {
    WORKER
        .get_or_init(|| {
            let worker = Arc::new(Worker {
                hwnd: AtomicUsize::new(0),
                thread: AtomicU32::new(0),
                failed: AtomicBool::new(false),
                pending: Mutex::new(Pending::default()),
            });
            let endpoint = Arc::clone(&worker);
            std::thread::Builder::new()
                .name("monhop-dimming".into())
                .spawn(move || {
                    let _lifetime = WorkerLifetime(Arc::clone(&endpoint));
                    let outcome = Native::new();
                    if let Ok(mut native) = outcome {
                        // SAFETY: a thread ID has no ownership or pointer preconditions.
                        endpoint
                            .thread
                            .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
                        endpoint
                            .hwnd
                            .store(native.control.0.0 as usize, Ordering::Release);
                        // Public calls may queue before the hidden window exists.
                        let initial = std::mem::take(&mut *lock(&endpoint.pending));
                        native.apply(initial);
                        native.run(&endpoint.pending);
                    } else if let Err(error) = outcome {
                        log::warn!("dimming: {error}");
                    }
                })
                .map_err(|_| UNAVAILABLE.to_owned())?;
            Ok(worker)
        })
        .as_ref()
        .map_err(Clone::clone)
}
impl Worker {
    fn update(&self, update: impl FnOnce(&mut Pending)) -> Result<(), String> {
        let mut pending = lock(&self.pending);
        if self.failed.load(Ordering::Acquire) {
            return Err(UNAVAILABLE.into());
        }
        update(&mut pending);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        // One wake covers any number of slider events. Callbacks run after this lock is released.
        if hwnd != 0 && !pending.wake_posted {
            // SAFETY: this is our worker's HWND; the message carries no borrowed pointers.
            if unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), APPLY, WPARAM(0), LPARAM(0)) }
                .is_err()
            {
                // Cancel before releasing pending: a concurrent startup drain must never apply a failed request.
                if let Some(Some(request)) = pending.hotkey.take() {
                    lock(&request.response).canceled = true;
                }
                pending.shown = None;
                pending.level = None;
                return Err(UNAVAILABLE.into());
            }
            pending.wake_posted = true;
        }
        Ok(())
    }
}

pub fn show(level: DimLevel) -> Result<(), String> {
    worker()?.update(|pending| {
        pending.shown = Some(true);
        pending.level = Some(level);
    })
}
pub fn hide() {
    if let Some(Ok(worker)) = WORKER.get()
        && let Err(error) = worker.update(|pending| pending.shown = Some(false))
    {
        log::warn!("dimming: {error}");
    }
}
pub fn set_level(level: DimLevel) {
    if let Err(error) =
        worker().and_then(|worker| worker.update(|pending| pending.level = Some(level)))
    {
        log::warn!("dimming: {error}");
    }
}
pub fn register_hotkey(
    chord: Chord,
    on_press: Box<dyn Fn() + Send + Sync + 'static>,
    retry: Retry,
) -> Result<(), String> {
    let worker = worker()?;
    // SAFETY: querying the calling thread's ID does not access external memory.
    if unsafe { GetCurrentThreadId() } == worker.thread.load(Ordering::Acquire) {
        return Err("Change the dimming shortcut from MonHop settings.".into());
    }
    let request = Arc::new(Registration {
        chord,
        press: Arc::from(on_press),
        retry,
        response: Mutex::new(Response::default()),
        ready: Condvar::new(),
    });
    if let Err(error) = worker.update(|pending| {
        if let Some(Some(prior)) = pending.hotkey.replace(Some(Arc::clone(&request))) {
            prior.finish(Err("The dimming shortcut request was replaced.".into()));
        }
    }) {
        lock(&request.response).canceled = true;
        return Err(error);
    }
    request.wait()
}
pub fn unregister_hotkey() {
    if let Some(Ok(worker)) = WORKER.get()
        && let Err(error) = worker.update(|pending| {
            if let Some(Some(prior)) = pending.hotkey.replace(None) {
                prior.finish(Err("The dimming shortcut was turned off.".into()));
            }
        })
    {
        log::warn!("dimming: {error}");
    }
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: Window is uniquely owned and never leaves its creating thread.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}
#[derive(Clone, Copy)]
struct Fade {
    from: f64,
    to: f64,
    started: Instant,
}
impl Fade {
    fn alpha(self, now: Instant) -> f64 {
        let t = (now.saturating_duration_since(self.started).as_secs_f64()
            / FADE_TIME.as_secs_f64())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * (t * t * (3.0 - 2.0 * t))
    }
    fn finished(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= FADE_TIME
    }
}
struct Native {
    control: Window,
    instance: HINSTANCE,
    overlays: Vec<Window>,
    shown: bool,
    level: DimLevel,
    alpha: f64,
    fade: Option<Fade>,
    hotkey: Option<Binding>,
    /// Set while another app holds the chord and the worker is still asking for it.
    retrying: Option<Retrying>,
}
struct Binding {
    id: i32,
    chord: Chord,
    press: Press,
}
impl Native {
    fn new() -> Result<Self, String> {
        // SAFETY: initialized output buffers and static class names/callbacks outlive registration;
        // all HWNDs stay on this thread and the stock brush remains owned by Windows.
        unsafe {
            let mut version = OSVERSIONINFOW {
                dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
                ..Default::default()
            };
            if RtlGetVersion(&mut version).is_err() || version.dwBuildNumber < 19041 {
                return Err(
                    "Screenshot-safe dimming needs Windows 10 version 2004 or newer.".into(),
                );
            }
            if SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                .0
                .is_null()
            {
                return Err(UNAVAILABLE.into());
            }
            let instance: HINSTANCE = GetModuleHandleW(None)
                .map_err(|_| UNAVAILABLE.to_owned())?
                .into();
            for (name, proc) in [
                (
                    CONTROL_CLASS,
                    Some(
                        control_proc
                            as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                    ),
                ),
                (OVERLAY_CLASS, Some(overlay_proc)),
            ] {
                let class = WNDCLASSW {
                    lpfnWndProc: proc,
                    hInstance: instance,
                    lpszClassName: name,
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    ..Default::default()
                };
                if RegisterClassW(&class) == 0 {
                    return Err(UNAVAILABLE.into());
                }
            }
            let control = Window(
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    CONTROL_CLASS,
                    w!("MonHop dimming control"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance),
                    None,
                )
                .map_err(|_| UNAVAILABLE.to_owned())?,
            );
            Ok(Self {
                control,
                instance,
                overlays: Vec::new(),
                shown: false,
                level: DimLevel::DEFAULT,
                alpha: 0.0,
                fade: None,
                hotkey: None,
                retrying: None,
            })
        }
    }
    fn run(&mut self, pending: &Mutex<Pending>) {
        let mut message = MSG::default();
        loop {
            // SAFETY: GetMessage writes the initialized MSG on its owning thread.
            let got = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
            if got <= 0 {
                break;
            }
            if message.hwnd == self.control.0 {
                match message.message {
                    APPLY => {
                        let commands = std::mem::take(&mut *lock(pending));
                        self.apply(commands);
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == FADE_TIMER => {
                        self.tick(Instant::now());
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == RETRY_TIMER => {
                        self.retry();
                        continue;
                    }
                    DISPLAYS_CHANGED => {
                        self.tick(Instant::now());
                        if self.shown || self.fade.is_some() {
                            self.rebuild();
                        }
                        continue;
                    }
                    WM_HOTKEY => {
                        if let Some(binding) = &self.hotkey
                            && message.wParam.0 == binding.id as usize
                        {
                            let press = Arc::clone(&binding.press);
                            // The closure may queue overlay changes; no pending/state lock is held.
                            press();
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            // SAFETY: the message was retrieved from this thread's queue.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    fn apply(&mut self, pending: Pending) {
        if let Some(request) = pending.hotkey {
            if let Some(request) = request {
                if !lock(&request.response).canceled {
                    self.register(&request);
                }
            } else {
                self.unregister();
            }
        }
        let now = Instant::now();
        self.tick(now);
        let level_changed = pending.level.is_some_and(|level| level != self.level);
        if let Some(level) = pending.level {
            self.level = level;
        }
        match pending.shown {
            Some(true) if !self.shown => {
                self.shown = true;
                if self.overlays.is_empty() {
                    self.rebuild();
                }
                self.animate(self.level.alpha(), now);
            }
            Some(false) if self.shown => {
                self.shown = false;
                self.animate(0.0, now);
            }
            _ if self.shown
                && (level_changed || (pending.shown.is_none() && pending.level.is_some())) =>
            {
                self.stop_timer();
                self.alpha = self.level.alpha();
                self.paint();
            }
            _ => {}
        }
    }
    fn prepare_registration(&self, request: &Registration) -> Result<Binding, Failure> {
        if let Some(binding) = &self.hotkey
            && binding.chord == request.chord
        {
            return Ok(Binding {
                id: binding.id,
                chord: request.chord,
                press: Arc::clone(&request.press),
            });
        }
        let scan = set1_from_hid_usage(request.chord.key).map_err(|_| Failure::Key)?;
        let scan = u32::from(scan.make_code) | if scan.is_extended() { 0xe000 } else { 0 };
        // SAFETY: the key mapper consumes only an integer scan code.
        let key = unsafe { MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX) };
        if key == 0 {
            return Err(Failure::Key);
        }
        let mut modifiers = MOD_NOREPEAT;
        if request.chord.control {
            modifiers |= MOD_CONTROL;
        }
        if request.chord.alt {
            modifiers |= MOD_ALT;
        }
        if request.chord.shift {
            modifiers |= MOD_SHIFT;
        }
        if request.chord.command {
            modifiers |= MOD_WIN;
        }
        let id = if self
            .hotkey
            .as_ref()
            .is_some_and(|binding| binding.id == HOTKEY_ID)
        {
            HOTKEY_ID + 1
        } else {
            HOTKEY_ID
        };
        // SAFETY: the control window belongs to this thread, and the callback stays in Rust state.
        unsafe { RegisterHotKey(Some(self.control.0), id, modifiers, key) }
            .map_err(Failure::Held)?;
        Ok(Binding {
            id,
            chord: request.chord,
            press: Arc::clone(&request.press),
        })
    }
    fn register(&mut self, request: &Arc<Registration>) {
        let candidate = self.prepare_registration(request);
        let mut response = lock(&request.response);
        let mut settled = Settled::Unchanged;
        let release = match candidate {
            Ok(binding) if response.canceled => (!self
                .hotkey
                .as_ref()
                .is_some_and(|prior| prior.id == binding.id))
            .then_some(binding),
            Ok(binding) => {
                let id = binding.id;
                let prior = self.hotkey.replace(binding).filter(|prior| prior.id != id);
                response.result = Some(Ok(()));
                settled = Settled::Registered;
                prior
            }
            Err(failure) => {
                // The only line a taken chord writes to the log: the retries themselves are silent.
                if let Failure::Held(error) = &failure {
                    log::warn!("dimming: RegisterHotKey failed: {error}");
                }
                if failure.chased() {
                    settled = Settled::Held;
                }
                response.result = Some(Err(failure.message()));
                None
            }
        };
        request.ready.notify_one();
        drop(response);
        if let Some(binding) = release {
            self.release(binding);
        }
        match settled {
            Settled::Registered => self.stop_retrying(),
            // A fresh request restarts the backoff at its first, shortest wait.
            Settled::Held => self.chase(request),
            Settled::Unchanged => {}
        }
    }
    /// Asks again for a chord another app holds, one call per wake. Nothing happens once the user
    /// turns the shortcut off or the app goes away: the gate reads that under the toggle's lock.
    fn retry(&mut self) {
        let Some(mut retrying) = self.retrying.take() else {
            self.kill_retry_timer();
            return;
        };
        if !(retrying.request.retry.wanted)() {
            self.kill_retry_timer();
            return;
        }
        let attempts = retrying.attempt();
        match self.prepare_registration(&retrying.request) {
            Ok(binding) => {
                self.kill_retry_timer();
                let id = binding.id;
                if let Some(prior) = self.hotkey.replace(binding).filter(|prior| prior.id != id) {
                    self.release(prior);
                }
                (retrying.request.retry.recovered)(attempts, retrying.started.elapsed());
            }
            Err(failure) if failure.chased() => {
                let delay = retrying.back_off();
                self.retrying = Some(retrying);
                self.arm_retry(delay);
            }
            Err(_) => self.kill_retry_timer(),
        }
    }
    fn chase(&mut self, request: &Arc<Registration>) {
        self.retrying = Some(Retrying::new(request));
        self.arm_retry(RETRY_FIRST);
    }
    fn arm_retry(&mut self, delay: Duration) {
        // SAFETY: the timer posts messages to our control HWND and has no callback pointer.
        if unsafe {
            SetTimer(
                Some(self.control.0),
                RETRY_TIMER,
                delay.as_millis() as u32,
                None,
            )
        } == 0
        {
            log::warn!("dimming: the shortcut retry timer could not start");
            self.stop_retrying();
        }
    }
    fn stop_retrying(&mut self) {
        if self.retrying.take().is_some() {
            self.kill_retry_timer();
        }
    }
    fn kill_retry_timer(&self) {
        // SAFETY: this thread owns the control HWND and its timer ID.
        unsafe {
            let _ = KillTimer(Some(self.control.0), RETRY_TIMER);
        }
    }
    fn release(&self, binding: Binding) {
        // SAFETY: this ID was registered to the worker's own control window.
        unsafe {
            let _ = UnregisterHotKey(Some(self.control.0), binding.id);
        }
    }
    fn unregister(&mut self) {
        self.stop_retrying();
        if let Some(binding) = self.hotkey.take() {
            self.release(binding);
        }
    }
    fn rebuild(&mut self) {
        // SAFETY: this query has no pointer parameters or state changes.
        if !unsafe { DwmIsCompositionEnabled() }.is_ok_and(|enabled| enabled.as_bool()) {
            self.overlays.clear();
            log::warn!("dimming: desktop composition is unavailable; leaving screens undimmed");
            return;
        }
        let mut rectangles = Vec::<RECT>::new();
        // SAFETY: enumeration is synchronous; the callback borrows this vector only until return.
        let ok = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(monitor_rect),
                LPARAM((&mut rectangles as *mut Vec<RECT>) as isize),
            )
        };
        if !ok.as_bool() {
            self.overlays.clear();
            log::warn!("dimming: display enumeration failed; leaving screens undimmed");
            return;
        }
        let mut replacements = Vec::new();
        for bounds in rectangles {
            match self.cover(bounds) {
                Ok(window) => replacements.push(window),
                Err(error) => log::warn!("dimming: leaving display undimmed: {error}"),
            }
        }
        // Prepare replacements while hidden so display changes do not expose intermediate covers.
        self.overlays = replacements;
        for window in &self.overlays {
            // SAFETY: these live HWNDs belong to this thread; exclusion was verified while hidden.
            unsafe {
                let _ = ShowWindow(window.0, SW_SHOWNOACTIVATE);
            }
        }
    }
    fn cover(&self, bounds: RECT) -> windows::core::Result<Window> {
        // SAFETY: monitor bounds and the registered class are valid; RAII destroys failed covers
        // before they can become visible. Affinity output points to initialized local storage.
        unsafe {
            let window = Window(CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                OVERLAY_CLASS,
                w!("MonHop dimming layer"),
                WS_POPUP,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                None,
                None,
                Some(self.instance),
                None,
            )?);
            SetWindowDisplayAffinity(window.0, WDA_EXCLUDEFROMCAPTURE)?;
            let mut affinity = 0;
            GetWindowDisplayAffinity(window.0, &mut affinity)?;
            if affinity != WDA_EXCLUDEFROMCAPTURE.0 {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80004005_u32 as i32,
                )));
            }
            SetLayeredWindowAttributes(window.0, COLORREF(0), alpha_byte(self.alpha), LWA_ALPHA)?;
            Ok(window)
        }
    }
    fn animate(&mut self, target: f64, now: Instant) {
        self.fade = Some(Fade {
            from: self.alpha,
            to: target,
            started: now,
        });
        // SAFETY: the timer posts messages to our control HWND and has no callback pointer.
        if unsafe { SetTimer(Some(self.control.0), FADE_TIMER, 10, None) } == 0 {
            log::warn!("dimming: animation timer failed; restoring screens");
            self.stop_timer();
            self.overlays.clear();
            self.alpha = 0.0;
            self.shown = false;
        }
    }
    fn tick(&mut self, now: Instant) {
        if let Some(fade) = self.fade {
            self.alpha = fade.alpha(now);
            self.paint();
            if fade.finished(now) {
                self.stop_timer();
                if !self.shown {
                    self.overlays.clear();
                }
            }
        }
    }
    fn paint(&mut self) {
        self.overlays.retain(|window| {
            // SAFETY: every cover belongs to this thread and remains alive for this call.
            if let Err(error) = unsafe {
                SetLayeredWindowAttributes(window.0, COLORREF(0), alpha_byte(self.alpha), LWA_ALPHA)
            } {
                log::warn!("dimming: opacity update failed; removing overlay: {error}");
                false
            } else {
                true
            }
        });
    }
    fn stop_timer(&mut self) {
        // SAFETY: this thread owns the control HWND and its timer ID.
        unsafe {
            let _ = KillTimer(Some(self.control.0), FADE_TIMER);
        }
        self.fade = None;
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.stop_timer();
        self.unregister();
        self.overlays.clear();
    }
}
fn alpha_byte(alpha: f64) -> u8 {
    (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
}
unsafe extern "system" fn monitor_rect(_: HMONITOR, _: HDC, rect: *mut RECT, data: LPARAM) -> BOOL {
    if !rect.is_null() {
        // SAFETY: EnumDisplayMonitors supplies RECT for this callback; data is rebuild's live vector.
        unsafe {
            (&mut *(data.0 as *mut Vec<RECT>)).push(*rect);
        }
    }
    BOOL(1)
}
unsafe extern "system" fn control_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_DISPLAYCHANGE || msg == WM_DWMCOMPOSITIONCHANGED {
        // SAFETY: defer work with a pointer-free message to the HWND supplied by Windows.
        unsafe {
            let _ = PostMessageW(Some(hwnd), DISPLAYS_CHANGED, WPARAM(0), LPARAM(0));
        }
    }
    // SAFETY: forwarding the unmodified parameters provided by the window manager.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}
unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        // SAFETY: forwarding the unmodified parameters provided by the window manager.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimming::{DimmingFile, State, view_of};
    use std::sync::mpsc;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, GetPixel, ReleaseDC, SRCCOPY, SelectObject, UpdateWindow, WHITE_BRUSH,
    };

    fn capture_test_pixel() -> u32 {
        // SAFETY: the compatible bitmap is selected only into its private DC; restore before deletion.
        unsafe {
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 1, 1);
            let old = SelectObject(memory, bitmap.into());
            let copied = BitBlt(
                memory,
                0,
                0,
                1,
                1,
                Some(screen),
                48,
                48,
                SRCCOPY | CAPTUREBLT,
            );
            let color = GetPixel(memory, 0, 0).0;
            SelectObject(memory, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            copied.unwrap();
            color
        }
    }

    fn test_pattern() -> Window {
        // SAFETY: the class has static names/callbacks and a system-owned stock brush;
        // the returned HWND remains owned by this test thread and never activates.
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: instance,
                lpszClassName: w!("MonHop.Dimming.TestPattern"),
                hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let window = Window(
                CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    class.lpszClassName,
                    w!("MonHop capture test"),
                    WS_POPUP,
                    16,
                    16,
                    64,
                    64,
                    None,
                    None,
                    Some(instance),
                    None,
                )
                .unwrap(),
            );
            let _ = ShowWindow(window.0, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(window.0);
            window
        }
    }

    fn covers() -> Vec<HWND> {
        unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
            let mut name = [0u16; 64];
            // SAFETY: Windows provides the HWND; the output buffer and enumeration vector are live.
            unsafe {
                let length = GetClassNameW(hwnd, &mut name) as usize;
                if String::from_utf16_lossy(&name[..length]) == "MonHop.Dimming.Overlay" {
                    (&mut *(data.0 as *mut Vec<HWND>)).push(hwnd);
                }
            }
            BOOL(1)
        }
        let mut windows = Vec::new();
        // SAFETY: synchronous enumeration borrows this local vector only during the call.
        unsafe {
            EnumThreadWindows(
                worker().unwrap().thread.load(Ordering::Acquire),
                Some(collect),
                LPARAM((&mut windows as *mut Vec<HWND>) as isize),
            )
            .unwrap();
        }
        windows
    }

    fn opacity(hwnd: HWND) -> u8 {
        let mut alpha = 0;
        // SAFETY: the overlay HWND is owned by the running worker; the output points to a live byte.
        unsafe {
            GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), None).unwrap();
        }
        alpha
    }

    /// A chord the test expects to win or to fail for good: any chase stops at its first gate.
    fn unchased() -> Retry {
        Retry {
            wanted: Box::new(|| false),
            recovered: Box::new(|_, _| unreachable!("a chase that never runs cannot recover")),
        }
    }

    /// The error Windows reports when another application already owns the chord.
    fn already_registered() -> Failure {
        Failure::Held(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x8007_0581_u32 as i32,
        )))
    }

    fn eventually(mut condition: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(
                Instant::now() < until,
                "native dimmer did not reach the expected state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "briefly dims real monitors; run explicitly on an interactive Windows desktop"]
    fn native_fades_exclude_capture_and_release_windows_and_shortcuts() {
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                hide();
                unregister_hotkey();
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        let _cleanup = Cleanup;
        // SAFETY: a foreground HWND query has no pointer preconditions.
        let foreground = unsafe { GetForegroundWindow() };
        let _pattern = test_pattern();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            capture_test_pixel(),
            0x00ff_ffff,
            "the white capture test pattern must be visible"
        );
        let (pressed, received) = mpsc::channel();
        let chord = Chord {
            shift: true,
            key: monhop_core::HidUsage(0x43),
            ..monhop_core::dimming::DIM_TOGGLE
        };
        register_hotkey(
            chord,
            Box::new(move || {
                show(DimLevel::DEFAULT).unwrap();
                set_level(DimLevel::new(30).unwrap());
                pressed.send(()).unwrap();
            }),
            unchased(),
        )
        .unwrap();
        let control = HWND(worker().unwrap().hwnd.load(Ordering::Acquire) as *mut _);
        // SAFETY: this simulates only a notification to our own test window, without injecting input.
        unsafe {
            PostMessageW(
                Some(control),
                WM_HOTKEY,
                WPARAM(HOTKEY_ID as usize),
                LPARAM(0),
            )
            .unwrap();
        }
        received.recv_timeout(RESPONSE_TIME).unwrap();
        let invalid_chord = Chord {
            key: monhop_core::HidUsage(0xffff),
            ..chord
        };
        assert!(
            register_hotkey(
                invalid_chord,
                Box::new(|| panic!("an invalid shortcut must not replace the callback")),
                unchased(),
            )
            .is_err()
        );
        let canceled = Arc::new(Registration {
            chord: invalid_chord,
            press: Arc::new(|| {}),
            retry: unchased(),
            response: Mutex::new(Response {
                canceled: true,
                result: None,
            }),
            ready: Condvar::new(),
        });
        worker()
            .unwrap()
            .update(|pending| pending.hotkey = Some(Some(canceled)))
            .unwrap();
        // SAFETY: a canceled/invalid replacement must leave the original test-only shortcut working.
        unsafe {
            PostMessageW(
                Some(control),
                WM_HOTKEY,
                WPARAM(HOTKEY_ID as usize),
                LPARAM(0),
            )
            .unwrap();
        }
        received.recv_timeout(RESPONSE_TIME).unwrap();
        eventually(|| !covers().is_empty());
        let mut samples = Vec::new();
        for _ in 0..24 {
            samples.push(opacity(covers()[0]));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            samples.iter().any(|alpha| *alpha > 0 && *alpha < 77),
            "missing intermediate frames: {samples:?}"
        );
        assert_eq!(*samples.last().unwrap(), 77);
        assert_eq!(
            capture_test_pixel(),
            0x00ff_ffff,
            "the dimming layer must be absent from a screen capture"
        );
        let mut monitors = Vec::<RECT>::new();
        // SAFETY: enumeration is synchronous and writes only into this vector through monitor_rect.
        unsafe {
            assert!(
                EnumDisplayMonitors(
                    None,
                    None,
                    Some(monitor_rect),
                    LPARAM((&mut monitors as *mut Vec<RECT>) as isize)
                )
                .as_bool()
            );
        }
        let windows = covers();
        assert_eq!(windows.len(), monitors.len());
        for window in &windows {
            let mut affinity = 0;
            // SAFETY: these are our live overlays; the flags query and affinity output are read-only.
            unsafe {
                GetWindowDisplayAffinity(*window, &mut affinity).unwrap();
                assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
                let flags = GetWindowLongPtrW(*window, GWL_EXSTYLE) as u32;
                let required = (WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST)
                    .0;
                assert_eq!(flags & required, required);
                assert_eq!(
                    SendMessageW(*window, WM_NCHITTEST, None, None).0,
                    HTTRANSPARENT as isize
                );
            }
        }
        // SAFETY: the overlay should not change foreground activation.
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        hide();
        std::thread::sleep(Duration::from_millis(65));
        let halfway = opacity(covers()[0]);
        assert!(halfway > 0 && halfway < 77);
        show(DimLevel::new(30).unwrap()).unwrap();
        eventually(|| opacity(covers()[0]) == 77);
        assert_eq!(
            covers(),
            windows,
            "a reversal must reuse the existing covers"
        );
        set_level(DimLevel::new(70).unwrap());
        eventually(|| opacity(covers()[0]) == 179);
        // SAFETY: ask only this worker to rebuild its monitor covers.
        unsafe {
            PostMessageW(Some(control), WM_DISPLAYCHANGE, WPARAM(0), LPARAM(0)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(covers().len(), monitors.len());
        assert!(covers().iter().all(|window| opacity(*window) == 179));
        hide();
        eventually(|| covers().is_empty());
        unregister_hotkey();
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: acquiring the same test-only chord proves the worker released it; release immediately.
        unsafe {
            RegisterHotKey(
                None,
                71,
                MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
                0x79,
            )
            .unwrap();
            UnregisterHotKey(None, 71).unwrap();
        }
        println!(
            "native dimming: {} monitors, fade alpha samples {samples:?}, capture affinity/click-through/reversal/display rebuild/cleanup PASS",
            monitors.len()
        );
    }

    #[test]
    fn fade_uses_elapsed_time_and_finishes_after_a_delayed_frame() {
        let now = Instant::now();
        let fade = Fade {
            from: 0.0,
            to: 0.9,
            started: now,
        };
        assert_eq!(fade.alpha(now), 0.0);
        assert!((fade.alpha(now + FADE_TIME / 2) - 0.45).abs() < 1e-12);
        assert_eq!(fade.alpha(now + Duration::from_secs(3)), 0.9);
        assert!(!fade.finished(now + Duration::from_millis(179)));
        assert!(fade.finished(now + FADE_TIME));
    }
    #[test]
    fn a_reversal_starts_at_the_current_opacity_and_reaches_zero() {
        let start = Instant::now();
        let showing = Fade {
            from: 0.0,
            to: 0.8,
            started: start,
        };
        let reversal = start + Duration::from_millis(65);
        let hiding = Fade {
            from: showing.alpha(reversal),
            to: 0.0,
            started: reversal,
        };
        assert_eq!(hiding.alpha(reversal), showing.alpha(reversal));
        assert!(hiding.alpha(reversal + FADE_TIME / 2) < showing.alpha(reversal));
        assert_eq!(hiding.alpha(reversal + FADE_TIME), 0.0);
        assert_eq!(alpha_byte(0.5), 128);
    }
    #[test]
    fn cancellation_rejects_a_late_registration_result() {
        let request = Registration {
            chord: monhop_core::dimming::DIM_TOGGLE,
            press: Arc::new(|| {}),
            retry: unchased(),
            response: Mutex::new(Response {
                canceled: true,
                result: None,
            }),
            ready: Condvar::new(),
        };
        assert!(!request.finish(Ok(())));
        assert!(lock(&request.response).result.is_none());
    }

    #[test]
    fn a_held_chord_is_chased_on_a_widening_backoff_and_an_unusable_key_is_not() {
        assert!(already_registered().chased());
        assert!(!Failure::Key.chased());
        assert_eq!(Failure::Key.message(), UNUSABLE_KEY);
        let request = Arc::new(Registration {
            chord: monhop_core::dimming::DIM_TOGGLE,
            press: Arc::new(|| {}),
            retry: unchased(),
            response: Mutex::new(Response::default()),
            ready: Condvar::new(),
        });
        let mut retrying = Retrying::new(&request);
        assert_eq!((retrying.attempts, retrying.delay), (1, RETRY_FIRST));
        let mut schedule = Vec::new();
        for _ in 0..6 {
            retrying.attempt();
            schedule.push(retrying.back_off());
        }
        assert_eq!(schedule, [10, 20, 40, 60, 60, 60].map(Duration::from_secs));
        assert_eq!(retrying.attempts, 7);
    }

    #[test]
    fn the_card_says_monhop_keeps_trying_until_the_chord_comes_free() {
        let mut state = State {
            file: DimmingFile::default(),
            dimmed: false,
            error: None,
            shortcut_failed: false,
        };
        state.fail(Some(already_registered().message()), true);
        assert_eq!(
            view_of(&state).error.as_deref(),
            Some(
                "The dimming shortcut is held by another app. MonHop keeps trying; Dim now still works."
            )
        );
        state.registered();
        assert!(view_of(&state).error.is_none());
    }
}
