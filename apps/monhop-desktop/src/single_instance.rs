//! One MonHop per signed-in session. The first copy holds a session-local named mutex (Windows) or
//! a file lock (macOS) for its whole life; a later launch asks that copy to show its window (at
//! login, only whether it runs) and leaves, unless the copy is quitting, as during an update, in
//! which case it waits to take over.

use crate::launch::Launch;

#[cfg(windows)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[cfg(any(windows, target_os = "macos", test))]
use std::time::Duration;
#[cfg(any(target_os = "macos", test))]
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::Path,
    time::Instant,
};

#[cfg(windows)]
use tauri::AppHandle;
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, HANDLE, WAIT_ABANDONED_0, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{
            CreateEventW, CreateMutexW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
            WaitForSingleObject,
        },
        UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow},
    },
    core::HSTRING,
};

/// `identifier` in tauri.conf.json; a test holds the two together.
const BUNDLE_ID: &str = "com.manuelgozzi.monhop";
#[cfg(any(windows, test))]
const MUTEX: &str = "instance";
/// Set by the running copy once it handled a request while running normally.
#[cfg(any(windows, test))]
const ANSWER: &str = "answered";

/// Long enough for a copy still starting to reach its answer, and for one quitting (drain and
/// installer launch, each bounded at 5 s) to let go of its claim.
#[cfg(any(windows, target_os = "macos"))]
const ANSWER_WAIT: Duration = Duration::from_secs(15);

/// In the data folder Tauri resolves for `BUNDLE_ID`, beside the logs.
#[cfg(target_os = "macos")]
const LOCK_FILE: &str = "instance.lock";
/// How often a later macOS launch retries the lock while a quitting copy lets go.
#[cfg(any(target_os = "macos", test))]
const LOCK_RETRY: Duration = Duration::from_millis(50);

/// Cleared once shutdown begins, so a relaunch during an update waits for the mutex instead of
/// leaving on an answer from a copy that is about to exit.
#[cfg(windows)]
static ANSWERING: AtomicBool = AtomicBool::new(true);

/// What a later launch asks of the running copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    /// Show and focus the main window.
    Show,
    /// Only confirm it runs: a login launch never brings the window up.
    Confirm,
}

impl Request {
    /// `--check-ui` is a diagnostic run that never shares, so it neither claims nor asks.
    fn for_launch(launch: Launch) -> Option<Self> {
        match launch {
            Launch::Normal => Some(Self::Show),
            Launch::Login => Some(Self::Confirm),
            Launch::CheckUi => None,
        }
    }

    #[cfg(any(windows, test))]
    fn object(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Confirm => "confirm",
        }
    }
}

/// How the wait of a launch that found the mutex held ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waited {
    /// The running copy handled the request while running normally.
    // Only the Windows copy answers.
    #[cfg_attr(not(windows), allow(dead_code))]
    Answered,
    /// The mutex came free: released, or abandoned by a copy that exited while quitting.
    Released,
    /// Neither within `ANSWER_WAIT`, or the request could not be made.
    Unanswered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// This copy holds the claim and answers later launches; `took_over` from a quitting copy.
    Owner { took_over: bool },
    /// No guard: `--check-ui`, or the claim could not be made, so this runs as before.
    Unguarded,
    /// Another copy runs: leave before anything starts.
    Exit,
}

/// A launch takes over only from a copy that let go of the mutex; one that answered, or stayed
/// silent past the bound, keeps running and this launch leaves.
fn decide(launch: Launch, waited: Waited) -> Start {
    match (Request::for_launch(launch), waited) {
        (None, _) => Start::Unguarded,
        (Some(_), Waited::Released) => Start::Owner { took_over: true },
        (Some(_), Waited::Answered | Waited::Unanswered) => Start::Exit,
    }
}

/// Runs first in `main`, before Tauri, hooks, hotkeys or sockets exist. The login agent starts its
/// copy straight from launchd, past LaunchServices' one-copy rule, so macOS needs this claim too.
#[cfg(target_os = "macos")]
pub fn claim(launch: Launch) -> Start {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Start::Unguarded;
    };
    let path = Path::new(&home)
        .join("Library/Application Support")
        .join(BUNDLE_ID)
        .join(LOCK_FILE);
    let (start, held) = claim_file(launch, &path, ANSWER_WAIT);
    // Never closed, so this copy holds the lock until the process ends, however it ends.
    std::mem::forget(held);
    start
}

/// The owner holds an exclusive lock on `path`; std opens it close-on-exec, so a relaunch never
/// inherits it. Nothing answers on macOS, so a later launch only waits for a quitting copy.
#[cfg(any(target_os = "macos", test))]
fn claim_file(launch: Launch, path: &Path, wait: Duration) -> (Start, Option<File>) {
    if Request::for_launch(launch).is_none() {
        return (Start::Unguarded, None);
    }
    let opened = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(path)
        });
    let Ok(file) = opened else {
        return (Start::Unguarded, None);
    };
    let deadline = Instant::now() + wait;
    let mut waited = false;
    loop {
        match file.try_lock() {
            Ok(()) if waited => return (decide(launch, Waited::Released), Some(file)),
            Ok(()) => return (Start::Owner { took_over: false }, Some(file)),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                waited = true;
                std::thread::sleep(LOCK_RETRY);
            }
            Err(TryLockError::WouldBlock) => return (decide(launch, Waited::Unanswered), None),
            Err(TryLockError::Error(_)) => return (Start::Unguarded, None),
        }
    }
}

