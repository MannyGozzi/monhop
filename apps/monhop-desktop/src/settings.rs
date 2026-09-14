use serde::Deserialize;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPane {
    Accessibility,
    InputMonitoring,
    LocationServices,
    LocalNetwork,
}

impl PermissionPane {
    fn url(self) -> &'static str {
        match self {
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            Self::LocalNetwork => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork"
            }
            Self::LocationServices => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_LocationServices"
            }
        }
    }
}

pub fn open(pane: PermissionPane) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWorkspace;
        use objc2_foundation::{NSString, NSURL};
        let url = NSURL::URLWithString(&NSString::from_str(pane.url()))
            .ok_or("Could not construct the macOS Settings link.")?;
        if NSWorkspace::sharedWorkspace().openURL(&url) {
            Ok(())
        } else {
            Err("Open System Settings > Privacy & Security manually, then choose the permission and check again in MonHop.".into())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pane.url();
        Err(
            "These privacy settings are macOS-only. MonHop never requests Windows elevation."
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_four_fixed_local_settings_destinations_exist() {
        assert_eq!(
            PermissionPane::Accessibility.url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        );
        assert_eq!(
            PermissionPane::InputMonitoring.url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        );
        assert!(serde_json::from_str::<PermissionPane>("\"accessibility\"").is_ok());
        assert!(serde_json::from_str::<PermissionPane>("\"input-monitoring\"").is_ok());
        assert!(serde_json::from_str::<PermissionPane>("\"location-services\"").is_ok());
        assert_eq!(
            PermissionPane::LocationServices.url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_LocationServices"
        );
        assert_eq!(
            PermissionPane::LocalNetwork.url(),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork"
        );
        assert!(serde_json::from_str::<PermissionPane>("\"local-network\"").is_ok());
        for pane in [
            "\"https://example.com\"",
            "\"input-monitoring?pane=privacy\"",
            "\"Accessibility\"",
        ] {
            assert!(serde_json::from_str::<PermissionPane>(pane).is_err());
        }
    }
}
