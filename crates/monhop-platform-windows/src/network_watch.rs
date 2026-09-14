//! Explicit, fail-closed network-change revocation for one native session.
//!
//! Every IP Helper notice re-reads the selected adapter through the caller's check; only a changed,
//! missing, or unreadable adapter revokes. Two notices revoke without the check: a deletion of the
//! selected interface or its pinned address, which is final even if a later add restores the same
//! values before the check runs, and an attachment-relevant Wi-Fi event on the selected adapter,
//! because the check queries WLAN state and a WLAN callback must not.

#[cfg(any(windows, test))]
use monhop_core::RevocationSignal;
#[cfg(any(windows, test))]
use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg(windows)]
use std::{ffi::c_void, io, net::Ipv4Addr, ptr};

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

/// Re-reads the selected adapter after an IP Helper notice; `false` revokes. It runs on an IP Helper
/// notification thread, never inside a WLAN callback, so it may query WLAN state.
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

#[cfg(any(windows, test))]
fn is_network_change(notification_type: i32) -> bool {
    notification_type != MIB_INITIAL_NOTIFICATION
}

/// Only attachment-relevant Wi-Fi events revoke. In particular, scan completion is not a change.
#[cfg(any(windows, test))]
fn wifi_event_revokes(source: u32, code: u32) -> bool {
    match source {
        WLAN_SOURCE_ACM => matches!(
            code,
            9  // connection_start
                | 10 // connection_complete
                | 11 // connection_attempt_fail
                | 13 // interface_arrival
                | 14 // interface_removal
                | 18 // network_not_available
                | 19 // network_available
                | 20 // disconnecting
                | 21 // disconnected
                | 27 // operational_state_change
        ),
        WLAN_SOURCE_MSM => matches!(
            code,
            1  // associating
                | 2  // associated
                | 3  // authenticating
                | 4  // connected
                | 5  // roaming_start
                | 6  // roaming_end
                | 7  // radio_state_change
                | 9  // disassociating
                | 10 // disconnected
                | 13 // adapter_removal
                | 14 // adapter_operation_mode_change
        ),
        _ => false,
    }
}

#[cfg(any(windows, test))]
fn selected_wifi_event_revokes(interface_matches: bool, source: u32, code: u32) -> bool {
    interface_matches && wifi_event_revokes(source, code)
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
    still_pinned: PinnedCheck,
}

#[cfg(windows)]
impl CallbackContext {
    fn lost(&self, what: &str) {
        log::warn!("network watch: {what}; revoked");
        self.latch.revoke();
    }

    /// A notice revokes only when the pinned adapter no longer reads back as it was selected.
    fn observe(&self, change: &str) {
        if self.latch.is_revoked() {
            return;
        }
        if pinned_facts_hold(&self.still_pinned) {
            log::debug!(
                "network watch: {change}; the selected adapter is unchanged, session continues"
            );
        } else {
            log::warn!("network watch: {change} and the selected adapter differs; revoked");
            self.latch.revoke();
        }
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
        let context = Box::new(CallbackContext {
            latch: signal.clone(),
            wifi_identity: initial.guid,
            wifi: initial.wifi,
            index: selected.index,
            address: selected.address,
            still_pinned,
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
    if selected_wifi_event_revokes(
        context.wifi && guid_bytes(&notification.InterfaceGuid) == context.wifi_identity,
        notification.NotificationSource,
        notification.NotificationCode,
    ) {
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

    #[test]
    fn wifi_attachment_events_revoke_but_scans_do_not() {
        assert!(wifi_event_revokes(WLAN_SOURCE_ACM, 21));
        assert!(wifi_event_revokes(WLAN_SOURCE_MSM, 5));
        assert!(wifi_event_revokes(WLAN_SOURCE_MSM, 6));
        assert!(!wifi_event_revokes(WLAN_SOURCE_ACM, 7));
        assert!(!wifi_event_revokes(WLAN_SOURCE_ACM, 8));
        assert!(!wifi_event_revokes(WLAN_SOURCE_ACM, 26));
        assert!(!wifi_event_revokes(WLAN_SOURCE_MSM, 8));
        assert!(!selected_wifi_event_revokes(false, WLAN_SOURCE_MSM, 5));
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

    #[test]
    fn unrelated_wifi_events_do_not_revoke() {
        assert!(!wifi_event_revokes(0, 21));
        assert!(!wifi_event_revokes(WLAN_SOURCE_ACM, 15));
        assert!(!wifi_event_revokes(WLAN_SOURCE_MSM, 11));
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
