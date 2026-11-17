//! Voice priority for one endpoint's datagrams, kept only while it is shown to work.
//!
//! Wi-Fi queues datagrams marked as voice ahead of bulk traffic, but an access point can stop
//! delivering voice frames to a station while ordinary ones still arrive. So a socket starts at
//! normal priority, marks only while a share session runs past startup with nothing connecting,
//! drops the mark at any hold, and gives it up for its life when a hold follows marking closely.

use std::{
    io,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::session_health::{VOICE_PROOF, VOICE_RETRY_AFTER};

/// `LinkPriority::recheck` value: nothing can move the policy until a change arrives.
const SETTLED: u64 = u64::MAX;
/// `LinkPriority::recheck` value: compare the mark with the policy before the next send.
const DUE: u64 = 0;

/// Why an endpoint's datagrams go at normal priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Normal {
    /// No share session runs past startup.
    Idle,
    /// A member is dialed or awaited without a live session, a dial-back runs, or a session is
    /// still starting.
    Connecting,
    /// A session held less than `VOICE_RETRY_AFTER` ago.
    AfterHold,
    /// A hold followed marking within `VOICE_PROOF`, or the system refused the mark.
    Refused,
}

/// What decides the mark, given the time; it never reads a clock itself.
#[derive(Default)]
pub(crate) struct PriorityPolicy {
    running: usize,
    starting: usize,
    connecting: bool,
    dialing_back: usize,
    /// The latest hold plus `VOICE_RETRY_AFTER`.
    quiet_until: Option<Instant>,
    /// When the mark was last applied.
    marked_at: Option<Instant>,
    refused: bool,
}

impl PriorityPolicy {
    /// None while datagrams should go marked as voice.
    pub(crate) fn normal(&self, now: Instant) -> Option<Normal> {
        if self.refused {
            Some(Normal::Refused)
        } else if self.running == 0 {
            Some(Normal::Idle)
        } else if self.connecting || self.starting > 0 || self.dialing_back > 0 {
            Some(Normal::Connecting)
        } else if self.quiet_until.is_some_and(|until| now < until) {
            Some(Normal::AfterHold)
        } else {
            None
        }
    }

    /// When the answer can change with no event: the end of the quiet after a hold.
    fn next_check(&self, now: Instant) -> Option<Instant> {
        self.quiet_until.filter(|until| *until > now)
    }

    /// True when this hold came so soon after marking that marking ends for good.
    fn hold(&mut self, now: Instant) -> bool {
        self.quiet_until = Some(now + VOICE_RETRY_AFTER);
        let unanswered = !self.refused
            && self
                .marked_at
                .is_some_and(|at| now.saturating_duration_since(at) <= VOICE_PROOF);
        self.refused |= unanswered;
        unanswered
    }
}

struct State {
    policy: PriorityPolicy,
    /// What the socket carries now.
    marked: bool,
    /// The end of marking was logged.
    told_refused: bool,
}

impl State {
    fn tell_normal(&mut self, normal: Normal) {
        match normal {
            Normal::Idle => log::info!("link priority: normal; no session is running"),
            Normal::Connecting => log::info!("link priority: normal while connecting"),
            Normal::AfterHold => log::info!("link priority: normal after a hold"),
            Normal::Refused if self.told_refused => log::info!("link priority: normal"),
            Normal::Refused => self.tell_refused(),
        }
    }

    fn tell_refused(&mut self) {
        self.told_refused = true;
        log::info!("link priority: normal for this network; marked datagrams went unanswered");
    }
}

/// One endpoint's voice priority. Sessions and dials change the policy under a short lock; the
/// socket applies the mark itself before a send, and only when the policy may have moved.
pub(crate) struct LinkPriority {
    origin: Instant,
    state: Mutex<State>,
    /// When the socket next compares its mark with the policy, in nanoseconds after `origin`.
    recheck: AtomicU64,
}

