//! Read-only CoreWLAN attachment snapshots. The returned bytes are opaque and never logged.
//! They name the network, not the access point, so roaming within one network keeps them.

use std::{ffi::c_void, io, ptr};

use crate::cf_owned::{CFEqual, CFStringCreateWithCString, CFStringRef, CfOwned};
use crate::objc::{AutoreleasePool, Id, Objc, class, link_foundation, selector};

type CFDataRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const STATION_MODE: isize = 1;
const MAX_BSD_NAME_LEN: usize = 15;
const MAX_SSID_LEN: usize = 32;
const SIGNATURE_PREFIX: &[u8] = b"monhop/macos/wifi/2\0";

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

/// Reads a stable, current Wi-Fi SSID snapshot. It never prompts, scans, or changes Wi-Fi.
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
    link_foundation();
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
    let ssid = objc.send_id(interface, selector(c"ssidData"));
    if ssid.is_null() {
        return Ok(None);
    }
    Ok(ssid_bytes(ssid.cast()).and_then(|ssid| build_signature(&ssid)))
}

fn matches_requested_interface(objc: Objc, interface: Id, requested_name: CFStringRef) -> bool {
    let actual_name = objc.send_id(interface, selector(c"interfaceName"));
    if actual_name.is_null() {
        return false;
    }
    // SAFETY: CoreWLAN returns an NSString, toll-free bridged to CFString, and both values are live.
    unsafe { CFEqual(actual_name.cast(), requested_name) != 0 }
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

pub(crate) fn build_signature(ssid: &[u8]) -> Option<Vec<u8>> {
    if ssid.is_empty() || ssid.len() > MAX_SSID_LEN {
        return None;
    }
    let mut signature = Vec::with_capacity(SIGNATURE_PREFIX.len() + 1 + ssid.len());
    signature.extend_from_slice(SIGNATURE_PREFIX);
    signature.push(ssid.len() as u8);
    signature.extend_from_slice(ssid);
    Some(signature)
}

pub(crate) fn ssid_from_attachment(attachment: &[u8]) -> Option<&str> {
    let (&ssid_len, ssid) = attachment.strip_prefix(SIGNATURE_PREFIX)?.split_first()?;
    if !(1..=MAX_SSID_LEN).contains(&(ssid_len as usize)) || ssid.len() != ssid_len as usize {
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
    fn signature_is_versioned_and_bounded() {
        let signature = build_signature(&[7; MAX_SSID_LEN]).unwrap();
        assert!(signature.starts_with(SIGNATURE_PREFIX));
        assert!(signature.len() <= 128);
        assert!(build_signature(&[]).is_none());
        assert!(build_signature(&[7; MAX_SSID_LEN + 1]).is_none());
    }

    #[test]
    fn extracts_short_and_max_length_utf8_ssids() {
        let short = build_signature(b"studio").unwrap();
        assert_eq!(ssid_from_attachment(&short), Some("studio"));

        let max = build_signature(&[b'x'; MAX_SSID_LEN]).unwrap();
        assert_eq!(
            ssid_from_attachment(&max),
            Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );
    }

    #[test]
    fn rejects_truncated_wrong_length_and_non_utf8_ssids() {
        let signature = build_signature(b"studio").unwrap();
        assert!(ssid_from_attachment(&signature[..signature.len() - 1]).is_none());

        let mut wrong_length = signature;
        wrong_length[SIGNATURE_PREFIX.len()] = 5;
        assert!(ssid_from_attachment(&wrong_length).is_none());
        wrong_length[SIGNATURE_PREFIX.len()] = 0;
        assert!(ssid_from_attachment(&wrong_length).is_none());

        let mut overlong = build_signature(&[b'x'; MAX_SSID_LEN]).unwrap();
        overlong[SIGNATURE_PREFIX.len()] = (MAX_SSID_LEN + 1) as u8;
        assert!(ssid_from_attachment(&overlong).is_none());

        let non_utf8 = build_signature(&[0xff]).unwrap();
        assert!(ssid_from_attachment(&non_utf8).is_none());
    }

    #[test]
    fn inconsistent_repeated_snapshot_is_unknown() {
        let first = build_signature(b"one");
        let second = build_signature(b"two");
        assert!(consistent_signature(first, second).is_none());
        let stable = build_signature(b"same");
        assert!(consistent_signature(stable.clone(), stable).is_some());
    }
}
