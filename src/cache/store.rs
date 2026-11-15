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

    /// Invokes `visit` with borrowed frame microsecond delays and decode completion status without cloning.
    pub fn with_delays_us<R>(&self, visit: impl FnOnce(&[u32], bool) -> R) -> R
    {
        let guard = lock_mutex(&self.shared);
        visit(&guard.delays_us, guard.decode_done)
    }

    /// Returns total number of frames currently known for playback.
    pub fn frame_count(&self) -> usize
    {
        lock_mutex(&self.shared).delays_us.len()
    }

    /// Returns `true` when background streaming or decoding has completed.
    pub fn is_decode_done(&self) -> bool
    {
        lock_mutex(&self.shared).decode_done
    }

    /// Returns `(frame_count, decode_done, fits_source_ram_budget)` under one lock.
    pub fn cache_profile(&self) -> (usize, bool, bool)
    {
        let guard = lock_mutex(&self.shared);
        (
            guard.delays_us.len(),
            guard.decode_done,
            guard.fits_ram_budget(),
        )
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
        match self.prepare_frame_access(index)?
        {
            PreparedFrame::Cached(frame) => Ok(visit(&frame)),
            PreparedFrame::Compressed {
                source,
                expected_len,
                allow_ram_cache,
            } =>
            {
                let (result, slot) = {
                    let mut scratch = lock_mutex(&self.scratch);
                    decompress_chunk_source(
                        &self.cache_dir,
                        &mut scratch,
                        source,
                        index,
                        expected_len,
                    )?;
                    let result = visit(&scratch.argb);
                    let slot = allow_ram_cache.then(|| Arc::new(scratch.argb.clone()));
                    (result, slot)
                };
                if let Some(frame) = slot
                {
                    lock_mutex(&self.shared).insert_ram_frame(index, frame);
                }
                Ok(result)
            }
        }
    }

    /// Resolves `index` from in-memory RAM cache or prepares the backing compressed chunk source.
    fn prepare_frame_access(&self, index: usize) -> Result<PreparedFrame, OverlayError>
    {
        let mut guard = lock_mutex(&self.shared);
        if let Some(hit) = guard.ram_slots.get(&index).map(Arc::clone)
        {
            guard.promote_lru(index);
            return Ok(PreparedFrame::Cached(hit));
        }
        if index == 0
            && let Some(frame0) = guard.initial_frame0.take()
        {
            return Ok(PreparedFrame::Cached(frame0));
        }

        let allow_ram_cache = guard.fits_ram_budget();
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

        Ok(PreparedFrame::Compressed {
            source,
            expected_len: guard.frame_byte_len,
            allow_ram_cache,
        })
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

/// Backing storage location for one LZ4-compressed frame payload.
enum ChunkSource
{
    // Temporary in-memory LZ4 frame held during cold decoding before `.lraw` flush completes.
    Memory(Vec<u8>),

    // Committed `.lraw` file byte span for positional disk reads.
    Disk(ChunkSpan),
}

/// Result of resolving a frame index under `SharedFrames` lock.
enum PreparedFrame
{
    // Already uncompressed `ARGB8888` frame available directly from RAM LRU or `initial_frame0`.
    Cached(Arc<Vec<u8>>),

    // Compressed chunk ready to decode under `FrameScratch` lock.
    Compressed
    {
        // Backing chunk payload location.
        source: ChunkSource,

        // Expected uncompressed pixel buffer byte length.
        expected_len: usize,

        // Whether the animation fits the RAM budget to insert into `ram_slots`.
        allow_ram_cache: bool,
    },
}

/// Reads and LZ4-decompresses `source` for frame `index` into `scratch.argb`.
fn decompress_chunk_source(
    cache_dir: &Path,
    scratch: &mut FrameScratch,
    source: ChunkSource,
    index: usize,
    expected_len: usize,
) -> Result<(), CacheError>
{
    match source
    {
        ChunkSource::Disk(span) =>
        {
            let Some(reader) = scratch.lraw.as_mut()
            else
            {
                return Err(CacheError::CorruptMetadata {
                    path: cache_dir.to_path_buf(),
                    reason: format!("Missing .lraw reader while accessing frame index {index}"),
                });
            };
            let err_path = reader.path().to_path_buf();
            let chunk = reader.read_chunk(span)?;
            decompress_chunk_into(&err_path, index, chunk, expected_len, &mut scratch.argb)
        }
        ChunkSource::Memory(chunk) =>
        {
            decompress_chunk_into(cache_dir, index, &chunk, expected_len, &mut scratch.argb)
        }
    }
}
