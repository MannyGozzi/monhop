//! MonHop's only outbound request: a signed build announced by the pinned endpoint, downloaded in
//! the background and installed when the app quits or the user presses Restart to update.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Url};
use tauri_plugin_updater::UpdaterExt;

use crate::{lifecycle::AppController, sharing_preferences::SetupFile};

/// Tauri event carrying an `UpdatesView` whenever the phase or the download progress changes.
pub const EVENT: &str = "updates";
/// The only host MonHop ever talks to, shown to the user so the one request is never a surprise.
const HOST: &str = "github.com";
const FILE_NAME: &str = "updates.json";
const FILE_VERSION: u32 = 2;
const MAX_FILE_BYTES: u64 = 512;
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The endpoint's own JSON is not signed, so its text is bounded before it reaches the window.
const MAX_VERSION_CHARS: usize = 32;
const MAX_NOTES_CHARS: usize = 2000;

#[cfg(test)]
const SHARING_HINT: &str = "Sharing is running. Turn it off first or quit MonHop to update.";
const NOTHING_READY: &str = "No update is ready to install yet.";
const NO_BUILD: &str = "The downloaded build is no longer available. Check again.";
const RESTARTING: &str = "MonHop is restarting to finish the update.";
const INSTALL_FAILED: &str = "The update could not be installed. Try again.";

/// Debug builds may be pointed at a local server; a release build always uses the pinned endpoint.
#[cfg(debug_assertions)]
const CONFIGURED_ENDPOINT: Option<&str> = option_env!("MONHOP_UPDATE_ENDPOINT");
#[cfg(not(debug_assertions))]
const CONFIGURED_ENDPOINT: Option<&str> = None;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdatesFile {
    version: u32,
    automatic: bool,
}

impl Default for UpdatesFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            automatic: false,
        }
    }
}

impl UpdatesFile {
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
    io::Error::new(io::ErrorKind::InvalidData, "invalid updates preference")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Ready,
    Installing,
    Failed,
}

/// A newer build the endpoint announced. `update` carries the plugin's handle; tests leave it out.
#[derive(Clone)]
pub struct Found {
    version: String,
    notes: Option<String>,
    update: Option<tauri_plugin_updater::Update>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatesView {
    automatic: bool,
    phase: Phase,
    current_version: &'static str,
    build_commit: &'static str,
    available_version: Option<String>,
    notes: Option<String>,
    progress_percent: Option<u8>,
    message: String,
    checked_seconds_ago: Option<u64>,
    host: &'static str,
    sharing_active: bool,
}

type Eventual<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything the state machine touches outside itself: the network, the sharing switch, the saved
/// setup and the window. Tests replace it, so no unit test ever reaches a network.
pub trait Environment: Send + Sync + 'static {
    fn check(&self) -> Eventual<'_, Result<Option<Found>, String>>;
    fn download(
        &self,
        found: &Found,
        progress: Arc<dyn Fn(u8) + Send + Sync>,
    ) -> Eventual<'_, Result<Vec<u8>, String>>;
    fn install(&self, found: &Found, bytes: &[u8]) -> Result<(), String>;
    fn acquire_lease(&self, installing: bool) -> Result<Box<dyn Send>, String>;
    fn sharing_active(&self) -> bool;
    /// True once a paired computer is the active one; no automatic check runs before that.
    fn paired(&self) -> bool;
    fn announce(&self, view: &UpdatesView);
}

struct State {
    file: UpdatesFile,
    phase: Phase,
    found: Option<Found>,
    /// Bytes the plugin already verified against the bundled public key.
    package: Option<Vec<u8>>,
    progress: Option<u8>,
    message: String,
    checked_at: Option<Instant>,
    running: bool,
    exit_intent: Option<ExitIntent>,
    exit_ready: bool,
}

pub struct Updates {
    path: Option<PathBuf>,
    /// False in the UI smoke check: no scheduler, and a check request reaches no network.
    allowed: bool,
    state: Mutex<State>,
    cancel_network: tokio::sync::Notify,
    environment: Box<dyn Environment>,
}

#[derive(Clone, Copy)]
enum ExitIntent {
    Quit,
    Restart,
}

struct Operation {
    updates: Arc<Updates>,
    lease: Option<Box<dyn Send>>,
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.lease.take();
        let mut state = lock(&self.updates.state);
        state.running = false;
        if matches!(
            state.phase,
            Phase::Checking | Phase::Downloading | Phase::Installing
        ) {
            state.message = "The update did not finish. Try again.".into();
            self.updates.settle(&mut state, Phase::Failed);
        }
    }
}

impl Updates {
    /// Missing, old or unreadable preferences require a fresh opt-in.
    pub fn start(app: &AppHandle, allowed: bool) -> Arc<Self> {
        let path = app
            .path()
            .app_local_data_dir()
            .ok()
            .map(|directory| directory.join(FILE_NAME));
        let file = match path.as_deref().map(UpdatesFile::load) {
            Some(Ok(file)) => file,
            Some(Err(_)) | None => {
                log::warn!("updates: preference could not be read, keeping automatic updates off");
                UpdatesFile::default()
            }
        };
        let updates = Self::with(
            path,
            allowed,
            file,
            Box::new(AppEnvironment { app: app.clone() }),
        );
        if allowed {
            schedule(updates.clone());
        }
        updates
    }

