//! Inert, bounded sharing setup preferences. Loading never authorizes or starts a session.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    hash::{DefaultHasher, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use monhop_core::{MonitorIdentity, Platform, Point};
use monhop_transport::{
    crypto::CertificateFingerprint,
    pairing::opposite_platform,
    session_handshake::device_id_from_fingerprint,
    session_setup::{
        DisplayDescription, DisplayTopology, InspectedPeer, MAX_DISPLAY_NAME_BYTES,
        MAX_LOGICAL_ORIGIN_ABS, MAX_LOGICAL_SIZE, MAX_NATIVE_DIMENSION, MAX_SCALE_FACTOR,
        MIN_SCALE_FACTOR,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::computers::{ComputerList, LIST_FILE};
use crate::group_record::{GroupRecord, MAX_GROUP_MEMBERS};
use crate::sharing::{ArrangementRequest, LayoutRequest, parse_display, parse_link};

/// A pairwise layout as the version 2 arrangement library and the version 3 setup file keep it.
pub const SHARING_PREFERENCES_VERSION: u8 = 2;
/// The file holding every group's record, the active group, and the network.
pub const SETUP_FILE: &str = "sharing-groups.json";
pub const SETUP_FILE_VERSION: u8 = 4;
/// Where version 3 kept its setup, beside `SETUP_FILE`. It is only ever read, so an older MonHop
/// still finds its own setup there.
const V3_SETUP_FILE: &str = "sharing-setup.json";
/// The file before groups: one record per paired computer, read only to migrate it.
const SETUP_FILE_V3_VERSION: u8 = 3;
pub const MAX_SHARING_PREFERENCES_BYTES: u64 = 32 * 1024;
pub const MAX_SETUP_FILE_BYTES: u64 = 512 * 1024;
/// Paired computers this app keeps setups and names for.
pub const MAX_COMPUTERS: usize = 16;
/// Past this many records the oldest inactive one is dropped.
const MAX_GROUPS: usize = 16;
/// Migrated records are revision 1 or 2, so the first change after migration is newer than both.
const MIGRATED_CLOCK: u64 = 2;

const MAX_INTERFACE_ID_BYTES: usize = 512;
pub(crate) const MAX_LINKS: usize = 64;
const GROUP_FULL: &str = "At most 8 computers can share together. Switch one off first.";
const NOT_ANOTHER_COMPUTER: &str = "Choose one of the other computers.";
const TEMPORARY_FILE_ATTEMPTS: usize = 64;

static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

/// Whether each computer's keyboard and mouse may control the other, keyed by that computer's
/// full lowercase fingerprint. Both computers of a pair are always named; at least one is true.
pub(crate) type ControlMap = BTreeMap<String, bool>;

/// A paired computer's platform, as its list entry names it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputerPlatform {
    Windows,
    Macos,
}

impl From<monhop_core::Platform> for ComputerPlatform {
    fn from(platform: monhop_core::Platform) -> Self {
        match platform {
            monhop_core::Platform::Windows => Self::Windows,
            monhop_core::Platform::MacOs => Self::Macos,
        }
    }
}

impl From<ComputerPlatform> for monhop_core::Platform {
    fn from(platform: ComputerPlatform) -> Self {
        match platform {
            ComputerPlatform::Windows => Self::Windows,
            ComputerPlatform::Macos => Self::MacOs,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisplaySnapshot {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) origin: [f64; 2],
    pub(crate) size: [f64; 2],
    pub(crate) native_size: [u32; 2],
    pub(crate) scale: f32,
    pub(crate) primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) monitor: Option<String>,
}

impl DisplaySnapshot {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// Both computers write the same meaning from their own side: the layout names displays and
/// computers by identity, never by which side of the file they sit on.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharingPreferences {
    pub(crate) version: u8,
    pub(crate) interface_id: String,
    pub(crate) local_fingerprint: String,
    pub(crate) peer_fingerprint: String,
    pub(crate) local_displays: Vec<DisplaySnapshot>,
    pub(crate) peer_displays: Vec<DisplaySnapshot>,
    pub(crate) layout: LayoutRequest,
}

/// Every group's saved layout, the group MonHop shares input with, and the one network all
/// sharing uses. Loading never authorizes or starts anything; a computer paired without a layout
/// is in no record here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupFile {
    version: u8,
    /// The physical network all sharing uses. Records carry none, and nothing but an explicit
    /// choice replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interface_id: Option<String>,
    /// This computer's fingerprint as its records name it, full uppercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local: Option<String>,
    /// Strictly sorted lowercase fingerprints of the other members of the active group; may name
    /// computers without a record.
    enabled: Vec<String>,
    /// Pause keeps the group; any choice clears it.
    paused: bool,
    /// The highest revision this computer made or saw.
    clock: u64,
    /// Strictly sorted by member set, each set naming this computer.
    groups: Vec<GroupRecord>,
    /// Read from a file a newer MonHop wrote: empty here, and never saved over.
    #[serde(skip)]
    newer: bool,
}

impl Default for SetupFile {
    fn default() -> Self {
        Self {
            version: SETUP_FILE_VERSION,
            interface_id: None,
            local: None,
            enabled: Vec::new(),
            paused: false,
            clock: 0,
            groups: Vec::new(),
            newer: false,
        }
    }
}

/// The file as version 3 wrote it: one record per paired computer and the active one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetupFileV3 {
    version: u8,
    #[serde(default)]
    interface_id: Option<String>,
    /// Lowercase fingerprint of the active computer; may name a computer without a record.
    #[serde(default)]
    active: Option<String>,
    /// Keyed by the lowercase peer fingerprint of each record.
    #[serde(default)]
    computers: BTreeMap<String, SharingPreferences>,
}

/// Only the version is read first: a file from another version holds nothing this build reads.
#[derive(Deserialize)]
struct VersionedFile {
    version: u8,
}

impl SetupFile {
    /// `path` is this version's file. While it is absent, the version 3 file beside it is
    /// migrated in memory; that file is never written, so an older MonHop keeps its own setup,
    /// and the next save writes `path`. A file a newer MonHop wrote, in either place, loads empty
    /// and read-only, so no save hides it; an older file with nothing to migrate loads empty, and
    /// a damaged one is an error.
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::load_as(
            path,
            crate::sharing::local_platform(),
            || listed_platforms(path),
            stored_identity,
        )
    }

    /// A migrated record names this computer with `platform` and each other computer with the
    /// platform `listed` gives it, else the other platform. `identity` is this computer's
    /// fingerprint as its identity store holds it, when that can be read.
    fn load_as(
        path: &Path,
        platform: Platform,
        listed: impl FnOnce() -> BTreeMap<String, Platform>,
        identity: impl FnOnce() -> Option<String>,
    ) -> io::Result<Self> {
        if let Some(bytes) = read_bounded(path, MAX_SETUP_FILE_BYTES)? {
            let file = match file_version(&bytes)? {
                SETUP_FILE_VERSION => {
                    serde_json::from_slice::<Self>(&bytes).map_err(|_| invalid_data())?
                }
                version if version > SETUP_FILE_VERSION => return Ok(Self::from_newer(version)),
                _ => return Err(invalid_data()),
            };
            file.validate().map_err(|_| invalid_data())?;
            return Ok(file);
        }
        let Some(bytes) = read_bounded(&path.with_file_name(V3_SETUP_FILE), MAX_SETUP_FILE_BYTES)?
        else {
            return Ok(Self::default());
        };
        let file = match file_version(&bytes)? {
            SETUP_FILE_V3_VERSION => {
                let old: SetupFileV3 =
                    serde_json::from_slice(&bytes).map_err(|_| invalid_data())?;
                old.validate().map_err(|_| invalid_data())?;
                let identity = if old.computers.is_empty() {
                    None
                } else {
                    identity()
                };
                old.migrate(platform, &listed(), identity.as_deref())
            }
            // Builds between version 3 and this one wrote version 4 in the old file's place.
            SETUP_FILE_VERSION => {
                serde_json::from_slice::<Self>(&bytes).map_err(|_| invalid_data())?
            }
            version if version > SETUP_FILE_VERSION => return Ok(Self::from_newer(version)),
            _ => return Ok(Self::default()),
        };
        file.validate().map_err(|_| invalid_data())?;
        Ok(file)
    }

    /// Empty and never saved: a save would overwrite the newer file, or, when it sits in the
    /// version 3 place, hide it behind a `path` that is read instead from then on.
    fn from_newer(version: u8) -> Self {
        log::warn!(
            "setup: the saved setup is version {version}, from a newer MonHop; it is kept as it is and nothing is saved over it"
        );
        Self {
            newer: true,
            ..Self::default()
        }
    }

    /// The file `load` reads for `path` right now: `path` itself, else the version 3 file it
    /// migrates from.
    pub(crate) fn stored_at(path: &Path) -> PathBuf {
        if fs::symlink_metadata(path).is_ok() {
            path.to_path_buf()
        } else {
            path.with_file_name(V3_SETUP_FILE)
        }
    }

    /// Loaded from a file a newer MonHop wrote, which this version never saves over.
    pub(crate) fn written_by_newer(&self) -> bool {
        self.newer
    }

    /// Writes the whole file atomically, creating the setup folder on first use.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if self.newer {
            return Err(newer_file());
        }
        self.validate().map_err(|_| invalid_data())?;
        let bytes = serde_json::to_vec(self).map_err(|_| invalid_data())?;
        fs::create_dir_all(preference_parent(path))?;
        save_bounded(path, &bytes, MAX_SETUP_FILE_BYTES)
    }

    /// This computer's fingerprint as its records name it; None until a record names it.
    pub(crate) fn local(&self) -> Option<&str> {
        self.local.as_deref()
    }

    pub(crate) fn enabled(&self) -> &[String] {
        &self.enabled
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused
    }

    pub(crate) fn clock(&self) -> u64 {
        self.clock
    }

    pub(crate) fn groups(&self) -> &[GroupRecord] {
        &self.groups
    }

    /// The record for this computer and the enabled computers; None means they need arranging.
    pub(crate) fn active_group(&self) -> Option<&GroupRecord> {
        let members = self.active_members()?;
        self.groups
            .iter()
            .find(|group| group.member_keys() == members)
    }

    /// The record a card for that computer draws: the active group's while it names that
    /// computer, else the pair's own, else the newest naming it.
    pub(crate) fn group_with(&self, fingerprint: &str) -> Option<&GroupRecord> {
        let naming = || {
            self.groups
                .iter()
                .filter(|group| group.has_member(fingerprint))
        };
        self.active_group()
            .filter(|group| group.has_member(fingerprint))
            .or_else(|| naming().find(|group| group.members().len() == 2))
            .or_else(|| naming().max_by_key(|group| group.stamp()))
    }

    /// Keeps whichever of `record` and the record held for its members has the greater stamp
    /// (`record` on a tie, for its labels) and enables exactly its other members. Pause and the
    /// network stay as the user chose them. Ok(true) when `record` is now its members' record.
    pub(crate) fn adopt(
        &mut self,
        local: &str,
        record: GroupRecord,
    ) -> Result<bool, PreferenceError> {
        self.take(local, record, false)
    }

    /// Like `adopt`, but `record` replaces the record held for its members whatever that one's
    /// stamp: a fixture that must hold exactly `record`.
    #[cfg(test)]
    pub(crate) fn store_agreed(
        &mut self,
        local: &str,
        record: GroupRecord,
    ) -> Result<(), PreferenceError> {
        self.take(local, record, true).map(drop)
    }

    fn take(
        &mut self,
        local: &str,
        record: GroupRecord,
        replace: bool,
    ) -> Result<bool, PreferenceError> {
        record.validate()?;
        let local = CertificateFingerprint::parse_full(local)
            .map_err(|_| PreferenceError::Invalid)?
            .full_hex();
        if !record.has_member(&local) {
            return Err(PreferenceError::Invalid);
        }
        self.set_local(&local);
        let members = record.member_keys();
        let local = fingerprint_key(&local);
        self.enabled = members
            .iter()
            .filter(|member| **member != local)
            .cloned()
            .collect();
        self.clock = self.clock.max(record.revision());
        let newer = replace
            || self
                .groups
                .iter()
                .find(|group| group.member_keys() == members)
                .is_none_or(|held| record.stamp() >= held.stamp());
        if newer {
            self.store(record);
        }
        Ok(newer)
    }

    /// The one enabled computer while sharing is not paused.
    pub fn active(&self) -> Option<&str> {
        match self.enabled.as_slice() {
            [peer] if !self.paused => Some(peer),
            _ => None,
        }
    }

    /// The user chose computers to share with and has not paused.
    pub fn sharing_chosen(&self) -> bool {
        !self.paused && !self.enabled.is_empty()
    }

    /// Enables exactly that computer and resumes; None pauses and keeps the group.
    pub fn set_active(&mut self, fingerprint: Option<&CertificateFingerprint>) {
        match fingerprint {
            Some(fingerprint) => {
                self.enabled = vec![fingerprint_key(&fingerprint.full_hex())];
                self.paused = false;
            }
            None => self.paused = true,
        }
    }

    /// `set_active(Some)` as a local activation: when it changes what is shared, the resulting
    /// group's record is touched so this choice wins over every copy made before it.
    pub(crate) fn choose(&mut self, fingerprint: &CertificateFingerprint) {
        let chosen = self.active() == Some(fingerprint_key(&fingerprint.full_hex()).as_str());
        self.set_active(Some(fingerprint));
        if !chosen {
            self.touch_active();
        }
    }

    /// Switches one other computer into or out of the enabled group as a local choice, touching
    /// the resulting group's record. Switching one off leaves a group without a record of its
    /// own derived from the previous record without it; a group that cannot be derived needs
    /// arranging. Switching a computer on resumes a paused group; switching one off never does.
    pub(crate) fn set_enabled(
        &mut self,
        fingerprint: &CertificateFingerprint,
        enabled: bool,
    ) -> Result<(), &'static str> {
        let key = fingerprint_key(&fingerprint.full_hex());
        if self.local.as_deref().map(fingerprint_key).as_ref() == Some(&key) {
            return Err(NOT_ANOTHER_COMPUTER);
        }
        let was = self.enabled.contains(&key);
        if was == enabled && !(enabled && self.paused) {
            return Ok(());
        }
        let previous = self.active_group().cloned();
        if enabled {
            if !was {
                if self.enabled.len() + 1 >= MAX_GROUP_MEMBERS {
                    return Err(GROUP_FULL);
                }
                self.enabled.push(key.clone());
                self.enabled.sort();
            }
            self.paused = false;
        } else {
            self.enabled.retain(|peer| *peer != key);
        }
        if self.enabled.is_empty() || self.touch_active() || enabled {
            return Ok(());
        }
        if let (Some(previous), Some(local)) = (previous, self.local.clone()) {
            let revision = self.clock.saturating_add(1);
            if let Some(derived) = previous.without_member(&key, revision, &local) {
                self.store(derived);
                self.clock = revision;
            }
        }
        Ok(())
    }

    /// Ends a pause, keeping the group.
    pub(crate) fn resume(&mut self) {
        self.paused = false;
    }

    /// Restamps the active group's record as this computer's newest change; false when that
    /// group has no record here.
    pub(crate) fn touch_active(&mut self) -> bool {
        let (Some(local), Some(members)) = (self.local.clone(), self.active_members()) else {
            return false;
        };
        let revision = self.clock.saturating_add(1);
        let Some(held) = self
            .groups
            .iter_mut()
            .find(|group| group.member_keys() == members)
        else {
            return false;
        };
        let Ok(touched) = held.restamped(revision, &local) else {
            return false;
        };
        *held = touched;
        self.clock = revision;
        true
    }

    /// A revision seen elsewhere, so the next change made here is stamped past it.
    pub(crate) fn observe_clock(&mut self, seen: u64) {
        self.clock = self.clock.max(seen);
    }

    pub fn interface_id(&self) -> Option<&str> {
        self.interface_id.as_deref()
    }

    pub fn set_interface_id(&mut self, interface_id: &str) {
        self.interface_id = Some(interface_id.to_owned());
    }

    /// A copy to draw from and never save: each group replaced by `preview`'s answer for it,
    /// the enabled group and the network left as they are.
    pub(crate) fn previewed(&self, preview: impl FnMut(&GroupRecord) -> GroupRecord) -> Self {
        Self {
            groups: self.groups.iter().map(preview).collect(),
            ..self.clone()
        }
    }

    /// Forgets a paired computer. Each group naming it gives way to the group without it: the
    /// record held for those members when there is one, else one derived as a local change. An
    /// enabled computer leaves the enabled group, whose record is then touched.
    pub fn forget(&mut self, fingerprint: &str) {
        let key = fingerprint_key(fingerprint);
        let was_enabled = self.enabled.contains(&key);
        self.enabled.retain(|peer| *peer != key);
        let Some(local) = self.local.clone() else {
            return;
        };
        let (naming, mut kept): (Vec<GroupRecord>, Vec<GroupRecord>) =
            std::mem::take(&mut self.groups)
                .into_iter()
                .partition(|group| group.has_member(&key));
        let revision = self.clock.saturating_add(1);
        let mut touched = false;
        for group in &naming {
            let Some(derived) = group.without_member(&key, revision, &local) else {
                continue;
            };
            if !kept
                .iter()
                .any(|held| held.member_keys() == derived.member_keys())
            {
                kept.push(derived);
                touched = true;
            }
        }
        let active = was_enabled.then(|| self.active_members()).flatten();
        if let Some(held) = kept
            .iter_mut()
            .find(|group| Some(group.member_keys()) == active)
            && let Ok(record) = held.restamped(revision, &local)
        {
            *held = record;
            touched = true;
        }
        kept.sort_by_cached_key(GroupRecord::member_keys);
        self.groups = kept;
        if touched {
            self.clock = revision;
        }
    }

    /// Records name this computer by one identity; those of an earlier one can never be used.
    fn set_local(&mut self, local: &str) {
        let key = fingerprint_key(local);
        if self.local.as_deref().map(fingerprint_key) == Some(key.clone()) {
            return;
        }
        let held = self.groups.len();
        self.groups.retain(|group| group.has_member(&key));
        if self.groups.len() < held {
            log::warn!(
                "setup: dropped {} saved layouts of an earlier identity of this computer",
                held - self.groups.len()
            );
        }
        self.enabled.retain(|peer| *peer != key);
        self.local = Some(key.to_ascii_uppercase());
    }

    /// Sorted lowercase fingerprints of this computer and the enabled computers.
    fn active_members(&self) -> Option<Vec<String>> {
        if self.enabled.is_empty() {
            return None;
        }
        let mut members = self.enabled.clone();
        members.push(fingerprint_key(self.local.as_deref()?));
        members.sort();
        Some(members)
    }

    /// Replaces the record held for `record`'s members, then drops the oldest inactive records
    /// past the bound.
    fn store(&mut self, record: GroupRecord) {
        let members = record.member_keys();
        self.groups.retain(|group| group.member_keys() != members);
        self.groups.push(record);
        self.groups.sort_by_cached_key(GroupRecord::member_keys);
        let active = self.active_members();
        while self.groups.len() > MAX_GROUPS {
            let Some(oldest) = self
                .groups
                .iter()
                .enumerate()
                .filter(|(_, group)| active.as_ref() != Some(&group.member_keys()))
                .min_by_key(|(_, group)| group.stamp())
                .map(|(index, _)| index)
            else {
                break;
            };
            self.groups.remove(oldest);
        }
    }

    fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != SETUP_FILE_VERSION
            || self.groups.len() > MAX_GROUPS
            || self.enabled.len() >= MAX_GROUP_MEMBERS
            || self.enabled.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .interface_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_INTERFACE_ID_BYTES)
        {
            return Err(PreferenceError::Invalid);
        }
        if let Some(local) = &self.local {
            validate_fingerprint(local)?;
        }
        let local = self.local.as_deref().map(fingerprint_key);
        for peer in &self.enabled {
            validate_fingerprint(&peer.to_ascii_uppercase())?;
            if fingerprint_key(peer) != *peer || local.as_ref() == Some(peer) {
                return Err(PreferenceError::Invalid);
            }
        }
        let mut previous: Option<Vec<String>> = None;
        for group in &self.groups {
            group.validate()?;
            let members = group.member_keys();
            if !local.as_ref().is_some_and(|local| group.has_member(local))
                || group.revision() > self.clock
                || previous
                    .as_ref()
                    .is_some_and(|previous| *previous >= members)
            {
                return Err(PreferenceError::Invalid);
            }
            previous = Some(members);
        }
        Ok(())
    }
}

