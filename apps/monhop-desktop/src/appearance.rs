//! Persisted appearance preference: system/light/dark, applied to the main window's native theme.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const FILE_NAME: &str = "appearance.json";
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Theme {
    pub(crate) fn native(self) -> Option<tauri::Theme> {
        match self {
            Self::System => None,
            Self::Light => Some(tauri::Theme::Light),
            Self::Dark => Some(tauri::Theme::Dark),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AppearanceFile {
    version: u32,
    theme: Theme,
}

impl Default for AppearanceFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            theme: Theme::System,
        }
    }
}

impl AppearanceFile {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid appearance preference")
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceView {
    theme: Theme,
}

pub struct Appearance {
    path: Option<PathBuf>,
    state: Mutex<AppearanceFile>,
}

impl Appearance {
    /// Loads the preference; a read failure falls back to the system theme rather than blocking startup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(FILE_NAME));
        let file = match path.as_deref().map(AppearanceFile::load) {
            Some(Ok(file)) => file,
            Some(Err(_)) | None => {
                log::warn!("appearance: preference could not be read, using the system theme");
                AppearanceFile::default()
            }
        };
        Arc::new(Self {
            path,
            state: Mutex::new(file),
        })
    }

    pub fn theme(&self) -> Theme {
        lock(&self.state).theme
    }

    /// Saves the theme and applies it to the main window live. A save failure keeps the in-memory choice.
    pub fn set_theme(&self, app: &AppHandle, theme: Theme) -> Result<AppearanceView, String> {
        let mut save_error = None;
        {
            let mut state = lock(&self.state);
            state.theme = theme;
            let saved = self
                .path
                .as_deref()
                .ok_or_else(invalid)
                .and_then(|path| state.save(path));
            if saved.is_err() {
                save_error = Some("The appearance preference could not be saved.".to_owned());
            }
        }
        let apply_error = app
            .get_webview_window("main")
            .and_then(|window| window.set_theme(theme.native()).err())
            .map(|err| err.to_string());
        log::info!("user: theme set to {theme:?}");
        match apply_error.or(save_error) {
            Some(error) => Err(error),
            None => Ok(AppearanceView { theme }),
        }
    }
}

fn lock(state: &Mutex<AppearanceFile>) -> std::sync::MutexGuard<'_, AppearanceFile> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tauri::command]
pub fn appearance_status(appearance: tauri::State<'_, Arc<Appearance>>) -> AppearanceView {
    AppearanceView {
        theme: appearance.theme(),
    }
}

#[tauri::command]
pub fn appearance_set_theme(
    app: AppHandle,
    appearance: tauri::State<'_, Arc<Appearance>>,
    theme: Theme,
) -> Result<AppearanceView, String> {
    appearance.set_theme(&app, theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("monhop-appearance-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_means_the_system_theme() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        let file = AppearanceFile::load(&path).unwrap();
        assert_eq!(file.theme, Theme::System);
    }

    #[test]
    fn a_saved_preference_round_trips() {
        let path = temporary_path("roundtrip");
        let file = AppearanceFile {
            version: FILE_VERSION,
            theme: Theme::Dark,
        };
        file.save(&path).unwrap();
        assert_eq!(AppearanceFile::load(&path).unwrap(), file);
    }

    #[test]
    fn an_invalid_version_is_rejected() {
        let path = temporary_path("bad-version");
        std::fs::write(&path, br#"{"version":2,"theme":"light"}"#).unwrap();
        assert_eq!(
            AppearanceFile::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn an_oversized_file_is_rejected() {
        let path = temporary_path("oversized");
        let padding = "x".repeat(MAX_FILE_BYTES as usize + 1);
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"theme":"light","p":"{padding}"}}"#),
        )
        .unwrap();
        assert!(AppearanceFile::load(&path).is_err());
    }

    #[test]
    fn theme_serializes_to_the_three_lowercase_strings() {
        assert_eq!(serde_json::to_string(&Theme::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Theme::Light).unwrap(), "\"light\"");
        assert_eq!(serde_json::to_string(&Theme::Dark).unwrap(), "\"dark\"");
    }
}
