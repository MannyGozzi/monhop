//! Explicit macOS sleep and wake revocation observer.

use std::{
    error::Error,
    ffi::{c_char, c_void},
    fmt,
    mem::MaybeUninit,
    ptr,
};

use monhop_core::{
    RevocationSignal,
    capture::{CaptureStop, StopReason},
};

const DISPATCH_QUEUE_LABEL: &[u8] = b"com.manuelgozzi.monhop.power-watch\0";
const IO_OBJECT_NULL: IoObject = 0;
const IO_RETURN_SUCCESS: IoReturn = 0;
const IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
const IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
const IO_MESSAGE_SYSTEM_WILL_NOT_SLEEP: u32 = 0xE000_0290;
const IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;
const IO_MESSAGE_SYSTEM_WILL_POWER_ON: u32 = 0xE000_0320;

type IoReturn = i32;
type IoObject = u32;
type IoConnect = u32;
type IoService = u32;
type IONotificationPortRef = *mut c_void;
type DispatchQueueRef = *mut c_void;
type IOServiceInterestCallback = unsafe extern "C" fn(*mut c_void, IoService, u32, *mut c_void);

/// Categories only. Native return codes and power state details are not exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerWatchError {
    AlreadyRevoked,
    DispatchQueueCreationFailed,
    ConnectionUnavailable,
    NotificationPortUnavailable,
    NotifierUnavailable,
    LateRevocation,
}

impl fmt::Display for PowerWatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyRevoked => "local authorization was already revoked",
            Self::DispatchQueueCreationFailed => "macOS power-notification queue could not start",
            Self::ConnectionUnavailable => "macOS power-notification connection could not start",
            Self::NotificationPortUnavailable => "macOS power-notification port could not start",
            Self::NotifierUnavailable => "macOS power-notification notifier could not start",
            Self::LateRevocation => "local authorization was revoked during power registration",
        })
    }
}

impl Error for PowerWatchError {}

#[derive(Clone)]
struct PowerLatches {
    signal: RevocationSignal,
    stop: Option<CaptureStop>,
}

impl PowerLatches {
    fn revoke_without_wake(&self, reason: StopReason, cause: &str) {
        log::warn!("power watch: {cause}; revoked");
        if let Some(stop) = &self.stop {
            stop.stop(reason);
        }
        self.signal.mark_revoked_without_wake();
    }
}

struct PowerContext {
    connection: IoConnect,
    latches: PowerLatches,
}

struct Registration {
    queue: DispatchQueueRef,
    port: IONotificationPortRef,
    notifier: IoObject,
}

/// Owns power registration from explicit local enablement through capture cleanup.
pub struct PowerWatch {
    latches: PowerLatches,
    registration: Option<Registration>,
}

// SAFETY: callers may move the owner to a worker. Teardown is thread-safe, while the queue
// finalizer retains the context until the IOKit source cancels queued callbacks.
unsafe impl Send for PowerWatch {}

impl PowerWatch {
    pub fn start_after_local_enable(
        signal: RevocationSignal,
        stop: Option<CaptureStop>,
    ) -> Result<Self, PowerWatchError> {
        let latches = PowerLatches { signal, stop };
        if latches.signal.is_revoked() {
            return Err(PowerWatchError::AlreadyRevoked);
        }
        let queue = create_queue();
        if queue.is_null() {
            return fail_start(&latches, PowerWatchError::DispatchQueueCreationFailed);
        }
        let context = Box::into_raw(Box::<PowerContext>::new_uninit());
        let mut port = ptr::null_mut();
        let mut notifier = IO_OBJECT_NULL;
        // SAFETY: delivery is not enabled until the initialized context and finalizer are installed.
        let connection = unsafe {
            IORegisterForSystemPower(
                context.cast(),
                &mut port,
                Some(power_notification_callback),
                &mut notifier,
            )
        };
        if let Err(error) = require_registration(queue, connection, port, notifier, &latches) {
            // SAFETY: no dispatch source was installed, so the uninitialized context is unreachable.
            unsafe { cleanup_failed_registration(connection, port, notifier, context) };
            // SAFETY: create_queue returned this retained queue without a context or finalizer.
            unsafe { dispatch_release(queue) };
            return Err(error);
        }
        let context = context.cast::<PowerContext>();
        // SAFETY: IOKit cannot deliver before SetDispatchQueue; the queue finalizer owns context.
        unsafe {
            context.write(PowerContext {
                connection,
                latches: latches.clone(),
            });
            dispatch_set_context(queue, context.cast());
            dispatch_set_finalizer_f(queue, Some(power_context_finalizer));
            IONotificationPortSetDispatchQueue(port, queue);
        }
        let registration = Registration {
            queue,
            port,
            notifier,
        };
        if latches.signal.is_revoked() {
            teardown_registration(registration);
            return Err(PowerWatchError::LateRevocation);
        }
        Ok(Self {
            latches,
            registration: Some(registration),
        })
    }
}

