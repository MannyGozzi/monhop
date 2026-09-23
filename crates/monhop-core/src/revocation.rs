//! Shared, permanent session revocation without exposing a reset operation.

use std::{
    panic::{AssertUnwindSafe, Location, catch_unwind},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::task::AtomicWaker;

struct RevocationState {
    revoked: AtomicBool,
    stopping: AtomicBool,
    // A stop specifically for a control-change resync; distinct so the close reason can tell it
    // apart from every other deliberate stop.
    control_change: AtomicBool,
    observer: AtomicWaker,
    // The first revocation wins; a session that ends "Revoked" names this call site.
    origin: OnceLock<&'static Location<'static>>,
}

#[derive(Clone)]
pub struct RevocationSignal(Arc<RevocationState>);

impl Default for RevocationSignal {
    fn default() -> Self {
        Self(Arc::new(RevocationState {
            revoked: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            control_change: AtomicBool::new(false),
            observer: AtomicWaker::new(),
            origin: OnceLock::new(),
        }))
    }
}

impl RevocationSignal {
    #[track_caller]
    pub fn revoke(&self) {
        self.mark_revoked_without_wake();
        // Native callbacks may call this, so an observer panic cannot cross their FFI boundary.
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.0.observer.wake())) {
            // A panic payload can itself panic on Drop. Do not unwind through a native callback.
            std::mem::forget(payload);
        }
    }

    /// Permanently records revocation without invoking an observer waker.
    ///
    /// Native capture uses this before its local teardown, then arranges a wake from a safe
    /// context after teardown. Call [`Self::revoke`] when an observer may be woken immediately.
    #[track_caller]
    pub fn mark_revoked_without_wake(&self) {
        let _ = self.0.origin.set(Location::caller());
        self.0.revoked.store(true, Ordering::Release);
    }

    /// Asks the session to end on its own terms: loops exit at once while the socket stays open
    /// long enough for the QUIC close to leave. A later revocation still closes the socket.
    #[track_caller]
    pub fn request_stop(&self) {
        let _ = self.0.origin.set(Location::caller());
        self.0.stopping.store(true, Ordering::Release);
    }

    /// Same as [`Self::request_stop`], marked so the close path can send a control-change reason
    /// instead of the ordinary ended one.
    #[track_caller]
    pub fn request_stop_for_control_change(&self) {
        let _ = self.0.origin.set(Location::caller());
        self.0.control_change.store(true, Ordering::Release);
        self.0.stopping.store(true, Ordering::Release);
    }

    /// The call site of the first stop request or revocation, if any.
    pub fn origin(&self) -> Option<&'static Location<'static>> {
        self.0.origin.get().copied()
    }

    pub fn is_revoked(&self) -> bool {
        self.0.revoked.load(Ordering::Acquire)
    }

    /// What a session loop polls: a stop request or a revocation both end it.
    pub fn is_stopping(&self) -> bool {
        self.0.stopping.load(Ordering::Acquire) || self.is_revoked()
    }

    /// True once [`Self::request_stop_for_control_change`] was called on this signal.
    pub fn is_stopping_for_control_change(&self) -> bool {
        self.0.control_change.load(Ordering::Acquire)
    }

    /// Waits for permanent revocation. One task may observe; competing pollers replace its waker.
    /// Other consumers must call [`Self::is_revoked`].
    pub fn poll_revoked(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_revoked_after_initial_check(cx, || {})
    }

    fn poll_revoked_after_initial_check(
        &self,
        cx: &mut Context<'_>,
        before_register: impl FnOnce(),
    ) -> Poll<()> {
        if self.is_revoked() {
            return Poll::Ready(());
        }

        before_register();
        self.0.observer.register(cx.waker());

        if self.is_revoked() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Wake, Waker},
    };

    use super::*;

    #[derive(Default)]
    struct CountingWake(AtomicUsize);

    impl CountingWake {
        fn count(&self) -> usize {
            self.0.load(Ordering::Acquire)
        }
    }

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    struct PanicWake;

    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("registered waker panicked");
        }
    }

    fn context(waker: &Waker) -> Context<'_> {
        Context::from_waker(waker)
    }

    #[test]
    fn the_first_revocation_names_its_call_site() {
        let signal = RevocationSignal::default();
        assert!(signal.origin().is_none());
        signal.revoke();
        let first = signal.origin().expect("revoked");
        assert!(first.file().ends_with("revocation.rs"));
        signal.mark_revoked_without_wake();
        assert_eq!(signal.origin().map(|at| at.line()), Some(first.line()));
    }

    #[test]
    fn revocation_before_registration_is_ready() {
        let signal = RevocationSignal::default();
        let observer = Arc::new(CountingWake::default());
        let waker = Waker::from(observer.clone());
        signal.revoke();

        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Ready(())
        ));
        assert_eq!(observer.count(), 0);
    }

    #[test]
    fn registration_then_revocation_wakes_observer() {
        let signal = RevocationSignal::default();
        let observer = Arc::new(CountingWake::default());
        let waker = Waker::from(observer.clone());

        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Pending
        ));
        signal.revoke();

        assert_eq!(observer.count(), 1);
        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Ready(())
        ));
    }

    #[test]
    fn a_stop_request_ends_loops_without_revoking_the_socket() {
        let signal = RevocationSignal::default();
        signal.request_stop();
        assert!(signal.is_stopping());
        assert!(!signal.is_revoked());
        assert!(
            signal
                .origin()
                .is_some_and(|at| at.file().ends_with("revocation.rs"))
        );
        signal.revoke();
        assert!(signal.is_revoked());
        assert!(signal.is_stopping());
    }

    #[test]
    fn a_control_change_stop_is_stopping_and_distinguishable_from_a_plain_stop() {
        let signal = RevocationSignal::default();
        assert!(!signal.is_stopping_for_control_change());
        signal.request_stop_for_control_change();
        assert!(signal.is_stopping());
        assert!(signal.is_stopping_for_control_change());
        assert!(!signal.is_revoked());

        let plain = RevocationSignal::default();
        plain.request_stop();
        assert!(plain.is_stopping());
        assert!(!plain.is_stopping_for_control_change());
    }

    #[test]
    fn mark_without_wake_is_visible_without_invoking_the_observer() {
        let signal = RevocationSignal::default();
        let observer = Arc::new(CountingWake::default());
        let waker = Waker::from(observer.clone());

        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Pending
        ));
        signal.mark_revoked_without_wake();

        assert!(signal.is_revoked());
        assert_eq!(observer.count(), 0);
        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Ready(())
        ));
    }

    #[test]
    fn sequential_observer_replacement_wakes_only_latest() {
        let signal = RevocationSignal::default();
        let first = Arc::new(CountingWake::default());
        let second = Arc::new(CountingWake::default());
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());

        assert!(matches!(
            signal.poll_revoked(&mut context(&first_waker)),
            Poll::Pending
        ));
        assert!(matches!(
            signal.poll_revoked(&mut context(&second_waker)),
            Poll::Pending
        ));
        signal.revoke();

        assert_eq!(first.count(), 0);
        assert_eq!(second.count(), 1);
    }

    #[test]
    fn revocation_between_check_and_registration_is_observed() {
        let signal = RevocationSignal::default();
        let revoker = signal.clone();
        let observer = Arc::new(CountingWake::default());
        let waker = Waker::from(observer);

        assert!(matches!(
            signal.poll_revoked_after_initial_check(&mut context(&waker), || revoker.revoke()),
            Poll::Ready(())
        ));
    }

    #[test]
    fn every_clone_observes_permanent_revocation() {
        let source = RevocationSignal::default();
        let receiver = source.clone();
        assert!(!receiver.is_revoked());
        std::thread::spawn(move || source.revoke()).join().unwrap();
        assert!(receiver.is_revoked());
        receiver.revoke();
        assert!(receiver.clone().is_revoked());
    }

    #[test]
    fn new_session_does_not_rearm_an_old_signal() {
        let old = RevocationSignal::default();
        old.revoke();
        let new = RevocationSignal::default();
        assert!(!new.is_revoked());
        assert!(old.is_revoked());
    }

    struct PanicPayload;

    impl Drop for PanicPayload {
        fn drop(&mut self) {
            panic!("panic payload destructor must not run in a native callback");
        }
    }

    struct PanicPayloadWake;

    impl Wake for PanicPayloadWake {
        fn wake(self: Arc<Self>) {
            std::panic::panic_any(PanicPayload);
        }
    }

    #[test]
    fn panicking_payload_drop_cannot_escape_revocation() {
        let signal = RevocationSignal::default();
        let waker = Waker::from(Arc::new(PanicPayloadWake));
        assert!(signal.poll_revoked(&mut context(&waker)).is_pending());
        assert!(catch_unwind(AssertUnwindSafe(|| signal.revoke())).is_ok());
        assert!(signal.is_revoked());
    }

    #[test]
    fn panicking_waker_does_not_escape_or_clear_revocation() {
        let signal = RevocationSignal::default();
        let waker = Waker::from(Arc::new(PanicWake));
        assert!(matches!(
            signal.poll_revoked(&mut context(&waker)),
            Poll::Pending
        ));

        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| signal.revoke())).is_ok());
        assert!(signal.is_revoked());
    }
}
