//! The island's AppKit subclasses: a panel that never takes focus, and the screen-lock observer.

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
};
use objc2_app_kit::{
    NSAccessibilityUnknownSubrole, NSBackingStoreType, NSPanel, NSResponder, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSCopying, NSNotification, NSObject, NSRect, NSString};

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
