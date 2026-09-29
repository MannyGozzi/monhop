//! Fixed 32-byte header for the clipboard QUIC unidirectional stream (a separate wire from the
//! 8 KiB `Frame` codec, never sent on the ordered control stream). The connection is already
//! authenticated and only locally originated clipboard items are ever sent, so the header carries
//! no sender-origin or content-hash field; the receiver hashes content itself when it needs to.
//!
//! ```text
//! 0..4   magic b"LKMC"
//! 4..6   PROTOCOL_VERSION (u16 BE)
//! 6      clipboard format version = 1
//! 7      kind: 1 State, 2 Text, 3 Png
//! 8..16  epoch (u64 BE), must match the session's negotiated epoch
//! 16..24 sequence (u64 BE), strictly increasing per sender per connection, starts at 1
//! 24..28 payload length (u32 BE), bounded per kind so the receiver can refuse before reading
//! 28..32 reserved, must be zero
//! ```

use std::error::Error;
use std::fmt;

use crate::PROTOCOL_VERSION;

pub const CLIPBOARD_MAGIC: [u8; 4] = *b"LKMC";
const CLIPBOARD_FORMAT_VERSION: u8 = 1;
pub const CLIPBOARD_HEADER_LEN: usize = 32;

pub const MAX_CLIPBOARD_TEXT: u32 = 1024 * 1024;
pub const MAX_CLIPBOARD_PNG: u32 = 32 * 1024 * 1024;
pub const MAX_CLIPBOARD_PIXELS: u64 = 64_000_000;
pub const MAX_CLIPBOARD_SIDE: u32 = 32_768;

const STATE_PAYLOAD_LEN: u32 = 1;
/// Smallest a PNG can be and still carry a full IHDR chunk: 8-byte signature + 4-byte length +
/// 4-byte "IHDR" + 13-byte IHDR data + 4-byte CRC.
const MIN_PNG_PAYLOAD_LEN: u32 = 33;
/// `png_dimensions` never reads past this many bytes: signature, IHDR length, type, data and CRC.
const PNG_PREFIX_LEN: usize = 33;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_IHDR_TYPE: [u8; 4] = *b"IHDR";
const PNG_IHDR_DATA_LEN: u32 = 13;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardKind {
    /// One byte, the sender's switch (0/1); sent on attach and on every toggle.
    State = 1,
    Text = 2,
    Png = 3,
}

impl ClipboardKind {
    const fn from_wire(value: u8) -> Result<Self, ClipboardHeaderError> {
        match value {
            1 => Ok(Self::State),
            2 => Ok(Self::Text),
            3 => Ok(Self::Png),
            _ => Err(ClipboardHeaderError::InvalidKind),
        }
    }

    const fn to_wire(self) -> u8 {
        self as u8
    }

