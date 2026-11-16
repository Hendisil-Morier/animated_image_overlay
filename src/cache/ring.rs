//! Bounded lookahead RAM ring buffer for live-streaming long videos without massive `.lraw` files.

// Bounded ring storage, paths, mutex/condvar coordination, atomic cancellation, and worker threads.
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

// Lanczos3 resizer instance used cleanly when scaling frame buffers if dimensions differ.
use fast_image_resize::Resizer;

// Cache lock helper, wakeup hook, pixel premultiply helper, and video stream runner.
use super::memory::{WakeupCallback, lock_mutex};
use crate::{
    decode::{
        checked_argb_byte_len,
        codecs::io_err,
        pixel::raw_to_premultiplied_argb8888,
        video::{VideoFrameIterator, VideoProbe, open_video_frames},
    },
    error::{CacheError, OverlayError},
};

// Maximum ahead premultiplied `ARGB8888` frames buffered in RAM; full pressure pauses `ffmpeg`.
const LIVE_RING_CAPACITY: usize = 24;

// Startup frame interval waking compositor one-by-one before batching every 4 frames.
const EARLY_WAKEUP_FRAMES: usize = 8;

// Steady-state wakeup stride reducing `calloop` wakeups once buffered playback is underway.
const STEADY_WAKEUP_INTERVAL: usize = 4;

// Timeout per wait slice when awaiting an upcoming pass frame, preserving responsive cancel checks.
const RING_WAIT_SLICE_MS: u64 = 40;

// Maximum time spent waiting for one live frame from `ffmpeg` before returning a retryable miss.
const RING_FETCH_TIMEOUT_MS: u64 = 2_000;

/// One decoded lookahead frame inside the bounded live ring queue.
struct RingEntry
{
    // Zero-based animation index within the loop cycle (`0..total_frames`).
    index: usize,

    // Pre-scaled premultiplied `ARGB8888` buffer ready for SHM upload.
    argb: Arc<Vec<u8>>,
}

/// Synchronized sliding window state shared between background `ffmpeg` reader and Wayland consumer.
struct LiveRingState
{
    // Stored frame pixel width.
    width: u32,

    // Stored frame pixel height.
    height: u32,

    // Exact uncompressed `ARGB8888` byte length of every frame.
    frame_byte_len: usize,

    // Immutable frame 0 kept pinned forever so wrapping `N-1 -> 0` never waits for `ffmpeg`.
    frame0: Arc<Vec<u8>>,

    // Last accessed frame buffer retained for paused rotations/flips without stalling on `ffmpeg`.
    pinned: Option<(usize, Arc<Vec<u8>>)>,

    // Lookahead buffer holding up to `LIVE_RING_CAPACITY` frames across loop seams seamlessly.
    queue: VecDeque<RingEntry>,

    // Accumulated microsecond delays (`0..decoded_known`) exposed to `PlaybackClock`.
    delays_us: Vec<u32>,

    // True once the first full loop pass reached EOF or `max_frames` and future wraps know `0..N`.
    decode_done: bool,

    // Terminal failure flag waking consumer waits cleanly if `ffmpeg` aborts unexpectedly.
    stream_failed: bool,
}

/// Shared interior monitor pairing `LiveRingState` with `Condvar` backpressure wakeups.
struct LiveRingMonitor
{
    // Guarded sliding queue, delays vector, and pinned frames.
    state: Mutex<LiveRingState>,

    // Signals both producer space openings (consumer popped) and consumer frame arrivals.
    changed: Condvar,
}

/// Bounded lookahead live video frame source eliminating multi-gigabyte `.lraw` disk caches.
pub(crate) struct LiveVideoRing
{
    // Shared monitor state accessed on playback ticks and resize/rotate refreshes.
    monitor: Arc<LiveRingMonitor>,
}

