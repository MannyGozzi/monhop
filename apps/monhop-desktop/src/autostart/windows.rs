//! Login registration through the user's Run key. No `reg.exe`, `schtasks` or other shell
//! execution; every registry access goes through `windows::Win32::System::Registry` directly.

use std::path::{Path, PathBuf};

// Read only by the Windows registration backend below; the build/parse helpers stay
// cross-platform so their round trip is unit tested on every host.
#[cfg_attr(not(windows), allow(dead_code))]
const VALUE_NAME: &str = "MonHop";
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg_attr(not(windows), allow(dead_code))]
const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// The exact value MonHop writes to the Run key: the quoted executable plus `--login`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn build_command_line(executable: &Path) -> String {
    format!("\"{}\" --login", executable.display())
}

/// The quoted leading path from a Run-key value, when the value has exactly that shape.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_command_line(value: &str) -> Option<PathBuf> {
    let rest = value.strip_prefix('"')?;
    let (path, tail) = rest.split_once('"')?;
    if path.is_empty() || tail.trim() != "--login" {
        return None;
    }
    Some(PathBuf::from(path))
}

#[cfg(windows)]
mod registry {
    use windows::{
        Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
        Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SAM_FLAGS, REG_SZ, RegCloseKey,
            RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
        core::HSTRING,
    };

    use super::{
        RUN_KEY, STARTUP_APPROVED_KEY, VALUE_NAME, build_command_line, parse_command_line,
    };
    use crate::autostart::{Registration, Status};

