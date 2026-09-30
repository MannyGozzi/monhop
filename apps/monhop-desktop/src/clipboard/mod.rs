//! Optional clipboard sharing with connected computers: text and images, off by default.

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

pub use hub::ClipboardAttachment;
pub use hub::ClipboardHub;
pub use settings::ClipboardView;

/// Where the switch is saved: `clipboard.json` in the app's local data directory.
pub fn setting_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_local_data_dir()
        .ok()
        .map(|directory| directory.join(settings::FILE_NAME))
}

#[tauri::command]
pub fn clipboard_status(hub: tauri::State<'_, Arc<ClipboardHub>>) -> ClipboardView {
    hub.view()
}

/// Turning the switch off waits for a clipboard read in progress, so it never runs on the UI
/// thread.
#[tauri::command]
pub async fn clipboard_set_enabled(app: AppHandle, enabled: bool) -> Result<ClipboardView, String> {
    let hub = app.state::<Arc<ClipboardHub>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || hub.set_enabled(enabled))
        .await
        .map_err(|_| "The clipboard switch did not change. Try again.".to_owned())
}
