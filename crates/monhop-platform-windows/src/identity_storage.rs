//! Explicit-action storage for the current-user DPAPI identity record and the confirmed-peer records.
//!
//! The Windows AppData ACL is not a substitute for DPAPI. This module writes
//! only the DPAPI blob and never treats a storage or DPAPI error as absence.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(windows)]
use {
    crate::identity,
    std::{
        ffi::{OsStr, OsString},
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
            io::{FromRawHandle, RawHandle},
        },
        path::{Component, Prefix},
        ptr,
    },
    windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE, S_OK},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileDispositionInfo, GetDriveTypeW, GetFileInformationByHandle,
            OPEN_EXISTING, SetFileInformationByHandle,
        },
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
    },
    zeroize::Zeroizing,
};

#[cfg(windows)]
const APP_DIRECTORY: &str = "MonHop";
const IDENTITY_FILE: &str = "identity.dpapi";
/// The single confirmed peer of earlier builds: still read and deleted like any other peer record.
const CONFIRMED_PEER_FILE: &str = "confirmed-peer.dpapi";
const PEER_FILE_PREFIX: &str = "peer-";
const PEER_FILE_SUFFIX: &str = ".dpapi";
const PEER_KEY_LENGTH: usize = 64;
#[cfg(windows)]
const MIN_RECORD_BYTES: usize = 1;
#[cfg(windows)]
const MAX_RECORD_BYTES: usize = 4096;
const MIN_PROTECTED_BYTES: usize = 1;
const MAX_PROTECTED_BYTES: usize = 16384;
const MAX_TEMPORARY_ATTEMPTS: u64 = 32;
#[cfg(windows)]
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;
#[cfg(windows)]
const DRIVE_FIXED: u32 = 3;

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Loads the one current-user record after an explicit user identity action.
///
/// This never creates, repairs, or replaces storage. `None` means the fixed
/// record path did not exist; all path, permission, malformed-data, and DPAPI
/// errors are returned to the caller.
#[cfg(windows)]
pub fn load_after_user_action() -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let Some(path) = identity_path_for_load()? else {
        return Ok(None);
    };
    load_protected_file(&path, identity::unprotect_key)
}

/// Creates the one current-user record after an explicit user identity action.
///
/// DPAPI encryption completes in memory before any storage path is created or
/// written. The final fixed path is published without overwriting an existing
/// record, including corrupt or unsupported existing data.
#[cfg(windows)]
pub fn create_after_user_action(record: &[u8]) -> io::Result<()> {
    validate_length(
        record.len(),
        MIN_RECORD_BYTES,
        MAX_RECORD_BYTES,
        "identity record",
    )?;
    let protected = Zeroizing::new(identity::protect_key(record)?);
    validate_length(
        protected.len(),
        MIN_PROTECTED_BYTES,
        MAX_PROTECTED_BYTES,
        "protected identity record",
    )?;
    let path = identity_path_for_create()?;
    write_new_protected_file(&path, protected.as_slice())
}

/// Every confirmed-peer record with its file name, the legacy single-peer record included, after
/// an explicit pairing or sharing action. Identity storage is untouched.
#[cfg(windows)]
pub fn list_peers_after_user_action() -> io::Result<Vec<(String, Zeroizing<Vec<u8>>)>> {
    let Some(directory) = peer_directory_for_load()? else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| is_peer_file_name(name))
        .collect();
    names.sort();
    let mut peers = Vec::with_capacity(names.len());
    for name in names {
        if let Some(record) = load_protected_file(&directory.join(&name), identity::unprotect_key)?
        {
            peers.push((name, record));
        }
    }
    Ok(peers)
}