impl SetupFileV3 {
    fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != SETUP_FILE_V3_VERSION
            || self.computers.len() > MAX_COMPUTERS
            || self
                .interface_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_INTERFACE_ID_BYTES)
        {
            return Err(PreferenceError::Invalid);
        }
        if let Some(active) = &self.active {
            validate_fingerprint(&active.to_ascii_uppercase())?;
            if active.to_ascii_lowercase() != *active {
                return Err(PreferenceError::Invalid);
            }
        }
        for (key, setup) in &self.computers {
            setup.validate()?;
            if *key != fingerprint_key(&setup.peer_fingerprint) {
                return Err(PreferenceError::Invalid);
            }
        }
        Ok(())
    }

    /// This computer is `identity`, its identity store's; only when that could not be read, the
    /// identity most records name, the active record's on a tie. Every record of it becomes a
    /// two-member group with its layout verbatim, stamped so that both computers' copies of one
    /// pair agree (`GroupRecord::from_pairwise`).
    fn migrate(
        self,
        platform: Platform,
        listed: &BTreeMap<String, Platform>,
        identity: Option<&str>,
    ) -> SetupFile {
        let active = self
            .active
            .as_ref()
            .and_then(|active| self.computers.get(active));
        let mut votes: BTreeMap<&str, usize> = BTreeMap::new();
        for record in self.computers.values() {
            *votes.entry(record.local_fingerprint.as_str()).or_default() += 1;
        }
        let most = votes.values().copied().max().unwrap_or_default();
        let local = identity
            .and_then(|identity| CertificateFingerprint::parse_full(identity).ok())
            .map(|identity| identity.full_hex())
            .or_else(|| {
                active
                    .map(|record| record.local_fingerprint.as_str())
                    .filter(|local| votes.get(local) == Some(&most))
                    .or_else(|| {
                        votes
                            .iter()
                            .find(|(_, count)| **count == most)
                            .map(|(local, _)| *local)
                    })
                    .map(str::to_owned)
            });
        let interface_id = self
            .interface_id
            .clone()
            .or_else(|| active.map(|record| record.interface_id.clone()));
        let (mut earlier_identity, mut unusable) = (0_usize, 0_usize);
        let mut groups = Vec::new();
        for record in self.computers.values() {
            if Some(&record.local_fingerprint) != local.as_ref() {
                earlier_identity += 1;
                continue;
            }
            let peer_platform = listed_platform(listed, &record.peer_fingerprint, platform);
            match GroupRecord::from_pairwise(record, platform, peer_platform) {
                Ok(group) => groups.push(group),
                Err(_) => unusable += 1,
            }
        }
        if earlier_identity > 0 {
            log::warn!(
                "setup: dropped {earlier_identity} saved layouts of an earlier identity of this computer"
            );
        }
        if unusable > 0 {
            log::warn!("setup: dropped {unusable} saved layouts that could not be carried over");
        }
        groups.sort_by_cached_key(GroupRecord::member_keys);
        let local_key = local.as_deref().map(fingerprint_key);
        SetupFile {
            version: SETUP_FILE_VERSION,
            interface_id,
            enabled: self
                .active
                .into_iter()
                .filter(|peer| Some(peer) != local_key.as_ref())
                .collect(),
            local,
            paused: false,
            clock: MIGRATED_CLOCK,
            groups,
            newer: false,
        }
    }
}

/// Each paired computer's platform as the computer list beside `path` records it, keyed by
/// lowercase fingerprint; empty when the list is absent or unreadable.
pub(crate) fn listed_platforms(path: &Path) -> BTreeMap<String, Platform> {
    ComputerList::load(&path.with_file_name(LIST_FILE))
        .map(|list| list.platforms())
        .unwrap_or_default()
}

/// The platform a migrated pairwise record gives the other computer of a pair made on a
/// `local` computer: the one the computer list records, else the other platform.
pub(crate) fn listed_platform(
    listed: &BTreeMap<String, Platform>,
    peer: &str,
    local: Platform,
) -> Platform {
    listed
        .get(&fingerprint_key(peer))
        .copied()
        .unwrap_or_else(|| opposite_platform(local))
}

/// This computer's identity as its identity store holds it. Once found it is kept for the
/// process: an identity is only ever created where there was none, never replaced.
#[cfg(not(test))]
fn stored_identity() -> Option<String> {
    use monhop_transport::{identity_store::load_identity, native_storage::NativeIdentityStore};
    static FOUND: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(found) = FOUND.get() {
        return Some(found.clone());
    }
    match load_identity(&NativeIdentityStore) {
        Ok(Some(identity)) => Some(
            FOUND
                .get_or_init(|| identity.fingerprint().full_hex())
                .clone(),
        ),
        Ok(None) => None,
        Err(error) => {
            log::warn!("setup: this computer's identity could not be read to migrate: {error}");
            None
        }
    }
}

/// Tests never read this computer's identity store.
#[cfg(test)]
fn stored_identity() -> Option<String> {
    None
}

/// The lowercase form every view and file key uses; the input must already be a valid fingerprint.
pub(crate) fn fingerprint_key(fingerprint: &str) -> String {
    fingerprint.to_ascii_lowercase()
}

/// Accepts a fingerprint in any case from a command and returns its parsed form.
pub(crate) fn parse_fingerprint(value: &str) -> Result<CertificateFingerprint, String> {
    CertificateFingerprint::parse_full(value.trim())
        .map_err(|_| "That computer identity is not valid.".to_owned())
}

/// Both directions allowed: the control map of a pair that has no record yet.
#[cfg(test)]
pub(crate) fn both_directions(local_fingerprint: &str, peer_fingerprint: &str) -> ControlMap {
    [local_fingerprint, peer_fingerprint]
        .into_iter()
        .map(|fingerprint| (fingerprint_key(fingerprint), true))
        .collect()
}

/// Exactly the two computers of the pair, by full lowercase fingerprint, and at least one allowed.
pub(crate) fn validate_control(
    control: &ControlMap,
    local_fingerprint: &str,
    peer_fingerprint: &str,
) -> Result<(), PreferenceError> {
    let local = fingerprint_key(local_fingerprint);
    let peer = fingerprint_key(peer_fingerprint);
    if local == peer
        || control.len() != 2
        || !control.contains_key(&local)
        || !control.contains_key(&peer)
        || !control.values().any(|allowed| *allowed)
    {
        return Err(PreferenceError::Invalid);
    }
    Ok(())
}

/// The computer with the lower DeviceId decides layout changes; both computers agree on which.
pub(crate) fn local_decides(inspection: &InspectedPeer) -> bool {
    inspection.local_device < inspection.peer_device
}

