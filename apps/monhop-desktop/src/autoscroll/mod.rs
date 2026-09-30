//! Middle-click autoscroll on a Mac driven by a Windows mouse: the saved switch, on unless the user
//! turned it off, handed to the transport that runs it, and on macOS the origin marker it shows.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use monhop_transport::session_native::set_autoscroll_enabled;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[cfg(target_os = "macos")]
mod macos;

const FILE_NAME: &str = "autoscroll.json";
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutoscrollFile {
    version: u32,
    enabled: bool,
}

impl Default for AutoscrollFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: true,
        }
    }
}

impl AutoscrollFile {
    fn load(path: &Path) -> io::Result<Self> {
        let Some(bytes) = crate::sharing_preferences::read_bounded(path, MAX_FILE_BYTES)? else {
            return Ok(Self::default());
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid autoscroll preference")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoscrollView {
    enabled: bool,
    error: Option<String>,
}

struct State {
    file: AutoscrollFile,
    /// A read or save failure the user should see.
    error: Option<String>,
}

pub struct Autoscroll {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl Autoscroll {
    /// Loads the switch and hands it to the transport before any session starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(FILE_NAME));
        let (file, error) = match path.as_deref().map(AutoscrollFile::load) {
            Some(Ok(file)) => (file, None),
            Some(Err(_)) => (
                AutoscrollFile::default(),
                Some(
                    "The saved autoscroll setting could not be read. It is on until you change it."
                        .to_owned(),
                ),
            ),
            None => (
                AutoscrollFile::default(),
                Some("The autoscroll setting has nowhere to be saved.".to_owned()),
            ),
        };
        set_autoscroll_enabled(file.enabled);
        #[cfg(target_os = "macos")]
        macos::register(app);
        Arc::new(Self {
            path,
            state: Mutex::new(State { file, error }),
        })
    }

    pub fn view(&self) -> AutoscrollView {
        view_of(&lock(&self.state))
    }

    /// Applies from the next middle press even when saving fails; sharing keeps running.
    pub fn set_enabled(&self, enabled: bool) -> AutoscrollView {
        let mut state = lock(&self.state);
        state.file.enabled = enabled;
        set_autoscroll_enabled(enabled);
        log::info!(
            "user: middle-click autoscroll {}",
            if enabled { "on" } else { "off" }
        );
        let saved = self
            .path
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|path| state.file.save(path));
        state.error = saved
            .err()
            .map(|_| "The autoscroll setting could not be saved.".to_owned());
        view_of(&state)
    }
}

fn view_of(state: &State) -> AutoscrollView {
    AutoscrollView {
        enabled: state.file.enabled,
        error: state.error.clone(),
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tauri::command]
pub async fn autoscroll_status(app: AppHandle) -> Result<AutoscrollView, String> {
    Ok(app.state::<Arc<Autoscroll>>().view())
}

#[tauri::command]
pub async fn autoscroll_set_enabled(
    app: AppHandle,
    enabled: bool,
) -> Result<AutoscrollView, String> {
    let autoscroll = app.state::<Arc<Autoscroll>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || autoscroll.set_enabled(enabled))
        .await
        .map_err(|_| "The autoscroll setting was not saved. Try again.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("monhop-autoscroll-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_means_autoscroll_is_on() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(AutoscrollFile::load(&path).unwrap().enabled);
    }

    #[test]
    fn a_saved_switch_round_trips_and_another_version_is_rejected() {
        let path = temporary_path("roundtrip");
        let file = AutoscrollFile {
            version: FILE_VERSION,
            enabled: false,
        };
        file.save(&path).unwrap();
        assert_eq!(AutoscrollFile::load(&path).unwrap(), file);
        std::fs::write(&path, br#"{"version":2,"enabled":true}"#).unwrap();
        assert_eq!(
            AutoscrollFile::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, br#"{"version":1,"enabled":true,"speed":9}"#).unwrap();
        assert!(AutoscrollFile::load(&path).is_err());
    }

    #[test]
    fn a_view_carries_the_switch_and_the_last_failure() {
        let state = State {
            file: AutoscrollFile::default(),
            error: Some("nowhere to save".to_owned()),
        };
        assert_eq!(
            view_of(&state),
            AutoscrollView {
                enabled: true,
                error: Some("nowhere to save".to_owned()),
            }
        );
    }
}
