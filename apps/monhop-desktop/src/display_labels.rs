//! AppKit labels are refreshed on the main thread, never from the input worker.

use std::ptr::NonNull;

use block2::RcBlock;
use monhop_core::DisplayId;
use monhop_platform_macos::{display_names, enumerate_active_displays};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplicationDidChangeScreenParametersNotification, NSScreen};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNumber, NSOperationQueue, NSString,
};

/// Labels follow every arrangement change at once, so a display that just arrived is named
/// before the link or a session reads it, not when the window next asks.
pub fn watch(marker: MainThreadMarker) {
    let _ = refresh(marker);
    let block = RcBlock::new(|_: NonNull<NSNotification>| {
        if let Some(marker) = MainThreadMarker::new() {
            let _ = refresh(marker);
        }
    });
    // SAFETY: the observer runs on the main queue and touches only main-thread state.
    let observer = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationDidChangeScreenParametersNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };
    // Observes for the app's whole life.
    std::mem::forget(observer);
}

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
