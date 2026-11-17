//! Public `FrameCache` lifecycle controller bridging disk storage, live video rings, and RAM LRU cache.

// Filesystem setup, synchronization, and worker thread management.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

// Internal cache disk helpers, RAM LRU state, lazy reader, live video ring, and decode sink.
use super::{
    decompress::{PreparedFrame, fetch_compressed_frame, prepare_frame_access},
    disk::{DEFAULT_PRUNE_AGE_DAYS, compute_cache_paths, default_cache_dir, prune_cache},
    memory::{SharedFrames, WakeupCallback, lock_mutex},
    reader::{FrameScratch, try_open_cached_store},
    ring::LiveVideoRing,
    sink::DecodeCacheSink,
};
use crate::{
    decode::{
        FrameDecoder, checked_argb_byte_len,
        codecs::{OpenedSource, open_source},
        video::{DEFAULT_VIDEO_CACHE_BUDGET_BYTES, has_video_signature},
    },
    error::{CacheError, OverlayError},
};

/// Manages source/disk cache loading, lazy chunk streaming, live video rings, and pruning.
pub struct FrameCache
{
    // Directory storing `<hash>.lraw` and `<hash>.json` cache pairs.
    cache_dir: PathBuf,

    // Active storage strategy (`Indexed` `.lraw`/LRU or bounded `LiveRing` for long videos).
    backing: BackingStore,

    // Cancellation signal stopping worker threads cleanly on exit.
    cancel: Arc<AtomicBool>,

    // Join handle for active background source decoding thread.
    worker: Option<JoinHandle<()>>,
}

/// Active frame-backing storage inside `FrameCache`.
enum BackingStore
{
    // Standard indexed chunk store (cold LZ4 in memory / `.lraw` disk + configurable RAM LRU).
    Indexed
    {
        // Synchronized frame metadata, chunk spans, and conditional RAM cache.
        shared: Arc<Mutex<SharedFrames>>,

        // Reusable `.lraw` reader and decompression workspace kept separate from `shared`.
        scratch: Arc<Mutex<FrameScratch>>,
    },

    // Sliding lookahead RAM ring buffer used when long videos bypass `.lraw` disk caching.
    LiveRing(LiveVideoRing),
}

impl FrameCache
{
    /// Opens an overlay frame store with explicit RAM cap, video budget, and GPU decoding mode.
    pub fn open_with_options(
        image_path: &Path,
        base_width: u32,
        max_frames: usize,
        ram_budget_bytes: usize,
        video_cache_bytes: usize,
        gpu_mode: Option<&str>,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, OverlayError>
    {
        let cache_dir = default_cache_dir()?;
        Self::open_in_dir_with_options(
            image_path,
            base_width,
            max_frames,
            cache_dir,
            ram_budget_bytes,
            video_cache_bytes,
            gpu_mode,
            DEFAULT_PRUNE_AGE_DAYS,
            wakeup,
        )
    }

    /// Opens a frame store against an explicit cache directory, RAM budget, and prune age.
    pub fn open_in_dir(
        image_path: &Path,
        base_width: u32,
        max_frames: usize,
        cache_dir: impl Into<PathBuf>,
        ram_budget_bytes: usize,
        prune_age_days: u64,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, OverlayError>
    {
        Self::open_in_dir_with_options(
            image_path,
            base_width,
            max_frames,
            cache_dir,
            ram_budget_bytes,
            DEFAULT_VIDEO_CACHE_BUDGET_BYTES,
            None,
            prune_age_days,
            wakeup,
        )
    }

    /// Opens a frame store with explicit cache directory, RAM cap, video budget, GPU mode, and prune age.
    #[expect(clippy::too_many_arguments)]
    pub fn open_in_dir_with_options(
        image_path: &Path,
        base_width: u32,
        max_frames: usize,
        cache_dir: impl Into<PathBuf>,
        ram_budget_bytes: usize,
        video_cache_bytes: usize,
        gpu_mode: Option<&str>,
        prune_age_days: u64,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, OverlayError>
    {
        let cache_dir = cache_dir.into();
        fs::create_dir_all(&cache_dir).map_err(|source| CacheError::Io {
            path: cache_dir.clone(),
            source,
        })?;

        let paths = compute_cache_paths(&cache_dir, image_path, base_width, max_frames)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let force_live_video = video_cache_bytes == 0 && has_video_signature(image_path)?;

        if !force_live_video
            && let Some(cached) = try_open_cached_store(&paths, ram_budget_bytes, &cancel)
        {
            prune_cache(&cache_dir, prune_age_days);
            return Ok(cached);
        }

        let mut source = open_source(image_path)?;
        if let OpenedSource::Video(ref mut probe) = source
            && let Some(gpu) = gpu_mode
        {
            probe.gpu = Some(gpu.to_string());
        }

        if let OpenedSource::Video(probe) = &source
            && max_frames != 1
            && !probe.should_cache(base_width.max(1), max_frames, video_cache_bytes)
        {
            let (ring, worker) = LiveVideoRing::open(
                image_path,
                probe,
                base_width.max(1),
                max_frames,
                Arc::clone(&cancel),
                wakeup,
            )?;
            prune_cache(&cache_dir, prune_age_days);
            return Ok(Self {
                cache_dir,
                backing: BackingStore::LiveRing(ring),
                cancel,
                worker,
            });
        }

        let scratch = Arc::new(Mutex::new(FrameScratch::default()));
        let shared_slot: Arc<Mutex<Option<Arc<Mutex<SharedFrames>>>>> = Arc::new(Mutex::new(None));
        let sink = Box::new(DecodeCacheSink::new(
            paths,
            cache_dir.clone(),
            prune_age_days,
            ram_budget_bytes,
            Arc::clone(&shared_slot),
            Arc::clone(&scratch),
            wakeup,
        ));

        let (first, worker) = FrameDecoder::open_stream_from_source(
            image_path,
            source,
            base_width,
            max_frames,
            Arc::clone(&cancel),
            sink,
        )?;

        let existing = lock_mutex(&shared_slot).take();
        let shared = match existing
        {
            Some(found) => found,
            None =>
            {
                let byte_len = checked_argb_byte_len(first.width, first.height)?;
                SharedFrames::with_first_frame(
                    first.width,
                    first.height,
                    byte_len,
                    ram_budget_bytes,
                    first.lz4_payload,
                    first.delay_us,
                    first.argb8888,
                    worker.is_none(),
                )
            }
        };

        if worker.is_none()
        {
            prune_cache(&cache_dir, prune_age_days);
        }

        Ok(Self {
            cache_dir,
            backing: BackingStore::Indexed { shared, scratch },
            cancel,
            worker,
        })
    }

    /// Internal constructor used when attaching a pre-populated `SharedFrames` store.
    pub(crate) fn from_parts(
        cache_dir: PathBuf,
        shared: Arc<Mutex<SharedFrames>>,
        scratch: Arc<Mutex<FrameScratch>>,
        cancel: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    ) -> Self
    {
        Self {
            cache_dir,
            backing: BackingStore::Indexed { shared, scratch },
            cancel,
            worker,
        }
    }

    /// Returns the active cache directory backing this store.
    pub fn cache_dir(&self) -> &Path
    {
        &self.cache_dir
    }

    /// Returns `true` when this store streams long videos live via bounded lookahead ring buffers.
    pub fn is_live_ring(&self) -> bool
    {
        matches!(self.backing, BackingStore::LiveRing(_))
    }

    /// Returns stored pixel `(width, height)` for this overlay.
    pub fn frame_dimensions(&self) -> (u32, u32)
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } =>
            {
                let guard = lock_mutex(shared);
                (guard.width, guard.height)
            }
            BackingStore::LiveRing(ring) => ring.frame_dimensions(),
        }
    }

