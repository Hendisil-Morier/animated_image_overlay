//! Warm `.lraw` disk cache reader loading frame 0 synchronously and streaming remaining chunks.

// Filesystem reading, seeking, worker threads, and cancellation atomics.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

// Decompression for initial frame 0 verification on cache open.
use lz4_flex::decompress_size_prepended;

// Shared cache metadata, memory batch helpers, frame store constructor, and dimension checks.
use super::{
    disk::{CacheMetadata, CachePaths, load_and_validate_meta, touch_modified},
    memory::{SharedFrames, WakeupCallback, append_batch, mark_decode_done},
    store::FrameCache,
};
use crate::{decode::checked_argb_byte_len, error::CacheError, playback::MIN_FRAME_DELAY_MS};

// Batch threshold for flushing streamed cache-hit frames into shared state.
const CACHE_HIT_BATCH_SIZE: usize = 16;

/// Validates and loads cached frame 0 from `.json` + `.lraw`, spawning streamer for frames `1..`.
pub(crate) fn try_open_cached_store(
    paths: &CachePaths,
    ram_budget_bytes: usize,
    cancel: &Arc<AtomicBool>,
    wakeup: Option<WakeupCallback>,
) -> Option<FrameCache>
{
    if !paths.meta_path.is_file() || !paths.lraw_path.is_file()
    {
        return None;
    }

    let meta = load_and_validate_meta(paths)
        .inspect_err(|err| {
            tracing::warn!(error = %err, "Ignoring corrupt disk cache and falling back to decode");
        })
        .ok()?;

    let frame_byte_len = checked_argb_byte_len(meta.w, meta.h)
        .inspect_err(|err| tracing::warn!(error = %err, "Ignoring overflowed cached dimensions"))
        .ok()?;

    let (first_compressed, first_raw) = read_verified_frame0(paths, &meta, frame_byte_len)?;

    touch_modified(&paths.meta_path);
    touch_modified(&paths.lraw_path);

    let mut state = SharedFrames::new(meta.w, meta.h, frame_byte_len, ram_budget_bytes);
    state.compressed_frames.push(first_compressed);
    state.delays_ms.push(meta.delays[0].max(MIN_FRAME_DELAY_MS));
    state.insert_ram_frame(0, Arc::new(first_raw));

    let cache_dir = paths
        .lraw_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    if meta.delays.len() <= 1
    {
        state.decode_done = true;
        return Some(FrameCache::from_parts(
            cache_dir,
            Arc::new(Mutex::new(state)),
            Arc::clone(cancel),
            None,
        ));
    }

    let shared = Arc::new(Mutex::new(state));
    let shared_worker = Arc::clone(&shared);
    let cancel_worker = Arc::clone(cancel);
    let lraw_path = paths.lraw_path.clone();

    tracing::debug!(
        frames = meta.delays.len(),
        w = meta.w,
        h = meta.h,
        "Loaded overlay frame 0 from cache"
    );

    let worker = thread::Builder::new()
        .name(String::from("aiw-cache-reader"))
        .spawn(move || {
            stream_cached_chunks(&lraw_path, meta, shared_worker, cancel_worker, wakeup);
        })
        .ok()?;

    Some(FrameCache::from_parts(
        cache_dir,
        shared,
        Arc::clone(cancel),
        Some(worker),
    ))
}

/// Reads and LZ4-decompresses frame 0 from `.lraw`, validating uncompressed byte length.
fn read_verified_frame0(
    paths: &CachePaths,
    meta: &CacheMetadata,
    expected_len: usize,
) -> Option<(Vec<u8>, Vec<u8>)>
{
    let mut file = File::open(&paths.lraw_path)
        .inspect_err(|err| tracing::warn!(error = %err, "Failed opening .lraw cache file"))
        .ok()?;

    let mut compressed = vec![0_u8; meta.sizes[0]];
    if let Err(err) = file.read_exact(&mut compressed)
    {
        tracing::warn!(error = %err, "Failed reading frame 0 from .lraw cache");
        return None;
    }

    match decompress_size_prepended(&compressed)
    {
        Ok(raw) if raw.len() == expected_len => Some((compressed, raw)),
        Ok(raw) =>
        {
            tracing::warn!(
                actual = raw.len(),
                expected = expected_len,
                "Cached frame 0 decompressed size mismatch"
            );
            None
        }
        Err(err) =>
        {
            tracing::warn!(error = %err, "Cached frame 0 LZ4 decompression failed");
            None
        }
    }
}

/// Streams remaining LZ4 frame chunks from `.lraw` in batches until completion or cancellation.
fn stream_cached_chunks(
    lraw_path: &Path,
    meta: CacheMetadata,
    shared: Arc<Mutex<SharedFrames>>,
    cancel: Arc<AtomicBool>,
    wakeup: Option<WakeupCallback>,
)
{
    let result: Result<(), CacheError> = (|| {
        let mut file = File::open(lraw_path).map_err(|source| CacheError::Io {
            path: lraw_path.to_path_buf(),
            source,
        })?;
        file.seek(SeekFrom::Start(meta.sizes[0] as u64))
            .map_err(|source| CacheError::Io {
                path: lraw_path.to_path_buf(),
                source,
            })?;

        let mut batch = Vec::with_capacity(CACHE_HIT_BATCH_SIZE);
        for (&size, &delay) in meta.sizes[1..].iter().zip(meta.delays[1..].iter())
        {
            if cancel.load(Ordering::Acquire)
            {
                return Ok(());
            }

            let mut chunk = vec![0_u8; size];
            file.read_exact(&mut chunk)
                .map_err(|source| CacheError::Io {
                    path: lraw_path.to_path_buf(),
                    source,
                })?;
            batch.push((chunk, delay.max(MIN_FRAME_DELAY_MS)));

            if batch.len() >= CACHE_HIT_BATCH_SIZE
            {
                append_batch(&shared, &mut batch, false, wakeup.as_ref());
            }
        }

        append_batch(&shared, &mut batch, true, wakeup.as_ref());
        Ok(())
    })();

    if let Err(err) = result
    {
        tracing::warn!(error = %err, "Failed streaming remaining cached chunks");
        mark_decode_done(&shared, wakeup.as_ref());
    }
}
