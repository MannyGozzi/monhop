//! Read-only CoreWLAN attachment snapshots. The returned bytes are opaque and never logged.

use std::{
    ffi::{c_char, c_void},
    io, ptr,
};

use crate::cf_owned::{
    CFEqual, CFStringCreateWithCString, CFStringGetCString, CFStringRef, CfOwned,
};

type Id = *mut c_void;
type Sel = *mut c_void;
type CFDataRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const STATION_MODE: isize = 1;
const MAX_BSD_NAME_LEN: usize = 15;
const MAX_SSID_LEN: usize = 32;
const SIGNATURE_PREFIX: &[u8] = b"monhop/macos/wifi/1\0";

// SAFETY: These declarations match CoreFoundation and the documented Objective-C runtime entry points.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetLength(value: CFDataRef) -> isize;
    fn CFDataGetBytePtr(value: CFDataRef) -> *const u8;
}

// SAFETY: The CoreWLAN link anchor loads CWWiFiClient without querying network metadata.
#[link(name = "CoreWLAN", kind = "framework")]
unsafe extern "C" {
    static CWErrorDomain: Id;
}

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
struct Objc {
    message_send: *mut c_void,
}

impl Objc {
    fn load() -> io::Result<Self> {
        const RTLD_DEFAULT: Id = (-2_isize) as Id;
        // SAFETY: RTLD_DEFAULT and the static symbol name are valid inputs to dlsym.
        let message_send = unsafe { dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr()) };
        if message_send.is_null() {
            return Err(io::Error::other("Objective-C runtime unavailable"));
        }
        Ok(Self { message_send })
    }

    fn send_id(&self, receiver: Id, selector: Sel) -> Id {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact result ABI.
        let call: unsafe extern "C" fn(Id, Sel) -> Id =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and a no-argument selector.
        unsafe { call(receiver, selector) }
    }

    fn send_id_id(&self, receiver: Id, selector: Sel, value: Id) -> Id {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact result ABI.
        let call: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver, selector, and object argument.
        unsafe { call(receiver, selector, value) }
    }

    fn send_isize(&self, receiver: Id, selector: Sel) -> isize {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact integer ABI.
        let call: unsafe extern "C" fn(Id, Sel) -> isize =
            unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and integer-returning selector.
        unsafe { call(receiver, selector) }
    }

    fn send_void(&self, receiver: Id, selector: Sel) {
        // SAFETY: load resolved objc_msgSend, and this private wrapper uses its exact void ABI.
        let call: unsafe extern "C" fn(Id, Sel) = unsafe { std::mem::transmute(self.message_send) };
        // SAFETY: all internal callers provide an Objective-C receiver and void-returning selector.
        unsafe { call(receiver, selector) };
    }
}

struct AutoreleasePool {
    objc: Objc,
    value: Id,
}

impl AutoreleasePool {
    fn new(objc: Objc) -> io::Result<Self> {
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

/// Reads a stable, current Wi-Fi BSSID/SSID snapshot. It never prompts, scans, or changes Wi-Fi.
pub fn read_attachment(bsd_name: &str) -> io::Result<Option<Vec<u8>>> {
    if !is_valid_bsd_name(bsd_name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid Wi-Fi interface name",
        ));
    }
    framework_link_anchors();
    let objc = Objc::load()?;
    let _pool = AutoreleasePool::new(objc)?;
    let interface_name = cf_string(bsd_name)?;
    let client = objc.send_id(
        class(c"CWWiFiClient", "CoreWLAN unavailable")?,
        selector(c"sharedWiFiClient"),
    );
    if client.is_null() {
        return Err(io::Error::other("CoreWLAN unavailable"));
    }
    let interface = objc.send_id_id(
        client,
        selector(c"interfaceWithName:"),
        interface_name.0.cast_mut(),
    );
    if interface.is_null() {
        return Ok(None);
    }

    let first = read_snapshot(objc, interface, interface_name.0)?;
    let second = read_snapshot(objc, interface, interface_name.0)?;
    Ok(consistent_signature(first, second))
}

fn framework_link_anchors() {
    // Optimization barriers preserve both framework references without reading their state.
    std::hint::black_box(&raw const CWErrorDomain);
    std::hint::black_box(&raw const NSFoundationVersionNumber);
}

