//! Windows `NativeClipboard`: message-only window owner, sequence-number polling, privacy markers.

use std::ffi::c_void;
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WAIT_FAILED, WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG,
    MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    PM_NOREMOVE, PM_QS_SENDMESSAGE, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT,
    QS_SENDMESSAGE, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use super::content::{Pick, SourceMarker, pick, windows_line_ends};
use super::image::Rgba;
use super::native::{
    Access, Content, NativeClipboard, NativeError, Origin, ReadLimits, Skip, Snapshot,
};

const SOURCE_FORMAT: &str = "MonHop.ClipboardSource";
/// Presence alone asks monitors to leave the item alone (password managers set these two).
const EXCLUDE_FORMAT: &str = "ExcludeClipboardContentFromMonitorProcessing";
const VIEWER_IGNORE_FORMAT: &str = "Clipboard Viewer Ignore";
/// DWORD flags: zero keeps the item out of Windows clipboard history or cloud sync.
const HISTORY_FORMAT: &str = "CanIncludeInClipboardHistory";
const CLOUD_FORMAT: &str = "CanUploadToCloudClipboard";
const PNG_FORMAT: &str = "PNG";
const MIME_PNG_FORMAT: &str = "image/png";

const TEXT: u32 = CF_UNICODETEXT.0 as u32;
const DIB: u32 = CF_DIB.0 as u32;
const DIBV5: u32 = CF_DIBV5.0 as u32;
const FILES: u32 = CF_HDROP.0 as u32;

const CLASS_NAME: PCWSTR = w!("MonHop.Clipboard");
const WAKE: u32 = WM_APP + 1;
const MESSAGE_BATCH: usize = 64;
const OPEN_ATTEMPTS: usize = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

static CLASS: OnceLock<bool> = OnceLock::new();

/// The clipboard as seen from one dedicated thread, which must create, use and drop it (it is not
/// `Send`). Its message-only window owns what MonHop writes, and answers the messages other
/// programs send the owner only while that thread pumps in `wait`.
pub struct WindowsClipboard {
    window: Window,
    formats: Formats,
    wake: Arc<WakeTarget>,
}

impl WindowsClipboard {
    pub fn new() -> Result<Self, NativeError> {
        let formats = Formats::register()?;
        let window = Window::create()?;
        let wake = Arc::new(WakeTarget {
            window: Mutex::new(Some(window.0.0 as usize)),
            posted: AtomicBool::new(false),
        });
        Ok(Self {
            window,
            formats,
            wake,
        })
    }

    fn open(&self) -> Result<Opened, NativeError> {
        for attempt in 0..OPEN_ATTEMPTS {
            if attempt > 0 {
                answer_sent_messages(OPEN_RETRY_DELAY);
            }
            // SAFETY: the window belongs to this thread; the guard closes the clipboard on drop.
            if unsafe { OpenClipboard(Some(self.window.0)) }.is_ok() {
                return Ok(Opened(PhantomData));
            }
        }
        Err(NativeError::Busy)
    }

    /// Dispatches up to a batch of queued messages (sent ones are answered along the way); true
    /// when the waker's message was among them.
    fn dispatch_queued(&self) -> bool {
        let mut woken = false;
        for _ in 0..MESSAGE_BATCH {
            let mut message = MSG::default();
            // SAFETY: writable local storage; this thread owns the queue it reads.
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WAKE && message.hwnd == self.window.0 {
                woken = true;
                continue;
            }
            // SAFETY: the message was just taken from this thread's own queue.
            unsafe { DispatchMessageW(&message) };
        }
        woken
    }

    /// The trait's write sequence in one open session, so no reader sees part of an item; a
    /// failure empties the clipboard rather than leave content without MonHop's source marker.
    fn write(
        &self,
        marker: SourceMarker,
        expected: u64,
        content: impl IntoIterator<Item = (u32, Global)>,
    ) -> Result<u64, NativeError> {
        let never = 0u32.to_le_bytes();
        let markers = [
            (self.formats.source, Global::copied(&marker.to_bytes())?),
            (self.formats.cloud, Global::copied(&never)?),
            (self.formats.history, Global::copied(&never)?),
        ];
        let clipboard = self.open()?;
        if self.change_marker() != expected {
            return Err(NativeError::Changed);
        }
        clipboard.empty()?;
        for (format, memory) in markers.into_iter().chain(content) {
            if let Err(error) = memory.place(&clipboard, format) {
                let _ = clipboard.empty();
                return Err(error);
            }
        }
        let inside = self.change_marker();
        drop(clipboard);
        // Closing adds the formats Windows synthesizes (CF_TEXT, CF_DIB, ...), each moving the
        // counter again. The settled value is this write's only if MonHop's window still owns the
        // clipboard while the counter holds still; otherwise the source marker identifies the item.
        let settled = self.change_marker();
        // SAFETY: a plain query with no arguments.
        let owned = unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == self.window.0);
        Ok(if owned && self.change_marker() == settled {
            settled
        } else {
            inside
        })
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        // Wakers can outlive the window; once this is clear they post nothing.
        *lock(&self.wake.window) = None;
    }
}

