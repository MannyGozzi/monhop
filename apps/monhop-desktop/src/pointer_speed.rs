//! This computer's pointer speed on the computers it controls: the saved step, Same until the user
//! changes it, handed to the transport that scales the motion this computer sends.

use std::sync::Arc;

use monhop_core::pointer_speed::{MAX_STEP, MIN_STEP, SAME};
use monhop_transport::session_source::set_pointer_speed;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::switch_preference::{SavedSetting, SettingFile, SettingNames};

const NAMES: SettingNames = SettingNames {
    file: "pointer-speed.json",
    setting: "pointer speed",
    logged: "pointer speed",
};
const FILE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpeedFile {
    version: u32,
    step: i8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerSpeedView {
    step: i8,
    error: Option<String>,
}

impl SettingFile for SpeedFile {
    type Value = i8;
    type View = PointerSpeedView;

    fn new(step: i8) -> Self {
        Self {
            version: FILE_VERSION,
            step,
        }
    }

    fn value(self) -> i8 {
        self.step
    }

    fn is_valid(self) -> bool {
        self.version == FILE_VERSION && in_range(self.step)
    }

    fn view(self, error: Option<String>) -> PointerSpeedView {
        PointerSpeedView {
            step: self.step,
            error,
        }
    }

    /// Same is the only step a message names, as the default; the log records any step.
    fn words(step: i8) -> String {
        if step == SAME {
            "Same".to_owned()
        } else {
            format!("step {step}")
        }
    }
}

fn in_range(step: i8) -> bool {
    (MIN_STEP..=MAX_STEP).contains(&step)
}

pub struct PointerSpeed(SavedSetting<SpeedFile>);

impl PointerSpeed {
    /// Loads the saved step and hands it to the transport before any session starts. Runs in setup.
    pub fn start(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self(SavedSetting::start(
            app,
            NAMES,
            SAME,
            set_pointer_speed,
        )))
    }
}

#[tauri::command]
pub async fn pointer_speed_status(app: AppHandle) -> Result<PointerSpeedView, String> {
    Ok(app.state::<Arc<PointerSpeed>>().0.view())
}

/// Applies from the next motion even when saving fails.
#[tauri::command]
pub async fn pointer_speed_set(app: AppHandle, step: i8) -> Result<PointerSpeedView, String> {
    if !in_range(step) {
        return Err(format!(
            "Choose a pointer speed step from {MIN_STEP} to {MAX_STEP}."
        ));
    }
    let speed = app.state::<Arc<PointerSpeed>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || speed.0.set(step))
        .await
        .map_err(|_| NAMES.not_saved())
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "monhop-pointer-speed-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("pointer-speed.json")
    }

    #[test]
    fn a_missing_file_means_same() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(SpeedFile::load(&path, SAME).unwrap().step, SAME);
    }

    #[test]
    fn a_saved_step_round_trips() {
        let path = temporary_path("roundtrip");
        SpeedFile::new(-3).save(&path).unwrap();
        assert_eq!(SpeedFile::load(&path, SAME).unwrap(), SpeedFile::new(-3));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\n  \"version\": 1,\n  \"step\": -3\n}"
        );
    }

    #[test]
    fn other_versions_steps_out_of_range_and_unknown_fields_are_rejected() {
        let path = temporary_path("rejected");
        for content in [
            r#"{"version":2,"step":1}"#,
            r#"{"version":1,"step":6}"#,
            r#"{"version":1,"step":-6}"#,
            r#"{"version":1,"step":300}"#,
            r#"{"version":1,"step":1.5}"#,
            r#"{"version":1,"step":1,"enabled":true}"#,
            r#"{"version":1}"#,
            "not json",
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(
                SpeedFile::load(&path, SAME).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{content}"
            );
        }
    }

    #[test]
    fn only_the_contract_range_is_accepted() {
        assert!(in_range(MIN_STEP) && in_range(SAME) && in_range(MAX_STEP));
        assert!(!in_range(MIN_STEP - 1) && !in_range(MAX_STEP + 1));
    }

    #[test]
    fn the_view_and_the_words_name_the_step() {
        assert_eq!(
            serde_json::to_value(SpeedFile::new(2).view(None)).unwrap(),
            serde_json::json!({ "step": 2, "error": null })
        );
        assert_eq!(SpeedFile::words(SAME), "Same");
        assert_eq!(SpeedFile::words(-2), "step -2");
        assert_eq!(
            NAMES.not_saved(),
            "The pointer speed setting was not saved. Try again."
        );
    }
}
