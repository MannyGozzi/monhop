//! Brings the window's page back when macOS ends its web content process, which it does to hidden
//! windows under memory pressure; without a reload the window reopens blank.

use std::{
    collections::VecDeque,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use tauri::{Runtime, Webview};

/// A page that keeps crashing on load stops being reloaded instead of looping.
const RELOADS_PER_WINDOW: usize = 3;
const WINDOW: Duration = Duration::from_secs(60);

static BUDGET: Mutex<ReloadBudget> = Mutex::new(ReloadBudget::new());

/// Tauri's web-content-terminate hook: reloads the page unless it has already crashed too often.
pub fn reload<R: Runtime>(webview: &Webview<R>) {
    let allowed = BUDGET
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .allow(Instant::now());
    if !allowed {
        log::error!(
            "window: web content ended {RELOADS_PER_WINDOW} times within {}s; not reloading",
            WINDOW.as_secs()
        );
        return;
    }
    log::warn!("window: web content ended; reloading");
    if let Err(error) = webview.reload() {
        log::error!("window: reload after the web content ended failed: {error}");
    }
}

struct ReloadBudget {
    recent: VecDeque<Instant>,
}

impl ReloadBudget {
    const fn new() -> Self {
        Self {
            recent: VecDeque::new(),
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|reload| now.saturating_duration_since(*reload) >= WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= RELOADS_PER_WINDOW {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloads_until_the_page_crashes_too_often_then_again_once_the_window_passes() {
        let mut budget = ReloadBudget::new();
        let start = Instant::now();
        for second in 0..RELOADS_PER_WINDOW as u64 {
            assert!(budget.allow(start + Duration::from_secs(second)));
        }
        assert!(!budget.allow(start + Duration::from_secs(10)));
        assert!(!budget.allow(start + WINDOW - Duration::from_millis(1)));
        assert!(budget.allow(start + WINDOW));
    }

    #[test]
    fn a_crash_now_and_then_is_always_reloaded() {
        let mut budget = ReloadBudget::new();
        let start = Instant::now();
        for hour in 0..10 {
            assert!(budget.allow(start + Duration::from_secs(3600 * hour)));
        }
    }
}
