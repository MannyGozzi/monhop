//! AppKit labels are refreshed on the main thread, never from the input worker.

use monhop_core::DisplayId;
use monhop_platform_macos::{display_names, enumerate_active_displays};
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSNumber, NSString};

pub fn refresh(marker: MainThreadMarker) -> Result<(), String> {
    let displays =
        enumerate_active_displays().map_err(|_| "Could not read the current displays.")?;
    let key = NSString::from_str("NSScreenNumber");
    let mut names = Vec::new();
    for screen in NSScreen::screens(marker) {
        let description = screen.deviceDescription();
        let Some(number) = description.objectForKey(&key) else {
            continue;
        };
        let Some(number) = number.downcast_ref::<NSNumber>() else {
            continue;
        };
        names.push((
            DisplayId(u64::from(number.unsignedIntValue())),
            screen.localizedName().to_string(),
        ));
    }
    display_names::replace_names(&displays, names);
    Ok(())
}
