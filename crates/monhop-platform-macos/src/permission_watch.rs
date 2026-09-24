//! TCC probes on a helper thread: the capture owner loop only loads their latest answer, so a slow
//! probe can never hold it past a suppression lease.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::PermissionState;

const ACCESSIBILITY: u8 = 1;
const LISTEN_EVENTS: u8 = 1 << 1;

/// The latest probed permissions. A new cell reads as nothing granted.
#[derive(Clone, Default)]
pub(crate) struct PermissionCell(Arc<AtomicU8>);

impl PermissionCell {
    pub(crate) fn store(&self, state: PermissionState) {
        let bits = (u8::from(state.accessibility) * ACCESSIBILITY)
            | (u8::from(state.listen_events) * LISTEN_EVENTS);
        self.0.store(bits, Ordering::Release);
    }

    pub(crate) fn load(&self) -> PermissionState {
        let bits = self.0.load(Ordering::Acquire);
        PermissionState {
            accessibility: bits & ACCESSIBILITY != 0,
            listen_events: bits & LISTEN_EVENTS != 0,
        }
    }
}

/// Re-probes into its cell every interval until dropped. Dropping never waits for a probe in
/// flight; the helper exits after it.
pub(crate) struct PermissionWatch {
    _stop: mpsc::Sender<()>,
}

impl PermissionWatch {
    pub(crate) fn start(
        cell: PermissionCell,
        interval: Duration,
        probe: impl Fn() -> PermissionState + Send + 'static,
    ) -> io::Result<Self> {
        let (stop, stopped) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("monhop-permission-watch".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    cell.store(probe());
                }
            })?;
        Ok(Self { _stop: stop })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn a_cell_round_trips_every_state_and_starts_with_nothing_granted() {
        let cell = PermissionCell::default();
        assert_eq!(cell.load(), PermissionState::default());
        for accessibility in [false, true] {
            for listen_events in [false, true] {
                let state = PermissionState {
                    accessibility,
                    listen_events,
                };
                cell.store(state);
                assert_eq!(cell.load(), state);
            }
        }
    }

    #[test]
    fn the_helper_publishes_a_revocation_and_stops_probing_once_dropped() {
        let cell = PermissionCell::default();
        let probes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&probes);
        let granted = PermissionState {
            accessibility: true,
            listen_events: true,
        };
        cell.store(granted);
        let watch = PermissionWatch::start(cell.clone(), Duration::from_millis(1), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            PermissionState {
                accessibility: false,
                listen_events: true,
            }
        })
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while cell.load() == granted && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!cell.load().accessibility, "the loop reads the revocation");
        drop(watch);
        thread::sleep(Duration::from_millis(20));
        let settled = probes.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(probes.load(Ordering::SeqCst), settled);
    }
}
