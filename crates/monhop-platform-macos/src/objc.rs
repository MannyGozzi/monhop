//! The Objective-C runtime calls MonHop makes: typed `objc_msgSend` wrappers, class and selector
//! lookup, and an autorelease pool.

use std::{
    ffi::{c_char, c_void},
    io,
};

pub(crate) type Id = *mut c_void;
pub(crate) type Sel = *mut c_void;

// SAFETY: The Foundation version symbol is an ABI-stable link anchor; its value is never read.
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSFoundationVersionNumber: f64;
}

// SAFETY: objc_getClass and sel_registerName have fixed ABIs from the installed SDK.
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
}

// SAFETY: dlsym resolves the documented objc_msgSend entry point in the loaded runtime.
#[link(name = "System")]
unsafe extern "C" {
    fn dlsym(handle: Id, symbol: *const c_char) -> *mut c_void;
}

#[derive(Clone, Copy)]
pub(crate) struct Objc {
    message_send: *mut c_void,
}

impl Objc {
    pub(crate) fn load() -> io::Result<Self> {
        const RTLD_DEFAULT: Id = (-2_isize) as Id;
        // SAFETY: RTLD_DEFAULT and the static symbol name are valid inputs to dlsym.
        let message_send = unsafe { dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr()) };
        if message_send.is_null() {
            return Err(io::Error::other("Objective-C runtime unavailable"));
        }
        Ok(Self { message_send })
    }

    pub(crate) fn send_id(&self, receiver: Id, selector: Sel) -> Id {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact result ABI.
        let call: unsafe extern "C" fn(Id, Sel) -> Id =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and a no-argument selector.
        unsafe { call(receiver, selector) }
    }

    pub(crate) fn send_id_id(&self, receiver: Id, selector: Sel, value: Id) -> Id {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact result ABI.
        let call: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver, selector, and object argument.
        unsafe { call(receiver, selector, value) }
    }

    pub(crate) fn send_isize(&self, receiver: Id, selector: Sel) -> isize {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact integer ABI.
        let call: unsafe extern "C" fn(Id, Sel) -> isize =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and integer-returning selector.
        unsafe { call(receiver, selector) }
    }

    pub(crate) fn send_void(&self, receiver: Id, selector: Sel) {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact void ABI.
        let call: unsafe extern "C" fn(Id, Sel) = unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and void-returning selector.
        unsafe { call(receiver, selector) };
    }

    pub(crate) fn send_void_id(&self, receiver: Id, selector: Sel, value: Id) {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact void ABI.
        let call: unsafe extern "C" fn(Id, Sel, Id) =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver, selector, and object argument.
        unsafe { call(receiver, selector, value) };
    }

    pub(crate) fn send_id_u64_id(&self, receiver: Id, selector: Sel, flags: u64, value: Id) -> Id {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact result ABI.
        let call: unsafe extern "C" fn(Id, Sel, u64, Id) -> Id =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver, selector, integer and object.
        unsafe { call(receiver, selector, flags, value) }
    }
}

pub(crate) struct AutoreleasePool {
    objc: Objc,
    value: Id,
}

impl AutoreleasePool {
    pub(crate) fn new(objc: Objc) -> io::Result<Self> {
        let class = class(c"NSAutoreleasePool", "Foundation unavailable")?;
        let pool = objc.send_id(class, selector(c"alloc"));
        if pool.is_null() {
            return Err(io::Error::other("Objective-C autorelease pool unavailable"));
        }
        let pool = objc.send_id(pool, selector(c"init"));
        if pool.is_null() {
            return Err(io::Error::other("Objective-C autorelease pool unavailable"));
        }
        Ok(Self { objc, value: pool })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        self.objc.send_void(self.value, selector(c"drain"));
    }
}

pub(crate) fn class(name: &'static std::ffi::CStr, unavailable: &'static str) -> io::Result<Id> {
    // SAFETY: name is a static NUL-terminated Objective-C class name.
    let class = unsafe { objc_getClass(name.as_ptr()) };
    if class.is_null() {
        Err(io::Error::other(unavailable))
    } else {
        Ok(class)
    }
}

pub(crate) fn selector(name: &'static std::ffi::CStr) -> Sel {
    // SAFETY: name is a static NUL-terminated selector name.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Keeps Foundation linked for classes looked up by name, such as NSProcessInfo.
pub(crate) fn link_foundation() {
    std::hint::black_box(&raw const NSFoundationVersionNumber);
}
