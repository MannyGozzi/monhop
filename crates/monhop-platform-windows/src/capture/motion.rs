//! Pointer motion sent to another computer at the mouse's own report rate. Windows 11 merges a
//! background window's raw input to about 125 messages a second, while the low-level hook sees
//! every report: each withheld hook move is sent at once as the pixels Windows would have moved
//! the cursor, and the raw sample that later covers it sends only what the ballistics model gives
//! beyond that, so every raw sample's motion is sent exactly once.

use std::time::Duration;

use monhop_core::pointer_ballistics::{Ballistics, MAX_MERGED_REPORTS};

/// Overshoot owed back is paid from at most this share of each later move along its axis, so the
/// pointer never reverses or stalls mid-motion: a reversal at an edge would end a push there.
const ABSORB_SHARE: f64 = 0.5;

/// Once the hand has rested this long, overshoot still owed is sent back and hook moves no raw
/// sample covered stand as sent; a push that pauses this long starts over anyway.
pub(crate) const SETTLE_AFTER: Duration = Duration::from_millis(100);

/// DIPs below this are float rounding, kept owed instead of sent as motion.
const NEGLIGIBLE: f64 = 1e-9;

/// Hook moves are scaled toward what the model gave the reports they predicted, learned only from
/// samples whose moves predicted at least this many DIPs, where a pixel of rounding is noise.
const LEARN_FROM: f64 = 8.0;
/// Each such sample moves the scale this share of the way, within these bounds.
const SCALE_SMOOTHING: f64 = 0.25;
const SCALE_LIMITS: (f64, f64) = (0.25, 4.0);

const RATE_BUCKET: Duration = Duration::from_millis(10);
/// One second of buckets: the peak rate is over any second, to the bucket.
const RATE_BUCKETS: usize = 100;

/// What one raw sample sends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RawMotion {
    /// No hook move was sent for its reports: the sample as raw counts and their gain.
    Whole { dx: i32, dy: i32, gain: f64 },
    /// DIPs its reports moved beyond what their hook moves sent.
    Correction { dx: f64, dy: f64 },
    /// What the hook sent covers it, or what the hook overshot is owed back later.
    Covered,
}

#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    /// Predicted DIPs sent from the hook since the last raw sample.
    predicted: f64,
    /// DIPs the raw samples gave beyond what was sent for them; owed against the heading is
    /// overshoot. Invariant: `owed - predicted` is everything the raw samples gave less
    /// everything sent.
    owed: f64,
}

impl Axis {
    /// The DIPs to send for a hook move predicted to move `predicted`.
    fn send(&mut self, predicted: f64) -> f64 {
        self.predicted += predicted;
        let paid = if self.owed * predicted > 0.0 {
            self.owed
        } else if self.owed * predicted < 0.0 {
            self.owed.signum() * self.owed.abs().min(predicted.abs() * ABSORB_SHARE)
        } else {
            0.0
        };
        self.owed -= paid;
        predicted + paid
    }

    /// Settles a raw sample that moved `truth` DIPs; yields what to send now. Owed along the
    /// heading is sent; owed against it waits for later moves or rest.
    fn reconcile(&mut self, truth: f64) -> f64 {
        let heading = if self.predicted == 0.0 {
            truth
        } else {
            self.predicted
        };
        self.owed += truth - std::mem::take(&mut self.predicted);
        if self.owed * heading > 0.0 {
            self.take_owed()
        } else {
            0.0
        }
    }

    fn take_owed(&mut self) -> f64 {
        if self.owed.abs() < NEGLIGIBLE {
            return 0.0;
        }
        std::mem::take(&mut self.owed)
    }
}

