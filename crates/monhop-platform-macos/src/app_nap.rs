//! Keeps macOS from napping MonHop, or coalescing its timers, while a sharing session is live.

use core_foundation::{base::TCFType, string::CFString};

use crate::objc::{AutoreleasePool, Id, Objc, class, link_foundation, selector};

// NSActivityOptions bits, as declared in NSProcessInfo.h.
const IDLE_SYSTEM_SLEEP_DISABLED: u64 = 1 << 20;
const USER_INITIATED: u64 = 0x00FF_FFFF | IDLE_SYSTEM_SLEEP_DISABLED;
const USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP: u64 = USER_INITIATED & !IDLE_SYSTEM_SLEEP_DISABLED;
const LATENCY_CRITICAL: u64 = 0xFF_0000_0000;
/// No App Nap and no timer coalescing; idle system and display sleep stay allowed.
const SESSION_ACTIVITY: u64 = USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP | LATENCY_CRITICAL;
const REASON: &str = "Sharing keyboard and mouse with another computer";

/// Holds one `NSProcessInfo` activity from `begin` until drop.
pub struct SessionActivity {
    objc: Objc,
    process: Id,
    token: Id,
}

// SAFETY: NSProcessInfo is thread-safe, its activity token is an opaque object it accepts back
// from any thread, and objc_msgSend is a thread-agnostic entry point.
unsafe impl Send for SessionActivity {}

impl SessionActivity {
    /// None when Foundation refuses the activity; the session then runs as it did without one.
    pub fn begin() -> Option<Self> {
        link_foundation();
        let objc = Objc::load().ok()?;
        let reason = CFString::from_static_string(REASON);
        // The token is autoreleased, so it is retained before the pool drains.
        let _pool = AutoreleasePool::new(objc).ok()?;
        let info = class(c"NSProcessInfo", "Foundation unavailable").ok()?;
        let process = objc.send_id(info, selector(c"processInfo"));
        if process.is_null() {
            return None;
        }
        let token = objc.send_id_u64_id(
            process,
            selector(c"beginActivityWithOptions:reason:"),
            SESSION_ACTIVITY,
            reason.as_concrete_TypeRef().cast_mut().cast(),
        );
        if token.is_null() {
            return None;
        }
        let token = objc.send_id(token, selector(c"retain"));
        (!token.is_null()).then_some(Self {
            objc,
            process,
            token,
        })
    }
}

impl Drop for SessionActivity {
    fn drop(&mut self) {
        let _pool = AutoreleasePool::new(self.objc).ok();
        self.objc
            .send_void_id(self.process, selector(c"endActivity:"), self.token);
        self.objc.send_void(self.token, selector(c"release"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_activity_blocks_napping_but_never_idle_sleep() {
        const IDLE_DISPLAY_SLEEP_DISABLED: u64 = 1 << 40;
        assert_eq!(SESSION_ACTIVITY & IDLE_SYSTEM_SLEEP_DISABLED, 0);
        assert_eq!(SESSION_ACTIVITY & IDLE_DISPLAY_SLEEP_DISABLED, 0);
        assert_eq!(SESSION_ACTIVITY, 0xFF_00EF_FFFF);
    }

    #[test]
    fn an_activity_begins_and_ends() {
        let activity = SessionActivity::begin().expect("Foundation grants the activity");
        std::thread::spawn(move || drop(activity)).join().unwrap();
    }
}
