//! Clipboard image conversions: packed DIB to RGBA, RGBA to CF_DIBV5, fast PNG encoding, and a
//! bounded PNG decode that re-encodes peer images so platform decoders never parse peer bytes.

use std::fmt;
use std::io::{self, Cursor, Write};

use monhop_protocol::clipboard::{
    ClipboardPngError, MAX_CLIPBOARD_PIXELS, MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_SIDE, png_dimensions,
};

use super::native::Skip;

const BYTES_PER_PIXEL: usize = 4;
/// 32-bit pixels at the pixel cap plus 1 MiB for the header, masks, a color table and a profile.
pub const MAX_DIB_BYTES: usize = MAX_CLIPBOARD_PIXELS as usize * BYTES_PER_PIXEL + (1 << 20);
/// Fast compression can land above the peer's encoder, so a re-encode gets twice the wire cap.
pub const MAX_REENCODED_PNG: usize = 2 * MAX_CLIPBOARD_PNG as usize;
/// Covers the decoder's own row and chunk buffers. The RGBA output is allocated here instead and
/// is at most four bytes per capped pixel (about 256 MiB), because 16-bit channels are stripped.
const DECODER_BUDGET: usize = 16 << 20;

const BITMAPINFOHEADER_LEN: usize = 40;
const BITMAPV4HEADER_LEN: usize = 108;
const BITMAPV5HEADER_LEN: usize = 124;
const MASKS_LEN: usize = 12;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    TooLarge,
    Unsupported,
    Malformed,
}

impl ImageError {
    pub fn skip(self) -> Skip {
        match self {
            Self::TooLarge => Skip::TooLarge,
            Self::Unsupported | Self::Malformed => Skip::Unsupported,
        }
    }
}

