//! Named display arrangements kept on this computer. Each one is a complete layout for one group
//! of computers; loading one is only offered while every member shows the displays it was made for.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
};

use monhop_core::Platform;
use serde::{Deserialize, Serialize};

use crate::group_record::{GroupMember, GroupRecord, KnownDisplays};
use crate::sharing::LayoutRequest;
use crate::sharing_preferences::{
    SharingPreferences, file_version, fingerprint_key, listed_platform, listed_platforms,
    newer_file, read_bounded, save_bounded,
};

pub const MAX_ARRANGEMENTS: usize = 32;
pub const MAX_NAME_CHARS: usize = 64;
const LIBRARY_VERSION: u8 = 3;
/// The library before groups: one pairwise setup per entry, read only to migrate it.
const LIBRARY_V2_VERSION: u8 = 2;
/// Where version 2 kept the library, beside this version's file. It is only ever read, so an
/// older MonHop still finds its own arrangements there.
const V2_LIBRARY_FILE: &str = "arrangements.json";
const MAX_LIBRARY_BYTES: u64 = 512 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrangementLibrary {
    version: u8,
    arrangements: Vec<SavedArrangement>,
    /// Read from a file a newer MonHop wrote: empty here, and never saved over.
    #[serde(skip)]
    newer: bool,
}

