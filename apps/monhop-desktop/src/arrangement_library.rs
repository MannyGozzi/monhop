//! Named display arrangements kept on this computer. Each one is a complete setup for one pair
//! of computers; loading one is only offered while both computers show the displays it was made for.

use std::{io, path::Path};

use monhop_transport::session_setup::{DisplayTopology, InspectedPeer};
use serde::{Deserialize, Serialize};

use crate::sharing::LayoutRequest;
use crate::sharing_preferences::{
    DisplaySnapshot, SharingPreferences, fingerprint_key, load_bounded, save_bounded,
};

pub const MAX_ARRANGEMENTS: usize = 32;
pub const MAX_NAME_CHARS: usize = 64;
const LIBRARY_VERSION: u8 = 1;
const MAX_LIBRARY_BYTES: u64 = 512 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrangementLibrary {
    version: u8,
    arrangements: Vec<SavedArrangement>,
}

/// An absent file's library, at the current version rather than a derived all-zero default;
/// `load_bounded` returns this when there is nothing on disk to parse.
impl Default for ArrangementLibrary {
    fn default() -> Self {
        Self {
            version: LIBRARY_VERSION,
            arrangements: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedArrangement {
    name: String,
    setup: SharingPreferences,
    /// Remembered by MonHop at Apply instead of named by the user; absent in earlier files.
    #[serde(default)]
    automatic: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementView {
    pub name: String,
    pub source_side: &'static str,
    pub mode: String,
    pub crossings: usize,
    /// Present only when it fits the displays both computers show right now.
    pub layout: Option<LayoutRequest>,
    /// Remembered by MonHop at Apply instead of named by the user.
    pub automatic: bool,
    /// Loadable right now: this computer is connected to that one and both show the displays
    /// the entry was made with. Always false while the computer is not connected.
    pub fits: bool,
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
                "Delete an arrangement first. This computer keeps at most 32 for a pair of computers."
            }
            Self::Unknown => "That arrangement is no longer saved.",
        }
    }
}

impl ArrangementLibrary {
    /// An absent file is an empty library; a damaged one is an error rather than a silent reset.
    pub fn load(path: &Path) -> io::Result<Self> {
        load_bounded(path, MAX_LIBRARY_BYTES, |library: &Self| {
            if library.version != LIBRARY_VERSION
                || library.arrangements.len() > MAX_ARRANGEMENTS
                || library.arrangements.iter().any(|entry| {
                    normalize_name(&entry.name).as_deref() != Some(entry.name.as_str())
                })
                || library
                    .arrangements
                    .iter()
                    .any(|entry| entry.setup.validate().is_err())
            {
                return Err(invalid());
            }
            Ok(())
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|_| invalid())?;
        save_bounded(path, &bytes, MAX_LIBRARY_BYTES)
    }

    /// Saving under an existing name replaces that arrangement in place; names are per pair of
    /// computers. A name the user gives is never a remembered entry afterwards.
    pub fn upsert(&mut self, name: &str, setup: SharingPreferences) -> Result<(), LibraryError> {
        let name = normalize_name(name).ok_or(LibraryError::InvalidName)?;
        if let Some(existing) = self.position(&name, |entry| entry.setup.same_pair_as(&setup)) {
            self.arrangements[existing].setup = setup;
            self.arrangements[existing].automatic = false;
            return Ok(());
        }
        if self.kept_for_pair(&setup) >= MAX_ARRANGEMENTS {
            return Err(LibraryError::Full);
        }
        self.arrangements.push(SavedArrangement {
            name,
            setup,
            automatic: false,
        });
        Ok(())
    }

    /// Remembers an applied setup for the displays it was made with: one remembered entry per
    /// set of displays, newest last, never over a name the user gave. `setup` must be valid.
    pub fn remember_automatically(
        &mut self,
        setup: SharingPreferences,
    ) -> Result<bool, LibraryError> {
        if self.arrangements.iter().any(|entry| {
            entry.automatic
                && entry.setup.same_pair_as(&setup)
                && entry.setup.same_display_sets(&setup)
                && entry.setup == setup
        }) {
            return Ok(false);
        }
        self.arrangements.retain(|entry| {
            !(entry.automatic
                && entry.setup.same_pair_as(&setup)
                && entry.setup.same_display_sets(&setup))
        });
        if self.kept_for_pair(&setup) >= MAX_ARRANGEMENTS {
            let oldest = self
                .arrangements
                .iter()
                .position(|entry| entry.automatic && entry.setup.same_pair_as(&setup))
                .ok_or(LibraryError::Full)?;
            self.arrangements.remove(oldest);
        }
        let name = self
            .remembered_name(&setup)
            .ok_or(LibraryError::InvalidName)?;
        self.arrangements.push(SavedArrangement {
            name,
            setup,
            automatic: true,
        });
        Ok(true)
    }

    /// The most recently remembered entry made with the displays both computers show right now,
    /// its ids rewritten to theirs: a reconnected monitor carries a new OS id but the same identity.
    pub fn automatic_fit(&self, inspection: &InspectedPeer) -> Option<SharingPreferences> {
        self.arrangements
            .iter()
            .rev()
            .filter(|entry| entry.automatic)
            .find_map(|entry| entry.setup.remap_to_inspection(inspection))
    }

    /// The most recently remembered entry made with exactly the monitors both computers show
    /// now, whatever their geometry: what a changed arrangement is rebuilt from, so a display
    /// that only moved keeps every crossing that was made for it.
    pub fn automatic_for_same_displays(
        &self,
        inspection: &InspectedPeer,
    ) -> Option<&SharingPreferences> {
        self.arrangements
            .iter()
            .rev()
            .find(|entry| {
                entry.automatic
                    && entry.setup.validate().is_ok()
                    && entry.setup.same_displays_as_inspection(inspection)
            })
            .map(|entry| &entry.setup)
    }

    /// The most recently remembered entry for the same pair made with this computer's displays
    /// as they are now, its local ids rewritten to theirs; the other computer's displays are
    /// checked when the session connects.
    pub fn automatic_for_local(
        &self,
        pair: &SharingPreferences,
        current: &DisplayTopology,
    ) -> Option<SharingPreferences> {
        self.arrangements
            .iter()
            .rev()
            .filter(|entry| entry.automatic && entry.setup.same_pair_as(pair))
            .find_map(|entry| entry.setup.remap_to_local_displays(current))
    }

    fn kept_for_pair(&self, setup: &SharingPreferences) -> usize {
        self.arrangements
            .iter()
            .filter(|entry| entry.setup.same_pair_as(setup))
            .count()
    }

    /// The display names of both computers, trimmed to fit, then numbered when the pair
    /// already holds that name.
    fn remembered_name(&self, setup: &SharingPreferences) -> Option<String> {
        let side = |displays: &[DisplaySnapshot]| {
            displays
                .iter()
                .map(|display| readable(display.name()))
                .collect::<Vec<_>>()
                .join(" + ")
        };
        let base = format!(
            "{} \u{2194} {}",
            side(setup.local_displays()),
            side(setup.peer_displays())
        );
        let taken = |name: &str| {
            self.arrangements
                .iter()
                .any(|entry| entry.name == name && entry.setup.same_pair_as(setup))
        };
        for attempt in 1..=MAX_ARRANGEMENTS + 1 {
            let suffix = if attempt == 1 {
                String::new()
            } else {
                format!(" {attempt}")
            };
            let room = MAX_NAME_CHARS - suffix.chars().count();
            let candidate = normalize_name(&format!("{}{suffix}", shortened(&base, room)))?;
            if !taken(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Forgets one of a paired computer's entries, connected or not. This library is this
    /// computer's own, so the peer's identity alone says which entries belong to that computer.
    pub fn remove_for(&mut self, peer: &str, name: &str) -> Result<(), LibraryError> {
        let name = normalize_name(name).ok_or(LibraryError::InvalidName)?;
        let index = self
            .position(&name, |entry| same_peer_key(&entry.setup, peer))
            .ok_or(LibraryError::Unknown)?;
        self.arrangements.remove(index);
        Ok(())
    }

    /// The arrangements made for these two computers, each with its layout when it fits now.
    pub fn views(&self, inspection: &InspectedPeer) -> Vec<ArrangementView> {
        self.views_for(&inspection.peer_fingerprint.full_hex(), Some(inspection))
    }

    /// Every arrangement kept for one paired computer, listable while it is not connected.
    /// `inspection` is the live link's displays; an entry is only marked as fitting when that
    /// inspection is with that same computer, so one computer's displays never vouch for another's.
    pub fn views_for(
        &self,
        peer: &str,
        inspection: Option<&InspectedPeer>,
    ) -> Vec<ArrangementView> {
        let inspection = inspection.filter(|inspection| {
            fingerprint_key(&inspection.peer_fingerprint.full_hex()) == fingerprint_key(peer)
        });
        self.arrangements
            .iter()
            .filter(|entry| same_peer_key(&entry.setup, peer))
            .map(|entry| {
                let fitted = inspection
                    .and_then(|inspection| entry.setup.remap_to_inspection(inspection))
                    .map(|fitted| fitted.layout().clone());
                ArrangementView {
                    name: entry.name.clone(),
                    source_side: entry.setup.source_side(),
                    mode: entry
                        .setup
                        .layout()
                        .arrangement
                        .as_ref()
                        .map_or("grouped", |arrangement| arrangement.mode.as_str())
                        .to_owned(),
                    crossings: entry.setup.layout().links.len() / 2,
                    fits: fitted.is_some(),
                    layout: fitted,
                    automatic: entry.automatic,
                }
            })
            .collect()
    }

    fn position(&self, name: &str, pair: impl Fn(&SavedArrangement) -> bool) -> Option<usize> {
        self.arrangements
            .iter()
            .position(|entry| entry.name == name && pair(entry))
    }
}

/// Identities reach here lowercase from the window and uppercase from a saved record, so one
/// form decides which computer an entry was made with.
fn same_peer_key(setup: &SharingPreferences, peer: &str) -> bool {
    fingerprint_key(setup.peer_fingerprint()) == fingerprint_key(peer)
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
    use crate::sharing_preferences::tests::{
        inspection, preferences, preferences_for_peer, two_display_record,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    /// The paired computer each fixture record was made with.
    fn peer_key(letter: char) -> String {
        letter.to_string().repeat(64)
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
        let path = directory.join("arrangements.json");
        let mut library = ArrangementLibrary::load(&path).unwrap();
        assert!(library.arrangements.is_empty());
        library.upsert("Desk", preferences()).unwrap();
        let mut replaced = preferences();
        replaced.set_interface_id_for_test("en1:7:192.168.1.5");
        library.upsert(" Desk ", replaced).unwrap();
        library.upsert("Couch", preferences()).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        assert_eq!(
            library.arrangements[0].setup.interface_id(),
            "en1:7:192.168.1.5"
        );
        library.save(&path).unwrap();
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
            loaded.remove_for(&peer_key('B'), "Nope"),
            Err(LibraryError::Unknown)
        );
        loaded.remove_for(&peer_key('B'), "Desk").unwrap();
        assert_eq!(loaded.arrangements.len(), 1);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn names_limits_and_deletion_are_scoped_to_one_pair_of_computers() {
        let mut library = ArrangementLibrary::default();
        let other = preferences_for_peer('C');
        library.upsert("Desk", preferences()).unwrap();
        library.upsert("Desk", other.clone()).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        for index in 0..MAX_ARRANGEMENTS - 1 {
            library.upsert(&format!("n{index}"), other.clone()).unwrap();
        }
        assert_eq!(
            library.upsert("one more", other.clone()),
            Err(LibraryError::Full)
        );
        library.upsert("Couch", preferences()).unwrap();
        let first = inspection(&preferences());
        let second = inspection(&other);
        assert_eq!(library.views(&first).len(), 2);
        assert_eq!(library.views(&second).len(), MAX_ARRANGEMENTS);
        assert_eq!(
            library.remove_for(&peer_key('C'), "Couch"),
            Err(LibraryError::Unknown)
        );
        library.remove_for(&peer_key('C'), "Desk").unwrap();
        assert_eq!(library.views(&first).len(), 2);
        assert_eq!(library.views(&second).len(), MAX_ARRANGEMENTS - 1);
    }

    #[test]
    fn a_computers_arrangements_list_and_forget_while_it_is_not_connected() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", preferences()).unwrap();
        library.remember_automatically(preferences()).unwrap();
        library
            .upsert("Elsewhere", preferences_for_peer('C'))
            .unwrap();
        let listed = library.views_for(&peer_key('B'), None);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Desk");
        // Nothing fits while nothing is connected, so nothing offers a layout to load.
        assert!(listed.iter().all(|entry| !entry.fits));
        assert!(listed.iter().all(|entry| entry.layout.is_none()));
        // The exact names the window reads, so a change here is a change the window sees.
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(json["name"], "Desk");
        assert_eq!(json["sourceSide"], "peer");
        assert_eq!(json["mode"], "grouped");
        assert_eq!(json["crossings"], 1);
        assert_eq!(json["automatic"], false);
        assert_eq!(json["fits"], false);
        assert!(json["layout"].is_null());
        // The same list with that computer's own live displays marks what fits and carries it.
        let live = inspection(&preferences());
        let connected = library.views_for(&peer_key('B'), Some(&live));
        assert!(connected.iter().all(|entry| entry.fits));
        assert_eq!(connected[0].layout.as_ref(), Some(preferences().layout()));
        // Another computer's displays never vouch for this one's entries.
        let other = inspection(&preferences_for_peer('C'));
        assert!(
            library
                .views_for(&peer_key('B'), Some(&other))
                .iter()
                .all(|entry| !entry.fits)
        );
        // Identities arrive lowercase from the window and uppercase from a record; both answer.
        assert_eq!(library.views_for(&peer_key('b'), None).len(), 2);
        library.remove_for(&peer_key('b'), "Desk").unwrap();
        assert_eq!(library.views_for(&peer_key('B'), None).len(), 1);
        // A forget never reaches another computer's entries.
        assert_eq!(
            library.remove_for(&peer_key('B'), "Elsewhere"),
            Err(LibraryError::Unknown)
        );
        assert_eq!(library.views_for(&peer_key('C'), None).len(), 1);
    }

    #[test]
    fn a_saved_layout_fits_whichever_side_holds_the_input_now() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", preferences()).unwrap();
        let mut switched = inspection(&preferences());
        switched.source = switched.local_device;
        let listed = library.views(&switched);
        assert_eq!(listed[0].source_side, "peer");
        assert_eq!(listed[0].layout.as_ref(), Some(preferences().layout()));
    }

    #[test]
    fn remembering_replaces_the_entry_for_the_same_displays_and_leaves_names_alone() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", preferences()).unwrap();
        library.remember_automatically(preferences()).unwrap();
        let mut moved = preferences();
        moved.set_interface_id_for_test("en1:7:192.168.1.5");
        library.remember_automatically(moved).unwrap();
        assert_eq!(library.arrangements.len(), 2);
        assert_eq!(library.arrangements[0].name, "Desk");
        assert!(!library.arrangements[0].automatic);
        assert!(library.arrangements[1].automatic);
        assert_eq!(
            library.arrangements[1].setup.interface_id(),
            "en1:7:192.168.1.5"
        );
        let mut other = preferences();
        other.set_local_displays_for_test(&["1", "3"]);
        library.remember_automatically(other).unwrap();
        assert_eq!(library.arrangements.len(), 3);
        let pair = inspection(&preferences());
        let listed = library.views(&pair);
        assert!(!listed[0].automatic);
        assert!(listed[1].automatic);
        // Saving over a remembered name makes it the user's own arrangement.
        let remembered = library.arrangements[1].name.clone();
        library.upsert(&remembered, preferences()).unwrap();
        assert_eq!(library.arrangements.len(), 3);
        assert!(!library.arrangements[1].automatic);
    }

    #[test]
    fn remembering_the_same_record_again_changes_nothing() {
        let mut library = ArrangementLibrary::default();
        assert_eq!(library.remember_automatically(preferences()), Ok(true));
        assert_eq!(library.remember_automatically(preferences()), Ok(false));
        assert_eq!(library.arrangements.len(), 1);
        let mut moved = preferences();
        moved.set_interface_id_for_test("en1:7:192.168.1.5");
        assert_eq!(library.remember_automatically(moved), Ok(true));
        assert_eq!(library.arrangements.len(), 1);
    }

    #[test]
    fn the_cap_evicts_the_oldest_remembered_entry_and_skips_a_library_of_names() {
        let mut library = ArrangementLibrary::default();
        for index in 0..MAX_ARRANGEMENTS - 1 {
            library.upsert(&format!("n{index}"), preferences()).unwrap();
        }
        let mut first = preferences();
        first.set_local_displays_for_test(&["1"]);
        first.set_local_display_names_for_test("First");
        library.remember_automatically(first).unwrap();
        assert_eq!(library.arrangements.len(), MAX_ARRANGEMENTS);
        let mut second = preferences();
        second.set_local_displays_for_test(&["1", "3"]);
        second.set_local_display_names_for_test("Second");
        library.remember_automatically(second).unwrap();
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
            named.upsert(&format!("n{index}"), preferences()).unwrap();
        }
        assert_eq!(
            named.remember_automatically(preferences()),
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
        library.remember_automatically(one).unwrap();
        library.remember_automatically(two).unwrap();
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
    }

    #[test]
    fn a_remembered_arrangement_is_found_by_the_connected_or_the_local_displays() {
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", preferences()).unwrap();
        let pair = inspection(&preferences());
        assert_eq!(library.automatic_fit(&pair), None);
        library.remember_automatically(preferences()).unwrap();
        assert_eq!(library.automatic_fit(&pair), Some(preferences()));
        let mut other = preferences();
        other.set_local_displays_for_test(&["1", "3"]);
        library.remember_automatically(other.clone()).unwrap();
        // The entry that fits is chosen, not simply the newest.
        assert_eq!(library.automatic_fit(&pair), Some(preferences()));
        let changed = inspection(&other);
        assert_eq!(library.automatic_fit(&changed), Some(other.clone()));
        assert_eq!(
            library.automatic_for_local(&preferences(), &changed.local_displays),
            Some(other)
        );
        assert_eq!(
            library.automatic_for_local(&preferences_for_peer('C'), &changed.local_displays),
            None
        );
    }

    #[test]
    fn a_reconnected_monitor_is_the_same_remembered_displays_under_its_new_id() {
        let record = two_display_record();
        let mut reconnected = record.clone();
        reconnected.set_local_display_id_for_test("3", "7");
        let live = inspection(&reconnected);
        // The memory made before the reconnect is found and comes back naming the new id.
        let mut library = ArrangementLibrary::default();
        library.remember_automatically(record.clone()).unwrap();
        let fitted = library
            .automatic_fit(&live)
            .expect("the remembered monitors fit under their new id");
        assert!(fitted.fits_displays(&live));
        assert_eq!(fitted.layout().links[0].to_display, "7");
        assert_eq!(library.automatic_for_same_displays(&live), Some(&record));
        assert_eq!(
            library
                .automatic_for_local(&record, &live.local_displays)
                .map(|entry| entry.layout().clone()),
            Some(fitted.layout().clone())
        );
        // Remembering the refreshed record replaces the memory instead of adding one.
        assert_eq!(library.remember_automatically(fitted.clone()), Ok(true));
        assert_eq!(library.arrangements.len(), 1);
        assert_eq!(library.automatic_fit(&live), Some(fitted.clone()));
        // Moved as well, the memory no longer fits exactly but is still the one to rebuild from.
        let mut moved = reconnected.clone();
        moved.move_local_display_for_test("7", [1920.0, 0.0]);
        let moved = inspection(&moved);
        assert_eq!(library.automatic_fit(&moved), None);
        assert_eq!(library.automatic_for_same_displays(&moved), Some(&fitted));
        // A memory made with another pair of computers is never offered.
        let mut other_pair = ArrangementLibrary::default();
        let mut other = record.clone();
        other.set_peer_fingerprint_for_test('C');
        other_pair.remember_automatically(other).unwrap();
        assert_eq!(other_pair.automatic_fit(&live), None);
        assert_eq!(other_pair.automatic_for_same_displays(&live), None);
    }

    #[test]
    fn a_named_arrangement_is_offered_with_the_ids_the_monitors_carry_now() {
        let record = two_display_record();
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", record.clone()).unwrap();
        let mut reconnected = record.clone();
        reconnected.set_local_display_id_for_test("3", "7");
        let listed = library.views(&inspection(&reconnected));
        let layout = listed[0].layout.as_ref().expect("the arrangement fits");
        assert_eq!(layout.links[0].to_display, "7");
        assert_eq!(layout.links[1].from_display, "7");
        // Named entries are never promoted on their own.
        assert_eq!(library.automatic_fit(&inspection(&reconnected)), None);
    }

    #[test]
    fn a_library_written_before_remembering_loads_as_named_arrangements() {
        let directory = directory();
        let path = directory.join("arrangements.json");
        let mut library = ArrangementLibrary::default();
        library.remember_automatically(preferences()).unwrap();
        let written = serde_json::to_string(&library).unwrap();
        assert!(written.contains("\"automatic\":true"));
        std::fs::write(&path, written.replace(",\"automatic\":true", "")).unwrap();
        let loaded = ArrangementLibrary::load(&path).unwrap();
        assert_eq!(loaded.arrangements.len(), 1);
        assert!(!loaded.arrangements[0].automatic);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn the_library_is_bounded_and_rejects_damaged_files() {
        let mut library = ArrangementLibrary::default();
        for index in 0..MAX_ARRANGEMENTS {
            library.upsert(&format!("n{index}"), preferences()).unwrap();
        }
        assert_eq!(
            library.upsert("one more", preferences()),
            Err(LibraryError::Full)
        );
        assert_eq!(
            library.upsert("", preferences()),
            Err(LibraryError::InvalidName)
        );
        let directory = directory();
        let path = directory.join("arrangements.json");
        std::fs::write(&path, b"{corrupt").unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        std::fs::write(&path, br#"{"version":2,"arrangements":[]}"#).unwrap();
        assert!(ArrangementLibrary::load(&path).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }
}
