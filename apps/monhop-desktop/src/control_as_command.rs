//! The keyboard switches, each kept by the computer its keyboard belongs to and off unless the user
//! turned it on: Ctrl as Command on Windows, Command as Ctrl on a Mac. Only this computer's own
//! switch reaches the transport, which asks each computer of the other platform for it as the
//! pointer enters.

use std::sync::Arc;

use monhop_transport::session_source::set_control_as_command;
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SettingNames, SwitchPreference, SwitchView};

const CONTROL_AS_COMMAND: SettingNames = SettingNames {
    file: "control-as-command.json",
    setting: "Ctrl as Command",
    logged: "Ctrl as Command on a Mac",
};

const COMMAND_AS_CONTROL: SettingNames = SettingNames {
    file: "command-as-control.json",
    setting: "Command as Ctrl",
    logged: "Command as Ctrl on Windows",
};

pub struct KeyboardSwitches {
    /// A Windows keyboard's switch for the Macs it controls.
    control_as_command: SwitchPreference,
    /// A Mac keyboard's switch for the PCs it controls.
    command_as_control: SwitchPreference,
}

impl KeyboardSwitches {
    /// Loads both switches and hands this computer's own to the transport before any session
    /// starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self {
            control_as_command: SwitchPreference::start(app, CONTROL_AS_COMMAND, false, |on| {
                apply(cfg!(windows), on)
            }),
            command_as_control: SwitchPreference::start(app, COMMAND_AS_CONTROL, false, |on| {
                apply(cfg!(target_os = "macos"), on)
            }),
        })
    }
}

/// The other platform's switch is kept but never applied, so it cannot overwrite this one's.
fn apply(owned: bool, enabled: bool) {
    if owned {
        set_control_as_command(enabled);
    }
}

fn switches(app: &AppHandle) -> Arc<KeyboardSwitches> {
    app.state::<Arc<KeyboardSwitches>>().inner().clone()
}

/// Applies from the next time the pointer enters a computer of the other platform, even when
/// saving fails.
async fn set(
    app: AppHandle,
    names: SettingNames,
    pick: fn(&KeyboardSwitches) -> &SwitchPreference,
    enabled: bool,
) -> Result<SwitchView, String> {
    let switches = switches(&app);
    tauri::async_runtime::spawn_blocking(move || pick(&switches).set(enabled))
        .await
        .map_err(|_| names.not_saved())
}

#[tauri::command]
pub async fn control_as_command_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(switches(&app).control_as_command.view())
}

#[tauri::command]
pub async fn control_as_command_set_enabled(
    app: AppHandle,
    enabled: bool,
) -> Result<SwitchView, String> {
    set(
        app,
        CONTROL_AS_COMMAND,
        |switches| &switches.control_as_command,
        enabled,
    )
    .await
}

#[tauri::command]
pub async fn command_as_control_status(app: AppHandle) -> Result<SwitchView, String> {
    Ok(switches(&app).command_as_control.view())
}

#[tauri::command]
pub async fn command_as_control_set_enabled(
    app: AppHandle,
    enabled: bool,
) -> Result<SwitchView, String> {
    set(
        app,
        COMMAND_AS_CONTROL,
        |switches| &switches.command_as_control,
        enabled,
    )
    .await
}
