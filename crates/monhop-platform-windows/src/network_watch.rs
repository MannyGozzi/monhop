//! Explicit, fail-closed network-change revocation for one native session.
//!
//! Every IP Helper notice re-reads the selected adapter through the caller's check; only a changed,
//! missing, or unreadable adapter revokes. Two notices revoke without the check: a deletion of the
//! selected interface or its pinned address, which is final even if a later add restores the same
//! values before the check runs, and a Wi-Fi event on the selected adapter that leaves the network.
//! A finished roam runs the check on a helper thread, because it queries WLAN state and a WLAN
//! callback must not.

#[cfg(any(windows, test))]
use monhop_core::RevocationSignal;
#[cfg(any(windows, test))]
use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
};

#[cfg(windows)]
use std::{ffi::c_void, net::Ipv4Addr, ptr};

#[cfg(windows)]
use windows_sys::{
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::{
            IpHelper::{
                CancelMibChangeNotify2, GetIfEntry2, IF_TYPE_IEEE80211, MIB_IF_ROW2,
                MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
                MIB_UNICASTIPADDRESS_ROW, NotifyIpInterfaceChange, NotifyRouteChange2,
                NotifyUnicastIpAddressChange,
            },
            WiFi::{
                L2_NOTIFICATION_DATA, WLAN_NOTIFICATION_SOURCE_ACM, WLAN_NOTIFICATION_SOURCE_MSM,
                WLAN_NOTIFICATION_SOURCE_NONE, WlanCloseHandle, WlanOpenHandle,
                WlanRegisterNotification,
            },
        },
        Networking::WinSock::{AF_INET, AF_UNSPEC},
    },
    core::GUID,
};

#[cfg(any(windows, test))]
const WLAN_SOURCE_ACM: u32 = 8;
#[cfg(any(windows, test))]
const WLAN_SOURCE_MSM: u32 = 16;
#[cfg(any(windows, test))]
const MIB_INITIAL_NOTIFICATION: i32 = 3;
#[cfg(any(windows, test))]
const MIB_DELETE_INSTANCE: i32 = 2;

#[cfg(any(windows, test))]
fn selected_row_deleted(notification_type: i32, row_is_selected: bool) -> bool {
    notification_type == MIB_DELETE_INSTANCE && row_is_selected
}

/// Re-reads the selected adapter after an IP Helper notice or a finished roam; `false` revokes. It runs
/// on an IP Helper notification thread or the recheck thread, never inside a WLAN callback, so it may
/// query WLAN state.
#[cfg(any(windows, test))]
pub type PinnedCheck = Box<dyn Fn() -> bool + Send + Sync>;

/// A check that panics must not unwind through the native callback, so it revokes instead.
#[cfg(any(windows, test))]
fn pinned_facts_hold(still_pinned: &PinnedCheck) -> bool {
    match catch_unwind(AssertUnwindSafe(still_pinned)) {
        Ok(unchanged) => unchanged,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    }
}

/// Revokes unless the pinned facts still hold. A revoked session is never rechecked.
#[cfg(any(windows, test))]
fn observe(latch: &RevocationSignal, still_pinned: &PinnedCheck, change: &str) {
    if latch.is_revoked() {
        return;
    }
    if pinned_facts_hold(still_pinned) {
        log::debug!(
            "network watch: {change}; the selected adapter is unchanged, session continues"
        );
    } else {
        log::warn!("network watch: {change} and the selected adapter differs; revoked");
        latch.revoke();
    }
}