/// The ledger between hook moves sent to another computer and the raw samples that cover them.
#[derive(Clone, Debug)]
pub(crate) struct RemoteMotion {
    pixels_per_dip: f64,
    /// Model DIPs per DIP Windows moved the cursor, as recent samples measured it.
    scale: f64,
    /// Unscaled DIPs Windows would have moved for the hook moves since the last raw sample.
    withheld: (f64, f64),
    x: Axis,
    y: Axis,
    /// Physical hook moves since the last raw sample with motion: the reports it merges.
    reports: u32,
    /// A hook move since the last raw sample was sent from the hook.
    hook_sent: bool,
    /// A sourced raw sample arrived since the hand last rested, so raw samples cover this motion.
    sourced_in_run: bool,
    last_move: Option<Duration>,
    stats: MotionStats,
}

impl RemoteMotion {
    /// `pixels_per_dip` is the primary display's, which turns the pixels Windows moves into DIPs.
    pub(crate) fn new(pixels_per_dip: f64) -> Self {
        let mut motion = Self {
            pixels_per_dip: 1.0,
            scale: 1.0,
            withheld: (0.0, 0.0),
            x: Axis::default(),
            y: Axis::default(),
            reports: 0,
            hook_sent: false,
            sourced_in_run: false,
            last_move: None,
            stats: MotionStats::default(),
        };
        motion.set_pixels_per_dip(pixels_per_dip);
        motion
    }

    pub(crate) fn set_pixels_per_dip(&mut self, pixels_per_dip: f64) {
        if pixels_per_dip.is_finite() && pixels_per_dip > 0.0 {
            self.pixels_per_dip = pixels_per_dip;
        }
    }

    /// A physical hook move at `now`. `withheld` is the pixels Windows would have moved the cursor
    /// for a move withheld for another computer; yields the DIPs to send for it now, if any.
    pub(crate) fn hook_move(
        &mut self,
        withheld: Option<(f64, f64)>,
        now: Duration,
    ) -> Option<(f64, f64)> {
        self.reports = self.reports.saturating_add(1);
        self.last_move = Some(now);
        let (x, y) = withheld?;
        let (x, y) = (x / self.pixels_per_dip, y / self.pixels_per_dip);
        self.hook_sent = true;
        self.withheld = (self.withheld.0 + x, self.withheld.1 + y);
        let sent = (self.x.send(x * self.scale), self.y.send(y * self.scale));
        if sent == (0.0, 0.0) {
            return None;
        }
        self.stats.hook_moves += 1;
        self.stats.sent(now);
        Some(sent)
    }

    /// A sourced raw sample of `dx`, `dy` counts with motion, moved as `ballistics` model it.
    pub(crate) fn raw(
        &mut self,
        dx: i32,
        dy: i32,
        ballistics: &mut Ballistics,
        now: Duration,
    ) -> RawMotion {
        self.sourced_in_run = true;
        let reports = std::mem::take(&mut self.reports).clamp(1, MAX_MERGED_REPORTS);
        let gain = ballistics.gain(f64::from(dx), f64::from(dy), reports);
        if !std::mem::take(&mut self.hook_sent) {
            self.stats.whole_samples += 1;
            self.stats.sent(now);
            return RawMotion::Whole { dx, dy, gain };
        }
        let truth = (f64::from(dx) * gain, f64::from(dy) * gain);
        self.learn(truth);
        let x = self.x.reconcile(truth.0);
        let y = self.y.reconcile(truth.1);
        if (x, y) == (0.0, 0.0) {
            return RawMotion::Covered;
        }
        self.stats.corrections += 1;
        self.stats.correction_dips += x.hypot(y);
        self.stats.sent(now);
        RawMotion::Correction { dx: x, dy: y }
    }

    /// A raw sample with no device or absolute coordinates: hook moves sent since the last raw
    /// sample are its, which no sourced sample will cover, so they stand as sent. While sourced
    /// samples cover this motion it is someone else's injected input, and owns none of them.
    pub(crate) fn unsourced(&mut self) {
        if !self.sourced_in_run {
            self.stand_uncovered();
        }
    }

