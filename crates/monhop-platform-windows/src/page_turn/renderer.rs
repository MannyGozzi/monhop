//! One lazily started thread draws every live bloom, each in its own click-through layered window,
//! a frame per compositor pass, and blocks on its queue with no window alive while none runs.

use super::raster::Canvas;
use monhop_core::pointer_mark::{
    BLOOM_STILL, BloomFrame, PageDirection, PageTurn, bloom_frame, still_frame,
};
use std::{
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::{
            Dwm::DwmFlush,
            Gdi::{
                AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
                CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
                HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MonitorFromPoint, SelectObject,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
                SetThreadDpiAwarenessContext,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HTTRANSPARENT,
                MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
                SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE, SetWindowDisplayAffinity,
                ShowWindow, SystemParametersInfoW, ULW_ALPHA, USER_DEFAULT_SCREEN_DPI,
                UpdateLayeredWindow, WDA_EXCLUDEFROMCAPTURE, WM_MOUSEACTIVATE, WM_NCHITTEST,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
    },
    core::{BOOL, PCWSTR, w},
};

const CLASS_NAME: PCWSTR = w!("MonHop.PageTurn");
/// Turns beyond this many waiting on a stalled renderer are dropped.
const QUEUE: usize = 16;
/// The frame wait when the compositor cannot pace frames or nothing on screen moves.
const RESTING_FRAME: Duration = Duration::from_millis(8);

/// Hands the turn to the renderer thread and returns without waiting on it.
pub fn show(turn: PageTurn) {
    static RENDERER: OnceLock<Option<SyncSender<(PageTurn, Instant)>>> = OnceLock::new();
    let renderer = RENDERER.get_or_init(|| {
        let (sender, turns) = mpsc::sync_channel(QUEUE);
        let spawned = thread::Builder::new()
            .name("monhop-page-turn".into())
            .spawn(move || run(turns));
        if spawned.is_err() {
            warn_once("the renderer thread could not start");
        }
        spawned.ok().map(|_| sender)
    });
    if let Some(sender) = renderer {
        let _ = sender.try_send((turn, Instant::now()));
    }
}

/// Logs the first failure only, so a broken renderer never logs per turn.
fn warn_once(what: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log::warn!("page turn: {what}; skipping the chevron");
    }
}

fn run(turns: Receiver<(PageTurn, Instant)>) {
    let Some(instance) = prepare() else {
        return;
    };
    let mut blooms = Vec::new();
    loop {
        if blooms.is_empty() {
            let Ok(turn) = turns.recv() else {
                return;
            };
            blooms.extend(Bloom::start(instance, turn));
        }
        blooms.extend(
            turns
                .try_iter()
                .filter_map(|turn| Bloom::start(instance, turn)),
        );
        pump();
        let now = Instant::now();
        blooms.retain_mut(|bloom| bloom.advance(now));
        if !blooms.is_empty() {
            pace(blooms.iter().any(|bloom| !bloom.still));
        }
    }
}

/// Puts this thread in physical pixels and registers the chevron's window class.
fn prepare() -> Option<HINSTANCE> {
    // SAFETY: changes only the calling thread's DPI awareness.
    if unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_null()
    {
        warn_once("per-monitor DPI awareness is unavailable");
        return None;
    }
    // SAFETY: null names the running executable and loads nothing.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: the class holds only a static name and callback; the one renderer thread registers it once.
    if instance.is_null() || unsafe { RegisterClassW(&class) } == 0 {
        warn_once("the window class could not be registered");
        return None;
    }
    Some(instance)
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: this thread owns its queue and the writable message storage.
    while unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: dispatches a message just taken from this thread's queue.
        unsafe { DispatchMessageW(&message) };
    }
}

/// Moving blooms wait for the compositor's next pass, so each frame lands on a refresh at any rate.
fn pace(moving: bool) {
    // SAFETY: takes no arguments and only blocks the calling thread.
    if !moving || unsafe { DwmFlush() } < 0 {
        thread::sleep(RESTING_FRAME);
    }
}

fn animations_on() -> bool {
    let mut on: BOOL = 1;
    // SAFETY: this action writes one BOOL into the local.
    let read =
        unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
    read == 0 || on != 0
}

fn pixels_per_dip(at: POINT) -> f64 {
    let (mut x, mut y) = (0, 0);
    // SAFETY: the nearest monitor always exists and its DPI lands in two locals.
    let read = unsafe {
        GetDpiForMonitor(
            MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST),
            MDT_EFFECTIVE_DPI,
            &mut x,
            &mut y,
        )
    };
    if read < 0 || x == 0 {
        return 1.0;
    }
    f64::from(x) / f64::from(USER_DEFAULT_SCREEN_DPI)
}

