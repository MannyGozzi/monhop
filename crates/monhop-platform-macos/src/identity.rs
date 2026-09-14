//! Explicit-action storage for MonHop's identity record and its confirmed-peer records.
//!
//! This module uses the current user's file-based default Keychain. Calls may
//! show Apple's Keychain dialog and must never run during startup or diagnostics.

use std::{error::Error, ffi::c_void, fmt, ptr, sync::Arc};

use core_foundation::{
    array::{CFArray, CFArrayRef},
    base::{CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::CFMutableDictionary,
    string::{CFString, CFStringRef},
};
use security_framework_sys::{
    base::{
        SecAccessRef, SecKeychainAttribute, SecKeychainAttributeList, SecKeychainItemRef,
        errSecAuthFailed as ERR_SEC_AUTH_FAILED, errSecDuplicateItem as ERR_SEC_DUPLICATE_ITEM,
        errSecItemNotFound as ERR_SEC_ITEM_NOT_FOUND, errSecParam as ERR_SEC_PARAM, errSecSuccess,
    },
    item::{
        kSecAttrAccount, kSecAttrService, kSecAttrSynchronizable, kSecClass,
        kSecClassGenericPassword, kSecMatchSearchList, kSecReturnData, kSecReturnRef,
        kSecUseDataProtectionKeychain, kSecUseKeychain, kSecValueData,
    },
    keychain::SecKeychainCopyDefault,
    keychain_item::{SecItemAdd, SecItemCopyMatching, SecKeychainItemDelete},
};
use zeroize::Zeroizing;

const SERVICE: &str = "com.manuelgozzi.monhop";
/// The name Apple's Keychain dialog shows for these records.
const ACCESS_DESCRIPTOR: &str = "MonHop";
const MAX_RECORD_BYTES: usize = 4096;
/// Accounts of the confirmed-peer records created since several computers could be paired.
const PEER_ACCOUNT_PREFIX: &str = "ConfirmedPeer/";
const PEER_KEY_LENGTH: usize = 64;
const ERR_SEC_USER_CANCELED: i32 = -128;
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;
const ERR_SEC_RESTRICTED_API: i32 = -34020;
const ERR_SEC_NOT_AVAILABLE: i32 = -25291;
const ERR_SEC_NO_DEFAULT_KEYCHAIN: i32 = -25307;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_SEC_INTERACTION_REQUIRED: i32 = -25315;

enum OpaqueSecKeychainSearchRef {}
type SecKeychainSearchRef = *mut OpaqueSecKeychainSearchRef;

#[repr(C)]
struct SecKeychainAttributeInfo {
    count: u32,
    tag: *mut u32,
    format: *mut u32,
}

const GENERIC_PASSWORD_ITEM_CLASS: u32 = u32::from_be_bytes(*b"genp");
const SERVICE_ITEM_ATTR: u32 = u32::from_be_bytes(*b"svce");
const ACCOUNT_ITEM_ATTR: u32 = u32::from_be_bytes(*b"acct");

// SAFETY: These declarations match the installed Security.framework headers.
// security-framework-sys omits these legacy file-keychain symbols.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrAccess: CFStringRef;
    fn SecAccessCreate(
        descriptor: CFStringRef,
        trusted_list: CFArrayRef,
        access: *mut SecAccessRef,
    ) -> i32;
    fn SecKeychainSearchCreateFromAttributes(
        keychain_or_array: CFTypeRef,
        item_class: u32,
        attr_list: *const SecKeychainAttributeList,
        search: *mut SecKeychainSearchRef,
    ) -> i32;
    fn SecKeychainSearchCopyNext(
        search: SecKeychainSearchRef,
        item: *mut SecKeychainItemRef,
    ) -> i32;
    fn SecKeychainItemCopyAttributesAndData(
        item: SecKeychainItemRef,
        info: *mut SecKeychainAttributeInfo,
        item_class: *mut u32,
        attr_list: *mut *mut SecKeychainAttributeList,
        length: *mut u32,
        data: *mut *mut c_void,
    ) -> i32;
    fn SecKeychainItemFreeAttributesAndData(
        attr_list: *mut SecKeychainAttributeList,
        data: *mut c_void,
    ) -> i32;
}

