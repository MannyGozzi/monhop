//! Login registration through ServiceManagement's `SMAppService`, for the bundled LaunchAgent
//! plist. Runs off the main thread; the API is not AppKit and carries no thread requirement.

use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use super::{PLIST_NAME, Registration, Status};

fn service() -> objc2::rc::Retained<SMAppService> {
    let name = NSString::from_str(PLIST_NAME);
    // SAFETY: the plist name is a fixed string naming the agent bundled at
    // Contents/Library/LaunchAgents; the call returns a new service handle, no aliasing.
    unsafe { SMAppService::agentServiceWithPlistName(&name) }
}

fn status_from(raw: SMAppServiceStatus) -> Status {
    match raw {
        SMAppServiceStatus::Enabled => Status::Enabled,
        SMAppServiceStatus::RequiresApproval => Status::RequiresApproval,
        SMAppServiceStatus::NotFound => Status::NotFound,
        _ => Status::NotRegistered,
    }
}

fn describe(error: objc2::rc::Retained<objc2_foundation::NSError>) -> String {
    log::warn!("autostart: registration failed (code {})", error.code());
    error.localizedDescription().to_string()
}

pub struct MacRegistration;

impl Registration for MacRegistration {
    fn status(&self) -> Result<Status, String> {
        // SAFETY: `status` takes no arguments and reads a plain status value.
        Ok(status_from(unsafe { service().status() }))
    }

    fn register(&self) -> Result<(), String> {
        // SAFETY: `registerAndReturnError:` is a self-contained registration call.
        unsafe { service().registerAndReturnError() }.map_err(describe)
    }

    fn unregister(&self) -> Result<(), String> {
        // SAFETY: `unregisterAndReturnError:` is a self-contained call.
        unsafe { service().unregisterAndReturnError() }.map_err(describe)
    }
}

/// Opens System Settings > General > Login Items. Always succeeds; macOS reports no failure here.
pub fn open_settings() -> Result<(), String> {
    // SAFETY: takes no arguments and returns nothing.
    unsafe { SMAppService::openSystemSettingsLoginItems() };
    Ok(())
}
