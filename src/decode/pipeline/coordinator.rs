//! Synchronized pipeline queues, backpressure limits, and cancellation state.

// Ordered frame staging, FIFO work queues, synchronization, paths, and atomic cancellation.
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{
        Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

// APNG patch/packet records and frame pixel types managed across worker stages.
use crate::{
    decode::{
        apng::{DecodedApngPatch, RawApngPacket},
        pixel::{EncodedFrame, RawRgbaFrame},
    },
    error::CacheError,
};

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

/// Shared synchronization state between packet readers, compositors, workers, and ordered sink.
struct PipelineState
{
    // Compressed APNG frame packets (`seq = 1..`) awaiting parallel PNG inflation.
    packet_queue: VecDeque<RawApngPacket>,

    // Out-of-order inflated APNG patches waiting for `next_composite_seq`.
    decoded_patches: BTreeMap<usize, DecodedApngPatch>,

    // Next expected APNG frame sequence index (`1..`) delivered to `ApngCompositor`.
    next_composite_seq: usize,

    // Total APNG packets queued, inflating, or staged in `decoded_patches`.
    packets_in_flight: usize,

    // Set once APNG packet splitting hits EOF/`max_frames` (always `true` for non-APNG streams).
    packet_reader_done: bool,

    // Unencoded full-canvas frames `(seq, raw)` queued strictly in decode/composite order `1..`.
    raw_queue: VecDeque<(usize, RawRgbaFrame)>,

    // Out-of-order finished chunks waiting for `next_sink_seq`.
    encoded: BTreeMap<usize, EncodedFrame>,

    // Next expected frame sequence index (`1..`) delivered to `FrameStreamSink::on_frame`.
    next_sink_seq: usize,

    // Total frames queued or staged prior to sink consumption, capping transient RGBA RAM usage.
    in_flight: usize,

    // Set once raw reading or ordered APNG compositing completes cleanly.
    reader_done: bool,

    // Immediate termination flag triggered on cancellation or first reader/worker/sink failure.
    aborted: bool,

    // First recoverable cache or codec error captured across any pipeline stage.
    error: Option<CacheError>,
}

/// Unit of parallel work claimed by a pool worker (`Encode` takes priority over `DecodeApng`).
pub(super) enum WorkerJob
{
    // Resize, premultiply, and LZ4-compress one composited full-canvas frame.
    Encode(usize, RawRgbaFrame),

    // Inflate compressed APNG frame chunks into one straight-RGBA patch.
    DecodeApng(RawApngPacket),
}

/// Next action returned to the sequential APNG compositor after inspecting shared state.
pub(super) enum PatchNext
{
    // Next sequential inflated APNG patch ready for `ApngCompositor::apply_patch`.
    Patch(DecodedApngPatch),

    // Packet reader completed cleanly and every inflated patch drained in order.
    Done,

    // Pipeline stopped early due to cancellation or fatal stage failure.
    Stopped,
}

/// Next action returned to the ordered sink consumer after inspecting shared state.
pub(super) enum SinkNext
{
    // Next sequential encoded frame ready for `FrameStreamSink::on_frame`.
    Frame(EncodedFrame),

    // Reader completed cleanly and all queued frames drained in order.
    Done,

    // Pipeline stopped early due to external cancellation or fatal stage error.
    Stopped(Option<CacheError>),
}

/// Coordinates bounded in-flight queueing, lock discipline, and clean early abort signaling.
pub(super) struct PipelineCoordinator
{
    // Mutex guarding packet/patch/raw/encoded queues, sequence cursors, and termination flags.
    state: Mutex<PipelineState>,

    // Notifies the APNG packet reader once `packets_in_flight` drops below `max_in_flight`.
    can_push_packet: Condvar,

    // Notifies the sequential APNG compositor when patches arrive or packet splitting completes.
    has_composite_patch: Condvar,

    // Notifies the bounded reader or APNG compositor once `in_flight` drops below `max_in_flight`.
    can_push: Condvar,

    // Notifies idle pool workers when raw frames or APNG packets arrive or stages terminate.
    has_job: Condvar,

    // Notifies the ordered sink thread when `encoded` receives chunks or reader finishes.
    has_ordered_output: Condvar,

    // Hard cap on concurrent in-flight items per bounded queue.
    max_in_flight: usize,
}

impl PipelineCoordinator
{
    /// Creates a pipeline coordinator allowing up to `max_in_flight` buffered frames per queue.
    pub(super) fn new(max_in_flight: usize, apng_mode: bool) -> Self
    {
        Self {
            state: Mutex::new(PipelineState {
                packet_queue: VecDeque::new(),
                decoded_patches: BTreeMap::new(),
                next_composite_seq: 1,
                packets_in_flight: 0,
                packet_reader_done: !apng_mode,
                raw_queue: VecDeque::new(),
                encoded: BTreeMap::new(),
                next_sink_seq: 1,
                in_flight: 0,
                reader_done: false,
                aborted: false,
                error: None,
            }),
            can_push_packet: Condvar::new(),
            has_composite_patch: Condvar::new(),
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

    /// Wakes all waiting reader, compositor, worker, and sink threads after abort or error transitions.
    fn notify_all_stages(&self)
    {
        self.can_push_packet.notify_all();
        self.has_composite_patch.notify_all();
        self.can_push.notify_all();
        self.has_job.notify_all();
        self.has_ordered_output.notify_all();
    }

    /// Records the first error if unset, aborts remaining work, and wakes every stage.
    pub(super) fn fail(&self, err: CacheError)
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
    pub(super) fn abort(&self)
    {
        {
            let mut guard = self.lock();
            guard.aborted = true;
        }
        self.notify_all_stages();
    }

    /// Reports whether any stage has aborted processing.
    pub(super) fn is_aborted(&self) -> bool
    {
        self.lock().aborted
    }

    /// Marks sequential APNG packet splitting complete and wakes compositor and pool workers.
    pub(super) fn finish_packet_reader(&self)
    {
        {
            let mut guard = self.lock();
            guard.packet_reader_done = true;
        }
        self.has_job.notify_all();
        self.has_composite_patch.notify_all();
    }

    /// Marks sequential container reading or APNG compositing complete and wakes workers and sink.
    pub(super) fn finish_reader(&self)
    {
        {
            let mut guard = self.lock();
            guard.reader_done = true;
        }
        self.has_job.notify_all();
        self.has_ordered_output.notify_all();
    }

    /// Enqueues compressed APNG packet `seq`, blocking when `packets_in_flight` hits cap.
    pub(super) fn push_packet(&self, packet: RawApngPacket, cancel: &AtomicBool) -> bool
    {
        let mut guard = self.lock();
        while guard.packets_in_flight >= self.max_in_flight
            && !guard.aborted
            && !cancel.load(Ordering::Acquire)
        {
            guard = Self::wait_cv(&self.can_push_packet, guard);
        }

        if guard.aborted || cancel.load(Ordering::Acquire)
        {
            guard.aborted = true;
            drop(guard);
            self.notify_all_stages();
            return false;
        }

        guard.packet_queue.push_back(packet);
        guard.packets_in_flight += 1;
        drop(guard);
        self.has_job.notify_one();
        true
    }

    /// Stores an inflated APNG patch in the reorder buffer and wakes the sequential compositor.
    pub(super) fn push_decoded_patch(&self, patch: DecodedApngPatch, cancel: &AtomicBool)
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
        guard.decoded_patches.insert(patch.seq, patch);
        drop(guard);
        self.has_composite_patch.notify_one();
    }

    /// Pops the next sequential inflated APNG patch, relieves packet backpressure, or stops cleanly.
    pub(super) fn pop_next_decoded_patch(&self, cancel: &AtomicBool) -> PatchNext
    {
        let mut guard = self.lock();
        loop
        {
            if guard.aborted || cancel.load(Ordering::Acquire)
            {
                guard.aborted = true;
                drop(guard);
                self.notify_all_stages();
                return PatchNext::Stopped;
            }

            let target = guard.next_composite_seq;
            if let Some(patch) = guard.decoded_patches.remove(&target)
            {
                guard.next_composite_seq = target + 1;
                guard.packets_in_flight = guard.packets_in_flight.saturating_sub(1);
                drop(guard);
                self.can_push_packet.notify_one();
                return PatchNext::Patch(patch);
            }

            if guard.packet_reader_done && guard.packets_in_flight == 0
            {
                return PatchNext::Done;
            }

            guard = Self::wait_cv(&self.has_composite_patch, guard);
        }
    }

    /// Enqueues raw frame `seq`, blocking on backpressure when `in_flight` hits `max_in_flight`.
    pub(super) fn push_raw(&self, seq: usize, raw: RawRgbaFrame, cancel: &AtomicBool) -> bool
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

    /// Claims the next worker job (prioritizing raw canvas encoding), or `None` when done/aborted.
    pub(super) fn pop_worker_job(&self, cancel: &AtomicBool) -> Option<WorkerJob>
    {
        let mut guard = self.lock();
        loop
        {
            if guard.aborted || cancel.load(Ordering::Acquire)
            {
                guard.aborted = true;
                drop(guard);
                self.notify_all_stages();
                return None;
            }

            // Drain full composited RGBA frames before inflating more compressed APNG chunks.
            if let Some((seq, raw)) = guard.raw_queue.pop_front()
            {
                return Some(WorkerJob::Encode(seq, raw));
            }

            if let Some(packet) = guard.packet_queue.pop_front()
            {
                return Some(WorkerJob::DecodeApng(packet));
            }

            if guard.reader_done && guard.packet_reader_done
            {
                return None;
            }

            guard = Self::wait_cv(&self.has_job, guard);
        }
    }

    /// Stores encoded frame `seq` in the reorder buffer and notifies the sink consumer.
    pub(super) fn push_encoded(&self, seq: usize, encoded: EncodedFrame, cancel: &AtomicBool)
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
    pub(super) fn pop_next_ordered(&self, cancel: &AtomicBool) -> SinkNext
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
