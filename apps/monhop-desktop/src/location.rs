use std::cell::{Cell, RefCell};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::NSApplication;
use objc2_core_location::{CLAuthorizationStatus, CLLocationManager, CLLocationManagerDelegate};
use objc2_foundation::{NSObject, NSObjectProtocol};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WifiAuthorization {
    NotDetermined,
    Denied,
    Restricted,
    Authorized,
    ServicesDisabled,
    Unknown,
}

impl WifiAuthorization {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotDetermined => "not-determined",
            Self::Denied => "denied",
            Self::Restricted => "restricted",
            Self::Authorized => "authorized",
            Self::ServicesDisabled => "services-disabled",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestAction {
    Schedule,
    AlreadyPending,
    ReturnCurrent,
}

#[derive(Default)]
struct AuthorizationDelegateIvars {
    request_pending: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class never implements Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AuthorizationDelegateIvars]
    struct AuthorizationDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for AuthorizationDelegate {}

    // SAFETY: CLLocationManagerDelegate has no safety requirements.
    unsafe impl CLLocationManagerDelegate for AuthorizationDelegate {
        #[allow(non_snake_case)]
        #[unsafe(method(locationManagerDidChangeAuthorization:))]
        fn locationManagerDidChangeAuthorization(&self, manager: &CLLocationManager) {
            self.ivars().request_pending.set(pending_after_callback(
                self.ivars().request_pending.get(),
                authorization_from_manager(manager),
            ));
        }
    }
);

impl AuthorizationDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AuthorizationDelegateIvars::default());
        // SAFETY: NSObject's init method has the declared signature.
        unsafe { msg_send![super(this), init] }
    }
}

struct LocationState {
    manager: Retained<CLLocationManager>,
    delegate: Retained<AuthorizationDelegate>,
}

