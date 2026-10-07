//! Ctrl as Command for this computer's keyboard on a Mac: the saved switch, off unless the user
//! turned it on, handed to the transport that asks each Mac for it as the pointer enters.

use std::sync::Arc;

use monhop_transport::session_source::set_control_as_command;
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SwitchNames, SwitchPreference, SwitchView};

const NAMES: SwitchNames = SwitchNames {
    file: "control-as-command.json",
    setting: "Ctrl as Command",
    logged: "Ctrl as Command on a Mac",
};

pub struct ControlAsCommand(SwitchPreference);

impl ControlAsCommand {
    /// Loads the switch and hands it to the transport before any session starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self(SwitchPreference::start(app, NAMES, false, apply)))
    }
}

/// Only a Windows keyboard asks: a Mac's own keyboard already has Command.
fn apply(enabled: bool) {
    set_control_as_command(cfg!(windows) && enabled);
}

#[tauri::command]
pub async fn control_as_command_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(app.state::<Arc<ControlAsCommand>>().0.view())
}

/// Applies from the next time the pointer enters a Mac, even when saving fails.
#[tauri::command]
pub async fn control_as_command_set_enabled(
    app: AppHandle,
    enabled: bool,
) -> Result<SwitchView, String> {
    let switch = app.state::<Arc<ControlAsCommand>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || switch.0.set_enabled(enabled))
        .await
        .map_err(|_| NAMES.not_saved())
}
