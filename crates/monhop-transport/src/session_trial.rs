//! Explicit, bounded guard for a current-process controlled input trial.

use crate::{
    session::{SessionFailure, SessionStartupFailure},
    session_clock::SessionClock,
    session_startup::STARTUP_DEADLINE,
};
use monhop_core::{Point, RevocationSignal};
use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// The input window is measured from the active phase, never from the press of Start.
const TRIAL_DURATION: Duration = Duration::from_secs(60);
/// Peer connect bound; the startup phase adds the control-plane readiness deadline on top.
const CONNECT_BOUND: Duration = Duration::from_secs(120);
const STARTUP_BOUND: Duration =
    Duration::from_secs(CONNECT_BOUND.as_secs() + STARTUP_DEADLINE.as_secs());
const WATCH_INTERVAL: Duration = Duration::from_millis(5);
/// Window-server samples are noisy, so only a run of confirmed misses ends an active trial.
const FOREGROUND_MISSES: u8 = 3;
const NOT_ACTIVE: u64 = u64::MAX;
const MAX_RECEIVER_BOUND: f64 = 1_000_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialAuthorizationError {
    UnsupportedPlatform,
    InvalidWindow,
    NotFocused,
    InvalidReceiverBounds,
    Clock,
    Watchdog,
}

impl fmt::Display for TrialAuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "Controlled tests run only on Windows and macOS.",
            Self::InvalidWindow => {
                "The test window could not be identified. Close it and open a new test."
            }
            Self::NotFocused => {
                "Click the test window to bring it to the front, then press Start again."
            }
            Self::InvalidReceiverBounds => {
                "The test area could not be measured. Make the test window larger, then press Start again."
            }
            Self::Clock => {
                "This computer's timer could not be read. Restart MonHop, then open a new test."
            }
            Self::Watchdog => {
                "The safety watchdog could not start. Restart MonHop, then open a new test."
            }
        })
    }
}

impl Error for TrialAuthorizationError {}

/// Why a trial ended, so the test window never blames the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialStopReason {
    FocusLost,
    InputWindowFinished,
    PeerNeverStarted,
    SystemShortcut,
}

impl TrialStopReason {
    pub fn message(self) -> &'static str {
        match self {
            Self::FocusLost => {
                "The test stopped because the window lost focus. Open a new test to try again."
            }
            Self::InputWindowFinished => {
                "The test finished its 60 second input window. Open a new test to try again."
            }
            Self::PeerNeverStarted => "The other computer did not start in time.",
            Self::SystemShortcut => {
                "The test stopped because a system shortcut was pressed. Open a new test to try again."
            }
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::FocusLost => 1,
            Self::InputWindowFinished => 2,
            Self::PeerNeverStarted => 3,
            Self::SystemShortcut => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::FocusLost),
            2 => Some(Self::InputWindowFinished),
            3 => Some(Self::PeerNeverStarted),
            4 => Some(Self::SystemShortcut),
            _ => None,
        }
    }
}

impl fmt::Display for TrialStopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrialPermission {
    /// Input may be delivered right now.
    Permitted,
    /// Startup phase with the window behind: readiness and capture wait, nothing is revoked.
    Waiting,
    Revoked,
}

impl TrialPermission {
    fn allows_input(self) -> bool {
        matches!(self, Self::Permitted)
    }

    fn is_live(self) -> bool {
        !matches!(self, Self::Revoked)
    }
}

/// One window-server reading. Unknown is the absence of an answer, never a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundSample {
    Front,
    Behind,
    Unknown,
}

#[derive(Clone)]
pub(crate) struct TrialInputGuard(Arc<TrialState>);

struct TrialState {
    origin: SessionClock,
    focused: Arc<AtomicBool>,
    revocation: RevocationSignal,
    stop: AtomicU8,
    foreground: AtomicBool,
    misses: AtomicU8,
    active_at_millis: AtomicU64,
    system_shortcut: AtomicBool,
    receiver_bounds: Option<(Point, Point)>,
    #[cfg(windows)]
    window: monhop_platform_windows::trial_window::TrialWindow,
    #[cfg(target_os = "macos")]
    window: monhop_platform_macos::trial_window::TrialWindow,
}