    /// Once the hand has rested [`SETTLE_AFTER`], yields overshoot still owed back.
    pub(crate) fn settle(&mut self, now: Duration) -> Option<(f64, f64)> {
        let rested = self
            .last_move
            .is_some_and(|last| now.saturating_sub(last) >= SETTLE_AFTER);
        if !rested {
            return None;
        }
        self.last_move = None;
        if std::mem::take(&mut self.sourced_in_run) {
            self.charge_back_uncovered();
        } else {
            self.stand_uncovered();
        }
        let owed = (self.x.take_owed(), self.y.take_owed());
        if owed == (0.0, 0.0) {
            return None;
        }
        self.stats.returns += 1;
        self.stats.returned_dips += owed.0.hypot(owed.1);
        self.stats.sent(now);
        Some(owed)
    }

    /// Least squares along the hook moves: how far the model moved what they predicted.
    fn learn(&mut self, truth: (f64, f64)) {
        let (x, y) = std::mem::take(&mut self.withheld);
        let length = x.hypot(y);
        if length < LEARN_FROM {
            return;
        }
        let ratio =
            ((truth.0 * x + truth.1 * y) / (length * length)).clamp(SCALE_LIMITS.0, SCALE_LIMITS.1);
        self.scale += (ratio - self.scale) * SCALE_SMOOTHING;
    }

    /// Raw samples cover this motion, so hook moves still uncovered at rest were paid for by a
    /// sample handled before their hook. One that does arrive later is sent whole: totals hold.
    fn charge_back_uncovered(&mut self) {
        if std::mem::take(&mut self.hook_sent) {
            self.stats.charged_back += 1;
            self.withheld = (0.0, 0.0);
            self.reports = 0;
            self.x.owed -= std::mem::take(&mut self.x.predicted);
            self.y.owed -= std::mem::take(&mut self.y.predicted);
        }
    }

    fn stand_uncovered(&mut self) {
        if std::mem::take(&mut self.hook_sent) {
            self.stats.uncovered += 1;
            self.withheld = (0.0, 0.0);
            self.x.predicted = 0.0;
            self.y.predicted = 0.0;
        }
    }

    /// One log line of counts for a capture that ran `elapsed`.
    pub(crate) fn summary(&self, elapsed: Duration) -> String {
        let stats = &self.stats;
        format!(
            "pointer motion: {} events in {:.1} s ({} hook moves, {} whole raw samples), peak {} in any second; {} raw corrections totalling {:.1} DIPs; {} overshoots returned totalling {:.1} DIPs; {} hook runs no raw sample covered; {} charged back; hook-to-model scale {:.3}",
            stats.hook_moves + stats.whole_samples + stats.corrections + stats.returns,
            elapsed.as_secs_f64(),
            stats.hook_moves,
            stats.whole_samples,
            stats.peak,
            stats.corrections,
            stats.correction_dips,
            stats.returns,
            stats.returned_dips,
            stats.uncovered,
            stats.charged_back,
            self.scale,
        )
    }
}

#[derive(Clone, Debug)]
struct MotionStats {
    hook_moves: u64,
    whole_samples: u64,
    corrections: u64,
    correction_dips: f64,
    returns: u64,
    returned_dips: f64,
    /// Runs of hook moves left standing without a raw sample.
    uncovered: u64,
    /// Runs of hook moves a raw sample handled before their hook had already paid for.
    charged_back: u64,
    buckets: [u32; RATE_BUCKETS],
    /// The bucket the latest event fell in, counted from capture start.
    bucket: u64,
    in_window: u32,
    peak: u32,
}

impl Default for MotionStats {
    fn default() -> Self {
        Self {
            hook_moves: 0,
            whole_samples: 0,
            corrections: 0,
            correction_dips: 0.0,
            returns: 0,
            returned_dips: 0.0,
            uncovered: 0,
            charged_back: 0,
            buckets: [0; RATE_BUCKETS],
            bucket: 0,
            in_window: 0,
            peak: 0,
        }
    }
}

