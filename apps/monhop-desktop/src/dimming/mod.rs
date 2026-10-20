//! Screen dimming owned by the desktop app: the persisted preference, whether the overlay is up,
//! and the system-wide shortcut, dispatched to the platform module that draws the overlay.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use monhop_core::dimming::{DIM_TOGGLE, DimLevel};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

/// Tauri event carrying a `DimmingView` whenever the overlay or the preference changes.
pub const EVENT: &str = "dimming";
const FILE_NAME: &str = "dimming.json";
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 4096;
#[cfg(target_os = "macos")]
const SHORTCUT: &str = "Control + Option + 0";
#[cfg(not(target_os = "macos"))]
const SHORTCUT: &str = "Ctrl + Alt + 0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DimmingFile {
    version: u32,
    enabled: bool,
    level: u8,
}

impl Default for DimmingFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: true,
            level: DimLevel::DEFAULT.percent(),
        }
    }
}

impl DimmingFile {
    fn load(path: &Path) -> io::Result<Self> {
        let Some(bytes) = crate::sharing_preferences::read_bounded(path, MAX_FILE_BYTES)? else {
            return Ok(Self::default());
        };
        let file: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if file.version != FILE_VERSION || DimLevel::new(file.level).is_none() {
            return Err(invalid());
        }
        Ok(file)
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|_| invalid())?;
        crate::sharing_preferences::save_bounded(path, &bytes, MAX_FILE_BYTES)
    }

    fn level(&self) -> DimLevel {
        DimLevel::new(self.level).unwrap_or_default()
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid dimming preference")
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DimmingView {
    enabled: bool,
    level: u8,
    min_level: u8,
    max_level: u8,
    dimmed: bool,
    shortcut: &'static str,
    error: Option<String>,
}

struct State {
    file: DimmingFile,
    dimmed: bool,
    /// The last failure the user should see: a shortcut that could not register, or a save that failed.
    error: Option<String>,
    /// Whether `error` is the shortcut failure, so a later registration clears only that message.
    shortcut_failed: bool,
}

impl State {
    /// One slot shows one failure. `from_shortcut` marks a registration failure so the message can
    /// be withdrawn if the chord later comes free.
    fn fail(&mut self, error: Option<String>, from_shortcut: bool) {
        self.shortcut_failed = from_shortcut && error.is_some();
        self.error = error;
    }

    /// The chord is in hand: drop the registration failure, leave any other failure showing.
    fn registered(&mut self) {
        if self.shortcut_failed {
            self.shortcut_failed = false;
            self.error = None;
        }
    }
}

/// How a platform chases a chord another application already holds: the gate it checks before
/// every retry, and the report it makes once one succeeds. Only Windows retries today; macOS
/// reports its own registration failure and stops, so its build never reads these.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Retry {
    wanted: Box<dyn Fn() -> bool + Send + Sync + 'static>,
    recovered: Box<dyn Fn(u32, Duration) + Send + Sync + 'static>,
}

pub struct Dimming {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl Dimming {
    /// Loads the preference and registers the shortcut when it is on. Runs in setup, on the main
    /// thread. The UI smoke check passes `register_shortcut = false` so it never claims the chord.
    pub fn start(app: &AppHandle, register_shortcut: bool) -> Arc<Self> {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(FILE_NAME));
        let (file, error) = match path.as_deref().map(DimmingFile::load) {
            Some(Ok(file)) => (file, None),
            Some(Err(_)) => (
                DimmingFile::default(),
                Some(
                    "The saved dimming preference could not be read. Defaults are in use."
                        .to_owned(),
                ),
            ),
            None => (
                DimmingFile::default(),
                Some("The dimming preference has nowhere to be saved.".to_owned()),
            ),
        };
        let dimming = Arc::new(Self {
            path,
            state: Mutex::new(State {
                file,
                dimmed: false,
                error,
                shortcut_failed: false,
            }),
        });
        if file.enabled && register_shortcut {
            match platform::register_hotkey(
                app,
                DIM_TOGGLE,
                press_handler(app),
                retry(app, &dimming),
            ) {
                Ok(()) => log::info!("dimming: shortcut registered at startup"),
                Err(error) => {
                    log::warn!("dimming: shortcut not registered at startup: {error}");
                    lock(&dimming.state).fail(Some(error), true);
                }
            }
        }
        dimming
    }

    pub fn view(&self) -> DimmingView {
        view_of(&lock(&self.state))
    }

