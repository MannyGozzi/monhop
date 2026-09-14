//! Explicit, fail-closed macOS network-change watcher.
//!
//! This module only subscribes to the dynamic store and the kernel routing socket after a local
//! enable action. Every notice re-reads the selected interface through the caller's check; a
//! changed, missing, or unreadable interface permanently revokes the watcher. Drop it before an
//! explicitly reconnected session.

use std::{
    error::Error,
    ffi::{c_char, c_void},
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::Arc,
    thread::JoinHandle,
};

use monhop_core::RevocationSignal;

use crate::network::RouteObserver;

use core_foundation::{
    array::{CFArray, CFArrayRef},
    base::{Boolean, CFAllocatorRef, CFIndex, CFRelease, CFTypeRef, TCFType},
    string::{CFString, CFStringRef},
};

const MAX_INTERFACE_NAME_BYTES: usize = 15;
const STORE_NAME: &str = "MonHop network watcher";
const DISPATCH_QUEUE_LABEL: &[u8] = b"com.manuelgozzi.monhop.network-watch\0";
const MAX_LOGGED_KEYS: usize = 6;

/// Re-reads the selected interface after a change notice; `false` revokes. It runs on the watch queue.
pub type PinnedCheck = Box<dyn Fn() -> bool + Send + Sync>;

type SCDynamicStoreRef = CFTypeRef;
type DispatchQueueRef = *mut c_void;
type SCDynamicStoreCallBack = extern "C" fn(SCDynamicStoreRef, CFArrayRef, *mut c_void);

#[repr(C)]
struct SCDynamicStoreContext {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
}

// SAFETY: These declarations match the installed SystemConfiguration SDK header.
#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCDynamicStoreCreate(
        allocator: CFAllocatorRef,
        name: CFStringRef,
        callback: Option<SCDynamicStoreCallBack>,
        context: *mut SCDynamicStoreContext,
    ) -> SCDynamicStoreRef;
    fn SCDynamicStoreSetNotificationKeys(
        store: SCDynamicStoreRef,
        keys: CFArrayRef,
        patterns: CFArrayRef,
    ) -> Boolean;
    fn SCDynamicStoreSetDispatchQueue(store: SCDynamicStoreRef, queue: DispatchQueueRef)
    -> Boolean;
}

// SAFETY: libdispatch is part of libSystem; these declarations match dispatch/queue.h.
#[link(name = "System")]
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueRef;
    fn dispatch_release(object: DispatchQueueRef);
}

const DISPATCH_QUEUE_CREATE_BINDING: unsafe extern "C" fn(
    *const c_char,
    *const c_void,
) -> DispatchQueueRef = dispatch_queue_create;
const DISPATCH_RELEASE_BINDING: unsafe extern "C" fn(DispatchQueueRef) = dispatch_release;
const DYNAMIC_STORE_SET_DISPATCH_QUEUE_BINDING: unsafe extern "C" fn(
    SCDynamicStoreRef,
    DispatchQueueRef,
) -> Boolean = SCDynamicStoreSetDispatchQueue;

/// Categories only. No variant contains an interface name or OS error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkWatchError {
    InvalidInterfaceName,
    PowerRegistrationFailed,
    RouteListenerFailed,
    DispatchQueueCreationFailed,
    DynamicStoreCreationFailed,
    NotificationRegistrationFailed,
    DispatchRegistrationFailed,
}

impl fmt::Display for NetworkWatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInterfaceName => "network interface name is invalid",
            Self::PowerRegistrationFailed => "macOS sleep and wake notifications could not start",
            Self::RouteListenerFailed => "macOS kernel route notifications could not start",
            Self::DispatchQueueCreationFailed => {
                "macOS network-change dispatch queue could not start"
            }
            Self::DynamicStoreCreationFailed => "macOS dynamic-store session could not start",
            Self::NotificationRegistrationFailed => {
                "macOS dynamic-store notification registration failed"
            }
            Self::DispatchRegistrationFailed => "macOS dynamic-store dispatch registration failed",
        })
    }
}

impl Error for NetworkWatchError {}

/// Watches the enabled interface and global routing state until dropped.
pub struct NetworkChangeWatcher {
    state: Arc<WatcherState>,
    registration: Option<DispatchRegistration>,
}

