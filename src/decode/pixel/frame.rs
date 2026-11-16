//! Frame pixel buffers, checked byte sizing, premultiplication, Lanczos3 downscaling, and LZ4 encoding.

// Standard path placeholder for resizing and validation errors.
use std::path::PathBuf;

// Lanczos3 SIMD downscaler preserving alpha channels cleanly.
use fast_image_resize::{
    PixelType, ResizeOptions, Resizer,
    images::{Image as FirImage, ImageRef},
};
// Fast LZ4 compression prepending uncompressed payload byte length.
use lz4_flex::compress_prepend_size;

// Cache subsystem error conversions and microsecond delay clamping.
use crate::{error::CacheError, playback::PlaybackClock};

/// Pixel format multiplier (bytes per pixel for Wayland `ARGB8888` / `RGBA8888`).
pub const BYTES_PER_PIXEL: usize = 4;

/// Synchronously prepared frame 0 ready for immediate Wayland upload and initial cache staging.
#[derive(Debug, Clone)]
pub struct DecodedFirstFrame
{
    /// Stored frame width in pixels (`<= base_width`).
    pub width: u32,

    /// Stored frame height in pixels proportional to stored width.
    pub height: u32,

    /// Display delay in milliseconds rounded from `delay_us` (clamped at cache floor `>= 1 ms`).
    pub delay_ms: u32,

    /// Display delay in microseconds (clamped to `>= 1_000 us` cache floor; playback clamps to `--max-fps`).
    pub delay_us: u32,

    /// Uncompressed premultiplied Wayland `ARGB8888` pixel buffer.
    pub argb8888: Vec<u8>,

    /// LZ4-compressed `ARGB8888` payload with prepended uncompressed byte length.
    pub lz4_payload: Vec<u8>,
}

/// Subsequent compressed animation frame emitted incrementally by the background decoder.
#[derive(Debug, Clone)]
pub struct EncodedFrame
{
    /// Display delay in milliseconds rounded from `delay_us` (clamped at cache floor `>= 1 ms`).
    pub delay_ms: u32,

    /// Display delay in microseconds (clamped to `>= 1_000 us` cache floor; playback clamps to `--max-fps`).
    pub delay_us: u32,

    /// LZ4-compressed premultiplied `ARGB8888` pixel chunk.
    pub lz4_payload: Vec<u8>,
}

/// Decoded RGBA frame unit produced by a container codec prior to scaling and compression.
pub(crate) struct RawRgbaFrame
{
    /// Source pixel width prior to `base_width` downscale.
    pub(crate) width: u32,

    /// Source pixel height prior to aspect-preserving downscale.
    pub(crate) height: u32,

    /// Normalized microsecond display duration (`>= 1_000 us` cache floor).
    pub(crate) delay_us: u32,

    /// Interleaved straight `[R, G, B, A]` pixels.
    pub(crate) rgba: Vec<u8>,
}

/// Converts interleaved straight RGBA pixels in place to Wayland premultiplied `ARGB8888`.
///
/// Little-endian `wl_shm::Format::Argb8888` stores `[B, G, R, A]` bytes per pixel with
/// RGB channels premultiplied by `A / 255`.
pub fn straight_rgba_to_premultiplied_argb8888(rgba: &mut [u8])
{
    for pixel in rgba.as_chunks_mut::<BYTES_PER_PIXEL>().0
    {
        let r = pixel[0];
        let g = pixel[1];
        let b = pixel[2];
        let a = pixel[3];

        match a
        {
            0 => *pixel = [0, 0, 0, 0],
            255 =>
            {
                pixel[0] = b;
                pixel[1] = g;
                pixel[2] = r;
            }
            alpha =>
            {
                let a16 = u16::from(alpha);
                pixel[0] = ((u16::from(b) * a16 + 127) / 255) as u8;
                pixel[1] = ((u16::from(g) * a16 + 127) / 255) as u8;
                pixel[2] = ((u16::from(r) * a16 + 127) / 255) as u8;
            }
        }
    }
}

/// Computes `w * h * 4` cleanly, failing on integer overflow before buffer allocation.
pub fn checked_argb_byte_len(width: u32, height: u32) -> Result<usize, CacheError>
{
    usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        .ok_or(CacheError::DimensionOverflow { width, height })
}

