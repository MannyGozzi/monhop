//! The island's feed: one thread, alive only while the island is on, that reads the app's views
//! when something changes (or on a short tick while a session shares input), runs the model, and
//! hands the main thread a presentation only when it changed. While nothing shows the island, as on
//! a Mac without a notch, it keeps no tick and wakes only for a change.

use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use objc2::MainThreadMarker;
use tauri::{AppHandle, Manager};

use super::{
    feed::{self, Views},
    macos::{self, Action},
    model::{IslandInput, Model, Phase, Presentation},
};
use crate::{
    clipboard::ClipboardHub, dimming::Dimming, lifecycle::AppController, updates::Updates,
};

/// Follows where the pointer is while a session shares input, the only state in which it moves
/// between computers without a change hook; otherwise checks in now and then.
const LIVE_TICK: Duration = Duration::from_millis(100);
const IDLE_TICK: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Signal {
    stop: bool,
    woken: bool,
    hovering: Option<bool>,
    /// The island is off screen: no tick and no deadline wakes the feed.
    resting: bool,
}

struct Shared {
    signal: Mutex<Signal>,
    wake: Condvar,
}

impl Shared {
    fn signal(&self) -> MutexGuard<'_, Signal> {
        self.signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Running {
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
/// Each start gets a new number, so the main thread ignores a stopped feed's last presentation.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The update version already announced; outlives each feed so turning the island off and on
/// does not announce it again.
static ANNOUNCED: Mutex<Option<String>> = Mutex::new(None);

fn running() -> MutexGuard<'static, Option<Running>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn announced() -> MutexGuard<'static, Option<String>> {
    ANNOUNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shows the island and starts its feed. Starting it again is a no-op.
pub fn start(app: &AppHandle) {
    let mut running = running();
    if running.is_some() {
        return;
    }
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let shared = Arc::new(Shared {
        signal: Mutex::new(Signal::default()),
        wake: Condvar::new(),
    });
    let context = context(app, &shared);
    // The window is created before the feed can post its first presentation.
    let _ = app.run_on_main_thread(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            macos::start(mtm, generation, context);
        }
    });
    let feed_app = app.clone();
    let feed_shared = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("monhop-island".into())
        .spawn(move || run(feed_app, feed_shared, generation));
    match thread {
        Ok(thread) => {
            log::info!("island: on");
            *running = Some(Running { shared, thread });
        }
        Err(error) => {
            log::warn!("island: the feed could not start: {error}");
            let _ = app.run_on_main_thread(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    macos::stop(mtm);
                }
            });
        }
    }
}

/// Stops the feed, then removes the island; nothing of it is left running.
pub fn stop(app: &AppHandle) {
    let Some(stopped) = running().take() else {
        return;
    };
    stopped.shared.signal().stop = true;
    stopped.shared.wake.notify_all();
    let _ = stopped.thread.join();
    let _ = app.run_on_main_thread(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            macos::stop(mtm);
        }
    });
    log::info!("island: off");
}

/// A change hook: the feed reads the app's views again now.
pub fn wake() {
    if let Some(running) = running().as_ref() {
        running.shared.signal().woken = true;
        running.shared.wake.notify_all();
    }
}

fn hovered(shared: &Shared, hovering: bool) {
    shared.signal().hovering = Some(hovering);
    shared.wake.notify_all();
}

/// Coming back on screen reads the app's views at once, since nothing was ticking meanwhile.
fn showing(shared: &Shared, showing: bool) {
    let resting = !showing;
    let mut signal = shared.signal();
    if signal.resting == resting {
        return;
    }
    signal.resting = resting;
    signal.woken |= showing;
    drop(signal);
    shared.wake.notify_all();
}

