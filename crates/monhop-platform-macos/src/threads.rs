//! Scheduling class for the threads that must answer the peer inside the liveness deadline.

/// The class the scheduler serves before default and background work, so a build or an export
/// running on every core no longer holds a heartbeat past the peer's deadline.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

// SAFETY: these declarations match <pthread/qos.h> in the macOS SDK.
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    #[cfg(test)]
    fn pthread_self() -> *mut std::ffi::c_void;
    #[cfg(test)]
    fn pthread_get_qos_class_np(
        thread: *mut std::ffi::c_void,
        qos_class: *mut u32,
        relative_priority: *mut i32,
    ) -> i32;
}

/// Moves the calling thread into the user-interactive class. The error is the errno value.
pub fn mark_time_sensitive() -> Result<(), i32> {
    // SAFETY: the call changes only the calling thread's own scheduling class.
    match unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) } {
        0 => Ok(()),
        code => Err(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_thread_runs_in_the_user_interactive_class() {
        let outcome = std::thread::spawn(|| {
            mark_time_sensitive()?;
            let mut class = 0;
            let mut relative = 0;
            // SAFETY: the out-pointers are valid for the call and the thread is the current one.
            let status =
                unsafe { pthread_get_qos_class_np(pthread_self(), &mut class, &mut relative) };
            assert_eq!(status, 0);
            Ok::<u32, i32>(class)
        })
        .join()
        .unwrap();
        assert_eq!(outcome, Ok(QOS_CLASS_USER_INTERACTIVE));
    }
}
