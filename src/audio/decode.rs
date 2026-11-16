//! Subprocess `ffmpeg` PCM extraction for preloaded tracks and bounded lookahead audio rings.

// Collections, I/O readers, paths, subprocess control, and thread synchronization.
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

// Audio ceiling and probe metadata types.
use super::probe::{AUDIO_RAM_CACHE_MAX_BYTES, AudioProbe};
use crate::error::AudioError;

// Stereo output channel count decoded from `ffmpeg` for consistent mixing.
pub(crate) const TARGET_PCM_CHANNELS: u16 = 2;

// PCM bytes per interleaved stereo `f32` frame (`2 * 4 = 8` bytes).
const PCM_BYTES_PER_SAMPLE: usize = 4;

// Buffered PCM read chunk size in bytes (~`85 ms` of stereo `48 kHz` `f32` audio).
const DECODE_CHUNK_BYTES: usize = 32_768;

// Extra headroom over `AUDIO_RAM_CACHE_MAX_BYTES` guarding buffered read loops cleanly.
const BUFFERED_PCM_HARD_CAP_BYTES: usize = AUDIO_RAM_CACHE_MAX_BYTES + (2 * 1024 * 1024);

// Prefill duration buffered cleanly inside `PcmRing` before signaling readiness (~`80 ms`).
const STREAM_PREFILL_NUMERATOR: usize = 80;
const STREAM_PREFILL_DENOMINATOR: usize = 1_000;

// Timeout slice for condition variable waits keeping cancellation responsive.
const RING_WAIT_SLICE_MS: u64 = 30;

/// Owns a running `ffmpeg` audio subprocess and guarantees kill+reap on drop.
struct AudioChildGuard
{
    // Running `ffmpeg` child process stopped cleanly when dropped.
    child: Option<Child>,
}