/// Straight-alpha RGBA8, rows top to bottom, always within the clipboard pixel caps.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl fmt::Debug for Rgba {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Rgba")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Rgba {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, ImageError> {
        if pixels.len() != rgba_len(width, height)? {
            return Err(ImageError::Malformed);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn dibv5_len(&self) -> usize {
        BITMAPV5HEADER_LEN + self.pixels.len()
    }

    /// Fills `out`, exactly `dibv5_len` bytes (e.g. locked clipboard memory), with a CF_DIBV5:
    /// BITMAPV5HEADER (BI_BITFIELDS, LCS_sRGB) and BGRA rows with straight alpha. Rows go bottom-up
    /// because some programs cannot paste a negative-height DIB.
    pub fn write_dibv5(&self, out: &mut [u8]) {
        assert_eq!(out.len(), self.dibv5_len(), "CF_DIBV5 buffer size");
        let (header, bits) = out.split_at_mut(BITMAPV5HEADER_LEN);
        header.fill(0);
        put_u32(header, 0, BITMAPV5HEADER_LEN as u32);
        put_u32(header, 4, self.width);
        put_u32(header, 8, self.height);
        put_u16(header, 12, 1);
        put_u16(header, 14, 32);
        put_u32(header, 16, BI_BITFIELDS);
        put_u32(
            header,
            20,
            self.width * self.height * BYTES_PER_PIXEL as u32,
        );
        put_u32(header, 40, 0x00FF_0000);
        put_u32(header, 44, 0x0000_FF00);
        put_u32(header, 48, 0x0000_00FF);
        put_u32(header, 52, 0xFF00_0000);
        put_u32(header, 56, LCS_SRGB);
        put_u32(header, 108, LCS_GM_IMAGES);
        let stride = self.width as usize * BYTES_PER_PIXEL;
        let rows = self.pixels.chunks_exact(stride).rev();
        for (target, source) in bits.chunks_exact_mut(stride).zip(rows) {
            let source = source.as_chunks::<4>().0;
            for (bgra, rgba) in target.as_chunks_mut::<4>().0.iter_mut().zip(source) {
                *bgra = [rgba[2], rgba[1], rgba[0], rgba[3]];
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Layout {
    Bgr,
    /// 32-bit BI_RGB: the fourth byte is alpha unless every pixel's is zero, which means opaque.
    Bgrx,
    Masks(Masks),
}

impl Layout {
    const fn source_bytes(self) -> usize {
        match self {
            Self::Bgr => 3,
            Self::Bgrx | Self::Masks(_) => 4,
        }
    }
}

#[derive(Clone, Copy)]
struct Masks {
    red: Channel,
    green: Channel,
    blue: Channel,
    alpha: Option<Channel>,
}

impl Masks {
    fn read(dib: &[u8], header_len: usize) -> Result<Self, ImageError> {
        let channel = |at| Channel::new(read_u32(dib, at)?)?.ok_or(ImageError::Unsupported);
        let alpha = if header_len > BITMAPINFOHEADER_LEN {
            Channel::new(read_u32(dib, 52)?)?
        } else {
            None
        };
        Ok(Self {
            red: channel(40)?,
            green: channel(44)?,
            blue: channel(48)?,
            alpha,
        })
    }

    fn rgba(self, pixel: u32) -> [u8; 4] {
        [
            self.red.extract(pixel),
            self.green.extract(pixel),
            self.blue.extract(pixel),
            self.alpha.map_or(u8::MAX, |alpha| alpha.extract(pixel)),
        ]
    }
}

#[derive(Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    /// `None` for an empty mask; a mask with gaps is unsupported.
    fn new(mask: u32) -> Result<Option<Self>, ImageError> {
        if mask == 0 {
            return Ok(None);
        }
        let shift = mask.trailing_zeros();
        let max = mask >> shift;
        if max & max.wrapping_add(1) != 0 {
            return Err(ImageError::Unsupported);
        }
        Ok(Some(Self { mask, shift, max }))
    }

    fn extract(self, pixel: u32) -> u8 {
        let value = u64::from((pixel & self.mask) >> self.shift);
        let max = u64::from(self.max);
        ((value * 255 + max / 2) / max) as u8
    }
}

/// Converts a packed DIB of 24 or 32 bits per pixel (BI_RGB or BI_BITFIELDS, either row order).
/// Palettized, RLE, JPEG and PNG DIBs are unsupported. Header dimensions and the buffer size are
/// checked against the caps before anything is allocated.
pub fn dib_to_rgba(dib: &[u8]) -> Result<Rgba, ImageError> {
    let header_len = read_u32(dib, 0)? as usize;
    if !matches!(
        header_len,
        BITMAPINFOHEADER_LEN | BITMAPV4HEADER_LEN | BITMAPV5HEADER_LEN
    ) {
        return Err(ImageError::Unsupported);
    }
    if dib.len() < header_len {
        return Err(ImageError::Malformed);
    }
    let width = read_i32(dib, 4)?;
    let height = read_i32(dib, 8)?;
    let planes = read_u16(dib, 12)?;
    let bit_count = read_u16(dib, 14)?;
    let compression = read_u32(dib, 16)?;
    let colors_used = read_u32(dib, 32)?;
    let layout = match (bit_count, compression) {
        (24, BI_RGB) => Layout::Bgr,
        (32, BI_RGB) => Layout::Bgrx,
        (32, BI_BITFIELDS) => Layout::Masks(Masks::read(dib, header_len)?),
        _ => return Err(ImageError::Unsupported),
    };
    if planes != 1 || width <= 0 || height == 0 {
        return Err(ImageError::Malformed);
    }
    let (width, rows) = (width.unsigned_abs(), height.unsigned_abs());
    let len = rgba_len(width, rows)?;

    let masks_after_header = if header_len == BITMAPINFOHEADER_LEN && compression == BI_BITFIELDS {
        MASKS_LEN
    } else {
        0
    };
    let offset = (header_len + masks_after_header) as u64 + u64::from(colors_used) * 4;
    let stride = (u64::from(width) * u64::from(bit_count)).div_ceil(32) * 4;
    let bits_len = stride * u64::from(rows);
    if offset + bits_len > dib.len() as u64 {
        return Err(ImageError::Malformed);
    }
    let bits = &dib[offset as usize..(offset + bits_len) as usize];
    let (width, rows, stride) = (width as usize, rows as usize, stride as usize);

    let mut pixels = zeroed(len)?;
    let mut any_alpha = false;
    for (row, target) in pixels.chunks_exact_mut(width * BYTES_PER_PIXEL).enumerate() {
        let source_row = if height < 0 { row } else { rows - 1 - row };
        let source = &bits[source_row * stride..][..width * layout.source_bytes()];
        let target = target.as_chunks_mut::<4>().0.iter_mut();
        match layout {
            Layout::Bgr => {
                for (rgba, bgr) in target.zip(source.as_chunks::<3>().0) {
                    *rgba = [bgr[2], bgr[1], bgr[0], u8::MAX];
                }
            }
            Layout::Bgrx => {
                for (rgba, bgra) in target.zip(source.as_chunks::<4>().0) {
                    any_alpha |= bgra[3] != 0;
                    *rgba = [bgra[2], bgra[1], bgra[0], bgra[3]];
                }
            }
            Layout::Masks(masks) => {
                for (rgba, raw) in target.zip(source.as_chunks::<4>().0) {
                    *rgba = masks.rgba(u32::from_le_bytes(*raw));
                }
            }
        }
    }
    if matches!(layout, Layout::Bgrx) && !any_alpha {
        pixels
            .iter_mut()
            .skip(3)
            .step_by(BYTES_PER_PIXEL)
            .for_each(|alpha| *alpha = u8::MAX);
    }
    Ok(Rgba {
        width: width as u32,
        height: rows as u32,
        pixels,
    })
}

/// A local DIB as wire PNG, which must fit the wire cap.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, ImageError> {
    encode_png(&dib_to_rgba(dib)?, MAX_CLIPBOARD_PNG as usize)
}

/// The platform's own PNG flavor goes out unchanged after the checks a receiver makes first.
pub fn check_outgoing_png(png: &[u8]) -> Result<(), ImageError> {
    if png.len() > MAX_CLIPBOARD_PNG as usize {
        return Err(ImageError::TooLarge);
    }
    png_dimensions(png).map_err(prefix_error)?;
    Ok(())
}

/// Fast compression; fails with `TooLarge` as soon as the output would pass `limit` bytes.
pub fn encode_png(image: &Rgba, limit: usize) -> Result<Vec<u8>, ImageError> {
    let mut output = CappedWriter {
        bytes: Vec::new(),
        limit,
    };
    let mut encoder = png::Encoder::new(&mut output, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(encode_error)?;
    writer
        .write_image_data(&image.pixels)
        .map_err(encode_error)?;
    writer.finish().map_err(encode_error)?;
    Ok(output.bytes)
}

/// Decodes a peer PNG with checksums verified. The IHDR is checked against the caps before the
/// decoder runs; text and ICC chunks are skipped, never parsed.
pub fn decode_png(png: &[u8]) -> Result<Rgba, ImageError> {
    if png.len() > MAX_CLIPBOARD_PNG as usize {
        return Err(ImageError::TooLarge);
    }
    let (width, height) = png_dimensions(png).map_err(prefix_error)?;
    let len = rgba_len(width, height)?;
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(png),
        png::Limits {
            bytes: DECODER_BUDGET,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.ignore_checksums(false);
    let mut reader = decoder.read_info().map_err(decode_error)?;
    let channels = match reader.output_color_type() {
        (png::ColorType::Grayscale, png::BitDepth::Eight) => 1,
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => 2,
        (png::ColorType::Rgb, png::BitDepth::Eight) => 3,
        (png::ColorType::Rgba, png::BitDepth::Eight) => 4,
        _ => return Err(ImageError::Malformed),
    };
    let mut pixels = zeroed(len)?;
    let frame = reader.next_frame(&mut pixels).map_err(decode_error)?;
    if (frame.width, frame.height) != (width, height) {
        return Err(ImageError::Malformed);
    }
    reader.finish().map_err(decode_error)?;
    expand_to_rgba(&mut pixels, channels);
    Ok(Rgba {
        width,
        height,
        pixels,
    })
}

/// What a peer PNG becomes before it reaches the local clipboard: decoded under the caps, then
/// encoded afresh, so platform image decoders only ever parse MonHop's own output.
pub fn reencode_png(peer: &[u8]) -> Result<(Rgba, Vec<u8>), ImageError> {
    let image = decode_png(peer)?;
    let png = encode_png(&image, MAX_REENCODED_PNG)?;
    Ok((image, png))
}

/// Widens packed pixels at the front of `pixels` to RGBA in place. Walking backwards is safe: a
/// pixel's RGBA slot never starts before its own source bytes or overlaps an unread pixel.
fn expand_to_rgba(pixels: &mut [u8], channels: usize) {
    if channels == BYTES_PER_PIXEL {
        return;
    }
    for index in (0..pixels.len() / BYTES_PER_PIXEL).rev() {
        let source = index * channels;
        let rgba = match channels {
            1 => [pixels[source], pixels[source], pixels[source], u8::MAX],
            2 => [
                pixels[source],
                pixels[source],
                pixels[source],
                pixels[source + 1],
            ],
            _ => [
                pixels[source],
                pixels[source + 1],
                pixels[source + 2],
                u8::MAX,
            ],
        };
        pixels[index * BYTES_PER_PIXEL..][..BYTES_PER_PIXEL].copy_from_slice(&rgba);
    }
}

fn rgba_len(width: u32, height: u32) -> Result<usize, ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Malformed);
    }
    let pixels = u64::from(width) * u64::from(height);
    if width > MAX_CLIPBOARD_SIDE || height > MAX_CLIPBOARD_SIDE || pixels > MAX_CLIPBOARD_PIXELS {
        return Err(ImageError::TooLarge);
    }
    usize::try_from(pixels * BYTES_PER_PIXEL as u64).map_err(|_| ImageError::TooLarge)
}

fn zeroed(len: usize) -> Result<Vec<u8>, ImageError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| ImageError::TooLarge)?;
    buffer.resize(len, 0);
    Ok(buffer)
}

