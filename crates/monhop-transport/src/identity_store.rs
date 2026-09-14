//! Versioned local identity records for platform-protected stores.
//!
//! Platform adapters own Keychain or DPAPI access. This module only defines a
//! bounded record format and never touches files, native APIs, or the network.

use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{CryptoError, DeviceIdentity, private_key_der_for_protected_storage};

const RECORD_MAGIC: [u8; 4] = *b"LKMI";
const RECORD_VERSION: u8 = 1;
const HEADER_LENGTH: usize = 13;

/// Maximum protected-store plaintext accepted for one complete identity record.
pub const MAX_IDENTITY_RECORD_LENGTH: usize = 4 * 1024;

/// A provider failure or fail-closed identity-record validation error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    ReadFailed,
    WriteFailed,
    AccessDenied,
    Locked,
    Unavailable,
    AlreadyExists,
    InvalidRecord,
    UnsupportedRecordVersion,
    ReadbackMissing,
    ReadbackMismatch,
    Identity(CryptoError),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadFailed => "protected identity storage could not be read",
            Self::WriteFailed => "protected identity storage could not be written",
            Self::AccessDenied => "OS-protected identity access was denied or canceled",
            Self::Locked => "OS-protected identity storage is locked",
            Self::Unavailable => "OS-protected identity storage is unavailable",
            Self::AlreadyExists => "a protected identity record already exists",
            Self::InvalidRecord => "protected identity record is invalid",
            Self::UnsupportedRecordVersion => "protected identity record version is unsupported",
            Self::ReadbackMissing => "protected identity record was missing after creation",
            Self::ReadbackMismatch => {
                "protected identity readback did not match the created identity"
            }
            Self::Identity(_) => "protected identity record could not be validated",
        })
    }
}

impl std::error::Error for StorageError {}

/// OS-protected local storage for the complete identity record plaintext.
///
/// Providers must protect every supplied byte with the platform facility and
/// atomically reject `create_new` when any record already exists.
pub trait ProtectedIdentityStore {
    fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError>;

    fn create_new(&self, record: &[u8]) -> Result<(), StorageError>;
}

/// One confirmed-peer record with the store's own name for it. The handle is only ever handed
/// back to `delete`; which computer a record belongs to is read from its content.
pub struct StoredPeer {
    pub handle: String,
    pub record: Zeroizing<Vec<u8>>,
}

/// OS-protected storage holding one record per paired computer.
///
/// Providers protect every byte with the platform facility, never replace a record, and reject
/// `create_new` for a key that already has one. `key` is the peer's lowercase 64-hex fingerprint.
pub trait ProtectedPeerStore {
    fn list(&self) -> Result<Vec<StoredPeer>, StorageError>;

    fn create_new(&self, key: &str, record: &[u8]) -> Result<(), StorageError>;

    fn delete(&self, handle: &str) -> Result<(), StorageError>;
}

/// Explicitly load and validate an existing identity without generating one.
pub fn load_identity(
    store: &impl ProtectedIdentityStore,
) -> Result<Option<DeviceIdentity>, StorageError> {
    let Some(record) = store.load()? else {
        return Ok(None);
    };
    decode_identity_record(record.as_slice()).map(Some)
}

/// Explicitly generate, persist, and read back a new identity.
///
/// This never replaces a record, repairs invalid storage, or falls back to a
/// generated identity after any storage error.
pub fn create_identity(
    store: &impl ProtectedIdentityStore,
) -> Result<DeviceIdentity, StorageError> {
    if load_identity(store)?.is_some() {
        return Err(StorageError::AlreadyExists);
    }

    let identity = DeviceIdentity::generate().map_err(StorageError::Identity)?;
    let mut record = encode_identity_record(&identity)?;
    let creation = store.create_new(record.as_slice());
    record.zeroize();
    creation?;

    let persisted = load_identity(store)?.ok_or(StorageError::ReadbackMissing)?;
    if persisted.fingerprint() != identity.fingerprint() {
        return Err(StorageError::ReadbackMismatch);
    }
    Ok(identity)
}

