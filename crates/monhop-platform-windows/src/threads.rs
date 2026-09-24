//! Scheduling priority for the threads that must answer the peer inside the liveness deadline.

/// Raises the calling thread above every normal-priority thread on the machine, so a build or an
/// export running on every core no longer holds a heartbeat past the peer's deadline. The error
/// is the Win32 error code.
#[cfg(windows)]
pub fn mark_time_sensitive() -> Result<(), u32> {
    use windows_sys::Win32::{
        Foundation::GetLastError,
        System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL},
    };
    // SAFETY: the pseudo handle names the calling thread and is never closed.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } == 0 {
        // SAFETY: reads the calling thread's last error code.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn mark_time_sensitive() -> Result<(), u32> {
    Ok(())
}

/// Windows throttles a process whose windows are hidden: it slows its threads and ignores its
/// 1 ms timer request, which the session loops need. The error is the Win32 error code.
#[cfg(windows)]
pub fn opt_out_of_power_throttling() -> Result<(), u32> {
    use windows_sys::Win32::{
        Foundation::GetLastError,
        System::Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        },
    };
    // A controlled bit with a clear state bit turns that throttle off.
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: the pseudo handle names this process and the size is that of the state passed.
    let set = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of_val(&state) as u32,
        )
    };
    if set == 0 {
        // SAFETY: reads the calling thread's last error code.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };

    use super::*;

    #[test]
    fn a_marked_thread_runs_at_time_critical_priority() {
        let outcome = std::thread::spawn(|| {
            mark_time_sensitive()?;
            // SAFETY: the pseudo handle names the calling thread and is never closed.
            Ok::<i32, u32>(unsafe { GetThreadPriority(GetCurrentThread()) })
        })
        .join()
        .unwrap();
        assert_eq!(outcome, Ok(THREAD_PRIORITY_TIME_CRITICAL));
    }

    #[test]
    fn the_process_opts_out_of_power_throttling() {
        assert_eq!(opt_out_of_power_throttling(), Ok(()));
    }
}