/// Creates the confirmed-peer record for `key` (the peer's 64-hex fingerprint) after an explicit
/// pairing action. Encryption completes before storage is created and no record is ever replaced.
#[cfg(windows)]
pub fn create_peer_after_user_action(key: &str, record: &[u8]) -> io::Result<()> {
    if !is_peer_key(key) {
        return Err(invalid_record_name());
    }
    validate_length(
        record.len(),
        MIN_RECORD_BYTES,
        MAX_RECORD_BYTES,
        "confirmed peer record",
    )?;
    let protected = Zeroizing::new(identity::protect_key(record)?);
    validate_length(
        protected.len(),
        MIN_PROTECTED_BYTES,
        MAX_PROTECTED_BYTES,
        "protected confirmed peer record",
    )?;
    let path =
        peer_directory_for_create()?.join(format!("{PEER_FILE_PREFIX}{key}{PEER_FILE_SUFFIX}"));
    write_new_protected_file(&path, protected.as_slice())
}

/// Deletes one confirmed-peer record by the file name `list_peers_after_user_action` reported,
/// after an explicit forget action. A missing peer is already forgotten; malformed peer data is
/// removed without decoding it, and the identity record is never addressable here.
#[cfg(windows)]
pub fn delete_peer_after_user_action(file_name: &str) -> io::Result<()> {
    if !is_peer_file_name(file_name) {
        return Err(invalid_record_name());
    }
    let Some(directory) = peer_directory_for_load()? else {
        return Ok(());
    };
    delete_regular_file_without_following(&directory.join(file_name))
}

fn is_peer_key(key: &str) -> bool {
    key.len() == PEER_KEY_LENGTH
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The legacy file and the per-computer files; the identity file is never one of them.
fn is_peer_file_name(name: &str) -> bool {
    name == CONFIRMED_PEER_FILE
        || name
            .strip_prefix(PEER_FILE_PREFIX)
            .and_then(|rest| rest.strip_suffix(PEER_FILE_SUFFIX))
            .is_some_and(is_peer_key)
}

#[cfg(windows)]
fn invalid_record_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "the record name is not one this store can address",
    )
}

#[cfg(windows)]
fn identity_path_for_load() -> io::Result<Option<PathBuf>> {
    let app_data = local_app_data()?;
    validate_existing_directory(&app_data)?;
    let directory = app_data.join(APP_DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            validate_existing_directory(&directory)?;
            Ok(Some(directory.join(IDENTITY_FILE)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn identity_path_for_create() -> io::Result<PathBuf> {
    let app_data = local_app_data()?;
    validate_existing_directory(&app_data)?;
    let directory = app_data.join(APP_DIRECTORY);
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_existing_directory(&directory)?;
    Ok(directory.join(IDENTITY_FILE))
}

#[cfg(windows)]
fn peer_directory_for_load() -> io::Result<Option<PathBuf>> {
    let app_data = local_app_data()?;
    validate_existing_directory(&app_data)?;
    let directory = app_data.join(APP_DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            validate_existing_directory(&directory)?;
            Ok(Some(directory))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn peer_directory_for_create() -> io::Result<PathBuf> {
    let app_data = local_app_data()?;
    validate_existing_directory(&app_data)?;
    let directory = app_data.join(APP_DIRECTORY);
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_existing_directory(&directory)?;
    Ok(directory)
}

#[cfg(windows)]
fn local_app_data() -> io::Result<PathBuf> {
    let mut raw = ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath initializes an allocator-owned UTF-16 path on success.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, ptr::null_mut(), &mut raw) };
    let path = KnownFolderAllocation(raw);
    if status != S_OK {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(LocalAppData) failed",
        ));
    }
    if path.0.is_null() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(LocalAppData) returned no path",
        ));
    }
    let length = wide_string_length(path.0)?;
    // SAFETY: wide_string_length found the terminator within the bounded allocation.
    let value = unsafe { std::slice::from_raw_parts(path.0, length) };
    let path = PathBuf::from(OsString::from_wide(value));
    if path.as_os_str().is_empty() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(LocalAppData) returned an empty path",
        ));
    }
    validate_local_path(&path)?;
    Ok(path)
}

#[cfg(windows)]
struct KnownFolderAllocation(*mut u16);