impl NativeClipboard for WindowsClipboard {
    fn change_marker(&self) -> u64 {
        // SAFETY: a plain query with no arguments.
        u64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn wait(&mut self, timeout: Duration) {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if self.dispatch_queued() {
                self.wake.posted.store(false, Ordering::Release);
                return;
            }
            let remaining = deadline.map_or(Duration::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if remaining.is_zero() {
                return;
            }
            // SAFETY: no handles and a bounded wait; returns for posted, sent or queued messages.
            let result = unsafe {
                MsgWaitForMultipleObjectsEx(
                    None,
                    wait_millis(remaining),
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if result == WAIT_FAILED {
                thread::sleep(remaining);
                return;
            }
        }
    }

    fn waker(&self) -> Waker {
        Waker::from(Arc::clone(&self.wake))
    }

    fn access(&self) -> Access {
        Access::Allowed
    }

    fn read(&mut self, limits: &ReadLimits) -> Result<Snapshot, NativeError> {
        let before = self.change_marker();
        let (origin, content) = {
            let clipboard = self.open()?;
            self.formats.read(&clipboard, limits)
        };
        Ok(Snapshot {
            before,
            after: self.change_marker(),
            origin,
            content,
        })
    }

    fn write_text(
        &mut self,
        text: &str,
        marker: SourceMarker,
        expected: u64,
    ) -> Result<u64, NativeError> {
        let wide: Vec<u16> = windows_line_ends(text)
            .encode_utf16()
            .chain(iter::once(0))
            .collect();
        let text = Global::filled(wide.len() * 2, |out| {
            for (target, unit) in out.as_chunks_mut::<2>().0.iter_mut().zip(&wide) {
                *target = unit.to_le_bytes();
            }
            Ok(())
        })?;
        self.write(marker, expected, [(TEXT, text)])
    }

    fn write_png(
        &mut self,
        png: &[u8],
        rgba: Option<&Rgba>,
        marker: SourceMarker,
        expected: u64,
    ) -> Result<u64, NativeError> {
        let mut content = vec![(self.formats.png, Global::copied(png)?)];
        if let Some(rgba) = rgba {
            let dib = Global::filled(rgba.dibv5_len(), |out| {
                rgba.write_dibv5(out).map_err(|_| NativeError::Failed)
            })?;
            content.push((DIBV5, dib));
        }
        self.write(marker, expected, content)
    }
}

struct Formats {
    source: u32,
    exclude: u32,
    viewer_ignore: u32,
    history: u32,
    cloud: u32,
    png: u32,
    mime_png: u32,
}

impl Formats {
    fn register() -> Result<Self, NativeError> {
        Ok(Self {
            source: register(SOURCE_FORMAT)?,
            exclude: register(EXCLUDE_FORMAT)?,
            viewer_ignore: register(VIEWER_IGNORE_FORMAT)?,
            history: register(HISTORY_FORMAT)?,
            cloud: register(CLOUD_FORMAT)?,
            png: register(PNG_FORMAT)?,
            mime_png: register(MIME_PNG_FORMAT)?,
        })
    }

    /// Markers first: nothing past them is read for MonHop's own, concealed or file items.
    fn read(&self, clipboard: &Opened, limits: &ReadLimits) -> (Origin, Content) {
        if clipboard.available(self.source) {
            let marker = clipboard
                .data(self.source)
                .and_then(|data| SourceMarker::parse(data.bytes()));
            return (Origin::MonHop(marker), Content::Empty);
        }
        let privacy = Privacy {
            exclude: clipboard.available(self.exclude),
            viewer_ignore: clipboard.available(self.viewer_ignore),
            history: clipboard.flag(self.history),
            cloud: clipboard.flag(self.cloud),
        };
        let content = if privacy.concealed() {
            Content::Skipped(Skip::Concealed)
        } else if clipboard.available(FILES) {
            Content::Skipped(Skip::Files)
        } else {
            self.content(clipboard, limits)
        };
        (Origin::Local, content)
    }

    fn content(&self, clipboard: &Opened, limits: &ReadLimits) -> Content {
        let text = match clipboard
            .data(TEXT)
            .map(|data| utf16_text(data.bytes(), limits.text_bytes))
        {
            Some(Ok(text)) => Some(text),
            Some(Err(skip)) => return Content::Skipped(skip),
            None => None,
        };
        let image_available = [self.png, self.mime_png, DIBV5, DIB]
            .into_iter()
            .any(|format| clipboard.available(format));
        match pick(text.as_deref(), image_available) {
            Pick::Text => text.map_or(Content::Empty, Content::Text),
            Pick::Image => self.image(clipboard, limits),
            Pick::Nothing => Content::Empty,
        }
    }

    /// The program's own PNG passes through; otherwise a DIB (Windows synthesizes one from a
    /// bitmap) goes to the codec, V5 first because it keeps alpha.
    fn image(&self, clipboard: &Opened, limits: &ReadLimits) -> Content {
        for format in [self.png, self.mime_png] {
            if let Some(data) = clipboard.data(format) {
                return png_bytes(data.bytes(), limits.png_bytes)
                    .map_or_else(Content::Skipped, Content::Png);
            }
        }
        for format in [DIBV5, DIB] {
            if let Some(data) = clipboard.data(format) {
                return copy_capped(data.bytes(), limits.dib_bytes)
                    .map_or_else(Content::Skipped, Content::Dib);
            }
        }
        Content::Skipped(Skip::Unsupported)
    }
}

fn register(name: &str) -> Result<u32, NativeError> {
    let wide: Vec<u16> = name.encode_utf16().chain(iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    match unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) } {
        0 => Err(NativeError::Failed),
        format => Ok(format),
    }
}

/// A DWORD privacy format as found on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    Absent,
    Value(u32),
    Unreadable,
}

impl Flag {
    /// Only the first four bytes count: Windows may round clipboard allocations up.
    fn parse(data: Option<&[u8]>) -> Self {
        match data.and_then(<[u8]>::first_chunk::<4>) {
            Some(value) => Self::Value(u32::from_le_bytes(*value)),
            None => Self::Unreadable,
        }
    }

