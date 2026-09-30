//! The autoscroll origin marker: a small ring with a center dot in its own click-through window
//! above normal windows on every Space. Everything here runs on the main thread, which is why each
//! entry point takes a `MainThreadMarker`.

use std::cell::RefCell;

use block2::RcBlock;
use monhop_core::Point;
use monhop_transport::session_native::{AutoscrollMarker, set_autoscroll_marker};
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSBox, NSBoxType, NSColor, NSScreen, NSStatusWindowLevel, NSTitlePosition, NSView, NSWindow,
    NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tauri::AppHandle;

use crate::dimming::macos::{close_all, fade, overlay_window};

/// The ring's outer diameter, in points.
const DIAMETER: f64 = 28.0;
const RING_WIDTH: f64 = 2.0;
const DOT_DIAMETER: f64 = 6.0;
/// A white ring and dot over a translucent dark disc read on light and dark content alike.
const INK_WHITE: f64 = 1.0;
const INK_ALPHA: f64 = 0.9;
const DISC_WHITE: f64 = 0.0;
const DISC_ALPHA: f64 = 0.35;

thread_local! {
    static MARKER: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

/// Hands the transport a sink that queues each change onto the main thread and never waits.
pub fn register(app: &AppHandle) {
    let app = app.clone();
    let registered = set_autoscroll_marker(move |marker| {
        let _ = app.run_on_main_thread(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                apply(mtm, marker);
            }
        });
    });
    if !registered {
        log::warn!("autoscroll: marker already registered");
    }
}

fn apply(mtm: MainThreadMarker, marker: AutoscrollMarker) {
    match marker {
        AutoscrollMarker::Show(origin) => show(mtm, origin),
        AutoscrollMarker::Hide => hide(mtm),
    }
}

/// Fades a fresh marker in at `origin`, given in the top-left global points input uses. A marker
/// still fading out stays where it was, so nothing jumps.
fn show(mtm: MainThreadMarker, origin: Point) {
    hide(mtm);
    let Some(primary) = NSScreen::screens(mtm).iter().next() else {
        return;
    };
    let bottom = primary.frame().size.height - origin.y - DIAMETER / 2.0;
    let frame = NSRect::new(
        NSPoint::new(origin.x - DIAMETER / 2.0, bottom),
        NSSize::new(DIAMETER, DIAMETER),
    );
    let window = overlay_window(mtm, frame, NSStatusWindowLevel);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setContentView(Some(&drawing(mtm)));
    window.orderFrontRegardless();
    if reduce_motion() {
        window.setAlphaValue(1.0);
    } else {
        fade(std::slice::from_ref(&window), 1.0, None);
    }
    MARKER.with(|marker| *marker.borrow_mut() = Some(window));
}

fn hide(mtm: MainThreadMarker) {
    let _ = mtm;
    let Some(window) = MARKER.with(|marker| marker.borrow_mut().take()) else {
        return;
    };
    if reduce_motion() {
        close_all(vec![window]);
        return;
    }
    let closing = window.clone();
    fade(
        &[window],
        0.0,
        Some(RcBlock::new(move || close_all(vec![closing.clone()]))),
    );
}

fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn drawing(mtm: MainThreadMarker) -> Retained<NSView> {
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DIAMETER, DIAMETER));
    let view = NSView::initWithFrame(mtm.alloc(), bounds);
    let ink = NSColor::colorWithWhite_alpha(INK_WHITE, INK_ALPHA);
    let disc = NSColor::colorWithWhite_alpha(DISC_WHITE, DISC_ALPHA);
    view.addSubview(&circle(mtm, bounds, RING_WIDTH, &disc, &ink));
    let inset = (DIAMETER - DOT_DIAMETER) / 2.0;
    let dot = NSRect::new(
        NSPoint::new(inset, inset),
        NSSize::new(DOT_DIAMETER, DOT_DIAMETER),
    );
    view.addSubview(&circle(mtm, dot, 0.0, &ink, &ink));
    view
}

fn circle(
    mtm: MainThreadMarker,
    frame: NSRect,
    border: f64,
    fill: &NSColor,
    stroke: &NSColor,
) -> Retained<NSBox> {
    let circle = NSBox::initWithFrame(mtm.alloc(), frame);
    circle.setBoxType(NSBoxType::Custom);
    circle.setTitlePosition(NSTitlePosition::NoTitle);
    circle.setBorderWidth(border);
    circle.setCornerRadius(frame.size.width / 2.0);
    circle.setFillColor(fill);
    circle.setBorderColor(stroke);
    circle
}