    /// Turns the shortcut on or off; turning it off also lifts a dimmed screen. Main thread only.
    /// Turning it off ends any retry in progress; turning it on starts the backoff over.
    pub fn set_enabled(
        self: &Arc<Self>,
        app: &AppHandle,
        enabled: bool,
    ) -> Result<DimmingView, String> {
        let outcome = if enabled {
            platform::register_hotkey(app, DIM_TOGGLE, press_handler(app), retry(app, self))
        } else {
            platform::unregister_hotkey(app)
        };
        match &outcome {
            Ok(()) => log::info!(
                "user: dimming shortcut {}",
                if enabled { "on" } else { "off" }
            ),
            Err(error) => log::warn!("dimming: shortcut change failed: {error}"),
        }
        let mut state = lock(&self.state);
        state.fail(outcome.err(), true);
        state.file.enabled = enabled;
        if !enabled && state.dimmed {
            state.dimmed = false;
            platform::hide(app);
        }
        self.persist(&mut state);
        Ok(self.announce(app, &state))
    }

    /// Saves a new darkness and applies it live when the screen is dimmed.
    pub fn set_level(&self, app: &AppHandle, level: u8) -> Result<DimmingView, String> {
        let level = valid_level(level)?;
        let mut state = lock(&self.state);
        state.file.level = level.percent();
        if state.dimmed {
            platform::set_level(app, level);
        }
        self.persist(&mut state);
        Ok(self.announce(app, &state))
    }

    /// Applies a darkness live without saving it, for a slider still being dragged.
    pub fn preview_level(&self, app: &AppHandle, level: u8) -> Result<(), String> {
        let level = valid_level(level)?;
        if lock(&self.state).dimmed {
            platform::set_level(app, level);
        }
        Ok(())
    }

    /// The Dim now button: toggles whether or not the shortcut is on.
    pub fn toggle(&self, app: &AppHandle) -> Result<DimmingView, String> {
        let mut state = lock(&self.state);
        self.toggle_locked(app, &mut state)
    }

    /// A shortcut press. The enabled check and the toggle share one lock hold, so a press already
    /// dispatched when the shortcut was turned off cannot show a disabled overlay.
    pub fn toggle_from_shortcut(&self, app: &AppHandle) -> Result<Option<DimmingView>, String> {
        let mut state = lock(&self.state);
        if !press_applies(&state) {
            log::debug!("dimming: shortcut press arrived after the shortcut was turned off");
            return Ok(None);
        }
        self.toggle_locked(app, &mut state).map(Some)
    }

    fn toggle_locked(&self, app: &AppHandle, state: &mut State) -> Result<DimmingView, String> {
        if state.dimmed {
            state.dimmed = false;
            platform::hide(app);
        } else {
            platform::show(app, state.file.level())?;
            state.dimmed = true;
        }
        log::info!(
            "dimming: overlay {}",
            if state.dimmed { "shown" } else { "hidden" }
        );
        Ok(self.announce(app, state))
    }

    fn persist(&self, state: &mut State) {
        let saved = self
            .path
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|path| state.file.save(path));
        if saved.is_err() {
            state.fail(
                Some("The dimming preference could not be saved.".to_owned()),
                false,
            );
        }
    }

    fn announce(&self, app: &AppHandle, state: &State) -> DimmingView {
        let view = view_of(state);
        let _ = app.emit(EVENT, view.clone());
        view
    }
}

fn valid_level(level: u8) -> Result<DimLevel, String> {
    DimLevel::new(level).ok_or_else(|| {
        format!(
            "Choose a dimming level between {} and {} percent.",
            DimLevel::MIN,
            DimLevel::MAX
        )
    })
}

fn view_of(state: &State) -> DimmingView {
    DimmingView {
        enabled: state.file.enabled,
        level: state.file.level,
        min_level: DimLevel::MIN,
        max_level: DimLevel::MAX,
        dimmed: state.dimmed,
        shortcut: SHORTCUT,
        error: state.error.clone(),
    }
}

/// A shortcut press toggles the overlay and tells the window; failures stay in the view.
/// Only an enabled shortcut acts, and only an enabled shortcut is worth retrying; both read this
/// under the state lock that the toggle itself holds.
fn press_applies(state: &State) -> bool {
    state.file.enabled
}

