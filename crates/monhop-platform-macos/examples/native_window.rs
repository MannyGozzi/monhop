//! Explicit self-process-only macOS input delivery diagnostic.
//!
//! Explicit `--run` creates a short-lived AppKit window and posts only to itself.
//! Explicit `--capture` opens a ten-second count-only physical input window.
//! Both consume input before AppKit shortcut dispatch; default startup is inert.

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("native_window status=UNSUPPORTED platform=non_macos");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::main()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{OsStr, OsString, c_char, c_void};
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use monhop_core::{HidUsage, MouseButton, Point, capture::SINGLE_CLICK};
    use monhop_platform_macos::{
        MacError, MacInjector, PassiveDiagnosticCounts, SYNTHETIC_EVENT_MARKER,
        run_passive_diagnostic_with_cancel,
    };

    type Id = *mut c_void;
    type Sel = *const c_void;
    // Apple arm64 defines BOOL as C bool; Intel macOS keeps signed-char BOOL.
    #[cfg(target_arch = "x86_64")]
    type ObjcBool = i8;
    #[cfg(not(target_arch = "x86_64"))]
    type ObjcBool = bool;
    #[cfg(target_arch = "x86_64")]
    const OBJC_YES: ObjcBool = 1;
    #[cfg(target_arch = "x86_64")]
    const OBJC_NO: ObjcBool = 0;
    #[cfg(not(target_arch = "x86_64"))]
    const OBJC_YES: ObjcBool = true;
    #[cfg(not(target_arch = "x86_64"))]
    const OBJC_NO: ObjcBool = false;
    type CgEventRef = *const c_void;
    type CgEventField = u32;

    #[cfg(target_arch = "x86_64")]
    const fn objc_bool_is_yes(value: ObjcBool) -> bool {
        value != 0
    }

    #[cfg(not(target_arch = "x86_64"))]
    const fn objc_bool_is_yes(value: ObjcBool) -> bool {
        value
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        origin: NsPoint,
        size: NsSize,
    }

    const MAX_SMOKE_DURATION: Duration = Duration::from_secs(14);
    const FOCUS_WAIT: Duration = Duration::from_secs(1);
    const EVENT_IDLE_WAIT: Duration = Duration::from_millis(75);
    const EVENT_POLL_SLICE: Duration = Duration::from_millis(10);

    const NS_APPLICATION_ACTIVATION_POLICY_REGULAR: isize = 0;
    const NS_BACKING_STORE_BUFFERED: usize = 2;
    const NS_WINDOW_STYLE_TITLED: usize = 1 << 0;
    const NS_WINDOW_STYLE_CLOSABLE: usize = 1 << 1;
    const NS_WINDOW_STYLE_MINIATURIZABLE: usize = 1 << 2;
    const NS_WINDOW_STYLE: usize =
        NS_WINDOW_STYLE_TITLED | NS_WINDOW_STYLE_CLOSABLE | NS_WINDOW_STYLE_MINIATURIZABLE;

    const NS_EVENT_MASK_ANY: u64 = usize::MAX as u64;
    const NS_EVENT_LEFT_MOUSE_DOWN: usize = 1;
    const NS_EVENT_LEFT_MOUSE_UP: usize = 2;
    const NS_EVENT_RIGHT_MOUSE_DOWN: usize = 3;
    const NS_EVENT_RIGHT_MOUSE_UP: usize = 4;
    const NS_EVENT_MOUSE_MOVED: usize = 5;
    const NS_EVENT_LEFT_MOUSE_DRAGGED: usize = 6;
    const NS_EVENT_RIGHT_MOUSE_DRAGGED: usize = 7;
    #[cfg(test)]
    const NS_EVENT_MOUSE_ENTERED: usize = 8;
    #[cfg(test)]
    const NS_EVENT_MOUSE_EXITED: usize = 9;
    const NS_EVENT_KEY_DOWN: usize = 10;
    const NS_EVENT_KEY_UP: usize = 11;
    const NS_EVENT_FLAGS_CHANGED: usize = 12;
    const NS_EVENT_SCROLL_WHEEL: usize = 22;
    const NS_EVENT_OTHER_MOUSE_DOWN: usize = 25;
    const NS_EVENT_OTHER_MOUSE_UP: usize = 26;
    const NS_EVENT_OTHER_MOUSE_DRAGGED: usize = 27;

    const NS_EVENT_MODIFIER_SHIFT: usize = 1 << 17;
    const NS_EVENT_MODIFIER_CONTROL: usize = 1 << 18;
    const NS_EVENT_MODIFIER_OPTION: usize = 1 << 19;
    const NS_EVENT_MODIFIER_COMMAND: usize = 1 << 20;

    const CG_EVENT_SOURCE_USER_DATA: CgEventField = 42;

    const LEFT_SHIFT_USAGE: HidUsage = HidUsage(0xE1);
    const RIGHT_SHIFT_USAGE: HidUsage = HidUsage(0xE5);
    const LEFT_CONTROL_USAGE: HidUsage = HidUsage(0xE0);
    const RIGHT_CONTROL_USAGE: HidUsage = HidUsage(0xE4);
    const LEFT_OPTION_USAGE: HidUsage = HidUsage(0xE2);
    const RIGHT_OPTION_USAGE: HidUsage = HidUsage(0xE6);
    const LEFT_COMMAND_USAGE: HidUsage = HidUsage(0xE3);
    const RIGHT_COMMAND_USAGE: HidUsage = HidUsage(0xE7);
    const SAFE_KEY_USAGE: HidUsage = HidUsage(0x04);

    const LEFT_SHIFT_KEY_CODE: u16 = 0x38;
    const RIGHT_SHIFT_KEY_CODE: u16 = 0x3C;
    const LEFT_CONTROL_KEY_CODE: u16 = 0x3B;
    const RIGHT_CONTROL_KEY_CODE: u16 = 0x3E;
    const LEFT_OPTION_KEY_CODE: u16 = 0x3A;
    const RIGHT_OPTION_KEY_CODE: u16 = 0x3D;
    const LEFT_COMMAND_KEY_CODE: u16 = 0x37;
    const RIGHT_COMMAND_KEY_CODE: u16 = 0x36;
    const SAFE_KEY_CODE: u16 = 0x00;

    // SAFETY: objc_getClass and sel_registerName have their documented fixed ABIs.
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
    }

    // SAFETY: dlsym has the installed libSystem ABI. RTLD_DEFAULT resolves the
    // already-linked Objective-C runtime without loading an arbitrary library.
    #[link(name = "System")]
    unsafe extern "C" {
        fn dlsym(handle: Id, symbol: *const c_char) -> *mut c_void;

        static NSDefaultRunLoopMode: Id;
    }

    macro_rules! typed_objc_message_send {
        () => {{
            let symbol = objc_msg_send_symbol();
            // SAFETY: Every wrapper below fixes objc_msgSend to the exact ABI of
            // its selector instead of using a variadic or untyped message bridge.
            unsafe { std::mem::transmute::<*mut c_void, _>(symbol) }
        }};
    }

    macro_rules! message_send_wrapper {
        ($name:ident($($argument:ident: $argument_type:ty),*) -> $return_type:ty) => {
            unsafe fn $name($($argument: $argument_type),*) -> $return_type {
                let call: unsafe extern "C" fn($($argument_type),*) -> $return_type =
                    typed_objc_message_send!();
                // SAFETY: The caller chooses the selector paired with this concrete
                // signature, and all arguments remain valid for this message send.
                unsafe { call($($argument),*) }
            }
        };
    }

    message_send_wrapper!(msg_send_id(receiver: Id, selector: Sel) -> Id);
    message_send_wrapper!(msg_send_id_c_string(receiver: Id, selector: Sel, value: *const c_char) -> Id);
    message_send_wrapper!(msg_send_id_f64(receiver: Id, selector: Sel, value: f64) -> Id);
    message_send_wrapper!(msg_send_id_id(receiver: Id, selector: Sel, value: Id) -> Id);
    message_send_wrapper!(msg_send_id_rect_usize_usize_bool(
        receiver: Id,
        selector: Sel,
        rect: NsRect,
        style: usize,
        backing: usize,
        defer: ObjcBool
    ) -> Id);
    message_send_wrapper!(msg_send_id_mask_date_mode_dequeue(
        receiver: Id,
        selector: Sel,
        mask: u64,
        date: Id,
        mode: Id,
        dequeue: ObjcBool
    ) -> Id);
    message_send_wrapper!(msg_send_bool(receiver: Id, selector: Sel) -> ObjcBool);
    message_send_wrapper!(msg_send_bool_integer(receiver: Id, selector: Sel, value: isize) -> ObjcBool);
    message_send_wrapper!(msg_send_usize(receiver: Id, selector: Sel) -> usize);
    message_send_wrapper!(msg_send_u16(receiver: Id, selector: Sel) -> u16);
    message_send_wrapper!(msg_send_f64(receiver: Id, selector: Sel) -> f64);
    message_send_wrapper!(msg_send_cg_event(receiver: Id, selector: Sel) -> CgEventRef);
    message_send_wrapper!(msg_send_void(receiver: Id, selector: Sel) -> ());
    message_send_wrapper!(msg_send_void_bool(receiver: Id, selector: Sel, value: ObjcBool) -> ());
    message_send_wrapper!(msg_send_void_id(receiver: Id, selector: Sel, value: Id) -> ());
    message_send_wrapper!(msg_send_void_rect(receiver: Id, selector: Sel, value: NsRect) -> ());

    fn objc_msg_send_symbol() -> *mut c_void {
        const OBJC_MSG_SEND: &[u8] = b"objc_msgSend\0";
        const RTLD_DEFAULT: Id = (-2_isize) as Id;
        // SAFETY: RTLD_DEFAULT searches the process image and OBJC_MSG_SEND is a
        // static NUL-terminated runtime symbol name.
        let symbol = unsafe { dlsym(RTLD_DEFAULT, OBJC_MSG_SEND.as_ptr().cast()) };
        assert!(!symbol.is_null(), "Objective-C runtime is unavailable");
        symbol
    }

    // SAFETY: NSApplicationLoad is the installed AppKit entry point that ensures
    // AppKit classes are available before this fixture asks for NSApplication.
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        fn NSApplicationLoad() -> ObjcBool;
    }

    // SAFETY: This is the Core Graphics function declaration from CGEvent.h.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventGetIntegerValueField(event: CgEventRef, field: CgEventField) -> i64;
    }

    #[derive(Clone, Copy)]
    enum UnmarkedPolicy {
        Abort,
        Consume,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Mode {
        Delivery,
        Capture,
    }

    fn select_mode(args: &[OsString]) -> Option<Mode> {
        match args {
            [argument] if argument == OsStr::new("--run") => Some(Mode::Delivery),
            [argument] if argument == OsStr::new("--capture") => Some(Mode::Capture),
            _ => None,
        }
    }

    #[derive(Clone, Copy)]
    enum Failure {
        Setup,
        Permission,
        Focus,
        Deadline,
        Injection,
        Capture,
        UnmarkedInput,
        Cleanup,
        Delivery,
    }

    impl Failure {
        const fn category(self) -> &'static str {
            match self {
                Self::Setup => "setup",
                Self::Permission => "permission",
                Self::Focus => "focus",
                Self::Deadline => "deadline",
                Self::Injection => "injection",
                Self::Capture => "capture",
                Self::UnmarkedInput => "unmarked_input",
                Self::Cleanup => "cleanup",
                Self::Delivery => "delivery",
            }
        }

        const fn from_mac_error(error: MacError) -> Self {
            match error {
                MacError::AccessibilityPermissionRequired
                | MacError::ListenEventPermissionRequired => Self::Permission,
                _ => Self::Injection,
            }
        }

        const fn from_capture_error(error: MacError) -> Self {
            match error {
                MacError::AccessibilityPermissionRequired
                | MacError::ListenEventPermissionRequired => Self::Permission,
                _ => Self::Capture,
            }
        }
    }

    #[derive(Clone, Copy)]
    struct EventSummary {
        event_type: usize,
        modifier_flags: usize,
        key_code: u16,
        delta_x: f64,
        delta_y: f64,
    }

    const fn blocks_unmarked_dispatch(event_type: usize) -> bool {
        !matches!(event_type, 13 | 15 | 16 | 17)
    }

    #[derive(Clone, Copy, Default)]
    enum ModifierPairStage {
        #[default]
        LeftDown,
        RightDown,
        LeftUp,
        RightUp,
        Complete,
    }

    #[derive(Default)]
    struct ModifierPair {
        stage: ModifierPairStage,
    }

    impl ModifierPair {
        fn record(
            &mut self,
            key_code: u16,
            flags: usize,
            left_code: u16,
            right_code: u16,
            flag: usize,
        ) {
            self.stage = match self.stage {
                ModifierPairStage::LeftDown if key_code == left_code && flags & flag != 0 => {
                    ModifierPairStage::RightDown
                }
                ModifierPairStage::RightDown if key_code == right_code && flags & flag != 0 => {
                    ModifierPairStage::LeftUp
                }
                ModifierPairStage::LeftUp if key_code == left_code && flags & flag != 0 => {
                    ModifierPairStage::RightUp
                }
                ModifierPairStage::RightUp if key_code == right_code && flags & flag == 0 => {
                    ModifierPairStage::Complete
                }
                stage => stage,
            };
        }

        const fn is_complete(&self) -> bool {
            matches!(self.stage, ModifierPairStage::Complete)
        }
    }

    #[derive(Clone, Copy, Default)]
    enum ChordStage {
        #[default]
        CommandDown,
        OptionDown,
        KeyDown,
        KeyUp,
        OptionUp,
        CommandUp,
        Complete,
    }

    #[derive(Default)]
    struct ChordObservation {
        stage: ChordStage,
    }

    impl ChordObservation {
        fn record(&mut self, event: &EventSummary) {
            let chord_flags =
                event.modifier_flags & (NS_EVENT_MODIFIER_COMMAND | NS_EVENT_MODIFIER_OPTION);
            self.stage = match self.stage {
                ChordStage::CommandDown
                    if event.event_type == NS_EVENT_FLAGS_CHANGED
                        && event.key_code == LEFT_COMMAND_KEY_CODE
                        && chord_flags == NS_EVENT_MODIFIER_COMMAND =>
                {
                    ChordStage::OptionDown
                }
                ChordStage::OptionDown
                    if event.event_type == NS_EVENT_FLAGS_CHANGED
                        && event.key_code == LEFT_OPTION_KEY_CODE
                        && chord_flags
                            == (NS_EVENT_MODIFIER_COMMAND | NS_EVENT_MODIFIER_OPTION) =>
                {
                    ChordStage::KeyDown
                }
                ChordStage::KeyDown
                    if event.event_type == NS_EVENT_KEY_DOWN
                        && event.key_code == SAFE_KEY_CODE
                        && chord_flags
                            == (NS_EVENT_MODIFIER_COMMAND | NS_EVENT_MODIFIER_OPTION) =>
                {
                    ChordStage::KeyUp
                }
                ChordStage::KeyUp
                    if event.event_type == NS_EVENT_KEY_UP
                        && event.key_code == SAFE_KEY_CODE
                        && chord_flags
                            == (NS_EVENT_MODIFIER_COMMAND | NS_EVENT_MODIFIER_OPTION) =>
                {
                    ChordStage::OptionUp
                }
                ChordStage::OptionUp
                    if event.event_type == NS_EVENT_FLAGS_CHANGED
                        && event.key_code == LEFT_OPTION_KEY_CODE
                        && chord_flags == NS_EVENT_MODIFIER_COMMAND =>
                {
                    ChordStage::CommandUp
                }
                ChordStage::CommandUp
                    if event.event_type == NS_EVENT_FLAGS_CHANGED
                        && event.key_code == LEFT_COMMAND_KEY_CODE
                        && chord_flags == 0 =>
                {
                    ChordStage::Complete
                }
                stage => stage,
            };
        }

        const fn is_complete(&self) -> bool {
            matches!(self.stage, ChordStage::Complete)
        }
    }

    #[derive(Default)]
    struct Observation {
        keyboard_events: u32,
        pointer_events: u32,
        scroll_events: u32,
        unmarked_keyboard: u32,
        unmarked_pointer: u32,
        unmarked_scroll: u32,
        unmarked_other: u32,
        shift: ModifierPair,
        control: ModifierPair,
        option: ModifierPair,
        command: ModifierPair,
        chord: ChordObservation,
        cleanup_control_down: bool,
        cleanup_control_up: bool,
        left_button_down: bool,
        left_button_dragged: bool,
        left_button_up: bool,
        right_button_down: bool,
        right_button_up: bool,
        scroll_x: bool,
        scroll_y: bool,
    }

    impl Observation {
        fn record_unmarked(&mut self, event_type: usize) {
            let count = match event_type {
                NS_EVENT_KEY_DOWN | NS_EVENT_KEY_UP | NS_EVENT_FLAGS_CHANGED => {
                    &mut self.unmarked_keyboard
                }
                NS_EVENT_SCROLL_WHEEL => &mut self.unmarked_scroll,
                1..=7 | 25..=27 => &mut self.unmarked_pointer,
                _ => &mut self.unmarked_other,
            };
            *count = count.saturating_add(1);
        }

        fn record(&mut self, event: &EventSummary) {
            match event.event_type {
                NS_EVENT_KEY_DOWN | NS_EVENT_KEY_UP | NS_EVENT_FLAGS_CHANGED => {
                    self.keyboard_events = self.keyboard_events.saturating_add(1);
                }
                NS_EVENT_SCROLL_WHEEL => {
                    self.scroll_events = self.scroll_events.saturating_add(1);
                    self.scroll_x |= event.delta_x != 0.0;
                    self.scroll_y |= event.delta_y != 0.0;
                }
                NS_EVENT_LEFT_MOUSE_DOWN
                | NS_EVENT_LEFT_MOUSE_UP
                | NS_EVENT_RIGHT_MOUSE_DOWN
                | NS_EVENT_RIGHT_MOUSE_UP
                | NS_EVENT_MOUSE_MOVED
                | NS_EVENT_LEFT_MOUSE_DRAGGED
                | NS_EVENT_RIGHT_MOUSE_DRAGGED
                | NS_EVENT_OTHER_MOUSE_DOWN
                | NS_EVENT_OTHER_MOUSE_UP
                | NS_EVENT_OTHER_MOUSE_DRAGGED => {
                    self.pointer_events = self.pointer_events.saturating_add(1);
                }
                _ => {}
            }

            if event.event_type == NS_EVENT_FLAGS_CHANGED {
                self.shift.record(
                    event.key_code,
                    event.modifier_flags,
                    LEFT_SHIFT_KEY_CODE,
                    RIGHT_SHIFT_KEY_CODE,
                    NS_EVENT_MODIFIER_SHIFT,
                );
                self.control.record(
                    event.key_code,
                    event.modifier_flags,
                    LEFT_CONTROL_KEY_CODE,
                    RIGHT_CONTROL_KEY_CODE,
                    NS_EVENT_MODIFIER_CONTROL,
                );
                self.option.record(
                    event.key_code,
                    event.modifier_flags,
                    LEFT_OPTION_KEY_CODE,
                    RIGHT_OPTION_KEY_CODE,
                    NS_EVENT_MODIFIER_OPTION,
                );
                self.command.record(
                    event.key_code,
                    event.modifier_flags,
                    LEFT_COMMAND_KEY_CODE,
                    RIGHT_COMMAND_KEY_CODE,
                    NS_EVENT_MODIFIER_COMMAND,
                );
                if self.control.is_complete() && event.key_code == LEFT_CONTROL_KEY_CODE {
                    if event.modifier_flags & NS_EVENT_MODIFIER_CONTROL != 0 {
                        self.cleanup_control_down = true;
                    } else if self.cleanup_control_down {
                        self.cleanup_control_up = true;
                    }
                }
            }

            if self.modifier_pairs_complete() {
                self.chord.record(event);
            }

            match event.event_type {
                NS_EVENT_LEFT_MOUSE_DOWN => self.left_button_down = true,
                NS_EVENT_LEFT_MOUSE_DRAGGED => self.left_button_dragged = true,
                NS_EVENT_LEFT_MOUSE_UP => self.left_button_up = true,
                NS_EVENT_RIGHT_MOUSE_DOWN => self.right_button_down = true,
                NS_EVENT_RIGHT_MOUSE_UP => self.right_button_up = true,
                _ => {}
            }
        }

        const fn modifier_pairs_complete(&self) -> bool {
            self.shift.is_complete()
                && self.control.is_complete()
                && self.option.is_complete()
                && self.command.is_complete()
        }

        fn verify(&self) -> bool {
            self.keyboard_events > 0
                && self.pointer_events > 0
                && self.scroll_events > 0
                && self.modifier_pairs_complete()
                && self.chord.is_complete()
                && self.cleanup_control_down
                && self.cleanup_control_up
                && self.left_button_down
                && self.left_button_dragged
                && self.left_button_up
                && self.right_button_down
                && self.right_button_up
                && self.scroll_x
                && self.scroll_y
        }
    }

    struct AppEvent {
        event: Id,
    }

    impl AppEvent {
        fn event_type(&self) -> usize {
            // SAFETY: `event` is an NSEvent returned by NSApplication and remains
            // valid until this queue iteration completes.
            unsafe { msg_send_usize(self.event, selector(b"type\0")) }
        }

        fn summary(&self) -> EventSummary {
            let event_type = self.event_type();
            // SAFETY: modifierFlags is valid for every NSEvent during this queue iteration.
            let modifier_flags =
                unsafe { msg_send_usize(self.event, selector(b"modifierFlags\0")) };
            let key_code = if matches!(
                event_type,
                NS_EVENT_KEY_DOWN | NS_EVENT_KEY_UP | NS_EVENT_FLAGS_CHANGED
            ) {
                // SAFETY: keyCode is valid for the keyboard event types matched above.
                unsafe { msg_send_u16(self.event, selector(b"keyCode\0")) }
            } else {
                0
            };
            let (delta_x, delta_y) = if event_type == NS_EVENT_SCROLL_WHEEL {
                // SAFETY: deltaX and deltaY are valid for NSEventTypeScrollWheel.
                unsafe {
                    (
                        msg_send_f64(self.event, selector(b"deltaX\0")),
                        msg_send_f64(self.event, selector(b"deltaY\0")),
                    )
                }
            } else {
                (0.0, 0.0)
            };
            EventSummary {
                event_type,
                modifier_flags,
                key_code,
                delta_x,
                delta_y,
            }
        }

        fn is_fixture_event(&self) -> bool {
            // SAFETY: `CGEvent` returns a borrowed Core Graphics event for this NSEvent.
            let cg_event = unsafe { msg_send_cg_event(self.event, selector(b"CGEvent\0")) };
            if cg_event.is_null() {
                return false;
            }
            // SAFETY: `cg_event` remains borrowed from the live NSEvent for this call.
            unsafe {
                CGEventGetIntegerValueField(cg_event, CG_EVENT_SOURCE_USER_DATA)
                    == SYNTHETIC_EVENT_MARKER
            }
        }
    }

    struct AutoreleasePool {
        pool: Id,
    }

    impl AutoreleasePool {
        fn new() -> Result<Self, Failure> {
            let class = class(b"NSAutoreleasePool\0")?;
            // SAFETY: `alloc` and `init` have their declared no-argument Objective-C ABI.
            let allocated = unsafe { msg_send_id(class, selector(b"alloc\0")) };
            if allocated.is_null() {
                return Err(Failure::Setup);
            }
            // SAFETY: `allocated` is a newly allocated NSAutoreleasePool instance.
            let pool = unsafe { msg_send_id(allocated, selector(b"init\0")) };
            if pool.is_null() {
                return Err(Failure::Setup);
            }
            Ok(Self { pool })
        }
    }

    impl Drop for AutoreleasePool {
        fn drop(&mut self) {
            if self.pool.is_null() {
                return;
            }
            // SAFETY: `pool` is the owned NSAutoreleasePool created above; `drain`
            // releases its scoped autoreleased objects exactly once.
            unsafe {
                msg_send_void(self.pool, selector(b"drain\0"));
            }
        }
    }

    struct NativeWindow {
        _pool: AutoreleasePool,
        app: Id,
        window: Id,
    }

    impl NativeWindow {
        fn open(instruction: &'static [u8]) -> Result<Self, Failure> {
            let pool = AutoreleasePool::new()?;
            // SAFETY: This documented AppKit initializer only prepares AppKit in
            // the current process and is called after the explicit --run gate.
            if !objc_bool_is_yes(unsafe { NSApplicationLoad() }) {
                return Err(Failure::Setup);
            }
            let app_class = class(b"NSApplication\0")?;
            // SAFETY: `sharedApplication` is an AppKit class method with a no-argument ABI.
            let app = unsafe { msg_send_id(app_class, selector(b"sharedApplication\0")) };
            if app.is_null() {
                return Err(Failure::Setup);
            }
            // SAFETY: `setActivationPolicy:` accepts the documented NSInteger policy value.
            let policy_set = unsafe {
                msg_send_bool_integer(
                    app,
                    selector(b"setActivationPolicy:\0"),
                    NS_APPLICATION_ACTIVATION_POLICY_REGULAR,
                )
            };
            if !objc_bool_is_yes(policy_set) {
                return Err(Failure::Setup);
            }
            // SAFETY: `finishLaunching` performs only this process's AppKit launch setup.
            unsafe {
                msg_send_void(app, selector(b"finishLaunching\0"));
            }

            let window_class = class(b"NSWindow\0")?;
            // SAFETY: `alloc` has the declared no-argument Objective-C ABI.
            let allocated = unsafe { msg_send_id(window_class, selector(b"alloc\0")) };
            if allocated.is_null() {
                return Err(Failure::Setup);
            }
            let content_rect = NsRect {
                origin: NsPoint { x: 120.0, y: 120.0 },
                size: NsSize {
                    width: 520.0,
                    height: 180.0,
                },
            };
            // SAFETY: The concrete NSWindow initializer ABI uses NSRect, NSUInteger,
            // NSUInteger, and BOOL exactly as declared in NSWindow.h.
            let window = unsafe {
                msg_send_id_rect_usize_usize_bool(
                    allocated,
                    selector(b"initWithContentRect:styleMask:backing:defer:\0"),
                    content_rect,
                    NS_WINDOW_STYLE,
                    NS_BACKING_STORE_BUFFERED,
                    OBJC_NO,
                )
            };
            if window.is_null() {
                return Err(Failure::Setup);
            }
            // SAFETY: Retaining the allocated window through close prevents a
            // user close event from invalidating the fixture's tracked pointer.
            unsafe {
                msg_send_void_bool(window, selector(b"setReleasedWhenClosed:\0"), OBJC_NO);
            }

            let string_class = class(b"NSString\0")?;
            const TITLE: &[u8] = b"MonHop self-process input diagnostic\0";
            // SAFETY: TITLE is NUL-terminated UTF-8 and NSString returns an autoreleased object.
            let title = unsafe {
                msg_send_id_c_string(
                    string_class,
                    selector(b"stringWithUTF8String:\0"),
                    TITLE.as_ptr().cast(),
                )
            };
            if title.is_null() {
                return Err(Failure::Setup);
            }
            add_instruction_label(window, string_class, instruction)?;
            // SAFETY: `setTitle:` and `makeKeyAndOrderFront:` use one object argument.
            unsafe {
                msg_send_void_id(window, selector(b"setTitle:\0"), title);
                msg_send_void_id(
                    window,
                    selector(b"makeKeyAndOrderFront:\0"),
                    std::ptr::null_mut(),
                );
                msg_send_void_bool(app, selector(b"activateIgnoringOtherApps:\0"), OBJC_YES);
            }

            Ok(Self {
                _pool: pool,
                app,
                window,
            })
        }

        fn is_active_key_window(&self) -> bool {
            // SAFETY: `app` and `window` are the fixture's live AppKit objects.
            unsafe {
                objc_bool_is_yes(msg_send_bool(self.app, selector(b"isActive\0")))
                    && objc_bool_is_yes(msg_send_bool(self.window, selector(b"isKeyWindow\0")))
            }
        }

        fn wait_for_focus(
            &self,
            deadline: Instant,
            mut observations: Option<&mut Observation>,
            unmarked_policy: UnmarkedPolicy,
        ) -> Result<(), Failure> {
            let focus_deadline = deadline.min(Instant::now() + FOCUS_WAIT);
            while Instant::now() < focus_deadline {
                if self.is_active_key_window() {
                    return Ok(());
                }
                self.process_one_event(
                    focus_deadline,
                    observations.as_deref_mut(),
                    unmarked_policy,
                )?;
            }
            if Instant::now() >= deadline {
                Err(Failure::Deadline)
            } else {
                Err(Failure::Focus)
            }
        }

        fn consume_events_until(
            &self,
            deadline: Instant,
            observations: &mut Observation,
        ) -> Result<(), Failure> {
            let idle_deadline = deadline.min(Instant::now() + EVENT_IDLE_WAIT);
            while Instant::now() < idle_deadline {
                self.process_one_event(idle_deadline, Some(observations), UnmarkedPolicy::Abort)?;
            }
            if Instant::now() >= deadline {
                Err(Failure::Deadline)
            } else {
                Ok(())
            }
        }

        fn process_one_event(
            &self,
            deadline: Instant,
            observations: Option<&mut Observation>,
            unmarked_policy: UnmarkedPolicy,
        ) -> Result<(), Failure> {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let wait = (deadline - now).min(EVENT_POLL_SLICE).as_secs_f64();
            let date_class = class(b"NSDate\0")?;
            // SAFETY: `dateWithTimeIntervalSinceNow:` takes the declared NSTimeInterval.
            let date = unsafe {
                msg_send_id_f64(
                    date_class,
                    selector(b"dateWithTimeIntervalSinceNow:\0"),
                    wait,
                )
            };
            if date.is_null() {
                return Err(Failure::Setup);
            }
            // SAFETY: NSDefaultRunLoopMode is a process-lifetime Foundation constant.
            let mode = unsafe { NSDefaultRunLoopMode };
            // SAFETY: The selector's exact ABI is NSEventMask, NSDate *, NSRunLoopMode,
            // and BOOL as declared by NSApplication's NSEvent category.
            let event = unsafe {
                msg_send_id_mask_date_mode_dequeue(
                    self.app,
                    selector(b"nextEventMatchingMask:untilDate:inMode:dequeue:\0"),
                    NS_EVENT_MASK_ANY,
                    date,
                    mode,
                    OBJC_YES,
                )
            };
            if event.is_null() {
                return Ok(());
            }

            let event = AppEvent { event };
            if event.is_fixture_event() {
                if let Some(observations) = observations {
                    observations.record(&event.summary());
                }
                return Ok(());
            }
            if blocks_unmarked_dispatch(event.event_type()) {
                if matches!(unmarked_policy, UnmarkedPolicy::Consume) {
                    return Ok(());
                }
                if let Some(observations) = observations {
                    observations.record_unmarked(event.event_type());
                }
                // Do not enter AppKit tracking or keyboard menu handling from an
                // unmarked event. The caller always performs self-only cleanup.
                return Err(Failure::UnmarkedInput);
            }
            // SAFETY: Non-input AppKit lifecycle events continue through the normal queue.
            unsafe {
                msg_send_void_id(self.app, selector(b"sendEvent:\0"), event.event);
            }
            Ok(())
        }
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            if self.window.is_null() {
                return;
            }
            // SAFETY: releasedWhenClosed is false, so this fixture retains ownership
            // through close and balances its alloc with one explicit release.
            unsafe {
                msg_send_void_id(self.window, selector(b"orderOut:\0"), std::ptr::null_mut());
                msg_send_void(self.window, selector(b"close\0"));
                msg_send_void(self.window, selector(b"release\0"));
            }
            self.window = std::ptr::null_mut();
        }
    }

    fn add_instruction_label(
        window: Id,
        string_class: Id,
        instruction: &'static [u8],
    ) -> Result<(), Failure> {
        // SAFETY: instruction is NUL-terminated UTF-8 and returns an autoreleased NSString.
        let text = unsafe {
            msg_send_id_c_string(
                string_class,
                selector(b"stringWithUTF8String:\0"),
                instruction.as_ptr().cast(),
            )
        };
        if text.is_null() {
            return Err(Failure::Setup);
        }
        let field_class = class(b"NSTextField\0")?;
        // SAFETY: labelWithString: takes the NSString created above and returns an NSTextField.
        let label = unsafe { msg_send_id_id(field_class, selector(b"labelWithString:\0"), text) };
        if label.is_null() {
            return Err(Failure::Setup);
        }
        // SAFETY: contentView is owned by the fixture window for its entire lifetime.
        let content_view = unsafe { msg_send_id(window, selector(b"contentView\0")) };
        if content_view.is_null() {
            return Err(Failure::Setup);
        }
        let label_frame = NsRect {
            origin: NsPoint { x: 24.0, y: 24.0 },
            size: NsSize {
                width: 472.0,
                height: 132.0,
            },
        };
        // SAFETY: setFrame: takes NSRect, and contentView retains label through addSubview:.
        unsafe {
            msg_send_void_rect(label, selector(b"setFrame:\0"), label_frame);
            msg_send_void_id(content_view, selector(b"addSubview:\0"), label);
        }
        Ok(())
    }

    fn run_diagnostic(observations: &mut Observation) -> Result<(), Failure> {
        let native_window = NativeWindow::open(
            b"Self-process delivery test.\nDo not type or move the mouse.\nThis window closes automatically.\0",
        )?;
        let deadline = Instant::now() + MAX_SMOKE_DURATION;
        native_window.wait_for_focus(deadline, Some(observations), UnmarkedPolicy::Abort)?;

        let mut injector = MacInjector::for_current_process().map_err(Failure::from_mac_error)?;
        let test = run_steps(&native_window, &mut injector, deadline, observations);
        let cleanup = injector.release_all().map_err(|_| Failure::Cleanup);
        let cleanup_delivery = native_window.consume_events_until(deadline, observations);

        test.and(cleanup).and(cleanup_delivery)?;
        if !native_window.is_active_key_window() {
            return Err(Failure::Focus);
        }
        if observations.verify() {
            Ok(())
        } else {
            Err(Failure::Delivery)
        }
    }

    fn run_capture_window() -> Result<PassiveDiagnosticCounts, Failure> {
        let window = NativeWindow::open(
            b"Physical input check - 10 seconds\nMove and click here. Scroll both ways and press a few keys.\nAvoid system shortcuts. No keys or text are recorded.\nThis window closes automatically.\0",
        )?;
        let deadline = Instant::now() + Duration::from_secs(12);
        window.wait_for_focus(deadline, None, UnmarkedPolicy::Consume)?;
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let capture = scope
                .spawn(|| run_passive_diagnostic_with_cancel(Duration::from_secs(10), &cancelled));
            let mut failure = None;
            while !capture.is_finished() {
                if failure.is_some() {
                    std::thread::sleep(EVENT_POLL_SLICE);
                    continue;
                }
                let state = if Instant::now() >= deadline {
                    Err(Failure::Deadline)
                } else if !window.is_active_key_window() {
                    Err(Failure::Focus)
                } else {
                    window.process_one_event(deadline, None, UnmarkedPolicy::Consume)
                };
                if let Err(error) = state {
                    failure = Some(error);
                    cancelled.store(true, Ordering::Release);
                }
            }
            let result = capture.join().map_err(|_| Failure::Setup)?;
            if let Some(error) = failure {
                return Err(error);
            }
            if !window.is_active_key_window() {
                return Err(Failure::Focus);
            }
            result.map_err(Failure::from_capture_error)
        })
    }

    fn run_steps(
        native_window: &NativeWindow,
        injector: &mut MacInjector,
        deadline: Instant,
        observations: &mut Observation,
    ) -> Result<(), Failure> {
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.move_to(Point::new(24.0, 24.0)),
        )?;

        exercise_modifier_pair(
            native_window,
            injector,
            deadline,
            observations,
            LEFT_SHIFT_USAGE,
            RIGHT_SHIFT_USAGE,
        )?;
        exercise_modifier_pair(
            native_window,
            injector,
            deadline,
            observations,
            LEFT_CONTROL_USAGE,
            RIGHT_CONTROL_USAGE,
        )?;
        exercise_modifier_pair(
            native_window,
            injector,
            deadline,
            observations,
            LEFT_OPTION_USAGE,
            RIGHT_OPTION_USAGE,
        )?;
        exercise_modifier_pair(
            native_window,
            injector,
            deadline,
            observations,
            LEFT_COMMAND_USAGE,
            RIGHT_COMMAND_USAGE,
        )?;

        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(LEFT_COMMAND_USAGE, true),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(LEFT_OPTION_USAGE, true),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(SAFE_KEY_USAGE, true),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(SAFE_KEY_USAGE, false),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(LEFT_OPTION_USAGE, false),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(LEFT_COMMAND_USAGE, false),
        )?;

        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.button(MouseButton::Left, true, SINGLE_CLICK),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.move_by(Point::new(1.0, 1.0)),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.button(MouseButton::Left, false, SINGLE_CLICK),
        )?;

        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.button(MouseButton::Right, true, SINGLE_CLICK),
        )?;
        for _ in 0..4 {
            post_step(
                native_window,
                injector,
                deadline,
                observations,
                |injector| injector.scroll(0.25, -0.5),
            )?;
        }
        // Keep one key and one button held so release_all has observable cleanup work.
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(LEFT_CONTROL_USAGE, true),
        )
    }

    fn exercise_modifier_pair(
        native_window: &NativeWindow,
        injector: &mut MacInjector,
        deadline: Instant,
        observations: &mut Observation,
        left: HidUsage,
        right: HidUsage,
    ) -> Result<(), Failure> {
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(left, true),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(right, true),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(left, false),
        )?;
        post_step(
            native_window,
            injector,
            deadline,
            observations,
            |injector| injector.key(right, false),
        )
    }

    fn post_step(
        native_window: &NativeWindow,
        injector: &mut MacInjector,
        deadline: Instant,
        observations: &mut Observation,
        post: impl FnOnce(&mut MacInjector) -> Result<(), MacError>,
    ) -> Result<(), Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if !native_window.is_active_key_window() {
            return Err(Failure::Focus);
        }
        post(injector).map_err(Failure::from_mac_error)?;
        native_window.consume_events_until(deadline, observations)?;
        if !native_window.is_active_key_window() {
            return Err(Failure::Focus);
        }
        Ok(())
    }

    fn class(name: &'static [u8]) -> Result<Id, Failure> {
        // SAFETY: Every call site supplies a static, NUL-terminated Objective-C class name.
        let class = unsafe { objc_getClass(name.as_ptr().cast()) };
        if class.is_null() {
            Err(Failure::Setup)
        } else {
            Ok(class)
        }
    }

    fn selector(name: &'static [u8]) -> Sel {
        // SAFETY: Every call site supplies a static, NUL-terminated selector name.
        unsafe { sel_registerName(name.as_ptr().cast()) }
    }

    pub(super) fn main() -> ExitCode {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let mode = match select_mode(&args) {
            Some(mode) => mode,
            None => {
                eprintln!(
                    "native_window status=NOT_RUN options='--run: self-process delivery; --capture: ten-second count-only window'"
                );
                return ExitCode::from(2);
            }
        };
        if mode == Mode::Capture {
            return match run_capture_window() {
                Ok(counts) => {
                    println!(
                        "native_window status=CAPTURE_COMPLETE observed={{keyboard:{},pointer:{},scroll:{},synthetic_filtered:{}}} delivery=NOT_TESTED",
                        counts.keyboard_events,
                        counts.pointer_events,
                        counts.scroll_events,
                        counts.synthetic_events_filtered,
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!(
                        "native_window status=FAIL category={} mode=capture",
                        error.category()
                    );
                    ExitCode::FAILURE
                }
            };
        }
        let mut observations = Observation::default();
        match run_diagnostic(&mut observations) {
            Ok(()) => {
                println!(
                    "native_window status=PASS observed={{keyboard:{},pointer:{},scroll:{}}} unsupported={{caps_lock,keyboard_layout,system_shortcuts}}",
                    observations.keyboard_events,
                    observations.pointer_events,
                    observations.scroll_events,
                );
                ExitCode::SUCCESS
            }
            Err(failure) => {
                eprintln!(
                    "native_window status=FAIL category={} observed={{keyboard:{},pointer:{},scroll:{}}} unmarked={{keyboard:{},pointer:{},scroll:{},other:{}}} unsupported={{caps_lock,keyboard_layout,system_shortcuts}}",
                    failure.category(),
                    observations.keyboard_events,
                    observations.pointer_events,
                    observations.scroll_events,
                    observations.unmarked_keyboard,
                    observations.unmarked_pointer,
                    observations.unmarked_scroll,
                    observations.unmarked_other,
                );
                ExitCode::from(1)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[derive(Clone, Copy)]
        struct TraceEvent {
            event: EventSummary,
            required_release: bool,
        }

        const fn event(
            event_type: usize,
            modifier_flags: usize,
            key_code: u16,
            delta_x: f64,
            delta_y: f64,
            required_release: bool,
        ) -> TraceEvent {
            TraceEvent {
                event: EventSummary {
                    event_type,
                    modifier_flags,
                    key_code,
                    delta_x,
                    delta_y,
                },
                required_release,
            }
        }

        fn add_modifier_pair(
            trace: &mut Vec<TraceEvent>,
            left_key_code: u16,
            right_key_code: u16,
            modifier_flag: usize,
        ) {
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                modifier_flag,
                left_key_code,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                modifier_flag,
                right_key_code,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                modifier_flag,
                left_key_code,
                0.0,
                0.0,
                true,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                0,
                right_key_code,
                0.0,
                0.0,
                true,
            ));
        }

        fn passing_trace() -> Vec<TraceEvent> {
            let mut trace = Vec::new();
            add_modifier_pair(
                &mut trace,
                LEFT_SHIFT_KEY_CODE,
                RIGHT_SHIFT_KEY_CODE,
                NS_EVENT_MODIFIER_SHIFT,
            );
            add_modifier_pair(
                &mut trace,
                LEFT_CONTROL_KEY_CODE,
                RIGHT_CONTROL_KEY_CODE,
                NS_EVENT_MODIFIER_CONTROL,
            );
            add_modifier_pair(
                &mut trace,
                LEFT_OPTION_KEY_CODE,
                RIGHT_OPTION_KEY_CODE,
                NS_EVENT_MODIFIER_OPTION,
            );
            add_modifier_pair(
                &mut trace,
                LEFT_COMMAND_KEY_CODE,
                RIGHT_COMMAND_KEY_CODE,
                NS_EVENT_MODIFIER_COMMAND,
            );

            let chord_flags = NS_EVENT_MODIFIER_COMMAND | NS_EVENT_MODIFIER_OPTION;
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                NS_EVENT_MODIFIER_COMMAND,
                LEFT_COMMAND_KEY_CODE,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                chord_flags,
                LEFT_OPTION_KEY_CODE,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_KEY_DOWN,
                chord_flags,
                SAFE_KEY_CODE,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_KEY_UP,
                chord_flags,
                SAFE_KEY_CODE,
                0.0,
                0.0,
                true,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                NS_EVENT_MODIFIER_COMMAND,
                LEFT_OPTION_KEY_CODE,
                0.0,
                0.0,
                true,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                0,
                LEFT_COMMAND_KEY_CODE,
                0.0,
                0.0,
                true,
            ));

            trace.push(event(NS_EVENT_LEFT_MOUSE_DOWN, 0, 0, 0.0, 0.0, false));
            trace.push(event(NS_EVENT_LEFT_MOUSE_DRAGGED, 0, 0, 0.0, 0.0, false));
            trace.push(event(NS_EVENT_LEFT_MOUSE_UP, 0, 0, 0.0, 0.0, true));
            trace.push(event(NS_EVENT_RIGHT_MOUSE_DOWN, 0, 0, 0.0, 0.0, false));
            trace.push(event(NS_EVENT_SCROLL_WHEEL, 0, 0, 1.0, -2.0, false));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                NS_EVENT_MODIFIER_CONTROL,
                LEFT_CONTROL_KEY_CODE,
                0.0,
                0.0,
                false,
            ));
            trace.push(event(
                NS_EVENT_FLAGS_CHANGED,
                0,
                LEFT_CONTROL_KEY_CODE,
                0.0,
                0.0,
                true,
            ));
            trace.push(event(NS_EVENT_RIGHT_MOUSE_UP, 0, 0, 0.0, 0.0, true));
            trace
        }

        fn observe(trace: &[TraceEvent], omitted: Option<usize>) -> Observation {
            let mut observations = Observation::default();
            for (index, trace_event) in trace.iter().enumerate() {
                if Some(index) != omitted {
                    observations.record(&trace_event.event);
                }
            }
            observations
        }

        #[test]
        fn complete_trace_passes_without_os_input() {
            assert!(observe(&passing_trace(), None).verify());
        }

        #[test]
        fn omitting_any_required_release_fails() {
            let trace = passing_trace();
            for (index, trace_event) in trace.iter().enumerate() {
                if trace_event.required_release {
                    assert!(
                        !observe(&trace, Some(index)).verify(),
                        "omitted required release at trace index {index} passed"
                    );
                }
            }
        }

        #[test]
        fn unmarked_input_never_reaches_appkit_dispatch() {
            for event_type in [
                NS_EVENT_LEFT_MOUSE_DOWN,
                NS_EVENT_LEFT_MOUSE_DRAGGED,
                NS_EVENT_KEY_DOWN,
                NS_EVENT_FLAGS_CHANGED,
                NS_EVENT_SCROLL_WHEEL,
                14,
                18,
                19,
                20,
                29,
                30,
                31,
                32,
                33,
                34,
                37,
                38,
                40,
                41,
            ] {
                assert!(blocks_unmarked_dispatch(event_type));
            }
            for event_type in [13, 15, 16, 17] {
                assert!(!blocks_unmarked_dispatch(event_type));
            }
        }

        #[test]
        fn unmarked_categories_match_delivery_receipts() {
            let mut observations = Observation::default();
            for event_type in [
                NS_EVENT_KEY_DOWN,
                NS_EVENT_LEFT_MOUSE_DOWN,
                NS_EVENT_MOUSE_ENTERED,
                NS_EVENT_MOUSE_EXITED,
                NS_EVENT_SCROLL_WHEEL,
                14,
            ] {
                observations.record_unmarked(event_type);
            }
            assert_eq!(observations.unmarked_keyboard, 1);
            assert_eq!(observations.unmarked_pointer, 1);
            assert_eq!(observations.unmarked_scroll, 1);
            assert_eq!(observations.unmarked_other, 3);
        }

        #[test]
        fn capture_failures_never_claim_injection() {
            for error in [
                MacError::DiagnosticCancelled,
                MacError::EventTapUnavailable,
                MacError::EventTapSourceUnavailable,
                MacError::EventTapDisabled,
            ] {
                assert_eq!(Failure::from_capture_error(error).category(), "capture");
            }
            assert_eq!(
                Failure::from_capture_error(MacError::ListenEventPermissionRequired).category(),
                "permission"
            );
        }

        #[test]
        fn only_exact_runtime_mode_arguments_are_accepted() {
            let arguments = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
            assert_eq!(select_mode(&arguments(&["--run"])), Some(Mode::Delivery));
            assert_eq!(select_mode(&arguments(&["--capture"])), Some(Mode::Capture));
            for values in [
                &[][..],
                &["--invalid"][..],
                &["--run", "extra"][..],
                &["--capture", "extra"][..],
                &["--run", "--capture"][..],
                &["--capture", "--run"][..],
            ] {
                assert_eq!(select_mode(&arguments(values)), None);
            }
        }
    }
}
