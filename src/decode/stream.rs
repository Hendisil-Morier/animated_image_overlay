//! Synchronous frame-0 decoding and cancellable background animation worker streaming.

// Atomic cancellation signals, channels, single-item iterators, paths, and worker threads.
use std::{
    iter,
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

// Single-open codec handles, ordered parallel pipeline, frame encoding helpers, and domain errors.
use super::{
    codecs::{
        FrameIterator, OpenedSource, apng_decoder_iter, decode_err, decode_image_reader,
        decode_single_frame_sync, decode_static_png, gif_reader_iter, io_err, open_source,
        with_webp_reader_frames,
    },
    pipeline::{PipelineParams, stream_remaining_frames},
    pixel::{DecodedFirstFrame, EncodedFrame, RawRgbaFrame, encode_first_frame},
};
use crate::error::{CacheError, OverlayError};

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
    /// Multi-frame animations reuse the single opened source handle on a dedicated worker thread,
    /// send frame 0 over a bounded channel, and stream remaining frames until completion or `cancel`.
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
        let source = open_source(path)?;

        if source.is_static() || max_frames == 1
        {
            let first = decode_single_frame_sync(path, source, target_width)?;
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
                    source,
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

/// Executes animated format decoding from the single opened handle on the background thread.
fn run_worker_decoder(
    path: &Path,
    source: OpenedSource,
    base_width: u32,
    max_frames: usize,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    sink: Box<dyn FrameStreamSink>,
)
{
    match source
    {
        OpenedSource::Apng(png) => stream_iter_result(
            path,
            apng_decoder_iter(path, *png),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
        OpenedSource::Gif(reader) => stream_iter_result(
            path,
            gif_reader_iter(path, reader),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
        OpenedSource::Webp(reader) => with_webp_reader_frames(path, reader, true, |opened| {
            stream_iter_result(path, opened, base_width, max_frames, cancel, first_tx, sink);
        }),
        OpenedSource::StaticPng(png) => stream_iter_result(
            path,
            Ok(Box::new(iter::once(decode_static_png(path, *png)))),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
        OpenedSource::Static(reader) => stream_iter_result(
            path,
            Ok(Box::new(iter::once(decode_image_reader(path, *reader)))),
            base_width,
            max_frames,
            cancel,
            first_tx,
            sink,
        ),
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
        Err(err) => abort_startup_error(err, cancel, first_tx, sink),
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
    let first = match next_raw_first_frame(path, iter.next())
        .and_then(|raw| encode_first_frame(raw, base_width, &mut resizer))
    {
        Ok(frame) => frame,
        Err(err) =>
        {
            abort_startup_error(err, cancel, first_tx, sink);
            return;
        }
    };

    let sink_result = sink.on_first_frame(&first);
    let _ = first_tx.send(Ok(first));
    if sink_result.is_err() || cancel.load(Ordering::Acquire)
    {
        sink.on_finish(cancel.load(Ordering::Acquire), sink_result);
        return;
    }

    stream_remaining_frames(
        iter,
        PipelineParams {
            base_width,
            max_frames,
            cancel,
            path,
        },
        sink,
    );
}

/// Sends a startup decoding error to the caller and shuts down the sink cleanly.
fn abort_startup_error(
    err: CacheError,
    cancel: &AtomicBool,
    first_tx: &mpsc::SyncSender<Result<DecodedFirstFrame, CacheError>>,
    sink: Box<dyn FrameStreamSink>,
)
{
    let _ = first_tx.send(Err(err));
    sink.on_finish(cancel.load(Ordering::Acquire), Ok(()));
}

/// Normalizes frame-0 iterator output, converting empty streams into `CacheError::Decode`.
fn next_raw_first_frame(
    path: &Path,
    item: Option<Result<RawRgbaFrame, CacheError>>,
) -> Result<RawRgbaFrame, CacheError>
{
    match item
    {
        Some(res) => res,
        None => Err(decode_err(
            path,
            String::from("Animation stream yielded zero frames"),
        )),
    }
}
