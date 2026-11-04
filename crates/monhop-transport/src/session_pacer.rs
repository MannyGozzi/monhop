//! Spaces absolute motion frames by the link's quality. Each frame carries the source's tracked
//! position, which already sums every gain-scaled delta exactly, so a frame waiting for its slot
//! is simply replaced by the next: no motion is lost or sent twice.

use monhop_core::Point;
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct MotionPacer {
    waiting: Option<Point>,
    last_sent: Option<Duration>,
}

impl MotionPacer {
    pub(crate) fn has_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// `position` to send now when its slot is open; otherwise it waits in place of any older one.
    pub(crate) fn offer(
        &mut self,
        position: Point,
        interval: Duration,
        now: Duration,
    ) -> Option<Point> {
        self.waiting = Some(position);
        self.due(interval, now)
    }

    /// The waiting position once `interval` has passed since the last one went out.
    pub(crate) fn due(&mut self, interval: Duration, now: Duration) -> Option<Point> {
        let open = self
            .last_sent
            .is_none_or(|last| now.saturating_sub(last) >= interval);
        if open { self.flush(now) } else { None }
    }

    /// The waiting position now, so an ordered frame never overtakes the motion before it.
    pub(crate) fn flush(&mut self, now: Duration) -> Option<Point> {
        let position = self.waiting.take()?;
        self.last_sent = Some(now);
        Some(position)
    }

    pub(crate) fn discard(&mut self) {
        self.waiting = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn with_no_interval_every_position_goes_at_once() {
        let mut pacer = MotionPacer::default();
        for step in 0..5 {
            let position = Point::new(f64::from(step), 0.0);
            assert_eq!(pacer.offer(position, Duration::ZERO, ms(1)), Some(position));
        }
        assert_eq!(pacer.flush(ms(1)), None);
    }

    #[test]
    fn positions_inside_an_interval_wait_and_only_the_newest_goes_when_it_ends() {
        let mut pacer = MotionPacer::default();
        let interval = ms(4);
        assert_eq!(
            pacer.offer(Point::new(1.0, 0.0), interval, ms(10)),
            Some(Point::new(1.0, 0.0))
        );
        assert_eq!(pacer.offer(Point::new(2.0, 0.0), interval, ms(11)), None);
        assert_eq!(pacer.offer(Point::new(3.0, 1.0), interval, ms(12)), None);
        assert_eq!(pacer.due(interval, ms(13)), None);
        assert_eq!(pacer.due(interval, ms(14)), Some(Point::new(3.0, 1.0)));
        assert_eq!(pacer.due(interval, ms(30)), None);
        // After a quiet spell the next position goes at once, with no added latency.
        assert_eq!(
            pacer.offer(Point::new(4.0, 1.0), interval, ms(30)),
            Some(Point::new(4.0, 1.0))
        );
    }

    #[test]
    fn a_flush_sends_the_waiting_position_early_and_restarts_the_interval() {
        let mut pacer = MotionPacer::default();
        let interval = ms(8);
        assert!(pacer.offer(Point::new(1.0, 0.0), interval, ms(0)).is_some());
        assert_eq!(pacer.offer(Point::new(2.0, 0.0), interval, ms(2)), None);
        assert!(pacer.has_waiting());
        assert_eq!(pacer.flush(ms(3)), Some(Point::new(2.0, 0.0)));
        assert!(!pacer.has_waiting());
        assert_eq!(pacer.flush(ms(3)), None);
        assert_eq!(pacer.offer(Point::new(3.0, 0.0), interval, ms(9)), None);
        assert_eq!(pacer.due(interval, ms(11)), Some(Point::new(3.0, 0.0)));
        assert_eq!(pacer.offer(Point::new(4.0, 0.0), interval, ms(12)), None);
        pacer.discard();
        assert_eq!(pacer.due(interval, ms(40)), None);
    }
}
