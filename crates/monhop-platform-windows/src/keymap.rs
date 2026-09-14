//! USB HID keyboard usages mapped to Windows set-1 scan codes.

use monhop_core::HidUsage;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Set1Prefix {
    None,
    E0,
    E1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Set1ScanCode {
    pub make_code: u8,
    pub prefix: Set1Prefix,
}

impl Set1ScanCode {
    pub const fn new(make_code: u8, prefix: Set1Prefix) -> Self {
        Self { make_code, prefix }
    }

    pub const fn is_extended(self) -> bool {
        matches!(self.prefix, Set1Prefix::E0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMapError {
    UnsupportedE1Sequence { make_code: u8 },
    UnsupportedSet1ScanCode { make_code: u8, prefix: Set1Prefix },
    UnsupportedHidUsage,
}

/// Converts a Windows raw-input set-1 make code to a physical USB HID usage.
///
/// `E1` is deliberately unsupported because its Pause/Break byte sequence is not a single
/// physical make code. Callers must handle that sequence at the raw-input boundary.
pub fn hid_usage_from_set1(scan_code: Set1ScanCode) -> Result<HidUsage, KeyMapError> {
    if matches!(scan_code.prefix, Set1Prefix::E1) {
        return Err(KeyMapError::UnsupportedE1Sequence {
            make_code: scan_code.make_code,
        });
    }

    lookup_set1(scan_code, BASE_MAPPINGS)
        .or_else(|| lookup_set1(scan_code, E0_MAPPINGS))
        .ok_or(KeyMapError::UnsupportedSet1ScanCode {
            make_code: scan_code.make_code,
            prefix: scan_code.prefix,
        })
}

/// Converts a canonical USB HID keyboard usage to the set-1 scan code accepted by `SendInput`.
pub fn set1_from_hid_usage(usage: HidUsage) -> Result<Set1ScanCode, KeyMapError> {
    BASE_MAPPINGS
        .iter()
        .chain(E0_MAPPINGS)
        .find_map(|(scan_code, mapped_usage)| (*mapped_usage == usage).then_some(*scan_code))
        .ok_or(KeyMapError::UnsupportedHidUsage)
}

fn lookup_set1(scan_code: Set1ScanCode, mappings: &[(Set1ScanCode, HidUsage)]) -> Option<HidUsage> {
    mappings
        .iter()
        .find_map(|(mapped_scan_code, usage)| (*mapped_scan_code == scan_code).then_some(*usage))
}

const fn base(make_code: u8, usage: u16) -> (Set1ScanCode, HidUsage) {
    (
        Set1ScanCode::new(make_code, Set1Prefix::None),
        HidUsage(usage),
    )
}

const fn e0(make_code: u8, usage: u16) -> (Set1ScanCode, HidUsage) {
    (
        Set1ScanCode::new(make_code, Set1Prefix::E0),
        HidUsage(usage),
    )
}

const BASE_MAPPINGS: &[(Set1ScanCode, HidUsage)] = &[
    base(0x01, 0x29),
    base(0x02, 0x1e),
    base(0x03, 0x1f),
    base(0x04, 0x20),
    base(0x05, 0x21),
    base(0x06, 0x22),
    base(0x07, 0x23),
    base(0x08, 0x24),
    base(0x09, 0x25),
    base(0x0a, 0x26),
    base(0x0b, 0x27),
    base(0x0c, 0x2d),
    base(0x0d, 0x2e),
    base(0x0e, 0x2a),
    base(0x0f, 0x2b),
    base(0x10, 0x14),
    base(0x11, 0x1a),
    base(0x12, 0x08),
    base(0x13, 0x15),
    base(0x14, 0x17),
    base(0x15, 0x1c),
    base(0x16, 0x18),
    base(0x17, 0x0c),
    base(0x18, 0x12),
    base(0x19, 0x13),
    base(0x1a, 0x2f),
    base(0x1b, 0x30),
    base(0x1c, 0x28),
    base(0x1d, 0xe0),
    base(0x1e, 0x04),
    base(0x1f, 0x16),
    base(0x20, 0x07),
    base(0x21, 0x09),
    base(0x22, 0x0a),
    base(0x23, 0x0b),
    base(0x24, 0x0d),
    base(0x25, 0x0e),
    base(0x26, 0x0f),
    base(0x27, 0x33),
    base(0x28, 0x34),
    base(0x29, 0x35),
    base(0x2a, 0xe1),
    base(0x2b, 0x31),
    base(0x2c, 0x1d),
    base(0x2d, 0x1b),
    base(0x2e, 0x06),
    base(0x2f, 0x19),
    base(0x30, 0x05),
    base(0x31, 0x11),
    base(0x32, 0x10),
    base(0x33, 0x36),
    base(0x34, 0x37),
    base(0x35, 0x38),
    base(0x36, 0xe5),
    base(0x37, 0x55),
    base(0x38, 0xe2),
    base(0x39, 0x2c),
    base(0x3a, 0x39),
    base(0x3b, 0x3a),
    base(0x3c, 0x3b),
    base(0x3d, 0x3c),
    base(0x3e, 0x3d),
    base(0x3f, 0x3e),
    base(0x40, 0x3f),
    base(0x41, 0x40),
    base(0x42, 0x41),
    base(0x43, 0x42),
    base(0x44, 0x43),
    base(0x45, 0x53),
    base(0x46, 0x47),
    base(0x47, 0x5f),
    base(0x48, 0x60),
    base(0x49, 0x61),
    base(0x4a, 0x56),
    base(0x4b, 0x5c),
    base(0x4c, 0x5d),
    base(0x4d, 0x5e),
    base(0x4e, 0x57),
    base(0x4f, 0x59),
    base(0x50, 0x5a),
    base(0x51, 0x5b),
    base(0x52, 0x62),
    base(0x53, 0x63),
    base(0x56, 0x64),
    base(0x57, 0x44),
    base(0x58, 0x45),
    base(0x64, 0x68),
    base(0x65, 0x69),
    base(0x66, 0x6a),
    base(0x67, 0x6b),
    base(0x68, 0x6c),
    base(0x69, 0x6d),
    base(0x6a, 0x6e),
    base(0x6b, 0x6f),
    base(0x6c, 0x70),
    base(0x6d, 0x71),
    base(0x6e, 0x72),
    base(0x76, 0x73),
];

const E0_MAPPINGS: &[(Set1ScanCode, HidUsage)] = &[
    e0(0x1c, 0x58),
    e0(0x1d, 0xe4),
    e0(0x35, 0x54),
    e0(0x37, 0x46),
    e0(0x38, 0xe6),
    e0(0x47, 0x4a),
    e0(0x48, 0x52),
    e0(0x49, 0x4b),
    e0(0x4b, 0x50),
    e0(0x4d, 0x4f),
    e0(0x4f, 0x4d),
    e0(0x50, 0x51),
    e0(0x51, 0x4e),
    e0(0x52, 0x49),
    e0(0x53, 0x4c),
    e0(0x5b, 0xe3),
    e0(0x5c, 0xe7),
    e0(0x5d, 0x65),
    e0(0x5e, 0x66),
];