/// An absent file's library, at the current version rather than a derived all-zero default.
impl Default for ArrangementLibrary {
    fn default() -> Self {
        Self {
            version: LIBRARY_VERSION,
            arrangements: Vec::new(),
            newer: false,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedArrangement {
    name: String,
    /// Its stamp means nothing here: a record loaded from the library is stamped anew when applied.
    record: GroupRecord,
    /// Remembered by MonHop at Apply instead of named by the user.
    automatic: bool,
}

/// The library as version 2 wrote it: one pairwise setup per entry.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LibraryV2 {
    version: u8,
    arrangements: Vec<SavedArrangementV2>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SavedArrangementV2 {
    name: String,
    setup: SharingPreferences,
    automatic: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementView {
    pub name: String,
    pub crossings: usize,
    /// Present only when it fits the displays every one of its computers shows right now.
    pub layout: Option<LayoutRequest>,
    /// Remembered by MonHop at Apply instead of named by the user.
    pub automatic: bool,
    /// Loadable right now: every computer it was made for shows the displays it was made with.
    /// Always false while one of them is not connected.
    pub fits: bool,
    /// The computers it was made for, this one included: lowercase and sorted.
    pub members: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryError {
    InvalidName,
    Full,
    Unknown,
}

impl LibraryError {
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidName => "Give the arrangement a name of up to 64 characters.",
            Self::Full => {
                "Delete an arrangement first. This computer keeps at most 32 for the same computers."
            }
            Self::Unknown => "That arrangement is no longer saved.",
        }
    }
}

impl ArrangementLibrary {
    /// `path` is this version's file. While it is absent, the version 2 file beside it is
    /// migrated in memory; that file is never written, so an older MonHop keeps its own
    /// arrangements, and the next save writes `path`. A file a newer MonHop wrote, in either
    /// place, loads empty and read-only, so no save hides it; one another older version wrote is
    /// empty, and a damaged one is an error rather than a silent reset.
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::load_as(path, crate::sharing::local_platform(), || {
            listed_platforms(path)
        })
    }

    /// A migrated entry names this computer with `platform` and the other computer with the
    /// platform `listed` gives it, else the other platform.
    fn load_as(
        path: &Path,
        platform: Platform,
        listed: impl FnOnce() -> BTreeMap<String, Platform>,
    ) -> io::Result<Self> {
        if let Some(bytes) = read_bounded(path, MAX_LIBRARY_BYTES)? {
            return match file_version(&bytes)? {
                LIBRARY_VERSION => {
                    let library: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
                    library.validate()?;
                    Ok(library)
                }
                version if version > LIBRARY_VERSION => Ok(Self::from_newer(version)),
                _ => Err(invalid()),
            };
        }
        let Some(bytes) = read_bounded(&path.with_file_name(V2_LIBRARY_FILE), MAX_LIBRARY_BYTES)?
        else {
            return Ok(Self::default());
        };
        match file_version(&bytes)? {
            LIBRARY_V2_VERSION => {
                let old: LibraryV2 = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
                old.validate()?;
                Ok(old.migrate(platform, &listed()))
            }
            // No MonHop up to this one writes past version 2 here; version 3 goes only to `path`.
            version if version > LIBRARY_V2_VERSION => Ok(Self::from_newer(version)),
            _ => Ok(Self::default()),
        }
    }

    /// Empty and never saved: a save would overwrite the newer file, or, when it sits in the
    /// version 2 place, hide it behind a `path` that is read instead from then on.
    fn from_newer(version: u8) -> Self {
        log::warn!(
            "arrangements: the saved arrangements are version {version}, from a newer MonHop; they are kept as they are and nothing is saved over them"
        );
        Self {
            newer: true,
            ..Self::default()
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if self.newer {
            return Err(newer_file());
        }
        let bytes = serde_json::to_vec(self).map_err(|_| invalid())?;
        save_bounded(path, &bytes, MAX_LIBRARY_BYTES)
    }

    /// Loaded from a file a newer MonHop wrote, which this version never saves over.
    pub fn written_by_newer(&self) -> bool {
        self.newer
    }

    /// Names are unique, and at most `MAX_ARRANGEMENTS` are kept, per set of computers.
    fn validate(&self) -> io::Result<()> {
        if self.version != LIBRARY_VERSION {
            return Err(invalid());
        }
        let mut names: BTreeMap<Vec<String>, BTreeSet<&str>> = BTreeMap::new();
        for entry in &self.arrangements {
            if normalize_name(&entry.name).as_deref() != Some(entry.name.as_str())
                || entry.record.validate().is_err()
            {
                return Err(invalid());
            }
            let kept = names.entry(entry.record.member_keys()).or_default();
            if !kept.insert(&entry.name) || kept.len() > MAX_ARRANGEMENTS {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// Saving under an existing name replaces that arrangement in place; names are per set of
    /// computers. A name the user gives is never a remembered entry afterwards.
    pub fn upsert(&mut self, name: &str, record: GroupRecord) -> Result<(), LibraryError> {
        let name = normalize_name(name).ok_or(LibraryError::InvalidName)?;
        let members = record.member_keys();
        if let Some(existing) = self.position(&name, &members) {
            self.arrangements[existing].record = record;
            self.arrangements[existing].automatic = false;
            return Ok(());
        }
        if self.kept_for(&members) >= MAX_ARRANGEMENTS {
            return Err(LibraryError::Full);
        }
        self.arrangements.push(SavedArrangement {
            name,
            record,
            automatic: false,
        });
        Ok(())
    }

    /// Remembers an applied record for the monitors it was made with: one remembered entry per
    /// set of computers and their monitors, newest last, never over a name the user gave. `local`
    /// is this computer, whose displays the name starts with. `record` must be valid.
    pub fn remember_automatically(
        &mut self,
        record: GroupRecord,
        local: &str,
    ) -> Result<bool, LibraryError> {
        let same_monitors =
            |entry: &SavedArrangement| entry.automatic && entry.record.same_monitors_as(&record);
        if self
            .arrangements
            .iter()
            .any(|entry| same_monitors(entry) && entry.record.same_content_as(&record))
        {
            return Ok(false);
        }
        self.arrangements.retain(|entry| !same_monitors(entry));
        let members = record.member_keys();
        if self.kept_for(&members) >= MAX_ARRANGEMENTS {
            let oldest = self
                .arrangements
                .iter()
                .position(|entry| entry.automatic && entry.record.member_keys() == members)
                .ok_or(LibraryError::Full)?;
            self.arrangements.remove(oldest);
        }
        let name = self
            .remembered_name(&record, local)
            .ok_or(LibraryError::InvalidName)?;
        self.arrangements.push(SavedArrangement {
            name,
            record,
            automatic: true,
        });
        Ok(true)
    }

    /// The most recently remembered entry for `current`'s computers that each of them shows
    /// right now, its ids rewritten to theirs: a reconnected monitor carries a new OS id but the
    /// same identity. A member `known` does not show must hold what `current` holds for it.
    pub fn automatic_fit(
        &self,
        current: &GroupRecord,
        known: &KnownDisplays,
    ) -> Option<GroupRecord> {
        let known = known.with_entries_of(current);
        self.automatic_for(current)
            .find_map(|record| record.remap_to_all(&known))
    }

    /// The most recently remembered entry for `current`'s computers made with exactly the
    /// monitors each of them shows now, whatever their geometry: what a changed arrangement is
    /// rebuilt from, so a display that only moved keeps every crossing that was made for it.
    pub fn automatic_for_same_displays(
        &self,
        current: &GroupRecord,
        known: &KnownDisplays,
    ) -> Option<&GroupRecord> {
        let known = known.with_entries_of(current);
        self.automatic_for(current)
            .find(|record| record.shows_same_monitors(&known))
    }

    /// Remembered entries for exactly `current`'s computers, newest first.
    fn automatic_for<'a>(
        &'a self,
        current: &GroupRecord,
    ) -> impl Iterator<Item = &'a GroupRecord> + 'a {
        let members = current.member_keys();
        self.arrangements
            .iter()
            .rev()
            .filter(move |entry| entry.automatic && entry.record.member_keys() == members)
            .map(|entry| &entry.record)
    }

    fn kept_for(&self, members: &[String]) -> usize {
        self.arrangements
            .iter()
            .filter(|entry| entry.record.member_keys() == members)
            .count()
    }

    /// Each computer's display names, this computer's first, trimmed to fit, then numbered when
    /// the same computers already hold that name.
    fn remembered_name(&self, record: &GroupRecord, local: &str) -> Option<String> {
        let local = fingerprint_key(local);
        let mut members: Vec<&GroupMember> = record.members().iter().collect();
        members.sort_by_key(|member| fingerprint_key(member.fingerprint()) != local);
        let base = members
            .iter()
            .map(|member| {
                member
                    .displays()
                    .iter()
                    .map(|display| readable(display.name()))
                    .collect::<Vec<_>>()
                    .join(" + ")
            })
            .collect::<Vec<_>>()
            .join(" \u{2194} ");
        let members = record.member_keys();
        for attempt in 1..=MAX_ARRANGEMENTS + 1 {
            let suffix = if attempt == 1 {
                String::new()
            } else {
                format!(" {attempt}")
            };
            let room = MAX_NAME_CHARS - suffix.chars().count();
            let candidate = normalize_name(&format!("{}{suffix}", shortened(&base, room)))?;
            if self.position(&candidate, &members).is_none() {
                return Some(candidate);
            }
        }
        None
    }

    /// Forgets one arrangement made with `computer`, connected or not: the one for exactly
    /// `members` when given, else the only one of that name, the pair's own when several share
    /// it. This library is this computer's own, so an entry's members say whose it is.
    pub fn remove_for(
        &mut self,
        computer: &str,
        name: &str,
        members: Option<&[String]>,
    ) -> Result<(), LibraryError> {
        let name = normalize_name(name).ok_or(LibraryError::InvalidName)?;
        let members = members.map(member_set);
        let named: Vec<usize> = self
            .arrangements
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.name == name
                    && entry.record.has_member(computer)
                    && members
                        .as_ref()
                        .is_none_or(|members| entry.record.member_keys() == *members)
            })
            .map(|(index, _)| index)
            .collect();
        let index = match named.as_slice() {
            [only] => *only,
            _ => named
                .into_iter()
                .find(|index| self.arrangements[*index].record.members().len() == 2)
                .ok_or(LibraryError::Unknown)?,
        };
        self.arrangements.remove(index);
        Ok(())
    }

    /// The arrangements made for exactly `members`, in any case and order, each with its layout
    /// when it fits the displays `known` shows.
    pub fn views(&self, members: &[String], known: &KnownDisplays) -> Vec<ArrangementView> {
        let members = member_set(members);
        self.arrangements
            .iter()
            .filter(|entry| entry.record.member_keys() == members)
            .map(|entry| view(entry, known))
            .collect()
    }

    /// Every arrangement made with `computer`, listable while it is not connected. An entry fits
    /// only while `known` shows every one of its computers, so one computer's displays never
    /// vouch for another's.
    pub fn views_for(&self, computer: &str, known: &KnownDisplays) -> Vec<ArrangementView> {
        self.arrangements
            .iter()
            .filter(|entry| entry.record.has_member(computer))
            .map(|entry| view(entry, known))
            .collect()
    }

    fn position(&self, name: &str, members: &[String]) -> Option<usize> {
        self.arrangements
            .iter()
            .position(|entry| entry.name == name && entry.record.member_keys() == members)
    }
}

impl LibraryV2 {
    fn validate(&self) -> io::Result<()> {
        if self.version != LIBRARY_V2_VERSION
            || self.arrangements.iter().any(|entry| {
                normalize_name(&entry.name).as_deref() != Some(entry.name.as_str())
                    || entry.setup.validate().is_err()
            })
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Every entry becomes the two-member group of its pair with its layout verbatim. One that
    /// cannot be carried over, or would repeat a name or pass the bound for its computers, goes.
    fn migrate(
        self,
        platform: Platform,
        listed: &BTreeMap<String, Platform>,
    ) -> ArrangementLibrary {
        let mut library = ArrangementLibrary::default();
        let mut dropped = 0_usize;
        for entry in self.arrangements {
            let peer = listed_platform(listed, &entry.setup.peer_fingerprint, platform);
            let Ok(record) = GroupRecord::from_pairwise(&entry.setup, platform, peer) else {
                dropped += 1;
                continue;
            };
            let members = record.member_keys();
            if library.kept_for(&members) >= MAX_ARRANGEMENTS
                || library.position(&entry.name, &members).is_some()
            {
                dropped += 1;
                continue;
            }
            library.arrangements.push(SavedArrangement {
                name: entry.name,
                record,
                automatic: entry.automatic,
            });
        }
        if dropped > 0 {
            log::warn!(
                "arrangements: dropped {dropped} saved arrangements that could not be carried over"
            );
        }
        library
    }
}

fn view(entry: &SavedArrangement, known: &KnownDisplays) -> ArrangementView {
    let fitted = entry
        .record
        .remap_to_all(known)
        .map(|fitted| fitted.layout().clone());
    ArrangementView {
        name: entry.name.clone(),
        crossings: entry.record.layout().links.len() / 2,
        fits: fitted.is_some(),
        layout: fitted,
        automatic: entry.automatic,
        members: entry.record.member_keys(),
    }
}

/// Identities reach here lowercase from the window and uppercase from a record, so one sorted
/// lowercase form decides which computers an entry was made with.
fn member_set(members: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = members
        .iter()
        .map(|member| fingerprint_key(member))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Display names come from the OS; a control character would make an unloadable entry.
/// A generated name that does not fit is cut before the mark, never leaving a bare separator.
fn shortened(base: &str, room: usize) -> String {
    if base.chars().count() <= room {
        return base.to_owned();
    }
    let head: String = base.chars().take(room.saturating_sub(1)).collect();
    let head = head.trim_end_matches([' ', '+', '\u{2194}']).trim_end();
    format!("{head}\u{2026}")
}

fn readable(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Trimmed, single-line, at most 64 characters; case is kept so names read as typed.
pub fn normalize_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_NAME_CHARS
        || trimmed.chars().any(char::is_control)
    {
        return None;
    }
    Some(trimmed.to_owned())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid arrangement library")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computers::{ComputerList, LIST_FILE};
    use crate::sharing_preferences::ComputerPlatform;
    use crate::sharing_preferences::tests::{
        group, inspection, preferences, preferences_for_peer, trio, two_display_record,
    };
    use monhop_transport::crypto::CertificateFingerprint;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// The computer each fixture record was made with.
    fn peer_key(letter: char) -> String {
        letter.to_string().repeat(64)
    }

    /// This computer in every fixture.
    fn local() -> String {
        peer_key('A')
    }

    /// Both computers of `setup`'s link, as the link shows them.
    fn known(setup: &SharingPreferences) -> KnownDisplays {
        KnownDisplays::of_link(&inspection(setup))
    }

    fn pair(setup: &SharingPreferences) -> Vec<String> {
        vec![
            setup.local_fingerprint.clone(),
            setup.peer_fingerprint.clone(),
        ]
    }

    fn members(owners: &[char]) -> Vec<String> {
        owners.iter().map(|owner| peer_key(*owner)).collect()
    }

    fn remember(library: &mut ArrangementLibrary, setup: &SharingPreferences) -> bool {
        library
            .remember_automatically(group(setup), &local())
            .unwrap()
    }

    /// `setup` with the other computer allowed to control this one no more: another layout for
    /// the same monitors.
    fn one_way(setup: &SharingPreferences) -> SharingPreferences {
        let mut changed = setup.clone();
        changed
            .layout
            .control
            .insert(fingerprint_key(&setup.peer_fingerprint), false);
        changed
    }

    fn directory() -> std::path::PathBuf {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "monhop-arrangements-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn names_are_trimmed_single_line_and_bounded() {
        assert_eq!(normalize_name("  Desk  ").as_deref(), Some("Desk"));
        assert_eq!(normalize_name(""), None);
        assert_eq!(normalize_name("   "), None);
        assert_eq!(normalize_name("two\nlines"), None);
        assert_eq!(normalize_name(&"x".repeat(64)).map(|n| n.len()), Some(64));
        assert_eq!(normalize_name(&"x".repeat(65)), None);
    }

    #[test]
    fn saving_under_an_existing_name_replaces_it_and_the_library_round_trips() {
        let directory = directory();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        let mut library = ArrangementLibrary::load(&path).unwrap();
        assert!(library.arrangements.is_empty());
        library.upsert("Desk", group(&preferences())).unwrap();
        let replaced = group(&one_way(&preferences()));
        library.upsert(" Desk ", replaced.clone()).unwrap();
        library.upsert("Couch", group(&preferences())).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        assert_eq!(library.arrangements[0].record, replaced);
        library.save(&path).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], 3);
        assert!(written["arrangements"][0]["setup"].is_null());
        assert_eq!(
            written["arrangements"][0]["record"]["members"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let loaded = ArrangementLibrary::load(&path).unwrap();
        assert_eq!(
            loaded
                .arrangements
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Desk", "Couch"]
        );
        let mut loaded = loaded;
        assert_eq!(
            loaded.remove_for(&peer_key('B'), "Nope", None),
            Err(LibraryError::Unknown)
        );
        loaded.remove_for(&peer_key('B'), "Desk", None).unwrap();
        assert_eq!(loaded.arrangements.len(), 1);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn names_limits_and_deletion_are_scoped_to_one_pair_of_computers() {
        let mut library = ArrangementLibrary::default();
        let other = preferences_for_peer('C');
        library.upsert("Desk", group(&preferences())).unwrap();
        library.upsert("Desk", group(&other)).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        for index in 0..MAX_ARRANGEMENTS - 1 {
            library.upsert(&format!("n{index}"), group(&other)).unwrap();
        }
        assert_eq!(
            library.upsert("one more", group(&other)),
            Err(LibraryError::Full)
        );
        library.upsert("Couch", group(&preferences())).unwrap();
        let first = || library.views(&pair(&preferences()), &known(&preferences()));
        assert_eq!(first().len(), 2);
        assert_eq!(
            library.views(&pair(&other), &known(&other)).len(),
            MAX_ARRANGEMENTS
        );
        assert_eq!(
            library.remove_for(&peer_key('C'), "Couch", None),
            Err(LibraryError::Unknown)
        );
        library.remove_for(&peer_key('C'), "Desk", None).unwrap();
        assert_eq!(
            library
                .views(&pair(&preferences()), &known(&preferences()))
                .len(),
            2
        );
        assert_eq!(
            library.views(&pair(&other), &known(&other)).len(),
            MAX_ARRANGEMENTS - 1
        );
    }

    #[test]
    fn names_and_limits_are_per_member_set() {
        let mut library = ArrangementLibrary::default();
        let three = trio(1, true);
        // The same name is free again for another set of computers, even one with more members.
        library.upsert("Desk", group(&preferences())).unwrap();
        library.upsert("Desk", three.clone()).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        for index in 0..MAX_ARRANGEMENTS - 1 {
            library.upsert(&format!("n{index}"), three.clone()).unwrap();
        }
        assert_eq!(
            library.upsert("one more", three.clone()),
            Err(LibraryError::Full)
        );
        // A full set leaves every other set its own room.
        library.upsert("Couch", group(&preferences())).unwrap();
        assert_eq!(
            library
                .views(&members(&['C', 'B', 'A']), &KnownDisplays::default())
                .len(),
            MAX_ARRANGEMENTS
        );
        assert_eq!(
            library
                .views(&members(&['A', 'B']), &KnownDisplays::default())
                .len(),
            2
        );
        // A remembered layout has the full set of three to itself as well.
        assert_eq!(
            library.remember_automatically(three.clone(), &local()),
            Err(LibraryError::Full)
        );
        assert_eq!(
            library.remember_automatically(group(&preferences()), &local()),
            Ok(true)
        );
        // Saving under a name another set holds changes only this set's entry.
        let replaced = three
            .with_control(trio(1, false).layout().control.clone())
            .unwrap();
        library.upsert("Desk", replaced.clone()).unwrap();
        assert_eq!(library.arrangements[0].record, group(&preferences()));
        assert_eq!(library.arrangements[1].record, replaced);
        // A file holding more for one set than this version keeps is not one it wrote.
        let directory = directory();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        library.save(&path).unwrap();
        let mut past: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut extra = past["arrangements"][1].clone();
        extra["name"] = serde_json::json!("n99");
        past["arrangements"].as_array_mut().unwrap().push(extra);
        std::fs::write(&path, serde_json::to_vec(&past).unwrap()).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        let mut repeated: serde_json::Value = past.clone();
        repeated["arrangements"].as_array_mut().unwrap().pop();
        repeated["arrangements"][2]["name"] = serde_json::json!("Desk");
        std::fs::write(&path, serde_json::to_vec(&repeated).unwrap()).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_computers_arrangements_list_and_forget_while_it_is_not_connected() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", group(&preferences())).unwrap();
        remember(&mut library, &preferences());
        library
            .upsert("Elsewhere", group(&preferences_for_peer('C')))
            .unwrap();
        let listed = library.views_for(&peer_key('B'), &KnownDisplays::default());
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Desk");
        // Nothing fits while nothing is connected, so nothing offers a layout to load.
        assert!(listed.iter().all(|entry| !entry.fits));
        assert!(listed.iter().all(|entry| entry.layout.is_none()));
        // The exact names the window reads, so a change here is a change the window sees.
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(
            json.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "automatic",
                "crossings",
                "fits",
                "layout",
                "members",
                "name"
            ]
        );
        assert_eq!(json["name"], "Desk");
        assert_eq!(
            json["members"],
            serde_json::json!(["a".repeat(64), "b".repeat(64)])
        );
        assert_eq!(json["crossings"], 1);
        assert_eq!(json["automatic"], false);
        assert_eq!(json["fits"], false);
        assert!(json["layout"].is_null());
        // The same list with that computer's own live displays marks what fits and carries it.
        let connected = library.views_for(&peer_key('B'), &known(&preferences()));
        assert!(connected.iter().all(|entry| entry.fits));
        assert_eq!(connected[0].layout.as_ref(), Some(&preferences().layout));
        // Another computer's displays never vouch for this one's entries.
        assert!(
            library
                .views_for(&peer_key('B'), &known(&preferences_for_peer('C')))
                .iter()
                .all(|entry| !entry.fits)
        );
        // Identities arrive lowercase from the window and uppercase from a record; both answer.
        assert_eq!(
            library
                .views_for(&peer_key('b'), &KnownDisplays::default())
                .len(),
            2
        );
        library.remove_for(&peer_key('b'), "Desk", None).unwrap();
        assert_eq!(
            library
                .views_for(&peer_key('B'), &KnownDisplays::default())
                .len(),
            1
        );
        // A forget never reaches another computer's entries.
        assert_eq!(
            library.remove_for(&peer_key('B'), "Elsewhere", None),
            Err(LibraryError::Unknown)
        );
        assert_eq!(
            library
                .views_for(&peer_key('C'), &KnownDisplays::default())
                .len(),
            1
        );
    }

    #[test]
    fn views_for_lists_every_group_containing_the_computer() {
        let mut library = ArrangementLibrary::default();
        let three = trio(1, true);
        library.upsert("Desk", group(&preferences())).unwrap();
        library
            .upsert("Couch", group(&preferences_for_peer('C')))
            .unwrap();
        library.upsert("Trio", three.clone()).unwrap();
        library.upsert("Desk", three.clone()).unwrap();
        let names = |library: &ArrangementLibrary, computer: char, known: &KnownDisplays| {
            library
                .views_for(&peer_key(computer), known)
                .into_iter()
                .map(|view| (view.name, view.fits))
                .collect::<Vec<_>>()
        };
        let nothing = KnownDisplays::default();
        assert_eq!(
            names(&library, 'B', &nothing),
            [
                ("Desk".to_owned(), false),
                ("Trio".to_owned(), false),
                ("Desk".to_owned(), false)
            ]
        );
        assert_eq!(
            names(&library, 'C', &nothing),
            [
                ("Couch".to_owned(), false),
                ("Trio".to_owned(), false),
                ("Desk".to_owned(), false)
            ]
        );
        assert!(names(&library, 'D', &nothing).is_empty());
        // This computer is in every one of its own entries.
        assert_eq!(names(&library, 'a', &nothing).len(), 4);
        // A group fits only while every member shows its displays; the pair's link alone is not
        // enough for the group of three.
        assert_eq!(
            names(&library, 'B', &known(&preferences())),
            [
                ("Desk".to_owned(), true),
                ("Trio".to_owned(), false),
                ("Desk".to_owned(), false)
            ]
        );
        let mut everyone = KnownDisplays::default();
        for member in three.members() {
            everyone.insert(
                member.fingerprint(),
                member.platform().into(),
                &crate::sharing_preferences::topology_of(member.displays()).unwrap(),
            );
        }
        let listed = library.views_for(&peer_key('B'), &everyone);
        assert!(listed[1].fits);
        assert_eq!(listed[1].layout.as_ref(), Some(three.layout()));
        assert_eq!(listed[1].crossings, 2);
        // Without its computers, a forget names the pair's own entry when groups share the name.
        library.remove_for(&peer_key('B'), "Desk", None).unwrap();
        assert_eq!(
            names(&library, 'B', &nothing),
            [("Trio".to_owned(), false), ("Desk".to_owned(), false)]
        );
        assert_eq!(
            library.remove_for(&peer_key('B'), "Desk", Some(&members(&['A', 'B']))),
            Err(LibraryError::Unknown)
        );
        library
            .remove_for(&peer_key('b'), "Desk", Some(&members(&['c', 'B', 'a'])))
            .unwrap();
        // The only entry of a name is found without its computers, whoever else is in it.
        library.remove_for(&peer_key('C'), "Trio", None).unwrap();
        assert!(names(&library, 'B', &nothing).is_empty());
        assert_eq!(
            names(&library, 'C', &nothing),
            [("Couch".to_owned(), false)]
        );
    }

    #[test]
    fn remembering_replaces_the_entry_for_the_same_displays_and_leaves_names_alone() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", group(&preferences())).unwrap();
        remember(&mut library, &preferences());
        let moved = one_way(&preferences());
        remember(&mut library, &moved);
        assert_eq!(library.arrangements.len(), 2);
        assert_eq!(library.arrangements[0].name, "Desk");
        assert!(!library.arrangements[0].automatic);
        assert!(library.arrangements[1].automatic);
        assert_eq!(library.arrangements[1].record, group(&moved));
        let mut other = preferences();
        other.set_local_displays_for_test(&["1", "3"]);
        remember(&mut library, &other);
        assert_eq!(library.arrangements.len(), 3);
        let listed = library.views(&pair(&preferences()), &known(&preferences()));
        assert!(!listed[0].automatic);
        assert!(listed[1].automatic);
        // Saving over a remembered name makes it the user's own arrangement.
        let remembered = library.arrangements[1].name.clone();
        library.upsert(&remembered, group(&preferences())).unwrap();
        assert_eq!(library.arrangements.len(), 3);
        assert!(!library.arrangements[1].automatic);
    }

    #[test]
    fn remembering_the_same_record_again_changes_nothing() {
        let mut library = ArrangementLibrary::default();
        assert!(remember(&mut library, &preferences()));
        assert!(!remember(&mut library, &preferences()));
        assert_eq!(library.arrangements.len(), 1);
        // Only the content counts: the same layout stamped again is the same memory.
        let restamped = group(&preferences()).restamped(9, &peer_key('B')).unwrap();
        assert_eq!(
            library.remember_automatically(restamped, &local()),
            Ok(false)
        );
        assert!(remember(&mut library, &one_way(&preferences())));
        assert_eq!(library.arrangements.len(), 1);
    }

    #[test]
    fn the_cap_evicts_the_oldest_remembered_entry_and_skips_a_library_of_names() {
        let mut library = ArrangementLibrary::default();
        for index in 0..MAX_ARRANGEMENTS - 1 {
            library
                .upsert(&format!("n{index}"), group(&preferences()))
                .unwrap();
        }
        let mut first = preferences();
        first.set_local_displays_for_test(&["1"]);
        first.set_local_display_names_for_test("First");
        remember(&mut library, &first);
        assert_eq!(library.arrangements.len(), MAX_ARRANGEMENTS);
        let mut second = preferences();
        second.set_local_displays_for_test(&["1", "3"]);
        second.set_local_display_names_for_test("Second");
        remember(&mut library, &second);
        assert_eq!(library.arrangements.len(), MAX_ARRANGEMENTS);
        assert!(
            !library
                .arrangements
                .iter()
                .any(|entry| entry.name.starts_with("First"))
        );
        assert_eq!(
            library.arrangements.iter().filter(|e| e.automatic).count(),
            1
        );
        let mut named = ArrangementLibrary::default();
        for index in 0..MAX_ARRANGEMENTS {
            named
                .upsert(&format!("n{index}"), group(&preferences()))
                .unwrap();
        }
        assert_eq!(
            named.remember_automatically(group(&preferences()), &local()),
            Err(LibraryError::Full)
        );
        assert_eq!(named.arrangements.len(), MAX_ARRANGEMENTS);
    }

    #[test]
    fn remembered_names_stay_unique_within_the_pair_and_within_64_characters() {
        let mut library = ArrangementLibrary::default();
        let mut one = preferences();
        one.set_local_displays_for_test(&["1"]);
        one.set_local_display_names_for_test(&"L".repeat(70));
        let mut two = preferences();
        two.set_local_displays_for_test(&["1", "3"]);
        two.set_local_display_names_for_test(&"L".repeat(70));
        remember(&mut library, &one);
        remember(&mut library, &two);
        let names: Vec<&str> = library
            .arrangements
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names[0].chars().count(), MAX_NAME_CHARS);
        assert!(names[0].ends_with('\u{2026}'));
        assert!(names.iter().all(|name| {
            !name
                .trim_end_matches(char::is_numeric)
                .trim_end()
                .ends_with('+')
        }));
        assert!(names[1].ends_with(" 2"));
        assert!(names[1].chars().count() <= MAX_NAME_CHARS);
        assert_ne!(names[0], names[1]);
        assert!(
            names
                .iter()
                .all(|name| normalize_name(name).as_deref() == Some(*name))
        );
        // Each computer names the pair from its own side.
        let mut here = ArrangementLibrary::default();
        here.remember_automatically(group(&preferences()), &local())
            .unwrap();
        assert_eq!(
            here.arrangements[0].name,
            "Local display \u{2194} Peer display"
        );
        let mut there = ArrangementLibrary::default();
        there
            .remember_automatically(group(&preferences()), &peer_key('b'))
            .unwrap();
        assert_eq!(
            there.arrangements[0].name,
            "Peer display \u{2194} Local display"
        );
    }

    #[test]
    fn a_remembered_arrangement_is_found_by_the_connected_displays() {
        let mut library = ArrangementLibrary::default();
        let current = group(&preferences());
        library.upsert("Desk", group(&preferences())).unwrap();
        let pair = known(&preferences());
        assert_eq!(library.automatic_fit(&current, &pair), None);
        remember(&mut library, &preferences());
        assert_eq!(
            library.automatic_fit(&current, &pair),
            Some(group(&preferences()))
        );
        let mut other = preferences();
        other.set_local_displays_for_test(&["1", "3"]);
        remember(&mut library, &other);
        // The entry that fits is chosen, not simply the newest.
        assert_eq!(
            library.automatic_fit(&current, &pair),
            Some(group(&preferences()))
        );
        assert_eq!(
            library.automatic_fit(&current, &known(&other)),
            Some(group(&other))
        );
        let elsewhere = preferences_for_peer('C');
        assert_eq!(
            library.automatic_fit(&group(&elsewhere), &known(&elsewhere)),
            None
        );
    }

    #[test]
    fn a_three_member_memory_is_found_only_with_the_same_members() {
        let three = trio(1, true);
        let mut library = ArrangementLibrary::default();
        assert_eq!(
            library.remember_automatically(three.clone(), &local()),
            Ok(true)
        );
        let live = |owners: &[char]| {
            let mut known = KnownDisplays::default();
            for owner in owners {
                let member = three.member(&peer_key(*owner)).unwrap();
                known.insert(
                    member.fingerprint(),
                    member.platform().into(),
                    &crate::sharing_preferences::topology_of(member.displays()).unwrap(),
                );
            }
            known
        };
        // Every member live, or one offline that still holds what the running record holds.
        assert_eq!(
            library.automatic_fit(&three, &live(&['A', 'B', 'C'])),
            Some(three.clone())
        );
        assert_eq!(
            library.automatic_fit(&three, &live(&['A', 'B'])),
            Some(three.clone())
        );
        assert_eq!(
            library.automatic_for_same_displays(&three, &live(&['A', 'B'])),
            Some(&three)
        );
        // The pair of two of its members never finds the memory of three, nor the reverse.
        let pair = GroupRecord::new(
            1,
            &local(),
            three
                .members()
                .iter()
                .filter(|member| member.fingerprint() != peer_key('C'))
                .cloned()
                .collect(),
            LayoutRequest {
                links: three.layout().links[..2].to_vec(),
                arrangement: None,
                control: crate::sharing_preferences::both_directions(&local(), &peer_key('B')),
            },
        )
        .unwrap();
        assert_eq!(library.automatic_fit(&pair, &live(&['A', 'B'])), None);
        assert_eq!(
            library.automatic_for_same_displays(&pair, &live(&['A', 'B'])),
            None
        );
        library
            .remember_automatically(pair.clone(), &local())
            .unwrap();
        assert_eq!(
            library.automatic_fit(&pair, &live(&['A', 'B'])),
            Some(pair.clone())
        );
        assert_eq!(
            library.automatic_fit(&three, &live(&['A', 'B', 'C'])),
            Some(three.clone())
        );
        // An offline member the running record saw with other displays rules the memory out.
        let mut moved = serde_json::to_value(&three).unwrap();
        moved["members"][2]["displays"][0]["origin"] = serde_json::json!([0.0, 500.0]);
        let moved: GroupRecord = serde_json::from_value(moved).unwrap();
        assert_eq!(moved.validate(), Ok(()));
        assert_eq!(library.automatic_fit(&moved, &live(&['A', 'B'])), None);
        assert_eq!(
            library.automatic_for_same_displays(&moved, &live(&['A', 'B'])),
            Some(&three)
        );
    }

    #[test]
    fn a_reconnected_monitor_is_the_same_remembered_displays_under_its_new_id() {
        let record = two_display_record();
        let current = group(&record);
        let mut reconnected = record.clone();
        reconnected.set_local_display_id_for_test("3", "7");
        let live = known(&reconnected);
        // The memory made before the reconnect is found and comes back naming the new id.
        let mut library = ArrangementLibrary::default();
        remember(&mut library, &record);
        let fitted = library
            .automatic_fit(&current, &live)
            .expect("the remembered monitors fit under their new id");
        assert!(fitted.fits_link(&inspection(&reconnected)));
        assert_eq!(fitted.layout().links[0].to_display, "7");
        assert_eq!(
            library.automatic_for_same_displays(&current, &live),
            Some(&group(&record))
        );
        // Remembering the refreshed record replaces the memory instead of adding one.
        assert_eq!(
            library.remember_automatically(fitted.clone(), &local()),
            Ok(true)
        );
        assert_eq!(library.arrangements.len(), 1);
        assert_eq!(library.automatic_fit(&current, &live), Some(fitted.clone()));
        // Moved as well, the memory no longer fits exactly but is still the one to rebuild from.
        let mut moved = reconnected.clone();
        moved.move_local_display_for_test("7", [1920.0, 0.0]);
        let moved = known(&moved);
        assert_eq!(library.automatic_fit(&current, &moved), None);
        assert_eq!(
            library.automatic_for_same_displays(&current, &moved),
            Some(&fitted)
        );
        // A memory made with another pair of computers is never offered.
        let mut other_pair = ArrangementLibrary::default();
        let mut other = record.clone();
        other.set_peer_fingerprint_for_test('C');
        remember(&mut other_pair, &other);
        assert_eq!(other_pair.automatic_fit(&current, &live), None);
        assert_eq!(
            other_pair.automatic_for_same_displays(&current, &live),
            None
        );
    }

    #[test]
    fn a_named_arrangement_is_offered_with_the_ids_the_monitors_carry_now() {
        let record = two_display_record();
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", group(&record)).unwrap();
        let mut reconnected = record.clone();
        reconnected.set_local_display_id_for_test("3", "7");
        let listed = library.views(&pair(&record), &known(&reconnected));
        let layout = listed[0].layout.as_ref().expect("the arrangement fits");
        assert_eq!(layout.links[0].to_display, "7");
        assert_eq!(layout.links[1].from_display, "7");
        // Named entries are never promoted on their own.
        assert_eq!(
            library.automatic_fit(&group(&record), &known(&reconnected)),
            None
        );
    }

    /// The Mac's library exactly as version 2 wrote it: a named layout and a remembered one with
    /// a PC that has two displays, and a named one with a second PC whose input may not control
    /// the Mac.
    const V2_LIBRARY: &str = r#"{
  "version": 2,
  "arrangements": [
    {
      "name": "Desk",
      "setup": {
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
      "automatic": false
    },
    {
      "name": "Built-in Retina Display ↔ DELL U2723QE + Generic PnP Monitor",
      "setup": {
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
            {"fromDisplay": "5764607523034234881", "fromEdge": "left", "fromSpan": [0.0, 1.0],
             "toDisplay": "12297829382473034410", "toEdge": "right", "toSpan": [0.0, 1.0],
             "hysteresis": 1.0},
            {"fromDisplay": "12297829382473034410", "fromEdge": "right", "fromSpan": [0.0, 1.0],
             "toDisplay": "5764607523034234881", "toEdge": "left", "toSpan": [0.0, 1.0],
             "hysteresis": 1.0}
          ],
          "control": {
            "2b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a93572b8f0c6d4e1a9357": true,
            "7c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f87c1e5a9034b6d2f8": true
          }
        }
      },
      "automatic": true
    },
    {
      "name": "Couch",
      "setup": {
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
      },
      "automatic": false
    }
  ]
}"#;

    const MAC: &str = "7C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F87C1E5A9034B6D2F8";
    const PC: &str = "2B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A93572B8F0C6D4E1A9357";
    const SECOND_PC: &str = "E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856E04A7B3C9D2F1856";

    /// A folder holding `V2_LIBRARY` where version 2 kept it, and the path of this version's file.
    fn v2_folder() -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = directory();
        std::fs::write(directory.join(V2_LIBRARY_FILE), V2_LIBRARY).unwrap();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        (directory, path)
    }