#[cfg(any(windows, test))]
fn is_network_change(notification_type: i32) -> bool {
    notification_type != MIB_INITIAL_NOTIFICATION
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WifiEventAction {
    Ignore,
    Revoke,
    Recheck,
}

/// Leaving the network revokes at once. A roam stays on it, so only its end rechecks: the steps in
/// between read mid-roam state, and a roam that fails ends in a disconnect.
#[cfg(any(windows, test))]
fn wifi_event_action(source: u32, code: u32) -> WifiEventAction {
    match (source, code) {
        (
            WLAN_SOURCE_ACM,
            9  // connection_start
            | 10 // connection_complete
            | 11 // connection_attempt_fail
            | 13 // interface_arrival
            | 14 // interface_removal
            | 18 // network_not_available
            | 19 // network_available
            | 20 // disconnecting
            | 21 // disconnected
            | 27, // operational_state_change
        )
        | (
            WLAN_SOURCE_MSM,
            7  // radio_state_change
            | 9  // disassociating
            | 10 // disconnected
            | 13 // adapter_removal
            | 14, // adapter_operation_mode_change
        ) => WifiEventAction::Revoke,
        (WLAN_SOURCE_MSM, 6) => WifiEventAction::Recheck, // roaming_end
        _ => WifiEventAction::Ignore,
    }
}

#[cfg(any(windows, test))]
fn selected_wifi_event_action(interface_matches: bool, source: u32, code: u32) -> WifiEventAction {
    if interface_matches {
        wifi_event_action(source, code)
    } else {
        WifiEventAction::Ignore
    }
}

#[cfg(any(windows, test))]
enum Recheck {
    Run,
    Stop,
}

/// Runs the pinned check off the WLAN callback thread. Dropping it stops and joins the thread.
#[cfg(any(windows, test))]
struct RecheckWorker {
    requests: SyncSender<Recheck>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(any(windows, test))]
impl RecheckWorker {
    fn start(latch: RevocationSignal, still_pinned: Arc<PinnedCheck>) -> io::Result<Self> {
        // One slot: a queued run already covers every roam that ends before it starts.
        let (requests, pending) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("monhop-wifi-recheck".to_owned())
            .spawn(move || run_rechecks(&pending, &latch, &still_pinned))?;
        Ok(Self {
            requests,
            thread: Some(thread),
        })
    }

    /// Never blocks, so a WLAN callback may call it. False only when the worker is gone.
    fn request(&self) -> bool {
        !matches!(
            self.requests.try_send(Recheck::Run),
            Err(TrySendError::Disconnected(_))
        )
    }
}

#[cfg(any(windows, test))]
fn run_rechecks(pending: &Receiver<Recheck>, latch: &RevocationSignal, still_pinned: &PinnedCheck) {
    while let Ok(Recheck::Run) = pending.recv() {
        observe(latch, still_pinned, "a Wi-Fi roam finished");
    }
}

#[cfg(any(windows, test))]
impl Drop for RecheckWorker {
    fn drop(&mut self) {
        let _ = self.requests.send(Recheck::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
struct OsInterfaceIdentity {
    index: u32,
    guid: [u8; 16],
    wifi: bool,
}

#[cfg(windows)]
impl PartialEq for OsInterfaceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.guid == other.guid && self.wifi == other.wifi
    }
}

#[cfg(windows)]
impl Eq for OsInterfaceIdentity {}

#[cfg(windows)]
struct CallbackContext {
    latch: RevocationSignal,
    wifi_identity: [u8; 16],
    wifi: bool,
    index: u32,
    address: Ipv4Addr,
    still_pinned: Arc<PinnedCheck>,
    /// Present exactly when the adapter is Wi-Fi. Dropping the context stops it.
    recheck: Option<RecheckWorker>,
}

#[cfg(windows)]
impl CallbackContext {
    fn lost(&self, what: &str) {
        log::warn!("network watch: {what}; revoked");
        self.latch.revoke();
    }

    /// A notice revokes only when the pinned adapter no longer reads back as it was selected.
    fn observe(&self, change: &str) {
        observe(&self.latch, &self.still_pinned, change);
    }
}

/// A started watcher can only move from usable to revoked. It never rearms.
#[cfg(windows)]
pub struct NetworkChangeWatch {
    signal: RevocationSignal,
    context: Option<Box<CallbackContext>>,
    interface_change: Option<MibRegistration>,
    address_change: Option<MibRegistration>,
    route_change: Option<MibRegistration>,
    wifi_change: Option<WlanRegistration>,
    _power: crate::power_watch::PowerWatch,
}

#[cfg(windows)]
impl NetworkChangeWatch {
    /// Starts native notifications for a previously selected adapter. This has no implicit startup path.
    ///
    /// Authorize the peer after start, and reject every use once `is_revoked()` is true.
    pub fn start(
        selected: &crate::network::Adapter,
        still_pinned: PinnedCheck,
    ) -> io::Result<Self> {
        let initial = resolve_os_identity(selected.index)?;
        validate_selected_adapter(selected, &initial)?;

        let signal = RevocationSignal::default();
        let power = crate::power_watch::PowerWatch::start(signal.clone())?;
        let still_pinned = Arc::new(still_pinned);
        let recheck = if initial.wifi {
            Some(RecheckWorker::start(
                signal.clone(),
                Arc::clone(&still_pinned),
            )?)
        } else {
            None
        };
        let context = Box::new(CallbackContext {
            latch: signal.clone(),
            wifi_identity: initial.guid,
            wifi: initial.wifi,
            index: selected.index,
            address: selected.address,
            still_pinned,
            recheck,
        });
        let context_ptr = (&*context as *const CallbackContext).cast::<c_void>();
        let mut watch = Self {
            signal,
            context: Some(context),
            interface_change: None,
            address_change: None,
            route_change: None,
            wifi_change: None,
            _power: power,
        };

        // AF_UNSPEC requests both IPv4 and IPv6 notifications. Initial snapshots are disabled.
        watch.interface_change = Some(MibRegistration::interface(context_ptr)?);
        watch.address_change = Some(MibRegistration::address(context_ptr)?);
        watch.route_change = Some(MibRegistration::route(context_ptr)?);
        if initial.wifi {
            watch.wifi_change = Some(WlanRegistration::start(context_ptr)?);
        }

        // A change between selection and complete registration is not trusted as a ready session.
        if resolve_os_identity(selected.index)? != initial {
            log::warn!("network watch: the selected interface changed while registering; revoked");
            watch.revoke();
        }
        Ok(watch)
    }

    pub fn is_revoked(&self) -> bool {
        self.signal.is_revoked()
    }

    /// Socket workers receive only the latch, never the native registration handles.
    pub fn revocation_signal(&self) -> RevocationSignal {
        self.signal.clone()
    }

    fn revoke(&self) {
        self.signal.revoke();
    }
}

#[cfg(windows)]
impl Drop for NetworkChangeWatch {
    fn drop(&mut self) {
        self.revoke();
        // Windows documents unregistering as the synchronization point for in-flight callbacks.
        let mut callbacks_stopped = self
            .interface_change
            .as_mut()
            .is_none_or(MibRegistration::cancel);
        callbacks_stopped &= self
            .address_change
            .as_mut()
            .is_none_or(MibRegistration::cancel);
        callbacks_stopped &= self
            .route_change
            .as_mut()
            .is_none_or(MibRegistration::cancel);
        callbacks_stopped &= self
            .wifi_change
            .as_mut()
            .is_none_or(WlanRegistration::unregister);

        if !callbacks_stopped {
            // A failed native cancellation cannot safely free callback storage. Leak rather than UAF.
            if let Some(context) = self.context.take() {
                let _ = Box::leak(context);
            }
        }
    }
}

#[cfg(windows)]
struct MibRegistration {
    handle: HANDLE,
}

#[cfg(windows)]
impl MibRegistration {
    fn interface(context: *const c_void) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        // SAFETY: the callback and context remain valid until CancelMibChangeNotify2 succeeds.
        win_result(unsafe {
            NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(ip_interface_changed),
                context,
                false,
                &mut handle,
            )
        })?;
        Self::from_handle(handle)
    }

    fn address(context: *const c_void) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        // SAFETY: the callback and context remain valid until CancelMibChangeNotify2 succeeds.
        win_result(unsafe {
            NotifyUnicastIpAddressChange(
                AF_UNSPEC,
                Some(ip_address_changed),
                context,
                false,
                &mut handle,
            )
        })?;
        Self::from_handle(handle)
    }

    fn route(context: *const c_void) -> io::Result<Self> {
        let mut handle = ptr::null_mut();
        // SAFETY: the callback and context remain valid until CancelMibChangeNotify2 succeeds.
        win_result(unsafe {
            NotifyRouteChange2(AF_UNSPEC, Some(route_changed), context, false, &mut handle)
        })?;
        Self::from_handle(handle)
    }

    fn from_handle(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            return Err(io::Error::other(
                "Network-change registration returned no handle",
            ));
        }
        Ok(Self { handle })
    }

    fn cancel(&mut self) -> bool {
        if self.handle.is_null() {
            return true;
        }
        let handle = std::mem::replace(&mut self.handle, ptr::null_mut());
        // SAFETY: this handle came from exactly one successful MIB notification registration.
        unsafe { CancelMibChangeNotify2(handle) == 0 }
    }
}