/// A short, stable digest for log lines; it names no identity and no input.
fn digest(hash: impl FnOnce(&mut DefaultHasher)) -> String {
    let mut hasher = DefaultHasher::new();
    hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

fn hash_displays(displays: &[DisplaySnapshot], hasher: &mut impl Hasher) {
    for display in displays {
        hasher.write(display.id.as_bytes());
        for value in display.origin.iter().chain(&display.size) {
            hasher.write_u64(value.to_bits());
        }
        hasher.write_u32(display.scale.to_bits());
        hasher.write_u8(u8::from(display.primary));
    }
}

/// "2 displays #1a2b3c4d": a side's display count and geometry digest, for log lines.
pub(crate) fn describe_displays(displays: &[DisplaySnapshot]) -> String {
    let count = displays.len();
    let noun = if count == 1 { "display" } else { "displays" };
    format!(
        "{count} {noun} #{}",
        digest(|hasher| hash_displays(displays, hasher))
    )
}

/// Where the arrangement moved one computer's block: its anchor display's position minus that
/// display's OS origin. The anchor is the primary display when placed, else the lowest placed id.
pub(crate) fn block_translation(
    arrangement: &ArrangementRequest,
    displays: &[DisplaySnapshot],
) -> Option<[f64; 2]> {
    let placed = |display: &DisplaySnapshot| {
        arrangement
            .positions
            .iter()
            .find(|position| position.display == display.id)
    };
    let numeric =
        |display: &&DisplaySnapshot| parse_display(&display.id).map_or(u64::MAX, |id| id.0);
    let anchor = displays
        .iter()
        .find(|display| display.primary && placed(display).is_some())
        .or_else(|| {
            displays
                .iter()
                .filter(|display| placed(display).is_some())
                .min_by_key(numeric)
        })?;
    let position = placed(anchor)?;
    Some([position.x - anchor.origin[0], position.y - anchor.origin[1]])
}

/// The displays both computers show while one of them is connected, so a card can draw what is
/// there now instead of what was there at the last Apply.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDisplays {
    local_displays: Vec<DisplaySnapshot>,
    peer_displays: Vec<DisplaySnapshot>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSetupView {
    saved: bool,
    local_displays: Vec<DisplaySnapshot>,
    peer_displays: Vec<DisplaySnapshot>,
    revision: String,
    layout: Option<LayoutRequest>,
    preview_layout: Option<LayoutRequest>,
    /// Null unless this computer is the one with a live link or session.
    live: Option<LiveDisplays>,
    message: &'static str,
    /// Every computer the record names, this one included, with the displays it holds for each.
    members: Vec<MemberDisplays>,
    /// Null without a record.
    record_revision: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberDisplays {
    /// Lowercase.
    fingerprint: String,
    displays: Vec<DisplaySnapshot>,
}

impl SavedSetupView {
    /// The card for `peer`: `saved` is the record this computer (`local`) keeps with it, and
    /// `inspection` must be the live inspection of that same computer.
    pub fn from_group(
        saved: Option<&GroupRecord>,
        local: Option<&str>,
        peer: &str,
        inspection: Option<&InspectedPeer>,
        revision: &str,
    ) -> Self {
        let fits = saved
            .zip(inspection)
            .is_some_and(|(saved, inspection)| saved.fits_link(inspection));
        Self {
            live: inspection.map(|inspection| LiveDisplays {
                local_displays: snapshots(&inspection.local_displays),
                peer_displays: snapshots(&inspection.peer_displays),
            }),
            ..Self::drawn(saved, local, Some(peer), fits, revision)
        }
    }

    /// The active group's picture. `live` are this computer's live connections; a pair draws
    /// exactly as its card does, and a larger group fits while every connection it names does.
    pub fn for_group(
        record: &GroupRecord,
        local: Option<&str>,
        live: &[&InspectedPeer],
        revision: &str,
    ) -> Self {
        let local_key = local.map(fingerprint_key);
        let others: Vec<String> = record
            .member_keys()
            .into_iter()
            .filter(|member| Some(member) != local_key.as_ref())
            .collect();
        let with = |member: &str| {
            live.iter().copied().find(|inspection| {
                fingerprint_key(&inspection.peer_fingerprint.full_hex()) == member
            })
        };
        if let [peer] = others.as_slice() {
            return Self::from_group(Some(record), local, peer, with(peer), revision);
        }
        let named: Vec<&InspectedPeer> = others.iter().filter_map(|member| with(member)).collect();
        let fits = !named.is_empty() && named.iter().all(|inspection| record.fits_link(inspection));
        Self::drawn(Some(record), local, None, fits, revision)
    }

    fn drawn(
        saved: Option<&GroupRecord>,
        local: Option<&str>,
        peer: Option<&str>,
        fits: bool,
        revision: &str,
    ) -> Self {
        let message = if fits {
            "Saved layout fits the connected displays."
        } else if saved.is_some() {
            "Layout saved. The displays are checked when this computer connects."
        } else {
            "No layout yet. Arrange the displays to start sharing."
        };
        let displays = |member: Option<&str>| {
            saved
                .zip(member)
                .and_then(|(saved, member)| saved.member(member))
                .map(|member| member.displays().to_vec())
                .unwrap_or_default()
        };
        Self {
            saved: saved.is_some(),
            local_displays: displays(local),
            peer_displays: displays(peer),
            revision: revision.to_owned(),
            layout: saved.filter(|_| fits).map(|saved| saved.layout().clone()),
            preview_layout: saved.map(|saved| saved.layout().clone()),
            live: None,
            message,
            members: saved
                .map(|saved| {
                    saved
                        .members()
                        .iter()
                        .map(|member| MemberDisplays {
                            fingerprint: fingerprint_key(member.fingerprint()),
                            displays: member.displays().to_vec(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            record_revision: saved.map(GroupRecord::revision),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreferenceError {
    Invalid,
    InspectionChanged,
}

impl std::fmt::Display for PreferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "The saved sharing setup is invalid.",
            Self::InspectionChanged => {
                "The saved sharing setup no longer matches the paired computers."
            }
        })
    }
}

impl std::error::Error for PreferenceError {}

impl SharingPreferences {
    /// Whether this computer decided layout changes for the pair, as `local_decides` does live.
    pub(crate) fn local_decides(&self) -> bool {
        let device = |fingerprint: &str| {
            CertificateFingerprint::parse_full(fingerprint)
                .ok()
                .map(device_id_from_fingerprint)
        };
        device(&self.local_fingerprint) < device(&self.peer_fingerprint)
    }

    pub(crate) fn validate(&self) -> Result<(), PreferenceError> {
        if self.version != SHARING_PREFERENCES_VERSION
            || self.interface_id.is_empty()
            || self.interface_id.len() > MAX_INTERFACE_ID_BYTES
        {
            return Err(PreferenceError::Invalid);
        }
        validate_fingerprint(&self.local_fingerprint)?;
        validate_fingerprint(&self.peer_fingerprint)?;
        validate_displays(&self.local_displays)?;
        validate_displays(&self.peer_displays)?;
        validate_control(
            &self.layout.control,
            &self.local_fingerprint,
            &self.peer_fingerprint,
        )?;
        validate_layout(&self.layout, &self.local_displays, &self.peer_displays)
    }
}

/// The displays a display notice was raised for, so one change raises it once.
#[derive(Clone, Debug)]
pub(crate) struct DisplayGeometry {
    local: Vec<DisplaySnapshot>,
    peer: Vec<DisplaySnapshot>,
}

impl DisplayGeometry {
    pub(crate) fn of(inspection: &InspectedPeer) -> Self {
        Self {
            local: snapshots(&inspection.local_displays),
            peer: snapshots(&inspection.peer_displays),
        }
    }

    /// Both sides for a log line: counts and geometry digests only.
    pub(crate) fn describe(&self) -> String {
        format!(
            "this computer {}, other computer {}",
            describe_displays(&self.local),
            describe_displays(&self.peer)
        )
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        same_display_geometry(&self.local, &other.local)
            && same_display_geometry(&self.peer, &other.peer)
    }
}

/// A regular file of at most `limit` bytes, or None when absent; symlinks and oversize fail.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file(&metadata)?;
    if metadata.len() > limit {
        return Err(invalid_data());
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid_data());
    }
    Ok(Some(bytes))
}

/// Reads, parses, and validates a bounded JSON file; an absent file is the default value.
pub(crate) fn load_bounded<T: DeserializeOwned + Default>(
    path: &Path,
    limit: u64,
    validate: impl FnOnce(&T) -> io::Result<()>,
) -> io::Result<T> {
    let Some(bytes) = read_bounded(path, limit)? else {
        return Ok(T::default());
    };
    let value: T = serde_json::from_slice(&bytes).map_err(|_| invalid_data())?;
    validate(&value)?;
    Ok(value)
}

/// The version a versioned JSON file names, read before anything else in it.
pub(crate) fn file_version(bytes: &[u8]) -> io::Result<u8> {
    serde_json::from_slice::<VersionedFile>(bytes)
        .map(|file| file.version)
        .map_err(|_| invalid_data())
}

/// What saving over a file a newer MonHop wrote fails with.
pub(crate) fn newer_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the file was written by a newer MonHop",
    )
}

// The setup file, the computer list, and the arrangement library share one bounded atomic write.
pub(crate) fn save_metadata(path: &Path, bytes: &[u8]) -> io::Result<()> {
    save_bounded(path, bytes, MAX_SHARING_PREFERENCES_BYTES)
}

/// Writes through a temporary file and a rename, so the target is whole or untouched.
pub(crate) fn save_bounded(path: &Path, bytes: &[u8], limit: u64) -> io::Result<()> {
    if bytes.len() as u64 > limit {
        return Err(invalid_data());
    }
    let parent = preference_parent(path);
    validate_parent(parent)?;
    validate_existing_target(path)?;

    let (temporary_path, mut temporary) = create_temporary_file(parent)?;
    let write_result = (|| -> io::Result<()> {
        temporary.write_all(bytes)?;
        temporary.sync_all()
    })();
    drop(temporary);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn same_display_geometry(left: &[DisplaySnapshot], right: &[DisplaySnapshot]) -> bool {
    left.len() == right.len()
        && left.iter().all(|display| {
            right.iter().any(|candidate| {
                display.id == candidate.id
                    && display.origin == candidate.origin
                    && display.size == candidate.size
                    && display.native_size == candidate.native_size
                    && display.scale == candidate.scale
                    && display.primary == candidate.primary
            })
        })
}

/// Everything but the id, name, and monitor identity: what a layout's crossings depend on.
pub(crate) fn same_geometry(left: &DisplaySnapshot, right: &DisplaySnapshot) -> bool {
    left.origin == right.origin
        && left.size == right.size
        && left.native_size == right.native_size
        && left.scale == right.scale
        && left.primary == right.primary
}

/// The monitor's EDID identity when this list reports it for exactly one display. Two identical
/// monitors without serial numbers share one, which tells them apart no better than their ids.
fn usable_monitor<'a>(display: &'a DisplaySnapshot, list: &[DisplaySnapshot]) -> Option<&'a str> {
    let key = display.monitor.as_deref()?;
    (list
        .iter()
        .filter(|other| other.monitor.as_deref() == Some(key))
        .count()
        == 1)
        .then_some(key)
}

/// Pairs remembered displays with the live displays that are the same monitor: by EDID identity
/// where both lists report one for exactly one display, otherwise by OS id. Two displays that
/// report different identities never pair, even when one has taken the other's id.
pub(crate) fn pair_displays<'a>(
    remembered: &'a [DisplaySnapshot],
    live: &'a [DisplaySnapshot],
) -> Vec<(&'a DisplaySnapshot, &'a DisplaySnapshot)> {
    let mut pairs: Vec<(&DisplaySnapshot, &DisplaySnapshot)> = Vec::new();
    let mut paired = vec![false; remembered.len()];
    for (index, display) in remembered.iter().enumerate() {
        let Some(key) = usable_monitor(display, remembered) else {
            continue;
        };
        if let Some(candidate) = live
            .iter()
            .find(|candidate| usable_monitor(candidate, live) == Some(key))
        {
            pairs.push((display, candidate));
            paired[index] = true;
        }
    }
    for (index, display) in remembered.iter().enumerate() {
        if paired[index] {
            continue;
        }
        let by_id = live.iter().find(|candidate| {
            candidate.id == display.id
                && !pairs.iter().any(|(_, taken)| taken.id == candidate.id)
                && (display.monitor.is_none()
                    || candidate.monitor.is_none()
                    || display.monitor == candidate.monitor)
        });
        if let Some(candidate) = by_id {
            pairs.push((display, candidate));
        }
    }
    pairs
}

/// Every display on both sides paired with the same monitor on the other, whatever the geometry.
pub(crate) fn same_displays<'a>(
    remembered: &'a [DisplaySnapshot],
    live: &'a [DisplaySnapshot],
) -> Option<Vec<(&'a DisplaySnapshot, &'a DisplaySnapshot)>> {
    let pairs = pair_displays(remembered, live);
    (remembered.len() == live.len() && pairs.len() == remembered.len()).then_some(pairs)
}

/// Remembered id to live id, for every paired display.
pub(crate) fn id_map<'a>(
    pairs: impl Iterator<Item = &'a (&'a DisplaySnapshot, &'a DisplaySnapshot)>,
) -> BTreeMap<String, String> {
    pairs
        .map(|(remembered, live)| (remembered.id.clone(), live.id.clone()))
        .collect()
}

/// The layout with every display id it names rewritten through `map`; unmapped ids stand.
pub(crate) fn remap_layout(
    layout: &LayoutRequest,
    map: &BTreeMap<String, String>,
) -> LayoutRequest {
    let rename = |id: &String| map.get(id).cloned().unwrap_or_else(|| id.clone());
    let mut remapped = layout.clone();
    for link in &mut remapped.links {
        link.from_display = rename(&link.from_display);
        link.to_display = rename(&link.to_display);
    }
    if let Some(arrangement) = remapped.arrangement.as_mut() {
        for position in &mut arrangement.positions {
            position.display = rename(&position.display);
        }
        for hidden in &mut arrangement.hidden {
            *hidden = rename(hidden);
        }
    }
    remapped
}

pub(crate) fn snapshots(topology: &DisplayTopology) -> Vec<DisplaySnapshot> {
    topology
        .displays()
        .iter()
        .map(|display| DisplaySnapshot {
            id: display.id.0.to_string(),
            name: display.name.clone(),
            origin: [display.logical_origin.x, display.logical_origin.y],
            size: [display.logical_size.x, display.logical_size.y],
            native_size: [display.native_width, display.native_height],
            scale: display.scale_factor,
            primary: display.is_primary,
            monitor: crate::sharing::monitor_key(display),
        })
        .collect()
}

/// The inverse of `snapshots`: saved displays as the topology an inspection carries.
pub(crate) fn topology_of(displays: &[DisplaySnapshot]) -> Option<DisplayTopology> {
    let displays = displays
        .iter()
        .map(|display| {
            Some(DisplayDescription {
                id: parse_display(&display.id).ok()?,
                name: display.name.clone(),
                native_width: display.native_size[0],
                native_height: display.native_size[1],
                logical_origin: Point::new(display.origin[0], display.origin[1]),
                logical_size: Point::new(display.size[0], display.size[1]),
                scale_factor: display.scale,
                is_primary: display.primary,
                monitor: display.monitor.as_deref().and_then(monitor_from_key),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    DisplayTopology::new(displays).ok()
}

/// The identity `crate::sharing::monitor_key` writes, read back.
fn monitor_from_key(key: &str) -> Option<MonitorIdentity> {
    let mut parts = key.split('-');
    let vendor = u16::from_str_radix(parts.next()?, 16).ok()?;
    let product = u16::from_str_radix(parts.next()?, 16).ok()?;
    let serial = u32::from_str_radix(parts.next()?, 16).ok()?;
    MonitorIdentity::new(vendor, product, serial)
}

pub(crate) fn validate_fingerprint(value: &str) -> Result<(), PreferenceError> {
    let fingerprint =
        CertificateFingerprint::parse_full(value).map_err(|_| PreferenceError::Invalid)?;
    if fingerprint.full_hex() == value {
        Ok(())
    } else {
        Err(PreferenceError::Invalid)
    }
}

pub(crate) fn validate_displays(displays: &[DisplaySnapshot]) -> Result<(), PreferenceError> {
    if displays.is_empty() || displays.len() > monhop_core::MAX_DISPLAYS {
        return Err(PreferenceError::Invalid);
    }
    let mut ids = BTreeSet::new();
    let mut primary_count = 0_u8;
    for display in displays {
        let id = parse_display(&display.id).map_err(|_| PreferenceError::Invalid)?;
        if id.0.to_string() != display.id
            || !ids.insert(id)
            || display.name.len() > MAX_DISPLAY_NAME_BYTES
            || display.native_size[0] == 0
            || display.native_size[1] == 0
            || display.native_size[0] > MAX_NATIVE_DIMENSION
            || display.native_size[1] > MAX_NATIVE_DIMENSION
            || !display.origin.iter().all(|value| value.is_finite())
            || !display.size.iter().all(|value| value.is_finite())
            || !display.scale.is_finite()
            || display.origin[0].abs() > MAX_LOGICAL_ORIGIN_ABS
            || display.origin[1].abs() > MAX_LOGICAL_ORIGIN_ABS
            || display.size[0] <= 0.0
            || display.size[1] <= 0.0
            || display.size[0] > MAX_LOGICAL_SIZE
            || display.size[1] > MAX_LOGICAL_SIZE
            || !(MIN_SCALE_FACTOR..=MAX_SCALE_FACTOR).contains(&display.scale)
            || !(display.origin[0] + display.size[0]).is_finite()
            || !(display.origin[1] + display.size[1]).is_finite()
        {
            return Err(PreferenceError::Invalid);
        }
        primary_count += u8::from(display.primary);
    }
    if primary_count == 1 {
        Ok(())
    } else {
        Err(PreferenceError::Invalid)
    }
}

fn validate_layout(
    layout: &LayoutRequest,
    local_displays: &[DisplaySnapshot],
    peer_displays: &[DisplaySnapshot],
) -> Result<(), PreferenceError> {
    if layout.links.len() > MAX_LINKS || layout.links.iter().any(|link| parse_link(link).is_err()) {
        return Err(PreferenceError::Invalid);
    }
    let arranged: Vec<crate::sharing::ArrangedDisplay<'_>> = local_displays
        .iter()
        .map(|display| (display, true))
        .chain(peer_displays.iter().map(|display| (display, false)))
        .map(|(display, local)| crate::sharing::ArrangedDisplay {
            id: &display.id,
            local,
        })
        .collect();
    crate::sharing::validate_arrangement(layout, &arranged).map_err(|_| PreferenceError::Invalid)
}

fn preference_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn validate_parent(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data());
    }
    Ok(())
}

fn validate_existing_target(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_file(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_regular_file(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data());
    }
    Ok(())
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMPORARY_FILE_ATTEMPTS {
        let sequence = NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".monhop-sharing-{}-{sequence}.tmp",
            std::process::id()
        ));
        let options = temporary_open_options();
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a sharing preferences temporary file",
    ))
}

fn temporary_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options
}

