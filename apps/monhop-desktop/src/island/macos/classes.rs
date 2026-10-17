//! The island's AppKit subclasses: a panel that never takes focus, the view that draws, hovers and
//! hit-tests the silhouette, the accessibility element each control exposes, and the screen-lock
//! observer.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
    rc::Retained, runtime::AnyObject,
};
use objc2_app_kit::{
    NSAccessibilityElement, NSAccessibilityUnknownSubrole, NSBackingStoreType, NSEvent, NSPanel,
    NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::CGPath;
use objc2_foundation::{NSArray, NSCopying, NSNotification, NSObject, NSPoint, NSRect, NSString};
use objc2_quartz_core::CAShapeLayer;

use super::Pointer;

define_class!(
    // SAFETY: NSPanel has no subclassing requirements and this class never implements Drop.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MonHopIslandPanel"]
    pub struct IslandPanel;

    impl IslandPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }

        /// Undescribed, so window managers that track dialogs leave it alone.
        #[unsafe(method_id(accessibilitySubrole))]
        fn accessibility_subrole(&self) -> Option<Retained<NSString>> {
            // SAFETY: AppKit's exported subrole constant.
            Some(unsafe { NSAccessibilityUnknownSubrole }.copy())
        }
    }
);

impl IslandPanel {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: NSPanel's designated initializer with a global-space frame, on the main thread.
        unsafe {
            msg_send![
                this,
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        }
    }
}

/// What the view needs without reaching the island's state: the layer that answers hit tests
/// and the hover rectangle, so AppKit's callbacks never re-enter that state.
#[derive(Default)]
pub struct ViewIvars {
    fill: OnceCell<Retained<CAShapeLayer>>,
    hover_rect: Cell<NSRect>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    children: RefCell<Vec<Retained<ControlElement>>>,
    /// Its island ended: while it fades out it passes every event through and reports none.
    detached: Cell<bool>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements and this class never implements Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MonHopIslandView"]
    #[ivars = ViewIvars]
    pub struct IslandView;

    impl IslandView {
        /// A click acts at once without first activating MonHop.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            false
        }

        /// Outside the silhouette the view is not there, so the window server passes the click
        /// to whatever is below.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            // SAFETY: the superview is the panel's frame view, alive while this view is in it.
            let superview = unsafe { self.superview() };
            let local = self.convertPoint_fromView(point, superview.as_deref());
            (!self.ivars().detached.get() && self.contains(local))
                .then(|| self.retain().into_super())
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            // SAFETY: the superclass implementation takes no arguments.
            let () = unsafe { msg_send![super(self), updateTrackingAreas] };
            self.install_tracking();
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.forward(Pointer::Entered);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.forward(Pointer::Exited);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.forward(Pointer::Moved(self.island_point(event)));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.forward(Pointer::Down(self.island_point(event)));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.forward(Pointer::Dragged(self.island_point(event)));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.forward(Pointer::Up(self.island_point(event)));
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Option<Retained<NSArray>> {
            let children = self.ivars().children.borrow();
            let objects: Vec<&AnyObject> = children.iter().map(|child| -> &AnyObject { child }).collect();
            Some(NSArray::from_slice(&objects))
        }
    }
);