    /// A flag MonHop cannot read forbids as much as zero does.
    fn forbids(self) -> bool {
        !matches!(self, Self::Absent | Self::Value(1..))
    }
}

struct Privacy {
    exclude: bool,
    viewer_ignore: bool,
    history: Flag,
    cloud: Flag,
}

impl Privacy {
    fn concealed(&self) -> bool {
        self.exclude || self.viewer_ignore || self.history.forbids() || self.cloud.forbids()
    }
}

/// UTF-16LE clipboard text up to its first NUL. At most `limit` bytes are scanned, so an item past
/// the limit is refused without copying, however large its allocation.
fn utf16_text(bytes: &[u8], limit: usize) -> Result<String, Skip> {
    let units = bytes.as_chunks::<2>().0;
    let max_units = limit / 2;
    let scanned = &units[..units.len().min(max_units + 1)];
    let text = match scanned.iter().position(|unit| *unit == [0, 0]) {
        Some(end) => &scanned[..end],
        None if units.len() > max_units => return Err(Skip::TooLarge),
        None => units,
    };
    Ok(
        char::decode_utf16(text.iter().map(|unit| u16::from_le_bytes(*unit)))
            .map(|decoded| decoded.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
    )
}

/// The PNG stream without the allocation tail after IEND; nothing past `limit` is examined.
fn png_bytes(bytes: &[u8], limit: usize) -> Result<Vec<u8>, Skip> {
    let window = &bytes[..bytes.len().min(limit)];
    match png_end(window) {
        Some(end) => copy_capped(&window[..end], limit),
        None => copy_capped(bytes, limit),
    }
}

/// Offset just past the IEND chunk, walking chunk lengths only.
fn png_end(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return None;
    }
    let mut at = PNG_SIGNATURE.len();
    loop {
        let [l0, l1, l2, l3, kind @ ..] = *bytes.get(at..)?.first_chunk::<8>()?;
        let len = usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).ok()?;
        let end = at.checked_add(12)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        if kind == *b"IEND" {
            return Some(end);
        }
        at = end;
    }
}

fn copy_capped(bytes: &[u8], limit: usize) -> Result<Vec<u8>, Skip> {
    if bytes.len() > limit {
        return Err(Skip::TooLarge);
    }
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| Skip::TooLarge)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

/// Rounds up, so a sub-millisecond remainder still waits, and never means INFINITE.
fn wait_millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros().div_ceil(1000))
        .map_or(u32::MAX - 1, |ms| ms.min(u32::MAX - 1))
}

/// Waits `delay` answering only sent messages, such as WM_DESTROYCLIPBOARD from a program that
/// holds the clipboard open while emptying it; posted messages stay queued for `wait`.
fn answer_sent_messages(delay: Duration) {
    let deadline = Instant::now() + delay;
    loop {
        let mut message = MSG::default();
        // SAFETY: writable local storage; PM_QS_SENDMESSAGE with PM_NOREMOVE only answers sent
        // messages and takes nothing off the queue.
        let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE) };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        // SAFETY: no handles and a bounded wait.
        let result = unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                wait_millis(remaining),
                QS_SENDMESSAGE,
                MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS::default(),
            )
        };
        if result == WAIT_FAILED {
            thread::sleep(remaining);
            return;
        }
    }
}

struct Window(HWND);

