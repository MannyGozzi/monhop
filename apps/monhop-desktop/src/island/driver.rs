//! The island's feed: one thread, alive only while the island is on, that reads the app's views
//! when the clipboard changes and once a second otherwise (sharing has no change hook, and the
//! window polls it as often), runs the model, and hands the main thread an activity only when it
//! changed. While nothing shows the island, as on a Mac without a notch, it keeps no tick and wakes
//! only for a change.

use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use objc2::MainThreadMarker;
use tauri::{AppHandle, Manager};

use super::{
    feed::{self, Views},
    macos,
    model::{Activity, IslandInput, Model},
};
use crate::{clipboard::ClipboardHub, lifecycle::AppController};

const TICK: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Signal {
    stop: bool,
    woken: bool,
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
/// Each start gets a new number, so the main thread ignores a stopped feed's last activity.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The saved clipboard notices switch; outlives each feed.
static CLIPBOARD_NOTICES: AtomicBool = AtomicBool::new(false);

fn running() -> MutexGuard<'static, Option<Running>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shows the island and starts its feed. Starting it again is a no-op.
pub fn start(app: &AppHandle) {
    let mut running = running();
    if running.is_some() {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let shared = Arc::new(Shared {
        signal: Mutex::new(Signal::default()),
        wake: Condvar::new(),
    });
    let showing_shared = Arc::clone(&shared);
    let context = macos::Context {
        motion: macos::Motion::FollowSystem,
        showing: Box::new(move |shown| showing(&showing_shared, shown)),
    };
    // The window is created before the feed can post its first activity.
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

/// Clipboard copies show from the next snapshot on; only the transfers the clipboard already
/// reports are read, never the clipboard itself.
pub fn set_clipboard_notices(enabled: bool) {
    CLIPBOARD_NOTICES.store(enabled, Ordering::Relaxed);
    wake();
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

/// Waits for a change, or `deadline` unless resting; false once the feed should stop.
fn wait(shared: &Shared, deadline: Instant) -> bool {
    let mut signal = shared.signal();
    while !signal.stop && !signal.woken {
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
    signal.woken = false;
    !signal.stop
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
        clipboard: app
            .try_state::<Arc<ClipboardHub>>()
            .and_then(|hub| serde_json::to_value(hub.view()).ok()),
        computers: names.current(),
    };
    Some(feed::snapshot(
        &views,
        CLIPBOARD_NOTICES.load(Ordering::Relaxed),
    ))
}

fn run(app: AppHandle, shared: Arc<Shared>, generation: u64) {
    let mut model = Model::default();
    let mut names = Names::new(
        crate::sharing_setup_path(&app)
            .ok()
            .map(|path| path.with_file_name(crate::computers::LIST_FILE)),
    );
    let mut shown: Option<Option<Activity>> = None;
    loop {
        let now = Instant::now();
        let deadline = model
            .next_deadline()
            .map_or(now + TICK, |deadline| deadline.min(now + TICK));
        if !wait(&shared, deadline) {
            return;
        }
        let Some(input) = gather(&app, &mut names) else {
            continue;
        };
        let activity = model.update(&input, Instant::now());
        if shown.as_ref() == Some(&activity) {
            continue;
        }
        shown = Some(activity.clone());
        let _ = app.run_on_main_thread(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                macos::render(mtm, generation, activity);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
        })
    }

    #[test]
    fn an_island_off_screen_waits_for_a_change_instead_of_its_deadline() {
        let shared = shared();
        assert!(
            wait(&shared, Instant::now()),
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
        assert!(wait(&shared, start));
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
        assert!(wait(&shared, Instant::now()));
        assert!(
            shared.signal().resting,
            "a change does not bring it back on screen"
        );
        shared.signal().stop = true;
        assert!(!wait(&shared, Instant::now()));
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
