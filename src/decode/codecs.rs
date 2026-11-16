//! Image container format detection and single-open codec iterators across APNG, GIF, WebP, and stills.

// Filesystem readers, in-memory cursors, stream reads, and explicit rewind seeks.
use std::{
    fs::File,
    io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
    iter,
    path::Path,
};

// Lanczos resizer reused across synchronous single-frame decode.
use fast_image_resize::Resizer;
// Still and animated codecs (`APNG`, `GIF`, and fallback static readers).
use image::{
    AnimationDecoder, DynamicImage, ImageFormat, ImageReader,
    codecs::{gif::GifDecoder, png::PngDecoder},
};
// Animated and static WebP decoding backed by `libwebp`.
use webp_animation::{ColorMode, Decoder as WebpDecoder, DecoderOptions as WebpOptions};

// Parallel-capable APNG chunk splitter, patch decoder, and sequential canvas compositor.
use super::apng::{ApngCompositor, ApngHeader, ApngPacketReader, decode_apng_packet};
// Shared raw frame struct, cache error mappings, and normalized frame delay clamping.
use super::pixel::{DecodedFirstFrame, RawRgbaFrame, encode_first_frame};
use crate::{error::CacheError, playback::PlaybackClock};

/// Single opened image container reused across startup probing and frame decoding.
pub(crate) enum OpenedSource
{
    /// PNG container verified to carry an `acTL` animation control chunk.
    Apng(Box<ApngPacketReader>),

    /// Static PNG already parsed past header chunks without an `acTL` sequence.
    StaticPng(Box<PngDecoder<BufReader<File>>>),

    /// Graphics Interchange Format file reader positioned at byte offset 0.
    Gif(BufReader<File>),

    /// WebP file reader positioned at byte offset 0.
    Webp(BufReader<File>),

    /// Static still image reader retaining its guessed format and buffered file handle.
    Static(Box<ImageReader<BufReader<File>>>),
}

impl OpenedSource
{
    /// Returns `true` when the container guarantees a single static frame.
    pub(crate) fn is_static(&self) -> bool
    {
        matches!(self, Self::StaticPng(_) | Self::Static(_))
    }
}

/// Boxed iterator yielding decoded straight-RGBA frames and normalized delays.
pub(crate) type FrameIterator<'a> = Box<dyn Iterator<Item = Result<RawRgbaFrame, CacheError>> + 'a>;

/// Opens `path` once, guesses its container format, and prepares a reusable source handle.
pub(crate) fn open_source(path: &Path) -> Result<OpenedSource, CacheError>
{
    let reader = open_guessed_reader(path)?;

    match reader.format()
    {
        Some(ImageFormat::Png) =>
        {
            let buf = rewind_reader(path, reader)?;
            match ApngPacketReader::try_open(path, buf)?
            {
                Ok(apng) => Ok(OpenedSource::Apng(Box::new(apng))),
                Err(static_buf) =>
                {
                    let png = PngDecoder::new(static_buf)
                        .map_err(|err| decode_err(path, err.to_string()))?;
                    Ok(OpenedSource::StaticPng(Box::new(png)))
                }
            }
        }
        Some(ImageFormat::Gif) => Ok(OpenedSource::Gif(rewind_reader(path, reader)?)),
        Some(ImageFormat::WebP) => Ok(OpenedSource::Webp(rewind_reader(path, reader)?)),
        _ => Ok(OpenedSource::Static(Box::new(reader))),
    }
}

/// Synchronously decodes a single frame for static images or `--max-frames 1`.
pub(crate) fn decode_single_frame_sync(
    path: &Path,
    source: OpenedSource,
    base_width: u32,
) -> Result<DecodedFirstFrame, CacheError>
{
    let mut resizer = Resizer::new();
    let raw = match source
    {
        OpenedSource::Apng(mut reader) =>
        {
            let (raw, _, _) = decode_apng_first_frame(path, &mut reader)?;
            Some(Ok(raw))
        }
        OpenedSource::Gif(reader) => gif_reader_iter(path, reader)?.next(),
        OpenedSource::Webp(reader) =>
        {
            with_webp_reader_frames(path, reader, false, |opened| opened.map(|mut it| it.next()))?
        }
        OpenedSource::StaticPng(png) => Some(decode_static_png(path, *png)),
        OpenedSource::Static(reader) => Some(decode_image_reader(path, *reader)),
    };

    let frame =
        raw.ok_or_else(|| decode_err(path, String::from("Image contained zero frames")))??;
    encode_first_frame(frame, base_width, &mut resizer)
}

/// Decodes APNG frame 0 synchronously and prepares the stateful canvas compositor for `1..`.
pub(crate) fn decode_apng_first_frame(
    path: &Path,
    reader: &mut ApngPacketReader,
) -> Result<(RawRgbaFrame, ApngHeader, ApngCompositor), CacheError>
{
    let header = reader.header();
    let packet = reader
        .next_packet()?
        .ok_or_else(|| decode_err(path, String::from("Image contained zero frames")))?;
    let patch = decode_apng_packet(path, &header, packet)?;
    let mut compositor = ApngCompositor::new(header.canvas_w, header.canvas_h)?;
    let raw = compositor.apply_patch(patch)?;
    Ok((raw, header, compositor))
}

