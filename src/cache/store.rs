//! Public `FrameCache` lifecycle controller bridging disk storage, decoder, and RAM LRU cache.

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

// Decompression for cached premultiplied `ARGB8888` frame buffers on RAM misses.
use lz4_flex::decompress_size_prepended;

// Internal cache disk helpers, RAM LRU state, warm reader, and decode sink.
use super::{
    disk::{DEFAULT_PRUNE_AGE_DAYS, compute_cache_paths, default_cache_dir, prune_cache},
    memory::{DEFAULT_RAM_CACHE_BUDGET_BYTES, SharedFrames, WakeupCallback, lock_mutex},
    reader::try_open_cached_store,
    sink::DecodeCacheSink,
};
use crate::{
    decode::{FrameDecoder, checked_argb_byte_len},
    error::{CacheError, OverlayError},
};

/// Manages source/disk cache loading, background streaming, LRU RAM caching, and pruning.
pub struct FrameCache
{
    // Directory storing `<hash>.lraw` and `<hash>.json` cache pairs.
    cache_dir: PathBuf,

    // Synchronized compressed + uncompressed frame store.
    shared: Arc<Mutex<SharedFrames>>,

    // Cancellation signal stopping worker threads cleanly on exit.
    cancel: Arc<AtomicBool>,

    // Join handle for active background decoding or cache streaming thread.
    worker: Option<JoinHandle<()>>,
}

impl FrameCache
{
    /// Opens an overlay frame store immediately loading frame 0 from cache or source file.
    pub fn open(
        image_path: &Path,
        base_width: u32,
        max_frames: usize,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, OverlayError>
    {
        let cache_dir = default_cache_dir()?;
        Self::open_in_dir(
            image_path,
            base_width,
            max_frames,
            cache_dir,
            DEFAULT_RAM_CACHE_BUDGET_BYTES,
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
        let cache_dir = cache_dir.into();
        fs::create_dir_all(&cache_dir).map_err(|source| CacheError::Io {
            path: cache_dir.clone(),
            source,
        })?;

        let paths = compute_cache_paths(&cache_dir, image_path, base_width, max_frames)?;
        let cancel = Arc::new(AtomicBool::new(false));

        if let Some(cached) =
            try_open_cached_store(&paths, ram_budget_bytes, &cancel, wakeup.clone())
        {
            prune_cache(&cache_dir, prune_age_days);
            return Ok(cached);
        }

        let shared_slot: Arc<Mutex<Option<Arc<Mutex<SharedFrames>>>>> = Arc::new(Mutex::new(None));
        let sink = Box::new(DecodeCacheSink::new(
            paths,
            cache_dir.clone(),
            prune_age_days,
            ram_budget_bytes,
            Arc::clone(&shared_slot),
            wakeup,
        ));

        let (first, worker) = FrameDecoder::open_stream(
            image_path,
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
                    first.delay_ms,
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
            shared,
            cancel,
            worker,
        })
    }

    /// Internal constructor used when attaching a pre-populated `SharedFrames` store.
    pub(crate) fn from_parts(
        cache_dir: PathBuf,
        shared: Arc<Mutex<SharedFrames>>,
        cancel: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    ) -> Self
    {
        Self {
            cache_dir,
            shared,
            cancel,
            worker,
        }
    }

    /// Returns the active cache directory backing this store.
    pub fn cache_dir(&self) -> &Path
    {
        &self.cache_dir
    }

    /// Returns stored pixel `(width, height)` for this overlay.
    pub fn frame_dimensions(&self) -> (u32, u32)
    {
        let guard = lock_mutex(&self.shared);
        (guard.width, guard.height)
    }

    /// Returns current buffered frame delays and whether all frames are loaded.
    pub fn delays_snapshot(&self) -> (Vec<u32>, bool)
    {
        let guard = lock_mutex(&self.shared);
        (guard.delays_ms.clone(), guard.decode_done)
    }

    /// Returns total number of frames currently available in memory.
    pub fn frame_count(&self) -> usize
    {
        lock_mutex(&self.shared).compressed_frames.len()
    }

    /// Returns `true` when background streaming or decoding has completed.
    pub fn is_decode_done(&self) -> bool
    {
        lock_mutex(&self.shared).decode_done
    }

    /// Returns current uncompressed RAM bytes held in the frame LRU cache.
    pub fn ram_used_bytes(&self) -> usize
    {
        lock_mutex(&self.shared).ram_used_bytes
    }

    /// Retrieves premultiplied `ARGB8888` pixel data for `index`, decompressing via LZ4 on RAM misses.
    pub fn frame_argb(&self, index: usize) -> Result<Arc<Vec<u8>>, OverlayError>
    {
        let (compressed, expected_len) = {
            let mut guard = lock_mutex(&self.shared);
            if let Some(hit) = guard.ram_slots.get(&index).map(Arc::clone)
            {
                guard.promote_lru(index);
                return Ok(hit);
            }

            let Some(chunk) = guard.compressed_frames.get(index)
            else
            {
                return Err(CacheError::CorruptMetadata {
                    path: self.cache_dir.clone(),
                    reason: format!("Frame index {index} out of bounds"),
                }
                .into());
            };
            (chunk.clone(), guard.frame_byte_len)
        };

        let raw = decompress_size_prepended(&compressed).map_err(|err| CacheError::Codec {
            path: self.cache_dir.clone(),
            frame_index: index,
            reason: err.to_string(),
        })?;

        if raw.len() != expected_len
        {
            return Err(CacheError::Codec {
                path: self.cache_dir.clone(),
                frame_index: index,
                reason: format!(
                    "Decompressed byte length {} did not match expected {expected_len}",
                    raw.len()
                ),
            }
            .into());
        }

        let arc = Arc::new(raw);
        let mut guard = lock_mutex(&self.shared);
        guard.insert_ram_frame(index, Arc::clone(&arc));
        Ok(arc)
    }

    /// Signals background worker cancellation and joins its thread cleanly.
    pub fn stop(&mut self)
    {
        self.cancel.store(true, Ordering::Release);
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
