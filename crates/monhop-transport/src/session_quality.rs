//! How well a session's link carries input right now, judged only from evidence the session
//! already has: when each heartbeat challenge was answered, and quinn's view of the path. It
//! classifies and nothing more; health deadlines and the protocol are untouched by it.

use std::{collections::VecDeque, fmt, time::Duration};

/// A reply at least this late shows queueing or retransmission. Wi-Fi with a busy channel routinely
/// answers in 20-30 ms and carries input fine, so that jitter alone never counts.
pub const LATE_REPLY: Duration = Duration::from_millis(30);
/// A reply at least this late, or a challenge unanswered this long, is a stall: the link stopped
/// carrying for a moment, as a Wi-Fi channel hop or a scan does.
pub const STALLED_REPLY: Duration = Duration::from_millis(60);
/// Evidence counts toward the link's tier for this long. One stall marks several heartbeats at
/// once, so a short window would grade a single hiccup as a bad link.
pub const QUALITY_WINDOW: Duration = Duration::from_secs(3);
/// Late marks within the window that keep a run of trouble alive: about one heartbeat in ten.
pub const UNSTEADY_LATE_MARKS: usize = 10;
/// Stalled marks within the window that keep a run of trouble alive.
pub const UNSTEADY_STALLED_MARKS: usize = 9;
/// Shares of a window's heartbeats divide by at least this, so first replies cannot set a tier.
pub const MIN_HEARTBEATS: usize = 33;
/// One heartbeat in this many late or unanswered means the typical one queued: a link degrades.
/// A radio away 14% of the time with clean replies between stays near one in ten.
pub const DEGRADED_DIVISOR: usize = 3;
/// One heartbeat in this many overdue or unanswered makes a link bad.
pub const BAD_DIVISOR: usize = 2;
/// Keeping a tier takes this much less of the share that earned it, so the line cannot flip it.
pub const KEEP_DIVISOR: usize = 2;
/// Quinn's packet loss over one judged span that counts as a late mark.
pub const LATE_LOSS: f64 = 0.01;
/// Quinn's packet loss over one judged span that counts as a stalled mark.
pub const STALLED_LOSS: f64 = 0.05;
/// Loss is judged once at least this many packets went out since the last judgment.
pub const LOSS_MIN_PACKETS: u64 = 50;
/// How often a runtime reads quinn's path stats for a link.
pub const PATH_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// A link climbs one tier only after its evidence has read better than its tier for this long.
pub const UPGRADE_AFTER: Duration = Duration::from_secs(2);
/// The least time between two motion frames on a degraded link: 250 Hz.
pub const DEGRADED_MOTION_INTERVAL: Duration = Duration::from_millis(4);
/// The least time between two motion frames on a bad link: 125 Hz.
pub const BAD_MOTION_INTERVAL: Duration = Duration::from_millis(8);

/// Overdue challenges sent closer together than this are one absence of the radio.
const HOP_SAME_ABSENCE: Duration = Duration::from_millis(200);
/// Gaps between absence starts that count as a beat, around a peer-to-peer radio's 525 ms.
const HOP_BEAT_MIN: Duration = Duration::from_millis(400);
const HOP_BEAT_MAX: Duration = Duration::from_millis(700);
/// How far a gap may differ from the one before it; heartbeats quantize absence starts to 30 ms.
const HOP_GAP_TOLERANCE: Duration = Duration::from_millis(60);
/// Absences in a row on the beat that make a train.
const HOP_TRAIN: u32 = 6;
/// One unbroken run this long is a schedule without waiting for a second train.
const HOP_LONG_RUN: Duration = Duration::from_secs(20);
/// A train starting this soon after the last train's final stall turns hopping on.
const HOP_TRAIN_WINDOW: Duration = Duration::from_secs(300);
/// Hopping stays on this long after a train stall, which bridges bursts 47 to 104 s apart.
const HOP_HOLD: Duration = Duration::from_secs(120);

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
    /// Heartbeats answered late or not at all.
    pub slow_beats: usize,
    /// Heartbeats answered past STALLED_REPLY or not at all.
    pub overdue_beats: usize,
    pub held: bool,
    /// The newest path sample's round trip and judged loss.
    pub path_rtt: Option<Duration>,
    pub loss: Option<f64>,
}

