//! Thread-safe display-label cache populated by the desktop app's main-thread AppKit bridge.

use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

use monhop_core::{DisplayId, LogicalRect, NativeSize};

use crate::MacDisplay;

const MAX_DISPLAY_NAME_BYTES: usize = 96;

#[derive(Clone)]
struct CachedDisplayName {
    native_size: NativeSize,
    logical_bounds: LogicalRect,
    name: String,
}

static DISPLAY_NAMES: OnceLock<Mutex<BTreeMap<DisplayId, CachedDisplayName>>> = OnceLock::new();

fn cache() -> &'static Mutex<BTreeMap<DisplayId, CachedDisplayName>> {
    DISPLAY_NAMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn sanitized_display_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_DISPLAY_NAME_BYTES
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
    {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Replaces labels captured on the main thread. Each label remains valid only for its matching
/// Core Graphics display snapshot, so a changed arrangement falls back to an unnamed display.
pub fn replace_names(displays: &[MacDisplay], names: Vec<(DisplayId, String)>) {
    let mut unique_displays = BTreeMap::<DisplayId, Option<&MacDisplay>>::new();
    for display in displays {
        match unique_displays.entry(display.id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(display));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    let mut unique_names = BTreeMap::<DisplayId, Option<String>>::new();
    for (id, name) in names {
        let Some(name) = sanitized_display_name(&name) else {
            continue;
        };
        match unique_names.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(name));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().as_deref() != Some(name.as_str()) {
                    entry.insert(None);
                }
            }
        }
    }

    let mut next = BTreeMap::new();
    for (id, display) in unique_displays {
        let (Some(display), Some(Some(name))) = (display, unique_names.remove(&id)) else {
            continue;
        };
        next.insert(
            id,
            CachedDisplayName {
                native_size: display.native_size,
                logical_bounds: display.logical_bounds,
                name,
            },
        );
    }

    if let Ok(mut cached) = cache().lock() {
        *cached = next;
    }
}

/// Returns a cached OS label only when the current display has the same identifier and geometry.
pub fn name_for(display: &MacDisplay) -> Option<String> {
    let cached = cache().lock().ok()?;
    let name = cached.get(&display.id)?;
    (name.native_size == display.native_size && name.logical_bounds == display.logical_bounds)
        .then(|| name.name.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use monhop_core::{DisplayId, LogicalRect, LogicalSize, NativeSize, Point};

    use super::{name_for, replace_names, sanitized_display_name};
    use crate::MacDisplay;

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn display(id: u64) -> MacDisplay {
        MacDisplay {
            id: DisplayId(id),
            name: None,
            native_size: NativeSize::new(3_840, 2_160),
            logical_bounds: LogicalRect {
                origin: Point::new(0.0, 0.0),
                size: LogicalSize::new(1_920.0, 1_080.0),
            },
            scale_factor: 2.0,
            refresh_rate_hz: Some(60.0),
            primary: true,
            monitor: None,
        }
    }

    #[test]
    fn cached_name_requires_the_same_display_snapshot() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let source = display(7);
        replace_names(
            std::slice::from_ref(&source),
            vec![(source.id, "Studio Display".into())],
        );

        assert_eq!(name_for(&source), Some("Studio Display".into()));
        let mut changed = source;
        changed.logical_bounds.origin.x = 1.0;
        assert_eq!(name_for(&changed), None);
    }

    #[test]
    fn invalid_or_ambiguous_names_are_not_cached() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let source = display(8);
        replace_names(
            std::slice::from_ref(&source),
            vec![(source.id, "\u{202e}spoof".into())],
        );
        assert_eq!(name_for(&source), None);

        replace_names(
            std::slice::from_ref(&source),
            vec![
                (source.id, "First display name".into()),
                (source.id, "Second display name".into()),
            ],
        );

        assert_eq!(name_for(&source), None);
        assert_eq!(sanitized_display_name("line\nbreak"), None);
        assert_eq!(sanitized_display_name(&"x".repeat(97)), None);
    }

    #[test]
    fn replacing_names_removes_stale_entries() {
        let _guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let first = display(9);
        let second = display(10);
        replace_names(
            std::slice::from_ref(&first),
            vec![(first.id, "Color LCD".into())],
        );
        replace_names(
            std::slice::from_ref(&second),
            vec![(second.id, "External Display".into())],
        );

        assert_eq!(name_for(&first), None);
        assert_eq!(name_for(&second), Some("External Display".into()));
    }
}