    fn with(
        path: Option<PathBuf>,
        allowed: bool,
        file: UpdatesFile,
        environment: Box<dyn Environment>,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            allowed,
            state: Mutex::new(State {
                file,
                phase: Phase::Idle,
                found: None,
                package: None,
                progress: None,
                message: String::new(),
                checked_at: None,
                running: false,
                exit_intent: None,
                exit_ready: false,
            }),
            cancel_network: tokio::sync::Notify::new(),
            environment,
        })
    }

    pub fn view(&self) -> UpdatesView {
        self.view_of(&lock(&self.state))
    }

    fn view_of(&self, state: &State) -> UpdatesView {
        UpdatesView {
            automatic: state.file.automatic,
            phase: state.phase,
            current_version: env!("CARGO_PKG_VERSION"),
            build_commit: env!("MONHOP_BUILD_COMMIT"),
            available_version: state.found.as_ref().map(|found| found.version.clone()),
            notes: state.found.as_ref().and_then(|found| found.notes.clone()),
            progress_percent: state.progress,
            message: state.message.clone(),
            checked_seconds_ago: state.checked_at.map(|at| at.elapsed().as_secs()),
            host: HOST,
            sharing_active: self.environment.sharing_active(),
        }
    }

    /// Every phase change goes through here, so the window never misses one.
    fn settle(&self, state: &mut State, phase: Phase) -> UpdatesView {
        state.phase = phase;
        let view = self.view_of(state);
        self.environment.announce(&view);
        view
    }

    /// Saves the switch. A save failure keeps the choice for this run rather than losing it.
    pub fn set_automatic(&self, enabled: bool) -> UpdatesView {
        let mut state = lock(&self.state);
        state.file.automatic = enabled;
        let saved = self
            .path
            .as_deref()
            .ok_or_else(invalid)
            .and_then(|path| state.file.save(path));
        state.message = if saved.is_err() {
            "The update preference could not be saved.".to_owned()
        } else {
            String::new()
        };
        log::info!(
            "user: automatic updates {}",
            if enabled { "on" } else { "off" }
        );
        let view = self.view_of(&state);
        self.environment.announce(&view);
        view
    }

    fn enter_checking(self: &Arc<Self>, automatic: bool) -> Option<(UpdatesView, Operation)> {
        let mut state = lock(&self.state);
        if state.running
            || !self.allowed
            || state.exit_intent.is_some()
            || (automatic && !state.file.automatic)
        {
            return None;
        }
        let lease = match self.environment.acquire_lease(false) {
            Ok(lease) => lease,
            Err(message) => {
                state.message = message;
                let view = self.view_of(&state);
                self.environment.announce(&view);
                return None;
            }
        };
        state.running = true;
        state.message.clear();
        state.progress = None;
        let view = self.settle(&mut state, Phase::Checking);
        Some((
            view,
            Operation {
                updates: self.clone(),
                lease: Some(lease),
            },
        ))
    }

    /// Check now grants consent for this check and its signed download only.
    pub fn request_check(self: &Arc<Self>) -> UpdatesView {
        match self.enter_checking(false) {
            Some((view, operation)) => {
                let updates = self.clone();
                tauri::async_runtime::spawn(async move {
                    updates.run_check("user", operation).await;
                });
                view
            }
            None => self.view(),
        }
    }

    async fn check_now(self: &Arc<Self>, reason: &'static str) -> UpdatesView {
        if let Some((_, operation)) = self.enter_checking(reason == "schedule") {
            self.clone().run_check(reason, operation).await;
        }
        self.view()
    }

    /// The schedule only reaches the network with automatic on and a computer already paired.
    fn scheduled_check_applies(&self) -> bool {
        lock(&self.state).file.automatic && self.environment.paired()
    }

    async fn scheduled_check(self: &Arc<Self>) {
        if self.scheduled_check_applies() {
            self.check_now("schedule").await;
        }
    }

    /// A found build is downloaded straight away: the window offers no separate download step.
    async fn run_check(self: Arc<Self>, reason: &'static str, _operation: Operation) {
        let cancelled = tokio::select! {
            biased;
            () = self.cancel_network.notified() => true,
            () = self.clone().check_and_download(reason) => false,
        };
        // Drop the request future before releasing the lease. Already verified bytes survive quit.
        if cancelled {
            let mut state = lock(&self.state);
            state.message.clear();
            state.progress = None;
            let phase = if state.package.is_some() {
                Phase::Ready
            } else {
                state.found = None;
                Phase::Idle
            };
            self.settle(&mut state, phase);
        }
    }

    async fn check_and_download(self: Arc<Self>, reason: &'static str) {
        log::info!("updates: checking ({reason})");
        let result = self.environment.check().await;
        let found = {
            let mut state = lock(&self.state);
            state.checked_at = Some(Instant::now());
            match result {
                Err(message) => {
                    log::warn!("updates: check failed: {message}");
                    state.message = message;
                    // A download already verified is not lost to a check that could not finish.
                    if state.package.is_some() {
                        self.settle(&mut state, Phase::Ready);
                    } else {
                        state.found = None;
                        self.settle(&mut state, Phase::Failed);
                    }
                    None
                }
                Ok(None) => {
                    log::info!("updates: no newer build");
                    state.found = None;
                    state.package = None;
                    self.settle(&mut state, Phase::UpToDate);
                    None
                }
                Ok(Some(found)) => {
                    log::info!("updates: {} available", found.version);
                    state.found = Some(found.clone());
                    state.package = None;
                    self.settle(&mut state, Phase::Available);
                    Some(found)
                }
            }
        };
        if let Some(found) = found {
            self.download(found).await;
        }
    }

    async fn download(self: Arc<Self>, found: Found) {
        {
            let mut state = lock(&self.state);
            state.progress = Some(0);
            self.settle(&mut state, Phase::Downloading);
        }
        let reporter: Arc<dyn Fn(u8) + Send + Sync> = {
            let updates = self.clone();
            Arc::new(move |percent| updates.note_progress(percent))
        };
        let result = self.environment.download(&found, reporter).await;
        let mut state = lock(&self.state);
        state.progress = None;
        match result {
            Ok(bytes) => {
                log::info!("updates: {} downloaded and verified", found.version);
                state.package = Some(bytes);
                self.settle(&mut state, Phase::Ready);
            }
            Err(message) => {
                log::warn!("updates: download failed: {message}");
                state.message = message;
                self.settle(&mut state, Phase::Failed);
            }
        }
    }

    fn note_progress(&self, percent: u8) {
        let mut state = lock(&self.state);
        if state.progress == Some(percent) {
            return;
        }
        state.progress = Some(percent);
        let view = self.view_of(&state);
        self.environment.announce(&view);
    }

    /// Leaves the phase alone and says why nothing happened.
    fn note(&self, message: &str) -> UpdatesView {
        let mut state = lock(&self.state);
        state.message = message.to_owned();
        let view = self.view_of(&state);
        self.environment.announce(&view);
        view
    }

    fn running(&self) -> bool {
        lock(&self.state).running
    }

    async fn install_ready(self: &Arc<Self>, during_quit: bool) -> Result<(), String> {
        let (found, bytes, operation) = {
            let mut state = lock(&self.state);
            if state.running
                || state.phase != Phase::Ready
                || (!during_quit && state.exit_intent.is_some())
            {
                return Err(NOTHING_READY.to_owned());
            }
            let lease = self.environment.acquire_lease(true)?;
            let (Some(found), Some(bytes)) = (state.found.clone(), state.package.take()) else {
                return Err(NOTHING_READY.to_owned());
            };
            state.running = true;
            state.message.clear();
            self.settle(&mut state, Phase::Installing);
            (
                found,
                bytes,
                Operation {
                    updates: self.clone(),
                    lease: Some(lease),
                },
            )
        };
        let updates = self.clone();
        // The blocking worker owns the lease even if the invoking future is cancelled.
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = operation;
            let result = updates.environment.install(&found, &bytes);
            let mut state = lock(&updates.state);
            match &result {
                Ok(()) => {
                    state.found = None;
                    updates.settle(&mut state, Phase::Idle);
                }
                Err(message) => {
                    state.package = Some(bytes);
                    state.message.clone_from(message);
                    updates.settle(&mut state, Phase::Ready);
                }
            }
            result
        })
        .await
        .map_err(|_| INSTALL_FAILED.to_owned())?
    }

    async fn finish_update_before_exit(self: &Arc<Self>, intent: ExitIntent) {
        while self.running() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let ready = lock(&self.state).phase == Phase::Ready;
        if matches!(intent, ExitIntent::Quit)
            && ready
            && let Err(message) = self.install_ready(true).await
        {
            log::warn!("updates: could not install before quit: {message}");
        }
    }

    fn begin_shutdown(&self, intent: ExitIntent) -> bool {
        let mut state = lock(&self.state);
        if state.exit_intent.is_some() {
            return false;
        }
        state.exit_intent = Some(intent);
        // At most one network operation exists. Keep a permit if it has not started polling yet.
        self.cancel_network.notify_one();
        true
    }

    fn request_shutdown(self: &Arc<Self>, app: &AppHandle, intent: ExitIntent) {
        if !self.begin_shutdown(intent) {
            return;
        }
        let controller = app.state::<Arc<AppController>>().inner().clone();
        controller.request_shutdown();
        let updates = self.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            while !controller.shutdown_ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            updates.finish_update_before_exit(intent).await;
            lock(&updates.state).exit_ready = true;
            match intent {
                ExitIntent::Quit => app.exit(0),
                ExitIntent::Restart => app.restart(),
            }
        });
    }
}