/// The fixed accounts: the local identity, and the single confirmed peer of earlier builds,
/// which is still read and deleted like any other peer record but never created again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentitySlot {
    LocalIdentity = 0,
    ConfirmedPeer = 1,
}

impl IdentitySlot {
    const fn account(self) -> &'static str {
        match self {
            Self::LocalIdentity => "LocalIdentity",
            Self::ConfirmedPeer => "ConfirmedPeer",
        }
    }
}

/// A bounded identity record that zeroizes its Rust allocation on drop.
pub struct ProtectedRecord(Zeroizing<Vec<u8>>);

impl ProtectedRecord {
    /// Takes ownership without making a plaintext copy.
    pub fn from_zeroizing(bytes: Zeroizing<Vec<u8>>) -> Result<Self, IdentityStoreError> {
        validate_payload_length(bytes.len())?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn into_zeroizing(self) -> Zeroizing<Vec<u8>> {
        self.0
    }

    fn from_keychain_data(data: &CFData) -> Result<Self, IdentityStoreError> {
        let length =
            usize::try_from(data.len()).map_err(|_| IdentityStoreError::InvalidKeychainResponse)?;
        if !is_valid_payload_length(length) {
            return Err(IdentityStoreError::InvalidKeychainResponse);
        }
        // Security owns the immutable CFData allocation. The returned Rust copy zeroizes on drop.
        Ok(Self(Zeroizing::new(data.bytes().to_vec())))
    }
}

/// Categories only. No variant contains a record, slot, status code, or OS text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityStoreError {
    InvalidPayloadLength,
    /// A peer key or account outside the shapes this store creates; nothing else is addressable.
    InvalidRecordName,
    RecordAlreadyExists,
    RecordNotFound,
    KeychainUnavailable,
    KeychainLocked,
    KeychainDenied,
    InvalidKeychainResponse,
    /// Any other OSStatus, kept so the message names the code.
    KeychainFailure(i32),
}

impl fmt::Display for IdentityStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::KeychainFailure(status) = self {
            return write!(
                formatter,
                "the Keychain operation failed (OSStatus {status})"
            );
        }
        formatter.write_str(match self {
            Self::InvalidPayloadLength => "identity record length is invalid",
            Self::InvalidRecordName => "the record name is not one this store can address",
            Self::RecordAlreadyExists => "identity record already exists",
            Self::RecordNotFound => "identity record was not found",
            Self::KeychainUnavailable => "the current-user Keychain is unavailable",
            Self::KeychainLocked => "the current-user Keychain is locked",
            Self::KeychainDenied => "the current-user Keychain denied access",
            Self::InvalidKeychainResponse => "the Keychain returned an invalid record",
            Self::KeychainFailure(_) => unreachable!("written above"),
        })
    }
}

impl Error for IdentityStoreError {}

/// Creates one fixed record in the current user's default file-based Keychain.
/// The caller must invoke this only after an explicit local identity action.
pub fn create_after_user_action(
    slot: IdentitySlot,
    record: ProtectedRecord,
) -> Result<(), IdentityStoreError> {
    create_record(slot.account(), record)
}

/// Creates the confirmed-peer record for `key` (the peer's 64-hex fingerprint) after an
/// explicit pairing action. An existing record for the same key is never replaced.
pub fn create_peer_after_user_action(
    key: &str,
    record: ProtectedRecord,
) -> Result<(), IdentityStoreError> {
    if !is_peer_key(key) {
        return Err(IdentityStoreError::InvalidRecordName);
    }
    create_record(&format!("{PEER_ACCOUNT_PREFIX}{key}"), record)
}

fn create_record(account: &str, record: ProtectedRecord) -> Result<(), IdentityStoreError> {
    let keychain = DefaultKeychain::copy_after_user_action()?;
    let access = CreatingApplicationAccess::new()?;
    let mut query = base_query(account);
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented query key.
        unsafe { kSecUseKeychain },
        &keychain.0,
    );
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented legacy ACL key.
        unsafe { kSecAttrAccess },
        &access.0,
    );

    // CFData retains this Arc for the synchronous call without another Rust copy.
    let data = CFData::from_arc(Arc::new(record.into_zeroizing()));
    let value = data.as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented value-data key.
        unsafe { kSecValueData },
        &value,
    );

    // SAFETY: query owns all Core Foundation values for the entire synchronous call.
    let status = unsafe { SecItemAdd(query.as_concrete_TypeRef(), ptr::null_mut()) };
    status_result(status)
}