#[cfg(windows)]
impl Drop for KnownFolderAllocation {
    fn drop(&mut self) {
        // SAFETY: SHGetKnownFolderPath documents this allocation as CoTaskMemFree-owned.
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

#[cfg(windows)]
fn wide_string_length(value: *const u16) -> io::Result<usize> {
    const MAX_UTF16_CODE_UNITS: usize = 32_768;
    for length in 0..MAX_UTF16_CODE_UNITS {
        // SAFETY: the bounded scan reads the NUL-terminated buffer returned by the shell API.
        if unsafe { *value.add(length) } == 0 {
            return Ok(length);
        }
    }
    Err(io::Error::other(
        "SHGetKnownFolderPath(LocalAppData) returned an unterminated path",
    ))
}

#[cfg(windows)]
fn validate_local_path(path: &Path) -> io::Result<()> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LocalAppData is not a drive-rooted local path",
        ));
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        Prefix::UNC(_, _)
        | Prefix::VerbatimUNC(_, _)
        | Prefix::DeviceNS(_)
        | Prefix::Verbatim(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "LocalAppData resolved to a network or device path",
            ));
        }
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LocalAppData is not an absolute drive path",
        ));
    }
    let root = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: root is a terminated drive-root UTF-16 string valid for this synchronous query.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "LocalAppData is not on a fixed local drive",
        ));
    }
    Ok(())
}

fn load_protected_file<T, F>(path: &Path, provider: F) -> io::Result<Option<T>>
where
    F: FnOnce(&[u8]) -> io::Result<T>,
{
    #[cfg(windows)]
    let (mut file, length) = match open_regular_file_without_following(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(not(windows))]
    let (mut file, length) = {
        let Some(metadata) = regular_file_metadata(path)? else {
            return Ok(None);
        };
        (File::open(path)?, checked_protected_length(metadata.len())?)
    };
    let mut protected = vec![0_u8; length];
    file.read_exact(&mut protected)?;
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protected identity record exceeded its checked size",
        ));
    }
    provider(&protected).map(Some)
}

fn write_new_protected_file(path: &Path, protected: &[u8]) -> io::Result<()> {
    validate_length(
        protected.len(),
        MIN_PROTECTED_BYTES,
        MAX_PROTECTED_BYTES,
        "protected identity record",
    )?;
    ensure_target_absent(path)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity storage path has no parent directory",
        )
    })?;
    validate_existing_directory(parent)?;

    let mut temporary = OwnedTemporary::create(parent)?;
    temporary.write_and_sync(protected)?;
    temporary.publish(path)
}

fn ensure_target_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the identity record already exists",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn regular_file_metadata(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_regular_file_metadata(path, &metadata)?;
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn validate_existing_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_directory_metadata(path, &metadata)?;
    #[cfg(windows)]
    validate_directory_ancestors_without_following(path)?;
    Ok(())
}

fn validate_directory_metadata(path: &Path, metadata: &Metadata) -> io::Result<()> {
    validate_non_reparse(path, metadata)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity storage directory is not a directory",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn validate_regular_file_metadata(path: &Path, metadata: &Metadata) -> io::Result<()> {
    validate_non_reparse(path, metadata)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity storage record is not a regular file",
        ));
    }
    Ok(())
}