impl NetworkChangeWatcher {
    /// Starts after a local enable action and returns only after native registration succeeds.
    /// Authorize a fresh network snapshot afterward and reject this watcher if already revoked.
    pub fn start_after_local_enable(
        interface_name: &str,
        interface_index: u32,
        still_pinned: PinnedCheck,
    ) -> Result<Self, NetworkWatchError> {
        let patterns = notification_patterns(interface_name)?;
        let state = Arc::new(WatcherState::new(still_pinned));
        let power =
            crate::power_watch::PowerWatch::start_after_local_enable(state.signal.clone(), None)
                .map_err(|_| NetworkWatchError::PowerRegistrationFailed)?;
        let routes = RouteListener::start(Arc::clone(&state), interface_index)?;
        let queue = OwnedDispatchQueue::create()?;
        let store = OwnedDynamicStore::create(&state)?;

        configure_notification_patterns(&state, &store, patterns)?;
        // This Boolean is the native registration acknowledgement, not merely source construction.
        require_native_success(
            &state,
            // SAFETY: the store and its owned queue remain valid through registration and teardown.
            unsafe { DYNAMIC_STORE_SET_DISPATCH_QUEUE_BINDING(store.as_raw(), queue.as_raw()) },
            NetworkWatchError::DispatchRegistrationFailed,
        )?;

        let watcher = Self {
            state: Arc::clone(&state),
            registration: Some(DispatchRegistration {
                _power: power,
                _routes: routes,
                state,
                store,
                _queue: queue,
            }),
        };
        if watcher.is_revoked() {
            return Err(NetworkWatchError::PowerRegistrationFailed);
        }
        Ok(watcher)
    }

    /// Returns whether a watched dynamic-store change permanently revoked this watcher.
    pub fn is_revoked(&self) -> bool {
        self.state.is_revoked()
    }

    /// Socket workers receive only the latch, never the thread-affine native registration.
    pub fn revocation_signal(&self) -> RevocationSignal {
        self.state.signal.clone()
    }
}

impl Drop for NetworkChangeWatcher {
    fn drop(&mut self) {
        self.state.revoke();
        if let Some(registration) = self.registration.take() {
            drop(registration);
        }
    }
}

struct WatcherState {
    signal: RevocationSignal,
    still_pinned: PinnedCheck,
}

impl WatcherState {
    fn new(still_pinned: PinnedCheck) -> Self {
        Self {
            signal: RevocationSignal::default(),
            still_pinned,
        }
    }

    fn revoke(&self) {
        self.signal.revoke();
    }

    fn is_revoked(&self) -> bool {
        self.signal.is_revoked()
    }

    /// A notice revokes only when the pinned interface no longer reads back as it was selected.
    fn observe(&self, changed_keys: &str) {
        if self.is_revoked() {
            return;
        }
        if pinned_facts_hold(&self.still_pinned) {
            log::debug!(
                "network watch: {changed_keys} changed; the selected interface is unchanged, session continues"
            );
        } else {
            log::warn!(
                "network watch: {changed_keys} changed and the selected interface differs; revoked"
            );
            self.revoke();
        }
    }
}

/// A check that panics must not unwind through the native callback, so it revokes instead.
fn pinned_facts_hold(still_pinned: &PinnedCheck) -> bool {
    match catch_unwind(AssertUnwindSafe(still_pinned)) {
        Ok(unchanged) => unchanged,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    }
}

struct OwnedDynamicStore(SCDynamicStoreRef);

impl OwnedDynamicStore {
    fn create(state: &Arc<WatcherState>) -> Result<Self, NetworkWatchError> {
        let name = CFString::from_static_string(STORE_NAME);
        let mut context = SCDynamicStoreContext {
            version: 0,
            info: Arc::as_ptr(state).cast_mut().cast::<c_void>(),
            retain: Some(dynamic_store_context_retain),
            release: Some(dynamic_store_context_release),
            copy_description: None,
        };
        // SAFETY: Core Foundation owns a retained context before this function can return it.
        let store = unsafe {
            SCDynamicStoreCreate(
                ptr::null(),
                name.as_concrete_TypeRef(),
                Some(dynamic_store_callback),
                &mut context,
            )
        };
        if store.is_null() {
            state.revoke();
            return Err(NetworkWatchError::DynamicStoreCreationFailed);
        }
        Ok(Self(store))
    }

    fn as_raw(&self) -> SCDynamicStoreRef {
        self.0
    }
}

impl Drop for OwnedDynamicStore {
    fn drop(&mut self) {
        // SAFETY: SCDynamicStoreCreate returned this retained Core Foundation object.
        unsafe { CFRelease(self.0) };
    }
}

struct OwnedDispatchQueue(DispatchQueueRef);

impl OwnedDispatchQueue {
    fn create() -> Result<Self, NetworkWatchError> {
        // SAFETY: the static label is NUL-terminated and a null attribute requests a serial queue.
        let queue = unsafe {
            DISPATCH_QUEUE_CREATE_BINDING(
                DISPATCH_QUEUE_LABEL.as_ptr().cast::<c_char>(),
                ptr::null(),
            )
        };
        if queue.is_null() {
            return Err(NetworkWatchError::DispatchQueueCreationFailed);
        }
        Ok(Self(queue))
    }