    /// Inclusive payload-length bounds this kind allows on the wire.
    const fn payload_bounds(self) -> (u32, u32) {
        match self {
            Self::State => (STATE_PAYLOAD_LEN, STATE_PAYLOAD_LEN),
            Self::Text => (1, MAX_CLIPBOARD_TEXT),
            Self::Png => (MIN_PNG_PAYLOAD_LEN, MAX_CLIPBOARD_PNG),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardHeader {
    pub kind: ClipboardKind,
    pub epoch: u64,
    pub sequence: u64,
    pub payload_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardHeaderError {
    BadMagic,
    UnsupportedVersion,
    UnsupportedFormatVersion,
    InvalidKind,
    NonZeroReservedField,
    EpochMismatch,
    ZeroSequence,
    InvalidPayloadLength,
}

impl fmt::Display for ClipboardHeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid clipboard stream header")
    }
}

impl Error for ClipboardHeaderError {}

pub fn encode_header(header: &ClipboardHeader) -> [u8; CLIPBOARD_HEADER_LEN] {
    let mut bytes = [0_u8; CLIPBOARD_HEADER_LEN];
    bytes[0..4].copy_from_slice(&CLIPBOARD_MAGIC);
    bytes[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    bytes[6] = CLIPBOARD_FORMAT_VERSION;
    bytes[7] = header.kind.to_wire();
    bytes[8..16].copy_from_slice(&header.epoch.to_be_bytes());
    bytes[16..24].copy_from_slice(&header.sequence.to_be_bytes());
    bytes[24..28].copy_from_slice(&header.payload_len.to_be_bytes());
    bytes
}

/// Validates every field, including the per-kind payload bounds, before the caller allocates a
/// buffer for the body that follows on the stream.
pub fn decode_header(
    bytes: &[u8; CLIPBOARD_HEADER_LEN],
    expected_epoch: u64,
) -> Result<ClipboardHeader, ClipboardHeaderError> {
    if bytes[0..4] != CLIPBOARD_MAGIC {
        return Err(ClipboardHeaderError::BadMagic);
    }
    if u16::from_be_bytes([bytes[4], bytes[5]]) != PROTOCOL_VERSION {
        return Err(ClipboardHeaderError::UnsupportedVersion);
    }
    if bytes[6] != CLIPBOARD_FORMAT_VERSION {
        return Err(ClipboardHeaderError::UnsupportedFormatVersion);
    }
    let kind = ClipboardKind::from_wire(bytes[7])?;
    if bytes[28..32] != [0; 4] {
        return Err(ClipboardHeaderError::NonZeroReservedField);
    }
    let epoch = u64::from_be_bytes(bytes[8..16].try_into().expect("8-byte slice"));
    if epoch != expected_epoch {
        return Err(ClipboardHeaderError::EpochMismatch);
    }
    let sequence = u64::from_be_bytes(bytes[16..24].try_into().expect("8-byte slice"));
    if sequence == 0 {
        return Err(ClipboardHeaderError::ZeroSequence);
    }
    let payload_len = u32::from_be_bytes(bytes[24..28].try_into().expect("4-byte slice"));
    let (min, max) = kind.payload_bounds();
    if !(min..=max).contains(&payload_len) {
        return Err(ClipboardHeaderError::InvalidPayloadLength);
    }
    Ok(ClipboardHeader {
        kind,
        epoch,
        sequence,
        payload_len,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardTextError {
    Empty,
    TooLarge,
    InvalidUtf8,
    ContainsNul,
}

impl fmt::Display for ClipboardTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid clipboard text")
    }
}

impl Error for ClipboardTextError {}

/// Wire text is strict UTF-8, no NUL, non-empty, capped at [`MAX_CLIPBOARD_TEXT`]. Callers
/// normalize CRLF to LF and truncate at the first NUL before this ever sees the bytes; this only
/// verifies the peer honored the same rule.
pub fn validate_text(bytes: &[u8]) -> Result<(), ClipboardTextError> {
    if bytes.is_empty() {
        return Err(ClipboardTextError::Empty);
    }
    if bytes.len() > MAX_CLIPBOARD_TEXT as usize {
        return Err(ClipboardTextError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ClipboardTextError::InvalidUtf8)?;
    if text.contains('\0') {
        return Err(ClipboardTextError::ContainsNul);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardPngError {
    Truncated,
    BadSignature,
    IhdrNotFirst,
    InvalidIhdrLength,
    InvalidDimensions,
    InvalidBitDepthColorType,
    InvalidInterlace,
}

impl fmt::Display for ClipboardPngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid clipboard PNG prefix")
    }
}

impl Error for ClipboardPngError {}

/// Whether an image of these pixel dimensions is within the clipboard caps: each side in
/// `1..=MAX_CLIPBOARD_SIDE` and the area at most [`MAX_CLIPBOARD_PIXELS`].
pub fn image_within_caps(width: u32, height: u32) -> bool {
    let side = 1..=MAX_CLIPBOARD_SIDE;
    side.contains(&width)
        && side.contains(&height)
        && u64::from(width) * u64::from(height) <= MAX_CLIPBOARD_PIXELS
}

/// Reads only the signature and IHDR chunk (the first [`PNG_PREFIX_LEN`] bytes) to recover
/// dimensions before any decode. Never inflates or checks a CRC; the receiver still fully decodes
/// (with checksums) and re-encodes before the bytes reach the OS clipboard.
pub fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), ClipboardPngError> {
    if bytes.len() < PNG_PREFIX_LEN {
        return Err(ClipboardPngError::Truncated);
    }
    let prefix = &bytes[..PNG_PREFIX_LEN];
    if prefix[0..8] != PNG_SIGNATURE {
        return Err(ClipboardPngError::BadSignature);
    }
    if prefix[12..16] != PNG_IHDR_TYPE {
        return Err(ClipboardPngError::IhdrNotFirst);
    }
    let chunk_len = u32::from_be_bytes(prefix[8..12].try_into().expect("4-byte slice"));
    if chunk_len != PNG_IHDR_DATA_LEN {
        return Err(ClipboardPngError::InvalidIhdrLength);
    }
    let width = u32::from_be_bytes(prefix[16..20].try_into().expect("4-byte slice"));
    let height = u32::from_be_bytes(prefix[20..24].try_into().expect("4-byte slice"));
    if !image_within_caps(width, height) {
        return Err(ClipboardPngError::InvalidDimensions);
    }
    let bit_depth = prefix[24];
    let color_type = prefix[25];
    if !valid_bit_depth_color_type(color_type, bit_depth) {
        return Err(ClipboardPngError::InvalidBitDepthColorType);
    }
    let interlace = prefix[28];
    if interlace > 1 {
        return Err(ClipboardPngError::InvalidInterlace);
    }
    Ok((width, height))
}

/// PNG's fixed bit-depth/color-type pairing table (greyscale, truecolor, indexed,
/// greyscale+alpha, truecolor+alpha; color type 1 and 5 do not exist, 7+ is unassigned).
const fn valid_bit_depth_color_type(color_type: u8, bit_depth: u8) -> bool {
    match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    }
}