fn validate_non_reparse(_path: &Path, metadata: &Metadata) -> io::Result<()> {
    let is_reparse = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let is_reparse = is_reparse || metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0;
    if is_reparse {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "identity storage path is a symlink or reparse point",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_directory_ancestors_without_following(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => {
                current.push(component.as_os_str());
                rooted = true;
            }
            Component::Normal(value) if rooted => current.push(value),
            Component::Normal(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "identity storage directory is not rooted",
                ));
            }
            Component::CurDir | Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "identity storage directory contains a non-normal path component",
                ));
            }
        }
        if rooted {
            validate_opened_directory_without_following(&current)?;
        }
    }
    if !rooted {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity storage directory is not rooted",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_opened_directory_without_following(path: &Path) -> io::Result<()> {
    let handle =
        open_path_without_following(path, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS)?;
    let information = file_information(handle.0)?;
    if information.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
        != FILE_ATTRIBUTE_DIRECTORY
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "identity storage directory is a reparse point or not a directory",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_regular_file_without_following(path: &Path) -> io::Result<(File, usize)> {
    let mut handle = open_path_without_following(path, GENERIC_READ, 0)?;
    let information = file_information(handle.0)?;
    validate_opened_regular_file(&information)?;
    let length =
        u64::from(information.nFileSizeLow) | (u64::from(information.nFileSizeHigh) << u32::BITS);
    let length = checked_protected_length(length)?;
    // SAFETY: this guard owns the live CreateFileW handle exactly once.
    let file = unsafe { File::from_raw_handle(handle.take() as RawHandle) };
    Ok((file, length))
}

#[cfg(windows)]
fn delete_regular_file_without_following(path: &Path) -> io::Result<()> {
    let handle = match open_path_without_following(path, DELETE, 0) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let information = file_information(handle.0)?;
    validate_opened_regular_file(&information)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity storage path has no parent directory",
        )
    })?;
    validate_existing_directory(parent)?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: handle owns the exact opened entry and disposition is a valid C-compatible input.
    if unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>()).expect("size fits u32"),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn validate_opened_regular_file(information: &BY_HANDLE_FILE_INFORMATION) -> io::Result<()> {
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "identity storage record is a reparse point",
        ));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "identity storage record is not a regular file",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_path_without_following(
    path: &Path,
    access: u32,
    extra_flags: u32,
) -> io::Result<WindowsHandle> {
    let path = wide_path(path)?;
    // SAFETY: the path is NUL-terminated; OPEN_REPARSE_POINT requests the entry itself.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | extra_flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(WindowsHandle(handle))
}

#[cfg(windows)]
fn file_information(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle is live and information is writable C-compatible output storage.
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

#[cfg(windows)]
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut path: Vec<u16> = OsStr::new(path).encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity storage path contains a NUL",
        ));
    }
    path.push(0);
    Ok(path)
}

#[cfg(windows)]
struct WindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsHandle {
    fn take(&mut self) -> windows_sys::Win32::Foundation::HANDLE {
        std::mem::replace(&mut self.0, INVALID_HANDLE_VALUE)
    }
}

#[cfg(windows)]
impl Drop for WindowsHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this guard owns the live handle until it is transferred or closed here.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn checked_protected_length(length: u64) -> io::Result<usize> {
    let length = usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "protected identity record length does not fit memory",
        )
    })?;
    validate_length(
        length,
        MIN_PROTECTED_BYTES,
        MAX_PROTECTED_BYTES,
        "protected identity record",
    )?;
    Ok(length)
}

fn validate_length(length: usize, minimum: usize, maximum: usize, label: &str) -> io::Result<()> {
    if !(minimum..=maximum).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} length is outside the supported bound"),
        ));
    }
    Ok(())
}

struct OwnedTemporary {
    path: PathBuf,
    file: Option<File>,
    remove_on_drop: bool,
}

