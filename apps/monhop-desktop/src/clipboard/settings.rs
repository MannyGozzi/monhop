//! The persisted clipboard-sharing switch, off unless the user turned it on, and the view the
//! settings screen renders. The view carries kinds and sizes, never clipboard content.

use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::native::{Access, Skip};

pub const FILE_NAME: &str = "clipboard.json";
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClipboardFile {
    version: u32,
    enabled: bool,
}

impl Default for ClipboardFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: false,
        }
    }
}

impl ClipboardFile {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard preference")
}

pub struct ClipboardSetting {
    path: Option<PathBuf>,
    file: ClipboardFile,
}

impl ClipboardSetting {
    /// `path` is `FILE_NAME` in the app's local data directory. Missing or unreadable means off.
    pub fn load(path: Option<PathBuf>) -> Self {
        let file = match path.as_deref().map(ClipboardFile::load) {
            Some(Ok(file)) => file,
            Some(Err(_)) | None => {
                log::warn!("clipboard: preference could not be read, sharing stays off");
                ClipboardFile::default()
            }
        };
        Self { path, file }
    }

    pub fn enabled(&self) -> bool {
        self.file.enabled
    }

    /// Takes effect for this run even when saving fails, so turning it off always stops sharing.
    pub fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.file.enabled = enabled;
        self.path
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|path| self.file.save(path))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Notice {
    TooLarge,
    Unsupported,
    AccessDenied,
}

impl Notice {
    /// Concealed items and file copies are skipped by design and stay silent.
    pub fn for_skip(skip: Skip) -> Option<Self> {
        match skip {
            Skip::TooLarge => Some(Self::TooLarge),
            Skip::Unsupported => Some(Self::Unsupported),
            Skip::Concealed | Skip::Files => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Sent,
    Received,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    Text,
    Image,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub fingerprint: String,
    /// The peer's last reported switch; `None` until it reports.
    pub peer_enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastTransfer {
    pub direction: Direction,
    pub kind: TransferKind,
    pub bytes: u64,
    pub peer: String,
    pub age_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardView {
    pub enabled: bool,
    pub access: Access,
    pub notice: Option<Notice>,
    pub peers: Vec<PeerView>,
    pub last: Option<LastTransfer>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::clipboard::image::ImageError;

    fn temporary_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("monhop-clipboard-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_means_off() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(!ClipboardFile::load(&path).unwrap().enabled);
        assert!(!ClipboardSetting::load(Some(path)).enabled());
        assert!(!ClipboardSetting::load(None).enabled());
    }

    #[test]
    fn a_saved_switch_round_trips_in_the_documented_shape() {
        let path = temporary_path("roundtrip");
        let mut setting = ClipboardSetting::load(Some(path.clone()));
        setting.set_enabled(true).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, json!({ "version": 1, "enabled": true }));
        assert!(ClipboardSetting::load(Some(path.clone())).enabled());
        setting.set_enabled(false).unwrap();
        assert!(!ClipboardSetting::load(Some(path)).enabled());
    }

    #[test]
    fn unreadable_files_mean_off() {
        let padding = "x".repeat(MAX_FILE_BYTES as usize + 1);
        for (name, contents) in [
            ("bad-version", r#"{"version":2,"enabled":true}"#.to_owned()),
            (
                "unknown-field",
                r#"{"version":1,"enabled":true,"extra":1}"#.to_owned(),
            ),
            ("wrong-type", r#"{"version":1,"enabled":"yes"}"#.to_owned()),
            ("not-json", "enabled".to_owned()),
            (
                "oversized",
                format!(r#"{{"version":1,"enabled":true,"p":"{padding}"}}"#),
            ),
        ] {
            let path = temporary_path(name);
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                ClipboardFile::load(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{name}"
            );
            assert!(!ClipboardSetting::load(Some(path)).enabled(), "{name}");
        }
    }

    #[test]
    fn a_failed_save_still_changes_the_switch_for_this_run() {
        let mut setting = ClipboardSetting::load(None);
        assert!(setting.set_enabled(true).is_err());
        assert!(setting.enabled());
        assert!(setting.set_enabled(false).is_err());
        assert!(!setting.enabled());
    }

    #[test]
    fn the_view_serializes_to_the_ui_shape() {
        let view = ClipboardView {
            enabled: true,
            access: Access::Ask,
            notice: Some(Notice::AccessDenied),
            peers: vec![
                PeerView {
                    fingerprint: "ab12".into(),
                    peer_enabled: Some(true),
                },
                PeerView {
                    fingerprint: "cd34".into(),
                    peer_enabled: None,
                },
            ],
            last: Some(LastTransfer {
                direction: Direction::Received,
                kind: TransferKind::Image,
                bytes: 2048,
                peer: "ab12".into(),
                age_seconds: 5,
            }),
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            json!({
                "enabled": true,
                "access": "ask",
                "notice": "accessDenied",
                "peers": [
                    { "fingerprint": "ab12", "peerEnabled": true },
                    { "fingerprint": "cd34", "peerEnabled": null },
                ],
                "last": {
                    "direction": "received",
                    "kind": "image",
                    "bytes": 2048,
                    "peer": "ab12",
                    "ageSeconds": 5,
                },
            })
        );
        let idle = ClipboardView {
            enabled: false,
            access: Access::Unknown,
            notice: None,
            peers: Vec::new(),
            last: None,
        };
        assert_eq!(
            serde_json::to_value(&idle).unwrap(),
            json!({ "enabled": false, "access": "unknown", "notice": null, "peers": [], "last": null })
        );
    }

    #[test]
    fn every_enum_uses_its_documented_spelling() {
        let spelled = |value: serde_json::Value| value.as_str().unwrap().to_owned();
        assert_eq!(
            [
                Access::Allowed,
                Access::Ask,
                Access::Denied,
                Access::Unknown
            ]
            .map(|access| spelled(serde_json::to_value(access).unwrap())),
            ["allowed", "ask", "denied", "unknown"]
        );
        assert_eq!(
            [Notice::TooLarge, Notice::Unsupported, Notice::AccessDenied]
                .map(|notice| spelled(serde_json::to_value(notice).unwrap())),
            ["tooLarge", "unsupported", "accessDenied"]
        );
        assert_eq!(
            [Direction::Sent, Direction::Received]
                .map(|direction| spelled(serde_json::to_value(direction).unwrap())),
            ["sent", "received"]
        );
        assert_eq!(
            [TransferKind::Text, TransferKind::Image]
                .map(|kind| spelled(serde_json::to_value(kind).unwrap())),
            ["text", "image"]
        );
    }

    #[test]
    fn only_actionable_skips_become_notices() {
        assert_eq!(Notice::for_skip(Skip::TooLarge), Some(Notice::TooLarge));
        assert_eq!(
            Notice::for_skip(Skip::Unsupported),
            Some(Notice::Unsupported)
        );
        assert_eq!(Notice::for_skip(Skip::Concealed), None);
        assert_eq!(Notice::for_skip(Skip::Files), None);
        assert_eq!(
            Notice::for_skip(ImageError::Malformed.skip()),
            Some(Notice::Unsupported)
        );
    }
}