fn encode_identity_record(identity: &DeviceIdentity) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    encode_identity_parts(
        identity.certificate_der(),
        private_key_der_for_protected_storage(identity),
    )
}

fn encode_identity_parts(
    certificate_der: &[u8],
    private_key_der: &[u8],
) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    if certificate_der.is_empty() || private_key_der.is_empty() {
        return Err(StorageError::InvalidRecord);
    }
    let total_length = HEADER_LENGTH
        .checked_add(certificate_der.len())
        .and_then(|length| length.checked_add(private_key_der.len()))
        .filter(|length| *length <= MAX_IDENTITY_RECORD_LENGTH)
        .ok_or(StorageError::InvalidRecord)?;
    let certificate_length =
        u32::try_from(certificate_der.len()).map_err(|_| StorageError::InvalidRecord)?;
    let private_key_length =
        u32::try_from(private_key_der.len()).map_err(|_| StorageError::InvalidRecord)?;

    let mut record = Zeroizing::new(Vec::with_capacity(total_length));
    record.extend_from_slice(&RECORD_MAGIC);
    record.push(RECORD_VERSION);
    record.extend_from_slice(&certificate_length.to_be_bytes());
    record.extend_from_slice(&private_key_length.to_be_bytes());
    record.extend_from_slice(certificate_der);
    record.extend_from_slice(private_key_der);
    Ok(record)
}

fn decode_identity_record(record: &[u8]) -> Result<DeviceIdentity, StorageError> {
    if record.len() < HEADER_LENGTH || record.len() > MAX_IDENTITY_RECORD_LENGTH {
        return Err(StorageError::InvalidRecord);
    }
    if record[..RECORD_MAGIC.len()] != RECORD_MAGIC {
        return Err(StorageError::InvalidRecord);
    }
    if record[RECORD_MAGIC.len()] != RECORD_VERSION {
        return Err(StorageError::UnsupportedRecordVersion);
    }

    let mut cursor = RECORD_MAGIC.len() + 1;
    let certificate_length = read_length(record, &mut cursor)?;
    let private_key_length = read_length(record, &mut cursor)?;
    if certificate_length == 0 || private_key_length == 0 {
        return Err(StorageError::InvalidRecord);
    }
    let certificate_der = read_record_part(record, &mut cursor, certificate_length)?;
    let private_key_der = read_record_part(record, &mut cursor, private_key_length)?;
    if cursor != record.len() {
        return Err(StorageError::InvalidRecord);
    }

    DeviceIdentity::from_pkcs8_and_certificate(private_key_der, certificate_der)
        .map_err(StorageError::Identity)
}

fn read_length(record: &[u8], cursor: &mut usize) -> Result<usize, StorageError> {
    let bytes = read_record_part(record, cursor, 4)?;
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| StorageError::InvalidRecord)?;
    usize::try_from(u32::from_be_bytes(bytes)).map_err(|_| StorageError::InvalidRecord)
}