impl QualitySummary {
    /// Whether the trouble marks still add up to a run; the card's note follows this.
    fn unsteady(&self) -> bool {
        self.held || self.stalled >= UNSTEADY_STALLED_MARKS || self.late >= UNSTEADY_LATE_MARKS
    }

    /// The tier these heartbeats call for at `current`. Only heartbeats count: quinn's samples
    /// feed the unsteady run and the log, and a lower rate cannot help a radio that is away.
    fn tier(&self, current: LinkQuality) -> LinkQuality {
        let beats = (self.replies + self.unanswered).max(MIN_HEARTBEATS);
        let reaches = |count: usize, divisor: usize, tier: LinkQuality| {
            let divisor = if current >= tier {
                divisor * KEEP_DIVISOR
            } else {
                divisor
            };
            count * divisor >= beats
        };
        if self.held || reaches(self.overdue_beats, BAD_DIVISOR, LinkQuality::Bad) {
            LinkQuality::Bad
        } else if reaches(self.slow_beats, DEGRADED_DIVISOR, LinkQuality::Degraded) {
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

/// Finds a radio leaving its channel on a beat, from when overdue challenges were sent. It never
/// feeds the tier, the rate or health, and each event is O(1) over these few times.
#[derive(Debug, Default)]
struct HopDetector {
    last_overdue: Option<Duration>,
    run_first: Duration,
    run_last: Duration,
    run_len: u32,
    /// The run's latest gap; `None` until it has one.
    run_gap: Option<Duration>,
    /// The last stall of the most recent run that grew into a train and then broke.
    last_train: Option<Duration>,
    /// The stall that last turned hopping on or extended it; `None` while off.
    last_hop: Option<Duration>,
}

impl HopDetector {
    fn overdue(&mut self, sent: Duration) {
        let previous = self.last_overdue;
        // A reply can report an older send after a newer one.
        self.last_overdue = previous.max(Some(sent));
        if previous.is_none_or(|previous| sent.saturating_sub(previous) >= HOP_SAME_ABSENCE) {
            self.absence(sent);
        }
    }

    fn absence(&mut self, at: Duration) {
        let gap = at.saturating_sub(self.run_last);
        let on_beat = self.run_len > 0
            && (HOP_BEAT_MIN..=HOP_BEAT_MAX).contains(&gap)
            && self
                .run_gap
                .is_none_or(|previous| gap.abs_diff(previous) <= HOP_GAP_TOLERANCE);
        if on_beat {
            self.run_len = self.run_len.saturating_add(1);
            self.run_gap = Some(gap);
        } else {
            if self.run_len >= HOP_TRAIN {
                self.last_train = Some(self.run_last);
            }
            self.run_first = at;
            self.run_len = 1;
            self.run_gap = None;
        }
        self.run_last = at;
        if self.run_len < HOP_TRAIN {
            return;
        }
        let follows_a_train = self
            .last_train
            .is_some_and(|last| self.run_first.saturating_sub(last) <= HOP_TRAIN_WINDOW);
        if self.last_hop.is_some()
            || follows_a_train
            || at.saturating_sub(self.run_first) >= HOP_LONG_RUN
        {
            self.last_hop = Some(at);
        }
    }

    fn expire(&mut self, now: Duration) {
        if self
            .last_hop
            .is_some_and(|last| now.saturating_sub(last) >= HOP_HOLD)
        {
            self.last_hop = None;
        }
    }
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
    hops: HopDetector,
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

    /// The link keeps dropping out on a regular beat, as a radio leaving its channel does.
    pub fn radio_hopping(&self) -> bool {
        self.hops.last_hop.is_some()
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
        let delay = now.saturating_sub(sent);
        if delay >= STALLED_REPLY {
            self.hops.overdue(sent);
        }
        self.mark(now, Evidence::Reply(delay));
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
            self.hops.overdue(sent);
            self.mark(now, Evidence::Unanswered);
        }
        self.hops.expire(now);
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
                    summary.slow_beats += usize::from(severity >= Severity::Late);
                    summary.overdue_beats += usize::from(severity == Severity::Stalled);
                }
                Evidence::Unanswered => {
                    summary.unanswered += 1;
                    summary.slow_beats += 1;
                    summary.overdue_beats += 1;
                }
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
        if !summary.unsteady() {
            self.trouble = None;
        }
        let evidence = summary.tier(self.tier);
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
        play_watching(monitor, first_token, from, delays, |_, _| {})
    }

    fn play_watching(
        monitor: &mut LinkMonitor,
        first_token: u64,
        from: Duration,
        delays: &[Option<u64>],
        mut watch: impl FnMut(&LinkMonitor, Duration),
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
            watch(monitor, now);
        }
        changes
    }

