//! Minimizing hides MonHop to the tray or menu bar instead of a taskbar button or Dock tile. It
//! is installed only once the tray exists, and a hide that fails falls back to a plain minimize,
//! so a window the user cannot get back is never hidden.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub use self::macos::hide_instead;
#[cfg(windows)]
pub use self::windows::{hide_if_minimized, hide_instead};

#[cfg(not(any(target_os = "macos", windows)))]
pub fn hide_instead(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}