impl LiveVideoRing
{
    /// Synchronously decodes frame 0 from `probe` and launches the bounded loop-aware reader thread.
    pub(crate) fn open(
        path: &Path,
        probe: &VideoProbe,
        base_width: u32,
        max_frames: usize,
        cancel: Arc<AtomicBool>,
        wakeup: Option<WakeupCallback>,
    ) -> Result<(Self, Option<JoinHandle<()>>), OverlayError>
    {
        let mut iter = open_video_frames(path, probe, base_width, max_frames)?;
        let raw0 = match iter.next()
        {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Err(err.into()),
            None =>
            {
                return Err(CacheError::Decode {
                    path: path.to_path_buf(),
                    reason: String::from("Video stream yielded zero frames"),
                }
                .into());
            }
        };

        let mut resizer = Resizer::new();
        let (width, height, delay0_us, argb0) =
            raw_to_premultiplied_argb8888(raw0, base_width, &mut resizer)?;
        let frame_byte_len = checked_argb_byte_len(width, height)?;
        let frame0 = Arc::new(argb0);

        let single_frame = max_frames == 1;
        let monitor = Arc::new(LiveRingMonitor {
            state: Mutex::new(LiveRingState {
                width,
                height,
                frame_byte_len,
                frame0: Arc::clone(&frame0),
                pinned: Some((0, Arc::clone(&frame0))),
                queue: VecDeque::with_capacity(LIVE_RING_CAPACITY),
                delays_us: vec![delay0_us],
                decode_done: single_frame,
                stream_failed: false,
            }),
            changed: Condvar::new(),
        });

        if single_frame
        {
            return Ok((Self { monitor }, None));
        }

        let worker_path = path.to_path_buf();
        let worker_probe = probe.clone();
        let worker_monitor = Arc::clone(&monitor);
        let handle = thread::Builder::new()
            .name(String::from("aiw-video-ring"))
            .spawn(move || {
                run_ring_producer(
                    &worker_path,
                    &worker_probe,
                    base_width,
                    max_frames,
                    iter,
                    &worker_monitor,
                    &cancel,
                    wakeup,
                );
            })
            .map_err(|source| io_err(path, source))?;

        Ok((Self { monitor }, Some(handle)))
    }

    /// Returns stored pixel `(width, height)` for this video stream.
    pub(crate) fn frame_dimensions(&self) -> (u32, u32)
    {
        let guard = lock_mutex(&self.monitor.state);
        (guard.width, guard.height)
    }

    /// Invokes `visit` with borrowed per-frame delays and loop-completion flag cleanly.
    pub(crate) fn with_delays_us<R>(&self, visit: impl FnOnce(&[u32], bool) -> R) -> R
    {
        let guard = lock_mutex(&self.monitor.state);
        visit(&guard.delays_us, guard.decode_done)
    }

    /// Returns count of known loop frames accumulated cleanly in pass 1.
    pub(crate) fn frame_count(&self) -> usize
    {
        lock_mutex(&self.monitor.state).delays_us.len()
    }

    /// Returns `true` once pass 1 finished probing all frame delays cleanly.
    pub(crate) fn is_decode_done(&self) -> bool
    {
        lock_mutex(&self.monitor.state).decode_done
    }

    /// Returns current heap RAM occupied by pinned frames and the lookahead queue in bytes.
    pub(crate) fn ram_used_bytes(&self) -> usize
    {
        let guard = lock_mutex(&self.monitor.state);
        let pinned_slots = usize::from(guard.pinned.is_some());
        (1 + pinned_slots + guard.queue.len()) * guard.frame_byte_len
    }

    /// Wakes producer and consumer waits immediately during overlay shutdown.
    pub(crate) fn wake_all(&self)
    {
        self.monitor.changed.notify_all();
    }