    /// One open registry handle, closed exactly once when it goes out of scope.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the one handle this type owns; closed exactly once.
            let _ = unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(subkey: &str, access: REG_SAM_FLAGS) -> Result<Key, WIN32_ERROR> {
        let mut handle = HKEY::default();
        // SAFETY: `subkey` is one of the two fixed, always-present per-user paths below;
        // `handle` receives exactly one owned result on success.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(subkey),
                None,
                access,
                &mut handle,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(Key(handle))
        } else {
            Err(status)
        }
    }

    fn open_failed(status: WIN32_ERROR) -> String {
        format!(
            "The Windows registry could not be opened (error {}).",
            status.0
        )
    }

    /// Reads a value's raw bytes regardless of its declared type; `None` when the value is absent.
    fn query_bytes(key: &Key, name: &str) -> Result<Option<Vec<u8>>, String> {
        let value_name = HSTRING::from(name);
        let mut size: u32 = 0;
        // SAFETY: this probe call passes no buffer pointer, only asks for the required size.
        let probe = unsafe {
            RegQueryValueExW(
                key.0,
                &value_name,
                None,
                None,
                None,
                Some(&mut size as *mut u32),
            )
        };
        if probe == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if probe != ERROR_SUCCESS {
            return Err(format!(
                "The startup entry could not be read (error {}).",
                probe.0
            ));
        }
        if size == 0 {
            return Ok(Some(Vec::new()));
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: `buffer` is sized to exactly what the probe call above reported.
        let filled = unsafe {
            RegQueryValueExW(
                key.0,
                &value_name,
                None,
                None,
                Some(buffer.as_mut_ptr()),
                Some(&mut size as *mut u32),
            )
        };
        if filled != ERROR_SUCCESS {
            return Err(format!(
                "The startup entry could not be read (error {}).",
                filled.0
            ));
        }
        buffer.truncate(size as usize);
        Ok(Some(buffer))
    }

    fn decode_utf16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_owned()
    }

    fn read_run_value() -> Result<Option<String>, String> {
        let key = match open(RUN_KEY, KEY_READ) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND) => return Ok(None),
            Err(status) => return Err(open_failed(status)),
        };
        Ok(query_bytes(&key, VALUE_NAME)?.map(|bytes| decode_utf16(&bytes)))
    }

    /// True when Settings > Apps > Startup or Task Manager turned this entry off; the stored
    /// first byte of the approval record is odd when the user disabled it.
    fn disabled_by_system() -> Result<bool, String> {
        let key = match open(STARTUP_APPROVED_KEY, KEY_READ) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND) => return Ok(false),
            Err(status) => return Err(open_failed(status)),
        };
        let first_byte = query_bytes(&key, VALUE_NAME)?.and_then(|bytes| bytes.first().copied());
        Ok(first_byte.is_some_and(|byte| byte % 2 == 1))
    }

    fn current_exe() -> Result<std::path::PathBuf, String> {
        std::env::current_exe()
            .map_err(|_| "MonHop's own executable location could not be read.".to_owned())
    }

    pub(super) fn status() -> Result<Status, String> {
        let Some(raw) = read_run_value()? else {
            return Ok(Status::NotRegistered);
        };
        let Some(stored_exe) = parse_command_line(&raw) else {
            return Ok(Status::NotFound);
        };
        if stored_exe != current_exe()? {
            return Ok(Status::NotFound);
        }
        if disabled_by_system()? {
            return Ok(Status::DisabledBySystem);
        }
        Ok(Status::Enabled)
    }

    pub(super) fn register() -> Result<(), String> {
        let command_line = build_command_line(&current_exe()?);
        let key = open(RUN_KEY, KEY_WRITE).map_err(open_failed)?;
        let value_name = HSTRING::from(VALUE_NAME);
        let data: Vec<u16> = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `data` is a UTF-16, nul-terminated buffer matching `REG_SZ`, native-endian on
        // every real Windows target; `key` owns the handle for the call's duration.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                data.as_ptr().cast::<u8>(),
                std::mem::size_of_val(data.as_slice()),
            )
        };
        // SAFETY: `key` is an open handle, `value_name` is nul-terminated and `bytes` is the
        // full REG_SZ payload; all outlive the call.
        let status = unsafe { RegSetValueExW(key.0, &value_name, None, REG_SZ, Some(bytes)) };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "The startup entry could not be written (error {}).",
                status.0
            ))
        }
    }

    pub(super) fn unregister() -> Result<(), String> {
        let key = open(RUN_KEY, KEY_WRITE).map_err(open_failed)?;
        let value_name = HSTRING::from(VALUE_NAME);
        // SAFETY: deletes only the one named value this app owns.
        let status = unsafe { RegDeleteValueW(key.0, &value_name) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!(
                "The startup entry could not be removed (error {}).",
                status.0
            ))
        }
    }

    pub struct WindowsRegistration;

    impl Registration for WindowsRegistration {
        fn status(&self) -> Result<Status, String> {
            status()
        }

        fn register(&self) -> Result<(), String> {
            register()
        }

        fn unregister(&self) -> Result<(), String> {
            unregister()
        }
    }

    /// Opens Settings > Apps > Startup, the same fixed-target `ShellExecuteW` pattern as `links.rs`.
    pub fn open_settings() -> Result<(), String> {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let target = HSTRING::from("ms-settings:startupapps");
        // SAFETY: a fixed OS settings URI; no window handle is retained afterward.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                &target,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err("Settings did not open. Open Settings > Apps > Startup yourself.".to_owned())
        }
    }
}

#[cfg(windows)]
pub use registry::{WindowsRegistration, open_settings};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_round_trips_through_build_and_parse() {
        let exe = Path::new(r"C:\Program Files\MonHop\MonHop.exe");
        let line = build_command_line(exe);
        assert_eq!(line, "\"C:\\Program Files\\MonHop\\MonHop.exe\" --login");
        assert_eq!(parse_command_line(&line).as_deref(), Some(exe));
    }

    #[test]
    fn malformed_values_are_not_read_as_a_command_line() {
        for value in [
            "",
            "no quotes --login",
            "\"C:\\MonHop.exe\"",
            "\"C:\\MonHop.exe\" --other",
            "\"\" --login",
            "\"C:\\MonHop.exe\" --login extra",
        ] {
            assert_eq!(parse_command_line(value), None, "accepted {value:?}");
        }
    }
}