    fn as_raw(&self) -> DispatchQueueRef {
        self.0
    }
}

impl Drop for OwnedDispatchQueue {
    fn drop(&mut self) {
        // SAFETY: dispatch_queue_create returned this retained queue.
        unsafe { DISPATCH_RELEASE_BINDING(self.0) };
    }
}

/// Kernel route changes never reach the dynamic store, so a routing socket feeds the same check.
struct RouteListener {
    observer: Arc<RouteObserver>,
    thread: Option<JoinHandle<()>>,
}

impl RouteListener {
    fn start(state: Arc<WatcherState>, interface_index: u32) -> Result<Self, NetworkWatchError> {
        let observer =
            Arc::new(RouteObserver::open().map_err(|_| NetworkWatchError::RouteListenerFailed)?);
        let reader = Arc::clone(&observer);
        let thread = std::thread::Builder::new()
            .name("monhop-route-watch".to_owned())
            .spawn(move || {
                loop {
                    match reader.next(interface_index) {
                        Ok(Some(change)) => state.observe(&format!("kernel route {change:?}")),
                        Ok(None) => break,
                        Err(_) => {
                            log::warn!("network watch: the kernel route listener failed; revoked");
                            state.revoke();
                            break;
                        }
                    }
                }
            })
            .map_err(|_| NetworkWatchError::RouteListenerFailed)?;
        Ok(Self {
            observer,
            thread: Some(thread),
        })
    }
}

