//! Sleep-inclusive monotonic time for explicit macOS input lifetimes.

use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockError {
    TimebaseUnavailable,
    InvalidTimebase,
}

impl fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TimebaseUnavailable => "macOS continuous clock timebase is unavailable",
            Self::InvalidTimebase => "macOS continuous clock timebase is invalid",
        })
    }
}

impl Error for ClockError {}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[derive(Clone, Copy)]
struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    fn new(numer: u32, denom: u32) -> Result<Self, ClockError> {
        if numer == 0 || denom == 0 {
            return Err(ClockError::InvalidTimebase);
        }
        Ok(Self { numer, denom })
    }

    fn duration_for(self, ticks: u64) -> Option<Duration> {
        let nanoseconds = u128::from(ticks)
            .checked_mul(u128::from(self.numer))?
            .checked_div(u128::from(self.denom))?;
        let seconds = nanoseconds.checked_div(1_000_000_000)?;
        let subsec_nanos = nanoseconds.checked_rem(1_000_000_000)?;
        Some(Duration::new(
            u64::try_from(seconds).ok()?,
            u32::try_from(subsec_nanos).ok()?,
        ))
    }
}

enum ClockReader {
    System,
    #[cfg(test)]
    Fake(Arc<std::sync::atomic::AtomicU64>),
}

impl ClockReader {
    fn now(&self) -> u64 {
        match self {
            Self::System => {
                // SAFETY: mach_continuous_time has no arguments and is documented as a cheap read.
                unsafe { mach_continuous_time() }
            }
            #[cfg(test)]
            Self::Fake(value) => value.load(Ordering::Acquire),
        }
    }
}

struct ClockState {
    origin_ticks: u64,
    timebase: Timebase,
    reader: ClockReader,
    failed: AtomicBool,
}

/// Clones share one continuous-clock origin and one terminal failure latch.
#[derive(Clone)]
pub struct ContinuousInstant(Arc<ClockState>);

impl ContinuousInstant {
    pub fn try_now() -> Result<Self, ClockError> {
        let timebase = system_timebase()?;
        let origin_ticks = ClockReader::System.now();
        Ok(Self::with_reader(
            origin_ticks,
            timebase,
            ClockReader::System,
        ))
    }

    /// Returns elapsed continuous time, or a sticky maximum duration after an impossible reading.
    pub fn elapsed(&self) -> Duration {
        if self.0.failed.load(Ordering::Acquire) {
            return Duration::MAX;
        }
        let elapsed = self
            .0
            .reader
            .now()
            .checked_sub(self.0.origin_ticks)
            .and_then(|ticks| self.0.timebase.duration_for(ticks));
        let Some(elapsed) = elapsed else {
            self.0.failed.store(true, Ordering::Release);
            return Duration::MAX;
        };
        if self.0.failed.load(Ordering::Acquire) {
            Duration::MAX
        } else {
            elapsed
        }
    }

    fn with_reader(origin_ticks: u64, timebase: Timebase, reader: ClockReader) -> Self {
        Self(Arc::new(ClockState {
            origin_ticks,
            timebase,
            reader,
            failed: AtomicBool::new(false),
        }))
    }

    #[cfg(test)]
    fn with_fake_reader(
        origin_ticks: u64,
        numer: u32,
        denom: u32,
        reader: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Self, ClockError> {
        Ok(Self::with_reader(
            origin_ticks,
            Timebase::new(numer, denom)?,
            ClockReader::Fake(reader),
        ))
    }
}

fn system_timebase() -> Result<Timebase, ClockError> {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: mach_timebase_info writes exactly one SDK-defined MachTimebaseInfo record.
    if unsafe { mach_timebase_info(&raw mut info) } != 0 {
        return Err(ClockError::TimebaseUnavailable);
    }
    Timebase::new(info.numer, info.denom)
}

// SAFETY: declarations match macOS SDK mach/mach_time.h for libSystem.
#[link(name = "System")]
unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn mach_continuous_time() -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_core::capture::{CaptureStop, StopReason, SuppressionLease};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn advancing_continuous_ticks_expire_even_when_fake_uptime_is_frozen() {
        let uptime = Arc::new(AtomicU64::new(40));
        let continuous = Arc::new(AtomicU64::new(40));
        let clock = ContinuousInstant::with_fake_reader(40, 1, 1, continuous.clone()).unwrap();
        continuous.store(190, Ordering::Release);

        assert_eq!(uptime.load(Ordering::Acquire), 40);
        assert_eq!(clock.elapsed(), Duration::from_nanos(150));
    }

    #[test]
    fn continuous_elapsed_expires_a_lease_while_uptime_stays_frozen() {
        let frozen_uptime = Arc::new(AtomicU64::new(0));
        let continuous_ticks = Arc::new(AtomicU64::new(0));
        let clock = ContinuousInstant::with_fake_reader(0, 1, 1, continuous_ticks.clone()).unwrap();
        let stop = CaptureStop::default();
        let mut lease = SuppressionLease::new(7, clock.elapsed(), stop.clone());
        lease
            .renew(7, clock.elapsed(), Duration::from_millis(120))
            .unwrap();

        continuous_ticks.store(120_000_000, Ordering::Release);
        let now = clock.elapsed();

        assert_eq!(frozen_uptime.load(Ordering::Acquire), 0);
        assert_eq!(now, Duration::from_millis(120));
        assert!(!lease.is_suppressing(now));
        assert_eq!(stop.reason(), Some(StopReason::LeaseExpired));
        assert_eq!(
            lease.renew(7, now, Duration::from_millis(120)),
            Err(StopReason::LeaseExpired)
        );
    }

    #[test]
    fn invalid_ratio_overflow_and_regression_latch_maximum_elapsed() {
        let reader = Arc::new(AtomicU64::new(1));
        assert_eq!(
            ContinuousInstant::with_fake_reader(1, 1, 0, reader.clone()).err(),
            Some(ClockError::InvalidTimebase)
        );
        assert_eq!(
            ContinuousInstant::with_fake_reader(1, 0, 1, reader.clone()).err(),
            Some(ClockError::InvalidTimebase)
        );

        let overflowing =
            ContinuousInstant::with_fake_reader(0, u32::MAX, 1, Arc::new(AtomicU64::new(u64::MAX)))
                .unwrap();
        assert_eq!(overflowing.elapsed(), Duration::MAX);
        assert_eq!(overflowing.clone().elapsed(), Duration::MAX);

        let regressing = ContinuousInstant::with_fake_reader(10, 1, 1, reader).unwrap();
        assert_eq!(regressing.elapsed(), Duration::MAX);
        assert_eq!(regressing.clone().elapsed(), Duration::MAX);
    }

    #[test]
    fn a_regression_latches_every_clone_even_after_the_counter_recovers() {
        let reader = Arc::new(AtomicU64::new(10));
        let original = ContinuousInstant::with_fake_reader(10, 1, 1, reader.clone()).unwrap();
        let clone = original.clone();

        reader.store(9, Ordering::Release);
        assert_eq!(original.elapsed(), Duration::MAX);

        reader.store(20, Ordering::Release);
        assert_eq!(original.elapsed(), Duration::MAX);
        assert_eq!(clone.elapsed(), Duration::MAX);
    }

    #[test]
    fn native_continuous_clock_links_and_reads_monotonically_without_waiting() {
        let clock = ContinuousInstant::try_now().expect("macOS continuous clock is available");
        let first = clock.elapsed();
        let second = clock.elapsed();

        assert_ne!(first, Duration::MAX);
        assert_ne!(second, Duration::MAX);
        assert!(second >= first);
    }
}
