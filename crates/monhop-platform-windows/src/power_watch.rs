//! An explicitly owned window receives power changes without a foreign callback context.

use monhop_core::RevocationSignal;
use std::{io, sync::mpsc, thread};

struct RevokeOnExit(RevocationSignal);

impl Drop for RevokeOnExit {
    fn drop(&mut self) {
        self.0.mark_revoked_without_wake();
    }
}

fn event_revokes(event: usize) -> bool {
    matches!(event, 4 | 6 | 7 | 18)
}

pub(crate) struct PowerWatch {
    signal: RevocationSignal,
    worker: Option<thread::JoinHandle<()>>,
}

impl PowerWatch {
    #[cfg(windows)]
    pub(crate) fn start(signal: RevocationSignal) -> io::Result<Self> {
        Self::spawn(signal, native::run)
    }

    fn spawn(
        signal: RevocationSignal,
        run: impl FnOnce(RevocationSignal, mpsc::SyncSender<io::Result<()>>) + Send + 'static,
    ) -> io::Result<Self> {
        require_active(&signal)?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_signal = signal.clone();
        let worker = thread::Builder::new()
            .name("monhop-power".into())
            .spawn(move || {
                let _revoke = RevokeOnExit(worker_signal.clone());
                run(worker_signal, ready_tx);
            })?;
        let watch = Self {
            signal,
            worker: Some(worker),
        };
        ready_rx
            .recv()
            .map_err(|_| io::Error::other("Power observer stopped before becoming ready"))??;
        require_active(&watch.signal)?;
        Ok(watch)
    }
}

impl Drop for PowerWatch {
    fn drop(&mut self) {
        self.signal.mark_revoked_without_wake();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn require_active(signal: &RevocationSignal) -> io::Result<()> {
    if signal.is_revoked() {
        Err(io::Error::other("Power observer authorization was revoked"))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{cell::RefCell, ptr, sync::OnceLock};
    use windows_sys::Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Power::{
                HPOWERNOTIFY, RegisterSuspendResumeNotification,
                UnregisterSuspendResumeNotification,
            },
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG,
            MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
            RegisterClassW, WM_POWERBROADCAST, WM_QUIT, WNDCLASSW,
        },
    };

    const CLASS_NAME: &[u16] = &[
        76, 111, 99, 97, 108, 75, 77, 80, 111, 119, 101, 114, 87, 97, 116, 99, 104, 0,
    ];
    const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0;
    const WAIT_FAILED: u32 = u32::MAX;
    const MAX_MESSAGE_BATCH: usize = 64;
    const POLL_MS: u32 = 10;
    static CLASS: OnceLock<Result<(), i32>> = OnceLock::new();
    thread_local! {
        static SIGNAL: RefCell<Option<RevocationSignal>> = const { RefCell::new(None) };
    }

    pub(super) fn run(signal: RevocationSignal, ready: mpsc::SyncSender<io::Result<()>>) {
        SIGNAL.with(|slot| *slot.borrow_mut() = Some(signal.clone()));
        let window = Window::create(&signal);
        match window {
            Ok(_window) => {
                if require_active(&signal).is_ok() && ready.send(Ok(())).is_ok() {
                    pump(&signal);
                }
                signal.mark_revoked_without_wake();
                // The callback uses only thread-local state, including during native teardown.
                SIGNAL.with(|slot| slot.borrow_mut().take());
            }
            Err(error) => {
                signal.mark_revoked_without_wake();
                SIGNAL.with(|slot| slot.borrow_mut().take());
                let _ = ready.send(Err(error));
            }
        }
    }

    fn pump(signal: &RevocationSignal) {
        while !signal.is_revoked() {
            for _ in 0..MAX_MESSAGE_BATCH {
                if signal.is_revoked() {
                    return;
                }
                let mut message = MSG::default();
                // SAFETY: this worker owns the queue and writable message storage.
                if unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    signal.mark_revoked_without_wake();
                    return;
                }
                // SAFETY: dispatch only messages retrieved from this worker's queue.
                unsafe { DispatchMessageW(&message) };
            }
            // SAFETY: zero handles, bounded wait. Cancellation is checked even without posted messages.
            if unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    ptr::null(),
                    POLL_MS,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            } == WAIT_FAILED
            {
                signal.mark_revoked_without_wake();
                return;
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_POWERBROADCAST && event_revokes(wparam) {
            log::warn!("power watch: power event {wparam} revoked the session");
            let _ = SIGNAL.try_with(|slot| {
                if let Some(signal) = slot.borrow().as_ref() {
                    signal.mark_revoked_without_wake();
                }
            });
            return 1;
        }
        // SAFETY: unhandled messages retain the default window behavior; no payload is dereferenced.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    struct Window {
        hwnd: HWND,
        notification: HPOWERNOTIFY,
    }

    impl Window {
        fn create(signal: &RevocationSignal) -> io::Result<Self> {
            require_active(signal)?;
            // SAFETY: null obtains the current executable module without loading another module.
            let module = unsafe { GetModuleHandleW(ptr::null()) };
            if module.is_null() {
                return Err(io::Error::last_os_error());
            }
            register_class(module)?;
            require_active(signal)?;
            // SAFETY: static class callback, no external context, hidden top-level window owned here.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    CLASS_NAME.as_ptr(),
                    ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    module,
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut result = Self {
                hwnd,
                notification: 0,
            };
            require_active(signal)?;
            // SAFETY: explicit window notification registration, including Modern Standby opt-in.
            result.notification =
                unsafe { RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) };
            if result.notification == 0 {
                return Err(io::Error::last_os_error());
            }
            require_active(signal)?;
            Ok(result)
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            if self.notification != 0 {
                // SAFETY: this worker owns the registration and still owns its window.
                unsafe { UnregisterSuspendResumeNotification(self.notification) };
            }
            // SAFETY: same-thread destruction. No foreign context exists even if destruction fails;
            // Windows destroys any remaining owned window when this dedicated thread exits.
            unsafe { DestroyWindow(self.hwnd) };
        }
    }

