//! Screen dimming: the overlay darkness and the chord that toggles it. Both computers agree on the
//! chord so a press forwarded to the other computer dims the screens the user is looking at.

use crate::types::HidUsage;

/// Overlay darkness in percent. Bounded so the screen always stays readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimLevel(u8);

impl DimLevel {
    pub const MIN: u8 = 10;
    pub const MAX: u8 = 99;
    pub const DEFAULT: DimLevel = DimLevel(50);

    /// None outside `MIN..=MAX`: a stored or typed value never silently clamps.
    pub const fn new(percent: u8) -> Option<Self> {
        if percent >= Self::MIN && percent <= Self::MAX {
            Some(Self(percent))
        } else {
            None
        }
    }

    pub const fn percent(self) -> u8 {
        self.0
    }

    /// Overlay opacity for a black window covering the screen.
    pub fn alpha(self) -> f64 {
        f64::from(self.0) / 100.0
    }
}

impl Default for DimLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A system-wide key chord: held modifiers plus one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub command: bool,
    pub key: HidUsage,
}

/// Ctrl+Alt+0 on Windows, Control+Option+0 on macOS: the same physical keys on either keyboard.
pub const DIM_TOGGLE: Chord = Chord {
    control: true,
    alt: true,
    shift: false,
    command: false,
    key: HidUsage(0x27),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_is_bounded_and_maps_to_opacity() {
        assert_eq!(DimLevel::new(9), None);
        assert_eq!(DimLevel::new(100), None);
        assert_eq!(DimLevel::new(10).map(DimLevel::percent), Some(10));
        assert_eq!(DimLevel::new(99).map(DimLevel::percent), Some(99));
        assert_eq!(DimLevel::default().percent(), 50);
        assert!((DimLevel::new(35).unwrap().alpha() - 0.35).abs() < f64::EPSILON);
    }

    #[test]
    fn the_toggle_chord_is_control_alt_zero() {
        let chord = std::hint::black_box(DIM_TOGGLE);
        assert!(chord.control && chord.alt);
        assert!(!chord.shift && !chord.command);
        assert!(chord.key.is_valid() && !chord.key.is_modifier());
    }
}
