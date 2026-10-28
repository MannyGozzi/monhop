//! One saved setting, at its default until the user changes it, handed to the runtime that acts on
//! it at startup and on every change. A failed save keeps the change for this run.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tauri::{AppHandle, Manager};

const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;

/// How one setting is named on disk, to the user and in the log.
pub struct SettingNames {
    /// In the app's local data folder.
    pub file: &'static str,
    /// As messages name it: "The {setting} setting could not be saved."
    pub setting: &'static str,
    /// As the log names the user's change: "user: {logged} on".
    pub logged: &'static str,
}

impl SettingNames {
    /// The command error when the change never reached the saved setting.
    pub fn not_saved(&self) -> String {
        format!("The {} setting was not saved. Try again.", self.setting)
    }
}

/// A setting's file: a version and the value, read and written whole. Each kind of value keeps its
/// own file type, so what one kind writes never changes when another is added.
pub trait SettingFile: Copy + Serialize + DeserializeOwned {
    type Value: Copy;
    /// What the window reads: the value and the last failure.
    type View;

    fn new(value: Self::Value) -> Self;
    fn value(self) -> Self::Value;
    /// This file version, holding a value the app accepts.
    fn is_valid(self) -> bool;
    fn view(self, error: Option<String>) -> Self::View;
    /// The value as messages and the log name it: "on", "Same".
    fn words(value: Self::Value) -> String;

    fn load(path: &Path, default: Self::Value) -> io::Result<Self> {
        let Some(bytes) = crate::sharing_preferences::read_bounded(path, MAX_FILE_BYTES)? else {
            return Ok(Self::new(default));
        };
        let file: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if !file.is_valid() {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid saved setting")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwitchFile {
    version: u32,
    enabled: bool,
}

impl SettingFile for SwitchFile {
    type Value = bool;
    type View = SwitchView;

    fn new(enabled: bool) -> Self {
        Self {
            version: FILE_VERSION,
            enabled,
        }
    }

    fn value(self) -> bool {
        self.enabled
    }

    fn is_valid(self) -> bool {
        self.version == FILE_VERSION
    }

    fn view(self, error: Option<String>) -> SwitchView {
        SwitchView {
            enabled: self.enabled,
            error,
        }
    }

    fn words(enabled: bool) -> String {
        if enabled { "on" } else { "off" }.to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchView {
    enabled: bool,
    error: Option<String>,
}

struct State<F> {
    file: F,
    /// A read or save failure the user should see.
    error: Option<String>,
}

pub struct SavedSetting<F: SettingFile> {
    names: SettingNames,
    path: Option<PathBuf>,
    apply: fn(F::Value),
    state: Mutex<State<F>>,
}

/// An on-or-off setting.
pub type SwitchPreference = SavedSetting<SwitchFile>;

impl<F: SettingFile> SavedSetting<F> {
    /// Loads the setting, `default` until the user changes it, and applies it before any session
    /// starts. Runs in setup.
    pub fn start(
        app: &AppHandle,
        names: SettingNames,
        default: F::Value,
        apply: fn(F::Value),
    ) -> Self {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(names.file));
        let (file, error) = match path.as_deref().map(|path| F::load(path, default)) {
            Some(Ok(file)) => (file, None),
            Some(Err(_)) => (
                F::new(default),
                Some(format!(
                    "The saved {} setting could not be read. It is {} until you change it.",
                    names.setting,
                    F::words(default)
                )),
            ),
            None => (
                F::new(default),
                Some(format!(
                    "The {} setting has nowhere to be saved.",
                    names.setting
                )),
            ),
        };
        apply(file.value());
        Self {
            names,
            path,
            apply,
            state: Mutex::new(State { file, error }),
        }
    }

    pub fn view(&self) -> F::View {
        view_of(&lock(&self.state))
    }

    /// Applies at once even when saving fails; sharing keeps running.
    pub fn set(&self, value: F::Value) -> F::View {
        let mut state = lock(&self.state);
        state.file = F::new(value);
        (self.apply)(value);
        log::info!("user: {} {}", self.names.logged, F::words(value));
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

fn view_of<F: SettingFile>(state: &State<F>) -> F::View {
    state.file.view(state.error.clone())
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
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
    fn a_saved_switch_keeps_its_bytes() {
        let path = temporary_path("bytes");
        SwitchFile::new(true).save(&path).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\n  \"version\": 1,\n  \"enabled\": true\n}"
        );
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
        let names = SettingNames {
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
