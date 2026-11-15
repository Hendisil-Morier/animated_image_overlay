//! Warm `.lraw` disk cache reader loading frame 0 synchronously and streaming frames lazily on demand.

// Filesystem reading, positional pread I/O, reusable staging buffers, and cancellation state.
use std::{
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};

// Direct LZ4 block header parsing and zero-allocation decompression into reusable buffers.
use lz4_flex::block::{decompress_into, uncompressed_size};

// Shared cache metadata, chunk span records, memory lock helpers, and frame store constructor.
use super::{
    disk::{CachePaths, load_and_validate_meta, touch_modified},
    memory::{ChunkSpan, SharedFrames},
    store::FrameCache,
};
use crate::{decode::checked_argb_byte_len, error::CacheError, playback::MIN_FRAME_DELAY_MS};

/// Random-access `.lraw` chunk reader reusing a single compressed byte buffer across frames.
pub(crate) struct LrawReader
{
    // On-disk `.lraw` path preserved for contextual I/O error reporting.
    lraw_path: PathBuf,

    // Open read-only handle used for offset-addressed chunk reads (`pread`).
    file: File,

    // Reusable compressed staging buffer grown lazily up to the largest requested chunk.
    read_buf: Vec<u8>,
}

impl LrawReader
{
    /// Opens a committed `.lraw` file at `path` for lazy per-frame chunk reading.
    pub(crate) fn open(path: &Path) -> Result<Self, CacheError>
    {
        let file = File::open(path).map_err(|source| CacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            lraw_path: path.to_path_buf(),
            file,
            read_buf: Vec::new(),
        })
    }

    /// Exposes the `.lraw` file path backing this reader for diagnostics.
    pub(crate) fn path(&self) -> &Path
    {
        &self.lraw_path
    }

    /// Reads the compressed LZ4 chunk at `span` using positional I/O without seeking.
    pub(crate) fn read_chunk(&mut self, span: ChunkSpan) -> Result<&[u8], CacheError>
    {
        if self.read_buf.len() < span.len
        {
            self.read_buf.resize(span.len, 0);
        }
        let target = &mut self.read_buf[..span.len];
        self.file
            .read_exact_at(target, span.offset)
            .map_err(|source| CacheError::Io {
                path: self.lraw_path.clone(),
                source,
            })?;
        Ok(target)
    }
}

/// Reusable decompression workspace serialized independently of `SharedFrames` during frame reads.
#[derive(Default)]
pub(crate) struct FrameScratch
{
    /// Active `.lraw` chunk reader once disk storage is committed or loaded warm.
    pub(crate) lraw: Option<LrawReader>,

    /// Reusable premultiplied `ARGB8888` pixel buffer avoiding per-frame allocations on RAM misses.
    pub(crate) argb: Vec<u8>,
}

impl FrameScratch
{
    /// Wraps an already-validated `.lraw` reader for synchronous warm cache hits.
    pub(crate) fn with_reader(lraw: LrawReader) -> Self
    {
        Self {
            lraw: Some(lraw),
            argb: Vec::new(),
        }
    }
}

/// Builds sequential `(offset, len)` byte spans from per-frame chunk sizes in `.json`.
pub(crate) fn build_chunk_spans(
    lraw_path: &Path,
    sizes: &[usize],
) -> Result<Vec<ChunkSpan>, CacheError>
{
    let mut spans = Vec::with_capacity(sizes.len());
    let mut offset = 0_u64;
    for &len in sizes
    {
        let len_u64 = u64::try_from(len).map_err(|_| CacheError::CorruptMetadata {
            path: lraw_path.to_path_buf(),
            reason: format!("Chunk length {len} overflowed offset calculation"),
        })?;
        spans.push(ChunkSpan { offset, len });
        offset = offset
            .checked_add(len_u64)
            .ok_or_else(|| CacheError::CorruptMetadata {
                path: lraw_path.to_path_buf(),
                reason: String::from("Total .lraw byte offset overflowed u64"),
            })?;
    }
    Ok(spans)
}