impl OwnedTemporary {
    fn create(directory: &Path) -> io::Result<Self> {
        for _ in 0..MAX_TEMPORARY_ATTEMPTS {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                ".identity.dpapi.{}.{}.tmp",
                std::process::id(),
                sequence
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        remove_on_drop: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique identity storage temporary file",
        ))
    }

    fn write_and_sync(&mut self, contents: &[u8]) -> io::Result<()> {
        let file = self.file.as_mut().ok_or_else(|| {
            io::Error::other("identity storage temporary file was already closed")
        })?;
        file.write_all(contents)?;
        file.sync_all()
    }

    fn publish(self, final_path: &Path) -> io::Result<()> {
        self.publish_with_cleanup(final_path, Self::remove)
    }

    fn publish_with_cleanup(
        mut self,
        final_path: &Path,
        cleanup: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        self.close()?;
        // hard_link creates the fixed name only if it remains absent. Both files
        // are in one validated directory, so this cannot cross filesystems.
        fs::hard_link(&self.path, final_path)?;
        // Publication already succeeded. Cleanup cannot skip the caller's identity readback.
        let _ = cleanup(&mut self);
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        file.sync_all()?;
        drop(file);
        Ok(())
    }

    fn remove(&mut self) -> io::Result<()> {
        self.close()?;
        fs::remove_file(&self.path)?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for OwnedTemporary {
    fn drop(&mut self) {
        self.file.take();
        if self.remove_on_drop {
            // This guard only ever targets a path created by OpenOptions::create_new above.
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_record_is_never_overwritten() {
        let directory = TestDirectory::new();
        let path = directory.0.join(IDENTITY_FILE);
        write_new_protected_file(&path, b"protected test record").unwrap();
        let error = write_new_protected_file(&path, b"replacement").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), b"protected test record");
    }

    #[test]
    fn corrupt_existing_record_also_blocks_creation() {
        let directory = TestDirectory::new();
        let path = directory.0.join(IDENTITY_FILE);
        fs::write(&path, b"corrupt protected test record").unwrap();
        assert_eq!(
            write_new_protected_file(&path, b"replacement")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(path).unwrap(), b"corrupt protected test record");
    }

    #[test]
    fn peer_creation_rejects_duplicates_without_touching_identity() {
        let directory = TestDirectory::new();
        let identity_path = directory.0.join(IDENTITY_FILE);
        let peer_path = directory.0.join(CONFIRMED_PEER_FILE);
        fs::write(&identity_path, b"protected identity record").unwrap();
        fs::write(&peer_path, b"protected peer record").unwrap();

        assert_eq!(
            write_new_protected_file(&peer_path, b"replacement")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(peer_path).unwrap(), b"protected peer record");
        assert_eq!(
            fs::read(identity_path).unwrap(),
            b"protected identity record"
        );
    }

    #[test]
    fn peer_file_names_are_recognized_by_shape_and_never_include_the_identity() {
        let key = "0123456789abcdef".repeat(4);
        assert!(is_peer_key(&key));
        assert!(!is_peer_key(&key.to_ascii_uppercase()));
        assert!(is_peer_file_name(CONFIRMED_PEER_FILE));
        assert!(is_peer_file_name(&format!("peer-{key}.dpapi")));
        assert!(!is_peer_file_name(IDENTITY_FILE));
        assert!(!is_peer_file_name("peer-.dpapi"));
        assert!(!is_peer_file_name(&format!("peer-{key}.tmp")));
    }

    #[test]
    fn provider_failure_is_not_missing() {
        let directory = TestDirectory::new();
        let path = directory.0.join(IDENTITY_FILE);
        fs::write(&path, b"protected test record").unwrap();
        let error = load_protected_file(&path, |_| Err::<Vec<u8>, _>(io::Error::other("reject")))
            .unwrap_err();
        assert_eq!(error.to_string(), "reject");
    }

    #[test]
    fn truncated_and_oversized_records_are_rejected_before_provider_use() {
        let directory = TestDirectory::new();
        let truncated = directory.0.join("truncated.dpapi");
        fs::write(&truncated, []).unwrap();
        assert!(
            load_protected_file(&truncated, |bytes| Ok::<_, io::Error>(bytes.to_vec())).is_err()
        );

        let oversized = directory.0.join("oversized.dpapi");
        let file = File::create(&oversized).unwrap();
        file.set_len((MAX_PROTECTED_BYTES + 1) as u64).unwrap();
        assert!(
            load_protected_file(&oversized, |bytes| Ok::<_, io::Error>(bytes.to_vec())).is_err()
        );
    }

    #[test]
    fn temporary_is_cleaned_when_publish_loses_a_create_race() {
        let directory = TestDirectory::new();
        let final_path = directory.0.join(IDENTITY_FILE);
        let mut temporary = OwnedTemporary::create(&directory.0).unwrap();
        let temporary_path = temporary.path.clone();
        temporary.write_and_sync(b"protected test record").unwrap();
        fs::write(&final_path, b"other protected record").unwrap();
        assert_eq!(
            temporary.publish(&final_path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!temporary_path.exists());
        assert_eq!(fs::read(final_path).unwrap(), b"other protected record");
    }

    #[test]
    fn cleanup_failure_after_publication_preserves_success_and_readback() {
        let directory = TestDirectory::new();
        let final_path = directory.0.join(IDENTITY_FILE);
        let mut temporary = OwnedTemporary::create(&directory.0).unwrap();
        let temporary_path = temporary.path.clone();
        temporary.write_and_sync(b"protected test record").unwrap();
        temporary
            .publish_with_cleanup(&final_path, |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup blocked",
                ))
            })
            .unwrap();
        let readback =
            load_protected_file(&final_path, |record| Ok::<_, io::Error>(record.to_vec()))
                .unwrap()
                .unwrap();
        assert_eq!(readback, b"protected test record");
        assert!(!temporary_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_record_is_rejected_without_provider_access() {
        let directory = TestDirectory::new();
        let target = directory.0.join("target.dpapi");
        let path = directory.0.join(IDENTITY_FILE);
        fs::write(&target, b"protected test record").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            load_protected_file(&path, |_| Ok::<_, io::Error>(Vec::<u8>::new()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn peer_only_delete_removes_an_oversized_peer_without_touching_identity() {
        let directory = TestDirectory::new();
        let identity_path = directory.0.join(IDENTITY_FILE);
        let peer_path = directory.0.join(CONFIRMED_PEER_FILE);
        fs::write(&identity_path, b"protected identity record").unwrap();
        let peer = File::create(&peer_path).unwrap();
        peer.set_len((MAX_PROTECTED_BYTES + 1) as u64).unwrap();
        drop(peer);

        delete_regular_file_for_test(&peer_path).unwrap();

        assert!(!peer_path.exists());
        assert_eq!(
            fs::read(identity_path).unwrap(),
            b"protected identity record"
        );
    }

    #[cfg(windows)]
    #[test]
    fn peer_delete_readback_stays_error_until_another_open_handle_closes() {
        let directory = TestDirectory::new();
        let identity_path = directory.0.join(IDENTITY_FILE);
        let peer_path = directory.0.join(CONFIRMED_PEER_FILE);
        fs::write(&identity_path, b"protected identity record").unwrap();
        let mut peer = File::create(&peer_path).unwrap();
        peer.write_all(b"protected peer record").unwrap();

        delete_regular_file_for_test(&peer_path).unwrap();
        assert!(load_protected_file(&peer_path, |_| Ok::<_, io::Error>(())).is_err());

        drop(peer);
        assert!(
            load_protected_file(&peer_path, |_| Ok::<_, io::Error>(()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs::read(identity_path).unwrap(),
            b"protected identity record"
        );
    }

    #[test]
    fn missing_peer_delete_is_a_success() {
        let directory = TestDirectory::new();
        assert!(delete_regular_file_for_test(&directory.0.join(CONFIRMED_PEER_FILE)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn peer_delete_rejects_a_symlink_without_touching_its_target() {
        let directory = TestDirectory::new();
        let target = directory.0.join(IDENTITY_FILE);
        let peer_path = directory.0.join(CONFIRMED_PEER_FILE);
        fs::write(&target, b"protected identity record").unwrap();
        std::os::unix::fs::symlink(&target, &peer_path).unwrap();

        assert_eq!(
            delete_regular_file_for_test(&peer_path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(target).unwrap(), b"protected identity record");
    }

    fn delete_regular_file_for_test(path: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            delete_regular_file_without_following(path)
        }
        #[cfg(not(windows))]
        {
            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    validate_regular_file_metadata(path, &metadata)?;
                    fs::remove_file(path)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            for _ in 0..MAX_TEMPORARY_ATTEMPTS {
                let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "monhop-identity-storage-test-{}.{}",
                    std::process::id(),
                    sequence
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
            panic!("could not create a unique test directory")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
