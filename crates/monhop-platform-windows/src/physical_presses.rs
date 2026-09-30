//! Physical key and button state the capture hook has seen, so the injector can tell a key the
//! user is pressing or holding from a release Windows dropped. Counts and flags only.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use monhop_core::{HidUsage, MouseButton};

static KEY_PRESSES: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];
static BUTTON_PRESSES: [AtomicU32; 5] = [const { AtomicU32::new(0) }; 5];
static KEYS_HELD: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static BUTTONS_HELD: [AtomicBool; 5] = [const { AtomicBool::new(false) }; 5];

/// A physical key down (auto-repeat included, which only a finger on the key produces) or up.
pub(crate) fn note_key(usage: HidUsage, pressed: bool) {
    let index = usize::from(usage.0);
    if let (Some(presses), Some(held)) = (KEY_PRESSES.get(index), KEYS_HELD.get(index)) {
        if pressed {
            presses.fetch_add(1, Ordering::Relaxed);
        }
        held.store(pressed, Ordering::Relaxed);
    }
}

pub(crate) fn note_button(button: MouseButton, pressed: bool) {
    if pressed {
        BUTTON_PRESSES[button.index()].fetch_add(1, Ordering::Relaxed);
    }
    BUTTONS_HELD[button.index()].store(pressed, Ordering::Relaxed);
}

/// A new capture has seen nothing yet: a release that happened while none ran leaves no hold.
pub(crate) fn forget_held() {
    for held in KEYS_HELD.iter().chain(BUTTONS_HELD.iter()) {
        held.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn key_presses(usage: HidUsage) -> u32 {
    KEY_PRESSES
        .get(usize::from(usage.0))
        .map_or(0, |count| count.load(Ordering::Relaxed))
}

pub(crate) fn button_presses(button: MouseButton) -> u32 {
    BUTTON_PRESSES[button.index()].load(Ordering::Relaxed)
}

pub(crate) fn key_held(usage: HidUsage) -> bool {
    KEYS_HELD
        .get(usize::from(usage.0))
        .is_some_and(|held| held.load(Ordering::Relaxed))
}

pub(crate) fn button_held(button: MouseButton) -> bool {
    BUTTONS_HELD[button.index()].load(Ordering::Relaxed)
}