impl TrialInputGuard {
    pub(crate) fn allows_new_input(&self) -> bool {
        self.0.permission().allows_input()
    }

    /// True until the trial is revoked. A paused startup phase is still live.
    pub(crate) fn is_live(&self) -> bool {
        self.0.permission().is_live()
    }

    /// Starts the input window and makes every window and focus rule terminal.
    pub(crate) fn begin_active(&self) {
        self.0.begin_active();
    }

    pub(crate) fn revoke(&self) {
        self.0.revocation.revoke();
    }

    pub(crate) fn reject_system_shortcut(&self) {
        self.0.system_shortcut.store(true, Ordering::Release);
        revoke_with(
            &self.0.revocation,
            &self.0.stop,
            TrialStopReason::SystemShortcut,
        );
    }
}

impl TrialState {
    fn active_at(&self) -> Option<Duration> {
        match self.active_at_millis.load(Ordering::Acquire) {
            NOT_ACTIVE => None,
            millis => Some(Duration::from_millis(millis)),
        }
    }

    fn begin_active(&self) {
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(NOT_ACTIVE - 1);
        let _ = self.active_at_millis.compare_exchange(
            NOT_ACTIVE,
            now.min(NOT_ACTIVE - 1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn permission(&self) -> TrialPermission {
        self.latch(self.foreground.load(Ordering::Acquire))
    }

    fn watch(&self) -> TrialPermission {
        let foreground = self.observe();
        self.latch(foreground)
    }

    fn latch(&self, foreground: bool) -> TrialPermission {
        latch_conditions(
            &self.revocation,
            &self.stop,
            &self.focused,
            foreground,
            self.origin.elapsed(),
            self.active_at(),
        )
    }

    /// A confirmed front sample also clears a startup-phase focus loss: the window is back.
    fn observe(&self) -> bool {
        let sample = self.sample();
        let foreground = observe_foreground(sample, &self.misses, &self.foreground);
        if sample == ForegroundSample::Front && self.active_at().is_none() {
            self.focused.store(true, Ordering::Release);
        }
        foreground
    }

    fn sample(&self) -> ForegroundSample {
        #[cfg(windows)]
        {
            use monhop_platform_windows::trial_window::ForegroundSample as Native;
            match self.window.foreground_sample() {
                Native::Front => ForegroundSample::Front,
                Native::Behind => ForegroundSample::Behind,
                Native::Unknown => ForegroundSample::Unknown,
            }
        }
        #[cfg(target_os = "macos")]
        {
            use monhop_platform_macos::trial_window::ForegroundSample as Native;
            match self.window.foreground_sample() {
                Native::Front => ForegroundSample::Front,
                Native::Behind => ForegroundSample::Behind,
                Native::Unknown => ForegroundSample::Unknown,
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            ForegroundSample::Behind
        }
    }
}

/// The test window reads the same clock and phase the watchdog enforces.
#[derive(Clone)]
pub struct TrialProgress(Arc<TrialState>);

impl TrialProgress {
    /// The full window until the active phase begins; no countdown runs during startup.
    pub fn remaining_seconds(&self) -> u64 {
        self.0.active_at().map_or(TRIAL_DURATION.as_secs(), |at| {
            TRIAL_DURATION
                .as_secs()
                .saturating_sub(self.0.origin.elapsed().saturating_sub(at).as_secs())
        })
    }

    pub fn is_active(&self) -> bool {
        self.0.active_at().is_some()
    }

    /// Startup is paused until the test window is in front again.
    pub fn is_waiting_for_window(&self) -> bool {
        !self.0.revocation.is_revoked()
            && self.0.active_at().is_none()
            && !(self.0.focused.load(Ordering::Acquire)
                && self.0.foreground.load(Ordering::Acquire))
    }

    pub fn stop_reason(&self) -> Option<TrialStopReason> {
        TrialStopReason::from_code(self.0.stop.load(Ordering::Acquire))
    }
}

/// Owns an exact focused window, a monotonic deadline, and its permanent revocation signal.
pub struct TrialAuthorization {
    guard: TrialInputGuard,
    stop: mpsc::Sender<()>,
    watchdog: Option<JoinHandle<()>>,
}

impl TrialAuthorization {
    pub fn for_current_window(
        native_window_id: usize,
        focused: Arc<AtomicBool>,
        receiver_bounds: Option<(Point, Point)>,
    ) -> Result<Self, TrialAuthorizationError> {
        let _ = validate_receiver_bounds(receiver_bounds)?;
        if !focused.load(Ordering::Acquire) {
            return Err(TrialAuthorizationError::NotFocused);
        }
        #[cfg(windows)]
        let window = monhop_platform_windows::trial_window::TrialWindow::for_current_process(
            native_window_id,
        )
        .map_err(|_| TrialAuthorizationError::InvalidWindow)?;
        #[cfg(target_os = "macos")]
        let window =
            monhop_platform_macos::trial_window::TrialWindow::for_current_process(native_window_id)
                .map_err(|_| TrialAuthorizationError::InvalidWindow)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = native_window_id;
            return Err(TrialAuthorizationError::UnsupportedPlatform);
        }
        let guard = TrialInputGuard(Arc::new(TrialState {
            origin: SessionClock::try_now().map_err(|_| TrialAuthorizationError::Clock)?,
            focused,
            revocation: RevocationSignal::default(),
            stop: AtomicU8::new(0),
            // Admission already proved the window is in front.
            foreground: AtomicBool::new(true),
            misses: AtomicU8::new(0),
            active_at_millis: AtomicU64::new(NOT_ACTIVE),
            system_shortcut: AtomicBool::new(false),
            receiver_bounds,
            #[cfg(any(windows, target_os = "macos"))]
            window,
        }));
        if !guard.allows_new_input() {
            return Err(TrialAuthorizationError::NotFocused);
        }
        let (stop, receive) = mpsc::channel();
        let worker_guard = guard.clone();
        let watchdog = thread::Builder::new()
            .name("monhop-trial-watch".into())
            .spawn(move || {
                loop {
                    // A paused startup keeps watching; only revocation ends the watchdog.
                    if !worker_guard.0.watch().is_live() {
                        return;
                    }
                    match receive.recv_timeout(WATCH_INTERVAL) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                }
            })
            .map_err(|_| TrialAuthorizationError::Watchdog)?;
        Ok(Self {
            guard,
            stop,
            watchdog: Some(watchdog),
        })
    }

    pub fn revocation(&self) -> RevocationSignal {
        self.guard.0.revocation.clone()
    }

    /// A countdown and stop reason the test window can read after the authorization moves on.
    pub fn progress(&self) -> TrialProgress {
        TrialProgress(self.guard.0.clone())
    }

    /// Startup pauses are not failures, so this only rejects a revoked trial.
    pub(crate) fn require_active(&self) -> Result<(), SessionFailure> {
        self.guard
            .is_live()
            .then_some(())
            .ok_or(SessionFailure::Revoked)
    }

    pub(crate) fn input_guard(&self) -> TrialInputGuard {
        self.guard.clone()
    }

    pub(crate) fn receiver_bounds(&self) -> Option<(Point, Point)> {
        self.guard.0.receiver_bounds
    }

    pub(crate) fn rejected_system_shortcut(&self) -> bool {
        self.guard.0.system_shortcut.load(Ordering::Acquire)
    }

    #[cfg(windows)]
    pub(crate) fn source_window(&self) -> monhop_platform_windows::trial_window::TrialWindow {
        self.guard.0.window
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn source_window(&self) -> monhop_platform_macos::trial_window::TrialWindow {
        self.guard.0.window
    }

    pub(crate) fn link_external_revocation(
        &self,
        external: RevocationSignal,
    ) -> Result<TrialRevocationLink, SessionFailure> {
        let trial = self.revocation();
        if trial.is_revoked() || external.is_revoked() {
            trial.revoke();
            external.revoke();
            return Err(SessionFailure::Revoked);
        }
        let (stop, receive) = mpsc::channel();
        let worker_trial = trial.clone();
        let worker_external = external.clone();
        let worker = thread::Builder::new()
            .name("monhop-trial-revoke".into())
            .spawn(move || {
                loop {
                    if worker_trial.is_revoked() || worker_external.is_revoked() {
                        worker_trial.revoke();
                        worker_external.revoke();
                        return;
                    }
                    match receive.recv_timeout(WATCH_INTERVAL) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                }
            })
            .map_err(|_| {
                SessionFailure::Startup(SessionStartupFailure::RevocationWatchUnavailable)
            })?;
        Ok(TrialRevocationLink {
            trial,
            external,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for TrialAuthorization {
    fn drop(&mut self) {
        self.guard.0.revocation.revoke();
        let _ = self.stop.send(());
        if let Some(worker) = self.watchdog.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) struct TrialRevocationLink {
    trial: RevocationSignal,
    external: RevocationSignal,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for TrialRevocationLink {
    fn drop(&mut self) {
        if self.trial.is_revoked() || self.external.is_revoked() {
            self.trial.revoke();
            self.external.revoke();
        }
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn validate_receiver_bounds(
    receiver_bounds: Option<(Point, Point)>,
) -> Result<Option<(Point, Point)>, TrialAuthorizationError> {
    let Some((origin, size)) = receiver_bounds else {
        return Ok(None);
    };
    if !origin.is_finite()
        || !size.is_finite()
        || size.x <= 0.0
        || size.y <= 0.0
        || origin.x.abs() >= MAX_RECEIVER_BOUND
        || origin.y.abs() >= MAX_RECEIVER_BOUND
        || size.x >= MAX_RECEIVER_BOUND
        || size.y >= MAX_RECEIVER_BOUND
        || !(origin.x + size.x).is_finite()
        || !(origin.y + size.y).is_finite()
    {
        return Err(TrialAuthorizationError::InvalidReceiverBounds);
    }
    Ok(Some((origin, size)))
}

fn observe_foreground(
    sample: ForegroundSample,
    misses: &AtomicU8,
    foreground: &AtomicBool,
) -> bool {
    match sample {
        ForegroundSample::Front => {
            misses.store(0, Ordering::Release);
            foreground.store(true, Ordering::Release);
            true
        }
        ForegroundSample::Behind => {
            let seen = misses.load(Ordering::Acquire).saturating_add(1);
            misses.store(seen, Ordering::Release);
            if seen >= FOREGROUND_MISSES {
                foreground.store(false, Ordering::Release);
            }
            foreground.load(Ordering::Acquire)
        }
        // An unreadable window list says nothing about z-order, so the last verdict stands.
        ForegroundSample::Unknown => foreground.load(Ordering::Acquire),
    }
}

/// The phase boundary: until the active phase begins no input can reach the peer, so a window
/// that is behind or unfocused only pauses startup, and only the startup bound can end it. Once
/// the receiver is native-ready and both sides have exchanged readiness, the trial is active and
/// every focus loss, window loss, and the input window's own expiry is immediately terminal.
fn latch_conditions(
    revocation: &RevocationSignal,
    stop: &AtomicU8,
    focused: &AtomicBool,
    foreground: bool,
    elapsed: Duration,
    active_at: Option<Duration>,
) -> TrialPermission {
    if revocation.is_revoked() {
        return TrialPermission::Revoked;
    }
    let in_front = focused.load(Ordering::Acquire) && foreground;
    let Some(active_at) = active_at else {
        if elapsed >= STARTUP_BOUND {
            return revoke_with(revocation, stop, TrialStopReason::PeerNeverStarted);
        }
        return if in_front {
            TrialPermission::Permitted
        } else {
            TrialPermission::Waiting
        };
    };
    if elapsed.saturating_sub(active_at) >= TRIAL_DURATION {
        return revoke_with(revocation, stop, TrialStopReason::InputWindowFinished);
    }
    if !in_front {
        return revoke_with(revocation, stop, TrialStopReason::FocusLost);
    }
    TrialPermission::Permitted
}

fn revoke_with(
    revocation: &RevocationSignal,
    stop: &AtomicU8,
    reason: TrialStopReason,
) -> TrialPermission {
    let _ = stop.compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire);
    revocation.revoke();
    TrialPermission::Revoked
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        revocation: RevocationSignal,
        stop: AtomicU8,
        focused: AtomicBool,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                revocation: RevocationSignal::default(),
                stop: AtomicU8::new(0),
                focused: AtomicBool::new(true),
            }
        }

        fn latch(
            &self,
            foreground: bool,
            elapsed: Duration,
            active_at: Option<Duration>,
        ) -> TrialPermission {
            latch_conditions(
                &self.revocation,
                &self.stop,
                &self.focused,
                foreground,
                elapsed,
                active_at,
            )
        }

        fn reason(&self) -> Option<TrialStopReason> {
            TrialStopReason::from_code(self.stop.load(Ordering::Acquire))
        }
    }

    #[test]
    fn startup_focus_loss_pauses_and_blocks_readiness_without_revoking() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.latch(true, Duration::ZERO, None),
            TrialPermission::Permitted
        );
        fixture.focused.store(false, Ordering::Release);
        let paused = fixture.latch(true, Duration::from_secs(30), None);
        assert_eq!(paused, TrialPermission::Waiting);
        // Readiness and capture start use the same gate, so a pause holds both back.
        assert!(!paused.allows_input());
        assert!(paused.is_live());
        assert!(!fixture.revocation.is_revoked());
        assert_eq!(fixture.reason(), None);

        // A window behind the others pauses startup for the same reason.
        fixture.focused.store(true, Ordering::Release);
        assert_eq!(
            fixture.latch(false, Duration::from_secs(31), None),
            TrialPermission::Waiting
        );
        assert!(!fixture.revocation.is_revoked());

        // The pause ends by itself once the window is back in front.
        assert_eq!(
            fixture.latch(true, Duration::from_secs(32), None),
            TrialPermission::Permitted
        );
    }

    #[test]
    fn active_focus_and_foreground_loss_revoke_and_stay_revoked() {
        let fixture = Fixture::new();
        let active = Some(Duration::from_secs(10));
        assert_eq!(
            fixture.latch(true, Duration::from_secs(10), active),
            TrialPermission::Permitted
        );
        fixture.focused.store(false, Ordering::Release);
        assert_eq!(
            fixture.latch(true, Duration::from_secs(10), active),
            TrialPermission::Revoked
        );
        assert_eq!(fixture.reason(), Some(TrialStopReason::FocusLost));
        fixture.focused.store(true, Ordering::Release);
        assert_eq!(
            fixture.latch(true, Duration::from_secs(11), active),
            TrialPermission::Revoked
        );

        let fixture = Fixture::new();
        assert_eq!(
            fixture.latch(false, Duration::from_secs(10), active),
            TrialPermission::Revoked
        );
        assert_eq!(fixture.reason(), Some(TrialStopReason::FocusLost));
    }

    #[test]
    fn the_input_window_starts_at_the_active_phase_and_startup_has_its_own_bound() {
        let fixture = Fixture::new();
        // Long past sixty seconds of startup, the input window has not begun.
        assert_eq!(
            fixture.latch(true, TRIAL_DURATION + Duration::from_secs(30), None),
            TrialPermission::Permitted
        );
        let active = Some(TRIAL_DURATION);
        assert_eq!(
            fixture.latch(
                true,
                TRIAL_DURATION + TRIAL_DURATION - Duration::from_secs(1),
                active
            ),
            TrialPermission::Permitted
        );
        assert_eq!(
            fixture.latch(true, TRIAL_DURATION + TRIAL_DURATION, active),
            TrialPermission::Revoked
        );
        assert_eq!(fixture.reason(), Some(TrialStopReason::InputWindowFinished));

        let fixture = Fixture::new();
        assert_eq!(
            fixture.latch(true, STARTUP_BOUND - Duration::from_secs(1), None),
            TrialPermission::Permitted
        );
        assert_eq!(
            fixture.latch(true, STARTUP_BOUND, None),
            TrialPermission::Revoked
        );
        assert_eq!(fixture.reason(), Some(TrialStopReason::PeerNeverStarted));
        assert_eq!(
            TrialStopReason::PeerNeverStarted.message(),
            "The other computer did not start in time."
        );
        assert_eq!(STARTUP_BOUND, CONNECT_BOUND + STARTUP_DEADLINE);
    }

    #[test]
    fn unknown_samples_never_stand_in_for_a_missing_window() {
        let misses = AtomicU8::new(0);
        let foreground = AtomicBool::new(true);
        for _ in 0..3 {
            assert!(observe_foreground(
                ForegroundSample::Unknown,
                &misses,
                &foreground
            ));
        }
        assert_eq!(misses.load(Ordering::Acquire), 0);
        assert!(foreground.load(Ordering::Acquire));
    }

    #[test]
    fn three_consecutive_misses_are_required_before_the_window_counts_as_gone() {
        let misses = AtomicU8::new(0);
        let foreground = AtomicBool::new(true);
        assert!(observe_foreground(
            ForegroundSample::Behind,
            &misses,
            &foreground
        ));
        assert!(observe_foreground(
            ForegroundSample::Behind,
            &misses,
            &foreground
        ));
        // A confirmed front sample clears the run.
        assert!(observe_foreground(
            ForegroundSample::Front,
            &misses,
            &foreground
        ));
        assert_eq!(misses.load(Ordering::Acquire), 0);
        assert!(observe_foreground(
            ForegroundSample::Behind,
            &misses,
            &foreground
        ));
        assert!(observe_foreground(
            ForegroundSample::Behind,
            &misses,
            &foreground
        ));
        assert!(!observe_foreground(
            ForegroundSample::Behind,
            &misses,
            &foreground
        ));
        // Once decided, an unknown sample keeps the verdict rather than reviving the window.
        assert!(!observe_foreground(
            ForegroundSample::Unknown,
            &misses,
            &foreground
        ));
    }

    #[test]
    fn receiver_bounds_reject_nonfinite_or_empty_rectangles() {
        for bounds in [
            Some((Point::new(f64::NAN, 0.0), Point::new(1.0, 1.0))),
            Some((Point::default(), Point::new(0.0, 1.0))),
            Some((Point::default(), Point::new(1.0, -1.0))),
            Some((Point::new(MAX_RECEIVER_BOUND, 0.0), Point::new(1.0, 1.0))),
        ] {
            assert_eq!(
                validate_receiver_bounds(bounds),
                Err(TrialAuthorizationError::InvalidReceiverBounds)
            );
        }
        assert_eq!(validate_receiver_bounds(None), Ok(None));
    }

    #[test]
    fn every_authorization_failure_has_its_own_instruction() {
        let messages = [
            TrialAuthorizationError::UnsupportedPlatform,
            TrialAuthorizationError::InvalidWindow,
            TrialAuthorizationError::NotFocused,
            TrialAuthorizationError::InvalidReceiverBounds,
            TrialAuthorizationError::Clock,
            TrialAuthorizationError::Watchdog,
        ]
        .map(|error| error.to_string());
        for (index, message) in messages.iter().enumerate() {
            assert!(!message.is_empty());
            assert!(messages[index + 1..].iter().all(|other| other != message));
        }
    }
}
