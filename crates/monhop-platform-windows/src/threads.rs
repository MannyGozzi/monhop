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
}
