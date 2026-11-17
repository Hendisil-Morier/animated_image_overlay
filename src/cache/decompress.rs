//! On-the-fly `.lraw` and memory chunk decompression into reusable scratch buffers.

// Filesystem paths, heap allocations, and thread synchronization.
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

// Scratch memory, chunk reader, LRU frame slots, and mutex helper.
use super::{
    memory::{ChunkSpan, SharedFrames, lock_mutex},
    reader::{FrameScratch, decompress_chunk_into},
};
use crate::error::{CacheError, OverlayError};

/// Backing storage location for one LZ4-compressed frame payload.
pub(crate) enum ChunkSource
{
    // Temporary in-memory LZ4 frame held during cold decoding before `.lraw` flush completes.
    Memory(Vec<u8>),

    // Committed `.lraw` file byte span for positional disk reads.
    Disk(ChunkSpan),
}

/// Result of resolving a frame index under `SharedFrames` lock.
pub(crate) enum PreparedFrame
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

/// Resolves `index` from in-memory RAM cache or prepares the backing compressed chunk source.
pub(crate) fn prepare_frame_access(
    cache_dir: &Path,
    shared: &Arc<Mutex<SharedFrames>>,
    index: usize,
) -> Result<PreparedFrame, OverlayError>
{
    let mut guard = lock_mutex(shared);
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
            path: cache_dir.to_path_buf(),
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

/// Decompresses the compressed chunk for `index` into `scratch` and visits its bytes.
#[expect(clippy::too_many_arguments)]
pub(crate) fn fetch_compressed_frame<R>(
    cache_dir: &Path,
    shared: &Arc<Mutex<SharedFrames>>,
    scratch: &Arc<Mutex<FrameScratch>>,
    source: ChunkSource,
    index: usize,
    expected_len: usize,
    allow_ram_cache: bool,
    visit: impl FnOnce(&[u8]) -> R,
) -> Result<R, OverlayError>
{
    let mut scratch_guard = lock_mutex(scratch);
    decompress_chunk_source(cache_dir, &mut scratch_guard, source, index, expected_len)?;
    let result = visit(&scratch_guard.argb);
    if allow_ram_cache
    {
        let slot = Arc::new(scratch_guard.argb.clone());
        lock_mutex(shared).insert_ram_frame(index, slot);
    }
    Ok(result)
}