fn schedule(updates: Arc<Updates>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            updates.scheduled_check().await;
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

pub fn request_quit(app: &AppHandle) {
    app.state::<Arc<Updates>>()
        .inner()
        .request_shutdown(app, ExitIntent::Quit);
}

pub fn exit_ready(app: &AppHandle) -> bool {
    app.try_state::<Arc<Updates>>()
        .is_none_or(|updates| lock(&updates.state).exit_ready)
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The endpoint an override replaces the pinned one with; a release build never has one.
fn endpoint_override(configured: Option<&str>) -> Option<Url> {
    configured
        .filter(|_| cfg!(debug_assertions))
        .and_then(|endpoint| endpoint.parse().ok())
}

fn bounded(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn percent(received: u64, total: u64) -> u8 {
    (received.min(total).saturating_mul(100) / total.max(1)) as u8
}

/// Plain text for each failure. The plugin's own errors can carry addresses, so they never escape.
fn describe(error: tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;
    match error {
        Error::ReleaseNotFound | Error::TargetNotFound(_) | Error::TargetsNotFound(_) => {
            "No MonHop build is published for this computer yet."
        }
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            "The build was not signed by MonHop, so it was discarded."
        }
        Error::Network(_) | Error::Reqwest(_) => {
            "github.com could not be reached. MonHop will try again later."
        }
        _ => "The update check did not finish. Try again later.",
    }
    .to_owned()
}

/// The failure's kind alone, for the log: the plugin's error text can carry paths.
fn kind(error: &tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;
    match error {
        Error::Io(io) => format!("io: {:?}", io.kind()),
        Error::TempDirNotOnSameMountPoint => "temp dir on another mount".to_owned(),
        Error::BinaryNotFoundInArchive => "binary missing from the archive".to_owned(),
        Error::TempDirNotFound => "temp dir missing".to_owned(),
        Error::AuthenticationFailed => "authentication failed".to_owned(),
        Error::InvalidUpdaterFormat => "invalid updater format".to_owned(),
        _ => "other".to_owned(),
    }
}

fn install_error_message(error: &tauri_plugin_updater::Error) -> &'static str {
    #[cfg(target_os = "macos")]
    if matches!(
        error,
        tauri_plugin_updater::Error::MacInstallParentNotWritable(_)
    ) {
        return "MonHop cannot replace this installation. Download the latest installer and replace it manually.";
    }
    if matches!(error, tauri_plugin_updater::Error::Io(io)
        if matches!(io.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem))
    {
        return "MonHop cannot replace this installation. Download the latest installer and replace it manually.";
    }
    INSTALL_FAILED
}

struct AppEnvironment {
    app: AppHandle,
}

impl Environment for AppEnvironment {
    fn check(&self) -> Eventual<'_, Result<Option<Found>, String>> {
        let app = self.app.clone();
        Box::pin(async move {
            // The install lease requires idle input and sockets. Latch shutdown only after
            // Windows launches its installer, so a failed launch leaves MonHop usable.
            let controller = app.state::<Arc<AppController>>().inner().clone();
            let mut builder = app
                .updater_builder()
                .timeout(CHECK_TIMEOUT)
                .configure_client(|client| {
                    client
                        .connect_timeout(CHECK_TIMEOUT)
                        .read_timeout(CHECK_TIMEOUT)
                })
                .on_before_exit(move || controller.drain_for_exit());
            if let Some(endpoint) = endpoint_override(CONFIGURED_ENDPOINT) {
                builder = builder.endpoints(vec![endpoint]).map_err(describe)?;
            }
            let found = builder
                .build()
                .map_err(describe)?
                .check()
                .await
                .map_err(describe)?;
            Ok(found.map(|update| Found {
                version: bounded(&update.version, MAX_VERSION_CHARS),
                notes: update
                    .body
                    .as_deref()
                    .map(str::trim)
                    .filter(|notes| !notes.is_empty())
                    .map(|notes| bounded(notes, MAX_NOTES_CHARS)),
                update: Some(update),
            }))
        })
    }

    fn download(
        &self,
        found: &Found,
        progress: Arc<dyn Fn(u8) + Send + Sync>,
    ) -> Eventual<'_, Result<Vec<u8>, String>> {
        let update = found.update.clone();
        Box::pin(async move {
            let update = update.ok_or_else(|| NO_BUILD.to_owned())?;
            let mut received: u64 = 0;
            update
                .download(
                    |chunk, total| {
                        received = received.saturating_add(chunk as u64);
                        if let Some(total) = total.filter(|total| *total > 0) {
                            progress(percent(received, total));
                        }
                    },
                    || {},
                )
                .await
                .map_err(describe)
        })
    }

    fn install(&self, found: &Found, bytes: &[u8]) -> Result<(), String> {
        let result = found
            .update
            .as_ref()
            .ok_or_else(|| NO_BUILD.to_owned())?
            .install(bytes);
        match result {
            Ok(()) => Ok(()),
            #[cfg(target_os = "macos")]
            Err(tauri_plugin_updater::Error::MacUpdateInstalledCleanupFailed(_)) => {
                log::warn!(
                    "updates: update installed, but old application files could not be fully removed"
                );
                Ok(())
            }
            Err(error) => {
                log::warn!("updates: install failed ({})", kind(&error));
                Err(install_error_message(&error).to_owned())
            }
        }
    }

    fn acquire_lease(&self, installing: bool) -> Result<Box<dyn Send>, String> {
        self.app
            .state::<Arc<AppController>>()
            .begin_update(installing)
            .map(|lease| Box::new(lease) as Box<dyn Send>)
    }

    fn sharing_active(&self) -> bool {
        self.app
            .state::<Arc<AppController>>()
            .sharing
            .status()
            .sharing_active
    }

    fn paired(&self) -> bool {
        crate::sharing_setup_path(&self.app)
            .ok()
            .and_then(|path| SetupFile::load(&path).ok())
            .is_some_and(|file| file.active().is_some())
    }

    fn announce(&self, view: &UpdatesView) {
        let _ = self.app.emit(EVENT, view);
    }
}