fn read_record_part<'a>(
    record: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], StorageError> {
    let end = cursor
        .checked_add(length)
        .filter(|end| *end <= record.len())
        .ok_or(StorageError::InvalidRecord)?;
    let part = &record[*cursor..end];
    *cursor = end;
    Ok(part)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use zeroize::Zeroizing;

    use super::{
        HEADER_LENGTH, MAX_IDENTITY_RECORD_LENGTH, ProtectedIdentityStore, StorageError,
        create_identity, decode_identity_record, encode_identity_parts, load_identity,
    };
    use crate::crypto::{CryptoError, DeviceIdentity, private_key_der_for_protected_storage};

    struct MemoryStore {
        record: RefCell<Option<Zeroizing<Vec<u8>>>>,
        create_calls: Cell<usize>,
        load_calls: Cell<usize>,
        fail_load_call: Cell<Option<(usize, StorageError)>>,
        create_error: Cell<Option<StorageError>>,
        replacement_after_create: RefCell<Option<Zeroizing<Vec<u8>>>>,
    }

    impl MemoryStore {
        fn empty() -> Self {
            Self {
                record: RefCell::new(None),
                create_calls: Cell::new(0),
                load_calls: Cell::new(0),
                fail_load_call: Cell::new(None),
                create_error: Cell::new(None),
                replacement_after_create: RefCell::new(None),
            }
        }

        fn with_record(record: Zeroizing<Vec<u8>>) -> Self {
            let store = Self::empty();
            *store.record.borrow_mut() = Some(record);
            store
        }
    }

    impl ProtectedIdentityStore for MemoryStore {
        fn load(&self) -> Result<Option<Zeroizing<Vec<u8>>>, StorageError> {
            let call = self.load_calls.get().saturating_add(1);
            self.load_calls.set(call);
            if let Some((failed_call, error)) = self.fail_load_call.get()
                && call == failed_call
            {
                return Err(error);
            }
            Ok(self
                .record
                .borrow()
                .as_ref()
                .map(|record| Zeroizing::new(record.to_vec())))
        }

        fn create_new(&self, record: &[u8]) -> Result<(), StorageError> {
            self.create_calls
                .set(self.create_calls.get().saturating_add(1));
            if let Some(error) = self.create_error.get() {
                return Err(error);
            }
            if self.record.borrow().is_some() {
                return Err(StorageError::AlreadyExists);
            }

            let replacement = self.replacement_after_create.borrow_mut().take();
            let record = replacement.unwrap_or_else(|| Zeroizing::new(record.to_vec()));
            *self.record.borrow_mut() = Some(record);
            Ok(())
        }
    }

    #[test]
    fn restart_load_returns_the_same_fingerprint() {
        let store = MemoryStore::empty();
        let created = create_identity(&store).expect("create identity");
        let reloaded = load_identity(&store)
            .expect("load identity")
            .expect("stored identity");

        assert!(created.fingerprint() == reloaded.fingerprint());
    }

    #[test]
    fn mismatched_private_key_and_certificate_are_rejected() {
        let certificate_identity = DeviceIdentity::generate().expect("certificate identity");
        let key_identity = DeviceIdentity::generate().expect("key identity");
        let record = encode_identity_parts(
            certificate_identity.certificate_der(),
            private_key_der_for_protected_storage(&key_identity),
        )
        .expect("record");
        let store = MemoryStore::with_record(record);

        assert!(matches!(
            load_identity(&store),
            Err(StorageError::Identity(CryptoError::KeyCertificateMismatch))
        ));
    }

    #[test]
    fn malformed_truncated_unknown_version_and_trailing_records_are_rejected() {
        let identity = DeviceIdentity::generate().expect("identity");
        let record = encode_identity_parts(
            identity.certificate_der(),
            private_key_der_for_protected_storage(&identity),
        )
        .expect("record");

        for malformed in [
            Zeroizing::new(Vec::new()),
            Zeroizing::new(vec![0; HEADER_LENGTH]),
            Zeroizing::new(record[..record.len() - 1].to_vec()),
            {
                let mut trailing = Zeroizing::new(record.to_vec());
                trailing.push(0);
                trailing
            },
        ] {
            let store = MemoryStore::with_record(malformed);
            assert!(matches!(
                load_identity(&store),
                Err(StorageError::InvalidRecord)
            ));
        }

        let mut unknown_version = Zeroizing::new(record.to_vec());
        unknown_version[4] = 2;
        let store = MemoryStore::with_record(unknown_version);
        assert!(matches!(
            load_identity(&store),
            Err(StorageError::UnsupportedRecordVersion)
        ));
    }

    #[test]
    fn corrupt_storage_is_not_replaced_with_a_generated_identity() {
        let store = MemoryStore::with_record(Zeroizing::new(vec![0; HEADER_LENGTH]));

        assert!(matches!(
            create_identity(&store),
            Err(StorageError::InvalidRecord)
        ));
        assert_eq!(store.create_calls.get(), 0);
    }

    #[test]
    fn duplicate_creation_refuses_existing_records() {
        let store = MemoryStore::empty();
        let first = create_identity(&store).expect("first identity");

        assert!(matches!(
            create_identity(&store),
            Err(StorageError::AlreadyExists)
        ));
        let reloaded = load_identity(&store)
            .expect("load identity")
            .expect("stored identity");
        assert!(reloaded.fingerprint() == first.fingerprint());
    }

    #[test]
    fn write_and_readback_failures_are_returned_without_recovery() {
        let write_failure = MemoryStore::empty();
        write_failure
            .create_error
            .set(Some(StorageError::WriteFailed));
        assert!(matches!(
            create_identity(&write_failure),
            Err(StorageError::WriteFailed)
        ));
        assert_eq!(write_failure.create_calls.get(), 1);

        let readback_failure = MemoryStore::empty();
        readback_failure
            .fail_load_call
            .set(Some((2, StorageError::ReadFailed)));
        assert!(matches!(
            create_identity(&readback_failure),
            Err(StorageError::ReadFailed)
        ));
        assert_eq!(readback_failure.create_calls.get(), 1);
    }

    #[test]
    fn readback_mismatch_is_rejected() {
        let replacement_identity = DeviceIdentity::generate().expect("replacement identity");
        let replacement = encode_identity_parts(
            replacement_identity.certificate_der(),
            private_key_der_for_protected_storage(&replacement_identity),
        )
        .expect("replacement record");
        let store = MemoryStore::empty();
        *store.replacement_after_create.borrow_mut() = Some(replacement);

        assert!(matches!(
            create_identity(&store),
            Err(StorageError::ReadbackMismatch)
        ));
    }

    #[test]
    fn record_payload_limit_is_exact() {
        let private_key_at_limit =
            Zeroizing::new(vec![0; MAX_IDENTITY_RECORD_LENGTH - HEADER_LENGTH - 1]);
        let record =
            encode_identity_parts(&[1], private_key_at_limit.as_slice()).expect("record at limit");
        assert_eq!(record.len(), MAX_IDENTITY_RECORD_LENGTH);

        let private_key_over_limit =
            Zeroizing::new(vec![0; MAX_IDENTITY_RECORD_LENGTH - HEADER_LENGTH]);
        assert!(matches!(
            encode_identity_parts(&[1], private_key_over_limit.as_slice()),
            Err(StorageError::InvalidRecord)
        ));
    }

    #[test]
    fn duplicate_provider_create_new_refuses_without_replacement() {
        let store = MemoryStore::empty();
        let first = Zeroizing::new(vec![1]);
        store.create_new(first.as_slice()).expect("first record");
        let second = Zeroizing::new(vec![2]);

        assert!(matches!(
            store.create_new(second.as_slice()),
            Err(StorageError::AlreadyExists)
        ));
        let stored = store.load().expect("stored record").expect("record");
        assert!(stored.as_slice() == first.as_slice());
    }

    #[test]
    fn provider_read_error_is_not_treated_as_an_absent_identity() {
        let store = MemoryStore::empty();
        store
            .fail_load_call
            .set(Some((1, StorageError::ReadFailed)));

        assert!(matches!(
            create_identity(&store),
            Err(StorageError::ReadFailed)
        ));
        assert_eq!(store.create_calls.get(), 0);
    }

    #[test]
    fn decoder_rejects_empty_fields() {
        let mut record = Zeroizing::new(Vec::with_capacity(HEADER_LENGTH));
        record.extend_from_slice(b"LKMI");
        record.push(1);
        record.extend_from_slice(&0_u32.to_be_bytes());
        record.extend_from_slice(&0_u32.to_be_bytes());

        assert!(matches!(
            decode_identity_record(record.as_slice()),
            Err(StorageError::InvalidRecord)
        ));
    }
}