impl Drop for AudioChildGuard
{
    /// Terminates and reaps any active `ffmpeg` audio subprocess cleanly.
    fn drop(&mut self)
    {
        if let Some(mut child) = self.child.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Bounded lookahead ring buffer for streaming multi-minute audio tracks at ~1 s RAM.
pub struct PcmRing
{
    // Guarded interleaved stereo `f32` samples and rewind/failure flags.
    state: Mutex<PcmRingState>,

    // Wakes waiting producer when consumer pops samples or triggers rewind.
    changed: Condvar,

    // Target sample capacity (~1 second of stereo `f32` frames).
    capacity_samples: usize,

    // Minimum buffered sample threshold signaling initial playback readiness.
    prefill_samples: usize,
}

/// Synchronized state backing `PcmRing`.
struct PcmRingState
{
    // Interleaved `f32` samples ordered strictly from playback frontier.
    samples: VecDeque<f32>,

    // Pending rewind request set when `CoupledLoop` restarts from frame 0.
    rewind_requested: bool,

    // Terminal error flag when `ffmpeg` encounters fatal decode failure.
    failed: bool,
}

impl PcmRing
{
    /// Creates a bounded 1-second stereo ring buffer calibrated for `sample_rate`.
    pub(crate) fn new(sample_rate: u32, channels: u16) -> Arc<Self>
    {
        let channels_usize = usize::from(channels.max(1));
        let rate_usize = usize::try_from(sample_rate.max(8_000)).unwrap_or(48_000);
        let capacity_samples = rate_usize.saturating_mul(channels_usize);
        let prefill_samples =
            (capacity_samples * STREAM_PREFILL_NUMERATOR) / STREAM_PREFILL_DENOMINATOR;

        Arc::new(Self {
            state: Mutex::new(PcmRingState {
                samples: VecDeque::with_capacity(capacity_samples),
                rewind_requested: false,
                failed: false,
            }),
            changed: Condvar::new(),
            capacity_samples,
            prefill_samples,
        })
    }

    /// Requests immediate stream rewind cleanly for coupled video loop restart.
    pub(crate) fn request_rewind(&self)
    {
        let mut guard = match self.state.lock()
        {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.samples.clear();
        guard.rewind_requested = true;
        self.changed.notify_all();
    }

    /// Wakes any blocked producer thread during shutdown cleanly.
    pub(crate) fn wake_all(&self)
    {
        self.changed.notify_all();
    }

    /// Pops up to `dst.len()` interleaved samples without blocking (`0` if lock contended).
    pub(crate) fn try_pop_interleaved(&self, dst: &mut [f32]) -> usize
    {
        let Ok(mut guard) = self.state.try_lock()
        else
        {
            return 0;
        };

        let take_count = dst.len().min(guard.samples.len());
        for slot in &mut dst[..take_count]
        {
            *slot = guard.samples.pop_front().unwrap_or(0.0);
        }
        if take_count > 0
        {
            self.changed.notify_all();
        }
        take_count
    }

    /// Pushes decoded `chunk` into `self.state`, throttling cleanly under full backpressure.
    fn push_chunk(
        &self,
        chunk: &[f32],
        cancel: &AtomicBool,
        prefill_signal: &mut impl FnMut(),
    ) -> PushOutcome
    {
        let mut guard = match self.state.lock()
        {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        loop
        {
            if cancel.load(Ordering::Acquire)
            {
                return PushOutcome::Cancelled;
            }
            if guard.rewind_requested
            {
                guard.rewind_requested = false;
                return PushOutcome::Rewind;
            }
            if guard.samples.len() < self.capacity_samples
            {
                break;
            }

            guard = match self
                .changed
                .wait_timeout(guard, Duration::from_millis(RING_WAIT_SLICE_MS))
            {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }

        for &sample in chunk
        {
            guard.samples.push_back(sample);
        }
        let ready = guard.samples.len() >= self.prefill_samples;
        self.changed.notify_all();
        drop(guard);

        if ready
        {
            prefill_signal();
        }
        PushOutcome::Continue
    }

    /// Waits briefly at stream EOF before respawning for another pass unless rewind/cancel triggers.
    fn consume_rewind_or_finish_pass(&self, cancel: &AtomicBool) -> bool
    {
        let guard = match self.state.lock()
        {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut guard = guard;
        if guard.rewind_requested
        {
            guard.rewind_requested = false;
            return !cancel.load(Ordering::Acquire);
        }
        while guard.samples.len() >= self.capacity_samples / 2
        {
            if cancel.load(Ordering::Acquire)
            {
                return false;
            }
            if guard.rewind_requested
            {
                guard.rewind_requested = false;
                return true;
            }
            guard = match self
                .changed
                .wait_timeout(guard, Duration::from_millis(RING_WAIT_SLICE_MS))
            {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
        !cancel.load(Ordering::Acquire)
    }

    /// Marks stream failure and wakes all waiters cleanly.
    fn mark_failed(&self)
    {
        let mut guard = match self.state.lock()
        {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.failed = true;
        self.changed.notify_all();
    }
}

/// Outcome returned when attempting to enqueue PCM samples into `PcmRing`.
enum PushOutcome
{
    Continue,
    Rewind,
    Cancelled,
}

/// Decodes an entire short audio track cleanly into interleaved stereo `f32` PCM in memory.
pub(crate) fn decode_buffered_pcm(
    path: &Path,
    probe: &AudioProbe,
    sample_rate: u32,
    channels: u16,
    cap_duration_secs: Option<f64>,
    cancel: &AtomicBool,
) -> Result<Arc<Vec<f32>>, AudioError>
{
    let mut cmd = build_ffmpeg_audio_cmd(path, probe, sample_rate, channels, cap_duration_secs);
    let mut child = cmd.spawn().map_err(|err| map_spawn_err(path, err))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AudioError::DecodeFailed {
            path: path.to_path_buf(),
            reason: String::from("Failed capturing ffmpeg audio stdout stream"),
        })?;
    let _guard = AudioChildGuard { child: Some(child) };

    let mut samples = Vec::new();
    let mut raw_buf = vec![0_u8; DECODE_CHUNK_BYTES];
    let mut carry_len = 0_usize;

    loop
    {
        if cancel.load(Ordering::Acquire)
        {
            return Err(AudioError::DecodeFailed {
                path: path.to_path_buf(),
                reason: String::from("Audio decoding cancelled"),
            });
        }

        let n = stdout
            .read(&mut raw_buf[carry_len..])
            .map_err(|err| AudioError::DecodeFailed {
                path: path.to_path_buf(),
                reason: err.to_string(),
            })?;
        if n == 0
        {
            break;
        }

        let valid = carry_len + n;
        let full_bytes = valid - (valid % PCM_BYTES_PER_SAMPLE);
        if samples
            .len()
            .saturating_mul(PCM_BYTES_PER_SAMPLE)
            .saturating_add(full_bytes)
            > BUFFERED_PCM_HARD_CAP_BYTES
        {
            break;
        }

        append_le_f32_samples(&raw_buf[..full_bytes], &mut samples);
        carry_len = valid - full_bytes;
        raw_buf.copy_within(full_bytes..valid, 0);
    }

    if samples.is_empty()
    {
        return Err(AudioError::DecodeFailed {
            path: path.to_path_buf(),
            reason: String::from("Audio stream decoded zero PCM samples"),
        });
    }

    Ok(Arc::new(samples))
}

/// Continuously decodes long audio streams into `ring`, honoring cancellation and loop rewinds.
#[expect(clippy::too_many_arguments)]
pub(crate) fn stream_pcm_to_ring(
    path: &Path,
    probe: &AudioProbe,
    sample_rate: u32,
    channels: u16,
    cap_duration_secs: Option<f64>,
    ring: &PcmRing,
    cancel: &AtomicBool,
    mut on_prefilled: impl FnMut(),
)
{
    let mut signaled_ready = false;
    let mut maybe_notify = || {
        if !signaled_ready
        {
            signaled_ready = true;
            on_prefilled();
        }
    };

    while !cancel.load(Ordering::Acquire)
    {
        let mut cmd = build_ffmpeg_audio_cmd(path, probe, sample_rate, channels, cap_duration_secs);
        let Ok(mut child) = cmd.spawn()
        else
        {
            ring.mark_failed();
            maybe_notify();
            return;
        };
        let Some(mut stdout) = child.stdout.take()
        else
        {
            ring.mark_failed();
            maybe_notify();
            return;
        };
        let _guard = AudioChildGuard { child: Some(child) };

        let mut raw_buf = vec![0_u8; DECODE_CHUNK_BYTES];
        let mut decoded_buf = Vec::with_capacity(DECODE_CHUNK_BYTES / PCM_BYTES_PER_SAMPLE);
        let mut carry_len = 0_usize;

        loop
        {
            if cancel.load(Ordering::Acquire)
            {
                return;
            }

            let Ok(n) = stdout.read(&mut raw_buf[carry_len..])
            else
            {
                ring.mark_failed();
                maybe_notify();
                return;
            };
            if n == 0
            {
                maybe_notify();
                break;
            }

            let valid = carry_len + n;
            let full_bytes = valid - (valid % PCM_BYTES_PER_SAMPLE);
            decoded_buf.clear();
            append_le_f32_samples(&raw_buf[..full_bytes], &mut decoded_buf);
            carry_len = valid - full_bytes;
            raw_buf.copy_within(full_bytes..valid, 0);

            match ring.push_chunk(&decoded_buf, cancel, &mut maybe_notify)
            {
                PushOutcome::Continue =>
                {}
                PushOutcome::Rewind => break,
                PushOutcome::Cancelled => return,
            }
        }

        drop(_guard);
        if !ring.consume_rewind_or_finish_pass(cancel)
        {
            break;
        }
    }
}

/// Converts aligned 4-byte little-endian `f32` PCM slices cleanly into finite samples in `dst`.
fn append_le_f32_samples(aligned_bytes: &[u8], dst: &mut Vec<f32>)
{
    for &chunk in aligned_bytes.as_chunks::<PCM_BYTES_PER_SAMPLE>().0
    {
        let val = f32::from_le_bytes(chunk);
        dst.push(if val.is_finite() { val } else { 0.0 });
    }
}

/// Builds a quiet single-stream `ffmpeg` command emitting interleaved `f32le` PCM over `stdout`.
fn build_ffmpeg_audio_cmd(
    path: &Path,
    probe: &AudioProbe,
    sample_rate: u32,
    channels: u16,
    cap_duration_secs: Option<f64>,
) -> Command
{
    let map_spec = format!("0:{}", probe.stream_index);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", &map_spec, "-vn"]);
    if let Some(secs) = cap_duration_secs.filter(|s| s.is_finite() && *s > 0.0)
    {
        cmd.args(["-t", &format!("{secs:.6}")]);
    }
    cmd.args([
        "-f",
        "f32le",
        "-acodec",
        "pcm_f32le",
        "-ac",
        &channels.to_string(),
        "-ar",
        &sample_rate.to_string(),
        "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    cmd
}

/// Maps `ffmpeg` spawn I/O errors cleanly into typed `AudioError`.
fn map_spawn_err(path: &Path, err: std::io::Error) -> AudioError
{
    if err.kind() == ErrorKind::NotFound
    {
        AudioError::UnavailableTool { tool: "ffmpeg" }
    }
    else
    {
        AudioError::DecodeFailed {
            path: PathBuf::from(path),
            reason: format!("Failed spawning ffmpeg audio decoder: {err}"),
        }
    }
}