    #[test]
    fn a_v2_library_migrates_every_entry() {
        let (directory, path) = v2_folder();
        let library = ArrangementLibrary::load_as(&path, Platform::MacOs, BTreeMap::new).unwrap();
        assert_eq!(library.version, LIBRARY_VERSION);
        assert!(!library.written_by_newer());
        let old: serde_json::Value = serde_json::from_str(V2_LIBRARY).unwrap();
        assert_eq!(library.arrangements.len(), 3);
        for (index, entry) in library.arrangements.iter().enumerate() {
            let before = &old["arrangements"][index];
            let setup: SharingPreferences =
                serde_json::from_value(before["setup"].clone()).unwrap();
            // Name, kind, displays, ids, monitors, arrangement and control verbatim; no network.
            assert_eq!(entry.name, before["name"].as_str().unwrap());
            assert_eq!(entry.automatic, before["automatic"].as_bool().unwrap());
            assert_eq!(entry.record.layout(), &setup.layout);
            assert_eq!(
                entry.record.to_pairwise(MAC, &setup.interface_id),
                Ok(setup.clone())
            );
            // Without a computer list, the other computer is taken for the other platform.
            let platform = |owner: &str| entry.record.member(owner).unwrap().platform();
            assert_eq!(platform(MAC), ComputerPlatform::Macos);
            assert_eq!(platform(&setup.peer_fingerprint), ComputerPlatform::Windows);
        }
        let names = |computer: &str| {
            library
                .views_for(computer, &KnownDisplays::default())
                .into_iter()
                .map(|view| view.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(PC),
            [
                "Desk",
                "Built-in Retina Display \u{2194} DELL U2723QE + Generic PnP Monitor"
            ]
        );
        assert_eq!(names(SECOND_PC), ["Couch"]);
        // The remembered entry is found for the displays it was made with, as it was before.
        let desk = &library.arrangements[0].record;
        let remembered = &library.arrangements[1].record;
        let live = crate::sharing_preferences::tests::link_of(desk, MAC, PC);
        assert_eq!(
            library.automatic_fit(desk, &KnownDisplays::of_link(&live)),
            Some(remembered.clone())
        );
        // Loading changes nothing on disk; the version 2 file stays for an older MonHop.
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(directory.join(V2_LIBRARY_FILE)).unwrap(),
            V2_LIBRARY.as_bytes()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_v2_library_names_each_computer_by_the_platform_the_list_records() {
        // Two Macs: the list records the first peer as a Mac, so the pair migrates as two Macs.
        let (directory, path) = v2_folder();
        let mut list = ComputerList::default();
        list.remember(
            CertificateFingerprint::parse_full(PC).unwrap(),
            "192.168.1.9:24872".parse().unwrap(),
            Some(Platform::MacOs),
        )
        .unwrap();
        list.save(&directory.join(LIST_FILE)).unwrap();
        let library =
            ArrangementLibrary::load_as(&path, Platform::MacOs, || listed_platforms(&path))
                .unwrap();
        let desk = &library.arrangements[0].record;
        assert!(
            desk.members()
                .iter()
                .all(|member| member.platform() == ComputerPlatform::Macos)
        );
        // The computer the list does not name is still taken for the other platform.
        let couch = &library.arrangements[2].record;
        assert_eq!(
            couch.member(SECOND_PC).unwrap().platform(),
            ComputerPlatform::Windows
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_v3_library_save_never_touches_the_v2_file() {
        let (directory, path) = v2_folder();
        let mut library = ArrangementLibrary::load(&path).unwrap();
        assert_eq!(library.arrangements.len(), 3);
        library.upsert("Studio", group(&preferences())).unwrap();
        library.save(&path).unwrap();
        assert_eq!(
            std::fs::read(directory.join(V2_LIBRARY_FILE)).unwrap(),
            V2_LIBRARY.as_bytes()
        );
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], 3);
        assert_eq!(
            ArrangementLibrary::load(&path).unwrap().arrangements.len(),
            4
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn the_v2_library_is_migrated_only_when_the_new_file_is_absent() {
        let (directory, path) = v2_folder();
        // Once this version wrote its own file, the version 2 file is never read again.
        ArrangementLibrary::default().save(&path).unwrap();
        assert!(
            ArrangementLibrary::load(&path)
                .unwrap()
                .arrangements
                .is_empty()
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            ArrangementLibrary::load(&path).unwrap().arrangements.len(),
            3
        );
        // A version 2 file where this version's belongs is not one this version wrote.
        std::fs::write(&path, V2_LIBRARY).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_newer_library_loads_read_only_and_refuses_to_save() {
        let directory = directory();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        let newer = br#"{"version":4,"arrangements":[{"name":"Desk","future":true}],"more":1}"#;
        std::fs::write(&path, newer).unwrap();
        let mut library = ArrangementLibrary::load(&path).unwrap();
        assert!(library.written_by_newer());
        assert!(library.arrangements.is_empty());
        library.upsert("Desk", group(&preferences())).unwrap();
        assert_eq!(
            library.save(&path).map_err(|error| error.kind()),
            Err(io::ErrorKind::Unsupported)
        );
        let refused = crate::sharing::save_library(&path, &library).unwrap_err();
        assert!(refused.contains("newer version of MonHop"), "{refused}");
        assert_eq!(std::fs::read(&path).unwrap(), newer);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_newer_legacy_library_loads_read_only() {
        for version in [LIBRARY_VERSION, LIBRARY_VERSION + 1] {
            let directory = directory();
            let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
            let old_path = directory.join(V2_LIBRARY_FILE);
            let mut newer: serde_json::Value = serde_json::from_str(V2_LIBRARY).unwrap();
            newer["version"] = serde_json::json!(version);
            newer["more"] = serde_json::json!(1);
            let newer = serde_json::to_vec(&newer).unwrap();
            std::fs::write(&old_path, &newer).unwrap();
            let mut library = ArrangementLibrary::load(&path).unwrap();
            assert!(library.written_by_newer(), "version {version}");
            assert!(library.arrangements.is_empty());

            // The first save is refused, so the newer file is never hidden behind one written here.
            library.upsert("Desk", group(&preferences())).unwrap();
            assert_eq!(
                library.save(&path).map_err(|error| error.kind()),
                Err(io::ErrorKind::Unsupported)
            );
            let names: Vec<_> = std::fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [V2_LIBRARY_FILE], "no file was created");
            assert_eq!(std::fs::read(&old_path).unwrap(), newer);
            assert!(ArrangementLibrary::load(&path).unwrap().written_by_newer());
            let _ = std::fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn v014_arrangement_library_loads_as_empty() {
        let directory = directory();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        let old_path = directory.join(V2_LIBRARY_FILE);
        let v014 = br#"{"version":1,"arrangements":[{"name":"Desk","automatic":true,"setup":{"version":1,"peerFingerprint":"BB","sourceDisplay":"2","sourcePlatform":"windows","sharingEnabled":true,"layout":{"links":[],"sourceDisplay":"2","arrangement":{"mode":"grouped","positions":{},"hidden":[]}}}}]}"#;
        std::fs::write(&old_path, v014).unwrap();
        let loaded = ArrangementLibrary::load(&path).unwrap();
        assert!(loaded.arrangements.is_empty());
        assert_eq!(loaded.version, LIBRARY_VERSION);
        // Loading never rewrites the old file, and saving writes this version's beside it.
        assert_eq!(std::fs::read(&old_path).unwrap(), v014);
        let mut library = loaded;
        remember(&mut library, &preferences());
        library.save(&path).unwrap();
        assert_eq!(
            ArrangementLibrary::load(&path).unwrap().arrangements.len(),
            1
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), v014);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn the_library_is_bounded_and_rejects_damaged_files() {
        let mut library = ArrangementLibrary::default();
        for index in 0..MAX_ARRANGEMENTS {
            library
                .upsert(&format!("n{index}"), group(&preferences()))
                .unwrap();
        }
        assert_eq!(
            library.upsert("one more", group(&preferences())),
            Err(LibraryError::Full)
        );
        assert_eq!(
            library.upsert("", group(&preferences())),
            Err(LibraryError::InvalidName)
        );
        let directory = directory();
        let path = directory.join(crate::sharing::ARRANGEMENTS_FILE);
        std::fs::write(&path, b"{corrupt").unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        std::fs::write(&path, br#"{"version":3,"arrangements":{}}"#).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        std::fs::write(&path, br#"{"arrangements":[]}"#).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        // A damaged version 2 file is an error too, never a silent reset.
        std::fs::remove_file(&path).unwrap();
        let old_path = directory.join(V2_LIBRARY_FILE);
        let mut damaged: serde_json::Value = serde_json::from_str(V2_LIBRARY).unwrap();
        damaged["arrangements"][0]["setup"]["layout"]["control"] = serde_json::json!({});
        std::fs::write(&old_path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }
}