impl Drop for RouteListener {
    fn drop(&mut self) {
        self.observer.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct DispatchRegistration {
    _power: crate::power_watch::PowerWatch,
    _routes: RouteListener,
    state: Arc<WatcherState>,
    store: OwnedDynamicStore,
    _queue: OwnedDispatchQueue,
}

impl Drop for DispatchRegistration {
    fn drop(&mut self) {
        // SAFETY: only this registration enables the store's dispatch queue and drops it once.
        let unregistered = unsafe {
            DYNAMIC_STORE_SET_DISPATCH_QUEUE_BINDING(self.store.as_raw(), ptr::null_mut())
        };
        record_unregistration_result(&self.state, unregistered);
    }
}

fn configure_notification_patterns(
    state: &WatcherState,
    store: &OwnedDynamicStore,
    patterns: Vec<String>,
) -> Result<(), NetworkWatchError> {
    let patterns: Vec<CFString> = patterns
        .iter()
        .map(|pattern| CFString::new(pattern))
        .collect();
    let patterns = CFArray::from_CFTypes(&patterns);
    // SAFETY: the store and pattern array remain valid for this synchronous registration call.
    let registered = unsafe {
        SCDynamicStoreSetNotificationKeys(
            store.as_raw(),
            ptr::null(),
            patterns.as_concrete_TypeRef(),
        )
    };
    require_native_success(
        state,
        registered,
        NetworkWatchError::NotificationRegistrationFailed,
    )
}

fn require_native_success(
    state: &WatcherState,
    result: Boolean,
    error: NetworkWatchError,
) -> Result<(), NetworkWatchError> {
    if result == 0 {
        log::warn!("network watch: {error:?}; revoked");
        state.revoke();
        return Err(error);
    }
    Ok(())
}

fn record_unregistration_result(state: &WatcherState, result: Boolean) {
    if result == 0 {
        state.revoke();
    }
}

extern "C" fn dynamic_store_context_retain(info: *const c_void) -> *const c_void {
    // SAFETY: Core Foundation calls retain only while its existing context reference is alive.
    unsafe { Arc::increment_strong_count(info.cast::<WatcherState>()) };
    info
}

extern "C" fn dynamic_store_context_release(info: *const c_void) {
    // SAFETY: each Core Foundation context release balances the retain above.
    unsafe { drop(Arc::from_raw(info.cast::<WatcherState>())) };
}

extern "C" fn dynamic_store_callback(
    _store: SCDynamicStoreRef,
    changed_keys: CFArrayRef,
    info: *mut c_void,
) {
    let changed_keys = changed_key_summary(changed_keys);
    // SAFETY: the dynamic-store context retains this state through queued and in-flight callbacks.
    unsafe { (&*info.cast::<WatcherState>()).observe(&changed_keys) };
}

/// Dynamic-store keys name interfaces and services only, never addresses or network names.
fn changed_key_summary(changed_keys: CFArrayRef) -> String {
    if changed_keys.is_null() {
        return "unknown keys".to_owned();
    }
    // SAFETY: SystemConfiguration passes an array of CFString keys that outlives this callback.
    let keys: CFArray<CFString> = unsafe { CFArray::wrap_under_get_rule(changed_keys) };
    keys.iter()
        .take(MAX_LOGGED_KEYS)
        .map(|key| key.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Only the selected interface's own keys and the primary IPv4 service can affect the pinned socket.
fn notification_patterns(interface_name: &str) -> Result<Vec<String>, NetworkWatchError> {
    validate_interface_name(interface_name)?;
    Ok(vec![
        format!("^State:/Network/Interface/{interface_name}/.*$"),
        "^State:/Network/Global/IPv4$".to_owned(),
    ])
}

fn validate_interface_name(interface_name: &str) -> Result<(), NetworkWatchError> {
    if interface_name.is_empty()
        || interface_name.len() > MAX_INTERFACE_NAME_BYTES
        || !interface_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(NetworkWatchError::InvalidInterfaceName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    const NATIVE_LIFECYCLE_ITERATIONS: usize = 8;
    const NATIVE_LIFECYCLE_LIMIT: Duration = Duration::from_secs(5);

    #[test]
    fn patterns_cover_only_the_selected_interface_and_the_primary_ipv4_service() {
        assert_eq!(
            notification_patterns("en0"),
            Ok(vec![
                "^State:/Network/Interface/en0/.*$".to_owned(),
                "^State:/Network/Global/IPv4$".to_owned(),
            ])
        );
    }

    #[test]
    fn a_notice_with_unchanged_facts_keeps_the_session() {
        let state = WatcherState::new(Box::new(|| true));
        state.observe("State:/Network/Interface/en0/IPv6");
        assert!(!state.is_revoked());
    }

    #[test]
    fn a_notice_with_changed_facts_revokes() {
        let state = WatcherState::new(Box::new(|| false));
        state.observe("State:/Network/Interface/en0/IPv4");
        assert!(state.is_revoked());
    }

    #[test]
    fn a_panicking_check_revokes_instead_of_unwinding() {
        let state = WatcherState::new(Box::new(|| -> bool { panic!("check failed") }));
        state.observe("State:/Network/Global/IPv4");
        assert!(state.is_revoked());
    }

    #[test]
    fn interface_validation_rejects_unbounded_or_pattern_input() {
        for interface_name in ["", "en0.*", "abcdefghijklmnop", "é0"] {
            assert_eq!(
                notification_patterns(interface_name),
                Err(NetworkWatchError::InvalidInterfaceName)
            );
        }
    }

    #[test]
    fn revocation_latch_is_irreversible() {
        let state = WatcherState::new(Box::new(|| true));
        assert!(!state.is_revoked());
        state.revoke();
        assert!(state.is_revoked());
    }

    #[test]
    fn dropping_owner_revokes_escaped_socket_signal() {
        let watcher = NetworkChangeWatcher {
            state: Arc::new(WatcherState::new(Box::new(|| true))),
            registration: None,
        };
        let signal = watcher.revocation_signal();
        assert!(!signal.is_revoked());
        drop(watcher);
        assert!(signal.is_revoked());
    }

    #[test]
    fn registration_failure_revokes_and_propagates() {
        let state = WatcherState::new(Box::new(|| true));
        assert_eq!(
            require_native_success(&state, 0, NetworkWatchError::DispatchRegistrationFailed,),
            Err(NetworkWatchError::DispatchRegistrationFailed)
        );
        assert!(state.is_revoked());
    }

    #[test]
    fn failed_unregistration_revokes_without_rearming() {
        let state = WatcherState::new(Box::new(|| true));
        record_unregistration_result(&state, 0);
        assert!(state.is_revoked());
    }

    #[test]
    fn context_retain_release_keeps_state_alive_for_native_callbacks() {
        let state = Arc::new(WatcherState::new(Box::new(|| true)));
        let weak = Arc::downgrade(&state);
        let info = Arc::as_ptr(&state).cast::<c_void>();

        assert_eq!(dynamic_store_context_retain(info), info);
        drop(state);
        assert!(weak.upgrade().is_some());
        dynamic_store_context_release(info);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    #[ignore = "explicit native Dynamic Store lifecycle check"]
    fn native_repeated_immediate_start_drop_is_bounded() {
        let started = Instant::now();
        for _ in 0..NATIVE_LIFECYCLE_ITERATIONS {
            let watcher =
                NetworkChangeWatcher::start_after_local_enable("lo0", 1, Box::new(|| true))
                    .unwrap();
            let signal = watcher.revocation_signal();
            drop(watcher);
            assert!(signal.is_revoked());
        }
        assert!(started.elapsed() < NATIVE_LIFECYCLE_LIMIT);
    }
}
