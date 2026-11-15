//! Image container format detection and codec frame iterators across APNG, GIF, WebP, and stills.

// Filesystem readers for image containers.
use std::{
    fs::{self, File},
    io::BufReader,
    path::Path,
};

// Lanczos resizer reused across synchronous single-frame decode.
use fast_image_resize::Resizer;
// Still and animated codecs (`APNG`, `GIF`, and fallback static readers).
use image::{
    AnimationDecoder, ImageFormat, ImageReader,
    codecs::{gif::GifDecoder, png::PngDecoder},
};
// Animated and static WebP decoding backed by `libwebp`.
use webp_animation::{ColorMode, Decoder as WebpDecoder, DecoderOptions as WebpOptions};

// Shared raw frame struct, cache error mappings, and normalized frame delay clamping.
use super::pixel::{DecodedFirstFrame, RawRgbaFrame, encode_first_frame};
use crate::{error::CacheError, playback::PlaybackClock};

/// Identified container decoder strategy for a validated source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind
{
    /// PNG containing an `acTL` animation control chunk.
    Apng,

    /// Graphics Interchange Format animation or single frame.
    Gif,

    /// Animated or still WebP bitstream.
    Webp,

    /// Single-frame image decoded directly by `image::ImageReader`.
    Static,
}

/// Boxed iterator yielding decoded straight-RGBA frames and normalized delays.
pub(crate) type FrameIterator<'a> = Box<dyn Iterator<Item = Result<RawRgbaFrame, CacheError>> + 'a>;

/// Determines container format and whether PNG carries an APNG animation sequence.
pub(crate) fn detect_source_kind(path: &Path) -> Result<SourceKind, CacheError>
{
    let reader = ImageReader::open(path)
        .map_err(|source| io_err(path, source))?
        .with_guessed_format()
        .map_err(|source| io_err(path, source))?;

    match reader.format()
    {
        Some(ImageFormat::Png) =>
        {
            let file = File::open(path).map_err(|source| io_err(path, source))?;
            let png = PngDecoder::new(BufReader::new(file))
                .map_err(|err| decode_err(path, err.to_string()))?;

            if png.is_apng().unwrap_or(false)
            {
                Ok(SourceKind::Apng)
            }
            else
            {
                Ok(SourceKind::Static)
            }
        }
        Some(ImageFormat::Gif) => Ok(SourceKind::Gif),
        Some(ImageFormat::WebP) => Ok(SourceKind::Webp),
        _ => Ok(SourceKind::Static),
    }
}

/// Synchronously decodes a single frame for static images or `--max-frames 1`.
pub(crate) fn decode_single_frame_sync(
    path: &Path,
    kind: SourceKind,
    base_width: u32,
) -> Result<DecodedFirstFrame, CacheError>
{
    let mut resizer = Resizer::new();
    let raw = match kind
    {
        SourceKind::Apng => open_apng_iter(path)?.next(),
        SourceKind::Gif => open_gif_iter(path)?.next(),
        SourceKind::Webp => Some(Ok(decode_first_webp_or_static(path)?)),
        SourceKind::Static => Some(Ok(decode_static_image(path)?)),
    };

    let frame =
        raw.ok_or_else(|| decode_err(path, String::from("Image contained zero frames")))??;
    encode_first_frame(frame, base_width, &mut resizer)
}

/// Opens a streaming APNG frame iterator yielding normalized `RawRgbaFrame` records.
pub(crate) fn open_apng_iter<'a>(path: &'a Path) -> Result<FrameIterator<'a>, CacheError>
{
    let file = File::open(path).map_err(|source| io_err(path, source))?;
    let png =
        PngDecoder::new(BufReader::new(file)).map_err(|err| decode_err(path, err.to_string()))?;
    let apng = png
        .apng()
        .map_err(|err| decode_err(path, err.to_string()))?;

    Ok(Box::new(
        apng.into_frames().map(|res| map_image_frame(path, res)),
    ))
}

/// Opens a streaming GIF frame iterator yielding normalized `RawRgbaFrame` records.
pub(crate) fn open_gif_iter<'a>(path: &'a Path) -> Result<FrameIterator<'a>, CacheError>
{
    let file = File::open(path).map_err(|source| io_err(path, source))?;
    let gif =
        GifDecoder::new(BufReader::new(file)).map_err(|err| decode_err(path, err.to_string()))?;

    Ok(Box::new(
        gif.into_frames().map(|res| map_image_frame(path, res)),
    ))
}

/// Decodes frame 0 of a WebP file, falling back to static reader on large canvases.
pub(crate) fn decode_first_webp_or_static(path: &Path) -> Result<RawRgbaFrame, CacheError>
{
    let bytes = fs::read(path).map_err(|source| io_err(path, source))?;
    let options = WebpOptions {
        use_threads: false,
        color_mode: ColorMode::Rgba,
    };

    if let Ok(decoder) = WebpDecoder::new_with_options(&bytes, options)
        && let Some(frame) = decoder.into_iter().next()
    {
        let (width, height) = frame.dimensions();
        let delay_ms = PlaybackClock::normalize_delay_ms(Some(f64::from(frame.timestamp().max(0))));
        return Ok(RawRgbaFrame {
            width,
            height,
            delay_ms,
            rgba: frame.data().to_vec(),
        });
    }

    decode_static_image(path)
}

/// Decodes a static image of any supported format into a single RGBA buffer.
pub(crate) fn decode_static_image(path: &Path) -> Result<RawRgbaFrame, CacheError>
{
    let dyn_img = ImageReader::open(path)
        .map_err(|source| io_err(path, source))?
        .with_guessed_format()
        .map_err(|source| io_err(path, source))?
        .decode()
        .map_err(|err| decode_err(path, err.to_string()))?;

    let rgba = dyn_img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(RawRgbaFrame {
        width,
        height,
        delay_ms: PlaybackClock::normalize_delay_ms(None),
        rgba: rgba.into_raw(),
    })
}

/// Maps `image::Frame` delay ratios and pixel ownership into `RawRgbaFrame`.
fn map_image_frame(
    path: &Path,
    res: image::ImageResult<image::Frame>,
) -> Result<RawRgbaFrame, CacheError>
{
    let frame = res.map_err(|err| decode_err(path, err.to_string()))?;
    let (numer, denom) = frame.delay().numer_denom_ms();
    let delay_opt = (denom != 0).then(|| f64::from(numer) / f64::from(denom));
    let delay_ms = PlaybackClock::normalize_delay_ms(delay_opt);

    let buffer = frame.into_buffer();
    let (width, height) = buffer.dimensions();
    Ok(RawRgbaFrame {
        width,
        height,
        delay_ms,
        rgba: buffer.into_raw(),
    })
}

/// Wraps a source file `std::io::Error` into `CacheError::Io`.
pub(crate) fn io_err(path: &Path, source: std::io::Error) -> CacheError
{
    CacheError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Wraps a decoder failure message into `CacheError::Decode`.
pub(crate) fn decode_err(path: &Path, reason: String) -> CacheError
{
    CacheError::Decode {
        path: path.to_path_buf(),
        reason,
    }
}
