//! One MonHop per Windows session. The first copy holds a session-local named mutex for its whole
//! life; a later launch asks that copy to show its window (at login, only whether it runs) and
//! leaves, unless the copy is quitting, as during an update, in which case it waits to take over.

use crate::launch::Launch;

#[cfg(windows)]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
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
const MUTEX: &str = "instance";
/// Set by the running copy once it handled a request while running normally.
const ANSWER: &str = "answered";

/// Long enough for a copy still starting to reach its answer, and for one quitting (drain and
/// installer launch, each bounded at 5 s) to let go of the mutex.
#[cfg(windows)]
const ANSWER_WAIT: Duration = Duration::from_secs(15);

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
    Answered,
    /// The mutex came free: released, or abandoned by a copy that exited while quitting.
    Released,
    /// Neither within `ANSWER_WAIT`, or the request could not be made.
    Unanswered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// This copy holds the mutex and answers later launches; `took_over` from a quitting copy.
    Owner { took_over: bool },
    /// No guard: `--check-ui`, or the mutex could not be made, so this runs as before.
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

/// `Local\` keeps each signed-in session's copy separate.
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
