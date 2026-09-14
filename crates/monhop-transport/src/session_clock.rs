//! Sharing deadlines include time spent suspended; queued replies cannot renew pre-sleep health.

use crate::session::SessionFailure;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct SessionClock {
    #[cfg(test)]
    reader: Option<std::sync::Arc<dyn Fn() -> Duration + Send + Sync>>,
    #[cfg(target_os = "macos")]
    origin: monhop_platform_macos::ContinuousInstant,
    #[cfg(not(target_os = "macos"))]
    origin: std::time::Instant,
}

impl SessionClock {
    pub fn try_now() -> Result<Self, SessionFailure> {
        Ok(Self {
            #[cfg(test)]
            reader: None,
            #[cfg(target_os = "macos")]
            origin: monhop_platform_macos::ContinuousInstant::try_now()
                .map_err(|_| SessionFailure::NativeStartup)?,
            #[cfg(not(target_os = "macos"))]
            origin: std::time::Instant::now(),
        })
    }

    pub fn elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(reader) = &self.reader {
            return reader();
        }
        self.origin.elapsed()
    }
}

#[cfg(test)]
impl SessionClock {
    pub(crate) fn with_test_reader(reader: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        let mut clock = Self::try_now().expect("test clock initializes");
        clock.reader = Some(std::sync::Arc::new(reader));
        clock
    }
}

/// Reported durations saturate: a span too large for the counter is a fault, never a small number.
pub(crate) fn millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn micros_u64(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::{micros_u64, millis_u64};
    use std::time::Duration;

    #[test]
    fn an_oversized_duration_saturates_instead_of_wrapping() {
        assert_eq!(millis_u64(Duration::from_millis(1_500)), 1_500);
        assert_eq!(micros_u64(Duration::from_millis(2)), 2_000);
        assert_eq!(millis_u64(Duration::MAX), u64::MAX);
        assert_eq!(micros_u64(Duration::MAX), u64::MAX);
    }
}
