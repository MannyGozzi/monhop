//! Login registration: an explicit wish, applied to the OS only while a computer is paired. No
//! registration or OS status read happens merely from starting the app.

#[cfg(target_os = "macos")]
mod macos;
mod windows;

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::sharing_preferences::SetupFile;

const FILE_NAME: &str = "autostart.json";
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 512;
/// The bundled login agent; must match the plist's Label and its LaunchAgents file name.
#[cfg(target_os = "macos")]
pub(crate) const PLIST_NAME: &str = "com.manuelgozzi.monhop.login.plist";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutostartFile {
    version: u32,
    enabled: bool,
}

impl Default for AutostartFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: true,
        }
    }
}

impl AutostartFile {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid autostart preference")
}

/// The OS registration outcomes `sync` reconciles against; `NotRegistered` is the only status a
/// registration call runs from, and any other status is the only one an unregistration runs from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NotRegistered,
    Enabled,
    // Constructed only by the macOS registration backend.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    RequiresApproval,
    // Constructed only by the Windows registration backend.
    #[cfg_attr(not(windows), allow(dead_code))]
    DisabledBySystem,
    NotFound,
}

/// The native login-item backend. Tests use a fake; production uses the platform module.
pub trait Registration: Send + Sync {
    fn status(&self) -> Result<Status, String>;
    fn register(&self) -> Result<(), String>;
    fn unregister(&self) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    On,
    Off,
    Pending,
    RequiresApproval,
    DisabledBySystem,
    NotFound,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutostartView {
    enabled: bool,
    state: State,
    paired: bool,
    message: String,
}

pub struct Autostart {
    path: Option<PathBuf>,
    state: Mutex<AutostartFile>,
    registration: Box<dyn Registration>,
}

impl Autostart {
    /// Loads the wish; an unreadable file leaves autostart on, same policy as every other
    /// preference file here. Never touches the OS registration.
    fn load(path: Option<PathBuf>, registration: Box<dyn Registration>) -> Self {
        let file = match path.as_deref().map(AutostartFile::load) {
            Some(Ok(file)) => file,
            Some(Err(_)) | None => {
                log::warn!("autostart: preference could not be read, keeping autostart on");
                AutostartFile::default()
            }
        };
        Self {
            path,
            state: Mutex::new(file),
            registration,
        }
    }

    fn wish(&self) -> bool {
        lock(&self.state).enabled
    }

    fn paired(setup_path: Option<&Path>) -> bool {
        setup_path
            .and_then(|path| SetupFile::load(path).ok())
            .is_some_and(|file| file.active().is_some())
    }

    fn pending_view(enabled: bool) -> AutostartView {
        AutostartView {
            enabled,
            state: if enabled { State::Pending } else { State::Off },
            paired: false,
            message: String::new(),
        }
    }

    fn view_of(enabled: bool, paired: bool, status: Status) -> AutostartView {
        let state = match status {
            Status::NotRegistered => State::Off,
            Status::Enabled => State::On,
            Status::RequiresApproval => State::RequiresApproval,
            Status::DisabledBySystem => State::DisabledBySystem,
            Status::NotFound => State::NotFound,
        };
        AutostartView {
            enabled,
            state,
            paired,
            message: String::new(),
        }
    }

    fn failed_view(enabled: bool, paired: bool, message: String) -> AutostartView {
        AutostartView {
            enabled,
            state: State::Failed,
            paired,
            message,
        }
    }

    /// Read-only: reports the OS status when paired, without registering or unregistering.
    pub fn view(&self, setup_path: Option<&Path>) -> AutostartView {
        let enabled = self.wish();
        if !Self::paired(setup_path) {
            return Self::pending_view(enabled);
        }
        match self.registration.status() {
            Ok(status) => Self::view_of(enabled, true, status),
            Err(message) => Self::failed_view(enabled, true, message),
        }
    }