/// Decompresses a size-prepended LZ4 chunk directly into `output` and verifies exact byte length.
pub(crate) fn decompress_chunk_into(
    err_path: &Path,
    frame_index: usize,
    chunk: &[u8],
    expected_len: usize,
    output: &mut Vec<u8>,
) -> Result<(), CacheError>
{
    let (declared_len, block_payload) =
        uncompressed_size(chunk).map_err(|err| CacheError::Codec {
            path: err_path.to_path_buf(),
            frame_index,
            reason: err.to_string(),
        })?;

    if declared_len != expected_len
    {
        return Err(CacheError::Codec {
            path: err_path.to_path_buf(),
            frame_index,
            reason: format!(
                "Declared LZ4 length {declared_len} did not match expected {expected_len}"
            ),
        });
    }

    if output.len() != expected_len
    {
        output.resize(expected_len, 0);
    }

    let written = decompress_into(block_payload, &mut output[..expected_len]).map_err(|err| {
        CacheError::Codec {
            path: err_path.to_path_buf(),
            frame_index,
            reason: err.to_string(),
        }
    })?;

    if written != expected_len
    {
        return Err(CacheError::Codec {
            path: err_path.to_path_buf(),
            frame_index,
            reason: format!(
                "Decompressed byte length {written} did not match expected {expected_len}"
            ),
        });
    }

    Ok(())
}

/// Validates `.json` + frame 0 from `.lraw` and opens a zero-thread lazy disk-backed `FrameCache`.
pub(crate) fn try_open_cached_store(
    paths: &CachePaths,
    ram_budget_bytes: usize,
    cancel: &Arc<AtomicBool>,
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

    let spans = build_chunk_spans(&paths.lraw_path, &meta.sizes)
        .inspect_err(|err| tracing::warn!(error = %err, "Ignoring invalid cached chunk spans"))
        .ok()?;

    let mut reader = LrawReader::open(&paths.lraw_path)
        .inspect_err(|err| tracing::warn!(error = %err, "Failed opening .lraw cache file"))
        .ok()?;

    let first_raw = read_verified_frame0(&mut reader, spans[0], frame_byte_len)?;

    touch_modified(&paths.meta_path);
    touch_modified(&paths.lraw_path);

    let delays_ms: Vec<u32> = meta
        .delays
        .iter()
        .map(|&delay| delay.max(MIN_FRAME_DELAY_MS))
        .collect();
    let shared = SharedFrames::with_warm_disk(
        meta.w,
        meta.h,
        frame_byte_len,
        ram_budget_bytes,
        spans,
        delays_ms,
        first_raw,
    );
    let scratch = Arc::new(Mutex::new(FrameScratch::with_reader(reader)));

    let cache_dir = paths
        .lraw_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    tracing::debug!(
        frames = meta.delays.len(),
        w = meta.w,
        h = meta.h,
        "Opened warm lazy .lraw cache"
    );

    Some(FrameCache::from_parts(
        cache_dir,
        shared,
        scratch,
        Arc::clone(cancel),
        None,
    ))
}

/// Reads and LZ4-decompresses frame 0 from `.lraw`, verifying exact uncompressed byte length.
fn read_verified_frame0(
    reader: &mut LrawReader,
    span0: ChunkSpan,
    expected_len: usize,
) -> Option<Vec<u8>>
{
    let err_path = reader.path().to_path_buf();
    let chunk0 = match reader.read_chunk(span0)
    {
        Ok(chunk) => chunk,
        Err(err) =>
        {
            tracing::warn!(error = %err, "Failed reading frame 0 from .lraw cache");
            return None;
        }
    };

    let mut raw = Vec::new();
    if let Err(err) = decompress_chunk_into(&err_path, 0, chunk0, expected_len, &mut raw)
    {
        tracing::warn!(error = %err, "Cached frame 0 LZ4 decompression failed");
        return None;
    }

    Some(raw)
}