/// LaunchServices answers a later normal launch with `RunEvent::Reopen`; this only logs the claim.
#[cfg(target_os = "macos")]
pub fn report(start: Start) {
    match start {
        Start::Owner { took_over: true } => {
            log::info!("instance: took over from a copy that was quitting");
        }
        Start::Unguarded => {
            log::warn!("instance: the lock is unavailable, so a second copy could start");
        }
        Start::Owner { took_over: false } | Start::Exit => {}
    }
}

/// `Local\` keeps each signed-in session's copy separate.
#[cfg(any(windows, test))]
fn object_name(object: &str) -> String {
    format!(r"Local\{BUNDLE_ID}.{object}")
}

/// Runs first in `main`, before COM, Tauri, hooks, hotkeys or sockets exist.
#[cfg(windows)]
pub fn claim(launch: Launch) -> Start {
    let Some(request) = Request::for_launch(launch) else {
        return Start::Unguarded;
    };
    // SAFETY: default security and a NUL-terminated name that outlives the call.
    let Ok(mutex) = (unsafe { CreateMutexW(None, false, &HSTRING::from(object_name(MUTEX))) })
    else {
        return Start::Unguarded;
    };
    // SAFETY: a zero wait on the handle just created. It is never closed, so the main thread
    // owns the mutex until the process ends.
    let tried = unsafe { WaitForSingleObject(mutex, 0) };
    if tried == WAIT_OBJECT_0 || tried == WAIT_ABANDONED_0 {
        return Start::Owner { took_over: false };
    }
    if tried != WAIT_TIMEOUT {
        return Start::Unguarded;
    }
    decide(launch, ask(mutex, request))
}

#[cfg(windows)]
fn ask(mutex: HANDLE, request: Request) -> Waited {
    let (Ok(asked), Ok(answer)) = (Event::open(request.object()), Event::open(ANSWER)) else {
        return Waited::Unanswered;
    };
    if request == Request::Show {
        // SAFETY: no pointers. The user just launched this copy, so it may pass the foreground on.
        let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }
    // SAFETY: both events are open. Clearing first drops an answer no launch waited for.
    if unsafe { ResetEvent(answer.0).and_then(|()| SetEvent(asked.0)) }.is_err() {
        return Waited::Unanswered;
    }
    // SAFETY: both handles stay open for the bounded wait; the answer is checked first.
    waited(unsafe { WaitForMultipleObjects(&[answer.0, mutex], false, wait_millis()) })
}

/// Maps the wait over `[answer, mutex]`.
#[cfg(windows)]
fn waited(woken: WAIT_EVENT) -> Waited {
    if woken == WAIT_OBJECT_0 {
        Waited::Answered
    } else if woken.0 == WAIT_OBJECT_0.0 + 1 || woken.0 == WAIT_ABANDONED_0.0 + 1 {
        Waited::Released
    } else {
        Waited::Unanswered
    }
}

#[cfg(windows)]
fn wait_millis() -> u32 {
    u32::try_from(ANSWER_WAIT.as_millis()).unwrap_or(u32::MAX)
}

/// Called as shutdown begins; from then on a later launch waits for this copy to let go.
#[cfg(windows)]
pub fn stop_answering() {
    ANSWERING.store(false, Ordering::Release);
}

/// Starts answering later launches once the main window exists.
#[cfg(windows)]
pub fn answer_later_launches(app: &AppHandle, start: Start) {
    match start {
        Start::Owner { took_over } => {
            if took_over {
                log::info!("instance: took over from a copy that was quitting");
            }
            let app = app.clone();
            if std::thread::Builder::new()
                .name("monhop-instance".into())
                .spawn(move || listen(&app))
                .is_err()
            {
                log::warn!("instance: later launches will not be answered");
            }
        }
        Start::Unguarded => {
            log::warn!("instance: the mutex is unavailable, so a second copy could start");
        }
        Start::Exit => {}
    }
}