impl MotionStats {
    /// Counts one motion event at `now` toward the busiest second.
    fn sent(&mut self, now: Duration) {
        let bucket = (now.as_nanos() / RATE_BUCKET.as_nanos()) as u64;
        let stale = bucket.saturating_sub(self.bucket).min(RATE_BUCKETS as u64);
        for step in 1..=stale {
            let slot = &mut self.buckets[((self.bucket + step) % RATE_BUCKETS as u64) as usize];
            self.in_window -= std::mem::take(slot);
        }
        self.bucket = self.bucket.max(bucket);
        let slot = &mut self.buckets[(self.bucket % RATE_BUCKETS as u64) as usize];
        *slot = slot.saturating_add(1);
        self.in_window = self.in_window.saturating_add(1);
        self.peak = self.peak.max(self.in_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn at(ms: u64) -> Duration {
        MS * ms as u32
    }

    /// Everything one raw sample of `counts` sends today, in DIPs.
    fn modelled(windows: &mut Ballistics, counts: (i32, i32), reports: u32) -> (f64, f64) {
        let gain = windows.gain(f64::from(counts.0), f64::from(counts.1), reports);
        (f64::from(counts.0) * gain, f64::from(counts.1) * gain)
    }

    fn dips(raw: RawMotion) -> (f64, f64) {
        match raw {
            RawMotion::Whole { dx, dy, gain } => (f64::from(dx) * gain, f64::from(dy) * gain),
            RawMotion::Correction { dx, dy } => (dx, dy),
            RawMotion::Covered => (0.0, 0.0),
        }
    }

    fn add(total: &mut (f64, f64), step: (f64, f64)) {
        total.0 += step.0;
        total.1 += step.1;
    }

    fn close(actual: (f64, f64), expected: (f64, f64)) {
        assert!(
            (actual.0 - expected.0).abs() < 1e-9 && (actual.1 - expected.1).abs() < 1e-9,
            "{actual:?} is not {expected:?}"
        );
    }

    #[test]
    fn eight_withheld_moves_are_sent_at_once_and_their_raw_sample_settles_the_total() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        // Eight 2-count reports at about 1.28 DIPs each, which Windows moves 1 or 2 pixels.
        let pixels = [1, 1, 2, 1, 1, 2, 1, 1];
        let mut sent = (0.0, 0.0);
        for (ms, x) in pixels.into_iter().enumerate() {
            let step = motion
                .hook_move(Some((f64::from(x), 0.0)), at(ms as u64))
                .expect("every withheld move is sent at once");
            assert_eq!(step, (f64::from(x), 0.0));
            add(&mut sent, step);
        }
        let truth = modelled(&mut model, (16, 0), 8);
        assert!(truth.0 > sent.0, "the hook undershot the model here");
        let raw = motion.raw(16, 0, &mut windows, at(8));
        assert!(matches!(raw, RawMotion::Correction { .. }));
        add(&mut sent, dips(raw));
        close(sent, truth);
        assert_eq!(motion.stats.hook_moves, 8);
        assert_eq!(motion.stats.corrections, 1);
    }

    #[test]
    fn overshoot_is_paid_from_later_moves_without_reversing_and_the_rest_at_rest() {
        let mut motion = RemoteMotion::new(1.0);
        let (mut windows, mut model) = (Ballistics::default(), Ballistics::default());
        let mut sent = (0.0, 0.0);
        let mut truth = (0.0, 0.0);
        for ms in 0..8 {
            add(
                &mut sent,
                motion.hook_move(Some((2.0, -1.0)), at(ms)).unwrap(),
            );
        }
        // 8 reports of (1, -1) counts model to about 4.6 DIPs each way, short of the 16, -8 sent.
        add(&mut truth, modelled(&mut model, (8, -8), 8));
        assert_eq!(motion.raw(8, -8, &mut windows, at(8)), RawMotion::Covered);
        assert!(
            motion.x.owed < 0.0 && motion.y.owed > 0.0,
            "both axes overshot"
        );
        assert!(
            motion.scale < 1.0,
            "later moves are scaled toward the model"
        );
        for ms in 8..16 {
            let step = motion.hook_move(Some((2.0, -1.0)), at(ms)).unwrap();
            assert!(
                step.0 > 0.0 && step.1 < 0.0,
                "never reverses or stalls: {step:?}"
            );
            add(&mut sent, step);
        }
        add(&mut truth, modelled(&mut model, (8, -8), 8));
        add(&mut sent, dips(motion.raw(8, -8, &mut windows, at(16))));
        // Rest counts from the last hook move.
        assert_eq!(motion.settle(at(15) + SETTLE_AFTER - MS), None);
        add(&mut sent, motion.settle(at(15) + SETTLE_AFTER).unwrap());
        close(sent, truth);
        assert_eq!(motion.settle(at(15) + SETTLE_AFTER * 2), None);
        assert_eq!(motion.stats.returns, 1);
    }

    #[test]
    fn a_raw_sample_with_no_hook_moves_is_sent_whole_as_today() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        let gain = model.gain(7.0, -3.0, 1);
        assert_eq!(
            motion.raw(7, -3, &mut windows, at(0)),
            RawMotion::Whole {
                dx: 7,
                dy: -3,
                gain
            }
        );
        // Local moves the hook let through only count the reports the next sample merges.
        for ms in 1..=3 {
            assert_eq!(motion.hook_move(None, at(ms)), None);
        }
        let gain = model.gain(7.0, -3.0, 3);
        assert_eq!(
            motion.raw(7, -3, &mut windows, at(4)),
            RawMotion::Whole {
                dx: 7,
                dy: -3,
                gain
            }
        );
        assert_eq!(motion.stats.whole_samples, 2);
    }

