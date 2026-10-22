//! Which pointer is live: the saved switch, on unless the user turned it off, handed to the park
//! signal, and the platform renderer that dims this computer's cursor while control is elsewhere and
//! rings where control lands. The renderer draws nothing until the first park, idle or arrival.

use std::sync::Arc;

use monhop_core::cursor_park::PARK;
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SwitchNames, SwitchPreference, SwitchView};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

const NAMES: SwitchNames = SwitchNames {
    file: "live-pointer.json",
    setting: "live pointer",
    logged: "live pointer marks",
};

pub struct CursorMark(SwitchPreference);

impl CursorMark {
    /// Loads the switch before any session starts and, with `show`, starts the renderer. Runs in
    /// setup.
    pub fn start(app: &AppHandle, show: bool) -> Arc<Self> {
        let preference = SwitchPreference::start(app, NAMES, true, set_enabled);
        if show {
            start_renderer(app);
        }
        Arc::new(Self(preference))
    }
}

fn set_enabled(enabled: bool) {
    PARK.set_enabled(enabled);
}

fn start_renderer(app: &AppHandle) {
    let style = match crate::island::tokens::mark_style() {
        Ok(style) => *style,
        Err(error) => {
            log::warn!("live pointer: {error}; its marks stay off");
            return;
        }
    };
    #[cfg(target_os = "macos")]
    macos::start(app, style);
    #[cfg(windows)]
    {
        let _ = app;
        windows::start(style);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (app, style);
}

/// Before the quit drain, on the main thread: nothing cloaks or rings again and the real cursor
/// shows at once.
pub fn reveal_for_exit() {
    PARK.close();
    #[cfg(target_os = "macos")]
    macos::reveal_for_exit();
    #[cfg(windows)]
    windows::reveal_for_exit();
}

#[tauri::command]
pub async fn live_pointer_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(app.state::<Arc<CursorMark>>().0.view())
}

/// Applies at once, revealing any copy when off, even when saving fails; sharing keeps running.
#[tauri::command]
pub async fn live_pointer_set_enabled(app: AppHandle, enabled: bool) -> Result<SwitchView, String> {
    let switch = app.state::<Arc<CursorMark>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || switch.0.set_enabled(enabled))
        .await
        .map_err(|_| NAMES.not_saved())
}
