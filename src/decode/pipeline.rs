//! Bounded, order-preserving parallel encode pipeline behind sequential container decoding.

// Ordered frame staging, FIFO work queue, synchronization, paths, and worker scoping.
use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroUsize,
    path::Path,
    sync::{
        Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

// Lanczos3 resizer instance owned exclusively per encode worker thread.
use fast_image_resize::Resizer;

// Frame payload records, encoder helper, sink trait, and domain errors.
use super::{
    pixel::{EncodedFrame, RawRgbaFrame, encode_subsequent_frame},
    stream::FrameStreamSink,
};
use crate::error::CacheError;

// Minimum parallel encode worker count even on low-core CPUs.
const MIN_ENCODE_WORKERS: usize = 1;

// Upper worker cap avoiding allocator contention and preserving UI/compositor CPU headroom.
const MAX_ENCODE_WORKERS: usize = 6;

// Number of CPU threads reserved for sequential container decode, Wayland UI, and compositor work.
const RESERVED_PIPELINE_THREADS: usize = 2;

// Log progress batch interval matching legacy single-threaded decoder cadence.
const PROGRESS_LOG_INTERVAL: usize = 30;

/// Configuration borrowed by the scoped encode pipeline for frames `1..`.
pub(crate) struct PipelineParams<'a>
{
    // Target stored frame width in pixels passed to `encode_subsequent_frame`.
    pub(crate) base_width: u32,

    // Maximum total frames to emit including frame 0 (`0` = unlimited).
    pub(crate) max_frames: usize,

    // Cooperative cancellation signal polled by reader, workers, and sink consumer.
    pub(crate) cancel: &'a AtomicBool,

    // Source file path included in batch debug trace events.
    pub(crate) path: &'a Path,
}

/// Shared synchronization state between the sequential reader, encode workers, and ordered sink.
struct PipelineState
{
    // Unencoded frames `(seq, raw)` queued strictly in decode order `1..`.
    raw_queue: VecDeque<(usize, RawRgbaFrame)>,

    // Out-of-order finished chunks waiting for `next_sink_seq`.
    encoded: BTreeMap<usize, EncodedFrame>,

    // Next expected frame sequence index (`1..`) delivered to `FrameStreamSink::on_frame`.
    next_sink_seq: usize,

    // Total frames queued or staged prior to sink consumption, capping transient RGBA RAM usage.
    in_flight: usize,

    // Set once the reader hits EOF, `max_frames`, cancellation, or an error.
    reader_done: bool,

    // Immediate termination flag triggered on cancellation or first reader/worker/sink failure.
    aborted: bool,

    // First recoverable cache or codec error captured across any pipeline stage.
    error: Option<CacheError>,
}

/// Next action returned to the ordered sink consumer after inspecting shared state.
enum SinkNext
{
    // Next sequential encoded frame ready for `FrameStreamSink::on_frame`.
    Frame(EncodedFrame),

    // Reader completed cleanly and all queued frames drained in order.
    Done,

    // Pipeline stopped early due to external cancellation or fatal stage error.
    Stopped(Option<CacheError>),
}

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

/// Coordinates bounded in-flight queueing, lock discipline, and clean early abort signaling.
struct PipelineCoordinator
{
    // Mutex guarding `raw_queue`, `encoded`, sequence cursor, and termination flags.
    state: Mutex<PipelineState>,

    // Notifies the bounded reader once `in_flight` drops below `max_in_flight`.
    can_push: Condvar,

    // Notifies idle encode workers when raw frames arrive or pipeline terminates.
    has_job: Condvar,

    // Notifies the ordered sink thread when `encoded` receives chunks or reader finishes.
    has_ordered_output: Condvar,

    // Hard cap on concurrent queued raw and waiting encoded frames.
    max_in_flight: usize,
}

impl PipelineCoordinator
{
    /// Creates a pipeline coordinator allowing up to `max_in_flight` buffered frames.
    fn new(max_in_flight: usize) -> Self
    {
        Self {
            state: Mutex::new(PipelineState {
                raw_queue: VecDeque::new(),
                encoded: BTreeMap::new(),
                next_sink_seq: 1,
                in_flight: 0,
                reader_done: false,
                aborted: false,
                error: None,
            }),
            can_push: Condvar::new(),
            has_job: Condvar::new(),
            has_ordered_output: Condvar::new(),
            max_in_flight: max_in_flight.max(1),
        }
    }

    /// Acquires the pipeline mutex cleanly, recovering inner state if another thread panicked.
    fn lock(&self) -> MutexGuard<'_, PipelineState>
    {
        match self.state.lock()
        {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Blocks on `cv`, recovering inner mutex state without panicking on poison.
    fn wait_cv<'a>(
        cv: &Condvar,
        guard: MutexGuard<'a, PipelineState>,
    ) -> MutexGuard<'a, PipelineState>
    {
        match cv.wait(guard)
        {
            Ok(next) => next,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Wakes all waiting reader, worker, and sink threads after abort or error transitions.
    fn notify_all_stages(&self)
    {
        self.can_push.notify_all();
        self.has_job.notify_all();
        self.has_ordered_output.notify_all();
    }

    /// Records the first error if unset, aborts remaining work, and wakes every stage.
    fn fail(&self, err: CacheError)
    {
        {
            let mut guard = self.lock();
            if guard.error.is_none()
            {
                guard.error = Some(err);
            }
            guard.aborted = true;
        }
        self.notify_all_stages();
    }

    /// Marks the pipeline aborted (e.g. external cancel) and wakes every waiting thread.
    fn abort(&self)
    {
        {
            let mut guard = self.lock();
            guard.aborted = true;
        }
        self.notify_all_stages();
    }

    /// Reports whether any stage has aborted processing.
    fn is_aborted(&self) -> bool
    {
        self.lock().aborted
    }

    /// Marks sequential container reading complete and wakes workers plus the ordered sink.
    fn finish_reader(&self)
    {
        {
            let mut guard = self.lock();
            guard.reader_done = true;
        }
        self.has_job.notify_all();
        self.has_ordered_output.notify_all();
    }

    /// Enqueues raw frame `seq`, blocking on backpressure when `in_flight` hits `max_in_flight`.
    fn push_raw(&self, seq: usize, raw: RawRgbaFrame, cancel: &AtomicBool) -> bool
    {
        let mut guard = self.lock();
        while guard.in_flight >= self.max_in_flight
            && !guard.aborted
            && !cancel.load(Ordering::Acquire)
        {
            guard = Self::wait_cv(&self.can_push, guard);
        }

        if guard.aborted || cancel.load(Ordering::Acquire)
        {
            guard.aborted = true;
            drop(guard);
            self.notify_all_stages();
            return false;
        }

        guard.raw_queue.push_back((seq, raw));
        guard.in_flight += 1;
        drop(guard);
        self.has_job.notify_one();
        true
    }

    /// Pops the next queued raw frame for worker encoding, or returns `None` on completion/abort.
    fn pop_raw(&self, cancel: &AtomicBool) -> Option<(usize, RawRgbaFrame)>
    {
        let mut guard = self.lock();
        while guard.raw_queue.is_empty()
            && !guard.aborted
            && !guard.reader_done
            && !cancel.load(Ordering::Acquire)
        {
            guard = Self::wait_cv(&self.has_job, guard);
        }

        if guard.aborted || cancel.load(Ordering::Acquire)
        {
            guard.aborted = true;
            drop(guard);
            self.notify_all_stages();
            return None;
        }

        guard.raw_queue.pop_front()
    }

    /// Stores encoded frame `seq` in the reorder buffer and notifies the sink consumer.
    fn push_encoded(&self, seq: usize, encoded: EncodedFrame, cancel: &AtomicBool)
    {
        if cancel.load(Ordering::Acquire)
        {
            self.abort();
            return;
        }
        let mut guard = self.lock();
        if guard.aborted
        {
            return;
        }
        guard.encoded.insert(seq, encoded);
        drop(guard);
        self.has_ordered_output.notify_one();
    }

    /// Pops the next sequential encoded frame, signals backpressure relief, or stops cleanly.
    fn pop_next_ordered(&self, cancel: &AtomicBool) -> SinkNext
    {
        let mut guard = self.lock();
        loop
        {
            if guard.aborted || cancel.load(Ordering::Acquire)
            {
                guard.aborted = true;
                let err = guard.error.take();
                drop(guard);
                self.notify_all_stages();
                return SinkNext::Stopped(err);
            }

            let target = guard.next_sink_seq;
            if let Some(frame) = guard.encoded.remove(&target)
            {
                guard.next_sink_seq = target + 1;
                guard.in_flight = guard.in_flight.saturating_sub(1);
                drop(guard);
                self.can_push.notify_one();
                return SinkNext::Frame(frame);
            }

            if guard.reader_done && guard.in_flight == 0
            {
                return SinkNext::Done;
            }

            guard = Self::wait_cv(&self.has_ordered_output, guard);
        }
    }
}

/// Computes default encode worker thread count bounded to preserve UI and compositor headroom.
pub(crate) fn default_encode_workers() -> usize
{
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(RESERVED_PIPELINE_THREADS)
        .saturating_sub(RESERVED_PIPELINE_THREADS)
        .clamp(MIN_ENCODE_WORKERS, MAX_ENCODE_WORKERS)
}

/// Decodes frames `1..` from `iter` through a bounded parallel pool and delivers them in order.
pub(crate) fn stream_remaining_frames<I>(
    iter: I,
    params: PipelineParams<'_>,
    sink: Box<dyn FrameStreamSink>,
) where
    I: Iterator<Item = Result<RawRgbaFrame, CacheError>>,
{
    let mut workers = default_encode_workers();
    if params.max_frames > 1
    {
        workers = workers.min(params.max_frames - 1);
    }

    let coordinator = PipelineCoordinator::new(workers.saturating_mul(2));
    let coord = &coordinator;
    let base_width = params.base_width;
    let cancel = params.cancel;

    let sink_outcome = thread::scope(|scope| {
        for _ in 0..workers
        {
            scope.spawn(move || run_encode_worker(coord, base_width, cancel));
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
        if count.is_multiple_of(PROGRESS_LOG_INTERVAL)
        {
            tracing::debug!(
                frames_decoded = count,
                path = %params.path.display(),
                "Decoded animation batch"
            );
        }
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

/// Resizes, premultiplies, and LZ4-compresses raw frames with a reusable worker `Resizer`.
fn run_encode_worker(coordinator: &PipelineCoordinator, base_width: u32, cancel: &AtomicBool)
{
    let mut resizer = Resizer::new();
    while let Some((seq, raw)) = coordinator.pop_raw(cancel)
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