fn press_handler(app: &AppHandle) -> Box<dyn Fn() + Send + Sync + 'static> {
    let app = app.clone();
    Box::new(move || {
        let dimming = app.state::<Arc<Dimming>>().inner().clone();
        if let Err(error) = dimming.toggle_from_shortcut(&app) {
            let mut state = lock(&dimming.state);
            state.fail(Some(error), false);
            dimming.announce(&app, &state);
        }
    })
}

/// What the platform needs to chase a chord another application holds: keep asking only while the
/// user still wants the shortcut and the app is still running, then take the message off the Home
/// card. Both closures run on the thread that owns the chord, holding no dimming lock.
fn retry(app: &AppHandle, dimming: &Arc<Dimming>) -> Retry {
    let (gate_app, gate) = (app.clone(), Arc::downgrade(dimming));
    let (report_app, report) = (app.clone(), Arc::downgrade(dimming));
    Retry {
        wanted: Box::new(move || {
            let Some(dimming) = gate.upgrade() else {
                return false;
            };
            if !press_applies(&lock(&dimming.state)) {
                return false;
            }
            // An event loop that refuses work is an app on its way out: nothing left to chase for.
            gate_app.run_on_main_thread(|| {}).is_ok()
        }),
        recovered: Box::new(move |attempts, waited| {
            let Some(dimming) = report.upgrade() else {
                return;
            };
            log::info!(
                "dimming: shortcut registered after {attempts} attempts, {} s",
                waited.as_secs()
            );
            let mut state = lock(&dimming.state);
            state.registered();
            dimming.announce(&report_app, &state);
        }),
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs a main-thread-only dimming action from a command and waits for its result.
async fn on_main_thread<T: Send + 'static>(
    app: &AppHandle,
    action: impl FnOnce(&AppHandle) -> T + Send + 'static,
) -> Result<T, String> {
    let (sender, mut receiver) = tauri::async_runtime::channel(1);
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = sender.try_send(action(&handle));
    })
    .map_err(|_| "The dimming change could not be scheduled. Try again.".to_owned())?;
    receiver
        .recv()
        .await
        .ok_or_else(|| "The dimming change ended before reporting a result.".to_owned())
}

#[tauri::command]
pub async fn dimming_status(app: AppHandle) -> Result<DimmingView, String> {
    Ok(app.state::<Arc<Dimming>>().view())
}

#[tauri::command]
pub async fn dimming_set_enabled(app: AppHandle, enabled: bool) -> Result<DimmingView, String> {
    on_main_thread(&app, move |app| {
        app.state::<Arc<Dimming>>()
            .inner()
            .set_enabled(app, enabled)
    })
    .await?
}

#[tauri::command]
pub async fn dimming_set_level(app: AppHandle, level: u8) -> Result<DimmingView, String> {
    let dimming = app.state::<Arc<Dimming>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || dimming.set_level(&app, level))
        .await
        .map_err(|_| "The dimming level was not saved. Try again.".to_owned())?
}

#[tauri::command]
pub async fn dimming_preview_level(app: AppHandle, level: u8) -> Result<(), String> {
    app.state::<Arc<Dimming>>().preview_level(&app, level)
}

#[tauri::command]
pub async fn dimming_toggle(app: AppHandle) -> Result<DimmingView, String> {
    app.state::<Arc<Dimming>>().toggle(&app)
}

#[cfg(target_os = "macos")]
mod platform {
    use monhop_core::dimming::{Chord, DimLevel};
    use objc2::MainThreadMarker;
    use tauri::AppHandle;