    fn register_class(module: HINSTANCE) -> io::Result<()> {
        match CLASS.get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module,
                lpszClassName: CLASS_NAME.as_ptr(),
                ..Default::default()
            };
            // SAFETY: this process-lifetime class contains only static pointers and a static callback.
            if unsafe { RegisterClassW(&class) } == 0 {
                Err(io::Error::last_os_error().raw_os_error().unwrap_or(1))
            } else {
                Ok(())
            }
        }) {
            Ok(()) => Ok(()),
            Err(code) => Err(io::Error::from_raw_os_error(*code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn only_suspend_and_resume_events_revoke() {
        for event in 0..=32 {
            assert_eq!(event_revokes(event), matches!(event, 4 | 6 | 7 | 18));
        }
    }

    #[test]
    fn registration_failure_joins_the_worker() {
        let ended = Arc::new(AtomicBool::new(false));
        let worker_ended = ended.clone();
        let signal = RevocationSignal::default();
        let result = PowerWatch::spawn(signal.clone(), move |_, ready| {
            ready
                .send(Err(io::Error::other("registration failed")))
                .unwrap();
            worker_ended.store(true, Ordering::Release);
        });
        assert!(result.is_err());
        assert!(signal.is_revoked());
        assert!(ended.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_before_registration_starts_no_worker() {
        let signal = RevocationSignal::default();
        signal.mark_revoked_without_wake();
        assert!(PowerWatch::spawn(signal, |_, _| panic!("must not start")).is_err());
    }

    #[test]
    fn cancellation_during_registration_cannot_publish_ready() {
        let signal = RevocationSignal::default();
        let result = PowerWatch::spawn(signal.clone(), |signal, ready| {
            signal.mark_revoked_without_wake();
            ready.send(Ok(())).unwrap();
        });
        assert!(result.is_err());
        assert!(signal.is_revoked());
    }

    #[test]
    fn unexpected_exit_before_readiness_revokes_and_joins() {
        for should_panic in [false, true] {
            let signal = RevocationSignal::default();
            let result = PowerWatch::spawn(signal.clone(), move |_, _ready| {
                assert!(!should_panic, "simulated worker failure");
            });
            assert!(result.is_err());
            assert!(signal.is_revoked());
        }
    }

    #[test]
    fn unexpected_exit_after_readiness_revokes_without_owner_drop() {
        for should_panic in [false, true] {
            let signal = RevocationSignal::default();
            let (exit_tx, exit_rx) = mpsc::sync_channel(0);
            let mut watch = PowerWatch::spawn(signal.clone(), move |_, ready| {
                ready.send(Ok(())).unwrap();
                exit_rx.recv().unwrap();
                assert!(!should_panic, "simulated worker failure");
            })
            .unwrap();
            assert!(!signal.is_revoked());
            exit_tx.send(()).unwrap();
            assert_eq!(watch.worker.take().unwrap().join().is_err(), should_panic);
            assert!(signal.is_revoked());
            drop(watch);
        }
    }

    #[test]
    fn drop_revokes_before_waiting_for_owned_teardown() {
        let signal = RevocationSignal::default();
        let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        let watch = PowerWatch::spawn(signal.clone(), move |signal, ready| {
            ready.send(Ok(())).unwrap();
            while !signal.is_revoked() {
                thread::yield_now();
            }
            cleanup_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        })
        .unwrap();
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(watch);
            dropped_tx.send(()).unwrap();
        });
        cleanup_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(signal.is_revoked());
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        finish_tx.send(()).unwrap();
        dropper.join().unwrap();
        dropped_rx.recv().unwrap();
    }
}
