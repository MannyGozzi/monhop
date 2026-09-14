//! Current-user DPAPI protection. No machine-wide keys, prompts, or credential enumeration.

use std::{io, ptr};
use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
use zeroize::{Zeroize, Zeroizing};

const MAX_KEY_BYTES: usize = 4096;
const MAX_PROTECTED_BYTES: usize = 16384;
const ENTROPY: &[u8] = b"MonHop identity storage v1";

struct DpapiAllocation(CRYPT_INTEGER_BLOB);
impl Drop for DpapiAllocation {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            // SAFETY: DPAPI owns this writable allocation of cbData bytes until LocalFree.
            unsafe {
                std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                LocalFree(self.0.pbData.cast());
            }
        }
    }
}

pub fn protect_key(key: &[u8]) -> io::Result<Vec<u8>> {
    validate_len(key.len(), MAX_KEY_BYTES)?;
    let input = blob(key);
    let entropy = blob(ENTROPY);
    let mut output = DpapiAllocation(CRYPT_INTEGER_BLOB::default());
    // SAFETY: DPAPI treats inputs as read-only and initializes the output allocation on success.
    let result = unsafe {
        CryptProtectData(
            &input,
            windows_sys::core::w!("MonHop device identity"),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    validate_len(output.0.cbData as usize, MAX_PROTECTED_BYTES)?;
    if output.0.pbData.is_null() {
        return Err(io::Error::other("DPAPI returned no protected data"));
    }
    // SAFETY: validated non-null DPAPI result is borrowed only until it is copied.
    Ok(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec())
}

pub fn unprotect_key(protected: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    validate_len(protected.len(), MAX_PROTECTED_BYTES)?;
    let input = blob(protected);
    let entropy = blob(ENTROPY);
    let mut output = DpapiAllocation(CRYPT_INTEGER_BLOB::default());
    // SAFETY: buffers stay valid during this synchronous call; no optional description is requested.
    let result = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    validate_len(output.0.cbData as usize, MAX_KEY_BYTES)?;
    if output.0.pbData.is_null() {
        return Err(io::Error::other("DPAPI returned no key"));
    }
    // SAFETY: the validated result lives until the guarded DPAPI allocation is zeroized and freed.
    Ok(Zeroizing::new(
        unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec(),
    ))
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    }
}
fn validate_len(len: usize, max: usize) -> io::Result<()> {
    if len == 0 || len > max {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Identity buffer length rejected",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpapi_round_trip_and_tamper_rejection() {
        let test_bytes = b"non-secret unit-test data, never an actual identity";
        let mut encrypted = protect_key(test_bytes).unwrap();
        assert_ne!(encrypted.as_slice(), test_bytes);
        assert_eq!(unprotect_key(&encrypted).unwrap().as_slice(), test_bytes);
        let end = encrypted.len() - 1;
        encrypted[end] ^= 0x80;
        assert!(unprotect_key(&encrypted).is_err());
    }
    #[test]
    fn identity_sizes_are_bounded_before_os_calls() {
        assert!(protect_key(&[]).is_err());
        assert!(protect_key(&[0; MAX_KEY_BYTES + 1]).is_err());
        assert!(unprotect_key(&[0; MAX_PROTECTED_BYTES + 1]).is_err());
    }
}
