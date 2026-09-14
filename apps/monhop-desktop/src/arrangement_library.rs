//! Named display arrangements kept on this computer. Each one is a complete setup for one pair
//! of computers; loading one is only offered while both computers show the displays it was made for.

use std::{io, path::Path};

use monhop_transport::session_setup::{DisplayTopology, InspectedPeer};
use serde::{Deserialize, Serialize};

use crate::sharing::LayoutRequest;
use crate::sharing_preferences::{DisplaySnapshot, SharingPreferences, load_bounded, save_bounded};

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

    /// The most recently remembered entry that fits both computers' displays right now.
    pub fn automatic_fit(&self, inspection: &InspectedPeer) -> Option<&SharingPreferences> {
        self.arrangements
            .iter()
            .rev()
            .find(|entry| entry.automatic && entry.setup.fits_displays(inspection))
            .map(|entry| &entry.setup)
    }

    /// The most recently remembered entry for the same pair made with this computer's displays
    /// as they are now; the other computer's displays are checked when the session connects.
    pub fn automatic_for_local(
        &self,
        pair: &SharingPreferences,
        current: &DisplayTopology,
    ) -> Option<&SharingPreferences> {
        self.arrangements
            .iter()
            .rev()
            .find(|entry| {
                entry.automatic
                    && entry.setup.same_pair_as(pair)
                    && entry.setup.validate().is_ok()
                    && entry.setup.matches_local_displays(current)
            })
            .map(|entry| &entry.setup)
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

    pub fn remove(&mut self, name: &str, inspection: &InspectedPeer) -> Result<(), LibraryError> {
        let name = normalize_name(name).ok_or(LibraryError::InvalidName)?;
        let index = self
            .position(&name, |entry| entry.setup.same_pair(inspection))
            .ok_or(LibraryError::Unknown)?;
        self.arrangements.remove(index);
        Ok(())
    }

    /// The arrangements made for these two computers, each with its layout when it fits now.
    pub fn views(&self, inspection: &InspectedPeer) -> Vec<ArrangementView> {
        self.arrangements
            .iter()
            .filter(|entry| entry.setup.same_pair(inspection))
            .map(|entry| ArrangementView {
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
                layout: entry
                    .setup
                    .fits_displays(inspection)
                    .then(|| entry.setup.layout().clone()),
                automatic: entry.automatic,
            })
            .collect()
    }

    fn position(&self, name: &str, pair: impl Fn(&SavedArrangement) -> bool) -> Option<usize> {
        self.arrangements
            .iter()
            .position(|entry| entry.name == name && pair(entry))
    }
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
    use crate::sharing_preferences::tests::{inspection, preferences, preferences_for_peer};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

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
        let pair = inspection(&preferences());
        assert_eq!(loaded.remove("Nope", &pair), Err(LibraryError::Unknown));
        loaded.remove("Desk", &pair).unwrap();
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
        assert_eq!(library.remove("Couch", &second), Err(LibraryError::Unknown));
        library.remove("Desk", &second).unwrap();
        assert_eq!(library.views(&first).len(), 2);
        assert_eq!(library.views(&second).len(), MAX_ARRANGEMENTS - 1);
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
        assert_eq!(library.automatic_fit(&pair), Some(&preferences()));
        let mut other = preferences();
        other.set_local_displays_for_test(&["1", "3"]);
        library.remember_automatically(other.clone()).unwrap();
        // The entry that fits is chosen, not simply the newest.
        assert_eq!(library.automatic_fit(&pair), Some(&preferences()));
        let changed = inspection(&other);
        assert_eq!(library.automatic_fit(&changed), Some(&other));
        assert_eq!(
            library.automatic_for_local(&preferences(), &changed.local_displays),
            Some(&other)
        );
        assert_eq!(
            library.automatic_for_local(&preferences_for_peer('C'), &changed.local_displays),
            None
        );
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