#[cfg(windows)]
fn listen(app: &AppHandle) {
    let (Ok(show), Ok(confirm), Ok(answer)) = (
        Event::open(Request::Show.object()),
        Event::open(Request::Confirm.object()),
        Event::open(ANSWER),
    ) else {
        log::warn!("instance: later launches will not be answered");
        return;
    };
    loop {
        // SAFETY: both events stay open for the loop's life.
        let woken = unsafe { WaitForMultipleObjects(&[show.0, confirm.0], false, INFINITE) };
        let request = if woken == WAIT_OBJECT_0 {
            Request::Show
        } else if woken.0 == WAIT_OBJECT_0.0 + 1 {
            Request::Confirm
        } else {
            log::warn!("instance: stopped answering later launches");
            return;
        };
        if !ANSWERING.load(Ordering::Acquire) {
            continue;
        }
        // The answer follows the main thread's own reply, so a stalled or ending loop never answers.
        let (handled, reply) = mpsc::channel();
        let handle = app.clone();
        let scheduled = app.run_on_main_thread(move || {
            if request == Request::Show
                && ANSWERING.load(Ordering::Acquire)
                && let Err(message) = crate::tray::show_main_window(&handle)
            {
                log::warn!("instance: could not show the window: {message}");
            }
            let _ = handled.send(());
        });
        if scheduled.is_ok()
            && reply.recv_timeout(ANSWER_WAIT).is_ok()
            && ANSWERING.load(Ordering::Acquire)
        {
            // SAFETY: the event stays open for the loop's life.
            let _ = unsafe { SetEvent(answer.0) };
        }
    }
}

/// An auto-reset event shared by name with the other copies; closed on drop.
#[cfg(windows)]
struct Event(HANDLE);

#[cfg(windows)]
impl Event {
    fn open(object: &str) -> windows::core::Result<Self> {
        // SAFETY: default security and a NUL-terminated name that outlives the call.
        unsafe { CreateEventW(None, false, false, &HSTRING::from(object_name(object))) }.map(Self)
    }
}

#[cfg(windows)]
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this owner holds the only copy of the handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_derive_from_the_bundle_id_in_the_session_namespace() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["identifier"], BUNDLE_ID);
        assert_eq!(object_name(MUTEX), r"Local\com.manuelgozzi.monhop.instance");
        assert_eq!(
            object_name(Request::Show.object()),
            r"Local\com.manuelgozzi.monhop.show"
        );
        assert_eq!(
            object_name(Request::Confirm.object()),
            r"Local\com.manuelgozzi.monhop.confirm"
        );
        assert_eq!(
            object_name(ANSWER),
            r"Local\com.manuelgozzi.monhop.answered"
        );
    }

    #[test]
    fn only_a_login_launch_asks_without_showing_and_check_ui_never_asks() {
        assert_eq!(Request::for_launch(Launch::Normal), Some(Request::Show));
        assert_eq!(Request::for_launch(Launch::Login), Some(Request::Confirm));
        assert_eq!(Request::for_launch(Launch::CheckUi), None);
    }

    #[test]
    fn an_answer_or_silence_leaves_and_only_a_released_mutex_takes_over() {
        for launch in [Launch::Normal, Launch::Login] {
            assert_eq!(decide(launch, Waited::Answered), Start::Exit);
            assert_eq!(decide(launch, Waited::Unanswered), Start::Exit);
            assert_eq!(
                decide(launch, Waited::Released),
                Start::Owner { took_over: true }
            );
        }
    }

    fn lock_path(test: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("monhop-instance-{test}-{}", std::process::id()))
            .join("instance.lock")
    }

    #[test]
    fn a_later_copy_leaves_while_the_first_holds_the_lock() {
        let path = lock_path("held");
        let (first, held) = claim_file(Launch::Normal, &path, Duration::ZERO);
        let wait = Duration::from_millis(120);
        let later = [Launch::Login, Launch::Normal].map(|launch| claim_file(launch, &path, wait).0);
        assert_eq!(
            (first, later),
            (
                Start::Owner { took_over: false },
                [Start::Exit, Start::Exit]
            )
        );
        assert!(held.is_some());
        assert_eq!(
            claim_file(Launch::CheckUi, &path, Duration::ZERO).0,
            Start::Unguarded
        );
        drop(held);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_later_copy_takes_over_once_the_quitting_copy_lets_go() {
        let path = lock_path("quitting");
        let (first, held) = claim_file(Launch::Login, &path, Duration::ZERO);
        assert_eq!(first, Start::Owner { took_over: false });
        let quitting = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });
        let (next, _held) = claim_file(Launch::Login, &path, Duration::from_secs(10));
        quitting.join().unwrap();
        assert_eq!(next, Start::Owner { took_over: true });
        assert_eq!(
            claim_file(Launch::Normal, &path, Duration::ZERO).0,
            Start::Exit
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn check_ui_runs_unguarded_whatever_it_would_have_seen() {
        for waited in [Waited::Answered, Waited::Released, Waited::Unanswered] {
            assert_eq!(decide(Launch::CheckUi, waited), Start::Unguarded);
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_answer_outranks_the_mutex_and_abandonment_counts_as_release() {
        use windows::Win32::Foundation::WAIT_FAILED;

        assert_eq!(waited(WAIT_OBJECT_0), Waited::Answered);
        assert_eq!(waited(WAIT_EVENT(WAIT_OBJECT_0.0 + 1)), Waited::Released);
        assert_eq!(waited(WAIT_EVENT(WAIT_ABANDONED_0.0 + 1)), Waited::Released);
        assert_eq!(waited(WAIT_TIMEOUT), Waited::Unanswered);
        assert_eq!(waited(WAIT_FAILED), Waited::Unanswered);
        assert_eq!(u128::from(wait_millis()), ANSWER_WAIT.as_millis());
    }
}
