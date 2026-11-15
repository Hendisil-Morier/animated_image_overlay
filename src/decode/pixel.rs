//! Frame pixel buffers, checked byte sizing, Lanczos3 downscaling, and LZ4 compression.

// Standard path placeholder for resizing errors.
use std::path::PathBuf;

// Lanczos3 SIMD downscaler preserving alpha channels cleanly.
use fast_image_resize::{
    PixelType, ResizeOptions, Resizer,
    images::{Image as FirImage, ImageRef},
};
// Fast LZ4 compression prepending uncompressed payload byte length.
use lz4_flex::compress_prepend_size;

// Cache subsystem error conversions, angle bounding-box geometry, and microsecond delay rounding.
use crate::{
    error::CacheError,
    layout::{normalized_angle_deg, rotated_bounds},
    playback::PlaybackClock,
};

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

// 40.24 fixed-point sampling resolution (24 fractional bits, 12-bit bilinear weights).
// 12-bit weights keep max weighted channel sum `255 * 4096^2 + 2^23 = 0xFF80_0000 < u32::MAX`,
// allowing two independent channels per `u64` SWAR lane without carry between lanes.
const ROTATE_STEP_SHIFT: i32 = 24;
const ROTATE_STEP_SCALE: f64 = (1_u64 << ROTATE_STEP_SHIFT) as f64;
const ROTATE_WEIGHT_BITS: i32 = 12;
const ROTATE_WEIGHT_SUBSHIFT: i32 = ROTATE_STEP_SHIFT - ROTATE_WEIGHT_BITS;
const ROTATE_WEIGHT_ONE: u64 = 1_u64 << ROTATE_WEIGHT_BITS;
const ROTATE_WEIGHT_MASK: i64 = (ROTATE_WEIGHT_ONE as i64) - 1;
const ROTATE_WEIGHT_TOTAL_BITS: i32 = ROTATE_WEIGHT_BITS * 2;
const ROTATE_ROUND_SINGLE: u64 = 1_u64 << (ROTATE_WEIGHT_TOTAL_BITS - 1);
const ROTATE_ROUND_DUAL: u64 = (ROTATE_ROUND_SINGLE << 32) | ROTATE_ROUND_SINGLE;

/// Computes normalized 2x2 bilinear weights `[w00, w10, w01, w11]` summing to `1 << 24`.
#[inline]
fn bilinear_weights(sx_fp: i64, sy_fp: i64) -> (u64, u64, u64, u64)
{
    let fx = ((sx_fp >> ROTATE_WEIGHT_SUBSHIFT) & ROTATE_WEIGHT_MASK) as u64;
    let fy = ((sy_fp >> ROTATE_WEIGHT_SUBSHIFT) & ROTATE_WEIGHT_MASK) as u64;
    let inv_x = ROTATE_WEIGHT_ONE - fx;
    let inv_y = ROTATE_WEIGHT_ONE - fy;
    (inv_x * inv_y, fx * inv_y, inv_x * fy, fx * fy)
}

/// Fetches `[B, G, R, A]` as `u64` inside `[0, src_w) x [0, src_h)`, or transparent black in the apron.
#[inline]
fn sample_apron_pixel(src: &[u8], src_w: i64, src_h: i64, x: i64, y: i64)
-> [u64; BYTES_PER_PIXEL]
{
    if x < 0 || x >= src_w || y < 0 || y >= src_h
    {
        return [0; BYTES_PER_PIXEL];
    }

    let offset = ((y * src_w + x) as usize) * BYTES_PER_PIXEL;
    [
        u64::from(src[offset]),
        u64::from(src[offset + 1]),
        u64::from(src[offset + 2]),
        u64::from(src[offset + 3]),
    ]
}

/// Resamples one boundary/exterior pixel cleanly across the `1 px` transparent apron.
#[inline]
fn write_apron_pixel(
    src: &[u8],
    src_w: i64,
    src_h: i64,
    sx_fp: i64,
    sy_fp: i64,
    out_px: &mut [u8; BYTES_PER_PIXEL],
)
{
    let x0 = sx_fp >> ROTATE_STEP_SHIFT;
    let y0 = sy_fp >> ROTATE_STEP_SHIFT;
    if x0 < -1 || x0 >= src_w || y0 < -1 || y0 >= src_h
    {
        *out_px = [0; BYTES_PER_PIXEL];
        return;
    }

    let (w00, w10, w01, w11) = bilinear_weights(sx_fp, sy_fp);
    let p00 = sample_apron_pixel(src, src_w, src_h, x0, y0);
    let p10 = sample_apron_pixel(src, src_w, src_h, x0 + 1, y0);
    let p01 = sample_apron_pixel(src, src_w, src_h, x0, y0 + 1);
    let p11 = sample_apron_pixel(src, src_w, src_h, x0 + 1, y0 + 1);

    for ch in 0..BYTES_PER_PIXEL
    {
        let sum =
            w00 * p00[ch] + w10 * p10[ch] + w01 * p01[ch] + w11 * p11[ch] + ROTATE_ROUND_SINGLE;
        out_px[ch] = (sum >> ROTATE_WEIGHT_TOTAL_BITS) as u8;
    }
}