    /// Invokes `visit` with borrowed frame microsecond delays and decode completion status without cloning.
    pub fn with_delays_us<R>(&self, visit: impl FnOnce(&[u32], bool) -> R) -> R
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } =>
            {
                let guard = lock_mutex(shared);
                visit(&guard.delays_us, guard.decode_done)
            }
            BackingStore::LiveRing(ring) => ring.with_delays_us(visit),
        }
    }

    /// Returns total number of frames currently known for playback.
    pub fn frame_count(&self) -> usize
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } => lock_mutex(shared).delays_us.len(),
            BackingStore::LiveRing(ring) => ring.frame_count(),
        }
    }

    /// Returns `true` when background streaming or initial loop delay discovery has completed.
    pub fn is_decode_done(&self) -> bool
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } => lock_mutex(shared).decode_done,
            BackingStore::LiveRing(ring) => ring.is_decode_done(),
        }
    }

    /// Returns `(frame_count, decode_done, fits_source_ram)` under one lock.
    pub fn cache_profile(&self) -> (usize, bool, bool)
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } =>
            {
                let guard = lock_mutex(shared);
                (
                    guard.delays_us.len(),
                    guard.decode_done,
                    guard.fits_ram_budget(),
                )
            }
            BackingStore::LiveRing(ring) => (ring.frame_count(), ring.is_decode_done(), false),
        }
    }

    /// Returns current uncompressed RAM bytes held in source LRU or live ring buffers.
    pub fn ram_used_bytes(&self) -> usize
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } => lock_mutex(shared).ram_used_bytes,
            BackingStore::LiveRing(ring) => ring.ram_used_bytes(),
        }
    }

    /// Returns compressed chunk bytes currently retained in heap RAM.
    pub fn compressed_ram_bytes(&self) -> usize
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, .. } => lock_mutex(shared).compressed_ram_bytes(),
            BackingStore::LiveRing(_) => 0,
        }
    }

    /// Streams or reads premultiplied `ARGB8888` pixel data for `index` into `visit` without copying on hits.
    pub fn with_frame_argb<R>(
        &self,
        index: usize,
        visit: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, OverlayError>
    {
        match &self.backing
        {
            BackingStore::Indexed { shared, scratch } =>
            {
                match prepare_frame_access(&self.cache_dir, shared, index)?
                {
                    PreparedFrame::Cached(frame) => Ok(visit(&frame)),
                    PreparedFrame::Compressed {
                        source,
                        expected_len,
                        allow_ram_cache,
                    } => fetch_compressed_frame(
                        &self.cache_dir,
                        shared,
                        scratch,
                        source,
                        index,
                        expected_len,
                        allow_ram_cache,
                        visit,
                    ),
                }
            }
            BackingStore::LiveRing(ring) =>
            {
                ring.with_frame_argb(&self.cache_dir, index, &self.cancel, visit)
            }
        }
    }

    /// Retrieves premultiplied `ARGB8888` pixel data for `index`, decompressing or reading live frames.
    pub fn frame_argb(&self, index: usize) -> Result<Arc<Vec<u8>>, OverlayError>
    {
        self.with_frame_argb(index, |bytes| Arc::new(bytes.to_vec()))
    }

    /// Signals background worker cancellation and joins its thread cleanly.
    pub fn stop(&mut self)
    {
        self.cancel.store(true, Ordering::Release);
        if let BackingStore::LiveRing(ring) = &self.backing
        {
            ring.wake_all();
        }
        if let Some(handle) = self.worker.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for FrameCache
{
    /// Cancels any live background worker when `FrameCache` is dropped.
    fn drop(&mut self)
    {
        self.stop();
    }
}