/// Loads one fixed record from the current user's default file-based Keychain.
/// The caller must invoke this only after an explicit local identity action.
pub fn load_after_user_action(slot: IdentitySlot) -> Result<ProtectedRecord, IdentityStoreError> {
    let keychain = DefaultKeychain::copy_after_user_action()?;
    read_record(slot.account(), &keychain)
}

/// One record's data by account, in the one keychain the caller already opened.
fn read_record(
    account: &str,
    keychain: &DefaultKeychain,
) -> Result<ProtectedRecord, IdentityStoreError> {
    let search_list = CFArray::from_CFTypes(&[keychain.0.as_CFType()]);
    let mut query = base_query(account);
    let search_value = search_list.as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented search-list key.
        unsafe { kSecMatchSearchList },
        &search_value,
    );
    let return_data = CFBoolean::true_value().as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented return-data key.
        unsafe { kSecReturnData },
        &return_data,
    );

    let mut result = ptr::null();
    // SAFETY: query and the result out-pointer are valid for the synchronous call.
    let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
    status_result(status)?;
    if result.is_null() {
        return Err(IdentityStoreError::InvalidKeychainResponse);
    }
    // SAFETY: successful SecItemCopyMatching returns a retained Core Foundation object.
    let result = unsafe { CFType::wrap_under_create_rule(result) };
    let Some(data) = result.downcast_into::<CFData>() else {
        return Err(IdentityStoreError::InvalidKeychainResponse);
    };
    ProtectedRecord::from_keychain_data(&data)
}

/// Every confirmed-peer record with its account, the legacy single-peer record included. The
/// caller must invoke this only after an explicit pairing or sharing action. The listing asks the
/// Keychain for accounts only; each peer record's data is then read on its own, so the local
/// identity's data never leaves the Keychain for a listing and every read keeps the single-item
/// access path the record was created for.
pub fn list_peers_after_user_action() -> Result<Vec<(String, ProtectedRecord)>, IdentityStoreError>
{
    let keychain = DefaultKeychain::copy_after_user_action()?;
    peer_accounts(&keychain)?
        .into_iter()
        .map(|account| read_record(&account, &keychain).map(|record| (account, record)))
        .collect()
}

/// The accounts of every record of this service, through the attribute search that never touches
/// a record's data. The SecItem listing checks each record's partition list instead and asks for
/// the keychain password on the first launch of every new build.
fn peer_accounts(keychain: &DefaultKeychain) -> Result<Vec<String>, IdentityStoreError> {
    let mut service = SERVICE.as_bytes().to_vec();
    let mut attribute = SecKeychainAttribute {
        tag: SERVICE_ITEM_ATTR,
        length: u32::try_from(service.len()).map_err(|_| IdentityStoreError::InvalidRecordName)?,
        data: service.as_mut_ptr().cast(),
    };
    let attributes = SecKeychainAttributeList {
        count: 1,
        attr: &mut attribute,
    };
    let mut search = ptr::null_mut();
    // SAFETY: the keychain reference is live and the attribute list outlives the call.
    let status = unsafe {
        SecKeychainSearchCreateFromAttributes(
            keychain.0.as_CFTypeRef(),
            GENERIC_PASSWORD_ITEM_CLASS,
            &attributes,
            &mut search,
        )
    };
    status_result(status)?;
    if search.is_null() {
        return Err(IdentityStoreError::InvalidKeychainResponse);
    }
    // SAFETY: SecKeychainSearchCreateFromAttributes returned one retained Core Foundation object.
    let search = unsafe { CFType::wrap_under_create_rule(search.cast()) };
    let mut accounts = Vec::new();
    loop {
        let mut item = ptr::null_mut();
        // SAFETY: the search is live and the out-pointer is valid for the call.
        let status = unsafe {
            SecKeychainSearchCopyNext(search.as_CFTypeRef().cast_mut().cast(), &mut item)
        };
        if status == ERR_SEC_ITEM_NOT_FOUND {
            return Ok(accounts);
        }
        status_result(status)?;
        if item.is_null() {
            return Err(IdentityStoreError::InvalidKeychainResponse);
        }
        // SAFETY: SecKeychainSearchCopyNext returned one retained item reference.
        let account =
            KeychainItem(unsafe { CFType::wrap_under_create_rule(item.cast()) }).account()?;
        if is_peer_account(&account) {
            accounts.push(account);
        }
    }
}

