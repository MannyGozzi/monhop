//! Keeps one multi-finger touch session on the route it started on, so a gesture that spans a
//! crossing never splits between the two computers. Allocation-free and callback-safe.

use std::time::Duration;

/// A session with no record for this long closes itself, so a lost end cannot pin its route.
pub const TOUCH_STREAM_IDLE: Duration = Duration::from_millis(500);

/// How a native record relates to the touch session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchRecord {
    /// Starts a session, replacing any still open: a new session latches the route now.
    Opens,
    /// Belongs to the open session.
    Continues,
    /// Belongs to the open session and ends it.
    Closes,
    /// A whole gesture in one record (smart zoom, force click, a committed system gesture).
    Standalone,
}

/// What the capture callback does with one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatchDecision {
    /// Local apps receive the record; false withholds it.
    pub deliver_locally: bool,
    /// Queue the record under the route now: forwarded when remote, seen only by take-back when
    /// local. Never set for a record whose session is latched to the other route.
    pub publish: bool,
}

impl LatchDecision {
    const fn routed(latched_remote: bool, remote: bool) -> Self {
        Self {
            deliver_locally: !latched_remote,
            publish: latched_remote == remote,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct OpenSession {
    remote: bool,
    last: Duration,
}

/// Records outside an open session (standalone, or a stray continuation) use the route now.
#[derive(Clone, Copy, Debug, Default)]
pub struct GestureLatch {
    open: Option<OpenSession>,
}

impl GestureLatch {
    pub const fn new() -> Self {
        Self { open: None }
    }

    /// Decides one record captured at `now` while the capture route is `remote`.
    pub fn decide(&mut self, record: TouchRecord, remote: bool, now: Duration) -> LatchDecision {
        self.expire(now);
        match record {
            TouchRecord::Opens => {
                self.open = Some(OpenSession { remote, last: now });
                LatchDecision::routed(remote, remote)
            }
            TouchRecord::Continues | TouchRecord::Closes => {
                let Some(session) = self.open.as_mut() else {
                    return LatchDecision::routed(remote, remote);
                };
                session.last = now;
                let decision = LatchDecision::routed(session.remote, remote);
                if record == TouchRecord::Closes {
                    self.open = None;
                }
                decision
            }
            TouchRecord::Standalone => LatchDecision::routed(remote, remote),
        }
    }

    /// The open session's route, `Some(true)` when remote; `None` when no session is open at `now`.
    pub fn latched(&self, now: Duration) -> Option<bool> {
        self.open
            .filter(|session| now.saturating_sub(session.last) < TOUCH_STREAM_IDLE)
            .map(|session| session.remote)
    }

    fn expire(&mut self, now: Duration) {
        if self.latched(now).is_none() {
            self.open = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: bool = false;
    const REMOTE: bool = true;

    const PASS_AND_PUBLISH: LatchDecision = LatchDecision {
        deliver_locally: true,
        publish: true,
    };
    const PASS: LatchDecision = LatchDecision {
        deliver_locally: true,
        publish: false,
    };
    const WITHHOLD_AND_PUBLISH: LatchDecision = LatchDecision {
        deliver_locally: false,
        publish: true,
    };
    const WITHHOLD: LatchDecision = LatchDecision {
        deliver_locally: false,
        publish: false,
    };

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// A latch whose session opened at 0 ms on `latched_remote`.
    fn opened(latched_remote: bool) -> GestureLatch {
        let mut latch = GestureLatch::new();
        latch.decide(TouchRecord::Opens, latched_remote, ms(0));
        latch
    }

    #[test]
    fn a_session_opens_on_the_route_now() {
        assert_eq!(
            GestureLatch::new().decide(TouchRecord::Opens, LOCAL, ms(0)),
            PASS_AND_PUBLISH
        );
        assert_eq!(
            GestureLatch::new().decide(TouchRecord::Opens, REMOTE, ms(0)),
            WITHHOLD_AND_PUBLISH
        );
    }

    #[test]
    fn latched_local_route_local_passes_and_publishes_for_take_back() {
        for record in [TouchRecord::Continues, TouchRecord::Closes] {
            assert_eq!(opened(LOCAL).decide(record, LOCAL, ms(1)), PASS_AND_PUBLISH);
        }
    }

    #[test]
    fn latched_local_route_remote_passes_and_is_never_forwarded() {
        for record in [TouchRecord::Continues, TouchRecord::Closes] {
            assert_eq!(opened(LOCAL).decide(record, REMOTE, ms(1)), PASS);
        }
    }

    #[test]
    fn latched_remote_route_remote_withholds_and_forwards() {
        for record in [TouchRecord::Continues, TouchRecord::Closes] {
            assert_eq!(
                opened(REMOTE).decide(record, REMOTE, ms(1)),
                WITHHOLD_AND_PUBLISH
            );
        }
    }

    #[test]
    fn latched_remote_route_local_withholds_the_tail_the_local_app_never_began() {
        for record in [TouchRecord::Continues, TouchRecord::Closes] {
            assert_eq!(opened(REMOTE).decide(record, LOCAL, ms(1)), WITHHOLD);
        }
    }

    #[test]
    fn a_stray_continuation_passes_or_withholds_by_the_route_now() {
        for record in [TouchRecord::Continues, TouchRecord::Closes] {
            assert_eq!(
                GestureLatch::new().decide(record, LOCAL, ms(0)),
                PASS_AND_PUBLISH
            );
            assert_eq!(
                GestureLatch::new().decide(record, REMOTE, ms(0)),
                WITHHOLD_AND_PUBLISH
            );
        }
    }

    #[test]
    fn a_standalone_record_uses_the_route_now_and_leaves_the_session_alone() {
        let mut latch = opened(REMOTE);
        assert_eq!(
            latch.decide(TouchRecord::Standalone, LOCAL, ms(1)),
            PASS_AND_PUBLISH
        );
        assert_eq!(latch.latched(ms(1)), Some(REMOTE));
        assert_eq!(latch.decide(TouchRecord::Continues, LOCAL, ms(2)), WITHHOLD);
        let mut idle = GestureLatch::new();
        assert_eq!(
            idle.decide(TouchRecord::Standalone, REMOTE, ms(0)),
            WITHHOLD_AND_PUBLISH
        );
        assert_eq!(
            idle.latched(ms(0)),
            None,
            "a standalone record opens nothing"
        );
    }

    #[test]
    fn closing_ends_the_session_so_the_next_record_is_stray() {
        let mut latch = opened(REMOTE);
        latch.decide(TouchRecord::Closes, LOCAL, ms(1));
        assert_eq!(latch.latched(ms(1)), None);
        assert_eq!(
            latch.decide(TouchRecord::Continues, LOCAL, ms(2)),
            PASS_AND_PUBLISH
        );
    }

    #[test]
    fn a_new_session_replaces_one_whose_end_was_lost() {
        let mut latch = opened(REMOTE);
        assert_eq!(
            latch.decide(TouchRecord::Opens, LOCAL, ms(1)),
            PASS_AND_PUBLISH
        );
        assert_eq!(latch.latched(ms(1)), Some(LOCAL));
        assert_eq!(latch.decide(TouchRecord::Continues, REMOTE, ms(2)), PASS);
    }

    #[test]
    fn a_session_silent_for_the_idle_limit_closes_itself() {
        let last = TOUCH_STREAM_IDLE - ms(1);
        let mut latch = opened(REMOTE);
        assert_eq!(latch.decide(TouchRecord::Continues, LOCAL, last), WITHHOLD);
        assert_eq!(
            latch.latched(last + TOUCH_STREAM_IDLE - ms(1)),
            Some(REMOTE)
        );
        assert_eq!(latch.latched(last + TOUCH_STREAM_IDLE), None);
        assert_eq!(
            latch.decide(TouchRecord::Continues, LOCAL, last + TOUCH_STREAM_IDLE),
            PASS_AND_PUBLISH,
            "each record restarts the idle limit; a full limit of silence expires it"
        );
        assert_eq!(latch.latched(last + TOUCH_STREAM_IDLE), None);
    }

    #[test]
    fn a_clock_step_backwards_never_expires_a_session() {
        let mut latch = GestureLatch::new();
        latch.decide(TouchRecord::Opens, REMOTE, ms(900));
        assert_eq!(
            latch.decide(TouchRecord::Continues, LOCAL, ms(100)),
            WITHHOLD
        );
        assert_eq!(latch.latched(ms(100)), Some(REMOTE));
    }
}
