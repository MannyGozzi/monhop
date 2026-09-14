//! Shared platform-neutral identifiers. Input values must never be written to operational logs.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayId(pub u64);

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HidUsage(pub u16);

impl HidUsage {
    pub const fn is_valid(self) -> bool {
        matches!(self.0, 0x04..=0xA4 | 0xE0..=0xE7)
    }

    pub const fn is_modifier(self) -> bool {
        matches!(self.0, 0xE0..=0xE7)
    }
}

impl std::fmt::Debug for HidUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HidUsage([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    /// The canonical slot order behind every fixed-size button array. It is not the wire encoding.
    pub const ALL: [Self; 5] = [
        Self::Left,
        Self::Right,
        Self::Middle,
        Self::Back,
        Self::Forward,
    ];

    pub const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::Back => 3,
            Self::Forward => 4,
        }
    }

    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Middle),
            3 => Some(Self::Back),
            4 => Some(Self::Forward),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState(pub u8);

impl ModifierState {
    pub const LEFT_CONTROL: u8 = 1;
    pub const LEFT_SHIFT: u8 = 2;
    pub const LEFT_ALT: u8 = 4;
    pub const LEFT_META: u8 = 8;
    pub const RIGHT_CONTROL: u8 = 16;
    pub const RIGHT_SHIFT: u8 = 32;
    pub const RIGHT_ALT: u8 = 64;
    pub const RIGHT_META: u8 = 128;

    pub const fn contains(self, mask: u8) -> bool {
        self.0 & mask == mask
    }
}

impl std::fmt::Debug for ModifierState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModifierState([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// The physical monitor behind a display, from its EDID. Two computers cabled to the same monitor
/// report the same identity, so the arrangement can show that monitor once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MonitorIdentity {
    /// EDID manufacturer id, the packed big-endian three-letter code.
    pub vendor: u16,
    /// EDID product code.
    pub product: u16,
    /// EDID serial number; zero when the monitor reports none.
    pub serial: u32,
}

impl MonitorIdentity {
    pub const fn new(vendor: u16, product: u16, serial: u32) -> Option<Self> {
        if vendor == 0 || product == 0 {
            None
        } else {
            Some(Self {
                vendor,
                product,
                serial,
            })
        }
    }
}