    /// Retrieves premultiplied `ARGB8888` pixels for `index`, draining older consumed frames cleanly.
    pub(crate) fn with_frame_argb<R>(
        &self,
        cache_dir: &Path,
        index: usize,
        cancel: &AtomicBool,
        visit: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, OverlayError>
    {
        let arc = self.fetch_frame_arc(cache_dir, index, cancel)?;
        Ok(visit(&arc))
    }

    /// Resolves `index` from `pinned`, `frame0`, or the ahead sliding `queue`.
    fn fetch_frame_arc(
        &self,
        cache_dir: &Path,
        index: usize,
        cancel: &AtomicBool,
    ) -> Result<Arc<Vec<u8>>, CacheError>
    {
        let mut waited_ms = 0_u64;
        let mut guard = lock_mutex(&self.monitor.state);

        loop
        {
            if let Some((p_idx, p_buf)) = &guard.pinned
                && *p_idx == index
            {
                return Ok(Arc::clone(p_buf));
            }

            if let Some(pos) = guard.queue.iter().position(|entry| entry.index == index)
            {
                for _ in 0..pos
                {
                    let _ = guard.queue.pop_front();
                }
                if let Some(entry) = guard.queue.pop_front()
                {
                    guard.pinned = Some((index, Arc::clone(&entry.argb)));
                    self.monitor.changed.notify_all();
                    return Ok(entry.argb);
                }
            }

            // `frame0` is always resident so wrapping to frame 0 succeeds even ahead of Pass 2 prefetch.
            if index == 0
            {
                let frame0 = Arc::clone(&guard.frame0);
                guard.pinned = Some((0, Arc::clone(&frame0)));
                return Ok(frame0);
            }

            if index >= guard.delays_us.len()
            {
                return Err(CacheError::CorruptMetadata {
                    path: cache_dir.to_path_buf(),
                    reason: format!("Live video frame index {index} out of bounds"),
                });
            }

            if guard.stream_failed
                || cancel.load(Ordering::Acquire)
                || waited_ms >= RING_FETCH_TIMEOUT_MS
            {
                return Err(CacheError::CorruptMetadata {
                    path: cache_dir.to_path_buf(),
                    reason: format!("Live video frame index {index} not buffered in ring"),
                });
            }

            guard = wait_slice(&self.monitor.changed, guard);
            waited_ms = waited_ms.saturating_add(RING_WAIT_SLICE_MS);
        }
    }
}

/// Continuously streams pass 1..N frames into `monitor.queue` under bounded lookahead backpressure.
#[expect(clippy::too_many_arguments)]
fn run_ring_producer(
    path: &Path,
    probe: &VideoProbe,
    base_width: u32,
    max_frames: usize,
    first_iter: VideoFrameIterator,
    monitor: &LiveRingMonitor,
    cancel: &AtomicBool,
    wakeup: Option<WakeupCallback>,
)
{
    let mut resizer = Resizer::new();
    let mut pass_done = push_pass_frames(
        path,
        1,
        true,
        first_iter,
        base_width,
        &mut resizer,
        monitor,
        cancel,
        wakeup.as_ref(),
    );

    while pass_done && !cancel.load(Ordering::Acquire)
    {
        if lock_mutex(&monitor.state).delays_us.len() <= 1
        {
            break;
        }

        if !wait_for_queue_space(monitor, cancel)
        {
            break;
        }

        let Ok(mut next_iter) = open_video_frames(path, probe, base_width, max_frames)
        else
        {
            mark_stream_failed(monitor, wakeup.as_ref());
            break;
        };

        // Consume pass-start frame 0 cleanly so loop seam `(0, frame0)` stays aligned with queue order.
        match next_iter.next()
        {
            Some(Ok(_)) =>
            {}
            _ =>
            {
                mark_stream_failed(monitor, wakeup.as_ref());
                break;
            }
        }

        let frame0 = lock_mutex(&monitor.state).frame0.clone();
        if !push_ring_entry(
            monitor,
            RingEntry {
                index: 0,
                argb: frame0,
            },
            None,
            cancel,
            wakeup.as_ref(),
            false,
        )
        {
            break;
        }

        pass_done = push_pass_frames(
            path,
            1,
            false,
            next_iter,
            base_width,
            &mut resizer,
            monitor,
            cancel,
            wakeup.as_ref(),
        );
    }
}

/// Streams frames `start_idx..` from `iter` into `monitor`, returning whether the pass reached clean EOF.
#[expect(clippy::too_many_arguments)]
fn push_pass_frames(
    path: &Path,
    start_idx: usize,
    is_first_pass: bool,
    iter: VideoFrameIterator,
    base_width: u32,
    resizer: &mut Resizer,
    monitor: &LiveRingMonitor,
    cancel: &AtomicBool,
    wakeup: Option<&WakeupCallback>,
) -> bool
{
    for (idx, raw_res) in (start_idx..).zip(iter)
    {
        if cancel.load(Ordering::Acquire)
        {
            return false;
        }

        let raw = match raw_res
        {
            Ok(frame) => frame,
            Err(err) =>
            {
                tracing::warn!(path = %path.display(), error = %err, "Live video stream decode failed");
                mark_stream_failed(monitor, wakeup);
                return false;
            }
        };

        let Ok((_, _, delay_us, argb)) = raw_to_premultiplied_argb8888(raw, base_width, resizer)
        else
        {
            mark_stream_failed(monitor, wakeup);
            return false;
        };

        let notify_ui = idx <= EARLY_WAKEUP_FRAMES || idx.is_multiple_of(STEADY_WAKEUP_INTERVAL);
        let delay_update = is_first_pass.then_some(delay_us);
        if !push_ring_entry(
            monitor,
            RingEntry {
                index: idx,
                argb: Arc::new(argb),
            },
            delay_update,
            cancel,
            wakeup,
            notify_ui,
        )
        {
            return false;
        }
    }

    if is_first_pass
    {
        let mut guard = lock_mutex(&monitor.state);
        guard.decode_done = true;
        monitor.changed.notify_all();
        drop(guard);
        if let Some(cb) = wakeup
        {
            cb();
        }
    }

    true
}

/// Waits until `queue.len() < LIVE_RING_CAPACITY` or cancellation triggers.
fn wait_for_queue_space(monitor: &LiveRingMonitor, cancel: &AtomicBool) -> bool
{
    let mut guard = lock_mutex(&monitor.state);
    while guard.queue.len() >= LIVE_RING_CAPACITY
    {
        if cancel.load(Ordering::Acquire)
        {
            return false;
        }
        guard = wait_slice(&monitor.changed, guard);
    }
    !cancel.load(Ordering::Acquire)
}

/// Inserts one decoded frame into `monitor.queue` once space opens and optionally wakes Wayland.
fn push_ring_entry(
    monitor: &LiveRingMonitor,
    entry: RingEntry,
    first_pass_delay_us: Option<u32>,
    cancel: &AtomicBool,
    wakeup: Option<&WakeupCallback>,
    notify_ui: bool,
) -> bool
{
    let mut guard = lock_mutex(&monitor.state);
    while guard.queue.len() >= LIVE_RING_CAPACITY
    {
        if cancel.load(Ordering::Acquire)
        {
            return false;
        }
        guard = wait_slice(&monitor.changed, guard);
    }
    if cancel.load(Ordering::Acquire)
    {
        return false;
    }

    guard.queue.push_back(entry);
    if let Some(delay_us) = first_pass_delay_us
    {
        guard.delays_us.push(delay_us);
    }
    let urgent = notify_ui || guard.queue.len() <= 2;
    monitor.changed.notify_all();
    drop(guard);

    if urgent && let Some(cb) = wakeup
    {
        cb();
    }
    true
}

/// Waits one slice duration on `changed` cleanly, recovering lock guards across poison boundaries.
fn wait_slice<'a>(
    changed: &Condvar,
    guard: MutexGuard<'a, LiveRingState>,
) -> MutexGuard<'a, LiveRingState>
{
    match changed.wait_timeout(guard, Duration::from_millis(RING_WAIT_SLICE_MS))
    {
        Ok((next, _)) => next,
        Err(poisoned) => poisoned.into_inner().0,
    }
}

/// Marks background live stream failure cleanly and unblocks waiting consumers.
fn mark_stream_failed(monitor: &LiveRingMonitor, wakeup: Option<&WakeupCallback>)
{
    let mut guard = lock_mutex(&monitor.state);
    guard.decode_done = true;
    guard.stream_failed = true;
    monitor.changed.notify_all();
    drop(guard);
    if let Some(cb) = wakeup
    {
        cb();
    }
}