/// Builds a streaming GIF frame iterator from an already-opened buffered reader.
pub(crate) fn gif_reader_iter<'a>(
    path: &'a Path,
    reader: BufReader<File>,
) -> Result<FrameIterator<'a>, CacheError>
{
    let gif = GifDecoder::new(reader).map_err(|err| decode_err(path, err.to_string()))?;

    Ok(Box::new(
        gif.into_frames().map(|res| map_image_frame(path, res)),
    ))
}

/// Reads an opened WebP handle once and invokes `f` with a frame iterator, falling back on 4K stills.
pub(crate) fn with_webp_reader_frames<R>(
    path: &Path,
    mut reader: BufReader<File>,
    use_threads: bool,
    f: impl FnOnce(Result<FrameIterator<'_>, CacheError>) -> R,
) -> R
{
    let mut bytes = Vec::new();
    if let Err(source) = reader.read_to_end(&mut bytes)
    {
        return f(Err(io_err(path, source)));
    }

    let options = WebpOptions {
        use_threads,
        color_mode: ColorMode::Rgba,
    };

    let Ok(decoder) = WebpDecoder::new_with_options(&bytes, options)
    else
    {
        let fallback = decode_image_reader(
            path,
            ImageReader::with_format(Cursor::new(&bytes), ImageFormat::WebP),
        );
        return match fallback
        {
            Ok(raw) => f(Ok(Box::new(iter::once(Ok(raw))))),
            Err(err) => f(Err(err)),
        };
    };

    let mut prev_timestamp = 0_i32;
    let iter = decoder.into_iter().map(move |frame| {
        let (width, height) = frame.dimensions();
        let delta = (frame.timestamp() - prev_timestamp).max(0);
        prev_timestamp = frame.timestamp();
        let delay_us = PlaybackClock::normalize_cache_delay_us(Some(f64::from(delta) * 1000.0));
        Ok(RawRgbaFrame {
            width,
            height,
            delay_us,
            rgba: frame.data().to_vec(),
        })
    });

    f(Ok(Box::new(iter)))
}

/// Decodes an already-parsed static `PngDecoder` into a single straight-RGBA frame.
pub(crate) fn decode_static_png(
    path: &Path,
    png: PngDecoder<BufReader<File>>,
) -> Result<RawRgbaFrame, CacheError>
{
    let dyn_img =
        DynamicImage::from_decoder(png).map_err(|err| decode_err(path, err.to_string()))?;
    Ok(dynamic_image_into_raw(dyn_img))
}

/// Decodes a static image from an already-opened buffered `ImageReader`.
pub(crate) fn decode_image_reader<R: BufRead + Seek>(
    path: &Path,
    reader: ImageReader<R>,
) -> Result<RawRgbaFrame, CacheError>
{
    let dyn_img = reader
        .decode()
        .map_err(|err| decode_err(path, err.to_string()))?;
    Ok(dynamic_image_into_raw(dyn_img))
}

/// Converts a decoded `DynamicImage` into tightly packed straight-RGBA pixels with static timing.
fn dynamic_image_into_raw(dyn_img: DynamicImage) -> RawRgbaFrame
{
    let rgba = dyn_img.into_rgba8();
    let (width, height) = rgba.dimensions();
    RawRgbaFrame {
        width,
        height,
        delay_us: PlaybackClock::normalize_cache_delay_us(None),
        rgba: rgba.into_raw(),
    }
}

/// Rewinds an opened `ImageReader` handle back to byte offset 0 before constructing a codec decoder.
fn rewind_reader(
    path: &Path,
    reader: ImageReader<BufReader<File>>,
) -> Result<BufReader<File>, CacheError>
{
    let mut buf = reader.into_inner();
    buf.seek(SeekFrom::Start(0))
        .map_err(|source| io_err(path, source))?;
    Ok(buf)
}

/// Opens a buffered image reader once with format guessing applied from magic bytes and path extension.
fn open_guessed_reader(path: &Path) -> Result<ImageReader<BufReader<File>>, CacheError>
{
    ImageReader::open(path)
        .map_err(|source| io_err(path, source))?
        .with_guessed_format()
        .map_err(|source| io_err(path, source))
}

/// Maps `image::Frame` delay ratios and pixel ownership into `RawRgbaFrame`.
fn map_image_frame(
    path: &Path,
    res: image::ImageResult<image::Frame>,
) -> Result<RawRgbaFrame, CacheError>
{
    let frame = res.map_err(|err| decode_err(path, err.to_string()))?;
    let (numer, denom) = frame.delay().numer_denom_ms();
    let delay_opt = (denom != 0).then(|| (f64::from(numer) * 1000.0) / f64::from(denom));
    let delay_us = PlaybackClock::normalize_cache_delay_us(delay_opt);

    let buffer = frame.into_buffer();
    let (width, height) = buffer.dimensions();
    Ok(RawRgbaFrame {
        width,
        height,
        delay_us,
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
