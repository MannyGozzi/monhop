//! The threads that must answer the peer inside `PEER_LIVENESS` run ahead of ordinary work.

/// Raises the calling thread's scheduling priority. A refusal is logged and the thread runs at
/// normal priority, which costs resilience under load but nothing else.
pub fn mark_time_sensitive() {
    #[cfg(target_os = "macos")]
    let outcome = monhop_platform_macos::threads::mark_time_sensitive();
    #[cfg(windows)]
    let outcome = monhop_platform_windows::threads::mark_time_sensitive();
    #[cfg(not(any(target_os = "macos", windows)))]
    let outcome: Result<(), i32> = Ok(());
    if let Err(code) = outcome {
        log::warn!("session thread keeps normal priority (code {code})");
    }
}
