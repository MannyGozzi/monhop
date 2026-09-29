use monhop_protocol::PROTOCOL_VERSION;
use monhop_protocol::clipboard::{
    CLIPBOARD_HEADER_LEN, ClipboardHeader, ClipboardHeaderError, ClipboardKind, ClipboardPngError,
    ClipboardTextError, MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_SIDE, MAX_CLIPBOARD_TEXT, decode_header,
    encode_header, png_dimensions, validate_text,
};

fn header(kind: ClipboardKind, epoch: u64, sequence: u64, payload_len: u32) -> ClipboardHeader {
    ClipboardHeader {
        kind,
        epoch,
        sequence,
        payload_len,
    }
}

#[test]
fn round_trips_a_header_for_every_kind() {
    for (kind, payload_len) in [
        (ClipboardKind::State, 1),
        (ClipboardKind::Text, 1_024),
        (ClipboardKind::Png, 33),
    ] {
        let source = header(kind, 7, 1, payload_len);
        let encoded = encode_header(&source);
        assert_eq!(encoded.len(), CLIPBOARD_HEADER_LEN);
        assert_eq!(decode_header(&encoded, 7), Ok(source));
    }
}

#[test]
fn rejects_a_bad_magic() {
    let mut encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
    encoded[0] = b'X';
    assert_eq!(
        decode_header(&encoded, 1),
        Err(ClipboardHeaderError::BadMagic)
    );
}

#[test]
fn rejects_another_protocol_version() {
    let mut encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
    encoded[4..6].copy_from_slice(&(PROTOCOL_VERSION - 1).to_be_bytes());
    assert_eq!(
        decode_header(&encoded, 1),
        Err(ClipboardHeaderError::UnsupportedVersion)
    );
}

#[test]
fn rejects_a_format_version_other_than_one() {
    for format_version in [0_u8, 2] {
        let mut encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
        encoded[6] = format_version;
        assert_eq!(
            decode_header(&encoded, 1),
            Err(ClipboardHeaderError::UnsupportedFormatVersion)
        );
    }
}

#[test]
fn rejects_unknown_kinds() {
    for raw_kind in [0_u8, 4, 255] {
        let mut encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
        encoded[7] = raw_kind;
        assert_eq!(
            decode_header(&encoded, 1),
            Err(ClipboardHeaderError::InvalidKind)
        );
    }
}

#[test]
fn rejects_a_non_zero_reserved_field() {
    let mut encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
    encoded[31] = 1;
    assert_eq!(
        decode_header(&encoded, 1),
        Err(ClipboardHeaderError::NonZeroReservedField)
    );
}

#[test]
fn rejects_an_epoch_that_does_not_match_the_session() {
    let encoded = encode_header(&header(ClipboardKind::State, 1, 1, 1));
    assert_eq!(
        decode_header(&encoded, 2),
        Err(ClipboardHeaderError::EpochMismatch)
    );
}

#[test]
fn rejects_a_zero_sequence() {
    let encoded = encode_header(&header(ClipboardKind::State, 1, 0, 1));
    assert_eq!(
        decode_header(&encoded, 1),
        Err(ClipboardHeaderError::ZeroSequence)
    );
}

#[test]
fn rejects_a_zero_or_oversize_length_per_kind() {
    let cases = [
        (ClipboardKind::State, 0_u32),
        (ClipboardKind::State, 2),
        (ClipboardKind::Text, 0),
        (ClipboardKind::Text, MAX_CLIPBOARD_TEXT + 1),
        (ClipboardKind::Png, 32),
        (ClipboardKind::Png, MAX_CLIPBOARD_PNG + 1),
    ];
    for (kind, payload_len) in cases {
        let encoded = encode_header(&header(kind, 1, 1, payload_len));
        assert_eq!(
            decode_header(&encoded, 1),
            Err(ClipboardHeaderError::InvalidPayloadLength)
        );
    }
}

#[test]
fn state_length_must_be_exactly_one() {
    let encoded = encode_header(&header(ClipboardKind::State, 1, 1, 2));
    assert_eq!(
        decode_header(&encoded, 1),
        Err(ClipboardHeaderError::InvalidPayloadLength)
    );
}

#[test]
fn text_rejects_invalid_utf8() {
    assert_eq!(
        validate_text(&[0xFF, 0xFE]),
        Err(ClipboardTextError::InvalidUtf8)
    );
}

#[test]
fn text_rejects_a_nul_byte() {
    assert_eq!(
        validate_text(b"before\0after"),
        Err(ClipboardTextError::ContainsNul)
    );
}

#[test]
fn text_rejects_oversize_payloads() {
    let oversize = vec![b'a'; MAX_CLIPBOARD_TEXT as usize + 1];
    assert_eq!(validate_text(&oversize), Err(ClipboardTextError::TooLarge));
}

#[test]
fn text_rejects_empty_payloads() {
    assert_eq!(validate_text(&[]), Err(ClipboardTextError::Empty));
}

