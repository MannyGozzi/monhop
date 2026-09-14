//! macOS dimming: one black borderless window per screen above everything, click-through and
//! hidden from screen capture, plus the Carbon hotkey that toggles it. Every entry point runs on
//! the main thread, which is why each takes a `MainThreadMarker`.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use block2::RcBlock;
use monhop_core::dimming::{Chord, DimLevel};
use objc2::{
    MainThreadMarker,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext,
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSColor, NSScreen,
    NSScreenSaverWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue};

const FADE_SECONDS: f64 = 0.18;

#[derive(Default)]
struct Overlay {
    windows: Vec<Retained<NSWindow>>,
    level: DimLevel,
    shown: bool,
    screen_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

thread_local! {
    static OVERLAY: RefCell<Overlay> = RefCell::new(Overlay::default());
    static HOTKEY: RefCell<Option<HotkeyRegistration>> = const { RefCell::new(None) };
}

/// Covers every screen at `level`, fading in; a shown overlay only changes level.
pub fn show(mtm: MainThreadMarker, level: DimLevel) {
    let already_shown = OVERLAY.with(|overlay| {
        let mut overlay = overlay.borrow_mut();
        overlay.level = level;
        std::mem::replace(&mut overlay.shown, true)
    });
    if already_shown {
        set_level(mtm, level);
        return;
    }
    watch_screens(mtm);
    let windows = cover_screens(mtm);
    fade(&windows, level.alpha(), None);
    let previous =
        OVERLAY.with(|overlay| std::mem::replace(&mut overlay.borrow_mut().windows, windows));
    close_all(previous);
}

/// Fades every overlay window out and closes it.
pub fn hide(mtm: MainThreadMarker) {
    let _ = mtm;
    let windows = OVERLAY.with(|overlay| {
        let mut overlay = overlay.borrow_mut();
        overlay.shown = false;
        std::mem::take(&mut overlay.windows)
    });
    if windows.is_empty() {
        return;
    }
    let closing = windows.clone();
    fade(
        &windows,
        0.0,
        Some(RcBlock::new(move || close_all(closing.clone()))),
    );
}

/// Changes the darkness of a shown overlay immediately, so a slider drag previews live.
pub fn set_level(mtm: MainThreadMarker, level: DimLevel) {
    let _ = mtm;
    OVERLAY.with(|overlay| {
        let mut overlay = overlay.borrow_mut();
        overlay.level = level;
        if overlay.shown {
            for window in &overlay.windows {
                window.setAlphaValue(level.alpha());
            }
        }
    });
}

/// Registers `chord` system-wide; `on_press` runs on the main thread for every press.
pub fn register_hotkey(
    mtm: MainThreadMarker,
    chord: Chord,
    on_press: Box<dyn Fn() + 'static>,
) -> Result<(), String> {
    unregister_hotkey(mtm);
    let registration = HotkeyRegistration::new(chord, on_press)?;
    HOTKEY.with(|slot| *slot.borrow_mut() = Some(registration));
    Ok(())
}

pub fn unregister_hotkey(mtm: MainThreadMarker) {
    let _ = mtm;
    HOTKEY.with(|slot| slot.borrow_mut().take());
}

fn cover_screens(mtm: MainThreadMarker) -> Vec<Retained<NSWindow>> {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| cover(mtm, &screen))
        .collect()
}

fn cover(mtm: MainThreadMarker, screen: &NSScreen) -> Retained<NSWindow> {
    // SAFETY: a borderless buffered window created on the main thread from an AppKit screen frame.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer_screen(
            mtm.alloc::<NSWindow>(),
            screen.frame(),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
            None,
        )
    };
    // SAFETY: the Retained handle owns the window; AppKit must not release it on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window.setIgnoresMouseEvents(true);
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    window.setOpaque(false);
    window.setHasShadow(false);
    // Never part of a screen recording or a shared screen, like the display list never sees it.
    window.setSharingType(NSWindowSharingType::None);
    window.setHidesOnDeactivate(false);
    window.setCanHide(false);
    window.setAlphaValue(0.0);
    window.orderFrontRegardless();
    window
}

fn fade(windows: &[Retained<NSWindow>], alpha: f64, completion: Option<RcBlock<dyn Fn()>>) {
    let targets = windows.to_vec();
    let changes = RcBlock::new(move |context: NonNull<NSAnimationContext>| {
        // SAFETY: AppKit hands the block a live animation context for the duration of the call.
        unsafe { context.as_ref() }.setDuration(FADE_SECONDS);
        for window in &targets {
            window.animator().setAlphaValue(alpha);
        }
    });
    NSAnimationContext::runAnimationGroup_completionHandler(&changes, completion.as_deref());
}

fn close_all(windows: Vec<Retained<NSWindow>>) {
    for window in windows {
        window.orderOut(None);
        window.close();
    }
}

