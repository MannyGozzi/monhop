//! How well a session's link carries input right now, judged only from evidence the session
//! already has: when each heartbeat challenge was answered, and quinn's view of the path. It
//! classifies and nothing more; health deadlines and the protocol are untouched by it.

use std::{collections::VecDeque, fmt, time::Duration};

/// A reply at least this late shows queueing or retransmission: a LAN, Wi-Fi included, answers
/// within a few ms.
pub const LATE_REPLY: Duration = Duration::from_millis(20);
/// A reply at least this late, or a challenge unanswered this long, is a stall: the link stopped
/// carrying for a moment, as a Wi-Fi channel hop or a scan does.
pub const STALLED_REPLY: Duration = Duration::from_millis(60);
/// Evidence counts toward the link's tier for this long.
pub const QUALITY_WINDOW: Duration = Duration::from_secs(1);
/// Late marks within the window that degrade a link: about one heartbeat in ten, never one blip.
pub const DEGRADED_MARKS: usize = 3;
/// Stalled marks within the window that make a link bad.
pub const BAD_MARKS: usize = 3;
/// Quinn's packet loss over one judged span that counts as a late mark.
pub const LATE_LOSS: f64 = 0.01;
/// Quinn's packet loss over one judged span that counts as a stalled mark.
pub const STALLED_LOSS: f64 = 0.05;
/// Loss is judged once at least this many packets went out since the last judgment.
pub const LOSS_MIN_PACKETS: u64 = 50;
/// How often a runtime reads quinn's path stats for a link.
pub const PATH_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// A link climbs one tier only after its evidence has read better than its tier for this long.
pub const UPGRADE_AFTER: Duration = Duration::from_secs(3);
/// The least time between two motion frames on a degraded link: 250 Hz.
pub const DEGRADED_MOTION_INTERVAL: Duration = Duration::from_millis(4);
/// The least time between two motion frames on a bad link: 125 Hz.
pub const BAD_MOTION_INTERVAL: Duration = Duration::from_millis(8);

/// Bounds memory if a caller feeds evidence faster than the window ages it out.
const MAX_MARKS: usize = 256;
/// Challenges go out every 30 ms, so this outlasts any stall the window can still see.
const MAX_CHALLENGES: usize = 64;

/// A link's tier, best first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkQuality {
    #[default]
    Healthy,
    Degraded,
    Bad,
}

impl LinkQuality {
    /// The least time between two motion frames; zero sends each one as it comes.
    pub const fn motion_interval(self) -> Duration {
        match self {
            Self::Healthy => Duration::ZERO,
            Self::Degraded => DEGRADED_MOTION_INTERVAL,
            Self::Bad => BAD_MOTION_INTERVAL,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Bad => "bad",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    pub(crate) const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Degraded,
            2 => Self::Bad,
            _ => Self::Healthy,
        }
    }

    const fn better(self) -> Self {
        match self {
            Self::Bad => Self::Degraded,
            Self::Degraded | Self::Healthy => Self::Healthy,
        }
    }
}

impl fmt::Display for LinkQuality {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Quinn's path counters at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathSample {
    /// Quinn's smoothed round trip.
    pub rtt: Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Clean,
    Late,
    Stalled,
}

fn lateness(delay: Duration) -> Severity {
    if delay >= STALLED_REPLY {
        Severity::Stalled
    } else if delay >= LATE_REPLY {
        Severity::Late
    } else {
        Severity::Clean
    }
}

fn loss_severity(loss: Option<f64>) -> Severity {
    match loss {
        Some(loss) if loss >= STALLED_LOSS => Severity::Stalled,
        Some(loss) if loss >= LATE_LOSS => Severity::Late,
        _ => Severity::Clean,
    }
}

#[derive(Clone, Copy, Debug)]
enum Evidence {
    Reply(Duration),
    /// A challenge still unanswered at STALLED_REPLY; its reply, if it ever comes, adds nothing.
    Unanswered,
    Path {
        rtt: Duration,
        loss: Option<f64>,
    },
    /// The session held: the peer was silent past the retreat point.
    Hold,
}

impl Evidence {
    fn severity(self) -> Severity {
        match self {
            Self::Reply(delay) => lateness(delay),
            Self::Unanswered | Self::Hold => Severity::Stalled,
            Self::Path { rtt, loss } => lateness(rtt).max(loss_severity(loss)),
        }
    }
}