fn prefix_error(error: ClipboardPngError) -> ImageError {
    match error {
        ClipboardPngError::InvalidDimensions => ImageError::TooLarge,
        _ => ImageError::Malformed,
    }
}

fn decode_error(error: png::DecodingError) -> ImageError {
    match error {
        png::DecodingError::LimitsExceeded => ImageError::TooLarge,
        _ => ImageError::Malformed,
    }
}

/// Only `CappedWriter` can fail with an I/O error, and only at the cap or out of memory.
fn encode_error(error: png::EncodingError) -> ImageError {
    match error {
        png::EncodingError::IoError(_) => ImageError::TooLarge,
        _ => ImageError::Malformed,
    }
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.limit - self.bytes.len() {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.bytes
            .try_reserve(data.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, ImageError> {
    let field = bytes.get(at..at + 2).ok_or(ImageError::Malformed)?;
    Ok(u16::from_le_bytes([field[0], field[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, ImageError> {
    let field = bytes.get(at..at + 4).ok_or(ImageError::Malformed)?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

fn read_i32(bytes: &[u8], at: usize) -> Result<i32, ImageError> {
    read_u32(bytes, at).map(|value| i32::from_le_bytes(value.to_le_bytes()))
}

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dib(
        header_len: usize,
        width: i32,
        height: i32,
        bit_count: u16,
        compression: u32,
    ) -> Vec<u8> {
        let mut dib = vec![0; header_len];
        put_u32(&mut dib, 0, header_len as u32);
        put_u32(&mut dib, 4, width as u32);
        put_u32(&mut dib, 8, height as u32);
        put_u16(&mut dib, 12, 1);
        put_u16(&mut dib, 14, bit_count);
        put_u32(&mut dib, 16, compression);
        dib
    }

    fn rgba(width: u32, height: u32, pixels: &[[u8; 4]]) -> Rgba {
        Rgba::new(width, height, pixels.concat()).unwrap()
    }

    fn png_with(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
        configure: impl FnOnce(&mut png::Encoder<'_, &mut Vec<u8>>),
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        configure(&mut encoder);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        bytes
    }

    fn chunk_at(png: &[u8], kind: &[u8; 4]) -> usize {
        png.windows(4).position(|window| window == kind).unwrap() - 4
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn a_bottom_up_24_bit_dib_becomes_top_down_rgba_and_skips_row_padding() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 2, 2, 24, BI_RGB);
        bytes.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let image = dib_to_rgba(&bytes).unwrap();
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.pixels(), [RED, GREEN, BLUE, WHITE].concat());
    }

    #[test]
    fn a_top_down_24_bit_dib_keeps_its_row_order() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 2, -2, 24, BI_RGB);
        bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        bytes.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        let image = dib_to_rgba(&bytes).unwrap();
        assert_eq!(image.pixels(), [RED, GREEN, BLUE, WHITE].concat());
    }

    #[test]
    fn a_color_table_beside_24_bit_pixels_is_skipped() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 1, 1, 24, BI_RGB);
        put_u32(&mut bytes, 32, 2);
        bytes.extend_from_slice(&[9; 8]);
        bytes.extend_from_slice(&[0, 0, 255, 0]);
        assert_eq!(dib_to_rgba(&bytes).unwrap().pixels(), RED);
    }

    #[test]
    fn a_32_bit_rgb_dib_whose_alpha_is_all_zero_is_opaque() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 1, -2, 32, BI_RGB);
        bytes.extend_from_slice(&[10, 20, 30, 0, 40, 50, 60, 0]);
        let image = dib_to_rgba(&bytes).unwrap();
        assert_eq!(image.pixels(), [30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn a_32_bit_rgb_dib_with_some_alpha_keeps_straight_alpha() {
        let mut bytes = dib(BITMAPV5HEADER_LEN, 1, -2, 32, BI_RGB);
        bytes.extend_from_slice(&[10, 20, 30, 128, 40, 50, 60, 0]);
        let image = dib_to_rgba(&bytes).unwrap();
        assert_eq!(image.pixels(), [30, 20, 10, 128, 60, 50, 40, 0]);
    }

    #[test]
    fn bitfields_after_a_40_byte_header_are_read_and_mean_opaque() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 1, 1, 32, BI_BITFIELDS);
        for mask in [0x00FF_0000_u32, 0x0000_FF00, 0x0000_00FF] {
            bytes.extend_from_slice(&mask.to_le_bytes());
        }
        bytes.extend_from_slice(&0x8011_2233_u32.to_le_bytes());
        assert_eq!(
            dib_to_rgba(&bytes).unwrap().pixels(),
            [0x11, 0x22, 0x33, 255]
        );
    }

    #[test]
    fn wide_bitfields_in_a_v5_header_scale_to_8_bits() {
        let mut bytes = dib(BITMAPV5HEADER_LEN, 1, 1, 32, BI_BITFIELDS);
        put_u32(&mut bytes, 40, 0x3FF0_0000);
        put_u32(&mut bytes, 44, 0x000F_FC00);
        put_u32(&mut bytes, 48, 0x0000_03FF);
        put_u32(&mut bytes, 52, 0xC000_0000);
        let pixel: u32 = (3 << 30) | (1023 << 20) | 512;
        bytes.extend_from_slice(&pixel.to_le_bytes());
        assert_eq!(dib_to_rgba(&bytes).unwrap().pixels(), [255, 0, 128, 255]);
    }

    #[test]
    fn palettized_compressed_and_odd_depth_dibs_are_unsupported() {
        for (bit_count, compression) in [(8, BI_RGB), (4, BI_RGB), (1, BI_RGB), (16, BI_RGB)] {
            let mut bytes = dib(BITMAPINFOHEADER_LEN, 1, 1, bit_count, compression);
            bytes.extend_from_slice(&[0; 1024]);
            assert_eq!(dib_to_rgba(&bytes), Err(ImageError::Unsupported));
        }
        for compression in [1, 2, 4, 5, 6] {
            let mut bytes = dib(BITMAPV5HEADER_LEN, 1, 1, 0, compression);
            bytes.extend_from_slice(&[0; 64]);
            assert_eq!(dib_to_rgba(&bytes), Err(ImageError::Unsupported));
        }
        let mut gaps = dib(BITMAPV5HEADER_LEN, 1, 1, 32, BI_BITFIELDS);
        put_u32(&mut gaps, 40, 0x00F0_F000);
        put_u32(&mut gaps, 44, 0x0000_00F0);
        put_u32(&mut gaps, 48, 0x0000_000F);
        gaps.extend_from_slice(&[0; 4]);
        assert_eq!(dib_to_rgba(&gaps), Err(ImageError::Unsupported));
        let core = [12, 0, 0, 0, 1, 0, 1, 0, 1, 0, 24, 0, 0, 0, 0];
        assert_eq!(dib_to_rgba(&core), Err(ImageError::Unsupported));
    }

    #[test]
    fn oversized_dimensions_are_refused_before_the_pixels_are_checked() {
        for (width, height) in [(32_769, 1), (1, -32_769), (8_001, 8_000), (1, i32::MIN)] {
            let bytes = dib(BITMAPINFOHEADER_LEN, width, height, 32, BI_RGB);
            assert_eq!(dib_to_rgba(&bytes), Err(ImageError::TooLarge));
        }
    }

    #[test]
    fn malformed_dibs_are_rejected() {
        assert_eq!(dib_to_rgba(&[40, 0]), Err(ImageError::Malformed));
        assert_eq!(
            dib_to_rgba(&dib(BITMAPV5HEADER_LEN, 1, 1, 32, BI_RGB)[..100]),
            Err(ImageError::Malformed)
        );
        for (width, height) in [(0, 1), (1, 0), (-1, 1)] {
            let mut bytes = dib(BITMAPINFOHEADER_LEN, width, height, 32, BI_RGB);
            bytes.extend_from_slice(&[0; 4]);
            assert_eq!(dib_to_rgba(&bytes), Err(ImageError::Malformed));
        }
        let mut planes = dib(BITMAPINFOHEADER_LEN, 1, 1, 32, BI_RGB);
        put_u16(&mut planes, 12, 2);
        planes.extend_from_slice(&[0; 4]);
        assert_eq!(dib_to_rgba(&planes), Err(ImageError::Malformed));
        let mut truncated = dib(BITMAPINFOHEADER_LEN, 2, 2, 24, BI_RGB);
        truncated.extend_from_slice(&[0; 15]);
        assert_eq!(dib_to_rgba(&truncated), Err(ImageError::Malformed));
        let mut huge_table = dib(BITMAPINFOHEADER_LEN, 1, 1, 24, BI_RGB);
        put_u32(&mut huge_table, 32, u32::MAX);
        huge_table.extend_from_slice(&[0; 4]);
        assert_eq!(dib_to_rgba(&huge_table), Err(ImageError::Malformed));
    }

    #[test]
    fn rgba_becomes_a_bottom_up_bitfields_dibv5_with_an_srgb_header() {
        let image = rgba(1, 2, &[[1, 2, 3, 4], [5, 6, 7, 8]]);
        let mut out = vec![0xAA; image.dibv5_len()];
        image.write_dibv5(&mut out);
        let field = |at| read_u32(&out, at).unwrap();
        assert_eq!(out.len(), 124 + 8);
        assert_eq!(field(0), 124);
        assert_eq!(read_i32(&out, 4).unwrap(), 1);
        assert_eq!(read_i32(&out, 8).unwrap(), 2);
        assert_eq!(read_u16(&out, 12).unwrap(), 1);
        assert_eq!(read_u16(&out, 14).unwrap(), 32);
        assert_eq!(field(16), BI_BITFIELDS);
        assert_eq!(field(20), 8);
        assert_eq!(
            [field(40), field(44), field(48), field(52)],
            [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000]
        );
        assert_eq!(field(56), LCS_SRGB);
        assert!(out[60..108].iter().all(|byte| *byte == 0));
        assert_eq!(field(108), LCS_GM_IMAGES);
        assert!(out[112..124].iter().all(|byte| *byte == 0));
        assert_eq!(&out[124..], [7, 6, 5, 8, 3, 2, 1, 4]);
    }

    #[test]
    fn a_dibv5_round_trips_through_the_dib_reader() {
        let image = rgba(2, 2, &[[1, 2, 3, 0], [4, 5, 6, 7], [8, 9, 10, 128], WHITE]);
        let mut out = vec![0; image.dibv5_len()];
        image.write_dibv5(&mut out);
        assert_eq!(dib_to_rgba(&out).unwrap(), image);
    }

    #[test]
    fn rgba_rejects_a_mismatched_or_oversized_buffer() {
        assert_eq!(Rgba::new(2, 1, vec![0; 4]), Err(ImageError::Malformed));
        assert_eq!(Rgba::new(32_769, 1, Vec::new()), Err(ImageError::TooLarge));
        assert_eq!(Rgba::new(0, 1, Vec::new()), Err(ImageError::Malformed));
        let debug = format!("{:?}", rgba(1, 1, &[RED]));
        assert!(debug.contains("width") && !debug.contains("255"));
    }

    #[test]
    fn png_round_trips_rgba_exactly() {
        let image = rgba(3, 1, &[[1, 2, 3, 0], [250, 128, 7, 99], WHITE]);
        let png = encode_png(&image, MAX_CLIPBOARD_PNG as usize).unwrap();
        check_outgoing_png(&png).unwrap();
        assert_eq!(decode_png(&png).unwrap(), image);
    }

    #[test]
    fn a_dib_encodes_to_a_png_of_the_same_pixels() {
        let mut bytes = dib(BITMAPINFOHEADER_LEN, 2, 2, 24, BI_RGB);
        bytes.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let png = dib_to_png(&bytes).unwrap();
        assert_eq!(decode_png(&png).unwrap(), dib_to_rgba(&bytes).unwrap());
    }

    #[test]
    fn every_png_color_type_decodes_to_8_bit_rgba() {
        let gray = png_with(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[7, 200],
            |_| {},
        );
        assert_eq!(
            decode_png(&gray).unwrap().pixels(),
            [7, 7, 7, 255, 200, 200, 200, 255]
        );

        let bits = png_with(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &[0b0100_0000],
            |_| {},
        );
        assert_eq!(
            decode_png(&bits).unwrap().pixels(),
            [0, 0, 0, 255, 255, 255, 255, 255]
        );

        let gray_alpha = png_with(
            1,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[9, 50],
            |_| {},
        );
        assert_eq!(decode_png(&gray_alpha).unwrap().pixels(), [9, 9, 9, 50]);

        let rgb = png_with(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[1, 2, 3],
            |_| {},
        );
        assert_eq!(decode_png(&rgb).unwrap().pixels(), [1, 2, 3, 255]);

        let deep = png_with(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Sixteen,
            &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
            |_| {},
        );
        assert_eq!(
            decode_png(&deep).unwrap().pixels(),
            [0x12, 0x56, 0x9A, 0xDE]
        );

        let indexed = png_with(
            2,
            1,
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            &[1, 0],
            |encoder| {
                encoder.set_palette(vec![10, 20, 30, 40, 50, 60]);
                encoder.set_trns(vec![255, 0]);
            },
        );
        assert_eq!(
            decode_png(&indexed).unwrap().pixels(),
            [40, 50, 60, 0, 10, 20, 30, 255]
        );
    }

    #[test]
    fn a_bad_crc_or_truncated_png_is_rejected() {
        let png = encode_png(&rgba(1, 1, &[RED]), MAX_CLIPBOARD_PNG as usize).unwrap();

        let mut bad_header_crc = png.clone();
        bad_header_crc[32] ^= 1;
        assert_eq!(decode_png(&bad_header_crc), Err(ImageError::Malformed));

        let mut bad_data_crc = png.clone();
        let idat = chunk_at(&png, b"IDAT");
        let data_len = u32::from_be_bytes(png[idat..idat + 4].try_into().unwrap()) as usize;
        bad_data_crc[idat + 8 + data_len] ^= 1;
        assert_eq!(decode_png(&bad_data_crc), Err(ImageError::Malformed));

        let without_end = &png[..chunk_at(&png, b"IEND")];
        assert_eq!(decode_png(without_end), Err(ImageError::Malformed));
        assert_eq!(decode_png(&png[..40]), Err(ImageError::Malformed));
    }

    #[test]
    fn over_cap_pngs_are_refused_before_decoding() {
        let png = encode_png(&rgba(1, 1, &[RED]), MAX_CLIPBOARD_PNG as usize).unwrap();
        for (width, height) in [(32_769_u32, 1_u32), (8_001, 8_000)] {
            let mut oversized = png.clone();
            oversized[16..20].copy_from_slice(&width.to_be_bytes());
            oversized[20..24].copy_from_slice(&height.to_be_bytes());
            assert_eq!(decode_png(&oversized), Err(ImageError::TooLarge));
            assert_eq!(check_outgoing_png(&oversized), Err(ImageError::TooLarge));
        }
        let mut too_long = png.clone();
        too_long.resize(MAX_CLIPBOARD_PNG as usize + 1, 0);
        assert_eq!(decode_png(&too_long), Err(ImageError::TooLarge));
        assert_eq!(check_outgoing_png(&too_long), Err(ImageError::TooLarge));
        assert_eq!(
            check_outgoing_png(b"not a png at all, just some text"),
            Err(ImageError::Malformed)
        );
    }

    #[test]
    fn encoding_stops_at_the_output_cap() {
        let image = rgba(2, 1, &[RED, BLUE]);
        assert_eq!(encode_png(&image, 16), Err(ImageError::TooLarge));
    }

    #[test]
    fn a_reencode_keeps_the_pixels_and_drops_peer_metadata() {
        let peer = png_with(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[1, 2, 3],
            |encoder| {
                encoder
                    .add_text_chunk("Comment".to_owned(), "from the peer".to_owned())
                    .unwrap();
            },
        );
        assert!(peer.windows(4).any(|window| window == b"tEXt"));
        let (image, png) = reencode_png(&peer).unwrap();
        assert_eq!(image.pixels(), [1, 2, 3, 255]);
        assert!(!png.windows(4).any(|window| window == b"tEXt"));
        assert_eq!(decode_png(&png).unwrap(), image);
    }

    #[test]
    fn image_errors_map_to_user_facing_skips() {
        assert_eq!(ImageError::TooLarge.skip(), Skip::TooLarge);
        assert_eq!(ImageError::Unsupported.skip(), Skip::Unsupported);
        assert_eq!(ImageError::Malformed.skip(), Skip::Unsupported);
    }
}