fn invalid_data() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid sharing setup file")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::group_record::{
        AdaptedGroup, GroupMember, KnownDisplays, shared_group_bytes, shared_group_for_link,
        wire_control,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "monhop-sharing-preferences-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("test directory must be created");
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl SharingPreferences {
        /// Replaces this computer's displays with one 1920x1080 display per id, stacked so the
        /// side edges the fixture layout crosses on stay free.
        pub(crate) fn set_local_displays_for_test(&mut self, ids: &[&str]) {
            self.local_displays = ids
                .iter()
                .enumerate()
                .map(|(index, id)| DisplaySnapshot {
                    id: (*id).to_owned(),
                    name: format!("Display {id}"),
                    origin: [0.0, 1080.0 * index as f64],
                    size: [1920.0, 1080.0],
                    native_size: [1920, 1080],
                    scale: 1.0,
                    primary: index == 0,
                    monitor: None,
                })
                .collect();
        }

        pub(crate) fn set_local_display_names_for_test(&mut self, name: &str) {
            for display in &mut self.local_displays {
                display.name = name.to_owned();
            }
        }

        /// Gives this computer's displays, in order, the monitor identities `keys` name.
        pub(crate) fn set_local_display_monitors_for_test(&mut self, keys: &[&str]) {
            for (display, key) in self.local_displays.iter_mut().zip(keys) {
                display.monitor = Some((*key).to_owned());
            }
        }

        /// The display list alone with `from` renumbered, as the OS does at a reconnect; the
        /// layout is left naming the old id.
        pub(crate) fn set_local_display_id_for_test(&mut self, from: &str, to: &str) {
            for display in &mut self.local_displays {
                if display.id == from {
                    display.id = to.to_owned();
                }
            }
        }

        /// Another paired computer; the control map follows so the record stays valid.
        pub(crate) fn set_peer_fingerprint_for_test(&mut self, fingerprint_char: char) {
            self.peer_fingerprint = fingerprint_char.to_string().repeat(64);
            self.layout.control = both_directions(&self.local_fingerprint, &self.peer_fingerprint);
        }

        pub(crate) fn move_local_display_for_test(&mut self, id: &str, origin: [f64; 2]) {
            for display in &mut self.local_displays {
                if display.id == id {
                    display.origin = origin;
                }
            }
        }
    }

    pub(crate) const BUILT_IN: &str = "0610-a050-00000000";
    pub(crate) const EXTERNAL: &str = "10ac-4173-00000001";

    /// A Mac with its built-in display and one external monitor, crossed to from the other
    /// computer's display 2; the external is display 3 until the OS renumbers it.
    pub(crate) fn two_display_record() -> SharingPreferences {
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.set_local_display_monitors_for_test(&[BUILT_IN, EXTERNAL]);
        record.layout.links = vec![
            link("2", "left", "3", "right"),
            link("3", "right", "2", "left"),
        ];
        record.validate().expect("the fixture is a valid record");
        record
    }

    /// The displays `now`'s link shows: both computers of the pair.
    fn shown(now: &SharingPreferences) -> KnownDisplays {
        KnownDisplays::of_link(&inspection(now))
    }

    /// `record`'s pair rebuilt for the displays `now`'s link shows.
    fn adapted(record: &SharingPreferences, now: &SharingPreferences) -> Option<AdaptedGroup> {
        group(record).adapt(&shown(now))
    }

    #[test]
    fn a_reconnected_monitor_with_a_new_id_fits_through_its_identity() {
        let record = two_display_record();
        let saved = group(&record);
        let mut live = record.clone();
        live.set_local_display_id_for_test("3", "7");
        let inspected = inspection(&live);
        assert!(!saved.fits_link(&inspected));
        let fitted = saved
            .remap_to(&shown(&live))
            .expect("the same monitors fit under new ids");
        assert!(fitted.fits_link(&inspected));
        assert_eq!(fitted.layout().links[0].to_display, "7");
        assert_eq!(fitted.layout().links[1].from_display, "7");
        assert!(saved.same_monitors_as(&fitted));
        assert!(saved.shows_same_monitors(&shown(&live)));
        // Without identities, only the id can tell displays apart, as before.
        let mut anonymous = record.clone();
        for display in &mut anonymous.local_displays {
            display.monitor = None;
        }
        assert_eq!(group(&anonymous).remap_to(&shown(&live)), None);
        assert!(group(&anonymous).remap_to(&shown(&anonymous)).is_some());
    }

    #[test]
    fn a_different_monitor_on_a_reused_id_is_a_new_display() {
        let record = two_display_record();
        let mut live = record.clone();
        live.local_displays[1].monitor = Some("04d9-0001-00000000".into());
        assert_eq!(group(&record).remap_to(&shown(&live)), None);
        assert!(!group(&record).shows_same_monitors(&shown(&live)));
        // The crossing led to the monitor that left; with none left, nothing can continue.
        live.move_local_display_for_test("3", [0.0, 1440.0]);
        assert!(adapted(&record, &live).is_none());
    }

    #[test]
    fn identical_monitors_without_serials_are_told_apart_by_id() {
        let mut record = two_display_record();
        record.set_local_display_monitors_for_test(&[EXTERNAL, EXTERNAL]);
        assert!(group(&record).remap_to(&shown(&record)).is_some());
        let mut renumbered = record.clone();
        renumbered.set_local_display_id_for_test("3", "7");
        assert_eq!(group(&record).remap_to(&shown(&renumbered)), None);
    }

    #[test]
    fn a_duplicate_identity_never_pairs_with_a_different_monitor_on_its_id() {
        // Two identical monitors without serials, then the second swapped for another model
        // that took over its id: the crossing made for the old one must not carry onto it.
        let mut record = two_display_record();
        record.set_local_display_monitors_for_test(&[EXTERNAL, EXTERNAL]);
        let mut live = record.clone();
        live.local_displays[1].monitor = Some("04d9-0001-00000000".into());
        assert_eq!(group(&record).remap_to(&shown(&live)), None);
        live.move_local_display_for_test("3", [0.0, 1440.0]);
        assert!(adapted(&record, &live).is_none());
        // The identical monitor that stayed keeps pairing by id.
        let mut still = record.clone();
        still.local_displays[1].monitor = None;
        assert!(group(&record).remap_to(&shown(&still)).is_some());
    }

    #[test]
    fn displays_that_appear_on_both_computers_join_their_own_blocks() {
        let record = placed_record((1920.0, 0.0));
        let mut changed = record.clone();
        // 9 appears under display 1 here and 5 on the other computer; each joins its own block.
        // Overlapping pictures route nothing, since every crossing is explicit.
        changed.set_local_displays_for_test(&["1", "9"]);
        changed.peer_displays.push(DisplaySnapshot {
            id: "5".into(),
            name: "Second peer display".into(),
            origin: [-1920.0, 1440.0],
            size: [1920.0, 1080.0],
            native_size: [1920, 1080],
            scale: 1.0,
            primary: false,
            monitor: None,
        });
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &changed).expect("newcomers on both computers keep the layout going");
        assert!(!left_out);
        let (positions, hidden) = arrangement_of(&adapted);
        assert!(hidden.is_empty());
        let mut placed: Vec<(&str, f64, f64)> = positions
            .iter()
            .map(|position| (position.display.as_str(), position.x, position.y))
            .collect();
        placed.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(
            placed,
            vec![
                ("1", 0.0, 0.0),
                ("2", 1920.0, 0.0),
                ("5", 0.0, 1440.0),
                ("9", 0.0, 1080.0)
            ]
        );
        assert!(adapted.fits_link(&inspection(&changed)));
    }

    #[test]
    fn a_placed_newcomer_that_breaks_the_topology_is_left_out_instead() {
        // The crossing leaves display 1's whole right edge, while the picture keeps the other
        // computer's display far right. Display 9 appears exactly right of 1: its spot is clear,
        // but its inherited seam would share that edge with the crossing.
        let record = placed_record((5000.0, 0.0));
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1", "9"]);
        changed.move_local_display_for_test("9", [1920.0, 0.0]);
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &changed)
            .expect("the layout without the newcomer is as valid as before");
        assert!(left_out);
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, vec!["9".to_owned()]);
        assert_eq!(positions.len(), 2);
        assert!(adapted.fits_link(&inspection(&changed)));
    }

    #[test]
    fn a_reconnected_monitor_that_moved_keeps_its_crossings() {
        let record = two_display_record();
        let mut live = record.clone();
        live.set_local_display_id_for_test("3", "7");
        live.move_local_display_for_test("7", [1920.0, 0.0]);
        assert_eq!(group(&record).remap_to(&shown(&live)), None);
        assert!(group(&record).shows_same_monitors(&shown(&live)));
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &live).expect("a moved monitor keeps the crossings made for it");
        // Nothing was lost, so this rebuild is silent: no banner follows it.
        assert!(!left_out);
        assert_eq!(adapted.layout().links.len(), 2);
        assert_eq!(adapted.layout().links[0].to_display, "7");
        assert_eq!(
            adapted
                .member(&record.local_fingerprint)
                .unwrap()
                .displays()[1]
                .origin,
            [1920.0, 0.0]
        );
        assert!(adapted.fits_link(&inspection(&live)));
    }

    fn link(from: &str, from_edge: &str, to: &str, to_edge: &str) -> crate::sharing::LinkRequest {
        crate::sharing::LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        }
    }

    #[test]
    fn a_display_that_appeared_keeps_the_grouped_arrangement_going() {
        let record = preferences();
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &changed).expect("a grouped layout survives a display that appeared");
        // A grouped layout moves each computer's displays as one block, so nothing was dropped.
        assert!(!left_out);
        assert_eq!(
            adapted
                .member(&record.local_fingerprint)
                .unwrap()
                .displays()
                .len(),
            2
        );
        assert_eq!(adapted.layout(), &record.layout);
        assert!(adapted.fits_link(&inspection(&changed)));
    }

    fn placed_record(peer_position: (f64, f64)) -> SharingPreferences {
        let mut record = preferences();
        record.layout.arrangement = Some(crate::sharing::ArrangementRequest {
            positions: vec![
                crate::sharing::DisplayPosition {
                    display: "1".into(),
                    x: 0.0,
                    y: 0.0,
                },
                crate::sharing::DisplayPosition {
                    display: "2".into(),
                    x: peer_position.0,
                    y: peer_position.1,
                },
            ],
            hidden: Vec::new(),
        });
        assert!(adapted(&record, &record).is_some());
        record
    }

    fn arrangement_of(
        adapted: &GroupRecord,
    ) -> (Vec<crate::sharing::DisplayPosition>, Vec<String>) {
        let arrangement = adapted
            .layout()
            .arrangement
            .as_ref()
            .expect("the arrangement survives");
        (arrangement.positions.clone(), arrangement.hidden.clone())
    }

    #[test]
    fn a_display_that_appeared_in_an_arrangement_is_placed_where_the_os_puts_it() {
        let record = placed_record((1920.0, 0.0));
        let mut changed = record.clone();
        // The fixture stacks display 3 directly under display 1, as the OS reports it.
        changed.set_local_displays_for_test(&["1", "3"]);
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &changed).expect("a placed layout survives a display that appeared");
        // The newcomer joined its computer's block, so nothing was left out.
        assert!(!left_out);
        let (positions, hidden) = arrangement_of(&adapted);
        assert_eq!(hidden, Vec::<String>::new());
        assert_eq!(positions.len(), 3);
        let newcomer = positions
            .iter()
            .find(|position| position.display == "3")
            .expect("the newcomer is placed");
        assert_eq!((newcomer.x, newcomer.y), (0.0, 1080.0));
        assert!(adapted.fits_link(&inspection(&changed)));
    }

    #[test]
    fn a_crossing_display_that_went_away_stops_the_adaptation() {
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.layout.links = vec![
            link("2", "left", "3", "right"),
            link("3", "right", "2", "left"),
        ];
        assert!(adapted(&record, &record).is_some());
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1"]);
        assert!(adapted(&record, &changed).is_none());
    }

    #[test]
    fn a_display_that_moved_refreshes_the_saved_geometry_and_keeps_sharing() {
        let record = preferences();
        let mut moved = record.clone();
        moved.local_displays[0].origin = [0.0, 240.0];
        let inspected = inspection(&moved);
        assert!(!group(&record).fits_link(&inspected));
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &moved).expect("a display that moved keeps the layout");
        assert!(!left_out);
        assert_eq!(
            adapted
                .member(&record.local_fingerprint)
                .unwrap()
                .displays()[0]
                .origin,
            [0.0, 240.0]
        );
        assert_eq!(adapted.layout(), &record.layout);
        assert!(adapted.fits_link(&inspected));
    }

    #[test]
    fn a_card_draws_the_displays_there_are_now_through_the_adapted_record() {
        let record = preferences();
        let saved = group(&record);
        let file = file_with(record.clone());
        let drawn = |file: &SetupFile, now: &KnownDisplays| {
            let previewed = file.previewed(|saved| saved.preview_for(now));
            let peer = "b".repeat(64);
            let view = SavedSetupView::from_group(
                previewed.group_with(&peer),
                previewed.local(),
                &peer,
                None,
                "0",
            );
            serde_json::to_value(view).unwrap()
        };
        // A record that still fits is drawn exactly as saved.
        assert_eq!(
            saved.preview_for(&KnownDisplays::of_link(&inspection(&record))),
            saved
        );

        // The link reports this computer's display somewhere else than the record saved it.
        let mut moved = record.clone();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let live = KnownDisplays::of_link(&inspection(&moved));
        let preview = saved.preview_for(&live);
        assert_eq!(
            preview,
            saved
                .adapt(&live)
                .expect("a display that moved adapts")
                .record
        );
        let view = drawn(&file, &live);
        assert_eq!(
            view["localDisplays"][0]["origin"],
            serde_json::json!([0.0, 240.0])
        );
        assert_eq!(
            view["previewLayout"]["links"],
            serde_json::to_value(&record.layout.links).unwrap()
        );
        // The saved file itself is untouched.
        assert_eq!(file.active_group(), Some(&saved));

        // Without a link: this computer's displays as read now, the other's as last saved.
        let mut now = KnownDisplays::default();
        now.insert(
            &record.local_fingerprint,
            Platform::MacOs,
            &topology_of(&moved.local_displays).unwrap(),
        );
        let preview = saved.preview_for(&now);
        let displays = |owner: &str| preview.member(owner).unwrap().displays().to_vec();
        assert_eq!(displays(&record.local_fingerprint), moved.local_displays);
        assert_eq!(displays(&record.peer_fingerprint), record.peer_displays);

        // The display every crossing led to went away, so nothing of the record survives: the
        // card draws the displays there are now with no crossing, never the saved ones.
        let mut crossed = preferences();
        crossed.set_local_displays_for_test(&["1", "3"]);
        crossed.layout.links = vec![
            link("2", "left", "3", "right"),
            link("3", "right", "2", "left"),
        ];
        let mut unplugged = crossed.clone();
        unplugged.set_local_displays_for_test(&["1"]);
        let now = KnownDisplays::of_link(&inspection(&unplugged));
        assert!(group(&crossed).adapt(&now).is_none());
        let view = drawn(&file_with(crossed), &now);
        assert_eq!(view["localDisplays"].as_array().unwrap().len(), 1);
        assert_eq!(view["previewLayout"]["links"], serde_json::json!([]));
        assert_eq!(view["saved"], serde_json::json!(true));
    }

    /// A crossing on part of an edge, so one display can lead to two others on the same side.
    fn split_link(
        from: &str,
        from_edge: &str,
        from_span: [f64; 2],
        to: &str,
        to_edge: &str,
        to_span: [f64; 2],
    ) -> crate::sharing::LinkRequest {
        crate::sharing::LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span,
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span,
            hysteresis: 1.0,
        }
    }

    #[test]
    fn a_crossing_that_went_away_with_its_display_is_reported_as_left_out() {
        // The other computer's one display leads to both of this computer's, each over its own
        // half of the edge. The second one goes away, so sharing continues on what is left.
        let mut record = preferences();
        record.set_local_displays_for_test(&["1", "3"]);
        record.layout.links = vec![
            split_link("2", "left", [0.0, 0.45], "1", "right", [0.0, 1.0]),
            split_link("1", "right", [0.0, 1.0], "2", "left", [0.0, 0.45]),
            split_link("2", "left", [0.55, 1.0], "3", "right", [0.0, 1.0]),
            split_link("3", "right", [0.0, 1.0], "2", "left", [0.55, 1.0]),
        ];
        record.validate().expect("the fixture is a valid record");
        assert!(adapted(&record, &record).is_some());
        let mut changed = record.clone();
        changed.set_local_displays_for_test(&["1"]);
        let AdaptedGroup {
            record: adapted,
            left_out,
        } = adapted(&record, &changed).expect("the crossings that are left survive");
        assert!(left_out);
        // The pair of crossings that named the display that went away is gone; the other pair,
        // and sharing with it, carries on.
        assert_eq!(adapted.layout().links.len(), 2);
        assert!(adapted.fits_link(&inspection(&changed)));
    }

    /// The same setup made with another paired computer.
    pub(crate) fn preferences_for_peer(fingerprint_char: char) -> SharingPreferences {
        let mut saved = preferences();
        saved.set_peer_fingerprint_for_test(fingerprint_char);
        saved
    }

    pub(crate) fn preferences() -> SharingPreferences {
        SharingPreferences {
            version: SHARING_PREFERENCES_VERSION,
            interface_id: "en0:4:192.168.1.4".into(),
            local_fingerprint: "A".repeat(64),
            peer_fingerprint: "B".repeat(64),
            local_displays: vec![DisplaySnapshot {
                id: "1".into(),
                name: "Local display".into(),
                origin: [0.0, 0.0],
                size: [1920.0, 1080.0],
                native_size: [1920, 1080],
                scale: 1.0,
                primary: true,
                monitor: None,
            }],
            peer_displays: vec![DisplaySnapshot {
                id: "2".into(),
                name: "Peer display".into(),
                origin: [0.0, 0.0],
                size: [2560.0, 1440.0],
                native_size: [2560, 1440],
                scale: 1.0,
                primary: true,
                monitor: None,
            }],
            layout: LayoutRequest {
                links: vec![
                    crate::sharing::LinkRequest {
                        from_display: "1".into(),
                        from_edge: "right".into(),
                        from_span: [0.0, 1.0],
                        to_display: "2".into(),
                        to_edge: "left".into(),
                        to_span: [0.0, 1.0],
                        hysteresis: 1.0,
                    },
                    crate::sharing::LinkRequest {
                        from_display: "2".into(),
                        from_edge: "left".into(),
                        from_span: [0.0, 1.0],
                        to_display: "1".into(),
                        to_edge: "right".into(),
                        to_span: [0.0, 1.0],
                        hysteresis: 1.0,
                    },
                ],
                arrangement: None,
                control: both_directions(&"A".repeat(64), &"B".repeat(64)),
            },
        }
    }

    /// The link `saved` was made over: this computer a Mac, the other a PC.
    pub(crate) fn inspection(saved: &SharingPreferences) -> InspectedPeer {
        let parsed = |fingerprint: &str| CertificateFingerprint::parse_full(fingerprint).unwrap();
        let (local, peer) = (
            parsed(&saved.local_fingerprint),
            parsed(&saved.peer_fingerprint),
        );
        InspectedPeer {
            local_device: device_id_from_fingerprint(local),
            peer_device: device_id_from_fingerprint(peer),
            local_fingerprint: local,
            peer_fingerprint: peer,
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays: topology_of(&saved.local_displays).unwrap(),
            peer_displays: topology_of(&saved.peer_displays).unwrap(),
            interface_id: saved.interface_id.clone(),
        }
    }

    /// The link between two members of `record` as `local` inspects it, showing exactly their
    /// entries.
    pub(crate) fn link_of(record: &GroupRecord, local: &str, peer: &str) -> InspectedPeer {
        let parsed = |fingerprint: &str| CertificateFingerprint::parse_full(fingerprint).unwrap();
        let entry = |fingerprint: &str| record.member(fingerprint).expect("a member");
        InspectedPeer {
            local_device: device_id_from_fingerprint(parsed(local)),
            peer_device: device_id_from_fingerprint(parsed(peer)),
            local_fingerprint: parsed(local),
            peer_fingerprint: parsed(peer),
            local_platform: entry(local).platform().into(),
            peer_platform: entry(peer).platform().into(),
            local_displays: topology_of(entry(local).displays()).unwrap(),
            peer_displays: topology_of(entry(peer).displays()).unwrap(),
            interface_id: "en0:4:192.168.1.4".into(),
        }
    }

    /// `setup` as the two-member group its `inspection` describes.
    pub(crate) fn group(setup: &SharingPreferences) -> GroupRecord {
        GroupRecord::from_pairwise(setup, Platform::MacOs, Platform::Windows)
            .expect("the fixture is a valid pair")
    }

    fn try_file_with(setup: &SharingPreferences) -> Result<SetupFile, PreferenceError> {
        let mut file = SetupFile::default();
        file.set_interface_id(&setup.interface_id);
        file.store_agreed(
            &setup.local_fingerprint,
            GroupRecord::from_pairwise(setup, Platform::MacOs, Platform::Windows)?,
        )?;
        Ok(file)
    }

    /// A file holding exactly `setup`'s group, enabled, on `setup`'s network.
    pub(crate) fn file_with(setup: SharingPreferences) -> SetupFile {
        try_file_with(&setup).expect("the fixture is a valid pair")
    }

    /// Keeps `setup`'s group beside the others, the enabled group left as it is.
    pub(crate) fn keep_group(file: &mut SetupFile, setup: &SharingPreferences) {
        let record = group(setup);
        file.set_local(&setup.local_fingerprint);
        file.clock = file.clock.max(record.revision());
        file.store(record);
    }

    /// Stores `setup`'s group as a new local change, the enabled group left as it is.
    fn save_group(file: &mut SetupFile, setup: &SharingPreferences) {
        let record = group(setup)
            .restamped(file.clock + 1, &setup.local_fingerprint)
            .unwrap();
        file.set_local(&setup.local_fingerprint);
        file.clock = record.revision();
        file.store(record);
    }

    /// The pair's own record with that computer.
    fn pair_of<'a>(file: &'a SetupFile, peer: &str) -> Option<&'a GroupRecord> {
        file.groups()
            .iter()
            .find(|group| group.members().len() == 2 && group.has_member(peer))
    }

    fn saved_group(path: &Path) -> Option<GroupRecord> {
        pair_of(&SetupFile::load(path).unwrap(), &"B".repeat(64)).cloned()
    }

    #[test]
    fn fractional_geometry_round_trip_preserves_exact_inspection_binding() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut saved = preferences();
        saved.peer_displays[0].size[1] = 1600.0 / 1.75;
        saved.peer_displays[0].scale = 1.75;
        let inspected = inspection(&saved);
        let expected = GroupRecord::for_link(&inspected, saved.layout.clone(), 1).unwrap();
        assert_eq!(expected.members(), group(&saved).members());
        let mut file = SetupFile::default();
        file.store_agreed(&saved.local_fingerprint, expected.clone())
            .unwrap();
        file.save(&path).unwrap();
        let restored = saved_group(&path).unwrap();
        assert_eq!(restored, expected);
        assert!(restored.fits_link(&inspected));
        assert_eq!(restored.layout(), &saved.layout);
    }

    #[test]
    fn the_file_keeps_one_record_per_computer_and_the_active_choice() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut file = file_with(preferences());
        keep_group(&mut file, &preferences_for_peer('C'));
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.active(), Some("b".repeat(64).as_str()));
        assert!(loaded.sharing_chosen());
        assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
        assert_eq!(loaded.groups().len(), 2);
        assert_eq!(
            loaded.group_with(&"c".repeat(64)),
            Some(&group(&preferences_for_peer('C')))
        );
        assert_eq!(loaded.active_group(), Some(&group(&preferences())));
        let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        assert!(text.contains("\"version\":4"));
        assert!(!text.contains("sharingEnabled"));
        assert!(text.contains(&format!("\"local\":\"{}\"", "A".repeat(64))));
        assert!(text.contains(&format!("\"enabled\":[\"{}\"]", "b".repeat(64))));
        assert!(text.contains("\"paused\":false"));
        assert!(!text.contains("\"computers\""));
        assert!(!text.contains("\"active\""));
        // Records carry no network: the file names the one all sharing uses, once.
        assert_eq!(text.matches("interfaceId").count(), 1);

        let mut file = loaded;
        file.forget(&"B".repeat(64));
        assert_eq!(file.active(), None);
        assert!(!file.sharing_chosen());
        assert_eq!(file.groups().len(), 1);
        file.forget(&"c".repeat(64));
        assert!(file.groups().is_empty());
        file.save(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), file);
    }

    #[test]
    fn an_active_computer_may_have_no_record_yet() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut file = SetupFile::default();
        file.set_active(Some(
            &CertificateFingerprint::parse_full(&"D".repeat(64)).unwrap(),
        ));
        file.set_interface_id("en0:4:192.168.1.4");
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded.active(), Some("d".repeat(64).as_str()));
        assert!(loaded.active_group().is_none());
        assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
    }

    /// A record as v0.1.4 wrote it: a source display, a platform, an enabled switch, no control.
    pub(crate) fn v014_record() -> serde_json::Value {
        let mut record = serde_json::to_value(preferences()).unwrap();
        record["version"] = serde_json::json!(1);
        record["sourcePlatform"] = serde_json::Value::Null;
        record["sharingEnabled"] = serde_json::json!(true);
        let layout = record["layout"].as_object_mut().unwrap();
        layout.remove("control");
        layout.insert("sourceDisplay".into(), serde_json::json!("2"));
        record
    }

    #[test]
    fn v014_setup_file_loads_as_empty() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let old_path = directory.path(V3_SETUP_FILE);
        let v014 = serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "interfaceId": "en0:4:192.168.1.4",
            "active": "b".repeat(64),
            "computers": { "b".repeat(64): v014_record() },
        }))
        .unwrap();
        write_raw(&old_path, &v014);
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded, SetupFile::default());
        assert!(loaded.active().is_none());
        // Loading never rewrites the old file, and saving writes this version's beside it.
        assert_eq!(fs::read(&old_path).unwrap(), v014);
        file_with(preferences()).save(&path).unwrap();
        assert_eq!(saved_group(&path), Some(group(&preferences())));
        assert_eq!(fs::read(&old_path).unwrap(), v014);
    }

    #[test]
    fn restored_layout_requires_same_identity_network_and_displays() {
        let saved = preferences();
        let current = inspection(&saved);
        assert!(group(&saved).fits_link(&current));
        let changes: [fn(&mut InspectedPeer); 6] = [
            |peer| {
                peer.local_fingerprint =
                    CertificateFingerprint::parse_full(&"C".repeat(64)).unwrap()
            },
            |peer| {
                peer.peer_fingerprint = CertificateFingerprint::parse_full(&"D".repeat(64)).unwrap()
            },
            |peer| peer.interface_id.push('x'),
            |peer| {
                let mut changed = preferences();
                changed.local_displays[0].size[0] += 1.0;
                peer.local_displays = inspection(&changed).local_displays;
            },
            |peer| {
                let mut changed = preferences();
                changed.peer_displays[0].origin[1] += 1.0;
                peer.peer_displays = inspection(&changed).peer_displays;
            },
            |peer| {
                let mut changed = preferences();
                changed.peer_displays[0].scale = 2.0;
                peer.peer_displays = inspection(&changed).peer_displays;
            },
        ];
        let card = |inspection: Option<&InspectedPeer>, revision: &str| {
            SavedSetupView::from_group(
                Some(&group(&saved)),
                Some(&saved.local_fingerprint),
                &saved.peer_fingerprint,
                inspection,
                revision,
            )
        };
        assert_eq!(card(Some(&current), "3").layout, Some(saved.layout.clone()));
        for (index, change) in changes.iter().enumerate() {
            let mut changed = current.clone();
            change(&mut changed);
            assert_eq!(group(&saved).fits_link(&changed), index == 2, "{index}");
            let view = card(Some(&changed), "4");
            assert!(view.saved);
            // A group record carries no network, so on another one the saved layout still fits.
            assert_eq!(view.layout.is_some(), index == 2, "{index}");
        }
        let without_inspection = card(None, "5");
        assert!(without_inspection.layout.is_none());
        assert_eq!(
            without_inspection.preview_layout,
            Some(saved.layout.clone())
        );
    }

    #[test]
    fn display_names_do_not_invalidate_a_saved_layout() {
        let saved = preferences();
        let mut renamed = saved.clone();
        renamed.local_displays[0].name = "Built-in Retina Display".into();
        renamed.peer_displays[0].name = "U2723QE".into();
        let current = inspection(&renamed);
        assert!(group(&saved).fits_link(&current));
        assert!(inspection(&saved).matches(&current));
    }

    #[test]
    fn saving_requires_a_crossing_in_both_directions() {
        let saved = preferences();
        let current = inspection(&saved);
        assert!(GroupRecord::for_link(&current, saved.layout.clone(), 1).is_ok());
        let mut no_crossing = saved.layout.clone();
        no_crossing.links.clear();
        assert!(GroupRecord::for_link(&current, no_crossing, 1).is_err());
        for missing in 0..2 {
            let mut one_way = saved.layout.clone();
            one_way.links.remove(missing);
            assert!(GroupRecord::for_link(&current, one_way, 1).is_err());
        }
    }

    #[test]
    fn control_map_requires_both_fingerprints_and_one_true() {
        let saved = preferences();
        let current = inspection(&saved);
        let (local, peer) = ("a".repeat(64), "b".repeat(64));
        let map = |entries: &[(&str, bool)]| -> ControlMap {
            entries
                .iter()
                .map(|(fingerprint, allowed)| ((*fingerprint).to_owned(), *allowed))
                .collect()
        };
        for accepted in [
            map(&[(&local, true), (&peer, true)]),
            map(&[(&local, true), (&peer, false)]),
            map(&[(&local, false), (&peer, true)]),
        ] {
            let mut layout = saved.layout.clone();
            layout.control = accepted;
            assert!(GroupRecord::for_link(&current, layout, 1).is_ok());
        }
        let other = "c".repeat(64);
        let upper = "B".repeat(64);
        for refused in [
            map(&[]),
            map(&[(&local, true)]),
            map(&[(&local, false), (&peer, false)]),
            map(&[(&local, true), (&other, true)]),
            map(&[(&local, true), (&upper, true)]),
            map(&[(&local, true), (&peer, true), (&other, true)]),
        ] {
            let mut layout = saved.layout.clone();
            layout.control = refused.clone();
            assert!(
                GroupRecord::for_link(&current, layout, 1).is_err(),
                "{refused:?}"
            );
            assert!(validate_control(&refused, &"A".repeat(64), &"B".repeat(64)).is_err());
            // No record carries it, so no session ever negotiates it.
            assert!(group(&saved).with_control(refused.clone()).is_err());
        }
    }

    /// The same pair seen from the other computer.
    fn mirrored(record: &SharingPreferences) -> SharingPreferences {
        let mut mirrored = record.clone();
        std::mem::swap(
            &mut mirrored.local_fingerprint,
            &mut mirrored.peer_fingerprint,
        );
        std::mem::swap(&mut mirrored.local_displays, &mut mirrored.peer_displays);
        mirrored
    }

    #[test]
    fn wire_control_is_the_same_from_both_sides() {
        let local = "A".repeat(64);
        let peer = "B".repeat(64);
        let lower_is_local =
            device_id_from_fingerprint(CertificateFingerprint::parse_full(&local).unwrap())
                < device_id_from_fingerprint(CertificateFingerprint::parse_full(&peer).unwrap());
        assert!(lower_is_local);
        for (local_allowed, peer_allowed) in [(true, true), (true, false), (false, true)] {
            let control: ControlMap = [
                (fingerprint_key(&local), local_allowed),
                (fingerprint_key(&peer), peer_allowed),
            ]
            .into_iter()
            .collect();
            let here = wire_control(&control, &local, &peer).unwrap();
            let there = wire_control(&control, &peer, &local).unwrap();
            assert_eq!(here, there);
            assert_eq!(here.lower_controls_higher, local_allowed);
            assert_eq!(here.higher_controls_lower, peer_allowed);
            let mut record = preferences();
            record.layout.control = control;
            let mine = group(&record);
            let theirs =
                GroupRecord::from_pairwise(&mirrored(&record), Platform::Windows, Platform::MacOs)
                    .unwrap();
            assert_eq!(
                wire_control(&mine.layout().control, &local, &peer),
                Some(here)
            );
            assert_eq!(
                wire_control(&theirs.layout().control, &peer, &local),
                Some(here)
            );
        }
        // With the higher computer local, its own entry maps to the other field.
        let control = both_directions(&"C".repeat(64), &peer);
        let mut only_c = control.clone();
        only_c.insert("b".repeat(64), false);
        let from_c = wire_control(&only_c, &"C".repeat(64), &peer).unwrap();
        assert!(!from_c.lower_controls_higher);
        assert!(from_c.higher_controls_lower);
        assert_eq!(
            from_c,
            wire_control(&only_c, &peer, &"C".repeat(64)).unwrap()
        );
    }

    #[test]
    fn decider_is_identical_from_both_sides() {
        for peer in ['B', '0', 'F'] {
            let mut record = preferences();
            record.set_peer_fingerprint_for_test(peer);
            let here = inspection(&record);
            let there = inspection(&mirrored(&record));
            assert_ne!(local_decides(&here), local_decides(&there), "{peer}");
            assert_eq!(record.local_decides(), local_decides(&here));
            assert_eq!(mirrored(&record).local_decides(), local_decides(&there));
            assert_eq!(
                group(&record).decided_by(&record.local_fingerprint),
                local_decides(&here)
            );
            assert_eq!(
                group(&record).decided_by(&record.peer_fingerprint),
                local_decides(&there)
            );
        }
    }

    #[test]
    fn invalid_saved_file_is_not_automatically_repaired() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let corrupt = b"{unrecognized user setup}";
        write_raw(&path, corrupt);
        assert!(SetupFile::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }

    fn write_raw(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("fixture file must be written");
    }

    #[test]
    fn round_trip_keeps_inert_setup_data() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let expected = preferences();
        file_with(expected.clone())
            .save(&path)
            .expect("preferences must save");
        assert_eq!(saved_group(&path), Some(group(&expected)));
    }

    #[test]
    fn inserting_never_touches_the_network_choice() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        // A record made on another network replaces the pair's whole layout, not the network.
        let mut replacement = preferences();
        replacement.interface_id = "en1:7:192.168.1.5".into();
        replacement.layout.control.insert("b".repeat(64), false);
        let mut file = file_with(preferences());
        file.store_agreed(&"A".repeat(64), group(&replacement))
            .unwrap();
        file.save(&path).unwrap();
        let loaded = SetupFile::load(&path).unwrap();
        assert_eq!(loaded.groups().len(), 1);
        assert_eq!(loaded.interface_id(), Some("en0:4:192.168.1.4"));
        assert_eq!(loaded.active_group(), Some(&group(&replacement)));

        // Only an explicit choice moves it; the records name none to follow.
        let mut file = loaded;
        file.set_interface_id("en1:7:192.168.1.5");
        assert_eq!(file.active_group(), Some(&group(&replacement)));
        // A record a link agreed on leaves it alone as well.
        file.adopt(&"A".repeat(64), trio(9, true)).unwrap();
        assert_eq!(file.interface_id(), Some("en1:7:192.168.1.5"));

        // A file with no network yet stays without one until the user chooses it.
        let mut fresh = SetupFile::default();
        fresh
            .store_agreed(&"A".repeat(64), group(&replacement))
            .unwrap();
        assert_eq!(fresh.interface_id(), None);
    }

    /// This computer (A, a Mac) with two PCs, B and C, each crossing to A's one display.
    pub(crate) fn trio(revision: u64, c_controls: bool) -> GroupRecord {
        let member = |owner: char, platform: Platform, id: &str| {
            let display = DisplaySnapshot {
                id: id.into(),
                name: format!("Display {id}"),
                origin: [0.0, 0.0],
                size: [1920.0, 1080.0],
                native_size: [1920, 1080],
                scale: 1.0,
                primary: true,
                monitor: None,
            };
            GroupMember::new(
                &owner.to_string().repeat(64),
                platform,
                &topology_of(&[display]).unwrap(),
            )
            .unwrap()
        };
        let mut control = both_directions(&"A".repeat(64), &"B".repeat(64));
        control.insert("c".repeat(64), c_controls);
        GroupRecord::new(
            revision,
            &"A".repeat(64),
            vec![
                member('A', Platform::MacOs, "1"),
                member('B', Platform::Windows, "2"),
                member('C', Platform::Windows, "3"),
            ],
            LayoutRequest {
                links: vec![
                    link("1", "right", "2", "left"),
                    link("2", "left", "1", "right"),
                    link("1", "left", "3", "right"),
                    link("3", "right", "1", "left"),
                ],
                arrangement: None,
                control,
            },
        )
        .expect("the fixture is a valid record")
    }

    #[test]
    fn adopting_sets_enabled_to_the_members_and_never_goes_back() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let local = "A".repeat(64);
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        let mut file = file_with(preferences());
        let pair_before = pair_of(&file, &b).cloned();
        assert!(pair_before.is_some());

        // A group of three: its other members become the enabled group.
        assert_eq!(file.adopt(&local, trio(5, true)), Ok(true));
        assert_eq!(file.enabled(), [b.clone(), c.clone()]);
        assert_eq!(file.active_group(), Some(&trio(5, true)));
        assert_eq!(file.clock(), 5);
        // No single computer is active, and the pair keeps its own record.
        assert_eq!(file.active(), None);
        assert!(file.sharing_chosen());
        assert_eq!(pair_of(&file, &b).cloned(), pair_before);
        // Its card draws the group it shares now.
        assert_eq!(file.group_with(&b), Some(&trio(5, true)));

        // An older copy of the group changes nothing held.
        assert_eq!(file.adopt(&local, trio(4, false)), Ok(false));
        assert_eq!(file.active_group(), Some(&trio(5, true)));
        assert_eq!(file.clock(), 5);
        // The same stamp is taken, for its labels.
        let mut known = KnownDisplays::default();
        let mut renamed = trio(5, true).members()[0].displays().to_vec();
        renamed[0].name = "Studio Display".into();
        known.insert(&local, Platform::MacOs, &topology_of(&renamed).unwrap());
        let relabeled = trio(5, true).relabeled(&known);
        assert_ne!(relabeled, trio(5, true));
        assert_eq!(relabeled.stamp(), trio(5, true).stamp());
        assert_eq!(file.adopt(&local, relabeled.clone()), Ok(true));
        assert_eq!(file.active_group(), Some(&relabeled));

        // Back to the pair: its newer record is taken and only B stays enabled, while the
        // group of three keeps its record for when C is switched on again.
        let pair = GroupRecord::from_pairwise(&preferences(), Platform::MacOs, Platform::Windows)
            .unwrap()
            .restamped(6, &local)
            .unwrap();
        assert_eq!(file.adopt(&local, pair.clone()), Ok(true));
        assert_eq!(file.enabled(), std::slice::from_ref(&b));
        assert_eq!(file.active(), Some(b.as_str()));
        assert_eq!(file.active_group(), Some(&pair));
        assert_eq!(file.groups().len(), 2);
        assert_eq!(file.clock(), 6);
        // An older pair record never replaces the held one, though its members are enabled.
        let older = pair.restamped(2, &"B".repeat(64)).unwrap();
        assert_eq!(file.adopt(&local, older), Ok(false));
        assert_eq!(file.active_group(), Some(&pair));

        // A pause is the user's and stays.
        file.set_active(None);
        assert_eq!(file.adopt(&local, trio(7, true)), Ok(true));
        assert!(file.paused());
        assert_eq!(file.enabled(), [b.clone(), c.clone()]);
        // A record that does not name this computer is refused and changes nothing.
        let before = file.clone();
        assert_eq!(
            file.adopt(&"D".repeat(64), trio(8, true)),
            Err(PreferenceError::Invalid)
        );
        assert_eq!(file, before);

        file.save(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), file);
    }

    /// `preferences()` made with the computer `peer` names instead.
    fn record_for(peer: &str) -> SharingPreferences {
        let mut record = preferences();
        record.peer_fingerprint = peer.to_owned();
        record.layout.control = both_directions(&record.local_fingerprint, peer);
        record
    }

    #[test]
    fn groups_are_bounded_and_the_oldest_inactive_is_evicted() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        // The active pair is the oldest record of all and still stays.
        let mut file = file_with(preferences());
        let peers: Vec<String> = (0..MAX_GROUPS)
            .map(|index| format!("{index:064X}"))
            .collect();
        for peer in &peers {
            save_group(&mut file, &record_for(peer));
        }
        let content = |record: Option<&GroupRecord>| record.map(GroupRecord::content_digest);
        assert_eq!(file.groups().len(), MAX_GROUPS);
        assert_eq!(file.active_group(), Some(&group(&preferences())));
        assert_eq!(pair_of(&file, &peers[0]), None);
        for peer in &peers[1..] {
            assert_eq!(
                content(pair_of(&file, peer)),
                Some(group(&record_for(peer)).content_digest())
            );
        }
        file.save(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), file);

        // Saving a record again makes it the newest, so the next one to go is the oldest left.
        save_group(&mut file, &record_for(&peers[1]));
        save_group(&mut file, &record_for(&peers[0]));
        assert_eq!(file.groups().len(), MAX_GROUPS);
        assert_eq!(pair_of(&file, &peers[2]), None);
        assert!(pair_of(&file, &peers[1]).is_some());
        assert!(pair_of(&file, &peers[0]).is_some());

        // A file past the bound was not written by this version and is an error.
        let mut groups = file.groups().to_vec();
        groups.push(
            GroupRecord::from_pairwise(&record_for(&peers[2]), Platform::MacOs, Platform::Windows)
                .unwrap(),
        );
        groups.sort_by_cached_key(GroupRecord::member_keys);
        let mut past = serde_json::to_value(&file).unwrap();
        past["groups"] = serde_json::to_value(groups).unwrap();
        write_raw(&path, &serde_json::to_vec(&past).unwrap());
        assert!(SetupFile::load(&path).is_err());
    }

    #[test]
    fn pausing_keeps_the_group_and_choosing_again_resumes() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        for record in [
            preferences(),
            mirrored(&preferences()),
            two_display_record(),
            placed_record((1920.0, 0.0)),
            preferences_for_peer('0'),
        ] {
            let key = fingerprint_key(&record.peer_fingerprint);
            let peer = CertificateFingerprint::parse_full(&record.peer_fingerprint).unwrap();
            let mut file = SetupFile::default();
            file.set_interface_id("en7:12:10.1.1.7");
            file.set_active(Some(&peer));
            file.store_agreed(&record.local_fingerprint, group(&record))
                .unwrap();
            file.save(&path).unwrap();
            let loaded = SetupFile::load(&path).unwrap();
            assert_eq!(loaded.active(), Some(key.as_str()));
            assert_eq!(loaded.active_group(), Some(&group(&record)));
            // The pair reads back exactly as it was made, on the file's network.
            let expected = SharingPreferences {
                interface_id: "en7:12:10.1.1.7".into(),
                ..record.clone()
            };
            assert_eq!(
                loaded
                    .active_group()
                    .unwrap()
                    .to_pairwise(&record.local_fingerprint, "en7:12:10.1.1.7"),
                Ok(expected)
            );

            // Pause keeps the computer and its record; choosing it again resumes.
            let mut paused = loaded;
            paused.set_active(None);
            assert!(paused.paused());
            assert!(!paused.sharing_chosen());
            assert_eq!(paused.active(), None);
            assert_eq!(paused.enabled(), std::slice::from_ref(&key));
            assert_eq!(paused.active_group(), Some(&group(&record)));
            paused.set_active(Some(&peer));
            assert!(!paused.paused());
            assert!(paused.sharing_chosen());
            assert_eq!(paused.active(), Some(key.as_str()));
        }
    }

    #[test]
    fn choosing_another_computer_touches_its_record_once() {
        let local = "A".repeat(64);
        let parsed = |owner: &str| CertificateFingerprint::parse_full(&owner.repeat(64)).unwrap();
        let with_c = record_for(&"C".repeat(64));
        let mut file = file_with(preferences());
        keep_group(&mut file, &with_c);
        assert_eq!(file.clock(), 2);
        // Switching to C restamps C's record as this computer's newest change, content kept.
        file.choose(&parsed("C"));
        let touched = file.active_group().expect("C's pair");
        assert_eq!((touched.revision(), touched.author()), (3, local.as_str()));
        assert_eq!(touched.content_digest(), group(&with_c).content_digest());
        assert_eq!(file.clock(), 3);
        // Choosing it again changes nothing; resuming after a pause is a choice again.
        let before = file.clone();
        file.choose(&parsed("C"));
        assert_eq!(file, before);
        file.set_active(None);
        file.choose(&parsed("C"));
        assert_eq!(file.active_group().unwrap().revision(), 4);
        // A computer without a record here has nothing to touch.
        file.choose(&parsed("D"));
        assert!(!file.touch_active());
        assert_eq!(file.clock(), 4);
        // A revision seen elsewhere is kept, so the next touch is stamped past it.
        file.observe_clock(9);
        file.observe_clock(5);
        file.choose(&parsed("B"));
        assert_eq!(file.active_group().unwrap().revision(), 10);
        assert_eq!(file.clock(), 10);
    }

    #[test]
    fn forgetting_a_computer_keeps_every_group_without_it() {
        let local = "A".repeat(64);
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        let pair_with_c = || group(&record_for(&c.to_ascii_uppercase()));
        // Sharing with B and C together; C also has a pair of its own.
        let mut file = file_with(preferences());
        keep_group(&mut file, &record_for(&c.to_ascii_uppercase()));
        file.adopt(&local, trio(5, true)).unwrap();
        assert_eq!(file.groups().len(), 3);
        file.forget(&"B".repeat(64));
        // The pair with B is gone; the group of three gives way to C's own pair, which is now
        // the one shared and is touched as this computer's change.
        assert_eq!(file.enabled(), std::slice::from_ref(&c));
        assert_eq!(file.groups().len(), 1);
        let kept = file.active_group().expect("C's pair is the active group");
        assert_eq!(kept.content_digest(), pair_with_c().content_digest());
        assert_eq!((kept.revision(), kept.author()), (6, local.as_str()));
        assert_eq!(file.clock(), 6);

        // Without a pair of its own, C's place in the group of three is derived from it.
        let mut file = file_with(preferences());
        file.adopt(&local, trio(5, false)).unwrap();
        file.forget(&b);
        assert_eq!(file.enabled(), std::slice::from_ref(&c));
        let derived = file.active_group().expect("the group without B");
        assert_eq!(
            derived.member_keys(),
            [local.to_ascii_lowercase(), c.clone()]
        );
        assert_eq!(derived.layout().links.len(), 2);
        assert_eq!(
            derived.layout().control,
            [(local.to_ascii_lowercase(), true), (c.clone(), false)]
                .into_iter()
                .collect::<ControlMap>()
        );
        assert_eq!((derived.revision(), derived.author()), (6, local.as_str()));

        // Forgetting a computer that is not shared with touches nothing else.
        let before = file.clone();
        file.forget(&"D".repeat(64));
        assert_eq!(file, before);
        // A pair cannot lose a member and stay a group, so it goes, and nothing is shared.
        file.forget(&c);
        assert!(file.groups().is_empty());
        assert!(file.enabled().is_empty());
        assert!(!file.sharing_chosen());
        assert!(file.validate().is_ok());
    }

    pub(crate) const LOCAL: &str =
        "7C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F8";
    pub(crate) const WINDOWS_PC: &str =
        "2B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A9357";
    const SECOND_PC: &str = "E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856";

    /// A Mac's setup file exactly as version 3 wrote it: a PC with two displays, arranged and
    /// active, and a second PC saved on another network whose input may not control the Mac.
    pub(crate) const V3_FILE: &str = r#"{
  "version": 3,
  "interfaceId": "en0:4:192.168.1.4",
  "active": "2b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a9357",
  "computers": {
    "2b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a9357": {
      "version": 2,
      "interfaceId": "en0:4:192.168.1.4",
      "localFingerprint": "7C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F8",
      "peerFingerprint": "2B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A9357",
      "localDisplays": [
        {"id": "5764607523034234881", "name": "Built-in Retina Display", "origin": [0.0, 0.0],
         "size": [1512.0, 982.0], "nativeSize": [3024, 1964], "scale": 2.0, "primary": true,
         "monitor": "0610-a050-00000000"}
      ],
      "peerDisplays": [
        {"id": "17293822569102704641", "name": "DELL U2723QE", "origin": [0.0, 0.0],
         "size": [2560.0, 1440.0], "nativeSize": [2560, 1440], "scale": 1.0, "primary": true,
         "monitor": "10ac-4173-00000001"},
        {"id": "12297829382473034410", "name": "Generic PnP Monitor", "origin": [2560.0, 0.0],
         "size": [1920.0, 1080.0], "nativeSize": [1920, 1080], "scale": 1.0, "primary": false}
      ],
      "layout": {
        "links": [
          {"fromDisplay": "5764607523034234881", "fromEdge": "right", "fromSpan": [0.0, 1.0],
           "toDisplay": "17293822569102704641", "toEdge": "left", "toSpan": [0.25, 0.75],
           "hysteresis": 1.0},
          {"fromDisplay": "17293822569102704641", "fromEdge": "left", "fromSpan": [0.25, 0.75],
           "toDisplay": "5764607523034234881", "toEdge": "right", "toSpan": [0.0, 1.0],
           "hysteresis": 1.0}
        ],
        "arrangement": {
          "positions": [
            {"display": "5764607523034234881", "x": -1512.0, "y": 144.0},
            {"display": "17293822569102704641", "x": 0.0, "y": 0.0},
            {"display": "12297829382473034410", "x": 2560.0, "y": 0.0}
          ],
          "hidden": []
        },
        "control": {
          "2b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a9357": true,
          "7c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f8": true
        }
      }
    },
    "e04a7b3c9d2f1856e04a7b3c9d2f1856e04a7b3c9d2f1856e04a7b3c9d2f1856": {
      "version": 2,
      "interfaceId": "en1:9:10.0.0.2",
      "localFingerprint": "7C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F8",
      "peerFingerprint": "E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856",
      "localDisplays": [
        {"id": "5764607523034234881", "name": "Built-in Retina Display", "origin": [0.0, 0.0],
         "size": [1512.0, 982.0], "nativeSize": [3024, 1964], "scale": 2.0, "primary": true,
         "monitor": "0610-a050-00000000"}
      ],
      "peerDisplays": [
        {"id": "9223372036854775817", "name": "LG HDR 4K", "origin": [0.0, 0.0],
         "size": [2560.0, 1440.0], "nativeSize": [3840, 2160], "scale": 1.5, "primary": true,
         "monitor": "1e6d-7750-0001e240"}
      ],
      "layout": {
        "links": [
          {"fromDisplay": "5764607523034234881", "fromEdge": "left", "fromSpan": [0.0, 1.0],
           "toDisplay": "9223372036854775817", "toEdge": "right", "toSpan": [0.25, 0.75],
           "hysteresis": 1.0},
          {"fromDisplay": "9223372036854775817", "fromEdge": "right", "fromSpan": [0.25, 0.75],
           "toDisplay": "5764607523034234881", "toEdge": "left", "toSpan": [0.0, 1.0],
           "hysteresis": 1.0}
        ],
        "control": {
          "7c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f8": true,
          "e04a7b3c9d2f1856e04a7b3c9d2f1856e04a7b3c9d2f1856e04a7b3c9d2f1856": false
        }
      }
    }
  }
}"#;

    /// The first PC's own version 3 file: the same pair from its side, on its own network.
    pub(crate) fn mirrored_v3_file() -> String {
        let v3: serde_json::Value = serde_json::from_str(V3_FILE).unwrap();
        let mut record = v3["computers"][fingerprint_key(WINDOWS_PC)].clone();
        let fields = record.as_object_mut().unwrap();
        for (here, there) in [
            ("localFingerprint", "peerFingerprint"),
            ("localDisplays", "peerDisplays"),
        ] {
            let (mine, theirs) = (fields.remove(here).unwrap(), fields.remove(there).unwrap());
            fields.insert(here.into(), theirs);
            fields.insert(there.into(), mine);
        }
        fields.insert("interfaceId".into(), "windows-physical-interface".into());
        serde_json::json!({
            "version": 3,
            "interfaceId": "windows-physical-interface",
            "active": fingerprint_key(LOCAL),
            "computers": { fingerprint_key(LOCAL): record },
        })
        .to_string()
    }

    /// `text`, where version 3 kept its setup, as `platform` migrates it with no computer list
    /// beside it and no identity to read.
    pub(crate) fn load_as(text: &str, platform: Platform) -> SetupFile {
        let directory = TestDirectory::new();
        write_raw(&directory.path(V3_SETUP_FILE), text.as_bytes());
        SetupFile::load_as(&directory.path(SETUP_FILE), platform, BTreeMap::new, || {
            None
        })
        .unwrap()
    }

    fn group_of<'a>(file: &'a SetupFile, peer: &str) -> &'a GroupRecord {
        file.groups()
            .iter()
            .find(|group| group.has_member(peer))
            .expect("the pair has a record")
    }

    #[test]
    fn a_v3_file_migrates_every_pairwise_layout() {
        let v3: serde_json::Value = serde_json::from_str(V3_FILE).unwrap();
        let file = load_as(V3_FILE, Platform::MacOs);
        assert_eq!(file.local(), Some(LOCAL));
        assert_eq!(file.enabled(), [fingerprint_key(WINDOWS_PC)]);
        assert!(!file.paused());
        assert_eq!(file.clock(), MIGRATED_CLOCK);
        assert_eq!(file.interface_id(), Some("en0:4:192.168.1.4"));
        assert_eq!(file.groups().len(), 2);
        // The PC with the lower DeviceId decided for the first pair, this Mac for the second.
        for (peer, revision, author) in [(WINDOWS_PC, 1, WINDOWS_PC), (SECOND_PC, 2, LOCAL)] {
            let saved: SharingPreferences =
                serde_json::from_value(v3["computers"][fingerprint_key(peer)].clone()).unwrap();
            assert_eq!(saved.local_decides(), revision == 2);
            let group = group_of(&file, peer);
            // Displays, ids, monitors, arrangement and control verbatim; no network.
            let expected = SharingPreferences {
                interface_id: "en0:4:192.168.1.4".into(),
                ..saved.clone()
            };
            assert_eq!(group.to_pairwise(LOCAL, "en0:4:192.168.1.4"), Ok(expected));
            assert_eq!((group.revision(), group.author()), (revision, author));
            assert!(group.members().iter().all(|member| {
                member.platform()
                    == if member.fingerprint() == LOCAL {
                        ComputerPlatform::Macos
                    } else {
                        ComputerPlatform::Windows
                    }
            }));
            assert!(group.topology_for(LOCAL).is_some());
            assert_eq!(
                wire_control(&group.layout().control, LOCAL, peer),
                wire_control(&saved.layout.control, LOCAL, peer)
            );
            assert!(wire_control(&group.layout().control, LOCAL, peer).is_some());
        }
        assert_eq!(file.active(), Some(fingerprint_key(WINDOWS_PC).as_str()));
        assert_eq!(file.active_group(), Some(group_of(&file, WINDOWS_PC)));
    }

    #[test]
    fn the_other_computers_v3_file_migrates_to_the_same_content() {
        let mac = load_as(V3_FILE, Platform::MacOs);
        let pc = load_as(&mirrored_v3_file(), Platform::Windows);
        assert_eq!(pc.local(), Some(WINDOWS_PC));
        assert_eq!(pc.active(), Some(fingerprint_key(LOCAL).as_str()));
        assert_eq!(pc.interface_id(), Some("windows-physical-interface"));
        let (here, there) = (group_of(&mac, WINDOWS_PC), group_of(&pc, LOCAL));
        assert_eq!(here.content_digest(), there.content_digest());
        assert_eq!(here.members(), there.members());
        // Both name the old decider; its own copy is newer, so copies that drifted converge to it.
        assert_eq!((here.author(), there.author()), (WINDOWS_PC, WINDOWS_PC));
        assert_eq!((here.revision(), there.revision()), (1, 2));
        assert_eq!(here.clone().merge(there.clone()), *there);
        // Each computer still reads the pair from its own side.
        let (mine, theirs) = (
            here.to_pairwise(LOCAL, "en0:4:192.168.1.4").unwrap(),
            there
                .to_pairwise(WINDOWS_PC, "windows-physical-interface")
                .unwrap(),
        );
        assert_eq!(theirs.local_fingerprint, WINDOWS_PC);
        assert_eq!(theirs.local_displays, mine.peer_displays);
        assert_eq!(theirs.layout, mine.layout);
        assert_eq!(
            wire_control(&here.layout().control, LOCAL, WINDOWS_PC),
            wire_control(&there.layout().control, WINDOWS_PC, LOCAL)
        );
    }

    #[test]
    fn a_migrated_pair_names_each_computer_by_the_platform_the_list_records() {
        // Two Macs: the list beside each file records the other one as a Mac, so both migrate
        // the pair to the same content instead of each guessing the other is a PC.
        let directory = TestDirectory::new();
        let list_path = directory.path(LIST_FILE);
        let address: std::net::SocketAddrV4 = "192.168.1.9:24872".parse().unwrap();
        let listed = |peer: &str| {
            let mut list = ComputerList::default();
            list.remember(
                CertificateFingerprint::parse_full(peer).unwrap(),
                address,
                Some(Platform::MacOs),
            )
            .unwrap();
            list.save(&list_path).unwrap();
        };
        let load = |text: &str| {
            let path = directory.path(SETUP_FILE);
            write_raw(&directory.path(V3_SETUP_FILE), text.as_bytes());
            SetupFile::load_as(&path, Platform::MacOs, || listed_platforms(&path), || None).unwrap()
        };
        listed(WINDOWS_PC);
        let first = load(V3_FILE);
        listed(LOCAL);
        let second = load(&mirrored_v3_file());
        let (here, there) = (group_of(&first, WINDOWS_PC), group_of(&second, LOCAL));
        assert!(
            here.members()
                .iter()
                .all(|member| member.platform() == ComputerPlatform::Macos)
        );
        assert_eq!(here.members(), there.members());
        assert_eq!(here.content_digest(), there.content_digest());
        // A computer the list does not name is taken for the other platform, as before.
        let second_pc = group_of(&first, SECOND_PC);
        assert_eq!(
            second_pc.member(SECOND_PC).unwrap().platform(),
            ComputerPlatform::Windows
        );
        // Without a list, each guesses the other is a PC and the two disagree until they link.
        let guessed = load_as(&mirrored_v3_file(), Platform::MacOs);
        assert_ne!(
            group_of(&guessed, LOCAL).content_digest(),
            here.content_digest()
        );
        // The link between the two Macs finds each record as it is, so sharing starts at once;
        // the guess would have sent them to arrange again.
        let mut macs = link_of(here, LOCAL, WINDOWS_PC);
        (macs.local_platform, macs.peer_platform) = (Platform::MacOs, Platform::MacOs);
        assert!(here.fits_link(&macs));
        assert!(there.fits_link(&opposite(&macs)));
        assert!(!group_of(&guessed, LOCAL).fits_link(&opposite(&macs)));
    }

    /// A version 3 file holding `records`, with the computer `active` names chosen.
    fn v3_file(active: Option<char>, records: &[SharingPreferences]) -> Vec<u8> {
        let computers: serde_json::Map<String, serde_json::Value> = records
            .iter()
            .map(|record| {
                (
                    fingerprint_key(&record.peer_fingerprint),
                    serde_json::to_value(record).unwrap(),
                )
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "version": 3,
            "interfaceId": "en0:4:192.168.1.4",
            "active": active.map(|owner| owner.to_string().repeat(64).to_ascii_lowercase()),
            "computers": computers,
        }))
        .unwrap()
    }

    /// `local`'s record with `peer`, as that identity of this computer saved it.
    fn record_of(local: char, peer: char) -> SharingPreferences {
        let mut record = preferences_for_peer(peer);
        record.local_fingerprint = local.to_string().repeat(64);
        record.layout.control =
            both_directions(&record.local_fingerprint, &record.peer_fingerprint);
        record
    }

    #[test]
    fn a_v3_file_keeps_the_layouts_of_the_stored_identity() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        // Two records from an earlier identity D outvote the one this computer, A, saved since.
        write_raw(
            &directory.path(V3_SETUP_FILE),
            &v3_file(
                Some('B'),
                &[
                    record_of('D', 'E'),
                    record_of('D', 'F'),
                    record_of('A', 'B'),
                ],
            ),
        );
        let load = |identity: Option<String>| {
            SetupFile::load_as(&path, Platform::MacOs, BTreeMap::new, || identity).unwrap()
        };
        let file = load(Some("a".repeat(64)));
        assert_eq!(file.local(), Some("A".repeat(64).as_str()));
        assert_eq!(file.groups().len(), 1);
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert_eq!(
            file.active_group().map(GroupRecord::content_digest),
            Some(group(&record_of('A', 'B')).content_digest())
        );
        // An identity no record names leaves nothing of the earlier ones to share with.
        let file = load(Some("C".repeat(64)));
        assert_eq!(file.local(), Some("C".repeat(64).as_str()));
        assert!(file.groups().is_empty());
        // Only an identity that cannot be read falls back to the records' own majority.
        let file = load(None);
        assert_eq!(file.local(), Some("D".repeat(64).as_str()));
        assert_eq!(file.groups().len(), 2);
        assert!(file.active_group().is_none());
    }

    #[test]
    fn a_record_of_an_older_identity_is_dropped() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let load = |active, records: &[SharingPreferences]| {
            write_raw(&directory.path(V3_SETUP_FILE), &v3_file(active, records));
            SetupFile::load(&path).unwrap()
        };
        let keys = |file: &SetupFile| {
            let local = file.local().map(fingerprint_key);
            file.groups()
                .iter()
                .flat_map(GroupRecord::member_keys)
                .filter(|member| Some(member) != local.as_ref())
                .collect::<Vec<_>>()
        };
        let active = |file: &SetupFile| {
            file.active_group()
                .and_then(|group| group.to_pairwise(file.local()?, "en0:4:192.168.1.4").ok())
        };

        // Two records name this computer A; the one it saved under its earlier identity D goes.
        let file = load(
            Some('B'),
            &[
                record_of('A', 'B'),
                record_of('A', 'C'),
                record_of('D', 'E'),
            ],
        );
        assert_eq!(file.local(), Some("A".repeat(64).as_str()));
        assert_eq!(keys(&file), ["b".repeat(64), "c".repeat(64)]);
        assert_eq!(file.groups().len(), 2);
        assert_eq!(pair_of(&file, &"e".repeat(64)), None);
        assert_eq!(active(&file), Some(record_of('A', 'B')));

        // On a tie the active record names this computer.
        let file = load(Some('E'), &[record_of('A', 'B'), record_of('D', 'E')]);
        assert_eq!(file.local(), Some("D".repeat(64).as_str()));
        assert_eq!(keys(&file), ["e".repeat(64)]);
        assert_eq!(active(&file), Some(record_of('D', 'E')));

        // Without one, every load settles the tie the same way.
        let file = load(None, &[record_of('A', 'B'), record_of('D', 'E')]);
        assert_eq!(file.local(), Some("A".repeat(64).as_str()));
        assert_eq!(keys(&file), ["b".repeat(64)]);
        assert!(file.enabled().is_empty());
        assert_eq!(file.active(), None);
    }

    #[test]
    fn loading_never_rewrites_and_saving_writes_version_4() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let old_path = directory.path(V3_SETUP_FILE);
        write_raw(&old_path, V3_FILE.as_bytes());
        let migrated = SetupFile::load(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), migrated);
        assert_eq!(fs::read(&old_path).unwrap(), V3_FILE.as_bytes());
        assert!(!path.exists());
        assert_eq!(SetupFile::stored_at(&path), old_path);

        migrated.save(&path).unwrap();
        assert_eq!(SetupFile::stored_at(&path), path);
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], 4);
        assert_eq!(written["clock"], 2);
        let mut fields: Vec<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "clock",
                "enabled",
                "groups",
                "interfaceId",
                "local",
                "paused",
                "version"
            ]
        );
        assert!(
            written["groups"]
                .as_array()
                .unwrap()
                .iter()
                .all(|group| group.get("interfaceId").is_none())
        );
        assert_eq!(SetupFile::load(&path).unwrap(), migrated);
    }

    #[test]
    fn a_v4_save_never_touches_the_v3_file() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let old_path = directory.path(V3_SETUP_FILE);
        write_raw(&old_path, V3_FILE.as_bytes());
        let mut file = SetupFile::load(&path).unwrap();
        file.set_active(None);
        file.save(&path).unwrap();
        file.forget(WINDOWS_PC);
        file.save(&path).unwrap();
        // An older MonHop still finds its own setup exactly as it left it.
        assert_eq!(fs::read(&old_path).unwrap(), V3_FILE.as_bytes());
        let saved = SetupFile::load(&path).unwrap();
        assert_eq!(saved, file);
        assert!(saved.paused());
        assert_eq!(saved.groups().len(), 1);
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], 4);
    }

    #[test]
    fn the_v3_file_is_migrated_only_when_the_group_file_is_absent() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let old_path = directory.path(V3_SETUP_FILE);
        write_raw(&old_path, V3_FILE.as_bytes());
        // Once this version wrote its own file, the version 3 file is never read again.
        let mut chosen = SetupFile::default();
        chosen.set_interface_id("en7:12:10.1.1.7");
        chosen.save(&path).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap(), chosen);
        fs::remove_file(&path).unwrap();
        let migrated = SetupFile::load(&path).unwrap();
        assert_eq!(migrated.groups().len(), 2);
        assert_eq!(migrated.local(), Some(LOCAL));
        // A build between version 3 and this one wrote version 4 in the old place; it is read too.
        write_raw(&old_path, &serde_json::to_vec(&chosen).unwrap());
        assert_eq!(SetupFile::load(&path).unwrap(), chosen);
        // A version 3 file where this version's belongs is not one this version wrote.
        write_raw(&path, V3_FILE.as_bytes());
        assert!(SetupFile::load(&path).is_err());
    }

    #[test]
    fn a_newer_version_loads_read_only_and_refuses_to_save() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let mut newer = serde_json::to_value(file_with(preferences())).unwrap();
        newer["version"] = serde_json::json!(SETUP_FILE_VERSION + 1);
        newer["members"] = serde_json::json!({ "unknown": "to this version" });
        let newer = serde_json::to_vec(&newer).unwrap();
        write_raw(&path, &newer);
        let mut loaded = SetupFile::load(&path).unwrap();
        assert!(loaded.written_by_newer());
        assert!(loaded.groups().is_empty());
        assert!(!loaded.sharing_chosen());
        loaded.set_active(Some(
            &CertificateFingerprint::parse_full(&"B".repeat(64)).unwrap(),
        ));
        assert_eq!(
            loaded.save(&path).map_err(|error| error.kind()),
            Err(io::ErrorKind::Unsupported)
        );
        assert_eq!(fs::read(&path).unwrap(), newer);
    }

    #[test]
    fn a_newer_legacy_setup_loads_read_only() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        let old_path = directory.path(V3_SETUP_FILE);
        let mut newer = serde_json::to_value(file_with(preferences())).unwrap();
        newer["version"] = serde_json::json!(SETUP_FILE_VERSION + 1);
        newer["members"] = serde_json::json!({ "unknown": "to this version" });
        let newer = serde_json::to_vec(&newer).unwrap();
        write_raw(&old_path, &newer);
        let mut loaded = SetupFile::load(&path).unwrap();
        assert!(loaded.written_by_newer());
        assert!(loaded.groups().is_empty());
        assert!(!loaded.sharing_chosen());

        // The first save is refused, so the newer file is never hidden behind one written here.
        loaded.set_interface_id("en7:12:10.1.1.7");
        assert_eq!(
            loaded.save(&path).map_err(|error| error.kind()),
            Err(io::ErrorKind::Unsupported)
        );
        let names: Vec<_> = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [V3_SETUP_FILE], "no file was created");
        assert_eq!(fs::read(&old_path).unwrap(), newer);
        assert!(SetupFile::load(&path).unwrap().written_by_newer());
    }

    #[test]
    fn missing_file_is_inert() {
        let directory = TestDirectory::new();
        let loaded = SetupFile::load(&directory.path("missing.json")).unwrap();
        assert_eq!(loaded, SetupFile::default());
        assert!(loaded.active().is_none());
        assert!(!directory.path("missing.json").exists());
    }

    #[test]
    fn corrupt_oversize_and_unknown_records_are_rejected() {
        let directory = TestDirectory::new();
        let path = directory.path(SETUP_FILE);
        write_raw(&path, b"{");
        assert!(SetupFile::load(&path).is_err());

        write_raw(&path, &vec![b'x'; MAX_SETUP_FILE_BYTES as usize + 1]);
        assert!(SetupFile::load(&path).is_err());

        let rejected = |value: &serde_json::Value| {
            write_raw(&path, &serde_json::to_vec(value).unwrap());
            SetupFile::load(&path).is_err()
        };
        let valid = serde_json::to_value(file_with(preferences())).unwrap();
        assert!(!rejected(&valid));
        for (level, field) in [
            ("file", "active"),
            ("file", "computers"),
            ("record", "interfaceId"),
            ("record", "enabled"),
            ("record", "sourcePlatform"),
        ] {
            let mut unknown = valid.clone();
            let target = if level == "file" {
                unknown.as_object_mut().unwrap()
            } else {
                unknown["groups"][0].as_object_mut().unwrap()
            };
            target.insert(field.into(), serde_json::Value::Bool(false));
            assert!(rejected(&unknown), "{level} {field}");
        }

        // A file that breaks its own rules is an error, never reset.
        let breaks: [fn(&mut serde_json::Value); 10] = [
            |file| file["enabled"] = serde_json::json!(["B".repeat(64)]),
            |file| file["enabled"] = serde_json::json!(["c".repeat(64), "b".repeat(64)]),
            |file| file["enabled"] = serde_json::json!(["a".repeat(64)]),
            |file| file["local"] = serde_json::json!("a".repeat(64)),
            |file| file["local"] = serde_json::json!("C".repeat(64)),
            |file| file["clock"] = serde_json::json!(0),
            |file| file["interfaceId"] = serde_json::json!(""),
            |file| file["groups"][0]["layout"]["control"] = serde_json::json!({}),
            |file| {
                let group = file["groups"][0].clone();
                file["groups"].as_array_mut().unwrap().push(group);
            },
            |file| {
                file.as_object_mut().unwrap().remove("paused");
            },
        ];
        for (index, change) in breaks.iter().enumerate() {
            let mut broken = valid.clone();
            change(&mut broken);
            assert!(rejected(&broken), "{index}");
        }

        let mut other_version = valid.clone();
        other_version["version"] = serde_json::json!(SETUP_FILE_VERSION + 1);
        write_raw(&path, &serde_json::to_vec(&other_version).unwrap());
        assert!(SetupFile::load(&path).unwrap().written_by_newer());

        // A version 3 file is read as strictly as that version read it before it is migrated.
        fs::remove_file(&path).unwrap();
        let old_path = directory.path(V3_SETUP_FILE);
        let rejected = |value: &serde_json::Value| {
            write_raw(&old_path, &serde_json::to_vec(value).unwrap());
            SetupFile::load(&path).is_err()
        };
        let v3: serde_json::Value =
            serde_json::from_slice(&v3_file(Some('B'), &[preferences()])).unwrap();
        assert!(!rejected(&v3));
        let (b, c) = ("b".repeat(64), "c".repeat(64));

        let mut old_record = v3.clone();
        old_record["computers"][&b] = v014_record();
        assert!(rejected(&old_record));

        let mut wrong_key = v3.clone();
        let computers = wrong_key["computers"].as_object_mut().unwrap();
        let record = computers.remove(&b).unwrap();
        computers.insert(c, record);
        assert!(rejected(&wrong_key));

        let mut upper_active = v3.clone();
        upper_active["active"] = serde_json::json!("B".repeat(64));
        assert!(rejected(&upper_active));

        let mut unknown = v3.clone();
        unknown["enabled"] = serde_json::json!([b]);
        assert!(rejected(&unknown));

        let too_many: Vec<SharingPreferences> = (0..=MAX_COMPUTERS)
            .map(|index| record_for(&format!("{index:064X}")))
            .collect();
        write_raw(&old_path, &v3_file(None, &too_many));
        assert!(SetupFile::load(&path).is_err());
        // Nothing past a damaged version 3 file was written in its place or beside it.
        assert!(!path.exists());
    }

    #[test]
    fn invalid_geometry_and_nonfinite_values_do_not_replace_a_valid_file() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let valid = preferences();
        file_with(valid.clone()).save(&path).unwrap();

        let mut nonfinite = valid.clone();
        nonfinite.local_displays[0].origin[0] = f64::NAN;
        assert!(try_file_with(&nonfinite).is_err());

        let mut invalid_geometry = valid.clone();
        invalid_geometry.peer_displays[0].native_size[0] = 0;
        assert!(try_file_with(&invalid_geometry).is_err());
        // Even a record that reached memory unchecked is never written.
        let mut record = serde_json::to_value(group(&valid)).unwrap();
        record["members"][1]["displays"][0]["nativeSize"] = serde_json::json!([0, 1440]);
        let mut file = file_with(valid.clone());
        file.groups = vec![serde_json::from_value(record).unwrap()];
        assert!(file.save(&path).is_err());

        assert_eq!(saved_group(&path), Some(group(&valid)));
    }

    #[test]
    fn display_and_layout_bounds_are_rejected() {
        let directory = TestDirectory::new();
        let path = directory.path("sharing.json");
        let mut too_many_displays = preferences();
        for id in 3..=18 {
            too_many_displays.local_displays.push(DisplaySnapshot {
                id: id.to_string(),
                name: format!("Display {id}"),
                origin: [f64::from(id) * 100.0, 0.0],
                size: [100.0, 100.0],
                native_size: [100, 100],
                scale: 1.0,
                primary: false,
                monitor: None,
            });
        }
        assert!(try_file_with(&too_many_displays).is_err());

        let mut too_many_links = preferences();
        too_many_links.layout.links = vec![too_many_links.layout.links[0].clone(); MAX_LINKS + 1];
        assert!(try_file_with(&too_many_links).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn record_has_no_enabled_state() {
        let encoded = serde_json::to_string(&preferences()).unwrap();
        assert!(!encoded.contains("enabled"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_paths_are_rejected_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = directory.path("target.json");
        let link = directory.path("link.json");
        let expected = preferences();
        file_with(expected.clone()).save(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(SetupFile::load(&link).is_err());
        assert!(file_with(preferences()).save(&link).is_err());
        assert_eq!(
            SetupFile::load(&target).unwrap().active_group(),
            Some(&group(&expected))
        );
    }

    /// The same pair as the other computer inspects it.
    pub(crate) fn opposite(inspection: &InspectedPeer) -> InspectedPeer {
        InspectedPeer {
            local_device: inspection.peer_device,
            peer_device: inspection.local_device,
            local_fingerprint: inspection.peer_fingerprint,
            peer_fingerprint: inspection.local_fingerprint,
            local_platform: inspection.peer_platform,
            peer_platform: inspection.local_platform,
            local_displays: inspection.peer_displays.clone(),
            peer_displays: inspection.local_displays.clone(),
            interface_id: "windows-physical-interface".into(),
        }
    }

    #[test]
    fn shared_group_maps_both_perspectives_without_importing_the_network() {
        let saved = preferences();
        let sender = inspection(&saved);
        let record = group(&saved);
        let bytes = shared_group_bytes(&sender, &record, false).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("interfaceId"));
        assert!(!text.contains("enabled"));
        for current in [sender.clone(), opposite(&sender)] {
            let (restored, left_out) = shared_group_for_link(&current, &bytes).unwrap();
            assert!(!left_out);
            assert_eq!(restored, record);
            assert!(restored.fits_link(&current));
            // Each computer reads the pair from its own side, on the network it chose.
            let pairwise = restored
                .to_pairwise(&current.local_fingerprint.full_hex(), &current.interface_id)
                .unwrap();
            assert_eq!(pairwise.layout, saved.layout);
            assert_eq!(pairwise.interface_id, current.interface_id);
        }
    }

    #[test]
    fn shared_group_v3_round_trips_control() {
        let saved = preferences();
        let sender = inspection(&saved);
        let receiver = opposite(&sender);
        let mut layout = saved.layout.clone();
        layout.control.insert("b".repeat(64), false);
        let record = group(&saved).with_control(layout.control.clone()).unwrap();
        let bytes = shared_group_bytes(&sender, &record, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["version"], 3);
        assert_eq!(value["leftOut"], true);
        assert_eq!(value["senderFingerprint"], "A".repeat(64));
        assert_eq!(value["record"]["layout"]["control"]["a".repeat(64)], true);
        assert_eq!(value["record"]["layout"]["control"]["b".repeat(64)], false);
        let (here, _) = shared_group_for_link(&sender, &bytes).unwrap();
        let (there, left_out) = shared_group_for_link(&receiver, &bytes).unwrap();
        assert!(left_out);
        assert_eq!(here.layout().control, layout.control);
        assert_eq!(there.layout().control, layout.control);
        let (a, b) = ("A".repeat(64), "B".repeat(64));
        assert_eq!(
            wire_control(&here.layout().control, &a, &b),
            wire_control(&there.layout().control, &b, &a)
        );
    }

    #[test]
    fn shared_group_rejects_stale_or_untrusted_metadata_before_saving() {
        let saved = preferences();
        let sender = inspection(&saved);
        let bytes = shared_group_bytes(&sender, &group(&saved), false).unwrap();
        let receiver = opposite(&sender);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (field, bad) in [
            ("version", serde_json::json!(2)),
            ("senderFingerprint", serde_json::json!("00".repeat(32))),
            ("receiverFingerprint", serde_json::json!("00".repeat(32))),
            ("sourcePlatform", serde_json::json!("windows")),
            ("interfaceId", serde_json::json!("remote-interface")),
            ("enabled", serde_json::json!(true)),
        ] {
            let mut corrupted = value.clone();
            corrupted[field] = bad;
            assert!(
                shared_group_for_link(&receiver, &serde_json::to_vec(&corrupted).unwrap()).is_err(),
                "{field}"
            );
        }
        // The sender's entry, as the receiver sees it, no longer matches its displays.
        let mut stale = value.clone();
        stale["record"]["members"][0]["displays"][0]["origin"][0] = serde_json::json!(100.5);
        assert_eq!(
            shared_group_for_link(&receiver, &serde_json::to_vec(&stale).unwrap()),
            Err(PreferenceError::InspectionChanged)
        );
        for (field, bad) in [
            ("links", serde_json::json!([])),
            ("control", serde_json::json!({})),
            ("sourceDisplay", serde_json::json!("2")),
        ] {
            let mut invalid = value.clone();
            invalid["record"]["layout"][field] = bad;
            assert!(
                shared_group_for_link(&receiver, &serde_json::to_vec(&invalid).unwrap()).is_err(),
                "{field}"
            );
        }
        let mut trailing = bytes;
        trailing.extend_from_slice(b"{}");
        assert!(shared_group_for_link(&receiver, &trailing).is_err());
        assert!(
            shared_group_for_link(
                &receiver,
                &vec![b' '; MAX_SHARING_PREFERENCES_BYTES as usize + 1]
            )
            .is_err()
        );
    }
}
