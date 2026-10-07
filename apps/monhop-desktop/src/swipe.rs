//! Swipe between pages from a Mac trackpad: the saved switch, on unless the user turned it off,
//! handed to the macOS capture that turns a quick two-finger swipe into Back or Forward.

use std::sync::Arc;

use monhop_platform_macos::set_page_swipes_enabled;
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SwitchNames, SwitchPreference, SwitchView};

const NAMES: SwitchNames = SwitchNames {
    file: "swipe.json",
    setting: "swipe",
    logged: "swipe between pages",
};

pub struct Swipe(SwitchPreference);

impl Swipe {
    /// Loads the switch and hands it to the capture before any session starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self(SwitchPreference::start(
            app,
            NAMES,
            true,
            set_page_swipes_enabled,
        )))
    }
}

#[tauri::command]
pub async fn swipe_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(app.state::<Arc<Swipe>>().0.view())
}

/// Applies from the next swipe even when saving fails; sharing keeps running.
#[tauri::command]
pub async fn swipe_set_enabled(app: AppHandle, enabled: bool) -> Result<SwitchView, String> {
    let swipe = app.state::<Arc<Swipe>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || swipe.0.set_enabled(enabled))
        .await
        .map_err(|_| NAMES.not_saved())
}