impl Window {
    fn create() -> Result<Self, NativeError> {
        // SAFETY: no name asks for this executable's own module; nothing is loaded.
        let module = unsafe { GetModuleHandleW(None) }.map_err(|_| NativeError::Failed)?;
        let registered = *CLASS.get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // SAFETY: a process-lifetime class holding only a static name and a static callback.
            unsafe { RegisterClassW(&class) != 0 }
        });
        if !registered {
            return Err(NativeError::Failed);
        }
        // SAFETY: the class is registered; a message-only window is never shown, gets no
        // broadcasts, and is destroyed by this thread on drop.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(module.into()),
                None,
            )
        }
        .map_err(|_| NativeError::Failed)?;
        Ok(Self(hwnd))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the window belongs to this thread (nothing holding it is Send), destroyed once.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// MonHop never delays rendering, so the default handling of every owner message is right.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: forwards Windows' own arguments unchanged.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

struct WakeTarget {
    /// The window's handle while it exists; cleared before it is destroyed.
    window: Mutex<Option<usize>>,
    /// Coalesces wakes into one queued message until `wait` takes it.
    posted: AtomicBool,
}

impl Wake for WakeTarget {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let window = lock(&self.window);
        let posted = window.is_some_and(|hwnd| {
            // SAFETY: the window is alive while the lock holds its handle, and the message carries
            // no pointers.
            unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), WAKE, WPARAM(0), LPARAM(0)) }
                .is_ok()
        });
        if !posted {
            self.posted.store(false, Ordering::Release);
        }
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The clipboard, open on this thread until dropped.
struct Opened(PhantomData<*const ()>);

impl Opened {
    fn available(&self, format: u32) -> bool {
        // SAFETY: a query of the clipboard this guard holds open.
        unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    /// `None` when the format is absent or its data cannot be locked.
    fn data(&self, format: u32) -> Option<Locked<'_>> {
        // SAFETY: the clipboard is open on this thread and keeps ownership of the handle.
        let handle = unsafe { GetClipboardData(format) }.ok()?;
        // SAFETY: every format MonHop reads is global memory, which the clipboard keeps until it
        // closes; the lock borrows this guard, so it cannot outlive that.
        unsafe { Locked::new(HGLOBAL(handle.0)) }
    }

    fn flag(&self, format: u32) -> Flag {
        if !self.available(format) {
            return Flag::Absent;
        }
        Flag::parse(self.data(format).as_ref().map(Locked::bytes))
    }