fn account_from(attributes: *mut SecKeychainAttributeList) -> Option<String> {
    // SAFETY: Security returned a valid list for the one requested attribute.
    let list = unsafe { &*attributes };
    if list.count != 1 || list.attr.is_null() {
        return None;
    }
    // SAFETY: the count says one attribute entry is present.
    let attribute = unsafe { &*list.attr };
    if attribute.length == 0 || attribute.data.is_null() {
        return Some(String::new());
    }
    let length = usize::try_from(attribute.length).ok()?;
    // SAFETY: Security fills `data` with `length` bytes that stay valid until the list is freed.
    let bytes = unsafe { std::slice::from_raw_parts(attribute.data.cast::<u8>(), length) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn is_peer_key(key: &str) -> bool {
    key.len() == PEER_KEY_LENGTH
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The legacy account and the per-computer accounts; the local identity is never one of them.
fn is_peer_account(account: &str) -> bool {
    account == IdentitySlot::ConfirmedPeer.account()
        || account
            .strip_prefix(PEER_ACCOUNT_PREFIX)
            .is_some_and(is_peer_key)
}

struct KeychainItem(CFType);

impl KeychainItem {
    fn find(account: &str, keychain: &DefaultKeychain) -> Result<Self, IdentityStoreError> {
        let search_list = CFArray::from_CFTypes(&[keychain.0.as_CFType()]);
        let mut query = base_query(account);
        let search_value = search_list.as_CFType();
        add_attribute(
            &mut query,
            // SAFETY: Security.framework exports this documented search-list key.
            unsafe { kSecMatchSearchList },
            &search_value,
        );
        let return_ref = CFBoolean::true_value().as_CFType();
        add_attribute(
            &mut query,
            // SAFETY: Security.framework exports this documented return-reference key.
            unsafe { kSecReturnRef },
            &return_ref,
        );
        let mut result = ptr::null();
        // SAFETY: query and the result out-pointer are valid for the synchronous call.
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
        status_result(status)?;
        if result.is_null() {
            return Err(IdentityStoreError::InvalidKeychainResponse);
        }
        // SAFETY: successful SecItemCopyMatching returns a retained Core Foundation object.
        Ok(Self(unsafe { CFType::wrap_under_create_rule(result) }))
    }

    fn item_ref(&self) -> SecKeychainItemRef {
        self.0.as_CFTypeRef().cast_mut().cast()
    }

    /// The account attribute alone; no data pointer is requested, so the record stays sealed.
    fn account(&self) -> Result<String, IdentityStoreError> {
        let mut tag = ACCOUNT_ITEM_ATTR;
        let mut format = 0;
        let mut info = SecKeychainAttributeInfo {
            count: 1,
            tag: &mut tag,
            format: &mut format,
        };
        let mut attributes: *mut SecKeychainAttributeList = ptr::null_mut();
        // SAFETY: the item is live, the info list outlives the call, and no data is requested.
        let status = unsafe {
            SecKeychainItemCopyAttributesAndData(
                self.item_ref(),
                &mut info,
                ptr::null_mut(),
                &mut attributes,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        status_result(status)?;
        if attributes.is_null() {
            return Err(IdentityStoreError::InvalidKeychainResponse);
        }
        let account = account_from(attributes);
        // SAFETY: the list came from SecKeychainItemCopyAttributesAndData and is freed once.
        unsafe { SecKeychainItemFreeAttributesAndData(attributes, ptr::null_mut()) };
        account.ok_or(IdentityStoreError::InvalidKeychainResponse)
    }
}

/// Deletes one confirmed-peer record by the account `list_peers_after_user_action` reported,
/// after an explicit forget action. A missing peer is already forgotten; the local identity is
/// not addressable here.
pub fn delete_peer_after_user_action(account: &str) -> Result<(), IdentityStoreError> {
    if !is_peer_account(account) {
        return Err(IdentityStoreError::InvalidRecordName);
    }
    let keychain = DefaultKeychain::copy_after_user_action()?;
    // SecItemDelete refuses a record whose access rule names another bundle basename (the product
    // rename did that) with errSecInvalidOwnerEdit and no dialog; deleting the resolved item skips that check.
    let item = match KeychainItem::find(account, &keychain) {
        Ok(item) => item,
        Err(IdentityStoreError::RecordNotFound) => return Ok(()),
        Err(error) => return Err(error),
    };
    // SAFETY: the item reference is live for the synchronous call.
    let status = unsafe { SecKeychainItemDelete(item.item_ref()) };
    delete_status_result(status)
}

struct DefaultKeychain(CFType);

impl DefaultKeychain {
    fn copy_after_user_action() -> Result<Self, IdentityStoreError> {
        let mut keychain = ptr::null_mut();
        // SAFETY: Security.framework initializes a retained default-keychain reference on success.
        let status = unsafe { SecKeychainCopyDefault(&mut keychain) };
        status_result(status)?;
        if keychain.is_null() {
            return Err(IdentityStoreError::KeychainUnavailable);
        }
        // SAFETY: SecKeychainCopyDefault returned one retained Core Foundation object.
        Ok(Self(unsafe {
            CFType::wrap_under_create_rule(keychain.cast())
        }))
    }
}

struct CreatingApplicationAccess(CFType);

impl CreatingApplicationAccess {
    fn new() -> Result<Self, IdentityStoreError> {
        let descriptor = CFString::from_static_string(ACCESS_DESCRIPTOR);
        let mut access = ptr::null_mut();
        // SAFETY: null trusted_list requests the SDK's creating-application default ACL.
        let status =
            unsafe { SecAccessCreate(descriptor.as_concrete_TypeRef(), ptr::null(), &mut access) };
        status_result(status)?;
        if access.is_null() {
            return Err(IdentityStoreError::InvalidKeychainResponse);
        }
        // SAFETY: SecAccessCreate returned one retained Core Foundation object.
        Ok(Self(unsafe {
            CFType::wrap_under_create_rule(access.cast())
        }))
    }
}

fn base_query(account: &str) -> CFMutableDictionary<CFType, CFType> {
    let mut query = service_query();
    let account = CFString::new(account).as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented account key.
        unsafe { kSecAttrAccount },
        &account,
    );
    query
}

/// Every record of this app, whatever its account: the service is fixed and never caller-chosen.
fn service_query() -> CFMutableDictionary<CFType, CFType> {
    let mut query = CFMutableDictionary::new();
    let item_class = security_value(
        // SAFETY: Security.framework exports this documented generic-password value.
        unsafe { kSecClassGenericPassword },
    );
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented class key.
        unsafe { kSecClass },
        &item_class,
    );
    let service = CFString::from_static_string(SERVICE).as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented service key.
        unsafe { kSecAttrService },
        &service,
    );
    let false_value = CFBoolean::false_value().as_CFType();
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented synchronization key.
        unsafe { kSecAttrSynchronizable },
        &false_value,
    );
    add_attribute(
        &mut query,
        // SAFETY: Security.framework exports this documented keychain-selector key.
        unsafe { kSecUseDataProtectionKeychain },
        &false_value,
    );
    query
}

fn add_attribute(
    query: &mut CFMutableDictionary<CFType, CFType>,
    key: CFStringRef,
    value: &CFType,
) {
    let key = security_value(key);
    query.add(&key, value);
}

fn security_value(value: CFStringRef) -> CFType {
    // SAFETY: Security.framework constants are live immutable Core Foundation values.
    unsafe { CFType::wrap_under_get_rule(value.cast()) }
}

fn status_result(status: i32) -> Result<(), IdentityStoreError> {
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(classify_status(status))
    }
}