fn read_snapshot(
    objc: Objc,
    interface: Id,
    requested_name: CFStringRef,
) -> io::Result<Option<Vec<u8>>> {
    if !matches_requested_interface(objc, interface, requested_name) {
        return Ok(None);
    }
    if objc.send_isize(interface, selector(c"interfaceMode")) != STATION_MODE {
        return Ok(None);
    }
    let bssid = objc.send_id(interface, selector(c"bssid"));
    let ssid = objc.send_id(interface, selector(c"ssidData"));
    if bssid.is_null() || ssid.is_null() {
        return Ok(None);
    }
    let Some(bssid) = bssid_bytes(bssid.cast()) else {
        return Ok(None);
    };
    let Some(ssid) = ssid_bytes(ssid.cast()) else {
        return Ok(None);
    };
    Ok(build_signature(bssid, &ssid))
}

fn matches_requested_interface(objc: Objc, interface: Id, requested_name: CFStringRef) -> bool {
    let actual_name = objc.send_id(interface, selector(c"interfaceName"));
    if actual_name.is_null() {
        return false;
    }
    // SAFETY: CoreWLAN returns an NSString, toll-free bridged to CFString, and both values are live.
    unsafe { CFEqual(actual_name.cast(), requested_name) != 0 }
}

fn bssid_bytes(value: CFStringRef) -> Option<[u8; 6]> {
    let mut text = [0_u8; 18];
    // SAFETY: CWWiFiClient returned a CFString and text is a writable UTF-8 buffer of this size.
    let converted = unsafe {
        CFStringGetCString(
            value,
            text.as_mut_ptr().cast(),
            text.len() as isize,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if converted == 0 {
        return None;
    }
    let length = text.iter().position(|byte| *byte == 0)?;
    parse_bssid(&text[..length])
}

fn ssid_bytes(value: CFDataRef) -> Option<Vec<u8>> {
    // SAFETY: CWWiFiClient returned a live CFData object for this synchronous snapshot.
    let length = unsafe { CFDataGetLength(value) };
    if !(1..=MAX_SSID_LEN as isize).contains(&length) {
        return None;
    }
    // SAFETY: the same live CFData object owns the bytes until this function returns.
    let value = unsafe { CFDataGetBytePtr(value) };
    if value.is_null() {
        return None;
    }
    // SAFETY: CFData reports this bounded byte length for its non-null owned buffer.
    let bytes = unsafe { std::slice::from_raw_parts(value, length as usize) };
    Some(bytes.to_vec())
}

fn is_valid_bsd_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_BSD_NAME_LEN
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn parse_bssid(value: &[u8]) -> Option<[u8; 6]> {
    if value.len() != 17 || [2, 5, 8, 11, 14].iter().any(|index| value[*index] != b':') {
        return None;
    }
    let mut result = [0_u8; 6];
    for (index, octet) in result.iter_mut().enumerate() {
        let offset = index * 3;
        *octet = hex(value[offset])?
            .checked_mul(16)?
            .checked_add(hex(value[offset + 1])?)?;
    }
    (result != [0; 6] && result[0] & 1 == 0).then_some(result)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn build_signature(bssid: [u8; 6], ssid: &[u8]) -> Option<Vec<u8>> {
    if ssid.is_empty() || ssid.len() > MAX_SSID_LEN || bssid == [0; 6] || bssid[0] & 1 != 0 {
        return None;
    }
    let mut signature = Vec::with_capacity(SIGNATURE_PREFIX.len() + bssid.len() + 1 + ssid.len());
    signature.extend_from_slice(SIGNATURE_PREFIX);
    signature.extend_from_slice(&bssid);
    signature.push(ssid.len() as u8);
    signature.extend_from_slice(ssid);
    Some(signature)
}

pub(crate) fn ssid_from_attachment(attachment: &[u8]) -> Option<&str> {
    let body = attachment.strip_prefix(SIGNATURE_PREFIX)?;
    if body.len() < 7 {
        return None;
    }
    let bssid = &body[..6];
    let ssid_len = body[6] as usize;
    let ssid = &body[7..];
    if bssid == [0; 6]
        || bssid[0] & 1 != 0
        || !(1..=MAX_SSID_LEN).contains(&ssid_len)
        || ssid.len() != ssid_len
    {
        return None;
    }
    std::str::from_utf8(ssid).ok()
}

fn consistent_signature(first: Option<Vec<u8>>, second: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match (first, second) {
        (Some(first), Some(second)) if first == second => Some(first),
        _ => None,
    }
}

fn class(name: &'static std::ffi::CStr, unavailable: &'static str) -> io::Result<Id> {
    // SAFETY: name is a static NUL-terminated Objective-C class name.
    let class = unsafe { objc_getClass(name.as_ptr()) };
    if class.is_null() {
        Err(io::Error::other(unavailable))
    } else {
        Ok(class)
    }
}

fn selector(name: &'static std::ffi::CStr) -> Sel {
    // SAFETY: name is a static NUL-terminated selector name.
    unsafe { sel_registerName(name.as_ptr()) }
}

fn cf_string(value: &str) -> io::Result<CfOwned> {
    let value = std::ffi::CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid Wi-Fi interface name"))?;
    // SAFETY: the default allocator and this NUL-terminated UTF-8 string satisfy CoreFoundation.
    let value =
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8) };
    if value.is_null() {
        Err(io::Error::other("CoreFoundation allocation failed"))
    } else {
        Ok(CfOwned(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_bounded_alphanumeric_bsd_names() {
        assert!(is_valid_bsd_name("en0"));
        assert!(is_valid_bsd_name("awdl1234567890"));
        assert!(!is_valid_bsd_name(""));
        assert!(!is_valid_bsd_name("en0.1"));
        assert!(!is_valid_bsd_name("interface-name-too-long"));
    }

    #[test]
    fn parses_only_unicast_nonzero_bssids() {
        assert!(parse_bssid(b"02:ab:cd:12:34:56").is_some());
        assert!(parse_bssid(b"00:00:00:00:00:00").is_none());
        assert!(parse_bssid(b"03:ab:cd:12:34:56").is_none());
        assert!(parse_bssid(b"02:ab:cd:12:34:5x").is_none());
    }

    #[test]
    fn signature_is_versioned_and_bounded() {
        let signature = build_signature([2, 1, 2, 3, 4, 5], &[7; MAX_SSID_LEN]).unwrap();
        assert!(signature.starts_with(SIGNATURE_PREFIX));
        assert!(signature.len() <= 128);
        assert!(build_signature([2, 1, 2, 3, 4, 5], &[]).is_none());
        assert!(build_signature([2, 1, 2, 3, 4, 5], &[7; MAX_SSID_LEN + 1]).is_none());
    }

    #[test]
    fn extracts_short_and_max_length_utf8_ssids() {
        let short = build_signature([2, 1, 2, 3, 4, 5], b"studio").unwrap();
        assert_eq!(ssid_from_attachment(&short), Some("studio"));

        let max = build_signature([2, 1, 2, 3, 4, 5], &[b'x'; MAX_SSID_LEN]).unwrap();
        assert_eq!(
            ssid_from_attachment(&max),
            Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );
    }

    #[test]
    fn rejects_truncated_wrong_length_and_non_utf8_ssids() {
        let signature = build_signature([2, 1, 2, 3, 4, 5], b"studio").unwrap();
        assert!(ssid_from_attachment(&signature[..signature.len() - 1]).is_none());

        let mut wrong_length = signature;
        wrong_length[SIGNATURE_PREFIX.len() + 6] = 5;
        assert!(ssid_from_attachment(&wrong_length).is_none());
        wrong_length[SIGNATURE_PREFIX.len() + 6] = 0;
        assert!(ssid_from_attachment(&wrong_length).is_none());

        let mut overlong = build_signature([2, 1, 2, 3, 4, 5], &[b'x'; MAX_SSID_LEN]).unwrap();
        overlong[SIGNATURE_PREFIX.len() + 6] = (MAX_SSID_LEN + 1) as u8;
        assert!(ssid_from_attachment(&overlong).is_none());

        let non_utf8 = build_signature([2, 1, 2, 3, 4, 5], &[0xff]).unwrap();
        assert!(ssid_from_attachment(&non_utf8).is_none());
    }

    #[test]
    fn inconsistent_repeated_snapshot_is_unknown() {
        let first = build_signature([2, 1, 2, 3, 4, 5], b"one");
        let second = build_signature([2, 1, 2, 3, 4, 5], b"two");
        assert!(consistent_signature(first, second).is_none());
        let stable = build_signature([2, 1, 2, 3, 4, 5], b"same");
        assert!(consistent_signature(stable.clone(), stable).is_some());
    }
}