/// Waits for a change, a hover, or `deadline` unless resting; returns the hover to apply, or None
/// once the feed should stop.
fn wait(shared: &Shared, deadline: Instant) -> Option<Option<bool>> {
    let mut signal = shared.signal();
    while !signal.stop && !signal.woken && signal.hovering.is_none() {
        if signal.resting {
            signal = shared
                .wake
                .wait(signal)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        signal = shared
            .wake
            .wait_timeout(signal, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    if signal.stop {
        return None;
    }
    signal.woken = false;
    Some(signal.hovering.take())
}

/// What the main thread may ask of the feed: hover changes and the island's own controls.
fn context(app: &AppHandle, shared: &Arc<Shared>) -> macos::Context {
    let hover_shared = Arc::clone(shared);
    let showing_shared = Arc::clone(shared);
    let action_app = app.clone();
    macos::Context {
        interactive: true,
        motion: macos::Motion::FollowSystem,
        hover: Box::new(move |hovering| hovered(&hover_shared, hovering)),
        act: Box::new(move |action| perform(&action_app, action)),
        showing: Box::new(move |shown| showing(&showing_shared, shown)),
    }
}

/// Runs a control's change the way the window's own command does, then wakes the feed. Work that
/// saves or waits leaves the main thread.
fn perform(app: &AppHandle, action: Action) {
    match action {
        Action::OpenWindow => {
            if let Err(message) = crate::tray::show_main_window(app) {
                log::warn!("island: the window did not open: {message}");
            }
        }
        Action::PreviewLevel(level) => {
            if let Some(dimming) = app.try_state::<Arc<Dimming>>() {
                let _ = dimming.preview_level(app, level);
            }
        }
        action => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(message) = perform_blocking(&app, action) {
                    log::warn!("island: a control did not apply: {message}");
                }
                wake();
            });
        }
    }
}

fn perform_blocking(app: &AppHandle, action: Action) -> Result<(), String> {
    match action {
        Action::SetComputer {
            fingerprint,
            enabled,
        } => {
            let controller = app.state::<Arc<AppController>>().inner().clone();
            let interface = saved_interface(app);
            controller
                .set_enabled(&fingerprint, enabled, interface.as_deref())
                .map(|_| ())
        }
        Action::ToggleDimming => app.state::<Arc<Dimming>>().toggle(app).map(|_| ()),
        Action::SetLevel(level) => app
            .state::<Arc<Dimming>>()
            .set_level(app, level)
            .map(|_| ()),
        Action::SetClipboard(enabled) => {
            if let Some(hub) = app.try_state::<Arc<ClipboardHub>>() {
                hub.set_enabled(enabled);
            }
            Ok(())
        }
        Action::OpenWindow | Action::PreviewLevel(_) => Ok(()),
    }
}

/// The network interface the setup chose, as the window passes it with a computer's switch.
fn saved_interface(app: &AppHandle) -> Option<String> {
    let path = crate::sharing_setup_path(app).ok()?;
    let file = crate::sharing_preferences::SetupFile::load(&path).ok()?;
    file.interface_id().map(str::to_owned)
}

/// When the names file last changed, and how long it was; None while it is missing.
type Stamp = Option<(Option<SystemTime>, u64)>;

/// The saved computer names, read again only when the file changes.
struct Names {
    path: Option<PathBuf>,
    /// The file's stamp at the last read; None before the first.
    read: Option<Stamp>,
    value: serde_json::Value,
}

impl Names {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            read: None,
            value: serde_json::Value::Null,
        }
    }

    fn current(&mut self) -> serde_json::Value {
        let stamp = self.path.as_deref().and_then(|path| {
            let metadata = std::fs::metadata(path).ok()?;
            Some((metadata.modified().ok(), metadata.len()))
        });
        if self.read != Some(stamp) {
            self.read = Some(stamp);
            self.value = self
                .path
                .as_deref()
                .and_then(|path| crate::computers::ComputerList::load(path).ok())
                .and_then(|list| serde_json::to_value(list).ok())
                .unwrap_or(serde_json::Value::Null);
        }
        self.value.clone()
    }
}

fn gather(app: &AppHandle, names: &mut Names) -> Option<IslandInput> {
    let controller = app.try_state::<Arc<AppController>>()?;
    let views = Views {
        sharing: serde_json::to_value(controller.sharing.status()).ok()?,
        dimming: serde_json::to_value(app.try_state::<Arc<Dimming>>()?.view()).ok()?,
        clipboard: app
            .try_state::<Arc<ClipboardHub>>()
            .and_then(|hub| serde_json::to_value(hub.view()).ok()),
        updates: serde_json::to_value(app.try_state::<Arc<Updates>>()?.view()).ok()?,
        computers: names.current(),
    };
    feed::snapshot(&views)
}

fn tick_for(input: &IslandInput) -> Duration {
    if input
        .computers
        .iter()
        .any(|computer| computer.phase == Phase::Sharing)
    {
        LIVE_TICK
    } else {
        IDLE_TICK
    }
}