    fn empty(&self) -> Result<(), NativeError> {
        // SAFETY: the clipboard is open on this thread, with MonHop's window as the new owner.
        unsafe { EmptyClipboard() }.map_err(|_| NativeError::Failed)
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: this guard opened the clipboard on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Global memory locked in place; unlocked on drop.
struct Locked<'a> {
    memory: HGLOBAL,
    start: *mut u8,
    len: usize,
    _borrow: PhantomData<&'a ()>,
}

impl Locked<'_> {
    /// # Safety
    /// `memory` must be global memory that stays allocated while the lock lives.
    unsafe fn new(memory: HGLOBAL) -> Option<Self> {
        // SAFETY: the caller vouches for the handle; failure comes back as zero.
        let len = unsafe { GlobalSize(memory) };
        if len == 0 {
            return None;
        }
        // SAFETY: as above; failure comes back as null.
        let start = unsafe { GlobalLock(memory) }.cast::<u8>();
        if start.is_null() {
            return None;
        }
        Some(Self {
            memory,
            start,
            len,
            _borrow: PhantomData,
        })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the lock pins GlobalSize bytes at `start` until it is dropped.
        unsafe { slice::from_raw_parts(self.start, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`, and `&mut self` makes this the only view.
        unsafe { slice::from_raw_parts_mut(self.start, self.len) }
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the GlobalLock in `new` on the same live memory.
        let _ = unsafe { GlobalUnlock(self.memory) };
    }
}

/// Movable global memory, freed on drop unless the clipboard took it.
struct Global(HGLOBAL);

impl Global {
    /// `fill` gets exactly `len` bytes even when Windows rounds the allocation up; the rest stays
    /// zero, so no stale heap bytes reach other programs. A failed fill frees the memory.
    fn filled(
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> Result<(), NativeError>,
    ) -> Result<Self, NativeError> {
        if len == 0 {
            return Err(NativeError::Failed);
        }
        // SAFETY: a plain allocation, owned by the returned guard from here on.
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, len) }
            .map_err(|_| NativeError::Failed)?;
        let mut memory = Self(memory);
        {
            let mut locked = memory.lock().ok_or(NativeError::Failed)?;
            let out = locked
                .bytes_mut()
                .get_mut(..len)
                .ok_or(NativeError::Failed)?;
            fill(out)?;
        }
        Ok(memory)
    }

    fn copied(bytes: &[u8]) -> Result<Self, NativeError> {
        Self::filled(bytes.len(), |out| {
            out.copy_from_slice(bytes);
            Ok(())
        })
    }

    fn lock(&mut self) -> Option<Locked<'_>> {
        // SAFETY: this guard owns the allocation, and the lock borrows the guard.
        unsafe { Locked::new(self.0) }
    }

    /// The clipboard frees the memory once this succeeds.
    fn place(self, _clipboard: &Opened, format: u32) -> Result<(), NativeError> {
        // SAFETY: the clipboard is open on this thread; the memory is unlocked global memory, and
        // the system takes it over only when the call succeeds.
        unsafe { SetClipboardData(format, Some(HANDLE(self.0.0))) }
            .map_err(|_| NativeError::Failed)?;
        mem::forget(self);
        Ok(())
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // SAFETY: this guard still owns the allocation and no lock on it is alive.
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use monhop_protocol::clipboard::MAX_CLIPBOARD_PNG;
    use windows::Win32::System::DataExchange::EnumClipboardFormats;

    use super::*;
    use crate::clipboard::content::{EchoGuard, Observation};
    use crate::clipboard::image::{dib_to_rgba, encode_png};

    const MB: usize = 1 << 20;

    #[test]
    fn format_names_are_the_ones_other_programs_register() {
        assert_eq!(SOURCE_FORMAT, "MonHop.ClipboardSource");
        assert_eq!(
            EXCLUDE_FORMAT,
            "ExcludeClipboardContentFromMonitorProcessing"
        );
        assert_eq!(VIEWER_IGNORE_FORMAT, "Clipboard Viewer Ignore");
        assert_eq!(HISTORY_FORMAT, "CanIncludeInClipboardHistory");
        assert_eq!(CLOUD_FORMAT, "CanUploadToCloudClipboard");
        assert_eq!([PNG_FORMAT, MIME_PNG_FORMAT], ["PNG", "image/png"]);
        assert_eq!([TEXT, DIB, FILES, DIBV5], [13, 8, 15, 17]);
    }

    #[test]
    fn privacy_flags_read_a_little_endian_dword_and_ignore_the_allocation_tail() {
        assert_eq!(Flag::parse(Some(&[0, 0, 0, 0])), Flag::Value(0));
        assert_eq!(Flag::parse(Some(&[1, 0, 0, 0])), Flag::Value(1));
        assert_eq!(
            Flag::parse(Some(&[0, 1, 0, 0, 9, 9, 9, 9])),
            Flag::Value(256)
        );
        assert_eq!(Flag::parse(Some(&[1, 0, 0])), Flag::Unreadable);
        assert_eq!(Flag::parse(Some(&[])), Flag::Unreadable);
        assert_eq!(Flag::parse(None), Flag::Unreadable);
    }

    fn concealed(exclude: bool, viewer_ignore: bool, history: Flag, cloud: Flag) -> bool {
        let privacy = Privacy {
            exclude,
            viewer_ignore,
            history,
            cloud,
        };
        privacy.concealed()
    }

    #[test]
    fn any_monitor_marker_zero_flag_or_unreadable_flag_conceals_the_item() {
        use Flag::{Absent, Unreadable, Value};
        assert!(!concealed(false, false, Absent, Absent));
        assert!(!concealed(false, false, Value(1), Value(7)));
        assert!(concealed(true, false, Absent, Absent));
        assert!(concealed(false, true, Value(1), Value(1)));
        assert!(concealed(false, false, Value(0), Absent));
        assert!(concealed(false, false, Absent, Value(0)));
        assert!(concealed(false, false, Unreadable, Value(1)));
        assert!(concealed(false, false, Value(1), Unreadable));
    }

    fn utf16z(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn text_ends_at_the_first_nul_and_is_bounded_by_the_raw_limit() {
        assert_eq!(utf16_text(&utf16z("a\r\nb"), 64).unwrap(), "a\r\nb");
        let mut padded = utf16z("short");
        padded.resize(4096, 0xAA);
        assert_eq!(utf16_text(&padded, 16).unwrap(), "short");
        assert_eq!(utf16_text(&utf16z("abcd"), 8).unwrap(), "abcd");
        assert_eq!(utf16_text(&utf16z("abcde"), 8), Err(Skip::TooLarge));
        assert_eq!(utf16_text(&[b'o', 0, b'k', 0, 7], 64).unwrap(), "ok");
        assert_eq!(
            utf16_text(&[0x00, 0xD8, b'x', 0, 0, 0], 64).unwrap(),
            "\u{FFFD}x"
        );
        assert_eq!(utf16_text(&[], 64).unwrap(), "");
    }

    fn png_fixture() -> Vec<u8> {
        let image = Rgba::new(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        encode_png(&image, MAX_CLIPBOARD_PNG as usize).unwrap()
    }

    #[test]
    fn a_png_loses_its_allocation_tail_and_is_refused_past_the_limit() {
        let png = png_fixture();
        assert_eq!(png_end(&png), Some(png.len()));
        let mut padded = png.clone();
        padded.extend_from_slice(&[0; 13]);
        assert_eq!(png_bytes(&padded, MB).unwrap(), png);
        assert_eq!(png_bytes(&padded, png.len()).unwrap(), png);
        assert_eq!(png_bytes(&padded, png.len() - 1), Err(Skip::TooLarge));

        let truncated = &png[..png.len() - 1];
        assert_eq!(png_end(truncated), None);
        assert_eq!(png_bytes(truncated, MB).unwrap(), truncated);
        assert_eq!(png_end(b"not a png"), None);
        let mut huge_chunk = PNG_SIGNATURE.to_vec();
        huge_chunk.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, b'I', b'D', b'A', b'T']);
        assert_eq!(png_end(&huge_chunk), None);
        assert_eq!(copy_capped(&[1, 2, 3], 2), Err(Skip::TooLarge));
        assert_eq!(copy_capped(&[1, 2, 3], 3).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn global_memory_fills_exactly_the_requested_bytes_and_zeroes_the_rest() {
        let mut seen = 0;
        let mut memory = Global::filled(13, |out| {
            seen = out.len();
            out.fill(7);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 13);
        let locked = memory.lock().unwrap();
        let (filled, tail) = locked.bytes().split_at(13);
        assert!(filled.iter().all(|&byte| byte == 7));
        assert!(tail.iter().all(|&byte| byte == 0));
        drop(locked);
        assert_eq!(
            Global::filled(0, |_| Ok(())).err(),
            Some(NativeError::Failed)
        );
        let refused = Global::filled(8, |_| Err(NativeError::Failed));
        assert_eq!(refused.err(), Some(NativeError::Failed));

        let image = Rgba::new(3, 1, (0..12).collect()).unwrap();
        let mut dib = Global::filled(image.dibv5_len(), |out| {
            image.write_dibv5(out).map_err(|_| NativeError::Failed)
        })
        .unwrap();
        assert_eq!(dib_to_rgba(dib.lock().unwrap().bytes()).unwrap(), image);
        let mut copy = Global::copied(b"MHCS").unwrap();
        assert_eq!(&copy.lock().unwrap().bytes()[..4], b"MHCS");
    }

    #[test]
    fn wait_times_round_up_and_never_mean_forever() {
        assert_eq!(wait_millis(Duration::ZERO), 0);
        assert_eq!(wait_millis(Duration::from_micros(1)), 1);
        assert_eq!(wait_millis(Duration::from_millis(250)), 250);
        assert_eq!(wait_millis(Duration::MAX), u32::MAX - 1);
    }

    #[test]
    fn a_wake_ends_a_wait_early_and_outlives_the_window_harmlessly() {
        let mut clipboard = WindowsClipboard::new().unwrap();
        let started = Instant::now();
        clipboard.wait(Duration::from_millis(30));
        assert!(started.elapsed() >= Duration::from_millis(30));

        clipboard.waker().wake();
        let started = Instant::now();
        clipboard.wait(Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(10));

        let (sent, woken) = mpsc::channel();
        let waker = clipboard.waker();
        let waking = thread::spawn(move || {
            woken.recv().unwrap();
            thread::sleep(Duration::from_millis(20));
            waker.wake_by_ref();
            waker.wake_by_ref();
            waker
        });
        sent.send(()).unwrap();
        let started = Instant::now();
        clipboard.wait(Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(10));
        let waker = waking.join().unwrap();

        drop(clipboard);
        waker.wake();
    }

    static SERIAL: Mutex<()> = Mutex::new(());

    /// Serializes native tests and puts back what the user had on the clipboard, even when the
    /// test fails. Nothing saved is ever printed.
    struct Restore {
        _saved: Saved,
        _serial: MutexGuard<'static, ()>,
    }

    impl Restore {
        fn save() -> Self {
            let serial = lock(&SERIAL);
            Self {
                _saved: Saved::take(),
                _serial: serial,
            }
        }
    }

    /// Every global-memory format on the clipboard, placed back on drop.
    struct Saved {
        owner: WindowsClipboard,
        items: Vec<(u32, Vec<u8>)>,
    }

    impl Saved {
        fn take() -> Self {
            let owner = WindowsClipboard::new().unwrap();
            let clipboard = owner.open().unwrap();
            let mut items = Vec::new();
            let mut format = 0;
            for _ in 0..1024 {
                // SAFETY: the clipboard is open on this thread.
                format = unsafe { EnumClipboardFormats(format) };
                if format == 0 {
                    break;
                }
                if !held_as_global_memory(format) {
                    continue;
                }
                if let Some(data) = clipboard.data(format) {
                    items.push((format, data.bytes().to_vec()));
                }
            }
            drop(clipboard);
            Self { owner, items }
        }
    }

    impl Drop for Saved {
        fn drop(&mut self) {
            let Some(clipboard) = (0..20).find_map(|_| self.owner.open().ok()) else {
                return;
            };
            let _ = clipboard.empty();
            for (format, bytes) in self.items.drain(..) {
                if let Ok(memory) = Global::copied(&bytes) {
                    let _ = memory.place(&clipboard, format);
                }
            }
        }
    }

    /// GDI handles (bitmaps, metafiles, palettes) and private formats are not global memory;
    /// Windows synthesizes the bitmap again from the restored DIB.
    fn held_as_global_memory(format: u32) -> bool {
        !matches!(
            format,
            2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x200..=0x3FF
        )
    }

    fn put(clipboard: &WindowsClipboard, items: &[(u32, &[u8])]) {
        let open = clipboard.open().unwrap();
        open.empty().unwrap();
        for &(format, bytes) in items {
            Global::copied(bytes).unwrap().place(&open, format).unwrap();
        }
    }

    fn raw(clipboard: &WindowsClipboard, format: u32) -> Option<Vec<u8>> {
        let open = clipboard.open().unwrap();
        open.data(format).map(|data| data.bytes().to_vec())
    }

    fn settled(clipboard: &mut WindowsClipboard) -> Snapshot {
        let snapshot = clipboard.read(&ReadLimits::STANDARD).unwrap();
        assert_eq!(
            snapshot.before, snapshot.after,
            "the clipboard moved mid-read"
        );
        snapshot
    }

    const MARKER: SourceMarker = SourceMarker {
        token: 0x4D6F_6E48_6F70_5465,
        write: 1,
    };

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_tests_put_back_what_was_on_the_clipboard() {
        let _restore = Restore::save();
        let clipboard = WindowsClipboard::new().unwrap();
        let before = utf16z("MonHop test before");
        let history = clipboard.formats.history;
        put(&clipboard, &[(TEXT, &before), (history, &[0, 0, 0, 0])]);
        {
            let _saved = Saved::take();
            put(&clipboard, &[(TEXT, &utf16z("MonHop test during"))]);
        }
        let open = clipboard.open().unwrap();
        assert_eq!(open.flag(history), Flag::Value(0));
        let text = open.data(TEXT).unwrap();
        assert_eq!(utf16_text(text.bytes(), MB).unwrap(), "MonHop test before");
    }

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_text_round_trip() {
        let _restore = Restore::save();
        let mut clipboard = WindowsClipboard::new().unwrap();
        let approved = clipboard.change_marker();
        let after = clipboard
            .write_text("MonHop test\nsecond line \u{2713}\r\n", MARKER, approved)
            .unwrap();
        assert_eq!(clipboard.change_marker(), after);
        let written = raw(&clipboard, TEXT).unwrap();
        assert_eq!(
            utf16_text(&written, MB).unwrap(),
            "MonHop test\r\nsecond line \u{2713}\r\n"
        );

        // A local copy after the approval wins: the write changes nothing.
        put(&clipboard, &[(TEXT, &utf16z("MonHop test\r\nlocal copy"))]);
        let copied = clipboard.change_marker();
        assert_eq!(
            clipboard.write_text("MonHop test late", MARKER, after),
            Err(NativeError::Changed)
        );
        assert_eq!(clipboard.change_marker(), copied);
        let snapshot = settled(&mut clipboard);
        assert_eq!(snapshot.origin, Origin::Local);
        assert!(
            matches!(snapshot.content, Content::Text(ref text) if text == "MonHop test\r\nlocal copy"),
            "{:?}",
            snapshot.content
        );
    }

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_image_round_trip() {
        let _restore = Restore::save();
        let mut clipboard = WindowsClipboard::new().unwrap();
        let image = Rgba::new(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        let png = png_fixture();
        let approved = clipboard.change_marker();
        let after = clipboard
            .write_png(&png, Some(&image), MARKER, approved)
            .unwrap();
        assert_eq!(clipboard.change_marker(), after);
        let written = raw(&clipboard, clipboard.formats.png).unwrap();
        assert_eq!(png_bytes(&written, MB).unwrap(), png);
        let dib = raw(&clipboard, DIBV5).unwrap();
        assert_eq!(dib_to_rgba(&dib).unwrap(), image);

        put(&clipboard, &[(clipboard.formats.png, &png)]);
        let snapshot = settled(&mut clipboard);
        assert_eq!(snapshot.origin, Origin::Local);
        assert!(matches!(snapshot.content, Content::Png(ref bytes) if *bytes == png));

        put(&clipboard, &[(clipboard.formats.mime_png, &png)]);
        let snapshot = settled(&mut clipboard);
        assert!(matches!(snapshot.content, Content::Png(ref bytes) if *bytes == png));

        put(&clipboard, &[(DIBV5, &dib)]);
        let snapshot = settled(&mut clipboard);
        let Content::Dib(local) = snapshot.content else {
            panic!("expected a DIB, got {:?}", snapshot.content);
        };
        assert_eq!(dib_to_rgba(&local).unwrap(), image);
    }

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_concealed_and_file_items_are_skipped_unread() {
        let _restore = Restore::save();
        let mut clipboard = WindowsClipboard::new().unwrap();
        let text = utf16z("MonHop test concealed");
        let formats = &clipboard.formats;
        let (exclude, ignore, history, cloud) = (
            formats.exclude,
            formats.viewer_ignore,
            formats.history,
            formats.cloud,
        );
        let never: &[u8] = &[0, 0, 0, 0];
        let allow: &[u8] = &[1, 0, 0, 0];
        let short: &[u8] = &[1, 0];
        let cases: [&[(u32, &[u8])]; 6] = [
            &[(exclude, allow)],
            &[(ignore, allow)],
            &[(history, never)],
            &[(cloud, never)],
            &[(cloud, short)],
            &[(cloud, allow), (history, never)],
        ];
        for extra in cases {
            let mut items = vec![(TEXT, text.as_slice())];
            items.extend_from_slice(extra);
            put(&clipboard, &items);
            let snapshot = settled(&mut clipboard);
            assert_eq!(snapshot.origin, Origin::Local);
            assert!(
                matches!(snapshot.content, Content::Skipped(Skip::Concealed)),
                "{:?}",
                snapshot.content
            );
        }
        put(
            &clipboard,
            &[(TEXT, &text), (cloud, allow), (history, allow)],
        );
        let snapshot = settled(&mut clipboard);
        assert!(
            matches!(snapshot.content, Content::Text(_)),
            "{:?}",
            snapshot.content
        );

        // DROPFILES { pFiles: 20, pt: 0, fNC: 0, fWide: 1 } and an empty wide file list.
        let mut drop_files = vec![20, 0, 0, 0];
        drop_files.extend_from_slice(&[0; 12]);
        drop_files.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        put(
            &clipboard,
            &[(TEXT, &text), (FILES, &drop_files), (history, never)],
        );
        let snapshot = settled(&mut clipboard);
        assert!(matches!(
            snapshot.content,
            Content::Skipped(Skip::Concealed)
        ));
        put(&clipboard, &[(TEXT, &text), (FILES, &drop_files)]);
        let snapshot = settled(&mut clipboard);
        assert!(
            matches!(snapshot.content, Content::Skipped(Skip::Files)),
            "{:?}",
            snapshot.content
        );
    }

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_own_writes_are_recognized_as_monhop_origin() {
        let _restore = Restore::save();
        let mut clipboard = WindowsClipboard::new().unwrap();
        let mut guard = EchoGuard::new(clipboard.change_marker());
        let text = "MonHop test own write";
        let approved = clipboard.change_marker();
        assert!(guard.may_apply(Instant::now(), approved));
        let marker = guard.begin_apply(guard.text_key(text));
        let after = clipboard.write_text(text, marker, approved).unwrap();
        guard.applied(after);
        let observed = guard.observe(clipboard.change_marker(), Instant::now());
        assert_eq!(observed, Observation::OwnWrite);
        let snapshot = settled(&mut clipboard);
        assert_eq!(snapshot.origin, Origin::MonHop(Some(marker)));
        assert!(matches!(snapshot.content, Content::Empty));

        let second = SourceMarker { write: 2, ..MARKER };
        let approved = clipboard.change_marker();
        clipboard
            .write_png(&png_fixture(), None, second, approved)
            .unwrap();
        assert_eq!(settled(&mut clipboard).origin, Origin::MonHop(Some(second)));

        let source = clipboard.formats.source;
        put(
            &clipboard,
            &[(source, b"MHCS"), (TEXT, &utf16z("MonHop test"))],
        );
        let snapshot = settled(&mut clipboard);
        assert_eq!(snapshot.origin, Origin::MonHop(None));
        assert!(matches!(snapshot.content, Content::Empty));
    }

    #[test]
    #[ignore = "Explicit native check: replaces the clipboard, then restores what was on it"]
    fn native_writes_keep_received_content_on_this_computer() {
        let _restore = Restore::save();
        let mut clipboard = WindowsClipboard::new().unwrap();
        let flags = |clipboard: &WindowsClipboard| {
            let open = clipboard.open().unwrap();
            [
                open.flag(clipboard.formats.cloud),
                open.flag(clipboard.formats.history),
            ]
        };
        let approved = clipboard.change_marker();
        clipboard
            .write_text("MonHop test", MARKER, approved)
            .unwrap();
        assert_eq!(flags(&clipboard), [Flag::Value(0); 2]);
        let image = Rgba::new(1, 1, vec![1, 2, 3, 255]).unwrap();
        let png = encode_png(&image, MAX_CLIPBOARD_PNG as usize).unwrap();
        let approved = clipboard.change_marker();
        clipboard
            .write_png(&png, Some(&image), MARKER, approved)
            .unwrap();
        assert_eq!(flags(&clipboard), [Flag::Value(0); 2]);
    }
}
