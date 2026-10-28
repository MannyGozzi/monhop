//! Whether this Mac's AirDrop accepts from everyone. That keeps Apple's peer-to-peer Wi-Fi hopping
//! channels, pausing the Mac's own Wi-Fi link for tens of milliseconds a few times a second, so the
//! UI warns while sharing runs over Wi-Fi. Read-only: MonHop never changes the setting.

use serde::Serialize;

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct AirDropView {
    everyone: bool,
}

#[tauri::command]
pub async fn airdrop_status() -> Result<AirDropView, String> {
    tauri::async_runtime::spawn_blocking(|| AirDropView {
        everyone: discoverable_by_everyone(),
    })
    .await
    .map_err(|_| "MonHop could not read the AirDrop setting.".to_owned())
}

#[cfg(target_os = "macos")]
fn discoverable_by_everyone() -> bool {
    use objc2_core_foundation::{CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, CFString};

    let domain = CFString::from_static_str("com.apple.sharingd");
    // Control Center writes it from another process; drop any copy this one cached.
    CFPreferencesAppSynchronize(&domain);
    CFPreferencesCopyAppValue(&CFString::from_static_str("DiscoverableMode"), &domain)
        .and_then(|value| value.downcast::<CFString>().ok())
        .is_some_and(|mode| is_everyone(&mode.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn discoverable_by_everyone() -> bool {
    false
}

/// macOS stores "Everyone", "Contacts Only" or "Off"; a timed Everyone keeps the same prefix.
#[cfg(any(target_os = "macos", test))]
fn is_everyone(mode: &str) -> bool {
    mode.starts_with("Everyone")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_everyone_counts_as_open_to_everyone() {
        assert!(is_everyone("Everyone"));
        assert!(!is_everyone("Contacts Only"));
        assert!(!is_everyone("Off"));
        assert!(!is_everyone(""));
    }

    /// Native: run with `--ignored` on a Mac; agrees with what `defaults` reads.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads this Mac's AirDrop setting"]
    fn agrees_with_the_setting_defaults_reads() {
        let output = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "com.apple.sharingd", "DiscoverableMode"])
            .output()
            .unwrap();
        let mode = String::from_utf8_lossy(&output.stdout);
        assert_eq!(discoverable_by_everyone(), is_everyone(mode.trim()));
    }

    #[test]
    fn the_view_names_one_field() {
        assert_eq!(
            serde_json::to_string(&AirDropView { everyone: true }).unwrap(),
            r#"{"everyone":true}"#
        );
    }
}