#[tauri::command]
pub fn updates_status(updates: tauri::State<'_, Arc<Updates>>) -> UpdatesView {
    updates.view()
}

#[tauri::command]
pub fn updates_set_automatic(
    updates: tauri::State<'_, Arc<Updates>>,
    enabled: bool,
) -> UpdatesView {
    updates.set_automatic(enabled)
}

#[tauri::command]
pub fn updates_check(updates: tauri::State<'_, Arc<Updates>>) -> UpdatesView {
    updates.inner().request_check()
}

#[tauri::command]
pub async fn updates_install(app: AppHandle) -> UpdatesView {
    let updates = app.state::<Arc<Updates>>().inner().clone();
    match updates.install_ready(false).await {
        Ok(()) => {
            updates.request_shutdown(&app, ExitIntent::Restart);
            updates.note(RESTARTING)
        }
        Err(message) => updates.note(&message),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    struct Fake {
        checks: AtomicUsize,
        downloads: AtomicUsize,
        installs: AtomicUsize,
        found: Mutex<Result<Option<Found>, String>>,
        bytes: Mutex<Result<Vec<u8>, String>>,
        sharing: AtomicBool,
        paired: AtomicBool,
        events: Mutex<Vec<UpdatesView>>,
        busy: Arc<AtomicBool>,
        active_requests: Arc<AtomicUsize>,
        install_error: Mutex<Option<String>>,
        check_pause: Mutex<Option<Arc<tokio::sync::Notify>>>,
        download_pause: Mutex<Option<Arc<tokio::sync::Notify>>>,
        install_pause: Mutex<Option<Arc<std::sync::Barrier>>>,
    }

    impl Fake {
        fn new(found: Result<Option<Found>, String>) -> Arc<Self> {
            Arc::new(Self {
                checks: AtomicUsize::new(0),
                downloads: AtomicUsize::new(0),
                installs: AtomicUsize::new(0),
                found: Mutex::new(found),
                bytes: Mutex::new(Ok(vec![1, 2, 3])),
                sharing: AtomicBool::new(false),
                paired: AtomicBool::new(true),
                events: Mutex::new(Vec::new()),
                busy: Arc::new(AtomicBool::new(false)),
                active_requests: Arc::new(AtomicUsize::new(0)),
                install_error: Mutex::new(None),
                check_pause: Mutex::new(None),
                download_pause: Mutex::new(None),
                install_pause: Mutex::new(None),
            })
        }

        fn phases(&self) -> Vec<Phase> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|view| view.phase)
                .collect()
        }
    }

    /// The environment the machine sees; the test keeps its own handle on the same counters.
    struct FakeEnvironment(Arc<Fake>);

    struct FakeLease(Arc<Fake>);
    impl Drop for FakeLease {
        fn drop(&mut self) {
            assert_eq!(self.0.active_requests.load(Ordering::SeqCst), 0);
            self.0.busy.store(false, Ordering::SeqCst);
        }
    }

    struct RequestLifetime(Arc<AtomicUsize>);
    impl RequestLifetime {
        fn new(counter: &Arc<AtomicUsize>) -> Self {
            counter.fetch_add(1, Ordering::SeqCst);
            Self(counter.clone())
        }
    }

    impl Drop for RequestLifetime {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Environment for FakeEnvironment {
        fn check(&self) -> Eventual<'_, Result<Option<Found>, String>> {
            self.0.checks.fetch_add(1, Ordering::Relaxed);
            let found = self.0.found.lock().unwrap().clone();
            let pause = lock(&self.0.check_pause).clone();
            let request = RequestLifetime::new(&self.0.active_requests);
            Box::pin(async move {
                let _request = request;
                if let Some(pause) = pause {
                    pause.notified().await;
                }
                found
            })
        }

        fn download(
            &self,
            _found: &Found,
            progress: Arc<dyn Fn(u8) + Send + Sync>,
        ) -> Eventual<'_, Result<Vec<u8>, String>> {
            self.0.downloads.fetch_add(1, Ordering::Relaxed);
            progress(50);
            let bytes = self.0.bytes.lock().unwrap().clone();
            let pause = lock(&self.0.download_pause).clone();
            let request = RequestLifetime::new(&self.0.active_requests);
            Box::pin(async move {
                let _request = request;
                if let Some(pause) = pause {
                    pause.notified().await;
                }
                bytes
            })
        }

        fn install(&self, _found: &Found, _bytes: &[u8]) -> Result<(), String> {
            self.0.installs.fetch_add(1, Ordering::Relaxed);
            if let Some(pause) = lock(&self.0.install_pause).clone() {
                pause.wait();
            }
            match lock(&self.0.install_error).clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }

        fn acquire_lease(&self, _installing: bool) -> Result<Box<dyn Send>, String> {
            if self.0.sharing.load(Ordering::SeqCst) {
                return Err(SHARING_HINT.to_owned());
            }
            self.0
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .map_err(|_| "Update work is already running.".to_owned())?;
            Ok(Box::new(FakeLease(self.0.clone())))
        }

        fn sharing_active(&self) -> bool {
            self.0.sharing.load(Ordering::Relaxed)
        }

        fn paired(&self) -> bool {
            self.0.paired.load(Ordering::Relaxed)
        }

        fn announce(&self, view: &UpdatesView) {
            self.0.events.lock().unwrap().push(view.clone());
        }
    }

    fn machine(automatic: bool, found: Result<Option<Found>, String>) -> (Arc<Updates>, Arc<Fake>) {
        let fake = Fake::new(found);
        let updates = Updates::with(
            None,
            true,
            UpdatesFile {
                version: FILE_VERSION,
                automatic,
            },
            Box::new(FakeEnvironment(fake.clone())),
        );
        (updates, fake)
    }

    fn build(version: &str) -> Result<Option<Found>, String> {
        Ok(Some(Found {
            version: version.to_owned(),
            notes: Some("Fixes.".to_owned()),
            update: None,
        }))
    }

    fn temporary_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("monhop-updates-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_keeps_automatic_updates_off() {
        let path = temporary_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(!UpdatesFile::load(&path).unwrap().automatic);
    }

    #[test]
    fn a_saved_preference_round_trips() {
        let path = temporary_path("roundtrip");
        let file = UpdatesFile {
            version: FILE_VERSION,
            automatic: false,
        };
        file.save(&path).unwrap();
        assert_eq!(UpdatesFile::load(&path).unwrap(), file);
    }

    #[test]
    fn an_unreadable_file_does_not_enable_automatic_updates() {
        let path = temporary_path("unreadable");
        std::fs::write(&path, br#"{"version":9,"automatic":false}"#).unwrap();
        assert!(UpdatesFile::load(&path).is_err());
        std::fs::write(&path, b"not json at all").unwrap();
        assert!(UpdatesFile::load(&path).is_err());
        let padding = "x".repeat(MAX_FILE_BYTES as usize + 1);
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"automatic":true,"p":"{padding}"}}"#),
        )
        .unwrap();
        assert!(UpdatesFile::load(&path).is_err());
        assert!(!UpdatesFile::default().automatic);
    }

    #[test]
    fn the_view_carries_exactly_the_keys_the_window_reads() {
        let (updates, _fake) = machine(true, Ok(None));
        let value = serde_json::to_value(updates.view()).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "automatic",
                "availableVersion",
                "buildCommit",
                "checkedSecondsAgo",
                "currentVersion",
                "host",
                "message",
                "notes",
                "phase",
                "progressPercent",
                "sharingActive",
            ]
        );
        assert_eq!(object["host"], "github.com");
        assert_eq!(object["automatic"], true);
        assert_eq!(object["message"], "");
        assert_eq!(object["availableVersion"], serde_json::Value::Null);
        assert_eq!(object["checkedSecondsAgo"], serde_json::Value::Null);
        assert_eq!(object["currentVersion"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn every_phase_serializes_to_the_string_the_window_expects() {
        for (phase, text) in [
            (Phase::Idle, "\"idle\""),
            (Phase::Checking, "\"checking\""),
            (Phase::UpToDate, "\"upToDate\""),
            (Phase::Available, "\"available\""),
            (Phase::Downloading, "\"downloading\""),
            (Phase::Ready, "\"ready\""),
            (Phase::Installing, "\"installing\""),
            (Phase::Failed, "\"failed\""),
        ] {
            assert_eq!(serde_json::to_string(&phase).unwrap(), text);
        }
    }

    #[tokio::test]
    async fn a_check_with_no_newer_build_ends_up_to_date() {
        let (updates, fake) = machine(true, Ok(None));
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::UpToDate);
        assert_eq!(fake.phases(), [Phase::Checking, Phase::UpToDate]);
        assert_eq!(fake.downloads.load(Ordering::Relaxed), 0);
        assert!(view.checked_seconds_ago.is_some());
    }

    #[tokio::test]
    async fn a_found_build_is_downloaded_and_waits_as_ready() {
        let (updates, fake) = machine(true, build("9.9.9"));
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::Ready);
        assert_eq!(view.available_version.as_deref(), Some("9.9.9"));
        assert_eq!(view.notes.as_deref(), Some("Fixes."));
        assert_eq!(view.progress_percent, None);
        assert_eq!(
            fake.phases(),
            [
                Phase::Checking,
                Phase::Available,
                Phase::Downloading,
                Phase::Downloading,
                Phase::Ready
            ]
        );
        assert_eq!(fake.downloads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_failed_check_says_what_the_user_can_do() {
        let (updates, fake) = machine(true, Err("github.com could not be reached.".to_owned()));
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::Failed);
        assert_eq!(view.message, "github.com could not be reached.");
        assert_eq!(fake.downloads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_failed_check_keeps_a_download_that_is_already_verified() {
        let (updates, fake) = machine(true, build("9.9.9"));
        assert_eq!(updates.check_now("test").await.phase, Phase::Ready);
        *fake.found.lock().unwrap() = Err("github.com could not be reached.".to_owned());
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::Ready);
        assert_eq!(view.message, "github.com could not be reached.");
        assert_eq!(view.available_version.as_deref(), Some("9.9.9"));
    }

    #[tokio::test]
    async fn a_failed_download_leaves_nothing_to_install() {
        let (updates, fake) = machine(true, build("9.9.9"));
        *fake.bytes.lock().unwrap() = Err("The build was not signed by MonHop.".to_owned());
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::Failed);
        assert_eq!(view.message, "The build was not signed by MonHop.");
        assert_eq!(
            updates.install_ready(false).await.err().as_deref(),
            Some(NOTHING_READY)
        );
    }

    #[tokio::test]
    async fn check_now_reaches_the_endpoint_once_even_with_automatic_off() {
        let (updates, fake) = machine(false, Ok(None));
        let view = updates.check_now("test").await;
        assert_eq!(view.phase, Phase::UpToDate);
        assert!(!view.automatic);
        assert_eq!(fake.checks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_schedule_reaches_nothing_while_automatic_is_off_or_nothing_is_paired() {
        let (updates, fake) = machine(false, Ok(None));
        updates.scheduled_check().await;
        assert_eq!(fake.checks.load(Ordering::Relaxed), 0);

        let (updates, fake) = machine(true, Ok(None));
        fake.paired.store(false, Ordering::Relaxed);
        updates.scheduled_check().await;
        assert_eq!(fake.checks.load(Ordering::Relaxed), 0);

        fake.paired.store(true, Ordering::Relaxed);
        updates.scheduled_check().await;
        assert_eq!(fake.checks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_ui_smoke_check_never_starts_a_check() {
        let fake = Fake::new(Ok(None));
        let updates = Updates::with(
            None,
            false,
            UpdatesFile::default(),
            Box::new(FakeEnvironment(fake.clone())),
        );
        assert_eq!(updates.request_check().phase, Phase::Idle);
        updates.check_now("test").await;
        assert_eq!(fake.checks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn installing_is_refused_while_a_sharing_session_runs() {
        let (updates, fake) = machine(true, build("9.9.9"));
        updates.check_now("test").await;
        fake.sharing.store(true, Ordering::Relaxed);

        assert_eq!(
            updates.install_ready(false).await.err().as_deref(),
            Some(SHARING_HINT)
        );
        let view = updates.note(SHARING_HINT);
        assert_eq!(view.phase, Phase::Ready);
        assert_eq!(view.message, SHARING_HINT);
        assert!(view.sharing_active);
        assert_eq!(fake.installs.load(Ordering::Relaxed), 0);

        assert!(updates.install_ready(true).await.is_err());
        fake.sharing.store(false, Ordering::Relaxed);
        updates.install_ready(true).await.unwrap();
        assert_eq!(fake.installs.load(Ordering::Relaxed), 1);
        assert_eq!(
            updates.install_ready(true).await.err().as_deref(),
            Some(NOTHING_READY)
        );
    }

    #[test]
    fn a_debug_endpoint_replaces_the_pinned_one_only_when_the_build_set_it() {
        assert!(endpoint_override(None).is_none());
        assert!(endpoint_override(Some("not a url")).is_none());
        let configured = endpoint_override(Some("https://127.0.0.1:8443/latest.json"));
        if cfg!(debug_assertions) {
            assert_eq!(
                configured.map(|url| url.to_string()).as_deref(),
                Some("https://127.0.0.1:8443/latest.json")
            );
        } else {
            assert!(configured.is_none());
        }
    }

    #[test]
    fn progress_and_announced_text_stay_inside_their_bounds() {
        assert_eq!(percent(0, 100), 0);
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(200, 100), 100);
        assert_eq!(percent(1, 0), 0);
        assert_eq!(bounded("abcdef", 3), "abc");
        assert_eq!(bounded(&"n".repeat(5000), MAX_NOTES_CHARS).len(), 2000);
    }
    async fn wait_until(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn old_preferences_do_not_count_as_explicit_opt_in() {
        let path = temporary_path("old-consent");
        std::fs::write(&path, br#"{"version":1,"automatic":true}"#).unwrap();
        assert!(UpdatesFile::load(&path).is_err());
        assert!(!UpdatesFile::default().automatic);
    }

    #[test]
    fn explicit_opt_in_round_trips_in_the_new_preference_format() {
        let path = temporary_path("explicit-consent");
        let (mut updates, _) = machine(false, Ok(None));
        Arc::get_mut(&mut updates).unwrap().path = Some(path.clone());
        assert!(updates.set_automatic(true).automatic);
        assert!(UpdatesFile::load(&path).unwrap().automatic);
        assert!(!updates.set_automatic(false).automatic);
        assert!(!UpdatesFile::load(&path).unwrap().automatic);
    }

    #[tokio::test]
    async fn sharing_blocks_both_manual_and_scheduled_network_requests() {
        let (updates, fake) = machine(true, build("9.9.9"));
        fake.sharing.store(true, Ordering::SeqCst);
        updates.request_check();
        updates.check_now("user").await;
        updates.scheduled_check().await;
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
        assert!(!fake.busy.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn check_and_download_keep_exclusive_ownership_until_completion() {
        let (updates, fake) = machine(true, build("9.9.9"));
        let check = Arc::new(tokio::sync::Notify::new());
        let download = Arc::new(tokio::sync::Notify::new());
        *lock(&fake.check_pause) = Some(check.clone());
        *lock(&fake.download_pause) = Some(download.clone());
        let worker = updates.clone();
        let task = tokio::spawn(async move { worker.check_now("user").await });
        wait_until(|| fake.checks.load(Ordering::SeqCst) == 1).await;
        assert!(fake.busy.load(Ordering::SeqCst));
        assert!(updates.environment.acquire_lease(false).is_err());
        updates.request_check();
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
        check.notify_one();
        wait_until(|| fake.downloads.load(Ordering::SeqCst) == 1).await;
        assert!(fake.busy.load(Ordering::SeqCst));
        assert!(updates.environment.acquire_lease(true).is_err());
        download.notify_one();
        assert_eq!(task.await.unwrap().phase, Phase::Ready);
        assert!(!fake.busy.load(Ordering::SeqCst));
        assert!(!updates.running());
    }

    #[tokio::test]
    async fn cancelled_check_releases_the_sharing_reservation() {
        let (updates, fake) = machine(true, build("9.9.9"));
        *lock(&fake.check_pause) = Some(Arc::new(tokio::sync::Notify::new()));
        let worker = updates.clone();
        let task = tokio::spawn(async move { worker.check_now("user").await });
        wait_until(|| fake.checks.load(Ordering::SeqCst) == 1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!fake.busy.load(Ordering::SeqCst));
        assert!(!updates.running());
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn quitting_cancels_stalled_checks_and_downloads_without_installing() {
        for intent in [ExitIntent::Quit, ExitIntent::Restart] {
            for downloading in [false, true] {
                let (updates, fake) = machine(true, build("9.9.9"));
                let pause = Arc::new(tokio::sync::Notify::new());
                let counter = if downloading {
                    *lock(&fake.download_pause) = Some(pause);
                    &fake.downloads
                } else {
                    *lock(&fake.check_pause) = Some(pause);
                    &fake.checks
                };
                let worker = updates.clone();
                let task = tokio::spawn(async move { worker.check_now("user").await });
                wait_until(|| counter.load(Ordering::SeqCst) == 1).await;
                assert!(updates.begin_shutdown(intent));
                assert!(!updates.begin_shutdown(intent));
                tokio::time::timeout(
                    Duration::from_secs(2),
                    updates.finish_update_before_exit(intent),
                )
                .await
                .unwrap();
                assert_eq!(task.await.unwrap().phase, Phase::Idle);
                assert_eq!(fake.active_requests.load(Ordering::SeqCst), 0);
                assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
                assert!(!fake.busy.load(Ordering::SeqCst));
                assert!(!updates.running());
            }
        }
    }

    #[tokio::test]
    async fn quit_cancellation_is_not_lost_before_the_network_task_starts() {
        let (updates, fake) = machine(true, build("9.9.9"));
        let (_, operation) = updates.enter_checking(false).unwrap();
        assert!(updates.begin_shutdown(ExitIntent::Quit));
        updates.clone().run_check("user", operation).await;
        updates.finish_update_before_exit(ExitIntent::Quit).await;
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
        assert!(!fake.busy.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_a_new_check_on_quit_preserves_the_verified_package() {
        let (updates, fake) = machine(true, build("9.9.9"));
        updates.check_now("user").await;
        *lock(&fake.check_pause) = Some(Arc::new(tokio::sync::Notify::new()));
        let worker = updates.clone();
        let task = tokio::spawn(async move { worker.check_now("user").await });
        wait_until(|| fake.checks.load(Ordering::SeqCst) == 2).await;
        assert!(updates.begin_shutdown(ExitIntent::Quit));
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.finish_update_before_exit(ExitIntent::Quit),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 1);
        assert_eq!(updates.view().phase, Phase::Idle);
    }

    #[test]
    fn unwritable_installations_explain_manual_recovery() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::ReadOnlyFilesystem,
        ] {
            let error = tauri_plugin_updater::Error::Io(io::Error::from(kind));
            assert!(install_error_message(&error).contains("replace it manually"));
        }
    }

    #[tokio::test]
    async fn install_failure_keeps_verified_bytes_and_does_not_latch_shutdown() {
        let (updates, fake) = machine(true, build("9.9.9"));
        updates.check_now("user").await;
        *lock(&fake.install_error) = Some("Cannot replace this app.".into());
        assert!(updates.install_ready(false).await.is_err());
        assert_eq!(updates.view().phase, Phase::Ready);
        assert!(lock(&updates.state).exit_intent.is_none());
        assert!(!fake.busy.load(Ordering::SeqCst));
        *lock(&fake.install_error) = None;
        updates.install_ready(false).await.unwrap();
        assert_eq!(fake.installs.load(Ordering::SeqCst), 2);
        assert_eq!(updates.view().phase, Phase::Idle);
    }

    #[tokio::test]
    async fn quit_waits_for_install_completion_and_never_starts_it_twice() {
        let (updates, fake) = machine(true, build("9.9.9"));
        updates.check_now("user").await;
        let pause = Arc::new(std::sync::Barrier::new(2));
        *lock(&fake.install_pause) = Some(pause.clone());
        let worker = updates.clone();
        let install = tokio::spawn(async move { worker.install_ready(false).await });
        wait_until(|| fake.installs.load(Ordering::SeqCst) == 1).await;
        assert!(updates.install_ready(false).await.is_err());
        let worker = updates.clone();
        assert!(updates.begin_shutdown(ExitIntent::Quit));
        let quit = tokio::spawn(async move {
            worker.finish_update_before_exit(ExitIntent::Quit).await;
        });
        tokio::task::yield_now().await;
        assert!(!quit.is_finished());
        assert!(fake.busy.load(Ordering::SeqCst));
        pause.wait();
        install.await.unwrap().unwrap();
        quit.await.unwrap();
        assert_eq!(fake.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_install_caller_cannot_release_an_active_installer() {
        let (updates, fake) = machine(true, build("9.9.9"));
        updates.check_now("user").await;
        let pause = Arc::new(std::sync::Barrier::new(2));
        *lock(&fake.install_pause) = Some(pause.clone());
        let worker = updates.clone();
        let install = tokio::spawn(async move { worker.install_ready(false).await });
        wait_until(|| fake.installs.load(Ordering::SeqCst) == 1).await;
        install.abort();
        assert!(install.await.unwrap_err().is_cancelled());
        assert!(updates.running());
        assert!(fake.busy.load(Ordering::SeqCst));
        pause.wait();
        wait_until(|| !updates.running()).await;
        assert!(!fake.busy.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn closing_refuses_new_update_networking() {
        let (updates, fake) = machine(true, Ok(None));
        assert!(updates.begin_shutdown(ExitIntent::Quit));
        updates.request_check();
        updates.scheduled_check().await;
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    }
}