fn run(app: AppHandle, shared: Arc<Shared>, generation: u64) {
    let mut remembered = announced().clone();
    let mut model = Model::remembering(remembered.clone());
    let mut names = Names::new(
        crate::sharing_setup_path(&app)
            .ok()
            .map(|path| path.with_file_name(crate::computers::LIST_FILE)),
    );
    let mut shown: Option<Presentation> = None;
    let mut tick = IDLE_TICK;
    loop {
        let now = Instant::now();
        let deadline = model
            .next_deadline()
            .map_or(now + tick, |deadline| deadline.min(now + tick));
        let Some(hovering) = wait(&shared, deadline) else {
            return;
        };
        let now = Instant::now();
        if let Some(hovering) = hovering {
            model.set_hovering(hovering, now);
        }
        let Some(input) = gather(&app, &mut names) else {
            continue;
        };
        tick = tick_for(&input);
        let presentation = model.update(&input, now);
        if model.update_shown() != remembered.as_deref() {
            remembered = model.update_shown().map(str::to_owned);
            *announced() = remembered.clone();
        }
        if shown.as_ref() == Some(&presentation) {
            continue;
        }
        shown = Some(presentation.clone());
        let _ = app.run_on_main_thread(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                macos::render(mtm, generation, presentation);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::model::{ComputerInput, DimmingInput, Platform};

    fn input(phases: &[Phase]) -> IslandInput {
        IslandInput {
            computers: phases
                .iter()
                .enumerate()
                .map(|(index, phase)| ComputerInput {
                    fingerprint: format!("pc{index}"),
                    name: format!("PC {index}"),
                    platform: Platform::Windows,
                    enabled: true,
                    phase: *phase,
                    held: false,
                    pointer_here: None,
                    tone: index,
                })
                .collect(),
            paused: false,
            phase: phases.first().copied().unwrap_or(Phase::Off),
            sharing_active: false,
            held: false,
            dimming: DimmingInput {
                dimmed: false,
                level: 50,
                min_level: 10,
                max_level: 99,
            },
            clipboard: None,
            update_ready: None,
        }
    }

    #[test]
    fn only_a_sharing_session_ticks_fast() {
        assert_eq!(tick_for(&input(&[])), IDLE_TICK);
        for phase in [
            Phase::Off,
            Phase::Connecting,
            Phase::Connected,
            Phase::Starting,
            Phase::Reconnecting,
            Phase::Stopping,
            Phase::Error,
            Phase::NotPaired,
        ] {
            assert_eq!(tick_for(&input(&[phase])), IDLE_TICK, "{phase:?}");
        }
        assert_eq!(tick_for(&input(&[Phase::Sharing])), LIVE_TICK);
        assert_eq!(
            tick_for(&input(&[Phase::Connecting, Phase::Sharing])),
            LIVE_TICK
        );
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
        })
    }

    #[test]
    fn an_island_off_screen_waits_for_a_change_instead_of_its_deadline() {
        let shared = shared();
        assert_eq!(
            wait(&shared, Instant::now()),
            Some(None),
            "on screen, a deadline wakes it"
        );
        showing(&shared, false);
        let back = Duration::from_millis(60);
        let shower = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                std::thread::sleep(back);
                showing(&shared, true);
            })
        };
        let start = Instant::now();
        assert_eq!(wait(&shared, start), Some(None));
        assert!(
            start.elapsed() >= back,
            "the passed deadline did not wake it"
        );
        shower.join().unwrap();
        let signal = shared.signal();
        assert!(!signal.resting && !signal.woken);
        drop(signal);
        showing(&shared, true);
        assert!(!shared.signal().woken, "staying on screen is no change");
    }

    #[test]
    fn a_change_or_stop_still_reaches_an_island_off_screen() {
        let shared = shared();
        showing(&shared, false);
        assert!(!shared.signal().woken, "leaving the screen reads nothing");
        shared.signal().woken = true;
        assert_eq!(wait(&shared, Instant::now()), Some(None));
        assert!(
            shared.signal().resting,
            "a change does not bring it back on screen"
        );
        shared.signal().stop = true;
        assert_eq!(wait(&shared, Instant::now()), None);
    }

    #[test]
    fn the_names_file_is_read_again_only_when_it_changes() {
        let directory =
            std::env::temp_dir().join(format!("monhop-island-names-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(crate::computers::LIST_FILE);
        let listed = br#"{"version":1,"computers":[]}"#;
        std::fs::write(&path, listed).unwrap();
        let mut names = Names::new(Some(path.clone()));
        let first = names.current();
        assert!(first.is_object());
        // The same length and time: the damaged bytes are never read.
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, vec![b'x'; listed.len()]).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(written).unwrap();
        assert_eq!(names.current(), first);
        file.set_modified(written + Duration::from_secs(1)).unwrap();
        assert_eq!(names.current(), serde_json::Value::Null);
        drop(file);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
