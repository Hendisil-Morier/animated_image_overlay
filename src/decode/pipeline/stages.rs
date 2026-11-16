//! Scoped thread runners for sequential reading, parallel inflation/encoding, compositing, and sinks.

// Worker thread counts, file paths, atomic cancellation, and scoped thread synchronization.
use std::{
    num::NonZeroUsize,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

// Lanczos3 resizer instance owned exclusively per encode worker thread.
use fast_image_resize::Resizer;

// Pipeline coordinator primitives, APNG splitter/compositor, pixel encoder, and stream sink.
use super::coordinator::{PatchNext, PipelineCoordinator, PipelineParams, SinkNext, WorkerJob};
use crate::{
    decode::{
        apng::{ApngCompositor, ApngHeader, ApngPacketReader, decode_apng_packet},
        pixel::{RawRgbaFrame, encode_subsequent_frame},
        stream::FrameStreamSink,
    },
    error::CacheError,
};

// Minimum parallel encode worker count even on low-core CPUs.
const MIN_ENCODE_WORKERS: usize = 1;

// Upper worker cap avoiding allocator contention and preserving UI/compositor CPU headroom.
const MAX_ENCODE_WORKERS: usize = 6;

// Number of CPU threads reserved for sequential container decode, Wayland UI, and compositor work.
const RESERVED_PIPELINE_THREADS: usize = 2;

// Log progress batch interval matching legacy single-threaded decoder cadence.
const PROGRESS_LOG_INTERVAL: usize = 30;

/// Ordered sink consumer result returned cleanly across `thread::scope` boundaries.
struct SinkOutcome
{
    // Sink instance returned so `on_finish` runs after all scoped workers exit.
    sink: Box<dyn FrameStreamSink>,

    // Snapshot of cooperative cancellation flag when sink loop stopped.
    cancelled: bool,

    // Terminal decode, encode, or sink error status delivered to `on_finish`.
    result: Result<(), CacheError>,
}

/// Computes default encode worker thread count bounded to preserve UI and compositor headroom.
fn default_encode_workers() -> usize
{
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(RESERVED_PIPELINE_THREADS)
        .saturating_sub(RESERVED_PIPELINE_THREADS)
        .clamp(MIN_ENCODE_WORKERS, MAX_ENCODE_WORKERS)
}

/// Clamps parallel worker count when `max_frames` caps the remaining animation length.
fn effective_workers(max_frames: usize) -> usize
{
    let mut workers = default_encode_workers();
    if max_frames > 1
    {
        workers = workers.min(max_frames - 1);
    }
    workers
}

/// Emits periodic decode-progress trace logs matching legacy worker cadence.
fn log_batch_progress(count: usize, path: &Path)
{
    if count.is_multiple_of(PROGRESS_LOG_INTERVAL)
    {
        tracing::debug!(
            frames_decoded = count,
            path = %path.display(),
            "Decoded animation batch"
        );
    }
}

/// Decodes frames `1..` from `iter` through a bounded parallel pool and delivers them in order.
pub(crate) fn stream_remaining_frames<I>(
    iter: I,
    params: PipelineParams<'_>,
    sink: Box<dyn FrameStreamSink>,
) where
    I: Iterator<Item = Result<RawRgbaFrame, CacheError>>,
{
    let workers = effective_workers(params.max_frames);
    let coordinator = PipelineCoordinator::new(workers.saturating_mul(2), false);
    let coord = &coordinator;
    let base_width = params.base_width;
    let cancel = params.cancel;

    let sink_outcome = thread::scope(|scope| {
        for _ in 0..workers
        {
            scope.spawn(move || run_pool_worker(coord, base_width, cancel, None));
        }

        let sink_handle = scope.spawn(move || run_ordered_sink(coord, cancel, sink));

        run_sequential_reader(coord, iter, &params);
        sink_handle.join().ok()
    });

    if let Some(outcome) = sink_outcome
    {
        outcome.sink.on_finish(outcome.cancelled, outcome.result);
    }
}

/// Streams APNG frames `1..` through parallel PNG inflation, ordered compositing, and parallel LZ4 encode.
pub(crate) fn stream_apng_remaining(
    mut reader: ApngPacketReader,
    header: ApngHeader,
    compositor: ApngCompositor,
    params: PipelineParams<'_>,
    sink: Box<dyn FrameStreamSink>,
)
{
    let workers = effective_workers(params.max_frames);
    let coordinator = PipelineCoordinator::new(workers.saturating_mul(2), true);
    let coord = &coordinator;
    let base_width = params.base_width;
    let cancel = params.cancel;
    let apng_header = &header;
    let path = params.path;

    let sink_outcome = thread::scope(|scope| {
        for _ in 0..workers
        {
            scope.spawn(move || {
                run_pool_worker(coord, base_width, cancel, Some((path, apng_header)));
            });
        }

        scope.spawn(move || run_apng_compositor(coord, compositor, path, cancel));
        let sink_handle = scope.spawn(move || run_ordered_sink(coord, cancel, sink));

        run_apng_packet_reader(coord, &mut reader, &params);
        sink_handle.join().ok()
    });

    if let Some(outcome) = sink_outcome
    {
        outcome.sink.on_finish(outcome.cancelled, outcome.result);
    }
}

/// Pulls decoded source frames sequentially from `iter` until EOF, `max_frames`, cancel, or error.
fn run_sequential_reader<I>(coordinator: &PipelineCoordinator, iter: I, params: &PipelineParams<'_>)
where
    I: Iterator<Item = Result<RawRgbaFrame, CacheError>>,
{
    let mut count = 1_usize;
    for next_raw in iter
    {
        if params.cancel.load(Ordering::Acquire)
            || (params.max_frames > 0 && count >= params.max_frames)
            || coordinator.is_aborted()
        {
            break;
        }

        match next_raw
        {
            Ok(raw) =>
            {
                if !coordinator.push_raw(count, raw, params.cancel)
                {
                    break;
                }
            }
            Err(err) =>
            {
                coordinator.fail(err);
                return;
            }
        }

        count += 1;
        log_batch_progress(count, params.path);
    }

    if params.cancel.load(Ordering::Acquire)
    {
        coordinator.abort();
    }
    else
    {
        coordinator.finish_reader();
    }
}

/// Reads compressed APNG chunks sequentially until EOF, `max_frames`, cancel, or error.
fn run_apng_packet_reader(
    coordinator: &PipelineCoordinator,
    reader: &mut ApngPacketReader,
    params: &PipelineParams<'_>,
)
{
    let mut count = 1_usize;
    loop
    {
        if params.cancel.load(Ordering::Acquire)
            || (params.max_frames > 0 && count >= params.max_frames)
            || coordinator.is_aborted()
        {
            break;
        }

        match reader.next_packet()
        {
            Ok(Some(packet)) =>
            {
                if !coordinator.push_packet(packet, params.cancel)
                {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) =>
            {
                coordinator.fail(err);
                return;
            }
        }

        count += 1;
    }

    if params.cancel.load(Ordering::Acquire)
    {
        coordinator.abort();
    }
    else
    {
        coordinator.finish_packet_reader();
    }
}

/// Blends and disposes inflated APNG patches strictly in sequence order before parallel encoding.
fn run_apng_compositor(
    coordinator: &PipelineCoordinator,
    mut compositor: ApngCompositor,
    path: &Path,
    cancel: &AtomicBool,
)
{
    let mut count = 1_usize;
    loop
    {
        let patch = match coordinator.pop_next_decoded_patch(cancel)
        {
            PatchNext::Patch(patch) => patch,
            PatchNext::Done =>
            {
                coordinator.finish_reader();
                return;
            }
            PatchNext::Stopped => return,
        };

        let seq = patch.seq;
        let raw = match compositor.apply_patch(patch)
        {
            Ok(frame) => frame,
            Err(err) =>
            {
                coordinator.fail(err);
                return;
            }
        };

        if !coordinator.push_raw(seq, raw, cancel)
        {
            return;
        }

        count += 1;
        log_batch_progress(count, path);
    }
}

/// Executes parallel APNG chunk inflation and RGBA-to-LZ4 compression on one scoped worker.
fn run_pool_worker(
    coordinator: &PipelineCoordinator,
    base_width: u32,
    cancel: &AtomicBool,
    apng_ctx: Option<(&Path, &ApngHeader)>,
)
{
    let mut resizer = Resizer::new();
    while let Some(job) = coordinator.pop_worker_job(cancel)
    {
        match job
        {
            WorkerJob::Encode(seq, raw) =>
            {
                match encode_subsequent_frame(raw, base_width, &mut resizer)
                {
                    Ok(encoded) => coordinator.push_encoded(seq, encoded, cancel),
                    Err(err) =>
                    {
                        coordinator.fail(err);
                        break;
                    }
                }
            }
            WorkerJob::DecodeApng(packet) =>
            {
                let Some((path, header)) = apng_ctx
                else
                {
                    continue;
                };
                match decode_apng_packet(path, header, packet)
                {
                    Ok(patch) => coordinator.push_decoded_patch(patch, cancel),
                    Err(err) =>
                    {
                        coordinator.fail(err);
                        break;
                    }
                }
            }
        }
    }
}

/// Delivers encoded frames to `sink` strictly in sequence order `1..`.
fn run_ordered_sink(
    coordinator: &PipelineCoordinator,
    cancel: &AtomicBool,
    mut sink: Box<dyn FrameStreamSink>,
) -> SinkOutcome
{
    loop
    {
        let encoded = match coordinator.pop_next_ordered(cancel)
        {
            SinkNext::Frame(encoded) => encoded,
            SinkNext::Done =>
            {
                return SinkOutcome {
                    sink,
                    cancelled: cancel.load(Ordering::Acquire),
                    result: Ok(()),
                };
            }
            SinkNext::Stopped(maybe_err) =>
            {
                return SinkOutcome {
                    sink,
                    cancelled: cancel.load(Ordering::Acquire),
                    result: maybe_err.map_or(Ok(()), Err),
                };
            }
        };

        if let Err(err) = sink.on_frame(encoded)
        {
            coordinator.abort();
            return SinkOutcome {
                sink,
                cancelled: cancel.load(Ordering::Acquire),
                result: Err(err),
            };
        }
    }
}