    fn on_main(app: &AppHandle, action: impl FnOnce(MainThreadMarker) + Send + 'static) {
        let _ = app.run_on_main_thread(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                action(mtm);
            }
        });
    }

    fn main_thread() -> Result<MainThreadMarker, String> {
        MainThreadMarker::new()
            .ok_or_else(|| "The dimming shortcut changes on the app's main thread.".to_owned())
    }

    pub fn show(app: &AppHandle, level: DimLevel) -> Result<(), String> {
        on_main(app, move |mtm| super::macos::show(mtm, level));
        Ok(())
    }

    pub fn hide(app: &AppHandle) {
        on_main(app, super::macos::hide);
    }

    pub fn set_level(app: &AppHandle, level: DimLevel) {
        on_main(app, move |mtm| super::macos::set_level(mtm, level));
    }

    /// macOS reports a registration failure and stops there, so the retry policy goes unused.
    pub fn register_hotkey(
        _: &AppHandle,
        chord: Chord,
        on_press: Box<dyn Fn() + Send + Sync + 'static>,
        _: super::Retry,
    ) -> Result<(), String> {
        super::macos::register_hotkey(main_thread()?, chord, on_press)
    }

    pub fn unregister_hotkey(_: &AppHandle) -> Result<(), String> {
        super::macos::unregister_hotkey(main_thread()?);
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use monhop_core::dimming::{Chord, DimLevel};
    use tauri::AppHandle;

    pub fn show(_: &AppHandle, level: DimLevel) -> Result<(), String> {
        super::windows::show(level)
    }

    pub fn hide(_: &AppHandle) {
        super::windows::hide();
    }

    pub fn set_level(_: &AppHandle, level: DimLevel) {
        super::windows::set_level(level);
    }

    pub fn register_hotkey(
        _: &AppHandle,
        chord: Chord,
        on_press: Box<dyn Fn() + Send + Sync + 'static>,
        retry: super::Retry,
    ) -> Result<(), String> {
        super::windows::register_hotkey(chord, on_press, retry)
    }

    pub fn unregister_hotkey(_: &AppHandle) -> Result<(), String> {
        super::windows::unregister_hotkey();
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use monhop_core::dimming::{Chord, DimLevel};
    use tauri::AppHandle;

    const UNSUPPORTED: &str = "Screen dimming is available on macOS and Windows.";

    pub fn show(_: &AppHandle, _: DimLevel) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub fn hide(_: &AppHandle) {}

    pub fn set_level(_: &AppHandle, _: DimLevel) {}

    pub fn register_hotkey(
        _: &AppHandle,
        _: Chord,
        _: Box<dyn Fn() + Send + Sync + 'static>,
        _: super::Retry,
    ) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    pub fn unregister_hotkey(_: &AppHandle) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("monhop-dimming-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_means_the_shortcut_is_on_at_the_default_level() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        let file = DimmingFile::load(&path).unwrap();
        assert!(file.enabled);
        assert_eq!(file.level(), DimLevel::DEFAULT);
    }

    #[test]
    fn a_saved_preference_round_trips_and_an_out_of_range_level_is_rejected() {
        let path = temporary_path("roundtrip");
        let file = DimmingFile {
            version: FILE_VERSION,
            enabled: false,
            level: 70,
        };
        file.save(&path).unwrap();
        assert_eq!(DimmingFile::load(&path).unwrap(), file);
        std::fs::write(&path, br#"{"version":1,"enabled":true,"level":100}"#).unwrap();
        assert_eq!(
            DimmingFile::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, br#"{"version":2,"enabled":true,"level":50}"#).unwrap();
        assert!(DimmingFile::load(&path).is_err());
    }

    /// Presses and retries share one gate, so turning the switch off cancels a pending retry the
    /// same way it ignores a press already on its way.
    #[test]
    fn a_shortcut_press_is_ignored_once_the_shortcut_is_off() {
        let mut state = State {
            file: DimmingFile::default(),
            dimmed: false,
            error: None,
            shortcut_failed: false,
        };
        assert!(press_applies(&state));
        state.file.enabled = false;
        assert!(!press_applies(&state));
    }

    #[test]
    fn a_registration_clears_only_the_message_the_shortcut_put_on_the_card() {
        let mut state = State {
            file: DimmingFile::default(),
            dimmed: false,
            error: None,
            shortcut_failed: false,
        };
        state.fail(Some("held by another app".to_owned()), true);
        assert_eq!(
            view_of(&state).error.as_deref(),
            Some("held by another app")
        );
        state.registered();
        assert!(view_of(&state).error.is_none());
        state.fail(Some("nowhere to save".to_owned()), false);
        state.registered();
        assert_eq!(view_of(&state).error.as_deref(), Some("nowhere to save"));
        state.fail(None, true);
        assert!(view_of(&state).error.is_none());
    }

    #[test]
    fn a_view_reports_the_bounds_and_the_shortcut() {
        let state = State {
            file: DimmingFile::default(),
            dimmed: true,
            error: None,
            shortcut_failed: false,
        };
        let view = view_of(&state);
        assert!(view.enabled && view.dimmed);
        assert_eq!((view.min_level, view.level, view.max_level), (10, 50, 99));
        assert!(view.shortcut.ends_with("0"));
        assert!(valid_level(9).is_err() && valid_level(10).is_ok());
    }
}
