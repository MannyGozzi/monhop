//! Windows schedules timers at 15.6 ms unless the process asks for less; the session loops tick
//! every millisecond and otherwise drain captured input in 15.6 ms bursts.

use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};

const PERIOD_MS: u32 = 1;

/// Holds the 1 ms scheduler period for the life of the process; dropping it restores the default.
pub struct TimerResolution {
    raised: bool,
}

impl TimerResolution {
    pub fn raise() -> Self {
        // SAFETY: timeBeginPeriod takes a plain integer and touches no caller memory.
        let raised = unsafe { timeBeginPeriod(PERIOD_MS) } == TIMERR_NOERROR;
        if raised {
            log::info!("timer resolution: 1 ms for the session loops");
        } else {
            log::warn!(
                "timer resolution: Windows kept its default period; motion may arrive in bursts"
            );
        }
        Self { raised }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.raised {
            // SAFETY: matches the timeBeginPeriod call above with the same period.
            unsafe {
                timeEndPeriod(PERIOD_MS);
            }
        }
    }
}