impl LinkPriority {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: Instant::now(),
            state: Mutex::new(State {
                policy: PriorityPolicy::default(),
                marked: false,
                told_refused: false,
            }),
            recheck: AtomicU64::new(SETTLED),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self, update: impl FnOnce(&mut State)) {
        let mut state = self.state();
        update(&mut state);
        self.recheck.store(DUE, Ordering::Release);
    }

    /// A share session's part, at normal priority until it starts.
    pub(crate) fn session(self: &Arc<Self>) -> SessionPriority {
        self.change(|state| state.policy.starting += 1);
        SessionPriority {
            control: Arc::clone(self),
            running: false,
            held: false,
        }
    }

    /// Whether some member is dialed or awaited without a live session.
    pub(crate) fn set_connecting(&self, connecting: bool) {
        self.change(|state| state.policy.connecting = connecting);
    }

    /// Keeps the socket at normal priority while a dial-back runs.
    pub(crate) fn dialing_back(self: &Arc<Self>) -> DialingBack {
        self.change(|state| state.policy.dialing_back += 1);
        DialingBack(Arc::clone(self))
    }

    fn hold(&self) {
        self.change(|state| {
            if state.policy.hold(Instant::now()) && !state.marked {
                state.tell_refused();
            }
        });
    }

    /// Brings the socket's mark in line with the policy before a send, calling `apply` with the
    /// wanted mark only when it differs from the one the socket carries.
    pub(crate) fn settle(&self, apply: impl FnOnce(bool) -> io::Result<()>) {
        let due = self.recheck.load(Ordering::Acquire);
        if due == SETTLED || (due != DUE && self.nanos_since_origin(Instant::now()) < due) {
            return;
        }
        let mut state = self.state();
        let now = Instant::now();
        let normal = state.policy.normal(now);
        let voice = normal.is_none();
        if voice != state.marked {
            match apply(voice) {
                Ok(()) => {
                    state.marked = voice;
                    match normal {
                        None => {
                            state.policy.marked_at = Some(now);
                            log::info!("link priority: voice");
                        }
                        Some(normal) => state.tell_normal(normal),
                    }
                }
                Err(error) => {
                    // A later change retries a failed unmark; a refused mark is not tried again.
                    state.policy.refused = true;
                    state.told_refused = true;
                    if voice {
                        log::warn!(
                            "link priority: the system refused voice; datagrams keep normal \
                             priority: {error}"
                        );
                    } else {
                        log::warn!("link priority: could not return to normal: {error}");
                    }
                }
            }
        }
        let next = state.policy.next_check(now);
        self.recheck.store(self.recheck_at(next), Ordering::Release);
    }

    fn nanos_since_origin(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.origin).as_nanos()).unwrap_or(SETTLED)
    }

    fn recheck_at(&self, at: Option<Instant>) -> u64 {
        at.map_or(SETTLED, |at| {
            self.nanos_since_origin(at).clamp(DUE + 1, SETTLED - 1)
        })
    }

    /// Whether the policy wants the mark now, whatever the socket carries.
    #[cfg(test)]
    pub(crate) fn wants_voice(&self) -> bool {
        self.state().policy.normal(Instant::now()).is_none()
    }
}

/// One share session's part in its endpoint's priority: starting until `start`, then running
/// until dropped.
pub(crate) struct SessionPriority {
    control: Arc<LinkPriority>,
    running: bool,
    held: bool,
}

impl SessionPriority {
    /// The session finished startup.
    pub(crate) fn start(&mut self) {
        if !self.running {
            self.running = true;
            self.control.change(|state| {
                state.policy.starting = state.policy.starting.saturating_sub(1);
                state.policy.running += 1;
            });
        }
    }

    /// Whether the running session is held now; each hold drops the mark when it begins.
    pub(crate) fn note_hold(&mut self, held: bool) {
        if held && !self.held && self.running {
            self.control.hold();
        }
        self.held = held;
    }
}

impl Drop for SessionPriority {
    fn drop(&mut self) {
        let running = self.running;
        self.control.change(|state| {
            let count = if running {
                &mut state.policy.running
            } else {
                &mut state.policy.starting
            };
            *count = count.saturating_sub(1);
        });
    }
}

/// A dial-back's hold on normal priority, released when dropped.
pub(crate) struct DialingBack(Arc<LinkPriority>);

