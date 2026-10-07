//! Middle-click autoscroll on a Mac driven by a Windows mouse: the saved switch, on unless the user
//! turned it off, handed to the transport that runs it, and on macOS the origin marker it shows.

use std::sync::Arc;

use monhop_transport::session_native::set_autoscroll_enabled;
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SwitchNames, SwitchPreference, SwitchView};

#[cfg(target_os = "macos")]
mod macos;

const NAMES: SwitchNames = SwitchNames {
    file: "autoscroll.json",
    setting: "autoscroll",
    logged: "middle-click autoscroll",
};

pub struct Autoscroll(SwitchPreference);

impl Autoscroll {
    /// Loads the switch and hands it to the transport before any session starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        let preference = SwitchPreference::start(app, NAMES, true, set_autoscroll_enabled);
        #[cfg(target_os = "macos")]
        macos::register(app);
        Arc::new(Self(preference))
    }
}

#[tauri::command]
pub async fn autoscroll_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(app.state::<Arc<Autoscroll>>().0.view())
}

/// Applies from the next middle press even when saving fails; sharing keeps running.
#[tauri::command]
pub async fn autoscroll_set_enabled(app: AppHandle, enabled: bool) -> Result<SwitchView, String> {
    let autoscroll = app.state::<Arc<Autoscroll>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || autoscroll.0.set_enabled(enabled))
        .await
        .map_err(|_| NAMES.not_saved())
}
