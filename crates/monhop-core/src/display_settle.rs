//! Decides whether a display change is real. Displays that drop out and come back inside the
//! settle window, such as a monitor shared by two computers flapping its input, never end a
//! session; a change that holds, or displays that stay unreadable, still do.

use std::time::{Duration, Instant};

/// How long displays may differ from the session's, or stay unreadable, before the change counts.
/// Sized above the shared monitor's ~2 s input flaps and under the 5 s health hold.
pub const DISPLAY_SETTLE_LIMIT: Duration = Duration::from_secs(3);
/// How long rereads must keep matching the session's displays before a suspicion clears.
pub const DISPLAY_STEADY_FOR: Duration = Duration::from_millis(250);
/// How often a watcher that is not already polling rereads while a change is unsettled.
pub const DISPLAY_SETTLE_POLL: Duration = Duration::from_millis(100);

/// One reread compared with the displays the session started on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayRead {
    Same,
    Different,
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayVerdict {
    /// The displays match the session's and nothing is suspected.
    Steady,
    /// A change is suspected but not decided; the session carries on meanwhile.
    Unsettled,
    /// The session's displays came back; the payload is how long they were away.
    Recovered(Duration),
    /// Different or unreadable for the whole settle window: the session must end.
    Changed,
}

#[derive(Clone, Debug)]
pub struct DisplaySettle {
    limit: Duration,
    steady_for: Duration,
    since: Option<Instant>,
    matching_since: Option<Instant>,
    decided: bool,
}

impl Default for DisplaySettle {
    fn default() -> Self {
        Self::with_timing(DISPLAY_SETTLE_LIMIT, DISPLAY_STEADY_FOR)
    }
}