/// Validates non-zero dimensions and exact byte length for an interleaved `ARGB8888` buffer.
pub(super) fn validate_argb_buffer(
    role: &str,
    width: u32,
    height: u32,
    byte_len: usize,
) -> Result<(), CacheError>
{
    let expected = checked_argb_byte_len(width, height)?;
    if width == 0 || height == 0 || byte_len != expected
    {
        return Err(CacheError::Decode {
            path: PathBuf::from("<memory>"),
            reason: format!("invalid {role} dimensions {width}x{height} for {byte_len} bytes"),
        });
    }
    Ok(())
}

/// Scales `raw` to fit `base_width`, converts to premultiplied `ARGB8888`, and compresses via LZ4.
pub(crate) fn encode_first_frame(
    raw: RawRgbaFrame,
    base_width: u32,
    resizer: &mut Resizer,
) -> Result<DecodedFirstFrame, CacheError>
{
    let src_w = raw.width.max(1);
    let src_h = raw.height.max(1);
    let target_w = base_width.min(src_w).max(1);
    let target_h = ((f64::from(src_h) * f64::from(target_w)) / f64::from(src_w))
        .round()
        .max(1.0) as u32;

    let _ = checked_argb_byte_len(target_w, target_h)?;
    let mut argb8888 = downscale_if_needed(raw.rgba, src_w, src_h, target_w, target_h, resizer)?;

    straight_rgba_to_premultiplied_argb8888(&mut argb8888);
    let lz4_payload = compress_prepend_size(&argb8888);

    Ok(DecodedFirstFrame {
        width: target_w,
        height: target_h,
        delay_ms: PlaybackClock::delay_us_to_ms(raw.delay_us),
        delay_us: raw.delay_us,
        argb8888,
        lz4_payload,
    })
}

/// Encodes subsequent frames, dropping the uncompressed buffer after LZ4 compression.
pub(crate) fn encode_subsequent_frame(
    raw: RawRgbaFrame,
    base_width: u32,
    resizer: &mut Resizer,
) -> Result<EncodedFrame, CacheError>
{
    let frame = encode_first_frame(raw, base_width, resizer)?;
    Ok(EncodedFrame {
        delay_ms: frame.delay_ms,
        delay_us: frame.delay_us,
        lz4_payload: frame.lz4_payload,
    })
}

/// Downscales a borrowed premultiplied `ARGB8888` pixel buffer to `(dst_w, dst_h)` with Lanczos3.
///
/// Leaves alpha multiplication disabled because `ARGB8888` cache frames are already premultiplied,
/// avoiding redundant alpha scratch allocations and double premultiplication rounding error.
pub fn resize_premultiplied_argb8888(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    resizer: &mut Resizer,
) -> Result<Vec<u8>, CacheError>
{
    resize_u8x4(src, src_w, src_h, dst_w, dst_h, resizer, false)
}

/// Resizes an `U8x4` pixel buffer with Lanczos3, enabling `mul_div_alpha` for straight alpha input.
fn resize_u8x4(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    resizer: &mut Resizer,
    use_alpha: bool,
) -> Result<Vec<u8>, CacheError>
{
    validate_argb_buffer("source", src_w, src_h, src.len())?;
    let _ = checked_argb_byte_len(dst_w, dst_h)?;
    if src_w == dst_w && src_h == dst_h
    {
        return Ok(src.to_vec());
    }

    let src_img =
        ImageRef::new(src_w, src_h, src, PixelType::U8x4).map_err(|err| CacheError::Decode {
            path: PathBuf::from("<memory>"),
            reason: err.to_string(),
        })?;

    let mut dst_img = FirImage::new(dst_w, dst_h, PixelType::U8x4);
    let options = ResizeOptions::default().use_alpha(use_alpha);
    resizer
        .resize(&src_img, &mut dst_img, &options)
        .map_err(|err| CacheError::Decode {
            path: PathBuf::from("<memory>"),
            reason: err.to_string(),
        })?;

    Ok(dst_img.into_vec())
}

/// Downscales straight `rgba` with Lanczos3 only when `target_w < src_w`, preserving original size otherwise.
fn downscale_if_needed(
    rgba: Vec<u8>,
    src_w: u32,
    src_h: u32,
    target_w: u32,
    target_h: u32,
    resizer: &mut Resizer,
) -> Result<Vec<u8>, CacheError>
{
    if src_w == target_w
    {
        return Ok(rgba);
    }

    resize_u8x4(&rgba, src_w, src_h, target_w, target_h, resizer, true)
}
