use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

pub async fn reveal(path: PathBuf) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        notepad_command(&path)?
            .spawn()
            .map_err(|error| format!("Could not open {} in Notepad: {error}", path.display()))?;
        Ok(())
    })
    .await
    .map_err(|_| "Opening the log in Notepad did not finish.".to_owned())?
}

fn notepad_command(path: &Path) -> Result<Command, String> {
    if !path.is_absolute() {
        return Err("The log file location is invalid.".to_owned());
    }
    let metadata = path
        .metadata()
        .map_err(|error| format!("Could not access {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "The log location is not a file: {}",
            path.display()
        ));
    }
    // App-data redirection can differ between MonHop and Notepad. Pass the physical file location.
    let physical_path = path
        .canonicalize()
        .map_err(|error| format!("Could not resolve {}: {error}", path.display()))?;
    let mut directory = [0u16; 32768];
    // SAFETY: Windows receives a writable buffer and its length through the binding.
    let length = unsafe { GetSystemDirectoryW(Some(&mut directory)) } as usize;
    if length == 0 || length >= directory.len() {
        return Err("Windows could not locate its system folder for Notepad.".to_owned());
    }
    let executable = PathBuf::from(OsString::from_wide(&directory[..length])).join("notepad.exe");
    // Use the Windows-owned executable, never PATH lookup, a file association, or a command shell.
    let mut command = Command::new(executable);
    command.arg(physical_path);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestLog(PathBuf);

    impl TestLog {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "MonHop log reveal {} {} é 日本 🗂 &",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).expect("create test directory");
            let path = dir.join("monhop.log");
            std::fs::write(&path, "test log\n").expect("write test log");
            Self(path)
        }
    }

    impl Drop for TestLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(self.0.parent().unwrap());
        }
    }

    #[test]
    fn physical_log_path_is_one_literal_argument_to_windows_notepad() {
        let log = TestLog::new();
        let command = notepad_command(&log.0).expect("prepare log opener");
        let executable = Path::new(command.get_program());
        assert!(executable.is_absolute());
        assert!(executable.is_file());
        assert_eq!(executable.file_name().unwrap(), "notepad.exe");
        let physical_path = log.0.canonicalize().unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [physical_path.as_os_str()]
        );
        assert_eq!(std::fs::read(physical_path).unwrap(), b"test log\n");
    }

    #[test]
    fn path_alias_is_resolved_before_cross_process_launch() {
        let log = TestLog::new();
        let nested = log.0.parent().unwrap().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("monhop.log");
        let command = notepad_command(&alias).unwrap();
        let physical_path = log.0.canonicalize().unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [physical_path.as_os_str()]
        );
        assert_ne!(physical_path, alias);
        std::fs::remove_dir(nested).unwrap();
    }

    #[test]
    fn missing_log_and_directory_return_errors_without_launching() {
        let log = TestLog::new();
        assert!(notepad_command(&log.0.with_file_name("missing.log")).is_err());
        assert!(notepad_command(log.0.parent().unwrap()).is_err());
        assert!(notepad_command(Path::new("monhop.log")).is_err());
    }

    #[tokio::test]
    #[ignore = "opens Notepad; set MONHOP_LOG_REVEAL_TEST_PATH to an existing log"]
    async fn open_existing_log_in_notepad() {
        let path = PathBuf::from(
            std::env::var_os("MONHOP_LOG_REVEAL_TEST_PATH").expect("set the log path to open"),
        );
        reveal(path.clone()).await.expect("launch Notepad with log");
        println!("Launched Notepad for {}", path.display());
    }
}
