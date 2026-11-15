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

// Internal cache disk helpers, RAM LRU state, lazy reader, and decode sink.
use super::{
    disk::{DEFAULT_PRUNE_AGE_DAYS, compute_cache_paths, default_cache_dir, prune_cache},
    memory::{ChunkSpan, DEFAULT_RAM_CACHE_BUDGET_BYTES, SharedFrames, WakeupCallback, lock_mutex},
    reader::{FrameScratch, decompress_chunk_into, try_open_cached_store},
    sink::DecodeCacheSink,
};
use crate::{
    decode::{FrameDecoder, checked_argb_byte_len},
    error::{CacheError, OverlayError},
};

/// Manages source/disk cache loading, lazy chunk streaming, conditional RAM caching, and pruning.
pub struct FrameCache
{
    // Directory storing `<hash>.lraw` and `<hash>.json` cache pairs.
    cache_dir: PathBuf,

    // Synchronized frame metadata, chunk spans, and conditional RAM cache.
    shared: Arc<Mutex<SharedFrames>>,

    // Reusable `.lraw` reader and decompression workspace kept separate from `shared`.
    scratch: Arc<Mutex<FrameScratch>>,

    // Cancellation signal stopping worker threads cleanly on exit.
    cancel: Arc<AtomicBool>,

    // Join handle for active background source decoding thread.
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
        Self::open_with_ram_budget(
            image_path,
            base_width,
            max_frames,
            DEFAULT_RAM_CACHE_BUDGET_BYTES,
            wakeup,
        )
    }

    /// Opens an overlay frame store honoring a custom uncompressed RAM cache ceiling in bytes.
    pub fn open_with_ram_budget(
        image_path: &Path,
        base_width: u32,
        max_frames: usize,
        ram_budget_bytes: usize,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, OverlayError>
    {
        let cache_dir = default_cache_dir()?;
        Self::open_in_dir(
            image_path,
            base_width,
            max_frames,
            cache_dir,
            ram_budget_bytes,
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

        if let Some(cached) = try_open_cached_store(&paths, ram_budget_bytes, &cancel)
        {
            prune_cache(&cache_dir, prune_age_days);
            return Ok(cached);
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
            scratch,
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
            shared,
            scratch,
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

    /// Invokes `visit` with borrowed frame delays and decode completion status without cloning.
    pub fn with_delays<R>(&self, visit: impl FnOnce(&[u32], bool) -> R) -> R
    {
        let guard = lock_mutex(&self.shared);
        visit(&guard.delays_ms, guard.decode_done)
    }

    /// Returns total number of frames currently known for playback.
    pub fn frame_count(&self) -> usize
    {
        lock_mutex(&self.shared).delays_ms.len()
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

    /// Returns compressed chunk bytes currently retained in heap RAM.
    pub fn compressed_ram_bytes(&self) -> usize
    {
        lock_mutex(&self.shared).compressed_ram_bytes()
    }

    /// Streams or reads premultiplied `ARGB8888` pixel data for `index` into `visit` without copying on disk hits.
    pub fn with_frame_argb<R>(
        &self,
        index: usize,
        visit: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, OverlayError>
    {
        // Identifies whether the frame chunk lives temporarily in heap RAM or inside committed `.lraw`.
        enum ChunkSource
        {
            Memory(Vec<u8>),
            Disk(ChunkSpan),
        }

        let (source, expected_len, allow_ram_cache) = {
            let mut guard = lock_mutex(&self.shared);
            if let Some(hit) = guard.ram_slots.get(&index).map(Arc::clone)
            {
                guard.promote_lru(index);
                drop(guard);
                return Ok(visit(&hit));
            }
            if index == 0
                && let Some(frame0) = guard.initial_frame0.take()
            {
                drop(guard);
                return Ok(visit(&frame0));
            }

            let fits = guard.fits_ram_budget();
            let source = if let Some(&span) = guard.disk_spans.get(index)
            {
                ChunkSource::Disk(span)
            }
            else if let Some(chunk) = guard.compressed_frames.get(index)
            {
                ChunkSource::Memory(chunk.clone())
            }
            else
            {
                return Err(CacheError::CorruptMetadata {
                    path: self.cache_dir.clone(),
                    reason: format!("Frame index {index} out of bounds"),
                }
                .into());
            };
            (source, guard.frame_byte_len, fits)
        };

        let cached_slot = {
            let mut scratch = lock_mutex(&self.scratch);
            let scratch = &mut *scratch;
            match source
            {
                ChunkSource::Disk(span) =>
                {
                    let Some(reader) = scratch.lraw.as_mut()
                    else
                    {
                        return Err(CacheError::CorruptMetadata {
                            path: self.cache_dir.clone(),
                            reason: format!(
                                "Missing .lraw reader while accessing frame index {index}"
                            ),
                        }
                        .into());
                    };
                    let err_path = reader.path().to_path_buf();
                    let chunk = reader.read_chunk(span)?;
                    decompress_chunk_into(
                        &err_path,
                        index,
                        chunk,
                        expected_len,
                        &mut scratch.argb,
                    )?;
                }
                ChunkSource::Memory(chunk) =>
                {
                    decompress_chunk_into(
                        &self.cache_dir,
                        index,
                        &chunk,
                        expected_len,
                        &mut scratch.argb,
                    )?;
                }
            }

            let result = visit(&scratch.argb);
            if !allow_ram_cache
            {
                return Ok(result);
            }
            (result, Arc::new(scratch.argb.clone()))
        };

        let mut guard = lock_mutex(&self.shared);
        guard.insert_ram_frame(index, cached_slot.1);
        Ok(cached_slot.0)
    }

    /// Retrieves premultiplied `ARGB8888` pixel data for `index`, decompressing via LZ4 on RAM misses.
    pub fn frame_argb(&self, index: usize) -> Result<Arc<Vec<u8>>, OverlayError>
    {
        self.with_frame_argb(index, |bytes| Arc::new(bytes.to_vec()))
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