    /// Plays a trace and also yields when the monitor first read as hopping.
    fn play_until_hopping(
        monitor: &mut LinkMonitor,
        first_token: u64,
        from: Duration,
        delays: &[Option<u64>],
    ) -> (Vec<(Duration, LinkQuality)>, Option<Duration>) {
        let mut first_hopping = None;
        let changes = play_watching(monitor, first_token, from, delays, |monitor, now| {
            if monitor.radio_hopping() && first_hopping.is_none() {
                first_hopping = Some(now);
            }
        });
        (changes, first_hopping)
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
    fn radio_absences_with_a_clean_link_between_keep_the_full_rate() {
        // Universal Control's 75-100 ms every 525 ms, as measured, and AirDrop's 80 every 500.
        for (length, every) in [(75, 525), (100, 525), (80, 500)] {
            let stalls: Vec<(u64, u64)> = (1..60).map(|hop| (hop * every, length)).collect();
            let mut monitor = LinkMonitor::default();
            let heartbeats = 60 * every / 30;
            let changes = play(&mut monitor, 1, ms(0), &stalled_trace(heartbeats, &stalls));
            assert!(
                changes.is_empty(),
                "{length} ms every {every} ms: {changes:?}"
            );
            assert_eq!(monitor.quality(), LinkQuality::Healthy);
        }
    }

    #[test]
    fn a_link_where_a_third_of_the_replies_queue_degrades_once_and_holds_through_a_wobble() {
        // 40% late, then 20% (under the entry line, over the keep line), then clean.
        let trace: Vec<Option<u64>> = (0..110usize)
            .map(|beat| if beat % 5 < 2 { Some(40) } else { Some(3) })
            .collect();
        let mut monitor = LinkMonitor::default();
        let mut changes = play(&mut monitor, 1, ms(0), &trace);
        assert_eq!(changes, vec![(changes[0].0, LinkQuality::Degraded)]);
        let wobble: Vec<Option<u64>> = (0..200usize)
            .map(|beat| if beat % 5 == 0 { Some(40) } else { Some(3) })
            .collect();
        changes = play(&mut monitor, 200, ms(3_500), &wobble);
        assert!(
            changes.is_empty(),
            "a share above the keep line holds the tier: {changes:?}"
        );
        assert_eq!(monitor.quality(), LinkQuality::Degraded);
        let changes = play(&mut monitor, 500, ms(9_700), &vec![Some(3); 200]);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].1, LinkQuality::Healthy);
    }

    #[test]
    fn replies_that_mostly_arrive_overdue_make_the_link_bad_within_the_window() {
        let mut monitor = LinkMonitor::default();
        let trace = vec![Some(70u64); MIN_HEARTBEATS.div_ceil(BAD_DIVISOR) + 4];
        let changes = play(&mut monitor, 1, ms(0), &trace);
        assert_eq!(changes.last().map(|(_, to)| *to), Some(LinkQuality::Bad));
        assert!(changes.last().unwrap().0 < ms(30 * trace.len() as u64 + 70));
    }

    #[test]
    fn a_stall_that_keeps_going_is_bad_before_any_reply_arrives() {
        let mut monitor = LinkMonitor::default();
        let changes = play(&mut monitor, 1, ms(0), &stalled_trace(30, &[(60, 1_200)]));
        let bad = changes
            .iter()
            .find(|(_, to)| *to == LinkQuality::Bad)
            .expect("the outage makes the link bad");
        assert!(
            bad.0 < ms(60 + 1_200),
            "bad while the outage is still running, at {:?}",
            bad.0
        );
    }

    #[test]
    fn hiccups_a_healthy_session_rides_through_keep_the_full_rate() {
        for (length, every) in [(100, 1_500), (120, 5_000), (150, 5_000), (250, 10_000)] {
            let stalls: Vec<(u64, u64)> = (0..4).map(|n| (500 + n * every, length)).collect();
            let heartbeats = (500 + 4 * every) / 30;
            let mut monitor = LinkMonitor::default();
            let changes = play(&mut monitor, 1, ms(0), &stalled_trace(heartbeats, &stalls));
            assert!(
                changes.is_empty(),
                "{length} ms every {every} ms: {changes:?}"
            );
        }
    }

    #[test]
    fn recovery_climbs_one_tier_at_a_time_after_a_clean_stretch() {
        let mut monitor = LinkMonitor::default();
        assert_eq!(
            monitor.held(ms(1_000)).map(|change| change.to),
            Some(LinkQuality::Bad)
        );
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
        let aged = ms(1_000) + QUALITY_WINDOW;
        let changes = idle(&mut monitor, ms(1_000), aged + UPGRADE_AFTER * 3);
        // The hold ages out of the window, then each tier waits out its own clean stretch.
        assert_eq!(
            changes,
            vec![
                (aged + UPGRADE_AFTER, LinkQuality::Degraded),
                (aged + ms(1) + UPGRADE_AFTER * 2, LinkQuality::Healthy),
            ]
        );
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
    }

    #[test]
    fn fresh_evidence_restarts_the_clean_stretch_and_can_drop_two_tiers_at_once() {
        let mut monitor = LinkMonitor::default();
        let late = [Some(35); MIN_HEARTBEATS.div_ceil(DEGRADED_DIVISOR)];
        assert_eq!(
            play(&mut monitor, 1, ms(0), &late)
                .last()
                .map(|(_, to)| *to),
            Some(LinkQuality::Degraded)
        );
        // Late again partway through the clean stretch: the climb waits a full stretch from there.
        assert!(idle(&mut monitor, ms(500), ms(4_500)).is_empty());
        assert!(play(&mut monitor, 20, ms(4_500), &late).is_empty());
        let changes = idle(&mut monitor, ms(5_000), ms(10_000));
        assert_eq!(
            changes,
            vec![(
                ms(4_535 + 5 * 30) + QUALITY_WINDOW + UPGRADE_AFTER,
                LinkQuality::Healthy
            )]
        );
        // An outage from healthy degrades first and turns bad as the overdue share climbs.
        let outage = [None; MIN_HEARTBEATS.div_ceil(BAD_DIVISOR) + 2];
        let changes = play(&mut monitor, 40, ms(11_000), &outage);
        assert_eq!(changes.last().map(|(_, to)| *to), Some(LinkQuality::Bad));
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
        for length in [400, 1_000] {
            let mut monitor = LinkMonitor::default();
            // The trace ends inside the window, so the run is still live and at its longest.
            let heartbeats = (300 + length + 100) / 30;
            let trace = stalled_trace(heartbeats, &[(300, length)]);
            play(&mut monitor, 1, ms(0), &trace);
            assert_ne!(monitor.quality(), LinkQuality::Healthy, "{length} ms stall");
            let mut peak = monitor.unsteady_for();
            for now in (length + 600)..12_000 {
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
        // The rate never dropped: the note's run reads trouble the tier does not act on.
        assert_eq!(monitor.quality(), LinkQuality::Healthy);
        idle(&mut monitor, ms(5_300), ms(6_000));
        assert_eq!(monitor.unsteady_for(), Duration::ZERO);
    }

    #[test]
    fn an_outage_that_lasts_earns_the_note_while_it_runs() {
        let mut monitor = LinkMonitor::default();
        play(&mut monitor, 1, ms(0), &[None; 100]);
        assert!(monitor.unsteady_for() >= NOTE_AFTER);
    }

    #[test]
    fn quinn_loss_and_round_trip_feed_the_unsteady_run_but_never_the_rate() {
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
        for span in 3..=UNSTEADY_LATE_MARKS as u64 + 1 {
            assert_eq!(monitor.path(sample(35, 8, 400), ms(250 * span)), None);
        }
        assert_eq!(monitor.summary().late, UNSTEADY_LATE_MARKS);
        let later = ms(250 * (UNSTEADY_LATE_MARKS as u64 + 2));
        assert_eq!(monitor.path(sample(35, 8, 400), later), None);
        assert_eq!(monitor.quality(), LinkQuality::Healthy);
        assert!(
            monitor.unsteady_for() > Duration::ZERO,
            "late round trips still run the note"
        );
        let mut monitor = LinkMonitor::default();
        for index in 0..UNSTEADY_STALLED_MARKS as u64 {
            assert_eq!(monitor.path(sample(70, 0, 0), ms(250 * index)), None);
        }
        assert_eq!(monitor.quality(), LinkQuality::Healthy);
    }

    /// Absences of `length` ms every 525 ms, which heartbeats quantize to 510 and 540 ms gaps.
    fn radio_absences(span: u64, length: u64) -> Vec<(u64, u64)> {
        (1..span / 525).map(|hop| (hop * 525, length)).collect()
    }

    fn radio_trace(span: u64, length: u64) -> (Vec<(u64, u64)>, Vec<Option<u64>>) {
        let stalls = radio_absences(span, length);
        let trace = stalled_trace(span / 30, &stalls);
        (stalls, trace)
    }

    #[test]
    fn a_radio_on_a_steady_beat_reads_as_hopping_and_keeps_the_full_rate() {
        for length in [75, 102] {
            let (_, trace) = radio_trace(60_000, length);
            let mut monitor = LinkMonitor::default();
            let (changes, hopping) = play_until_hopping(&mut monitor, 1, ms(0), &trace);
            assert!(changes.is_empty(), "{length} ms: {changes:?}");
            assert!(
                hopping.is_some_and(|at| at <= ms(25_000)),
                "{length} ms: first hopping at {hopping:?}"
            );
            assert!(monitor.radio_hopping(), "{length} ms");
            assert_eq!(monitor.quality(), LinkQuality::Healthy, "{length} ms");
        }
    }

    #[test]
    fn bursts_minutes_apart_read_as_hopping_from_the_second_full_train() {
        let (stalls, trace) = radio_trace(8_000, 75);
        let sixth_absence_overdue = ms(stalls[5].0) + STALLED_REPLY;
        let mut monitor = LinkMonitor::default();
        let mut idle_from = ms(0);
        for (burst, offset) in [0, 47, 112, 189, 266, 370].into_iter().enumerate() {
            let start = ms(offset * 1_000);
            idle(&mut monitor, idle_from, start.saturating_sub(ms(1)));
            let token = 1 + burst as u64 * 1_000;
            let (_, hopping) = play_until_hopping(&mut monitor, token, start, &trace);
            match burst {
                0 => assert_eq!(hopping, None),
                1 => assert_eq!(hopping, Some(start + sixth_absence_overdue)),
                _ => assert_eq!(hopping, Some(start), "burst at {offset} s"),
            }
            assert_eq!(monitor.radio_hopping(), burst > 0, "burst at {offset} s");
            idle_from = start + ms(30 * trace.len() as u64 + 201);
        }
    }

    #[test]
    fn isolated_stalls_never_read_as_hopping() {
        let stalls = [10_020, 30_000, 49_980, 70_020].map(|at| (at, 70));
        let mut monitor = LinkMonitor::default();
        let trace = stalled_trace(150_000 / 30, &stalls);
        let (_, hopping) = play_until_hopping(&mut monitor, 1, ms(0), &trace);
        assert_eq!(hopping, None);
        assert_eq!(
            monitor.hops.run_last,
            ms(stalls[3].0),
            "the stalls were seen"
        );
        assert_eq!(monitor.hops.run_len, 1);
    }

    #[test]
    fn hopping_ends_two_minutes_after_the_last_train_stall() {
        let (stalls, trace) = radio_trace(8_000, 75);
        let mut monitor = LinkMonitor::default();
        play(&mut monitor, 1, ms(0), &trace);
        let second = ms(30_000);
        play(&mut monitor, 1_000, second, &trace);
        assert!(monitor.radio_hopping());
        let last_stall = second + ms(stalls[stalls.len() - 1].0);
        monitor.tick(last_stall + ms(119_999));
        assert!(monitor.radio_hopping(), "still inside the two minutes");
        monitor.tick(last_stall + ms(120_000));
        assert!(!monitor.radio_hopping());
    }

    #[test]
    fn a_second_train_counts_for_five_minutes_after_the_first() {
        let (stalls, trace) = radio_trace(8_000, 75);
        let first_train_ends = stalls[stalls.len() - 1].0;
        // The first absence starts mid-beat, so the heartbeat that sees it goes out 15 ms in.
        let second_train_seen = stalls[0].0 + 15;
        for (past_the_window, hopping) in [(0, true), (1, false)] {
            let start = first_train_ends + 300_000 + past_the_window - second_train_seen;
            let mut monitor = LinkMonitor::default();
            play(&mut monitor, 1, ms(0), &trace);
            idle(
                &mut monitor,
                ms(30 * trace.len() as u64 + 201),
                ms(start - 1),
            );
            play(&mut monitor, 1_000, ms(start), &trace);
            assert_eq!(
                monitor.radio_hopping(),
                hopping,
                "{past_the_window} ms past"
            );
        }
    }

    #[test]
    fn a_reply_that_beats_the_tick_still_marks_an_absence() {
        for (delay, hopping) in [(STALLED_REPLY, true), (STALLED_REPLY - ms(1), false)] {
            let mut monitor = LinkMonitor::default();
            for beat in 0..60 {
                let sent = ms(beat * 525);
                monitor.challenge_sent(beat + 1, sent);
                monitor.reply(beat + 1, sent + delay);
                monitor.tick(sent + delay + ms(1));
            }
            assert_eq!(monitor.radio_hopping(), hopping, "{delay:?}");
        }
    }

    #[test]
    fn a_reply_for_an_older_send_cannot_split_an_absence() {
        let mut hops = HopDetector::default();
        hops.overdue(ms(1_000));
        hops.overdue(ms(500));
        hops.overdue(ms(1_100));
        assert_eq!((hops.run_first, hops.run_len), (ms(1_000), 1));
    }

    #[test]
    fn gaps_on_the_beat_must_also_hold_steady() {
        for (short, long, hopping) in [(480, 570, false), (510, 570, true)] {
            let mut at = 0;
            let stalls: Vec<(u64, u64)> = (0..240)
                .map(|hop| {
                    at += if hop % 2 == 0 { short } else { long };
                    (at, 75)
                })
                .collect();
            let trace = stalled_trace((at + 1_000) / 30, &stalls);
            let mut monitor = LinkMonitor::default();
            let (_, first) = play_until_hopping(&mut monitor, 1, ms(0), &trace);
            assert_eq!(first.is_some(), hopping, "{short}/{long}: {first:?}");
            assert_eq!(
                monitor.hops.last_overdue,
                Some(ms(at)),
                "the stalls were seen"
            );
        }
    }

    const TEN_MINUTES: u64 = 600_000;

    /// Xorshift, seeded so a failure replays exactly.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn stalls_at_random_never_read_as_hopping() {
        for seed in 1..=3 {
            let mut rng = Rng::new(seed);
            let mut stalls = Vec::new();
            let mut at = 0.0;
            loop {
                // Exponential gaps averaging one stall a second.
                at -= (1.0 - rng.unit()).ln() * 1_000.0;
                if at >= TEN_MINUTES as f64 {
                    break;
                }
                stalls.push((at as u64, 90));
            }
            let trace = stalled_trace(TEN_MINUTES / 30, &stalls);
            let mut monitor = LinkMonitor::default();
            let (_, hopping) = play_until_hopping(&mut monitor, 1, ms(0), &trace);
            assert_eq!(hopping, None, "seed {seed}");
            assert!(monitor.hops.last_overdue.is_some(), "seed {seed}");
        }
    }

    #[test]
    fn heartbeats_late_at_random_never_read_as_hopping() {
        for seed in 1..=3 {
            let mut rng = Rng::new(seed);
            let trace: Vec<Option<u64>> = (0..TEN_MINUTES / 30)
                .map(|_| Some(if rng.unit() < 0.05 { 70 } else { 2 }))
                .collect();
            let mut monitor = LinkMonitor::default();
            let (_, hopping) = play_until_hopping(&mut monitor, 1, ms(0), &trace);
            assert_eq!(hopping, None, "seed {seed}");
            assert!(monitor.hops.last_overdue.is_some(), "seed {seed}");
        }
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
            "in the last 3000 ms: 1 replies, 0 unanswered, 1 late, 0 stalled, slowest 42 ms; path rtt 6.5 ms"
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
