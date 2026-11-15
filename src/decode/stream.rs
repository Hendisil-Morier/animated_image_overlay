//! Synchronous frame-0 decoding and cancellable background animation worker streaming.

// Filesystem reads, atomic cancellation signals, channels, and worker threads.
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

// Lanczos3 resizer instantiated per worker thread.
use fast_image_resize::Resizer;
// Animated and static WebP decoding backed by `libwebp`.
use webp_animation::{ColorMode, Decoder as WebpDecoder, DecoderOptions as WebpOptions};

// Codec open helpers, frame encoding helpers, and domain errors.
use super::{
    codecs::{
        FrameIterator, SourceKind, decode_err, decode_single_frame_sync, decode_static_image,
        detect_source_kind, io_err, open_apng_iter, open_gif_iter,
    },
    pixel::{
        DecodedFirstFrame, EncodedFrame, RawRgbaFrame, encode_first_frame, encode_subsequent_frame,
    },
};
use crate::{
    error::{CacheError, OverlayError},
    playback::PlaybackClock,
};

/// Consumer hook receiving background-decoded frames and final stream status.
pub trait FrameStreamSink: Send + 'static
{
    /// Invoked on frame 0 before subsequent frames stream so disk staging can open cleanly.
    fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>;

    /// Invoked once per subsequent frame (`1..`) encoded by the background worker.
    fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>;

    /// Invoked once when decoding ends cleanly, errors out, or aborts via cancellation.
    fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>);
}

/// Coordinates synchronous frame-0 extraction and background animation streaming.
pub struct FrameDecoder;

impl FrameDecoder
{
    /// Decodes frame 0 immediately and streams any remaining frames through `sink`.
    ///
    /// Static images and `--max-frames 1` launches complete synchronously (`worker == None`).
    /// Multi-frame animations open the decoder on a dedicated worker thread, send frame 0
    /// over a bounded channel, and stream remaining frames until completion or `cancel`.
    pub fn open_stream<S: FrameStreamSink>(
        path: &Path,
        base_width: u32,
        max_frames: usize,
        cancel: Arc<AtomicBool>,
        mut sink: Box<S>,
    ) -> Result<(DecodedFirstFrame, Option<JoinHandle<()>>), OverlayError>
    {
        if cancel.load(Ordering::Acquire)
        {
            return Err(CacheError::Cancelled.into());
        }

        let target_width = base_width.max(1);
        let source_kind = detect_source_kind(path)?;

        if source_kind == SourceKind::Static || max_frames == 1
        {
            let first = decode_single_frame_sync(path, source_kind, target_width)?;
            let first_result = sink.on_first_frame(&first);
            sink.on_finish(cancel.load(Ordering::Acquire), first_result);
            return Ok((first, None));
        }

        let (first_tx, first_rx) = mpsc::sync_channel::<Result<DecodedFirstFrame, CacheError>>(1);
        let worker_path = path.to_path_buf();

        let handle = thread::Builder::new()
            .name(String::from("aiw-decoder"))
            .spawn(move || {
                run_worker_decoder(
                    &worker_path,
                    source_kind,
                    target_width,
                    max_frames,
                    &cancel,
                    &first_tx,
                    sink,
                );
            })
            .map_err(|source| io_err(path, source))?;

        let first = match first_rx.recv()
        {
            Ok(Ok(frame)) => frame,
            Ok(Err(err)) =>
            {
                let _ = handle.join();
                return Err(err.into());
            }
            Err(_) =>
            {
                let _ = handle.join();
                return Err(decode_err(
                    path,
                    String::from("Decoder worker exited before yielding frame 0"),
                )
                .into());
            }
        };

        Ok((first, Some(handle)))
    }
}