/// What the window holds, for the tier and its log line; counts and timings only.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QualitySummary {
    pub replies: usize,
    pub unanswered: usize,
    /// Marks at least late, stalled ones included.
    pub late: usize,
    pub stalled: usize,
    pub slowest_reply: Duration,
    pub held: bool,
    /// The newest path sample's round trip and judged loss.
    pub path_rtt: Option<Duration>,
    pub loss: Option<f64>,
}

impl QualitySummary {
    fn tier(&self) -> LinkQuality {
        if self.held || self.stalled >= BAD_MARKS {
            LinkQuality::Bad
        } else if self.late >= DEGRADED_MARKS {
            LinkQuality::Degraded
        } else {
            LinkQuality::Healthy
        }
    }
}

impl fmt::Display for QualitySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "in the last {} ms: {} replies, {} unanswered, {} late, {} stalled, slowest {} ms",
            QUALITY_WINDOW.as_millis(),
            self.replies,
            self.unanswered,
            self.late,
            self.stalled,
            self.slowest_reply.as_millis()
        )?;
        if let Some(rtt) = self.path_rtt {
            write!(formatter, "; path rtt {:.1} ms", rtt.as_secs_f64() * 1000.0)?;
        }
        if let Some(loss) = self.loss {
            write!(formatter, ", loss {:.1}%", loss * 100.0)?;
        }
        if self.held {
            formatter.write_str("; held")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierChange {
    pub from: LinkQuality,
    pub to: LinkQuality,
    pub summary: QualitySummary,
}

/// Downgrades as soon as the window's evidence reads worse than the tier, and climbs one tier at
/// a time once it has read better for UPGRADE_AFTER. Every `now` is on the session clock.
#[derive(Debug, Default)]
pub struct LinkMonitor {
    tier: LinkQuality,
    better_since: Option<Duration>,
    /// When the current run of trouble first and last showed; a clean window ends the run.
    trouble: Option<(Duration, Duration)>,
    /// Sent and neither answered nor overdue, oldest first.
    challenges: VecDeque<(u64, Duration)>,
    marks: VecDeque<(Duration, Evidence)>,
    /// The counters loss was last judged from.
    path_base: Option<PathSample>,
}

impl LinkMonitor {
    pub fn quality(&self) -> LinkQuality {
        self.tier
    }

    /// How long trouble has kept arriving, first evidence to latest: one stall stays as short as
    /// itself however long the tier takes to climb back, and a clean window reads zero.
    pub fn unsteady_for(&self) -> Duration {
        self.trouble
            .map_or(Duration::ZERO, |(first, last)| last.saturating_sub(first))
    }

    pub fn challenge_sent(&mut self, token: u64, now: Duration) {
        if self.challenges.len() == MAX_CHALLENGES {
            self.challenges.pop_front();
        }
        self.challenges.push_back((token, now));
    }

    /// A reply to a challenge this monitor saw go out; any other token proves nothing here.
    pub fn reply(&mut self, token: u64, now: Duration) -> Option<TierChange> {
        let index = self
            .challenges
            .iter()
            .position(|(sent, _)| *sent == token)?;
        let (_, sent) = self.challenges.remove(index)?;
        self.mark(now, Evidence::Reply(now.saturating_sub(sent)));
        self.settle(now)
    }

    pub fn path(&mut self, sample: PathSample, now: Duration) -> Option<TierChange> {
        let loss = match self.path_base {
            Some(base) => {
                let sent = sample.sent_packets.saturating_sub(base.sent_packets);
                (sent >= LOSS_MIN_PACKETS).then(|| {
                    let lost = sample.lost_packets.saturating_sub(base.lost_packets);
                    lost as f64 / sent as f64
                })
            }
            None => None,
        };
        if loss.is_some() || self.path_base.is_none() {
            self.path_base = Some(sample);
        }
        self.mark(
            now,
            Evidence::Path {
                rtt: sample.rtt,
                loss,
            },
        );
        self.settle(now)
    }

    pub fn held(&mut self, now: Duration) -> Option<TierChange> {
        self.mark(now, Evidence::Hold);
        self.settle(now)
    }

    /// Marks challenges that went overdue and lets a clean stretch climb a tier.
    pub fn tick(&mut self, now: Duration) -> Option<TierChange> {
        while let Some(&(_, sent)) = self.challenges.front() {
            if now.saturating_sub(sent) < STALLED_REPLY {
                break;
            }
            self.challenges.pop_front();
            self.mark(now, Evidence::Unanswered);
        }
        self.settle(now)
    }

    pub fn summary(&self) -> QualitySummary {
        let mut summary = QualitySummary::default();
        for (_, evidence) in &self.marks {
            let severity = evidence.severity();
            summary.late += usize::from(severity >= Severity::Late);
            summary.stalled += usize::from(severity == Severity::Stalled);
            match *evidence {
                Evidence::Reply(delay) => {
                    summary.replies += 1;
                    summary.slowest_reply = summary.slowest_reply.max(delay);
                }
                Evidence::Unanswered => summary.unanswered += 1,
                Evidence::Path { rtt, loss } => {
                    summary.path_rtt = Some(rtt);
                    summary.loss = loss.or(summary.loss);
                }
                Evidence::Hold => summary.held = true,
            }
        }
        summary
    }

    fn mark(&mut self, now: Duration, evidence: Evidence) {
        if self.marks.len() == MAX_MARKS {
            self.marks.pop_front();
        }
        self.marks.push_back((now, evidence));
        if evidence.severity() >= Severity::Late {
            let first = self.trouble.map_or(now, |(first, _)| first);
            self.trouble = Some((first, now));
        }
    }

    fn settle(&mut self, now: Duration) -> Option<TierChange> {
        while self
            .marks
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= QUALITY_WINDOW)
        {
            self.marks.pop_front();
        }
        let summary = self.summary();
        let evidence = summary.tier();
        if evidence == LinkQuality::Healthy {
            self.trouble = None;
        }
        let from = self.tier;
        if evidence > from {
            self.tier = evidence;
            self.better_since = None;
        } else if evidence < from {
            let since = *self.better_since.get_or_insert(now);
            if now.saturating_sub(since) < UPGRADE_AFTER {
                return None;
            }
            // The next tier up needs a clean stretch of its own.
            self.tier = from.better();
            self.better_since = None;
        } else {
            self.better_since = None;
            return None;
        }
        Some(TierChange {
            from,
            to: self.tier,
            summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// Plays a recorded heartbeat trace: a challenge every 30 ms from `from`, each answered after
    /// its delay (None never answers), with the 1 ms tick in between. Yields every tier change.
    fn play(
        monitor: &mut LinkMonitor,
        first_token: u64,
        from: Duration,
        delays: &[Option<u64>],
    ) -> Vec<(Duration, LinkQuality)> {
        let mut replies: Vec<(Duration, u64)> = Vec::new();
        let mut changes = Vec::new();
        for elapsed in 0..=30 * delays.len() as u64 + 200 {
            let now = from + ms(elapsed);
            if elapsed.is_multiple_of(30)
                && let Some(delay) = delays.get((elapsed / 30) as usize)
            {
                let token = first_token + elapsed / 30;
                monitor.challenge_sent(token, now);
                if let Some(delay) = delay {
                    replies.push((now + ms(*delay), token));
                }
            }
            replies.sort();
            while replies.first().is_some_and(|(at, _)| *at <= now) {
                let (_, token) = replies.remove(0);
                if let Some(change) = monitor.reply(token, now) {
                    changes.push((now, change.to));
                }
            }
            if let Some(change) = monitor.tick(now) {
                changes.push((now, change.to));
            }
        }
        changes
    }

    fn idle(
        monitor: &mut LinkMonitor,
        from: Duration,
        until: Duration,
    ) -> Vec<(Duration, LinkQuality)> {
        let mut changes = Vec::new();
        let mut now = from;
        while now <= until {
            if let Some(change) = monitor.tick(now) {
                changes.push((now, change.to));
            }
            now += ms(1);
        }
        changes
    }

    #[test]
    fn a_lan_answering_in_a_few_ms_with_one_blip_stays_healthy() {
        let mut monitor = LinkMonitor::default();
        let mut trace = vec![Some(3); 100];
        trace[40] = Some(45);
        trace[70] = Some(25);
        assert!(play(&mut monitor, 1, ms(0), &trace).is_empty());
        assert_eq!(monitor.quality(), LinkQuality::Healthy);
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
    }

    #[test]
    fn airdrop_hops_every_half_second_degrade_promptly() {
        // An 80 ms stall every 500 ms: the challenges caught in it come back 80, 50 and 20 ms late.
        let mut trace = vec![Some(3); 60];
        for stall in [10, 27, 44] {
            trace[stall] = Some(80);
            trace[stall + 1] = Some(50);
            trace[stall + 2] = Some(20);
        }
        let mut monitor = LinkMonitor::default();
        let changes = play(&mut monitor, 1, ms(0), &trace);
        assert_eq!(
            changes.first().map(|(_, to)| *to),
            Some(LinkQuality::Degraded)
        );
        let (at, _) = changes[0];
        assert!(
            at <= ms(30 * 13),
            "degraded within the first stall, at {at:?}"
        );
        assert!(!changes.iter().any(|(_, to)| *to == LinkQuality::Bad));
    }

    #[test]
    fn a_long_stall_is_bad_before_any_reply_arrives() {
        let mut monitor = LinkMonitor::default();
        // Six challenges vanish into a 200 ms stall, then all come back at once.
        let trace = [
            Some(3),
            Some(3),
            Some(200),
            Some(170),
            Some(140),
            Some(110),
            Some(80),
            Some(50),
            Some(23),
        ];
        let changes = play(&mut monitor, 1, ms(0), &trace);
        let bad = changes
            .iter()
            .find(|(_, to)| *to == LinkQuality::Bad)
            .expect("the stall makes the link bad");
        assert!(
            bad.0 < ms(60 + 200),
            "bad while the stall is still running, at {:?}",
            bad.0
        );
    }

    #[test]
    fn recovery_climbs_one_tier_at_a_time_after_a_clean_stretch() {
        let mut monitor = LinkMonitor::default();
        assert_eq!(
            monitor.held(ms(1_000)).map(|change| change.to),
            Some(LinkQuality::Bad)
        );
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
        let changes = idle(&mut monitor, ms(1_000), ms(9_000));
        // The hold ages out of the window at 2 s, then each tier waits out its own clean stretch.
        assert_eq!(
            changes,
            vec![
                (ms(2_000) + UPGRADE_AFTER, LinkQuality::Degraded),
                (ms(2_001) + UPGRADE_AFTER * 2, LinkQuality::Healthy),
            ]
        );
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
    }

    #[test]
    fn fresh_evidence_restarts_the_clean_stretch_and_can_drop_two_tiers_at_once() {
        let mut monitor = LinkMonitor::default();
        let trace = [Some(30), Some(30), Some(30)];
        assert_eq!(
            play(&mut monitor, 1, ms(0), &trace)
                .last()
                .map(|(_, to)| *to),
            Some(LinkQuality::Degraded)
        );
        // Late again 2.5 s into the clean stretch: the climb waits a full stretch from there.
        let changes = idle(&mut monitor, ms(300), ms(3_500));
        assert!(changes.is_empty());
        let trace = [Some(25), Some(25), Some(25)];
        assert!(play(&mut monitor, 10, ms(3_500), &trace).is_empty());
        let changes = idle(&mut monitor, ms(3_800), ms(8_000));
        assert_eq!(
            changes,
            vec![(ms(4_525) + UPGRADE_AFTER, LinkQuality::Healthy)]
        );
        // Three stalls in a row from healthy go straight to bad.
        let trace = [None, None, None, Some(3)];
        let changes = play(&mut monitor, 20, ms(9_000), &trace);
        assert_eq!(changes.first().map(|(_, to)| *to), Some(LinkQuality::Bad));
    }

    /// Heartbeats 30 ms apart answered in 2 ms, except that the link carries nothing during each
    /// `(start, length)` stall, so a reply waits for its end.
    fn stalled_trace(heartbeats: u64, stalls: &[(u64, u64)]) -> Vec<Option<u64>> {
        (0..heartbeats)
            .map(|beat| {
                let sent = beat * 30;
                let stall = stalls
                    .iter()
                    .find(|(start, length)| (*start..start + length).contains(&sent));
                Some(stall.map_or(2, |(start, length)| start + length - sent))
            })
            .collect()
    }

    /// The card's note waits for this much trouble (LINK_NOTE_AFTER_MS in link-note-model.mjs).
    const NOTE_AFTER: Duration = Duration::from_secs(2);

    #[test]
    fn one_stall_never_earns_the_note_however_long_the_tier_takes_to_climb() {
        for length in [100, 150, 400, 1_000] {
            let mut monitor = LinkMonitor::default();
            // The trace ends inside the window, so the run is still live and at its longest.
            let heartbeats = (300 + length + 100) / 30;
            let trace = stalled_trace(heartbeats, &[(300, length)]);
            play(&mut monitor, 1, ms(0), &trace);
            assert_ne!(monitor.quality(), LinkQuality::Healthy, "{length} ms stall");
            let mut peak = monitor.unsteady_for();
            for now in (length + 600)..9_000 {
                monitor.tick(ms(now));
                peak = peak.max(monitor.unsteady_for());
            }
            assert!(peak < NOTE_AFTER, "{length} ms stall peaked at {peak:?}");
            assert_eq!(monitor.quality(), LinkQuality::Healthy, "{length} ms stall");
            assert_eq!(monitor.unsteady_for(), Duration::ZERO, "{length} ms stall");
        }
    }

    #[test]
    fn stalls_that_keep_coming_earn_the_note_and_it_leaves_once_they_stop() {
        let stalls: Vec<(u64, u64)> = (0..10).map(|stall| (stall * 500, 80)).collect();
        let mut monitor = LinkMonitor::default();
        play(&mut monitor, 1, ms(0), &stalled_trace(170, &stalls));
        assert!(monitor.unsteady_for() >= NOTE_AFTER);
        // Well before the tier climbs back, the window has read clean.
        idle(&mut monitor, ms(5_300), ms(6_300));
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
        assert_ne!(monitor.quality(), LinkQuality::Healthy);
    }

    #[test]
    fn an_outage_that_lasts_earns_the_note_while_it_runs() {
        let mut monitor = LinkMonitor::default();
        play(&mut monitor, 1, ms(0), &[None; 100]);
        assert!(monitor.unsteady_for() >= NOTE_AFTER);
    }

    #[test]
    fn quinn_loss_and_round_trip_count_once_enough_packets_went_out() {
        let mut monitor = LinkMonitor::default();
        let sample = |rtt: u64, lost: u64, sent: u64| PathSample {
            rtt: ms(rtt),
            lost_packets: lost,
            sent_packets: sent,
        };
        assert_eq!(monitor.path(sample(2, 0, 100), ms(0)), None);
        // Too few packets to judge: the base stays, so the next span covers both.
        assert_eq!(monitor.path(sample(2, 5, 120), ms(250)), None);
        assert_eq!(monitor.summary().loss, None);
        assert_eq!(monitor.path(sample(2, 6, 300), ms(500)), None);
        assert_eq!(monitor.summary().loss, Some(0.03));
        assert_eq!(monitor.path(sample(2, 8, 400), ms(750)), None);
        let change = monitor
            .path(sample(25, 8, 410), ms(1_000))
            .expect("a third late mark");
        assert_eq!(change.to, LinkQuality::Degraded);
        assert_eq!(change.summary.late, 3);
        let mut monitor = LinkMonitor::default();
        let mut tier = None;
        for (index, rtt) in [70, 75, 80].into_iter().enumerate() {
            let at = ms(250 * index as u64);
            tier = monitor
                .path(sample(rtt, 0, 0), at)
                .map(|change| change.to)
                .or(tier);
        }
        assert_eq!(tier, Some(LinkQuality::Bad));
    }

    #[test]
    fn replies_to_challenges_it_never_saw_prove_nothing() {
        let mut monitor = LinkMonitor::default();
        monitor.challenge_sent(5, ms(0));
        assert_eq!(monitor.reply(4, ms(100)), None);
        assert_eq!(monitor.reply(5, ms(10)), None);
        assert_eq!(monitor.reply(5, ms(200)), None);
        assert_eq!(monitor.summary().replies, 1);
        // An overdue challenge counts once, as unanswered, however late its reply.
        monitor.challenge_sent(6, ms(300));
        assert_eq!(monitor.tick(ms(360)), None);
        assert_eq!(monitor.reply(6, ms(500)), None);
        let summary = monitor.summary();
        assert_eq!(
            (summary.unanswered, summary.stalled, summary.replies),
            (1, 1, 1)
        );
    }

    #[test]
    fn the_summary_reads_as_counts_and_timings() {
        let mut monitor = LinkMonitor::default();
        monitor.challenge_sent(1, ms(0));
        monitor.reply(1, ms(42));
        monitor.path(
            PathSample {
                rtt: Duration::from_micros(6_500),
                lost_packets: 0,
                sent_packets: 0,
            },
            ms(50),
        );
        assert_eq!(
            monitor.summary().to_string(),
            "in the last 1000 ms: 1 replies, 0 unanswered, 1 late, 0 stalled, slowest 42 ms; path rtt 6.5 ms"
        );
        assert_eq!(
            LinkQuality::from_code(LinkQuality::Bad.code()),
            LinkQuality::Bad
        );
        assert_eq!(LinkQuality::from_code(9), LinkQuality::Healthy);
        assert!(LinkQuality::Healthy.motion_interval().is_zero());
        assert_eq!(LinkQuality::Bad.motion_interval(), BAD_MOTION_INTERVAL);
    }
}