    #[test]
    fn hook_moves_no_raw_sample_covers_stand_as_sent() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        assert_eq!(motion.hook_move(Some((3.0, 1.0)), at(0)), Some((3.0, 1.0)));
        // Capture may end here: the move was sent once and nothing is fabricated for it.
        assert_eq!(motion.settle(at(0) + SETTLE_AFTER), None);
        assert_eq!(motion.stats.uncovered, 1);
        // A later sample is not charged for it.
        let gain = Ballistics::default().gain(7.0, 0.0, 1);
        assert_eq!(
            motion.raw(7, 0, &mut windows, at(500)),
            RawMotion::Whole { dx: 7, dy: 0, gain }
        );
    }

    #[test]
    fn an_unsourced_sample_owns_the_hook_moves_before_it() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        assert!(motion.hook_move(Some((40.0, 0.0)), at(0)).is_some());
        motion.unsourced();
        let mut model = Ballistics::default();
        let gain = model.gain(7.0, 0.0, 2);
        assert!(motion.hook_move(None, at(1)).is_none());
        // The merged report count still includes the unsourced sample's move, as before.
        assert_eq!(
            motion.raw(7, 0, &mut windows, at(2)),
            RawMotion::Whole { dx: 7, dy: 0, gain }
        );
    }

    #[test]
    fn a_withheld_move_windows_rounds_to_no_pixel_is_settled_by_its_raw_sample() {
        let mut motion = RemoteMotion::new(1.5);
        let mut windows = Ballistics::default();
        assert_eq!(motion.hook_move(Some((0.0, 0.0)), at(0)), None);
        assert_eq!(motion.hook_move(Some((3.0, 0.0)), at(1)), Some((2.0, 0.0)));
        let truth = modelled(&mut Ballistics::default(), (3, 0), 2);
        let mut sent = (2.0, 0.0);
        add(&mut sent, dips(motion.raw(3, 0, &mut windows, at(2))));
        add(
            &mut sent,
            motion.settle(at(2) + SETTLE_AFTER).unwrap_or_default(),
        );
        close(sent, truth);
    }

    #[test]
    fn a_hand_reversing_is_sent_what_it_was_owed_at_once() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        let mut sent = (0.0, 0.0);
        add(
            &mut sent,
            motion.hook_move(Some((10.0, 0.0)), at(0)).unwrap(),
        );
        let mut truth = modelled(&mut model, (4, 0), 1);
        add(&mut sent, dips(motion.raw(4, 0, &mut windows, at(1))));
        let owed = motion.x.owed;
        assert!(owed < 0.0);
        // Moving back the other way carries what was owed back with it.
        let step = motion.hook_move(Some((-1.0, 0.0)), at(2)).unwrap();
        close(step, (-motion.scale + owed, 0.0));
        assert_eq!(motion.x.owed, 0.0);
        add(&mut sent, step);
        add(&mut truth, modelled(&mut model, (-1, 0), 1));
        add(&mut sent, dips(motion.raw(-1, 0, &mut windows, at(3))));
        add(
            &mut sent,
            motion.settle(at(3) + SETTLE_AFTER).unwrap_or_default(),
        );
        close(sent, truth);
    }

    #[test]
    fn a_thousand_reports_a_second_keep_the_model_total_exactly() {
        // A hand speeding up and slowing down over a second of 1000 Hz reports, merged 8 to a raw
        // sample, with Windows' pixels truncated and carried as it does.
        let mut motion = RemoteMotion::new(1.0);
        let (mut windows, mut model, mut pixels) = (
            Ballistics::default(),
            Ballistics::default(),
            Ballistics::default(),
        );
        let (mut sent, mut truth, mut carry) = ((0.0, 0.0), (0.0, 0.0), (0.0, 0.0));
        let mut sample = (0, 0);
        let mut longest_gap = Duration::ZERO;
        let mut last = Duration::ZERO;
        for ms in 0..1000_u64 {
            let speed = 1 + (ms as i32 % 400 - 200).abs() / 12;
            let counts = (speed, -speed / 3);
            let moved = pixels.apply(f64::from(counts.0), f64::from(counts.1), 1);
            carry = (carry.0 + moved.0, carry.1 + moved.1);
            let whole = (carry.0.trunc(), carry.1.trunc());
            carry = (carry.0 - whole.0, carry.1 - whole.1);
            if let Some(step) = motion.hook_move(Some(whole), at(ms)) {
                assert!(step.0 >= 0.0 && step.1 <= 0.0, "never reverses: {step:?}");
                longest_gap = longest_gap.max(at(ms) - last);
                last = at(ms);
                add(&mut sent, step);
            }
            sample = (sample.0 + counts.0, sample.1 + counts.1);
            if ms % 8 == 7 {
                add(&mut truth, modelled(&mut model, sample, 8));
                add(
                    &mut sent,
                    dips(motion.raw(sample.0, sample.1, &mut windows, at(ms))),
                );
                sample = (0, 0);
            }
        }
        add(
            &mut sent,
            motion.settle(at(1000) + SETTLE_AFTER).unwrap_or_default(),
        );
        close(sent, truth);
        assert!(
            motion.stats.hook_moves >= 900,
            "{}",
            motion.stats.hook_moves
        );
        assert!(longest_gap <= at(3), "{longest_gap:?}");
        assert_eq!(
            u64::from(motion.stats.peak),
            motion.stats.hook_moves + motion.stats.corrections
        );
        assert!((motion.scale - 1.0).abs() < 0.1, "{}", motion.scale);
    }

    #[test]
    fn the_peak_counts_the_busiest_second_only() {
        let mut stats = MotionStats::default();
        for ms in 0..500 {
            stats.sent(at(ms));
        }
        for ms in 2000..2300 {
            stats.sent(at(ms));
        }
        assert_eq!(stats.peak, 500);
        // A second later the first burst has left the window.
        assert_eq!(stats.in_window, 300);
        let mut steady = MotionStats::default();
        for ms in 0..3000 {
            steady.sent(at(ms));
        }
        assert!((990..=1000).contains(&steady.peak), "{}", steady.peak);
    }

    #[test]
    fn a_raw_sample_handled_before_its_hook_is_not_sent_twice() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        let truth = modelled(&mut model, (8, 0), 7);
        let mut sent = (0.0, 0.0);
        for report in 0..7 {
            add(
                &mut sent,
                motion.hook_move(Some((3.0, 0.0)), at(report)).unwrap(),
            );
        }
        // The sample holds all eight reports, the eighth's hook runs after it, then the hand stops.
        add(&mut sent, dips(motion.raw(8, 0, &mut windows, at(7))));
        add(
            &mut sent,
            motion.hook_move(Some((3.0, 0.0)), at(8)).unwrap(),
        );
        add(
            &mut sent,
            motion.settle(at(8) + SETTLE_AFTER).unwrap_or_default(),
        );
        close(sent, truth);
        assert_eq!(motion.stats.charged_back, 1);
        assert_eq!(motion.stats.uncovered, 0);
    }

    #[test]
    fn a_raw_sample_arriving_after_the_rest_it_missed_keeps_the_total() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        let first = modelled(&mut model, (8, 0), 1);
        let late = modelled(&mut model, (4, 0), 1);
        let mut sent = (0.0, 0.0);
        add(
            &mut sent,
            motion.hook_move(Some((6.0, 0.0)), at(0)).unwrap(),
        );
        add(&mut sent, dips(motion.raw(8, 0, &mut windows, at(1))));
        add(
            &mut sent,
            motion.hook_move(Some((3.0, 0.0)), at(2)).unwrap(),
        );
        add(
            &mut sent,
            motion.settle(at(2) + SETTLE_AFTER).unwrap_or_default(),
        );
        add(&mut sent, dips(motion.raw(4, 0, &mut windows, at(200))));
        close(sent, (first.0 + late.0, first.1 + late.1));
    }

    #[test]
    fn another_programs_injected_sample_owns_no_hook_moves_of_a_covered_motion() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        let mut model = Ballistics::default();
        let mut truth = (0.0, 0.0);
        let mut sent = (0.0, 0.0);
        add(
            &mut sent,
            motion.hook_move(Some((6.0, 0.0)), at(0)).unwrap(),
        );
        add(&mut truth, modelled(&mut model, (8, 0), 1));
        add(&mut sent, dips(motion.raw(8, 0, &mut windows, at(1))));
        add(
            &mut sent,
            motion.hook_move(Some((6.0, 0.0)), at(2)).unwrap(),
        );
        motion.unsourced();
        add(&mut truth, modelled(&mut model, (8, 0), 1));
        add(&mut sent, dips(motion.raw(8, 0, &mut windows, at(3))));
        add(
            &mut sent,
            motion.settle(at(3) + SETTLE_AFTER).unwrap_or_default(),
        );
        close(sent, truth);
        assert_eq!(motion.stats.uncovered, 0);
    }

    #[test]
    fn the_summary_reports_counts_only() {
        let mut motion = RemoteMotion::new(1.0);
        let mut windows = Ballistics::default();
        motion.hook_move(Some((2.0, 0.0)), at(0));
        motion.raw(9, 0, &mut windows, at(1));
        assert_eq!(
            motion.summary(Duration::from_millis(2500)),
            format!(
                "pointer motion: 2 events in 2.5 s (1 hook moves, 0 whole raw samples), peak 2 in any second; 1 raw corrections totalling {:.1} DIPs; 0 overshoots returned totalling 0.0 DIPs; 0 hook runs no raw sample covered; 0 charged back; hook-to-model scale 1.000",
                motion.stats.correction_dips
            )
        );
    }

    #[test]
    fn invalid_scales_keep_the_last_good_one() {
        let mut motion = RemoteMotion::new(f64::NAN);
        assert_eq!(motion.pixels_per_dip, 1.0);
        motion.set_pixels_per_dip(2.0);
        for scale in [0.0, -1.0, f64::INFINITY] {
            motion.set_pixels_per_dip(scale);
        }
        assert_eq!(
            motion.hook_move(Some((4.0, -2.0)), at(0)),
            Some((2.0, -1.0))
        );
    }
}