fn delete_status_result(status: i32) -> Result<(), IdentityStoreError> {
    if status == ERR_SEC_ITEM_NOT_FOUND {
        Ok(())
    } else {
        status_result(status)
    }
}

const fn classify_status(status: i32) -> IdentityStoreError {
    match status {
        ERR_SEC_DUPLICATE_ITEM => IdentityStoreError::RecordAlreadyExists,
        ERR_SEC_ITEM_NOT_FOUND => IdentityStoreError::RecordNotFound,
        ERR_SEC_NOT_AVAILABLE | ERR_SEC_NO_DEFAULT_KEYCHAIN => {
            IdentityStoreError::KeychainUnavailable
        }
        ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_INTERACTION_REQUIRED => {
            IdentityStoreError::KeychainLocked
        }
        ERR_SEC_AUTH_FAILED
        | ERR_SEC_USER_CANCELED
        | ERR_SEC_MISSING_ENTITLEMENT
        | ERR_SEC_RESTRICTED_API => IdentityStoreError::KeychainDenied,
        ERR_SEC_PARAM => IdentityStoreError::InvalidKeychainResponse,
        status => IdentityStoreError::KeychainFailure(status),
    }
}

fn validate_payload_length(length: usize) -> Result<(), IdentityStoreError> {
    if is_valid_payload_length(length) {
        Ok(())
    } else {
        Err(IdentityStoreError::InvalidPayloadLength)
    }
}

