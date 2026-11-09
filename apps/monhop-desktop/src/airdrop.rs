//! Whether this Mac's AirDrop accepts from everyone and whether Universal Control is on. Either can
//! keep Apple's peer-to-peer Wi-Fi hopping channels, pausing the Mac's own Wi-Fi link for tens of
//! milliseconds a few times a second, so the UI warns while sharing runs over Wi-Fi. Read-only:
//! MonHop never changes either setting.

use serde::Serialize;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AirDropView {
    everyone: bool,
    universal_control: bool,
}

#[tauri::command]
pub async fn airdrop_status() -> Result<AirDropView, String> {
    tauri::async_runtime::spawn_blocking(|| AirDropView {
        everyone: discoverable_by_everyone(),
        universal_control: universal_control_on(),
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

#[cfg(target_os = "macos")]
fn universal_control_on() -> bool {
    use objc2_core_foundation::{
        CFPreferencesCopyValue, CFPreferencesSynchronize, CFString, kCFPreferencesCurrentHost,
        kCFPreferencesCurrentUser,
    };

    let domain = CFString::from_static_str("com.apple.universalcontrol");
    // SAFETY: Core Foundation exports both as constants that live as long as the process.
    let (user, host) = unsafe { (kCFPreferencesCurrentUser, kCFPreferencesCurrentHost) };
    // System Settings writes it from another process; drop any copy this one cached.
    CFPreferencesSynchronize(&domain, user, host);
    let key = CFString::from_static_str("Disable");
    let disabled =
        CFPreferencesCopyValue(&key, &domain, user, host).is_some_and(|value| flag_is_set(&value));
    !disabled
}

/// Preference writers store a flag as either a boolean or a number.
#[cfg(target_os = "macos")]
fn flag_is_set(value: &objc2_core_foundation::CFType) -> bool {
    use objc2_core_foundation::{CFBoolean, CFNumber};

    if let Some(flag) = value.downcast_ref::<CFBoolean>() {
        return flag.as_bool();
    }
    value
        .downcast_ref::<CFNumber>()
        .and_then(CFNumber::as_i64)
        .is_some_and(|number| number != 0)
}

#[cfg(not(target_os = "macos"))]
fn universal_control_on() -> bool {
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

    /// Native: run with `--ignored` on a Mac; agrees with what `defaults` reads for this host.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads this Mac's Universal Control setting"]
    fn universal_control_agrees_with_the_setting_defaults_reads() {
        let output = std::process::Command::new("/usr/bin/defaults")
            .args([
                "-currentHost",
                "read",
                "com.apple.universalcontrol",
                "Disable",
            ])
            .output()
            .unwrap();
        // `defaults` prints 1 for true and fails when the key is absent, which means on.
        let disabled = String::from_utf8_lossy(&output.stdout).trim() == "1";
        assert_eq!(universal_control_on(), !disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_flag_counts_as_set_whether_stored_as_a_boolean_or_a_number() {
        use objc2_core_foundation::{CFBoolean, CFNumber, CFString};

        assert!(flag_is_set(CFBoolean::new(true)));
        assert!(!flag_is_set(CFBoolean::new(false)));
        assert!(flag_is_set(&CFNumber::new_i32(1)));
        assert!(!flag_is_set(&CFNumber::new_i32(0)));
        assert!(!flag_is_set(&CFString::from_static_str("1")));
    }

    #[test]
    fn the_view_names_both_fields() {
        let view = AirDropView {
            everyone: true,
            universal_control: false,
        };
        assert_eq!(
            serde_json::to_string(&view).unwrap(),
            r#"{"everyone":true,"universalControl":false}"#
        );
    }
}