impl Drop for PowerWatch {
    fn drop(&mut self) {
        match &self.latches.stop {
            // The capture owning this watch decides whether its end revokes the session.
            Some(stop) => stop.stop(StopReason::Requested),
            None => self.latches.signal.revoke(),
        }
        if let Some(registration) = self.registration.take() {
            teardown_registration(registration);
        }
    }
}

fn fail_start<T>(latches: &PowerLatches, error: PowerWatchError) -> Result<T, PowerWatchError> {
    latches.revoke_without_wake(
        StopReason::NativeFailure,
        "sleep notifications could not start",
    );
    Err(error)
}

fn create_queue() -> DispatchQueueRef {
    // SAFETY: the static label is NUL-terminated and null requests a serial queue.
    unsafe { dispatch_queue_create(DISPATCH_QUEUE_LABEL.as_ptr().cast::<c_char>(), ptr::null()) }
}

fn require_registration(
    queue: DispatchQueueRef,
    connection: IoConnect,
    port: IONotificationPortRef,
    notifier: IoObject,
    latches: &PowerLatches,
) -> Result<(), PowerWatchError> {
    let error = if queue.is_null() {
        Some(PowerWatchError::DispatchQueueCreationFailed)
    } else if connection == IO_OBJECT_NULL {
        Some(PowerWatchError::ConnectionUnavailable)
    } else if port.is_null() {
        Some(PowerWatchError::NotificationPortUnavailable)
    } else if notifier == IO_OBJECT_NULL {
        Some(PowerWatchError::NotifierUnavailable)
    } else if latches.signal.is_revoked() {
        Some(PowerWatchError::LateRevocation)
    } else {
        None
    };
    match error {
        Some(error) => fail_start(latches, error),
        None => Ok(()),
    }
}

fn teardown_registration(mut registration: Registration) {
    // SAFETY: IOKit requires deregistration before port destruction; queue finalization closes context.
    unsafe {
        let _ = IODeregisterForSystemPower(&mut registration.notifier);
        IONotificationPortDestroy(registration.port);
        dispatch_release(registration.queue);
    }
}

unsafe fn cleanup_failed_registration(
    connection: IoConnect,
    port: IONotificationPortRef,
    mut notifier: IoObject,
    context: *mut MaybeUninit<PowerContext>,
) {
    if connection != IO_OBJECT_NULL && notifier != IO_OBJECT_NULL {
        // SAFETY: a non-null connection and notifier came from the same registration call.
        unsafe {
            let _ = IODeregisterForSystemPower(&mut notifier);
        }
    }
    if !port.is_null() {
        // SAFETY: this port was returned by IORegisterForSystemPower and is not delivery-enabled.
        unsafe { IONotificationPortDestroy(port) };
    }
    if connection != IO_OBJECT_NULL {
        // SAFETY: failed registration has no queue finalizer, so it alone closes the connection.
        unsafe {
            let _ = IOServiceClose(connection);
        }
    }
    // SAFETY: the allocation holds MaybeUninit because delivery was never enabled.
    unsafe { drop(Box::from_raw(context)) };
}

unsafe extern "C" fn power_notification_callback(
    refcon: *mut c_void,
    _service: IoService,
    message_type: u32,
    message_argument: *mut c_void,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: the dispatch queue finalizer runs only after its IOKit dispatch source is cancelled.
    let context = unsafe { &*refcon.cast::<PowerContext>() };
    handle_power_notification(
        context,
        message_type,
        message_argument as isize,
        |connection, id| {
            // SAFETY: this acknowledgement uses the immutable connection registered for this callback.
            unsafe { IOAllowPowerChange(connection, id) }
        },
    );
}

fn handle_power_notification(
    context: &PowerContext,
    message_type: u32,
    notification_id: isize,
    acknowledge: impl FnOnce(IoConnect, isize) -> IoReturn,
) {
    match message_type {
        IO_MESSAGE_CAN_SYSTEM_SLEEP => {
            if acknowledge(context.connection, notification_id) != IO_RETURN_SUCCESS {
                context.latches.revoke_without_wake(
                    StopReason::NativeFailure,
                    "the sleep acknowledgement failed",
                );
            }
        }
        IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            context
                .latches
                .revoke_without_wake(StopReason::Requested, "the system is going to sleep");
            if acknowledge(context.connection, notification_id) != IO_RETURN_SUCCESS {
                context.latches.revoke_without_wake(
                    StopReason::NativeFailure,
                    "the sleep acknowledgement failed",
                );
            }
        }
        IO_MESSAGE_SYSTEM_WILL_POWER_ON | IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            context
                .latches
                .revoke_without_wake(StopReason::Requested, "the system woke");
        }
        IO_MESSAGE_SYSTEM_WILL_NOT_SLEEP => {}
        _ => {}
    }
}