impl LocationState {
    fn new(mtm: MainThreadMarker) -> Self {
        // SAFETY: This main-thread-only flow creates the documented CoreLocation manager.
        let manager = unsafe { CLLocationManager::new() };
        let delegate = AuthorizationDelegate::new(mtm);
        // SAFETY: LocationState retains the delegate for at least as long as the manager holds it weakly.
        unsafe { manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        Self { manager, delegate }
    }
}

thread_local! {
    static LOCATION: RefCell<Option<LocationState>> = const { RefCell::new(None) };
}

pub fn read(_mtm: MainThreadMarker) -> Result<WifiAuthorization, String> {
    // SAFETY: This explicit main-thread read only creates a manager and reads its category state.
    let manager = unsafe { CLLocationManager::new() };
    Ok(authorization_from_manager(&manager))
}

pub fn request(mtm: MainThreadMarker) -> Result<WifiAuthorization, String> {
    if !NSApplication::sharedApplication(mtm).isActive() {
        return Err("Bring MonHop to the foreground, then request Wi-Fi permission again.".into());
    }

    LOCATION.with(|location| {
        let mut location = location.borrow_mut();
        let state = location.get_or_insert_with(|| LocationState::new(mtm));
        let authorization = authorization_from_manager(&state.manager);

        match request_action(authorization, state.delegate.ivars().request_pending.get()) {
            RequestAction::Schedule => {
                state.delegate.ivars().request_pending.set(true);
                // SAFETY: The explicit foreground user request is dispatched on the main thread.
                unsafe { state.manager.requestWhenInUseAuthorization() };
            }
            RequestAction::AlreadyPending | RequestAction::ReturnCurrent => {}
        }

        Ok(authorization)
    })
}

fn authorization_from_manager(manager: &CLLocationManager) -> WifiAuthorization {
    // SAFETY: These CoreLocation methods only report service and authorization categories.
    let services_enabled = unsafe { CLLocationManager::locationServicesEnabled_class() };
    // SAFETY: This manager was created for the current main-thread invocation.
    let authorization = unsafe { manager.authorizationStatus() };
    authorization_from_raw(services_enabled, authorization.0)
}

const NOT_DETERMINED: i32 = CLAuthorizationStatus::NotDetermined.0;
const RESTRICTED: i32 = CLAuthorizationStatus::Restricted.0;
const DENIED: i32 = CLAuthorizationStatus::Denied.0;
const AUTHORIZED_ALWAYS: i32 = CLAuthorizationStatus::AuthorizedAlways.0;
const AUTHORIZED_WHEN_IN_USE: i32 = CLAuthorizationStatus::AuthorizedWhenInUse.0;

const fn authorization_from_raw(services_enabled: bool, authorization: i32) -> WifiAuthorization {
    if !services_enabled {
        return WifiAuthorization::ServicesDisabled;
    }

    match authorization {
        NOT_DETERMINED => WifiAuthorization::NotDetermined,
        DENIED => WifiAuthorization::Denied,
        RESTRICTED => WifiAuthorization::Restricted,
        AUTHORIZED_ALWAYS | AUTHORIZED_WHEN_IN_USE => WifiAuthorization::Authorized,
        _ => WifiAuthorization::Unknown,
    }
}

const fn request_action(authorization: WifiAuthorization, request_pending: bool) -> RequestAction {
    match authorization {
        WifiAuthorization::NotDetermined if !request_pending => RequestAction::Schedule,
        WifiAuthorization::NotDetermined => RequestAction::AlreadyPending,
        WifiAuthorization::Denied
        | WifiAuthorization::Restricted
        | WifiAuthorization::Authorized
        | WifiAuthorization::ServicesDisabled
        | WifiAuthorization::Unknown => RequestAction::ReturnCurrent,
    }
}

const fn pending_after_callback(request_pending: bool, authorization: WifiAuthorization) -> bool {
    request_pending
        && matches!(
            authorization,
            WifiAuthorization::NotDetermined | WifiAuthorization::Unknown
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_supported_authorization_category() {
        assert_eq!(
            authorization_from_raw(true, NOT_DETERMINED),
            WifiAuthorization::NotDetermined
        );
        assert_eq!(
            authorization_from_raw(true, DENIED),
            WifiAuthorization::Denied
        );
        assert_eq!(
            authorization_from_raw(true, RESTRICTED),
            WifiAuthorization::Restricted
        );
        assert_eq!(
            authorization_from_raw(true, AUTHORIZED_ALWAYS),
            WifiAuthorization::Authorized
        );
        assert_eq!(
            authorization_from_raw(true, AUTHORIZED_WHEN_IN_USE),
            WifiAuthorization::Authorized
        );
        assert_eq!(authorization_from_raw(true, 99), WifiAuthorization::Unknown);
        assert_eq!(
            authorization_from_raw(false, AUTHORIZED_WHEN_IN_USE),
            WifiAuthorization::ServicesDisabled
        );
    }

    #[test]
    fn status_serializes_as_a_category_only() {
        assert_eq!(
            serde_json::to_string(&WifiAuthorization::ServicesDisabled).unwrap(),
            "\"services-disabled\""
        );
        assert_eq!(WifiAuthorization::Unknown.as_str(), "unknown");
    }

    #[test]
    fn schedules_one_request_then_deduplicates() {
        assert_eq!(
            request_action(WifiAuthorization::NotDetermined, false),
            RequestAction::Schedule
        );
        assert_eq!(
            request_action(WifiAuthorization::NotDetermined, true),
            RequestAction::AlreadyPending
        );
        assert_eq!(
            request_action(WifiAuthorization::Unknown, false),
            RequestAction::ReturnCurrent
        );
    }

    #[test]
    fn initial_and_unknown_callbacks_do_not_complete_a_pending_request() {
        assert!(pending_after_callback(
            true,
            WifiAuthorization::NotDetermined
        ));
        assert!(pending_after_callback(true, WifiAuthorization::Unknown));
        assert!(!pending_after_callback(true, WifiAuthorization::Authorized));
        assert!(!pending_after_callback(true, WifiAuthorization::Denied));
    }
}