/// Computes the index range `[start, end)` within `0..count` where `0 <= start_fp + i*step <= max_fp`.
#[inline]
fn linear_in_bounds(start_fp: i64, step: i64, max_fp: i64, count: usize) -> (usize, usize)
{
    if max_fp < 0 || count == 0
    {
        return (0, 0);
    }
    if step == 0
    {
        return if start_fp >= 0 && start_fp <= max_fp
        {
            (0, count)
        }
        else
        {
            (0, 0)
        };
    }

    let (lo_num, hi_num, denom) = if step > 0
    {
        (
            -i128::from(start_fp),
            i128::from(max_fp) - i128::from(start_fp),
            i128::from(step),
        )
    }
    else
    {
        (
            i128::from(start_fp) - i128::from(max_fp),
            i128::from(start_fp),
            -i128::from(step),
        )
    };
    if hi_num < 0
    {
        return (0, 0);
    }

    let lo = if lo_num <= 0
    {
        0
    }
    else
    {
        (lo_num + denom - 1) / denom
    };
    let hi_inc = hi_num.div_euclid(denom);
    let start_idx = usize::try_from(lo).unwrap_or(count).min(count);
    let end_idx = usize::try_from(hi_inc.saturating_add(1))
        .unwrap_or(count)
        .min(count);
    (start_idx, end_idx.max(start_idx))
}

/// Blends four strictly in-bounds `ARGB8888` neighbor pixels via packed `u64` two-channel SWAR lanes.
///
/// Packs `[B, R]` and `[G, A]` into bits `0..31` and `32..63` of `u64` accumulators because
/// 12-bit weights cap each product sum below `0xFF80_0000 < 2^32`, eliminating cross-lane carry.
#[inline]
fn write_interior_pixel_swar(
    top: &[u8],
    bot: &[u8],
    sx_fp: i64,
    sy_fp: i64,
    out_px: &mut [u8; BYTES_PER_PIXEL],
)
{
    let (w00, w10, w01, w11) = bilinear_weights(sx_fp, sy_fp);
    let top = &top[..8];
    let bot = &bot[..8];

    let p00_rb = u64::from(top[0]) | (u64::from(top[2]) << 32);
    let p00_ag = u64::from(top[1]) | (u64::from(top[3]) << 32);
    let p10_rb = u64::from(top[4]) | (u64::from(top[6]) << 32);
    let p10_ag = u64::from(top[5]) | (u64::from(top[7]) << 32);
    let p01_rb = u64::from(bot[0]) | (u64::from(bot[2]) << 32);
    let p01_ag = u64::from(bot[1]) | (u64::from(bot[3]) << 32);
    let p11_rb = u64::from(bot[4]) | (u64::from(bot[6]) << 32);
    let p11_ag = u64::from(bot[5]) | (u64::from(bot[7]) << 32);

    let rb = w00 * p00_rb + w10 * p10_rb + w01 * p01_rb + w11 * p11_rb + ROTATE_ROUND_DUAL;
    let ag = w00 * p00_ag + w10 * p10_ag + w01 * p01_ag + w11 * p11_ag + ROTATE_ROUND_DUAL;

    out_px[0] = (rb >> ROTATE_WEIGHT_TOTAL_BITS) as u8;
    out_px[1] = (ag >> ROTATE_WEIGHT_TOTAL_BITS) as u8;
    out_px[2] = (rb >> (ROTATE_WEIGHT_TOTAL_BITS + 32)) as u8;
    out_px[3] = (ag >> (ROTATE_WEIGHT_TOTAL_BITS + 32)) as u8;
}