impl IslandView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        fill: Retained<CAShapeLayer>,
    ) -> Retained<Self> {
        let ivars = ViewIvars::default();
        let _ = ivars.fill.set(fill);
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSView's designated initializer with a frame.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// The island hangs from the view's top center.
    pub fn origin(&self) -> NSPoint {
        let bounds = self.bounds();
        NSPoint::new(bounds.size.width / 2.0, bounds.size.height)
    }

    /// Converts a view point to the island's coordinates (origin at its top center, y up).
    pub fn to_island(&self, point: NSPoint) -> NSPoint {
        let origin = self.origin();
        NSPoint::new(point.x - origin.x, point.y - origin.y)
    }

    fn island_point(&self, event: &NSEvent) -> NSPoint {
        let local = self.convertPoint_fromView(event.locationInWindow(), None);
        self.to_island(local)
    }

    /// A drag that began before the island ended still reaches its old view; it goes nowhere.
    fn forward(&self, event: Pointer) {
        if !self.ivars().detached.get() {
            super::pointer(event);
        }
    }

    /// Cuts the view off from the island for good: no hover area, hits or accessibility children.
    pub fn detach(&self) {
        self.ivars().detached.set(true);
        self.set_hover_rect(NSRect::ZERO);
        self.set_accessibility_children(Vec::new());
    }

    /// Inside the silhouette as it shows on screen now.
    pub fn contains(&self, local: NSPoint) -> bool {
        let Some(fill) = self.ivars().fill.get() else {
            return false;
        };
        let island = self.to_island(local);
        // SAFETY: the presentation layer is a read-only copy of the fill layer.
        let shown = unsafe { fill.presentationLayer() }
            .and_then(|layer| layer.downcast::<CAShapeLayer>().ok())
            .and_then(|layer| layer.path())
            .or_else(|| fill.path());
        shown.is_some_and(|path| {
            // SAFETY: a null transform tests the point as given.
            unsafe { CGPath::contains_point(Some(&path), std::ptr::null(), island, false) }
        })
    }

    /// The hover area follows the island's target outline.
    pub fn set_hover_rect(&self, rect: NSRect) {
        if self.ivars().hover_rect.get() == rect {
            return;
        }
        self.ivars().hover_rect.set(rect);
        self.install_tracking();
    }

    /// One tracking area, always active, so hover works while another app is in front.
    fn install_tracking(&self) {
        if let Some(old) = self.ivars().tracking.borrow_mut().take() {
            self.removeTrackingArea(&old);
        }
        let rect = self.ivars().hover_rect.get();
        if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            return;
        }
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::EnabledDuringMouseDrag;
        let owner: &AnyObject = self;
        // SAFETY: the view owns the area and removes it before replacing it.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                rect,
                options,
                Some(owner),
                None,
            )
        };
        self.addTrackingArea(&area);
        *self.ivars().tracking.borrow_mut() = Some(area);
    }

    pub fn set_accessibility_children(&self, children: Vec<Retained<ControlElement>>) {
        *self.ivars().children.borrow_mut() = children;
    }
}

/// What an accessibility element asks of the island.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assist {
    Press,
    Increment,
    Decrement,
}

#[derive(Default)]
pub struct ElementIvars {
    control: Cell<usize>,
}

define_class!(
    // SAFETY: NSAccessibilityElement has no subclassing requirements and this class never
    // implements Drop.
    #[unsafe(super(NSAccessibilityElement, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MonHopIslandControl"]
    #[ivars = ElementIvars]
    pub struct ControlElement;

    impl ControlElement {
        #[unsafe(method(accessibilityPerformPress))]
        fn perform_press(&self) -> bool {
            super::assist(self, Assist::Press)
        }

        #[unsafe(method(accessibilityPerformIncrement))]
        fn perform_increment(&self) -> bool {
            super::assist(self, Assist::Increment)
        }

        #[unsafe(method(accessibilityPerformDecrement))]
        fn perform_decrement(&self) -> bool {
            super::assist(self, Assist::Decrement)
        }
    }
);

impl ControlElement {
    pub fn new(mtm: MainThreadMarker, control: usize) -> Retained<Self> {
        let ivars = ElementIvars::default();
        ivars.control.set(control);
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's init method has the declared signature.
        unsafe { msg_send![super(this), init] }
    }

    /// The index of its control among the open island's controls.
    pub fn control(&self) -> usize {
        self.ivars().control.get()
    }

    /// Rows came or went, so its control now has another place among the controls.
    pub fn set_control(&self, control: usize) {
        self.ivars().control.set(control);
    }
}

pub struct LockIvars {
    generation: u64,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and this class never implements Drop.
    #[unsafe(super(NSObject))]
    #[name = "MonHopIslandLockObserver"]
    #[ivars = LockIvars]
    pub struct LockObserver;

    impl LockObserver {
        #[unsafe(method(screenLocked:))]
        fn screen_locked(&self, _notification: &NSNotification) {
            super::screen_locked(self.ivars().generation, true);
        }

        #[unsafe(method(screenUnlocked:))]
        fn screen_unlocked(&self, _notification: &NSNotification) {
            super::screen_locked(self.ivars().generation, false);
        }
    }
);

impl LockObserver {
    pub fn new(generation: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(LockIvars { generation });
        // SAFETY: NSObject's init method has the declared signature.
        unsafe { msg_send![super(this), init] }
    }
}