struct Bloom {
    window: Window,
    surface: Surface,
    canvas: Canvas,
    direction: PageDirection,
    started: Instant,
    /// Reduced motion: the sharpest frame alone, unmoving.
    still: bool,
    shown: bool,
    origin: POINT,
    size: SIZE,
}

impl Bloom {
    fn start(instance: HINSTANCE, (turn, started): (PageTurn, Instant)) -> Option<Self> {
        let pointer = POINT {
            x: turn.at.x.round() as i32,
            y: turn.at.y.round() as i32,
        };
        let canvas = Canvas::new(pixels_per_dip(pointer));
        let (column, row) = canvas.pointer();
        let origin = POINT {
            x: pointer.x - column as i32,
            y: pointer.y - row as i32,
        };
        let size = SIZE {
            cx: canvas.width as i32,
            cy: canvas.height as i32,
        };
        Some(Self {
            surface: Surface::new(&canvas)?,
            window: Window::new(instance, origin, size)?,
            canvas,
            direction: turn.direction,
            started,
            still: !animations_on(),
            shown: false,
            origin,
            size,
        })
    }

    /// Draws the moment `now`; false once the run is over or the window cannot be drawn.
    fn advance(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if self.still {
            return elapsed < BLOOM_STILL && (self.shown || self.draw(still_frame(self.direction)));
        }
        bloom_frame(self.direction, elapsed).is_some_and(|frame| self.draw(frame))
    }

    fn draw(&mut self, frame: BloomFrame) -> bool {
        self.canvas
            .paint(self.surface.pixels(), self.direction, frame);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: u8::MAX,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let source = POINT { x: 0, y: 0 };
        // SAFETY: this thread owns the window and the DC holding the section; every pointer is live.
        let updated = unsafe {
            UpdateLayeredWindow(
                self.window.0,
                ptr::null_mut(),
                &self.origin,
                &self.size,
                self.surface.dc,
                &source,
                0,
                &blend,
                ULW_ALPHA,
            )
        };
        if updated == 0 {
            warn_once("a frame could not be drawn");
            return false;
        }
        if !self.shown {
            // SAFETY: shows this thread's own window without activating it.
            unsafe { ShowWindow(self.window.0, SW_SHOWNOACTIVATE) };
            self.shown = true;
        }
        true
    }
}

struct Window(HWND);

impl Window {
    fn new(instance: HINSTANCE, origin: POINT, size: SIZE) -> Option<Self> {
        // SAFETY: the registered class and static title outlive the call; the window starts hidden.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST,
                CLASS_NAME,
                w!("MonHop page turn"),
                WS_POPUP,
                origin.x,
                origin.y,
                size.cx,
                size.cy,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            warn_once("a window could not be created");
            return None;
        }
        // Screenshots skip the chevron like the dimming layer; where Windows refuses, it still shows.
        // SAFETY: sets the capture affinity of a window this thread just created.
        unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
        Some(Self(hwnd))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the window belongs to this thread and is destroyed exactly once.
        unsafe { DestroyWindow(self.0) };
    }
}

/// A top-down 32-bit DIB section selected into a memory DC for UpdateLayeredWindow to read.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Surface {
    fn new(canvas: &Canvas) -> Option<Self> {
        // SAFETY: a memory DC compatible with the screen, deleted on drop.
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if dc.is_null() {
            warn_once("a drawing surface could not be created");
            return None;
        }
        let mut surface = Self {
            dc,
            bitmap: ptr::null_mut(),
            previous: ptr::null_mut(),
            bits: ptr::null_mut(),
            len: canvas.byte_len(),
        };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: canvas.width as i32,
                // Negative runs the rows top-down, as the canvas paints them.
                biHeight: -(canvas.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = ptr::null_mut();
        // SAFETY: the header describes a 32-bit section; its memory pointer lands in the local.
        surface.bitmap =
            unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0) };
        surface.bits = bits.cast();
        if surface.bitmap.is_null() || surface.bits.is_null() {
            warn_once("a drawing surface could not be created");
            return None;
        }
        // SAFETY: both handles are live and owned here; drop selects the original back.
        surface.previous = unsafe { SelectObject(dc, surface.bitmap) };
        if surface.previous.is_null() {
            warn_once("a drawing surface could not be created");
            return None;
        }
        Some(surface)
    }

    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: the section holds `len` bytes while the bitmap lives, and no GDI call writes it.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: this thread owns the DC and section; the DC gets its own bitmap back before deletion.
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            DeleteDC(self.dc);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        // SAFETY: unhandled messages keep the default behavior with the parameters Windows passed.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
