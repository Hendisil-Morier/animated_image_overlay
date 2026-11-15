//! Streaming decode consumer writing `.tmp-*.lraw` before committing `.json` metadata.

// Filesystem writing, shared mutex handoff, and staging path tracking.
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

// Cache disk writers, shared frame buffer state, lazy `.lraw` reader, and decoder stream traits.
use super::{
    disk::{CachePaths, prune_cache, write_cache_metadata},
    memory::{SharedFrames, WakeupCallback, append_batch, finish_with_disk, lock_mutex},
    reader::{FrameScratch, LrawReader, build_chunk_spans},
};
use crate::{
    atomic_fs::AtomicTempFile,
    decode::{DecodedFirstFrame, EncodedFrame, FrameStreamSink, checked_argb_byte_len},
    error::CacheError,
};

// Batch threshold for flushing freshly decoded frames into shared state.
const DECODE_BATCH_SIZE: usize = 4;

/// Streaming sink bridging `FrameDecoder` output into `SharedFrames` and `.lraw`/`.json` storage.
pub(crate) struct DecodeCacheSink
{
    // Target `.lraw` and `.json` cache artifact paths.
    paths: CachePaths,

    // Parent cache directory pruned after persistence completes.
    cache_dir: PathBuf,

    // Retention threshold passed to `prune_cache`.
    prune_age_days: u64,

    // Byte limit applied when instantiating `SharedFrames`.
    ram_budget_bytes: usize,

    // Handoff slot delivering `Arc<Mutex<SharedFrames>>` once frame 0 arrives.
    shared_handoff: Arc<Mutex<Option<Arc<Mutex<SharedFrames>>>>>,

    // Shared frame state owned jointly with `FrameCache`.
    shared: Option<Arc<Mutex<SharedFrames>>>,

    // Decompression workspace receiving the committed `.lraw` reader when cold decode finishes.
    scratch: Arc<Mutex<FrameScratch>>,

    // Active uncommitted `.lraw` temporary writer (auto-removed on drop/abort).
    temp_lraw: Option<AtomicTempFile>,

    // Frame pixel width for metadata serialization.
    width: u32,

    // Frame pixel height for metadata serialization.
    height: u32,

    // Accumulated per-frame delays for `.json` commit.
    all_delays: Vec<u32>,

    // Accumulated per-frame `.lraw` chunk byte sizes for `.json` commit.
    all_sizes: Vec<usize>,

    // Pending compressed frames staged before taking the shared lock.
    pending_batch: Vec<(Vec<u8>, u32)>,

    // Optional wakeup callback to nudge the compositor loop.
    wakeup: Option<WakeupCallback>,
}

impl DecodeCacheSink
{
    /// Constructs a new staging sink prior to frame 0 readiness.
    pub(crate) fn new(
        paths: CachePaths,
        cache_dir: PathBuf,
        prune_age_days: u64,
        ram_budget_bytes: usize,
        shared_handoff: Arc<Mutex<Option<Arc<Mutex<SharedFrames>>>>>,
        scratch: Arc<Mutex<FrameScratch>>,
        wakeup: Option<WakeupCallback>,
    ) -> Self
    {
        Self {
            paths,
            cache_dir,
            prune_age_days,
            ram_budget_bytes,
            shared_handoff,
            shared: None,
            scratch,
            temp_lraw: None,
            width: 0,
            height: 0,
            all_delays: Vec::new(),
            all_sizes: Vec::new(),
            pending_batch: Vec::with_capacity(DECODE_BATCH_SIZE),
            wakeup,
        }
    }
}

impl FrameStreamSink for DecodeCacheSink
{
    /// Initializes shared frame storage, populates RAM frame 0, and opens temp `.lraw` staging.
    fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
    {
        self.width = frame.width;
        self.height = frame.height;
        let frame_bytes = checked_argb_byte_len(frame.width, frame.height)?;

        let shared = SharedFrames::with_first_frame(
            frame.width,
            frame.height,
            frame_bytes,
            self.ram_budget_bytes,
            frame.lz4_payload.clone(),
            frame.delay_ms,
            frame.argb8888.clone(),
            false,
        );
        self.shared = Some(Arc::clone(&shared));
        *lock_mutex(&self.shared_handoff) = Some(shared);

        self.all_delays.push(frame.delay_ms);
        self.all_sizes.push(frame.lz4_payload.len());

        let mut temp =
            AtomicTempFile::create(&self.paths.lraw_path, ".lraw").map_err(|source| {
                CacheError::Io {
                    path: self.paths.lraw_path.clone(),
                    source,
                }
            })?;
        temp.file_mut()
            .and_then(|f| f.write_all(&frame.lz4_payload))
            .map_err(|source| CacheError::Io {
                path: self.paths.lraw_path.clone(),
                source,
            })?;
        self.temp_lraw = Some(temp);

        Ok(())
    }

    /// Writes frame `n` into `.tmp-*.lraw` and stages it for batched insertion into `SharedFrames`.
    fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
    {
        if let Some(temp) = self.temp_lraw.as_mut()
        {
            temp.file_mut()
                .and_then(|f| f.write_all(&frame.lz4_payload))
                .map_err(|source| CacheError::Io {
                    path: self.paths.lraw_path.clone(),
                    source,
                })?;
        }

        self.all_delays.push(frame.delay_ms);
        self.all_sizes.push(frame.lz4_payload.len());
        self.pending_batch.push((frame.lz4_payload, frame.delay_ms));

        if self.pending_batch.len() >= DECODE_BATCH_SIZE
            && let Some(shared) = &self.shared
        {
            append_batch(shared, &mut self.pending_batch, false, self.wakeup.as_ref());
        }

        Ok(())
    }

    /// Commits `.lraw` before `.json` when completed cleanly and transitions playback to disk spans.
    fn on_finish(mut self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
    {
        let mut committed_disk = None;
        if !cancelled
            && result.is_ok()
            && !self.all_sizes.is_empty()
            && let Some(temp) = self.temp_lraw.take()
        {
            if let Err(err) = temp.commit()
            {
                tracing::warn!(error = %err, "Failed committing .lraw cache payload");
            }
            else if let Err(err) = write_cache_metadata(
                &self.paths.meta_path,
                self.width,
                self.height,
                &self.all_delays,
                &self.all_sizes,
            )
            {
                tracing::warn!(error = %err, "Failed writing .json cache metadata");
            }
            else if let Ok(spans) = build_chunk_spans(&self.paths.lraw_path, &self.all_sizes)
                .inspect_err(|err| {
                    tracing::warn!(error = %err, "Failed indexing committed .lraw chunk spans");
                })
                && let Ok(reader) = LrawReader::open(&self.paths.lraw_path).inspect_err(|err| {
                    tracing::warn!(error = %err, "Failed opening committed .lraw for streaming");
                })
            {
                committed_disk = Some((spans, reader));
            }
        }
        else
        {
            // Dropping `temp_lraw` unlinks any uncommitted `.tmp-*.lraw` file.
            drop(self.temp_lraw.take());
        }

        if let Some((spans, reader)) = committed_disk
        {
            // Publish the open reader before `disk_spans` so every consumer seeing spans has a live handle.
            lock_mutex(&self.scratch).lraw = Some(reader);
            if let Some(shared) = &self.shared
            {
                finish_with_disk(shared, &mut self.pending_batch, spans, self.wakeup.as_ref());
            }
        }
        else if let Some(shared) = &self.shared
        {
            append_batch(shared, &mut self.pending_batch, true, self.wakeup.as_ref());
        }

        if !cancelled
        {
            prune_cache(&self.cache_dir, self.prune_age_days);
        }
    }
}