#[test]
fn text_accepts_a_normal_string() {
    assert_eq!(validate_text("hello, MonHop".as_bytes()), Ok(()));
}

/// Builds a minimal PNG prefix: signature, one chunk header/type and a 13-byte IHDR body, padded
/// to the 33-byte prefix `png_dimensions` reads. The trailing 4 bytes stand in for a CRC that the
/// header pre-check never verifies.
fn png_prefix(
    chunk_type: &[u8; 4],
    chunk_len: u32,
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(33);
    bytes.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.extend_from_slice(&chunk_len.to_be_bytes());
    bytes.extend_from_slice(chunk_type);
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.push(bit_depth);
    bytes.push(color_type);
    bytes.push(0); // compression method
    bytes.push(0); // filter method
    bytes.push(interlace);
    bytes.extend_from_slice(&[0; 4]); // unchecked CRC placeholder
    bytes
}

fn valid_png(width: u32, height: u32) -> Vec<u8> {
    png_prefix(b"IHDR", 13, width, height, 8, 2, 0)
}

#[test]
fn png_accepts_a_one_by_one_image() {
    assert_eq!(png_dimensions(&valid_png(1, 1)), Ok((1, 1)));
}

#[test]
fn png_ignores_bytes_past_the_ihdr_prefix() {
    let mut bytes = valid_png(4, 5);
    bytes.extend_from_slice(&[0xAB; 64]);
    assert_eq!(png_dimensions(&bytes), Ok((4, 5)));
}

#[test]
fn png_rejects_a_truncated_prefix() {
    let bytes = valid_png(1, 1);
    assert_eq!(
        png_dimensions(&bytes[..32]),
        Err(ClipboardPngError::Truncated)
    );
}

#[test]
fn png_rejects_a_bad_signature() {
    let mut bytes = valid_png(1, 1);
    bytes[0] = 0;
    assert_eq!(png_dimensions(&bytes), Err(ClipboardPngError::BadSignature));
}

#[test]
fn png_rejects_a_first_chunk_that_is_not_ihdr() {
    let bytes = png_prefix(b"IDAT", 13, 1, 1, 8, 2, 0);
    assert_eq!(png_dimensions(&bytes), Err(ClipboardPngError::IhdrNotFirst));
}

#[test]
fn png_rejects_an_ihdr_length_other_than_thirteen() {
    let bytes = png_prefix(b"IHDR", 14, 1, 1, 8, 2, 0);
    assert_eq!(
        png_dimensions(&bytes),
        Err(ClipboardPngError::InvalidIhdrLength)
    );
}

#[test]
fn png_rejects_a_zero_side() {
    assert_eq!(
        png_dimensions(&valid_png(0, 10)),
        Err(ClipboardPngError::InvalidDimensions)
    );
    assert_eq!(
        png_dimensions(&valid_png(10, 0)),
        Err(ClipboardPngError::InvalidDimensions)
    );
}

#[test]
fn png_rejects_an_oversize_side() {
    assert_eq!(
        png_dimensions(&valid_png(MAX_CLIPBOARD_SIDE + 1, 10)),
        Err(ClipboardPngError::InvalidDimensions)
    );
}

#[test]
fn png_rejects_more_than_64_megapixels_even_with_sides_in_bounds() {
    // Each side stays within MAX_CLIPBOARD_SIDE; only the product exceeds the cap.
    assert_eq!(
        png_dimensions(&valid_png(8_001, 8_001)),
        Err(ClipboardPngError::InvalidDimensions)
    );
}

#[test]
fn png_rejects_an_invalid_bit_depth_and_color_type_combination() {
    // Truecolor (color type 2) never allows a 4-bit depth.
    let bytes = png_prefix(b"IHDR", 13, 1, 1, 4, 2, 0);
    assert_eq!(
        png_dimensions(&bytes),
        Err(ClipboardPngError::InvalidBitDepthColorType)
    );
}

#[test]
fn png_rejects_an_interlace_method_other_than_zero_or_one() {
    let bytes = png_prefix(b"IHDR", 13, 1, 1, 8, 2, 2);
    assert_eq!(
        png_dimensions(&bytes),
        Err(ClipboardPngError::InvalidInterlace)
    );
}

#[test]
fn image_caps_accept_the_edges_and_refuse_past_them() {
    use monhop_protocol::clipboard::{MAX_CLIPBOARD_PIXELS, MAX_CLIPBOARD_SIDE, image_within_caps};
    assert!(image_within_caps(1, 1));
    assert!(image_within_caps(MAX_CLIPBOARD_SIDE, 1));
    assert!(!image_within_caps(0, 1));
    assert!(!image_within_caps(1, 0));
    assert!(!image_within_caps(MAX_CLIPBOARD_SIDE + 1, 1));
    let square = (MAX_CLIPBOARD_PIXELS as f64).sqrt() as u32;
    assert!(image_within_caps(square, square));
    assert!(!image_within_caps(square + 1, square + 1));
    assert!(!image_within_caps(MAX_CLIPBOARD_SIDE, MAX_CLIPBOARD_SIDE));
}