    /// Makes the OS match `enabled && paired`, touching it only when the status differs; a
    /// registration failure is reported as `failed` and never propagated to the caller.
    fn sync(&self, setup_path: Option<&Path>) -> AutostartView {
        let enabled = self.wish();
        let paired = Self::paired(setup_path);
        let desired = enabled && paired;
        let outcome = self.registration.status().and_then(|current| {
            if desired && current == Status::NotRegistered {
                self.registration.register()?;
                self.registration.status()
            } else if !desired && current != Status::NotRegistered {
                self.registration.unregister()?;
                self.registration.status()
            } else {
                Ok(current)
            }
        });
        match (outcome, paired) {
            (Ok(status), true) => Self::view_of(enabled, true, status),
            (Ok(_), false) => Self::pending_view(enabled),
            (Err(message), _) => Self::failed_view(enabled, paired, message),
        }
    }

    /// Saves the wish and, when a computer is already paired, applies it to the OS immediately.
    pub fn set_enabled(&self, enabled: bool, setup_path: Option<&Path>) -> AutostartView {
        {
            let mut state = lock(&self.state);
            state.enabled = enabled;
            if let Some(path) = self.path.as_deref() {
                let _ = state.save(path);
            }
        }
        log::info!("user: autostart {}", if enabled { "on" } else { "off" });
        self.sync(setup_path)
    }
}

fn lock(state: &Mutex<AutostartFile>) -> MutexGuard<'_, AutostartFile> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The apply hooks run inside the sharing controller without an `AppHandle`, so the loaded
/// preference and the platform backend live here instead of in Tauri-managed state.
static GLOBAL: OnceLock<Autostart> = OnceLock::new();

fn platform_registration() -> Box<dyn Registration> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacRegistration)
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsRegistration)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        struct Unsupported;
        impl Registration for Unsupported {
            fn status(&self) -> Result<Status, String> {
                Ok(Status::NotRegistered)
            }
            fn register(&self) -> Result<(), String> {
                Err("Starting at login is not supported on this platform.".to_owned())
            }
            fn unregister(&self) -> Result<(), String> {
                Ok(())
            }
        }
        Box::new(Unsupported)
    }
}

/// Loads the preference and installs the platform registration backend. Performs no OS status
/// read or registration; the first OS call happens from `autostart_status` (page entry) or a
/// change (`autostart_set`, an applied layout, or forgetting the last paired computer).
pub fn init(app: &tauri::AppHandle) {
    let path = app
        .path()
        .app_local_data_dir()
        .ok()
        .map(|dir| dir.join(FILE_NAME));
    let _ = GLOBAL.set(Autostart::load(path, platform_registration()));
}

fn setup_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    crate::sharing_setup_path(app).ok()
}

#[tauri::command]
pub fn autostart_status(app: tauri::AppHandle) -> AutostartView {
    GLOBAL
        .get()
        .expect("autostart::init was not called during setup")
        .view(setup_path(&app).as_deref())
}

#[tauri::command]
pub fn autostart_set(app: tauri::AppHandle, enabled: bool) -> AutostartView {
    GLOBAL
        .get()
        .expect("autostart::init was not called during setup")
        .set_enabled(enabled, setup_path(&app).as_deref())
}

#[tauri::command]
pub fn autostart_open_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::open_settings()
    }
    #[cfg(windows)]
    {
        windows::open_settings()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err("Starting at login is not supported on this platform.".to_owned())
    }
}

/// Called after an applied layout is written to disk, from both the sending and the receiving
/// side. A registration error never fails the apply that triggered this: log and continue.
pub fn setup_applied(setup_path: &Path) {
    if let Some(autostart) = GLOBAL.get() {
        autostart.sync(Some(setup_path));
    }
}

