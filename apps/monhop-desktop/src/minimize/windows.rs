//! A minimize that arrives as a system command is turned into a hide before Windows starts it, so
//! the window never animates into the taskbar and is not minimized when shown again; any other
//! minimize is hidden once it lands there (`hide_if_minimized`).

use tauri::{AppHandle, Manager, WebviewWindow};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{SC_MINIMIZE, WM_NCDESTROY, WM_SYSCOMMAND},
    },
};

/// The subclass is keyed by this procedure and id together, so any constant id serves.
const SUBCLASS_ID: usize = 1;
/// The low four bits of a system command are Windows' own.
const SYSTEM_COMMAND_MASK: usize = 0xFFF0;

pub fn hide_instead(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    let app = Box::into_raw(Box::new(window.app_handle().clone()));
    // SAFETY: called on the window's own thread; the procedure frees `app` once, at WM_NCDESTROY.
    if unsafe { SetWindowSubclass(hwnd, Some(on_message), SUBCLASS_ID, app as usize) }.as_bool() {
        return Ok(());
    }
    // SAFETY: the subclass was refused, so nothing else holds the box.
    drop(unsafe { Box::from_raw(app) });
    Err("Windows refused the window subclass.".into())
}

/// Catches a minimize that skipped the system command, such as a direct `ShowWindow`: the window
/// is already in the taskbar, so it is hidden there, and the next show restores it.
pub fn hide_if_minimized(app: &AppHandle, label: &str) {
    if app
        .get_webview_window(label)
        .is_some_and(|window| window.is_minimized().unwrap_or(false))
    {
        let _ = crate::tray::hide_main_window(app);
    }
}

unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    app: usize,
) -> LRESULT {
    let app = app as *mut AppHandle;
    if message == WM_SYSCOMMAND && wparam.0 & SYSTEM_COMMAND_MASK == SC_MINIMIZE as usize {
        // SAFETY: `app` lives until WM_NCDESTROY, which has not arrived.
        if crate::tray::hide_main_window(unsafe { &*app }).is_ok() {
            return LRESULT(0);
        }
    } else if message == WM_NCDESTROY {
        // SAFETY: the window's last message; the subclass goes first, so nothing reads `app` again.
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(on_message), SUBCLASS_ID);
            drop(Box::from_raw(app));
        }
    }
    // SAFETY: forwards this message unchanged to the next procedure in the chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