impl DisplaySettle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timing(limit: Duration, steady_for: Duration) -> Self {
        Self {
            limit,
            steady_for,
            since: None,
            matching_since: None,
            decided: false,
        }
    }

    /// True from a suspicion until it recovers; a decided change stays unsettled.
    pub fn is_unsettled(&self) -> bool {
        self.since.is_some()
    }

    /// The platform said displays changed without a reread; a later matching reread must hold
    /// for the steady time again before the suspicion clears.
    pub fn suspect(&mut self, now: Instant) {
        self.since.get_or_insert(now);
        self.matching_since = None;
    }

    pub fn observe(&mut self, read: DisplayRead, now: Instant) -> DisplayVerdict {
        if self.decided {
            return DisplayVerdict::Changed;
        }
        let Some(since) = self.since else {
            if read == DisplayRead::Same {
                return DisplayVerdict::Steady;
            }
            self.since = Some(now);
            return self.expire(now, now);
        };
        if read != DisplayRead::Same {
            self.matching_since = None;
            return self.expire(since, now);
        }
        let matching = *self.matching_since.get_or_insert(now);
        if now.saturating_duration_since(matching) >= self.steady_for {
            self.since = None;
            self.matching_since = None;
            return DisplayVerdict::Recovered(matching.saturating_duration_since(since));
        }
        DisplayVerdict::Unsettled
    }

    fn expire(&mut self, since: Instant, now: Instant) -> DisplayVerdict {
        if now.saturating_duration_since(since) >= self.limit {
            self.decided = true;
            DisplayVerdict::Changed
        } else {
            DisplayVerdict::Unsettled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn settle() -> (DisplaySettle, Instant) {
        (DisplaySettle::new(), Instant::now())
    }

    #[test]
    fn matching_reads_stay_steady() {
        let (mut settle, start) = settle();
        for step in 0..100 {
            assert_eq!(
                settle.observe(DisplayRead::Same, start + step * 30 * MS),
                DisplayVerdict::Steady
            );
        }
        assert!(!settle.is_unsettled());
    }

    #[test]
    fn a_display_that_drops_out_for_two_seconds_and_returns_continues() {
        let (mut settle, start) = settle();
        let mut now = start;
        while now < start + 2_000 * MS {
            assert_eq!(
                settle.observe(DisplayRead::Different, now),
                DisplayVerdict::Unsettled
            );
            now += 30 * MS;
        }
        let back = now;
        while now < back + DISPLAY_STEADY_FOR {
            assert_eq!(
                settle.observe(DisplayRead::Same, now),
                DisplayVerdict::Unsettled
            );
            now += 30 * MS;
        }
        assert_eq!(
            settle.observe(DisplayRead::Same, now),
            DisplayVerdict::Recovered(back - start)
        );
        assert!(!settle.is_unsettled());
        assert_eq!(
            settle.observe(DisplayRead::Same, now + 30 * MS),
            DisplayVerdict::Steady
        );
    }

    #[test]
    fn a_change_that_holds_ends_the_session_at_the_limit_and_stays_decided() {
        let (mut settle, start) = settle();
        assert_eq!(
            settle.observe(DisplayRead::Different, start),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(DisplayRead::Different, start + DISPLAY_SETTLE_LIMIT - MS),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(DisplayRead::Different, start + DISPLAY_SETTLE_LIMIT),
            DisplayVerdict::Changed
        );
        assert_eq!(
            settle.observe(DisplayRead::Same, start + DISPLAY_SETTLE_LIMIT * 2),
            DisplayVerdict::Changed
        );
    }

    #[test]
    fn displays_that_stay_unreadable_count_as_changed() {
        let (mut settle, start) = settle();
        settle.observe(DisplayRead::Unreadable, start);
        assert_eq!(
            settle.observe(DisplayRead::Unreadable, start + DISPLAY_SETTLE_LIMIT),
            DisplayVerdict::Changed
        );
    }

    #[test]
    fn one_unreadable_read_between_matching_ones_recovers() {
        let (mut settle, start) = settle();
        assert_eq!(
            settle.observe(DisplayRead::Unreadable, start),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(DisplayRead::Same, start + 30 * MS),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(DisplayRead::Same, start + 30 * MS + DISPLAY_STEADY_FOR),
            DisplayVerdict::Recovered(30 * MS)
        );
    }

    #[test]
    fn a_flapping_display_never_settles_and_ends_at_the_limit() {
        let (mut settle, start) = settle();
        let mut now = start;
        let mut verdict = DisplayVerdict::Steady;
        let mut step = 0_u32;
        while verdict != DisplayVerdict::Changed {
            let read = if step.is_multiple_of(4) {
                DisplayRead::Different
            } else {
                DisplayRead::Same
            };
            verdict = settle.observe(read, now);
            assert!(!matches!(verdict, DisplayVerdict::Recovered(_)));
            now += 50 * MS;
            step += 1;
        }
        assert!(now - start <= DISPLAY_SETTLE_LIMIT + 250 * MS);
    }

    #[test]
    fn a_suspicion_with_unchanged_displays_clears_after_the_steady_time() {
        let (mut settle, start) = settle();
        settle.suspect(start);
        assert!(settle.is_unsettled());
        assert_eq!(
            settle.observe(DisplayRead::Same, start + DISPLAY_SETTLE_POLL),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(
                DisplayRead::Same,
                start + DISPLAY_SETTLE_POLL + DISPLAY_STEADY_FOR
            ),
            DisplayVerdict::Recovered(DISPLAY_SETTLE_POLL)
        );
    }

    #[test]
    fn a_new_suspicion_restarts_the_steady_time_but_not_the_limit() {
        let (mut settle, start) = settle();
        settle.suspect(start);
        settle.observe(DisplayRead::Same, start + 100 * MS);
        settle.suspect(start + 300 * MS);
        assert_eq!(
            settle.observe(DisplayRead::Same, start + 360 * MS),
            DisplayVerdict::Unsettled
        );
        assert_eq!(
            settle.observe(DisplayRead::Different, start + DISPLAY_SETTLE_LIMIT),
            DisplayVerdict::Changed
        );
    }
}