/// Called after forgetting a computer, so a registration made only for that pairing goes away.
pub fn computer_forgotten(setup_path: &Path) {
    if let Some(autostart) = GLOBAL.get() {
        autostart.sync(Some(setup_path));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use monhop_transport::crypto::CertificateFingerprint;

    use super::*;

    struct Fake {
        status: StdMutex<Result<Status, String>>,
        status_calls: AtomicUsize,
        register_calls: AtomicUsize,
        unregister_calls: AtomicUsize,
        register_result: StdMutex<Result<(), String>>,
        unregister_result: StdMutex<Result<(), String>>,
    }

    impl Fake {
        fn new(status: Status) -> Arc<Self> {
            Arc::new(Self {
                status: StdMutex::new(Ok(status)),
                status_calls: AtomicUsize::new(0),
                register_calls: AtomicUsize::new(0),
                unregister_calls: AtomicUsize::new(0),
                register_result: StdMutex::new(Ok(())),
                unregister_result: StdMutex::new(Ok(())),
            })
        }

        fn set_status(&self, status: Result<Status, String>) {
            *self.status.lock().unwrap() = status;
        }
    }

    /// The `Registration` the machine sees; the test keeps its own `Arc<Fake>` for assertions.
    struct FakeRegistration(Arc<Fake>);

    impl Registration for FakeRegistration {
        fn status(&self) -> Result<Status, String> {
            self.0.status_calls.fetch_add(1, Ordering::Relaxed);
            self.0.status.lock().unwrap().clone()
        }

        fn register(&self) -> Result<(), String> {
            self.0.register_calls.fetch_add(1, Ordering::Relaxed);
            let result = self.0.register_result.lock().unwrap().clone();
            if result.is_ok() {
                *self.0.status.lock().unwrap() = Ok(Status::Enabled);
            }
            result
        }

        fn unregister(&self) -> Result<(), String> {
            self.0.unregister_calls.fetch_add(1, Ordering::Relaxed);
            let result = self.0.unregister_result.lock().unwrap().clone();
            if result.is_ok() {
                *self.0.status.lock().unwrap() = Ok(Status::NotRegistered);
            }
            result
        }
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "monhop-autostart-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn autostart(enabled: bool, fake: &Arc<Fake>) -> Autostart {
        Autostart {
            path: None,
            state: Mutex::new(AutostartFile {
                version: FILE_VERSION,
                enabled,
            }),
            registration: Box::new(FakeRegistration(fake.clone())),
        }
    }

    fn paired_setup_path(dir: &Path) -> PathBuf {
        let path = dir.join("sharing-setup.json");
        let mut file = SetupFile::default();
        file.set_active(Some(
            &CertificateFingerprint::parse_full(&"A".repeat(64)).unwrap(),
        ));
        file.save(&path).unwrap();
        path
    }

    #[test]
    fn a_missing_file_keeps_autostart_on() {
        let path = temp_dir().join("missing").join(FILE_NAME);
        assert!(AutostartFile::load(&path).unwrap().enabled);
    }

    #[test]
    fn a_saved_preference_round_trips() {
        let path = temp_dir().join(FILE_NAME);
        let file = AutostartFile {
            version: FILE_VERSION,
            enabled: false,
        };
        file.save(&path).unwrap();
        assert_eq!(AutostartFile::load(&path).unwrap(), file);
    }

    #[test]
    fn an_unreadable_file_falls_back_to_autostart_on() {
        let path = temp_dir().join(FILE_NAME);
        std::fs::write(&path, br#"{"version":9,"enabled":false}"#).unwrap();
        assert!(AutostartFile::load(&path).is_err());
        std::fs::write(&path, b"not json").unwrap();
        assert!(AutostartFile::load(&path).is_err());
        assert!(AutostartFile::default().enabled);
    }

    #[test]
    fn the_view_carries_exactly_the_keys_the_window_reads() {
        let value = serde_json::to_value(Autostart::pending_view(true)).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["enabled", "message", "paired", "state"]);
    }

    #[test]
    fn every_state_serializes_to_the_string_the_window_expects() {
        for (state, text) in [
            (State::On, "\"on\""),
            (State::Off, "\"off\""),
            (State::Pending, "\"pending\""),
            (State::RequiresApproval, "\"requiresApproval\""),
            (State::DisabledBySystem, "\"disabledBySystem\""),
            (State::NotFound, "\"notFound\""),
            (State::Failed, "\"failed\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), text);
        }
    }

    #[test]
    fn constructing_the_state_performs_no_registration_call() {
        let fake = Fake::new(Status::NotRegistered);
        let _ = autostart(true, &fake);
        assert_eq!(fake.status_calls.load(Ordering::Relaxed), 0);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn unpaired_never_registers_and_removes_a_registration_left_behind() {
        let fake = Fake::new(Status::NotRegistered);
        let instance = autostart(true, &fake);
        let view = instance.sync(None);
        assert_eq!(view.state, State::Pending);
        assert!(!view.paired);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 0);
        let instance = autostart(false, &fake);
        let view = instance.sync(None);
        assert_eq!(view.state, State::Off);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 0);
        assert_eq!(fake.unregister_calls.load(Ordering::Relaxed), 0);

        // Forgetting the last computer leaves the wish on but must take the registration away.
        let fake = Fake::new(Status::Enabled);
        let instance = autostart(true, &fake);
        let view = instance.sync(None);
        assert_eq!(view.state, State::Pending);
        assert_eq!(fake.unregister_calls.load(Ordering::Relaxed), 1);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn paired_and_enabled_registers_only_when_not_already_registered() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        let fake = Fake::new(Status::NotRegistered);
        let instance = autostart(true, &fake);
        let view = instance.sync(Some(&setup_path));
        assert_eq!(view.state, State::On);
        assert!(view.paired);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 1);

        // Already enabled: no further OS call.
        let view = instance.sync(Some(&setup_path));
        assert_eq!(view.state, State::On);
        assert_eq!(fake.register_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn paired_and_disabled_unregisters_only_when_currently_registered() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        let fake = Fake::new(Status::Enabled);
        let instance = autostart(false, &fake);
        let view = instance.sync(Some(&setup_path));
        assert_eq!(view.state, State::Off);
        assert_eq!(fake.unregister_calls.load(Ordering::Relaxed), 1);

        // Already not registered: no further OS call.
        let view = instance.sync(Some(&setup_path));
        assert_eq!(view.state, State::Off);
        assert_eq!(fake.unregister_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn statuses_that_are_already_registered_are_left_alone_when_still_wanted() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        for status in [
            Status::RequiresApproval,
            Status::DisabledBySystem,
            Status::NotFound,
        ] {
            let fake = Fake::new(status);
            let instance = autostart(true, &fake);
            let view = instance.sync(Some(&setup_path));
            assert_eq!(fake.register_calls.load(Ordering::Relaxed), 0);
            assert_eq!(fake.unregister_calls.load(Ordering::Relaxed), 0);
            match status {
                Status::RequiresApproval => assert_eq!(view.state, State::RequiresApproval),
                Status::DisabledBySystem => assert_eq!(view.state, State::DisabledBySystem),
                Status::NotFound => assert_eq!(view.state, State::NotFound),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn a_status_read_failure_is_reported_as_failed_with_its_message() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        let fake = Fake::new(Status::NotRegistered);
        fake.set_status(Err("The login item service is unavailable.".to_owned()));
        let instance = autostart(true, &fake);
        let view = instance.view(Some(&setup_path));
        assert_eq!(view.state, State::Failed);
        assert_eq!(view.message, "The login item service is unavailable.");
    }

    #[test]
    fn a_failed_registration_is_reported_as_failed_and_never_panics() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        let fake = Fake::new(Status::NotRegistered);
        *fake.register_result.lock().unwrap() = Err("The system refused the request.".to_owned());
        let instance = autostart(true, &fake);
        let view = instance.sync(Some(&setup_path));
        assert_eq!(view.state, State::Failed);
        assert_eq!(view.message, "The system refused the request.");
    }

    #[test]
    fn set_enabled_saves_the_wish_and_syncs_when_paired() {
        let dir = temp_dir();
        let setup_path = paired_setup_path(&dir);
        let preference_path = dir.join(FILE_NAME);
        let fake = Fake::new(Status::NotRegistered);
        let instance = Autostart {
            path: Some(preference_path.clone()),
            state: Mutex::new(AutostartFile {
                version: FILE_VERSION,
                enabled: false,
            }),
            registration: Box::new(FakeRegistration(fake)),
        };
        let view = instance.set_enabled(true, Some(&setup_path));
        assert!(view.enabled);
        assert_eq!(view.state, State::On);
        assert!(AutostartFile::load(&preference_path).unwrap().enabled);
    }
}