#[cfg(windows)]
struct WlanRegistration {
    handle: HANDLE,
}

#[cfg(windows)]
impl WlanRegistration {
    fn start(context: *const c_void) -> io::Result<Self> {
        let mut version = 0;
        let mut handle = ptr::null_mut();
        // SAFETY: all output pointers are writable and the reserved argument is null.
        win_result(unsafe { WlanOpenHandle(2, ptr::null(), &mut version, &mut handle) })?;
        if handle.is_null() {
            return Err(io::Error::other("Wi-Fi registration returned no handle"));
        }
        // SAFETY: the live handle, callback, and stable context meet the WLAN API contract.
        let result = unsafe {
            WlanRegisterNotification(
                handle,
                WLAN_NOTIFICATION_SOURCE_ACM | WLAN_NOTIFICATION_SOURCE_MSM,
                0,
                Some(wifi_changed),
                context,
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if result != 0 {
            // SAFETY: no callback was registered after a failed registration attempt.
            unsafe {
                WlanCloseHandle(handle, ptr::null());
            }
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        Ok(Self { handle })
    }

    fn unregister(&mut self) -> bool {
        if self.handle.is_null() {
            return true;
        }
        let handle = std::mem::replace(&mut self.handle, ptr::null_mut());
        // SAFETY: the handle is live; unregistering waits for its in-flight callback before returning.
        let unregistered = unsafe {
            WlanRegisterNotification(
                handle,
                WLAN_NOTIFICATION_SOURCE_NONE,
                0,
                None,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            ) == 0
        };
        // SAFETY: the handle is closed once after unregistering, even if unregistering failed.
        unsafe {
            WlanCloseHandle(handle, ptr::null());
        }
        unregistered
    }
}

#[cfg(windows)]
unsafe extern "system" fn ip_interface_changed(
    caller_context: *const c_void,
    row: *const MIB_IPINTERFACE_ROW,
    notification_type: MIB_NOTIFICATION_TYPE,
) {
    if !is_network_change(notification_type) {
        return;
    }
    // SAFETY: successful registrations retain the context until synchronized cancellation.
    let Some(context) = (unsafe { callback_context(caller_context) }) else {
        return;
    };
    // SAFETY: Windows keeps the row alive for the callback; only its family and index are read.
    let selected = unsafe { row.as_ref() }
        .is_some_and(|row| row.Family == AF_INET && row.InterfaceIndex == context.index);
    if selected_row_deleted(notification_type, selected) {
        context.lost("the selected interface was deleted");
    } else {
        context.observe("interface change");
    }
}

#[cfg(windows)]
unsafe extern "system" fn ip_address_changed(
    caller_context: *const c_void,
    row: *const MIB_UNICASTIPADDRESS_ROW,
    notification_type: MIB_NOTIFICATION_TYPE,
) {
    if !is_network_change(notification_type) {
        return;
    }
    // SAFETY: successful registrations retain the context until synchronized cancellation.
    let Some(context) = (unsafe { callback_context(caller_context) }) else {
        return;
    };
    // SAFETY: Windows keeps the row alive for the callback; only its index and address are read.
    let selected = unsafe { row.as_ref() }.is_some_and(|row| {
        row.InterfaceIndex == context.index
            && crate::network::ipv4_from_sockaddr(&row.Address)
                .is_ok_and(|address| address == context.address)
    });
    if selected_row_deleted(notification_type, selected) {
        context.lost("the selected address was deleted");
    } else {
        context.observe("address change");
    }
}

#[cfg(windows)]
unsafe extern "system" fn route_changed(
    caller_context: *const c_void,
    _row: *const MIB_IPFORWARD_ROW2,
    notification_type: MIB_NOTIFICATION_TYPE,
) {
    if !is_network_change(notification_type) {
        return;
    }
    // SAFETY: successful registrations retain the context until synchronized cancellation.
    if let Some(context) = unsafe { callback_context(caller_context) } {
        context.observe("route change");
    }
}

#[cfg(windows)]
unsafe extern "system" fn wifi_changed(
    notification: *mut L2_NOTIFICATION_DATA,
    caller_context: *mut c_void,
) {
    // SAFETY: Windows owns the notification for the callback duration; context is held by watch.
    let (Some(notification), Some(context)) = (unsafe {
        (
            notification.as_ref(),
            caller_context.cast::<CallbackContext>().as_ref(),
        )
    }) else {
        return;
    };
    let action = selected_wifi_event_action(
        context.wifi && guid_bytes(&notification.InterfaceGuid) == context.wifi_identity,
        notification.NotificationSource,
        notification.NotificationCode,
    );
    let revoke = match action {
        WifiEventAction::Ignore => false,
        WifiEventAction::Recheck => !context.recheck.as_ref().is_some_and(RecheckWorker::request),
        WifiEventAction::Revoke => true,
    };
    if revoke {
        log::warn!(
            "network watch: Wi-Fi event source {} code {} revoked the session",
            notification.NotificationSource,
            notification.NotificationCode
        );
        context.latch.revoke();
    }
}

#[cfg(windows)]
unsafe fn callback_context<'a>(caller_context: *const c_void) -> Option<&'a CallbackContext> {
    // SAFETY: all native registrations receive only the pointer owned by NetworkChangeWatch.
    unsafe { caller_context.cast::<CallbackContext>().as_ref() }
}

#[cfg(windows)]
fn resolve_os_identity(index: u32) -> io::Result<OsInterfaceIdentity> {
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A nonzero interface index is required",
        ));
    }
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: InterfaceIndex identifies the requested system row, which Windows fills synchronously.
    win_result(unsafe { GetIfEntry2(&mut row) })?;
    if row.InterfaceIndex != index {
        return Err(io::Error::other(
            "Windows returned an unexpected interface index",
        ));
    }
    Ok(OsInterfaceIdentity {
        index,
        guid: guid_bytes(&row.InterfaceGuid),
        wifi: row.Type == IF_TYPE_IEEE80211,
    })
}