impl Drop for DialingBack {
    fn drop(&mut self) {
        self.0.change(|state| {
            state.policy.dialing_back = state.policy.dialing_back.saturating_sub(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn running(policy: &mut PriorityPolicy) {
        policy.running += 1;
    }

    fn secs(value: f64) -> Duration {
        Duration::from_secs_f64(value)
    }

    #[test]
    fn a_new_endpoint_is_normal_and_marks_only_once_a_session_runs_past_startup() {
        let now = Instant::now();
        let mut policy = PriorityPolicy::default();
        assert_eq!(policy.normal(now), Some(Normal::Idle));
        policy.starting += 1;
        assert_eq!(
            policy.normal(now),
            Some(Normal::Idle),
            "startup goes unmarked"
        );
        running(&mut policy);
        assert_eq!(policy.normal(now), Some(Normal::Connecting));
        policy.starting -= 1;
        assert_eq!(policy.normal(now), None);
        assert_eq!(policy.next_check(now), None);
    }

    #[test]
    fn a_dial_a_dial_back_or_another_starting_session_keeps_normal_beside_a_running_one() {
        let now = Instant::now();
        let mut policy = PriorityPolicy::default();
        running(&mut policy);
        policy.connecting = true;
        assert_eq!(policy.normal(now), Some(Normal::Connecting));
        policy.connecting = false;
        policy.dialing_back = 1;
        assert_eq!(policy.normal(now), Some(Normal::Connecting));
        policy.dialing_back = 0;
        policy.starting = 1;
        assert_eq!(policy.normal(now), Some(Normal::Connecting));
        policy.starting = 0;
        assert_eq!(policy.normal(now), None);
    }

    #[test]
    fn a_hold_keeps_normal_for_a_minute_from_the_latest_hold_then_marks_again() {
        let start = Instant::now();
        let mut policy = PriorityPolicy::default();
        running(&mut policy);
        policy.marked_at = Some(start);
        assert!(!policy.hold(start + secs(10.0)));
        assert_eq!(policy.normal(start + secs(10.0)), Some(Normal::AfterHold));
        assert_eq!(
            policy.next_check(start + secs(10.0)),
            Some(start + secs(70.0))
        );
        assert!(!policy.hold(start + secs(30.0)));
        assert_eq!(policy.normal(start + secs(89.9)), Some(Normal::AfterHold));
        assert_eq!(policy.normal(start + secs(90.0)), None);
        assert_eq!(policy.next_check(start + secs(90.0)), None);
    }

    #[test]
    fn a_hold_soon_after_marking_ends_marking_for_good() {
        let start = Instant::now();
        let mut policy = PriorityPolicy::default();
        running(&mut policy);
        policy.marked_at = Some(start);
        assert!(policy.hold(start + secs(1.5)));
        assert_eq!(policy.normal(start + secs(1_000.0)), Some(Normal::Refused));
        assert!(
            !policy.hold(start + secs(1_000.0)),
            "refused once, not again"
        );

        let mut policy = PriorityPolicy::default();
        running(&mut policy);
        policy.marked_at = Some(start);
        assert!(!policy.hold(start + secs(2.5)));
        policy.marked_at = Some(start + secs(62.5));
        assert!(
            policy.hold(start + secs(63.0)),
            "a re-mark must prove itself again"
        );
        assert_eq!(policy.normal(start + secs(500.0)), Some(Normal::Refused));
    }

    #[test]
    fn the_last_running_session_ending_returns_to_normal() {
        let now = Instant::now();
        let mut policy = PriorityPolicy::default();
        running(&mut policy);
        running(&mut policy);
        policy.running -= 1;
        assert_eq!(policy.normal(now), None);
        policy.running -= 1;
        assert_eq!(policy.normal(now), Some(Normal::Idle));
    }

    /// Settles `priority` as a send would, returning the mark it applied, if any.
    fn send(priority: &LinkPriority) -> Option<bool> {
        let mut applied = None;
        priority.settle(|voice| {
            applied = Some(voice);
            Ok(())
        });
        applied
    }

    #[test]
    fn sessions_and_dials_reach_the_mark_only_at_the_next_send_and_only_on_a_change() {
        let priority = LinkPriority::new();
        assert_eq!(
            send(&priority),
            None,
            "a bound socket starts and stays unmarked"
        );
        let mut session = priority.session();
        assert_eq!(send(&priority), None, "startup goes unmarked");
        session.start();
        assert_eq!(send(&priority), Some(true));
        assert_eq!(send(&priority), None, "settled until something changes");
        priority.set_connecting(true);
        assert_eq!(send(&priority), Some(false), "a dial never goes marked");
        priority.set_connecting(false);
        assert_eq!(send(&priority), Some(true));
        let dialing_back = priority.dialing_back();
        assert_eq!(send(&priority), Some(false));
        drop(dialing_back);
        assert_eq!(send(&priority), Some(true));
        drop(session);
        assert_eq!(
            send(&priority),
            Some(false),
            "the next dial after the last session is unmarked"
        );
    }

    #[test]
    fn a_hold_drops_the_mark_at_the_next_send_and_one_right_after_marking_ends_it() {
        let priority = LinkPriority::new();
        let mut session = priority.session();
        session.start();
        assert_eq!(send(&priority), Some(true));
        session.note_hold(true);
        assert_eq!(send(&priority), Some(false));
        session.note_hold(true);
        session.note_hold(false);
        assert_eq!(send(&priority), None, "only the start of a hold counts");
        assert!(!priority.wants_voice());
        assert!(
            priority.state().policy.refused,
            "the hold came right after marking"
        );
        assert!(priority.state().policy.quiet_until.is_some());
    }

    #[test]
    fn a_refused_mark_is_not_tried_again() {
        let priority = LinkPriority::new();
        let mut session = priority.session();
        session.start();
        let mut tries = 0;
        for _ in 0..3 {
            priority.settle(|_| {
                tries += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            });
        }
        assert_eq!(tries, 1);
        assert!(!priority.wants_voice());
    }
}
