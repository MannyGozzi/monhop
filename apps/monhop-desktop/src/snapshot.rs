use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupSnapshot {
    platform: &'static str,
    version: &'static str,
    permissions: Permissions,
    launch: Launch,
    interfaces: Vec<Interface>,
    displays: Vec<Display>,
    errors: Vec<String>,
    pairing_available: bool,
    /// Where the event log lives, once logging started.
    log_path: Option<String>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Permissions {
    accessibility: Option<bool>,
    input_monitoring: Option<bool>,
    wifi_authorization: Option<&'static str>,
}

#[cfg(target_os = "macos")]
impl SetupSnapshot {
    pub fn with_wifi_authorization(mut self, authorization: &'static str) -> Self {
        self.permissions.wifi_authorization = Some(authorization);
        self
    }
}

#[derive(Serialize)]
struct Launch {
    executable: String,
    bundled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Interface {
    id: String,
    index: u32,
    name: String,
    network_name: Option<String>,
    address: String,
    prefix_length: u8,
    kind: &'static str,
    physical: bool,
    up: bool,
    attachment_known: bool,
}

#[derive(Serialize)]
struct Display {
    name: String,
    width: f64,
    height: f64,
    primary: bool,
}

pub fn read() -> SetupSnapshot {
    let mut result = SetupSnapshot {
        platform: platform(),
        version: env!("CARGO_PKG_VERSION"),
        log_path: crate::logging::path().map(|path| path.to_string_lossy().into_owned()),
        permissions: Permissions::default(),
        launch: Launch {
            executable: String::new(),
            bundled: false,
        },
        interfaces: Vec::new(),
        displays: Vec::new(),
        errors: Vec::new(),
        pairing_available: cfg!(any(target_os = "macos", windows)),
    };
    match std::env::current_exe() {
        Ok(path) => {
            result.launch.bundled = is_app_bundle_executable(&path);
            result.launch.executable = path.to_string_lossy().into_owned();
        }
        Err(_) => result
            .errors
            .push("Could not identify this executable's launch path.".into()),
    }
    read_permissions(&mut result);
    read_interfaces(&mut result);
    read_displays(&mut result);
    result
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "unsupported"
    }
}

fn is_app_bundle_executable(path: &std::path::Path) -> bool {
    let Some(macos) = path.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    let Some(bundle) = contents.parent() else {
        return false;
    };
    macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && bundle
            .extension()
            .is_some_and(|extension| extension == "app")
}

#[cfg(target_os = "macos")]
fn read_permissions(result: &mut SetupSnapshot) {
    match monhop_platform_macos::preflight_permissions() {
        Ok(state) => {
            result.permissions.accessibility = Some(state.accessibility);
            result.permissions.input_monitoring = Some(state.listen_events);
        }
        Err(error) => result.errors.push(error.to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn read_permissions(_: &mut SetupSnapshot) {}

#[cfg(any(target_os = "macos", windows))]
fn read_interfaces(result: &mut SetupSnapshot) {
    #[cfg(target_os = "macos")]
    use monhop_platform_macos::network;
    #[cfg(windows)]
    use monhop_platform_windows::network;

    #[cfg(target_os = "macos")]
    let adapters = network::enumerate_adapters_with_attachment();
    #[cfg(windows)]
    let adapters = network::enumerate_adapters();
    match adapters {
        Ok(adapters) => {
            result.interfaces = adapters
                .into_iter()
                .map(|adapter| {
                    let network_name = adapter.wifi_network_name().and_then(display_network_name);
                    Interface {
                        id: format!(
                            "{}:{}:{}",
                            adapter.stable_id, adapter.index, adapter.address
                        ),
                        index: adapter.index,
                        name: adapter.name,
                        network_name,
                        address: adapter.address.to_string(),
                        prefix_length: adapter.prefix_len,
                        kind: if adapter.ethernet {
                            "Ethernet"
                        } else if adapter.wifi {
                            "Wi-Fi"
                        } else {
                            "Unsupported"
                        },
                        physical: adapter.physical,
                        up: adapter.up,
                        attachment_known: adapter.attachment.is_some(),
                    }
                })
                .collect();
        }
        Err(_) => result.errors.push(
            "Could not read physical network interfaces. Nothing was selected or connected.".into(),
        ),
    }
}

fn display_network_name(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 32
        || name.trim().is_empty()
        || name.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        None
    } else {
        Some(name.to_owned())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_interfaces(result: &mut SetupSnapshot) {
    result
        .errors
        .push("Native setup is unavailable on this platform.".into());
}

#[cfg(target_os = "macos")]
fn read_displays(result: &mut SetupSnapshot) {
    match monhop_platform_macos::enumerate_active_displays() {
        Ok(displays) => {
            result.displays = displays
                .into_iter()
                .map(|display| Display {
                    name: format!("Display {}", display.id.0),
                    width: display.logical_bounds.size.width,
                    height: display.logical_bounds.size.height,
                    primary: display.primary,
                })
                .collect();
        }
        Err(error) => result.errors.push(error.to_string()),
    }
}

#[cfg(windows)]
fn read_displays(result: &mut SetupSnapshot) {
    match monhop_platform_windows::enumerate_displays(monhop_core::DeviceId::default()) {
        Ok(displays) => {
            result.displays = displays
                .into_iter()
                .map(|display| Display {
                    name: display.name,
                    width: display.logical_size.width,
                    height: display.logical_size.height,
                    primary: display.primary,
                })
                .collect();
        }
        Err(_) => result
            .errors
            .push("Could not read connected displays.".into()),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_displays(_: &mut SetupSnapshot) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_names_are_bounded_display_text_not_authorization() {
        for name in ["Home Wi-Fi", "Caf\u{e9}", "\u{7f51}\u{7edc}", " <network> "] {
            assert_eq!(display_network_name(name).as_deref(), Some(name));
        }
        assert_eq!(display_network_name(&"a".repeat(32)), Some("a".repeat(32)));
        for name in [
            "",
            "   ",
            "home\nother",
            "home\0",
            "\u{85}",
            "\u{061c}name",
            "\u{200e}name",
            "\u{2028}name",
            "\u{202e}name",
            "\u{2066}name",
        ] {
            assert!(display_network_name(name).is_none());
        }
        assert!(display_network_name(&"a".repeat(33)).is_none());
        assert!(display_network_name(&"\u{e9}".repeat(17)).is_none());
    }

    #[test]
    fn bundle_path_requires_the_actual_executable_layout() {
        for valid in [
            "/Applications/MonHop.app/Contents/MacOS/monhop-desktop",
            "/tmp/MonHop.app/Contents/MacOS/monhop-desktop",
        ] {
            assert!(is_app_bundle_executable(std::path::Path::new(valid)));
        }
        for invalid in [
            "/tmp/monhop-desktop",
            "/tmp/not.app/Contents/monhop-desktop",
            "/tmp/not.app/Resources/MacOS/monhop-desktop",
            "/tmp/notbundle/Contents/MacOS/monhop-desktop",
        ] {
            assert!(!is_app_bundle_executable(std::path::Path::new(invalid)));
        }
    }
}
