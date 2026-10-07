//! One saved on-or-off setting, at its default until the user changes it, handed to the runtime
//! flag that acts on it at startup and on every change. A failed save keeps the change for this run.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;

/// How one setting is named on disk, to the user and in the log.
pub struct SwitchNames {
    /// In the app's local data folder.
    pub file: &'static str,
    /// As messages name it: "The {setting} setting could not be saved."
    pub setting: &'static str,
    /// As the log names the user's change: "user: {logged} on".
    pub logged: &'static str,
}

impl SwitchNames {
    /// The command error when the change never reached the saved setting.
    pub fn not_saved(&self) -> String {
        format!("The {} setting was not saved. Try again.", self.setting)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SwitchFile {
    version: u32,
    enabled: bool,
}

impl SwitchFile {
    const fn new(enabled: bool) -> Self {
        Self {
            version: FILE_VERSION,
            enabled,
        }
    }

    fn load(path: &Path, default: bool) -> io::Result<Self> {
        let Some(bytes) = crate::sharing_preferences::read_bounded(path, MAX_FILE_BYTES)? else {
            return Ok(Self::new(default));
        };
        let file: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if file.version != FILE_VERSION {
            return Err(invalid());
        }
        Ok(file)
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|_| invalid())?;
        crate::sharing_preferences::save_bounded(path, &bytes, MAX_FILE_BYTES)
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid switch preference")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchView {
    enabled: bool,
    error: Option<String>,
}

struct State {
    file: SwitchFile,
    /// A read or save failure the user should see.
    error: Option<String>,
}

pub struct SwitchPreference {
    names: SwitchNames,
    path: Option<PathBuf>,
    apply: fn(bool),
    state: Mutex<State>,
}

impl SwitchPreference {
    /// Loads the switch, `default` until the user changes it, and applies it before any session
    /// starts. Runs in setup.
    pub fn start(app: &AppHandle, names: SwitchNames, default: bool, apply: fn(bool)) -> Self {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(names.file));
        let (file, error) = match path.as_deref().map(|path| SwitchFile::load(path, default)) {
            Some(Ok(file)) => (file, None),
            Some(Err(_)) => (
                SwitchFile::new(default),
                Some(format!(
                    "The saved {} setting could not be read. It is {} until you change it.",
                    names.setting,
                    if default { "on" } else { "off" }
                )),
            ),
            None => (
                SwitchFile::new(default),
                Some(format!(
                    "The {} setting has nowhere to be saved.",
                    names.setting
                )),
            ),
        };
        apply(file.enabled);
        Self {
            names,
            path,
            apply,
            state: Mutex::new(State { file, error }),
        }
    }

    pub fn view(&self) -> SwitchView {
        view_of(&lock(&self.state))
    }

    /// Applies at once even when saving fails; sharing keeps running.
    pub fn set_enabled(&self, enabled: bool) -> SwitchView {
        let mut state = lock(&self.state);
        state.file.enabled = enabled;
        (self.apply)(enabled);
        log::info!(
            "user: {} {}",
            self.names.logged,
            if enabled { "on" } else { "off" }
        );
        let saved = self
            .path
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|path| state.file.save(path));
        state.error = saved
            .err()
            .map(|_| format!("The {} setting could not be saved.", self.names.setting));
        view_of(&state)
    }
}

fn view_of(state: &State) -> SwitchView {
    SwitchView {
        enabled: state.file.enabled,
        error: state.error.clone(),
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "monhop-switch-preference-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("switch.json")
    }

    #[test]
    fn a_missing_file_means_the_default() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(SwitchFile::load(&path, true).unwrap().enabled);
        assert!(!SwitchFile::load(&path, false).unwrap().enabled);
    }

    #[test]
    fn a_saved_switch_round_trips_and_another_version_is_rejected() {
        let path = temporary_path("roundtrip");
        let file = SwitchFile {
            version: FILE_VERSION,
            enabled: false,
        };
        file.save(&path).unwrap();
        assert_eq!(SwitchFile::load(&path, true).unwrap(), file);
        std::fs::write(&path, br#"{"version":2,"enabled":true}"#).unwrap();
        assert_eq!(
            SwitchFile::load(&path, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, br#"{"version":1,"enabled":true,"speed":9}"#).unwrap();
        assert!(SwitchFile::load(&path, true).is_err());
    }

    #[test]
    fn a_view_carries_the_switch_and_the_last_failure() {
        let state = State {
            file: SwitchFile::new(true),
            error: Some("nowhere to save".to_owned()),
        };
        assert_eq!(
            view_of(&state),
            SwitchView {
                enabled: true,
                error: Some("nowhere to save".to_owned()),
            }
        );
    }

    #[test]
    fn messages_name_the_setting() {
        let names = SwitchNames {
            file: "swipe.json",
            setting: "swipe",
            logged: "swipe between pages",
        };
        assert_eq!(
            names.not_saved(),
            "The swipe setting was not saved. Try again."
        );
    }
}