/// Validates non-zero dimensions and exact byte length for an interleaved `ARGB8888` buffer.
fn validate_argb_buffer(
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

/// Bilinearly resamples premultiplied `ARGB8888` frame `src` (`src_w x src_h`) directly into
/// caller-owned slice `dst` (`dst_w x dst_h`) rotated clockwise by `angle_deg` and mirrored
/// horizontally when `flipped`.
///
/// Scales across the rotated bounding box `(dst_w, dst_h)` without intermediate heap allocations
/// and uses a `1 px` transparent apron outside source bounds so borders anti-alias cleanly.
#[expect(clippy::too_many_arguments)]
pub fn rotate_argb8888_into(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    angle_deg: f64,
    flipped: bool,
    dst_w: u32,
    dst_h: u32,
    dst: &mut [u8],
) -> Result<(), CacheError>
{
    validate_argb_buffer("source", src_w, src_h, src.len())?;
    validate_argb_buffer("destination", dst_w, dst_h, dst.len())?;

    let (full_w, full_h) = rotated_bounds(src_w, src_h, angle_deg);
    let radians = normalized_angle_deg(angle_deg).to_radians();
    let cos_a = radians.cos();
    let sin_a = radians.sin();
    let flip_sign = if flipped { -1.0 } else { 1.0 };

    let src_cx = (f64::from(src_w) - 1.0) * 0.5;
    let src_cy = (f64::from(src_h) - 1.0) * 0.5;
    let full_wf = f64::from(full_w);
    let full_hf = f64::from(full_h);
    let step_x = full_wf / f64::from(dst_w);
    let step_y = full_hf / f64::from(dst_h);

    // Horizontal incremental step along output X in fixed-point source coordinates.
    let step_x_du = (flip_sign * cos_a * step_x * ROTATE_STEP_SCALE).round() as i64;
    let step_x_dv = (-flip_sign * sin_a * step_x * ROTATE_STEP_SCALE).round() as i64;

    let src_wi = i64::from(src_w);
    let src_hi = i64::from(src_h);
    let src_stride = (src_w as usize) * BYTES_PER_PIXEL;
    let dst_stride = (dst_w as usize) * BYTES_PER_PIXEL;
    let dst_len = dst_w as usize;
    let max_x_fp = ((src_wi - 1) << ROTATE_STEP_SHIFT) - 1;
    let max_y_fp = ((src_hi - 1) << ROTATE_STEP_SHIFT) - 1;
    let dx0 = 0.5 * step_x - 0.5 * full_wf;

    for (oy, row) in dst.chunks_exact_mut(dst_stride).enumerate()
    {
        let dy = (oy as f64 + 0.5) * step_y - 0.5 * full_hf;
        let row_sx0 = src_cx + (dx0 * flip_sign) * cos_a + dy * sin_a;
        let row_sy0 = src_cy - (dx0 * flip_sign) * sin_a + dy * cos_a;
        let mut sx_fp = (row_sx0 * ROTATE_STEP_SCALE).round() as i64;
        let mut sy_fp = (row_sy0 * ROTATE_STEP_SCALE).round() as i64;

        // Partition the output row into outer 1 px apron segments and one strictly interior span
        // so interior SWAR pixels slice neighbor rows directly, eliding per-pixel bounds branches.
        let (xl, xr) = linear_in_bounds(sx_fp, step_x_du, max_x_fp, dst_len);
        let (yl, yr) = linear_in_bounds(sy_fp, step_x_dv, max_y_fp, dst_len);
        let in_start = xl.max(yl);
        let in_end = xr.min(yr).max(in_start);

        let out_chunks = row.as_chunks_mut::<BYTES_PER_PIXEL>().0;
        for out_px in &mut out_chunks[..in_start]
        {
            write_apron_pixel(src, src_wi, src_hi, sx_fp, sy_fp, out_px);
            sx_fp += step_x_du;
            sy_fp += step_x_dv;
        }

        for out_px in &mut out_chunks[in_start..in_end]
        {
            let x0 = (sx_fp >> ROTATE_STEP_SHIFT) as usize;
            let y0 = (sy_fp >> ROTATE_STEP_SHIFT) as usize;
            let off0 = y0 * src_stride + x0 * BYTES_PER_PIXEL;
            let off1 = off0 + src_stride;
            write_interior_pixel_swar(&src[off0..], &src[off1..], sx_fp, sy_fp, out_px);
            sx_fp += step_x_du;
            sy_fp += step_x_dv;
        }

        for out_px in &mut out_chunks[in_end..]
        {
            write_apron_pixel(src, src_wi, src_hi, sx_fp, sy_fp, out_px);
            sx_fp += step_x_du;
            sy_fp += step_x_dv;
        }
    }

    Ok(())
}