/// Executes animated format decoding on the background thread and notifies caller + `sink`.
fn run_worker_decoder(
    path: &Path,
    kind: SourceKind,
    base_width: u32,
    max_frames: usize,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    sink: Box<dyn FrameStreamSink>,
)
{
    match kind
    {
        SourceKind::Apng => stream_iter_result(
            path,
            open_apng_iter(path),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
        SourceKind::Gif => stream_iter_result(
            path,
            open_gif_iter(path),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
        SourceKind::Webp => run_webp_worker(path, base_width, max_frames, cancel, first_tx, sink),
        SourceKind::Static =>
        {
            finish_static_worker(path, base_width, cancel, first_tx, sink);
        }
    }
}

/// Streams from an opened container iterator or reports open failure cleanly.
fn stream_iter_result<'a>(
    path: &Path,
    iter_res: Result<FrameIterator<'a>, CacheError>,
    base_width: u32,
    max_frames: usize,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    sink: Box<dyn FrameStreamSink>,
)
{
    match iter_res
    {
        Ok(iter) =>
        {
            stream_iter_frames(path, iter, base_width, max_frames, cancel, first_tx, sink);
        }
        Err(err) =>
        {
            let _ = first_tx.send(Err(err));
            sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
        }
    }
}

/// Decodes a static single frame inside the background worker (used directly and as WebP fallback).
fn finish_static_worker(
    path: &Path,
    base_width: u32,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    mut sink: Box<dyn FrameStreamSink>,
)
{
    match decode_static_image(path)
        .and_then(|raw| encode_first_frame(raw, base_width, &mut Resizer::new()))
    {
        Ok(first) =>
        {
            let cache_res = sink.on_first_frame(&first);
            let _ = first_tx.send(Ok(first));
            sink.on_finish(cancel.load(Ordering::Acquire), cache_res);
        }
        Err(err) =>
        {
            let _ = first_tx.send(Err(err));
            sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
        }
    }
}

/// Decodes a WebP bitstream with `webp_animation`, falling back to `image` if 4K limit hits.
fn run_webp_worker(
    path: &Path,
    base_width: u32,
    max_frames: usize,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    sink: Box<dyn FrameStreamSink>,
)
{
    let bytes = match fs::read(path)
    {
        Ok(data) => data,
        Err(source) =>
        {
            let _ = first_tx.send(Err(io_err(path, source)));
            sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
            return;
        }
    };

    let options = WebpOptions {
        use_threads: true,
        color_mode: ColorMode::Rgba,
    };

    match WebpDecoder::new_with_options(&bytes, options)
    {
        Ok(decoder) =>
        {
            let mut prev_timestamp = 0_i32;
            let iter = decoder.into_iter().map(move |frame| {
                let (width, height) = frame.dimensions();
                let delta = (frame.timestamp() - prev_timestamp).max(0);
                prev_timestamp = frame.timestamp();
                let delay_ms = PlaybackClock::normalize_delay_ms(Some(f64::from(delta)));
                Ok(RawRgbaFrame {
                    width,
                    height,
                    delay_ms,
                    rgba: frame.data().to_vec(),
                })
            });
            stream_iter_frames(path, iter, base_width, max_frames, cancel, first_tx, sink);
        }
        Err(_) => finish_static_worker(path, base_width, cancel, first_tx, sink),
    }
}

/// Streams frame 0 and remaining frames uniformly across APNG, GIF, and WebP iterators.
fn stream_iter_frames<I>(
    path: &Path,
    mut iter: I,
    base_width: u32,
    max_frames: usize,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    mut sink: Box<dyn FrameStreamSink>,
) where
    I: Iterator<Item = Result<RawRgbaFrame, CacheError>>,
{
    let mut resizer = Resizer::new();
    let Some(first_raw) = pop_first_frame(path, iter.next(), first_tx)
    else
    {
        sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
        return;
    };

    let first = match encode_first_frame(first_raw, base_width, &mut resizer)
    {
        Ok(frame) => frame,
        Err(err) =>
        {
            let _ = first_tx.send(Err(err));
            sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
            return;
        }
    };

    let mut sink_result = sink.on_first_frame(&first);
    let _ = first_tx.send(Ok(first));
    if sink_result.is_err()
    {
        sink.on_finish(cancel.load(Ordering::Acquire), sink_result);
        return;
    }

    let mut count = 1_usize;
    for next_raw in iter
    {
        if cancel.load(Ordering::Acquire) || (max_frames > 0 && count >= max_frames)
        {
            break;
        }

        let step = next_raw
            .and_then(|raw| encode_subsequent_frame(raw, base_width, &mut resizer))
            .and_then(|encoded| sink.on_frame(encoded));
        if let Err(err) = step
        {
            sink_result = Err(err);
            break;
        }

        count += 1;
        if count.is_multiple_of(30)
        {
            tracing::debug!(frames_decoded = count, path = %path.display(), "Decoded animation batch");
        }
    }

    sink.on_finish(cancel.load(Ordering::Acquire), sink_result);
}

/// Extracts the initial raw frame from an iterator or sends startup error to `first_tx`.
fn pop_first_frame(
    path: &Path,
    item: Option<Result<RawRgbaFrame, CacheError>>,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
) -> Option<RawRgbaFrame>
{
    match item
    {
        Some(Ok(frame)) => Some(frame),
        Some(Err(err)) =>
        {
            let _ = first_tx.send(Err(err));
            None
        }
        None =>
        {
            let _ = first_tx.send(Err(decode_err(
                path,
                String::from("Animation stream yielded zero frames"),
            )));
            None
        }
    }
}