#[cfg(windows)]
fn validate_selected_adapter(
    selected: &crate::network::Adapter,
    identity: &OsInterfaceIdentity,
) -> io::Result<()> {
    if selected.stable_id != hex_guid(identity.guid) || selected.wifi != identity.wifi {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Selected interface no longer matches the operating-system identity",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn guid_bytes(guid: &GUID) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&guid.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    bytes[8..].copy_from_slice(&guid.data4);
    bytes
}

#[cfg(windows)]
fn hex_guid(guid: [u8; 16]) -> String {
    guid.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(windows)]
fn win_result(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use super::*;

    #[test]
    fn latch_is_permanent() {
        let latch = RevocationSignal::default();
        assert!(!latch.is_revoked());
        latch.revoke();
        assert!(latch.is_revoked());
        latch.revoke();
        assert!(latch.is_revoked());
    }

    #[test]
    fn initial_mib_notification_does_not_revoke() {
        assert!(!is_network_change(MIB_INITIAL_NOTIFICATION));
        assert!(is_network_change(0));
        assert!(is_network_change(1));
        assert!(is_network_change(2));
    }

    fn action(source: u32, code: u32) -> WifiEventAction {
        selected_wifi_event_action(true, source, code)
    }

    #[test]
    fn leaving_the_network_revokes_at_once() {
        // ACM: connection start/complete/fail, interface arrival/removal, network (un)available,
        // disconnecting, disconnected, operational state change.
        for code in [9, 10, 11, 13, 14, 18, 19, 20, 21, 27] {
            assert_eq!(
                action(WLAN_SOURCE_ACM, code),
                WifiEventAction::Revoke,
                "ACM {code}"
            );
        }
        // MSM: radio state change, disassociating, disconnected, adapter removal, mode change.
        for code in [7, 9, 10, 13, 14] {
            assert_eq!(
                action(WLAN_SOURCE_MSM, code),
                WifiEventAction::Revoke,
                "MSM {code}"
            );
        }
    }

    #[test]
    fn only_a_finished_roam_rechecks_and_its_steps_wait_for_it() {
        assert_eq!(action(WLAN_SOURCE_MSM, 6), WifiEventAction::Recheck);
        // Associating, associated, authenticating, connected and roaming start read mid-roam state.
        for code in [1, 2, 3, 4, 5] {
            assert_eq!(
                action(WLAN_SOURCE_MSM, code),
                WifiEventAction::Ignore,
                "MSM {code}"
            );
        }
    }

    #[test]
    fn scans_link_quality_and_other_adapters_are_ignored() {
        for code in [7, 8, 15, 26] {
            assert_eq!(
                action(WLAN_SOURCE_ACM, code),
                WifiEventAction::Ignore,
                "ACM {code}"
            );
        }
        for code in [8, 11, 15, 16] {
            assert_eq!(
                action(WLAN_SOURCE_MSM, code),
                WifiEventAction::Ignore,
                "MSM {code}"
            );
        }
        assert_eq!(action(0, 21), WifiEventAction::Ignore);
        assert_eq!(
            selected_wifi_event_action(false, WLAN_SOURCE_MSM, 10),
            WifiEventAction::Ignore
        );
        assert_eq!(
            selected_wifi_event_action(false, WLAN_SOURCE_MSM, 6),
            WifiEventAction::Ignore
        );
    }

    const RECHECK_LIMIT: Duration = Duration::from_secs(5);

    fn counted_check(result: bool) -> (Arc<PinnedCheck>, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let check: PinnedCheck = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            result
        });
        (Arc::new(check), runs)
    }

    #[test]
    fn a_failed_recheck_revokes() {
        let latch = RevocationSignal::default();
        let (check, runs) = counted_check(false);
        let worker = RecheckWorker::start(latch.clone(), check).unwrap();
        worker.request();
        drop(worker);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(latch.is_revoked());
    }

    #[test]
    fn a_passing_recheck_keeps_the_session() {
        let latch = RevocationSignal::default();
        let (check, runs) = counted_check(true);
        let worker = RecheckWorker::start(latch.clone(), check).unwrap();
        worker.request();
        drop(worker);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(!latch.is_revoked());
    }

    #[test]
    fn a_revoked_session_is_not_rechecked() {
        let latch = RevocationSignal::default();
        latch.revoke();
        let (check, runs) = counted_check(false);
        let worker = RecheckWorker::start(latch.clone(), check).unwrap();
        worker.request();
        drop(worker);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_request_the_worker_can_no_longer_take_reports_it() {
        let (check, runs) = counted_check(true);
        let mut worker = RecheckWorker::start(RevocationSignal::default(), check).unwrap();
        worker.requests.send(Recheck::Stop).unwrap();
        worker.thread.take().unwrap().join().unwrap();
        assert!(!worker.request());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn requests_return_at_once_and_coalesce_while_a_recheck_runs_elsewhere() {
        let latch = RevocationSignal::default();
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let check: PinnedCheck = Box::new(move || {
            entered.send(thread::current().id()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(RECHECK_LIMIT)
                .is_ok()
        });
        let worker = RecheckWorker::start(latch.clone(), Arc::new(check)).unwrap();

        worker.request();
        let checker = entered_rx.recv_timeout(RECHECK_LIMIT).unwrap();
        assert_ne!(checker, thread::current().id());
        // The first check is blocked, so these must neither wait for it nor queue more than one run.
        for _ in 0..5 {
            assert!(worker.request());
        }
        release.send(()).unwrap();
        assert_eq!(entered_rx.recv_timeout(RECHECK_LIMIT).unwrap(), checker);
        release.send(()).unwrap();
        drop(worker);

        assert!(entered_rx.try_recv().is_err());
        assert!(!latch.is_revoked());
    }

    #[test]
    fn only_a_deletion_of_the_selected_row_skips_the_check() {
        assert!(selected_row_deleted(MIB_DELETE_INSTANCE, true));
        assert!(!selected_row_deleted(MIB_DELETE_INSTANCE, false));
        assert!(!selected_row_deleted(1, true));
        assert!(!selected_row_deleted(0, true));
    }

    #[test]
    fn a_check_that_fails_or_panics_reads_as_changed() {
        let unchanged: PinnedCheck = Box::new(|| true);
        let changed: PinnedCheck = Box::new(|| false);
        let panicking: PinnedCheck = Box::new(|| -> bool { panic!("check failed") });
        assert!(pinned_facts_hold(&unchanged));
        assert!(!pinned_facts_hold(&changed));
        assert!(!pinned_facts_hold(&panicking));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "explicit native observer lifecycle check with MONHOP_TEST_INTERFACE_INDEX"]
    fn native_selected_interface_start_stop_is_bounded() {
        let index = std::env::var("MONHOP_TEST_INTERFACE_INDEX")
            .expect("explicit selected interface index required")
            .parse::<u32>()
            .expect("numeric interface index required");
        assert_ne!(index, 0);
        let adapter = crate::network::enumerate_adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| adapter.index == index && adapter.physical && adapter.up)
            .expect("selected physical interface must be present and up");
        let started = std::time::Instant::now();
        for _ in 0..8 {
            let watcher = NetworkChangeWatch::start(&adapter, Box::new(|| true)).unwrap();
            let signal = watcher.revocation_signal();
            drop(watcher);
            assert!(signal.is_revoked());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
