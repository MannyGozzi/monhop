//! Explicit-action native adapters. No storage is accessed by construction.

use crate::identity_store::{ProtectedIdentityStore, ProtectedPeerStore, StorageError, StoredPeer};
use zeroize::Zeroizing;

pub struct NativeIdentityStore;

pub struct NativePeerStore;

#[cfg(target_os = "macos")]
impl ProtectedPeerStore for NativePeerStore {
    fn list(&self) -> Result<Vec<StoredPeer>, StorageError> {
        use monhop_platform_macos::identity;
        let peers = identity::list_peers_after_user_action()
            .map_err(|error| map_keychain_error(error, StorageError::ReadFailed))?;
        Ok(peers
            .into_iter()
            .map(|(handle, record)| StoredPeer {
                handle,
                record: record.into_zeroizing(),
            })
            .collect())
    }

    fn create_new(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        use monhop_platform_macos::identity::{self, ProtectedRecord};
        let record = ProtectedRecord::from_zeroizing(Zeroizing::new(bytes.to_vec()))
            .map_err(|_| StorageError::InvalidRecord)?;
        identity::create_peer_after_user_action(key, record)
            .map_err(|error| map_keychain_error(error, StorageError::WriteFailed))
    }

    fn delete(&self, handle: &str) -> Result<(), StorageError> {
        monhop_platform_macos::identity::delete_peer_after_user_action(handle)
            .map_err(|error| map_keychain_error(error, StorageError::WriteFailed))
    }
}

#[cfg(windows)]
impl ProtectedPeerStore for NativePeerStore {
    fn list(&self) -> Result<Vec<StoredPeer>, StorageError> {
        monhop_platform_windows::identity_storage::list_peers_after_user_action()
            .map(|peers| {
                peers
                    .into_iter()
                    .map(|(handle, record)| StoredPeer { handle, record })
                    .collect()
            })
            .map_err(|error| map_file_error(error.kind(), StorageError::ReadFailed))
    }

    fn create_new(&self, key: &str, record: &[u8]) -> Result<(), StorageError> {
        monhop_platform_windows::identity_storage::create_peer_after_user_action(key, record)
            .map_err(|error| map_file_error(error.kind(), StorageError::WriteFailed))
    }

    fn delete(&self, handle: &str) -> Result<(), StorageError> {
        monhop_platform_windows::identity_storage::delete_peer_after_user_action(handle)
            .map_err(|error| map_file_error(error.kind(), StorageError::WriteFailed))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl ProtectedPeerStore for NativePeerStore {
    fn list(&self) -> Result<Vec<StoredPeer>, StorageError> {
        Err(StorageError::Unavailable)
    }
    fn create_new(&self, _: &str, _: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
    fn delete(&self, _: &str) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
}

#[cfg(target_os = "macos")]
impl ProtectedIdentityStore for NativeIdentityStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError> {
        use monhop_platform_macos::identity::{self, IdentitySlot, IdentityStoreError};
        match identity::load_after_user_action(IdentitySlot::LocalIdentity) {
            Ok(record) => Ok(Some(record.into_zeroizing())),
            Err(IdentityStoreError::RecordNotFound) => Ok(None),
            Err(error) => Err(map_keychain_error(error, StorageError::ReadFailed)),
        }
    }

    fn create_new(&self, record: &[u8]) -> Result<(), StorageError> {
        use monhop_platform_macos::identity::{self, IdentitySlot, ProtectedRecord};
        let record = ProtectedRecord::from_zeroizing(Zeroizing::new(record.to_vec()))
            .map_err(|_| StorageError::InvalidRecord)?;
        identity::create_after_user_action(IdentitySlot::LocalIdentity, record)
            .map_err(|error| map_keychain_error(error, StorageError::WriteFailed))
    }
}

#[cfg(target_os = "macos")]
fn map_keychain_error(
    error: monhop_platform_macos::identity::IdentityStoreError,
    fallback: StorageError,
) -> StorageError {
    use monhop_platform_macos::identity::IdentityStoreError;
    match error {
        IdentityStoreError::RecordAlreadyExists => StorageError::AlreadyExists,
        IdentityStoreError::KeychainLocked => StorageError::Locked,
        IdentityStoreError::KeychainDenied => StorageError::AccessDenied,
        IdentityStoreError::KeychainUnavailable => StorageError::Unavailable,
        IdentityStoreError::InvalidPayloadLength
        | IdentityStoreError::InvalidRecordName
        | IdentityStoreError::InvalidKeychainResponse => StorageError::InvalidRecord,
        IdentityStoreError::RecordNotFound | IdentityStoreError::KeychainFailure(_) => fallback,
    }
}

#[cfg(windows)]
impl ProtectedIdentityStore for NativeIdentityStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError> {
        monhop_platform_windows::identity_storage::load_after_user_action()
            .map_err(|error| map_file_error(error.kind(), StorageError::ReadFailed))
    }

    fn create_new(&self, record: &[u8]) -> Result<(), StorageError> {
        monhop_platform_windows::identity_storage::create_after_user_action(record)
            .map_err(|error| map_file_error(error.kind(), StorageError::WriteFailed))
    }
}

#[cfg(any(windows, test))]
fn map_file_error(kind: std::io::ErrorKind, fallback: StorageError) -> StorageError {
    match kind {
        std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => StorageError::AccessDenied,
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => {
            StorageError::InvalidRecord
        }
        _ => fallback,
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
impl ProtectedIdentityStore for NativeIdentityStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn create_new(&self, _: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_failures_never_become_missing_records() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::Other,
            ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(
                map_file_error(kind, StorageError::ReadFailed),
                StorageError::ReadFailed
            );
        }
        assert_eq!(
            map_file_error(ErrorKind::PermissionDenied, StorageError::ReadFailed),
            StorageError::AccessDenied
        );
        assert_eq!(
            map_file_error(ErrorKind::AlreadyExists, StorageError::WriteFailed),
            StorageError::AlreadyExists
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_denial_locked_and_unavailable_stay_errors() {
        use monhop_platform_macos::identity::IdentityStoreError;
        for (native, expected) in [
            (
                IdentityStoreError::KeychainDenied,
                StorageError::AccessDenied,
            ),
            (IdentityStoreError::KeychainLocked, StorageError::Locked),
            (
                IdentityStoreError::KeychainUnavailable,
                StorageError::Unavailable,
            ),
            (IdentityStoreError::RecordNotFound, StorageError::ReadFailed),
        ] {
            assert_eq!(
                map_keychain_error(native, StorageError::ReadFailed),
                expected
            );
        }
    }
}
