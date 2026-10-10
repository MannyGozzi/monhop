//! The yellow traffic light and Window > Minimize (Cmd+M) are retargeted to hide the window, so it
//! never takes the Genie path into the Dock; any other minimize is hidden once it lands there.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSMenu, NSWindow, NSWindowButton, NSWindowDidMiniaturizeNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSOperationQueue};
use tauri::{AppHandle, Manager, WebviewWindow};

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class never implements Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "MonHopHideOnMinimize"]
    #[ivars = WebviewWindow]
    struct HideOnMinimize;

    impl HideOnMinimize {
        #[unsafe(method(hideToMenuBar:))]
        fn hide_to_menu_bar(&self, _sender: Option<&AnyObject>) {
            let window = self.ivars();
            if crate::tray::hide_main_window(window.app_handle()).is_err() {
                let _ = window.minimize();
            }
        }
    }
);

impl HideOnMinimize {
    fn new(mtm: MainThreadMarker, window: WebviewWindow) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(window);
        // SAFETY: NSObject's init method has the declared signature.
        unsafe { msg_send![super(this), init] }
    }
}

pub fn hide_instead(window: &WebviewWindow) -> Result<(), String> {
    let mtm =
        MainThreadMarker::new().ok_or("Minimize can only be redirected on the main thread.")?;
    let ns_window = window.ns_window().map_err(|error| error.to_string())?;
    // SAFETY: Tauri returns this window's live NSWindow, which outlives this call.
    let ns_window: &NSWindow = unsafe { &*ns_window.cast() };
    let target = HideOnMinimize::new(mtm, window.clone());
    let object: &AnyObject = &target;
    let action = sel!(hideToMenuBar:);
    if let Some(button) = ns_window.standardWindowButton(NSWindowButton::MiniaturizeButton) {
        // SAFETY: the target answers `action` and is kept alive below for the app's whole life.
        unsafe {
            button.setTarget(Some(object));
            button.setAction(Some(action));
        }
    }
    if let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() {
        retarget_minimize_items(&menu, object, action);
    }
    hide_once_minimized(ns_window, window.app_handle().clone());
    // The button and menu items hold their target weakly.
    std::mem::forget(target);
    Ok(())
}

/// Tauri's default Window menu sends `performMiniaturize:` to the key window.
fn retarget_minimize_items(menu: &NSMenu, target: &AnyObject, action: Sel) {
    for item in menu.itemArray().to_vec() {
        if item.action() == Some(sel!(performMiniaturize:)) {
            // SAFETY: the target answers `action` and lives for the app's whole life.
            unsafe {
                item.setTarget(Some(target));
                item.setAction(Some(action));
            }
        }
        if let Some(submenu) = item.submenu() {
            retarget_minimize_items(&submenu, target, action);
        }
    }
}

/// The fallback for any other path, such as a title-bar double-click set to minimize.
fn hide_once_minimized(ns_window: &NSWindow, app: AppHandle) {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        let _ = crate::tray::hide_main_window(&app);
    });
    let object: &AnyObject = ns_window;
    // SAFETY: the observer runs on the main queue and touches only main-thread state.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSWindowDidMiniaturizeNotification),
            Some(object),
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };
    // Observes for the app's whole life.
    std::mem::forget(observer);
}