/// Rebuilds the shown overlay when displays are added, removed, or rearranged.
fn watch_screens(mtm: MainThreadMarker) {
    if OVERLAY.with(|overlay| overlay.borrow().screen_observer.is_some()) {
        return;
    }
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let level = OVERLAY.with(|overlay| {
            let overlay = overlay.borrow();
            overlay.shown.then_some(overlay.level)
        });
        let Some(level) = level else {
            return;
        };
        let windows = cover_screens(mtm);
        for window in &windows {
            window.setAlphaValue(level.alpha());
        }
        let previous =
            OVERLAY.with(|overlay| std::mem::replace(&mut overlay.borrow_mut().windows, windows));
        close_all(previous);
    });
    let _ = mtm;
    // SAFETY: the observer runs on the main queue and touches only main-thread state.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidChangeScreenParametersNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };
    OVERLAY.with(|overlay| overlay.borrow_mut().screen_observer = Some(observer));
}

type OSStatus = i32;
type EventRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerUPP = unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUPP,
        count: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_handler: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_hotkey: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hotkey: EventHotKeyRef) -> OSStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
}

const NO_ERR: OSStatus = 0;
const EVENT_CLASS_KEYBOARD: u32 = 0x6B65_7962; // 'keyb'
const EVENT_HOT_KEY_PRESSED: u32 = 5;
const EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2D2D_2D2D; // '----'
const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686B_6964; // 'hkid'
const HOTKEY_SIGNATURE: u32 = 0x4B4D_646D; // 'KMdm'
const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

/// The system-wide hotkey and the handler delivering its presses to `callback`.
struct HotkeyRegistration {
    hotkey: EventHotKeyRef,
    handler: EventHandlerRef,
    callback: *mut Box<dyn Fn()>,
}

impl HotkeyRegistration {
    fn new(chord: Chord, on_press: Box<dyn Fn() + 'static>) -> Result<Self, String> {
        let key = monhop_platform_macos::hid_to_mac_virtual_key(chord.key)
            .map_err(|_| "The dimming shortcut uses a key this Mac cannot register.".to_owned())?;
        let spec = EventTypeSpec {
            event_class: EVENT_CLASS_KEYBOARD,
            event_kind: EVENT_HOT_KEY_PRESSED,
        };
        let callback = Box::into_raw(Box::new(on_press));
        let mut handler: EventHandlerRef = ptr::null_mut();
        // SAFETY: the callback box outlives the handler; Drop removes the handler before freeing it.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hotkey_pressed,
                1,
                &spec,
                callback.cast(),
                &mut handler,
            )
        };
        if status != NO_ERR {
            // SAFETY: nothing else holds the pointer after a failed installation.
            drop(unsafe { Box::from_raw(callback) });
            return Err(format!(
                "The dimming shortcut handler could not be installed (status {status})."
            ));
        }
        let id = EventHotKeyID {
            signature: HOTKEY_SIGNATURE,
            id: 1,
        };
        let mut hotkey: EventHotKeyRef = ptr::null_mut();
        // SAFETY: plain Carbon registration against the application event target.
        let status = unsafe {
            RegisterEventHotKey(
                u32::from(key.0),
                carbon_modifiers(chord),
                id,
                GetApplicationEventTarget(),
                0,
                &mut hotkey,
            )
        };
        if status != NO_ERR {
            // SAFETY: the handler was installed above and the callback is freed after its removal.
            unsafe {
                RemoveEventHandler(handler);
                drop(Box::from_raw(callback));
            }
            return Err(format!(
                "The dimming shortcut could not be registered (status {status})."
            ));
        }
        Ok(Self {
            hotkey,
            handler,
            callback,
        })
    }
}

impl Drop for HotkeyRegistration {
    fn drop(&mut self) {
        // SAFETY: removal is synchronous on the main thread, so no press can still use the callback.
        unsafe {
            UnregisterEventHotKey(self.hotkey);
            RemoveEventHandler(self.handler);
            drop(Box::from_raw(self.callback));
        }
    }
}

fn carbon_modifiers(chord: Chord) -> u32 {
    let mut modifiers = 0;
    if chord.control {
        modifiers |= CONTROL_KEY;
    }
    if chord.alt {
        modifiers |= OPTION_KEY;
    }
    if chord.shift {
        modifiers |= SHIFT_KEY;
    }
    if chord.command {
        modifiers |= CMD_KEY;
    }
    modifiers
}

unsafe extern "C" fn hotkey_pressed(
    _: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    let mut id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    // SAFETY: Carbon fills exactly one EventHotKeyID for a hot-key-pressed event.
    let status = unsafe {
        GetEventParameter(
            event,
            EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            ptr::null_mut(),
            (&raw mut id).cast(),
        )
    };
    if status == NO_ERR && id.signature == HOTKEY_SIGNATURE && !user_data.is_null() {
        // SAFETY: user_data is the callback box installed by HotkeyRegistration, still alive.
        let callback = unsafe { &*user_data.cast::<Box<dyn Fn()>>() };
        callback();
    }
    NO_ERR
}

#[allow(dead_code)]
fn _assert_any_object_is_used(_: &AnyObject) {}