extern "C" fn power_context_finalizer(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: this finalizer is installed exactly once with the initialized Box allocation.
    unsafe {
        let context = Box::from_raw(context.cast::<PowerContext>());
        let _ = IOServiceClose(context.connection);
        drop(context);
    }
}

// SAFETY: declarations match the installed IOKit and libdispatch SDK headers.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut IONotificationPortRef,
        callback: Option<IOServiceInterestCallback>,
        notifier: *mut IoObject,
    ) -> IoConnect;
    fn IODeregisterForSystemPower(notifier: *mut IoObject) -> IoReturn;
    fn IOAllowPowerChange(connection: IoConnect, notification_id: isize) -> IoReturn;
    fn IOServiceClose(connection: IoConnect) -> IoReturn;
    fn IONotificationPortDestroy(port: IONotificationPortRef);
    fn IONotificationPortSetDispatchQueue(port: IONotificationPortRef, queue: DispatchQueueRef);
}

#[link(name = "System")]
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueRef;
    fn dispatch_set_context(queue: DispatchQueueRef, context: *mut c_void);
    fn dispatch_set_finalizer_f(
        queue: DispatchQueueRef,
        finalizer: Option<extern "C" fn(*mut c_void)>,
    );
    #[cfg(test)]
    fn dispatch_async_f(
        queue: DispatchQueueRef,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_release(queue: DispatchQueueRef);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> (PowerContext, RevocationSignal, CaptureStop) {
        let signal = RevocationSignal::default();
        let stop = CaptureStop::default();
        (
            PowerContext {
                connection: 7,
                latches: PowerLatches {
                    signal: signal.clone(),
                    stop: Some(stop.clone()),
                },
            },
            signal,
            stop,
        )
    }

    #[test]
    fn notification_mapping_acknowledges_only_sleep_requests_and_never_reenables() {
        let (context, signal, stop) = test_context();
        let mut acknowledgements = Vec::new();
        handle_power_notification(
            &context,
            IO_MESSAGE_CAN_SYSTEM_SLEEP,
            19,
            |connection, id| {
                acknowledgements.push((connection, id));
                IO_RETURN_SUCCESS
            },
        );
        assert_eq!(acknowledgements, vec![(7, 19)]);
        assert!(!signal.is_revoked());
        assert!(!stop.is_stopped());

        handle_power_notification(&context, IO_MESSAGE_SYSTEM_WILL_NOT_SLEEP, 20, |_, _| {
            panic!("WillNotSleep must not be acknowledged")
        });
        assert!(!signal.is_revoked());

        handle_power_notification(
            &context,
            IO_MESSAGE_SYSTEM_WILL_SLEEP,
            21,
            |connection, id| {
                acknowledgements.push((connection, id));
                IO_RETURN_SUCCESS
            },
        );
        assert_eq!(acknowledgements, vec![(7, 19), (7, 21)]);
        assert!(signal.is_revoked());
        assert_eq!(stop.reason(), Some(StopReason::Requested));
    }

    #[test]
    fn wake_and_ack_failure_permanently_revoke_without_waiting_for_cleanup() {
        for message in [
            IO_MESSAGE_SYSTEM_WILL_POWER_ON,
            IO_MESSAGE_SYSTEM_HAS_POWERED_ON,
        ] {
            let (context, signal, stop) = test_context();
            handle_power_notification(&context, message, 0, |_, _| {
                panic!("wake messages must not be acknowledged")
            });
            assert!(signal.is_revoked());
            assert_eq!(stop.reason(), Some(StopReason::Requested));
        }

        let (context, signal, stop) = test_context();
        handle_power_notification(&context, IO_MESSAGE_CAN_SYSTEM_SLEEP, 0, |_, _| -1);
        assert!(signal.is_revoked());
        assert_eq!(stop.reason(), Some(StopReason::NativeFailure));

        let (context, signal, stop) = test_context();
        handle_power_notification(&context, IO_MESSAGE_SYSTEM_WILL_SLEEP, 0, |_, _| -1);
        assert!(signal.is_revoked());
        assert_eq!(stop.reason(), Some(StopReason::Requested));
    }

    #[test]
    fn a_capture_watch_drop_stops_its_capture_and_a_network_watch_drop_revokes() {
        let (context, signal, stop) = test_context();
        drop(PowerWatch {
            latches: context.latches.clone(),
            registration: None,
        });
        assert_eq!(stop.reason(), Some(StopReason::Requested));
        assert!(!signal.is_stopping(), "the capture decides the session");

        let signal = RevocationSignal::default();
        drop(PowerWatch {
            latches: PowerLatches {
                signal: signal.clone(),
                stop: None,
            },
            registration: None,
        });
        assert!(signal.is_revoked());
    }

    #[test]
    fn fake_registration_failures_and_late_cancellation_fail_closed() {
        for (queue, connection, port, notifier, expected) in [
            (
                ptr::null_mut(),
                7,
                ptr::dangling_mut(),
                3,
                PowerWatchError::DispatchQueueCreationFailed,
            ),
            (
                ptr::dangling_mut(),
                IO_OBJECT_NULL,
                ptr::dangling_mut(),
                3,
                PowerWatchError::ConnectionUnavailable,
            ),
            (
                ptr::dangling_mut(),
                7,
                ptr::null_mut(),
                3,
                PowerWatchError::NotificationPortUnavailable,
            ),
            (
                ptr::dangling_mut(),
                7,
                ptr::dangling_mut(),
                IO_OBJECT_NULL,
                PowerWatchError::NotifierUnavailable,
            ),
        ] {
            let latches = PowerLatches {
                signal: RevocationSignal::default(),
                stop: Some(CaptureStop::default()),
            };
            assert_eq!(
                require_registration(queue, connection, port, notifier, &latches),
                Err(expected)
            );
            assert!(latches.signal.is_revoked());
            assert_eq!(
                latches.stop.as_ref().and_then(CaptureStop::reason),
                Some(StopReason::NativeFailure)
            );
        }

        let latches = PowerLatches {
            signal: RevocationSignal::default(),
            stop: Some(CaptureStop::default()),
        };
        latches.signal.mark_revoked_without_wake();
        assert_eq!(
            require_registration(ptr::dangling_mut(), 7, ptr::dangling_mut(), 3, &latches),
            Err(PowerWatchError::LateRevocation)
        );
        assert!(latches.signal.is_revoked());
    }

    #[test]
    fn queue_finalizer_waits_for_a_gated_callback_before_reclaiming_context() {
        use std::{
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::Duration,
        };

        struct QueueContext {
            power: PowerContext,
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            finalized: mpsc::SyncSender<()>,
            acknowledged: Arc<AtomicBool>,
        }

        extern "C" fn gated_callback(context: *mut c_void) {
            // SAFETY: dispatch retains the queue, whose finalizer owns this context.
            let context = unsafe { &*context.cast::<QueueContext>() };
            handle_power_notification(
                &context.power,
                IO_MESSAGE_CAN_SYSTEM_SLEEP,
                29,
                |connection, id| {
                    if (connection, id) == (11, 29) {
                        context.acknowledged.store(true, Ordering::Release);
                        IO_RETURN_SUCCESS
                    } else {
                        -1
                    }
                },
            );
            let _ = context.entered.send(());
            let _ = context
                .release
                .lock()
                .expect("test gate lock")
                .recv_timeout(Duration::from_secs(1));
        }

        extern "C" fn test_finalizer(context: *mut c_void) {
            // SAFETY: this is the one Box installed as the queue context.
            let context = unsafe { Box::from_raw(context.cast::<QueueContext>()) };
            let _ = context.finalized.send(());
        }

        let queue = create_queue();
        assert!(!queue.is_null());
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (finalized_tx, finalized_rx) = mpsc::sync_channel(1);
        let acknowledged = Arc::new(AtomicBool::new(false));
        let context = Box::into_raw(Box::new(QueueContext {
            power: PowerContext {
                connection: 11,
                latches: PowerLatches {
                    signal: RevocationSignal::default(),
                    stop: None,
                },
            },
            entered: entered_tx,
            release: Mutex::new(release_rx),
            finalized: finalized_tx,
            acknowledged: acknowledged.clone(),
        }));
        // SAFETY: the finalizer takes this context only after the queued callback returns.
        unsafe {
            dispatch_set_context(queue, context.cast());
            dispatch_set_finalizer_f(queue, Some(test_finalizer));
            dispatch_async_f(queue, context.cast(), gated_callback);
            dispatch_release(queue);
        }

        let callback_entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        let finalized_before_release = finalized_rx.try_recv().is_ok();
        let _ = release_tx.send(());
        let finalizer_ran = finalized_rx.recv_timeout(Duration::from_secs(1)).is_ok();

        assert!(callback_entered);
        assert!(!finalized_before_release);
        assert!(finalizer_ran);
        assert!(acknowledged.load(Ordering::Acquire));
    }
}