fn is_valid_payload_length(length: usize) -> bool {
    length > 0 && length <= MAX_RECORD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_boundaries_are_validated_without_keychain_access() {
        assert_eq!(
            validate_payload_length(0),
            Err(IdentityStoreError::InvalidPayloadLength)
        );
        assert!(validate_payload_length(1).is_ok());
        assert!(validate_payload_length(MAX_RECORD_BYTES).is_ok());
        assert_eq!(
            validate_payload_length(MAX_RECORD_BYTES + 1),
            Err(IdentityStoreError::InvalidPayloadLength)
        );
    }

    #[test]
    fn status_categories_do_not_include_keychain_data() {
        assert_eq!(
            classify_status(ERR_SEC_DUPLICATE_ITEM),
            IdentityStoreError::RecordAlreadyExists
        );
        assert_eq!(
            classify_status(ERR_SEC_ITEM_NOT_FOUND),
            IdentityStoreError::RecordNotFound
        );
        assert_eq!(
            classify_status(ERR_SEC_NOT_AVAILABLE),
            IdentityStoreError::KeychainUnavailable
        );
        assert_eq!(
            classify_status(ERR_SEC_INTERACTION_NOT_ALLOWED),
            IdentityStoreError::KeychainLocked
        );
        assert_eq!(
            classify_status(ERR_SEC_AUTH_FAILED),
            IdentityStoreError::KeychainDenied
        );
        assert_eq!(classify_status(-1), IdentityStoreError::KeychainFailure(-1));
    }

    #[test]
    fn peer_accounts_are_recognized_by_shape_and_never_include_the_identity() {
        let key = "0123456789abcdef".repeat(4);
        assert!(is_peer_key(&key));
        assert!(!is_peer_key(&key.to_ascii_uppercase()));
        assert!(!is_peer_key(&key[..63]));
        assert!(is_peer_account("ConfirmedPeer"));
        assert!(is_peer_account(&format!("ConfirmedPeer/{key}")));
        assert!(!is_peer_account("LocalIdentity"));
        assert!(!is_peer_account("ConfirmedPeer/"));
        assert_eq!(
            create_peer_after_user_action(
                "LocalIdentity",
                ProtectedRecord::from_zeroizing(Zeroizing::new(vec![1])).unwrap()
            ),
            Err(IdentityStoreError::InvalidRecordName)
        );
        assert_eq!(
            delete_peer_after_user_action("LocalIdentity"),
            Err(IdentityStoreError::InvalidRecordName)
        );
    }

    #[test]
    fn slots_are_fixed_and_service_is_not_caller_selected() {
        assert_eq!(IdentitySlot::LocalIdentity.account(), "LocalIdentity");
        assert_eq!(IdentitySlot::ConfirmedPeer.account(), "ConfirmedPeer");
        assert_eq!(PEER_ACCOUNT_PREFIX, "ConfirmedPeer/");
        assert_eq!(SERVICE, "com.manuelgozzi.monhop");
        // The dialog name follows the product; the service is the key the records are found under.
        assert_eq!(ACCESS_DESCRIPTOR, "MonHop");
    }

    #[test]
    fn peer_delete_treats_only_absence_as_success() {
        assert!(delete_status_result(ERR_SEC_ITEM_NOT_FOUND).is_ok());
        assert_eq!(
            delete_status_result(ERR_SEC_AUTH_FAILED),
            Err(IdentityStoreError::KeychainDenied)
        );
        assert_eq!(
            delete_status_result(ERR_SEC_INTERACTION_NOT_ALLOWED),
            Err(IdentityStoreError::KeychainLocked)
        );
    }
}
