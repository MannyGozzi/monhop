//! Optional clipboard sharing with connected computers: text and images, off by default.

// The hub wiring step removes this once the app calls into the module.
#![cfg_attr(not(test), allow(dead_code))]

mod content;
mod hub;
mod image;
mod native;
mod settings;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::{path::PathBuf, sync::Arc};

use tauri::{AppHandle, Manager};

pub use hub::ClipboardHub;
// Each Share session holds one once the wiring step lands.
#[allow(unused_imports)]
pub use hub::ClipboardAttachment;
pub use settings::ClipboardView;

// The app glue below needs an AppHandle, which unit tests cannot build; main.rs uses it once wired.

/// Where the switch is saved: `clipboard.json` in the app's local data directory.
#[cfg_attr(test, allow(dead_code))]
pub fn setting_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|directory| directory.join(settings::FILE_NAME))
}

#[cfg_attr(test, allow(dead_code))]
#[tauri::command]
pub fn clipboard_status(hub: tauri::State<'_, Arc<ClipboardHub>>) -> ClipboardView {
    hub.view()
}

#[cfg_attr(test, allow(dead_code))]
#[tauri::command]
pub fn clipboard_set_enabled(
    hub: tauri::State<'_, Arc<ClipboardHub>>,
    enabled: bool,
) -> ClipboardView {
    hub.set_enabled(enabled)
}
