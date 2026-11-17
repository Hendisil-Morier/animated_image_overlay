//! In-process `cpal` audio device lifecycle, sample-format adapter, and background loader.

// Standard paths, atomics, thread spawning, and monotonic timing.
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

// Cross-platform safe audio host, device enumeration, sample conversion, and stream traits.
use cpal::{
    Device, FromSample, I24, Sample, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

// Audio probe, ffmpeg PCM decode/stream loops, clock state, and optional UI wakeup hook.
use super::{
    decode::{PcmRing, TARGET_PCM_CHANNELS, decode_buffered_pcm, stream_pcm_to_ring},
    probe::{AudioProbe, AudioStorageMode},
    sync::{AVSyncMode, AudioStatsSnapshot, AudioSyncCore, AudioTrackData},
};
use crate::{cache::WakeupCallback, error::AudioError};

// Default fallback sample rate used when hardware output device is unavailable cleanly.
const FALLBACK_SAMPLE_RATE: u32 = 48_000;

/// Coordinates audio decoding, in-process `cpal` playback, mute/volume control, and A/V sync.
pub struct AudioEngine
{
    // Shared atomic control state and sample mixer accessed by UI and audio callback.
    sync: Arc<AudioSyncCore>,

    // Active `cpal` output stream kept alive until shutdown cleanly.
    stream: Option<Stream>,

    // Optional streaming PCM ring handle signaled during `stop()` cleanly.
    streaming_ring: Option<Arc<PcmRing>>,

    // Cancellation flag terminating background decode workers immediately on exit.
    cancel: Arc<AtomicBool>,

    // Joined background audio decoder handle.
    worker: Option<JoinHandle<()>>,
}

impl AudioEngine
{
    /// Starts audio decoding and playback for `source_path` / `probe`, tolerating headless systems cleanly.
    #[expect(clippy::too_many_arguments)]
    pub fn open(
        source_path: &Path,
        probe: AudioProbe,
        sync_mode: AVSyncMode,
        cap_duration_secs: Option<f64>,
        start_instant: Instant,
        initial_volume: f64,
        initial_muted: bool,
        wakeup: Option<WakeupCallback>,
    ) -> Result<Self, AudioError>
    {
        let (device_opt, sample_rate, out_channels) = query_default_output();
        let sync = AudioSyncCore::new(
            sample_rate,
            out_channels,
            sync_mode,
            start_instant,
            device_opt.is_some(),
            initial_volume,
            initial_muted,
        );

        let stream = match device_opt
        {
            Some((device, config)) => match build_and_start_stream(&device, &config, &sync)
            {
                Ok(started) => Some(started),
                Err(err) =>
                {
                    tracing::warn!(error = %err, "Failed starting cpal audio stream; continuing visuals silently");
                    None
                }
            },
            None => None,
        };

        let storage_mode = probe.storage_mode(sample_rate, TARGET_PCM_CHANNELS, cap_duration_secs);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_path = source_path.to_path_buf();

        let streaming_ring = match storage_mode
        {
            AudioStorageMode::Buffered => None,
            AudioStorageMode::Streaming => Some(PcmRing::new(sample_rate, TARGET_PCM_CHANNELS)),
        };

        if let Some(ring) = &streaming_ring
        {
            sync.set_track(AudioTrackData::Streaming(Arc::clone(ring)));
            let handle = spawn_streaming_worker(
                worker_path,
                probe,
                sample_rate,
                cap_duration_secs,
                Arc::clone(ring),
                Arc::clone(&sync),
                Arc::clone(&cancel),
                wakeup,
            )?;
            return Ok(Self {
                sync,
                stream,
                streaming_ring,
                cancel,
                worker: Some(handle),
            });
        }

        let handle = spawn_buffered_worker(
            worker_path,
            probe,
            sample_rate,
            cap_duration_secs,
            Arc::clone(&sync),
            Arc::clone(&cancel),
            wakeup,
        )?;

        Ok(Self {
            sync,
            stream,
            streaming_ring: None,
            cancel,
            worker: Some(handle),
        })
    }

    /// Mirrors animation pause/resume cleanly onto the audio mixer and clock.
    pub fn set_paused(&self, paused: bool)
    {
        self.sync.set_paused(paused);
    }

    /// Sets live mute state cleanly.
    pub fn set_muted(&self, muted: bool)
    {
        self.sync.set_muted(muted);
    }

    /// Updates live volume multiplier clamped to `0.0..=1.0`.
    pub fn set_volume(&self, volume: f64)
    {
        self.sync.set_volume(volume);
    }

    /// Notifies `CoupledLoop` audio mixer that visual playback wrapped back to frame 0.
    pub fn notify_loop_restart(&self)
    {
        self.sync.notify_loop_restart();
    }

    /// Checks whether initial startup audio buffer prefill completed (or grace timeout expired).
    pub fn is_startup_ready(&self, now: Instant) -> bool
    {
        self.sync.is_startup_ready(now)
    }

    /// Returns monotonic coupled audio time in microseconds when `CoupledLoop` drives A/V timing.
    pub fn coupled_media_time_us(&self, now: Instant) -> Option<f64>
    {
        self.sync.coupled_media_time_us(now)
    }

    /// Captures live audio preferences, hardware availability, and sync mode for `--stats`.
    pub fn stats_snapshot(&self) -> AudioStatsSnapshot
    {
        self.sync.stats_snapshot()
    }

    /// Stops output playback cleanly and joins active audio decoder threads.
    pub fn stop(&mut self)
    {
        self.cancel.store(true, Ordering::Release);
        if let Some(ring) = &self.streaming_ring
        {
            ring.wake_all();
        }
        drop(self.stream.take());
        if let Some(handle) = self.worker.take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioEngine
{
    /// Stops `cpal` streams and background decoder threads cleanly on engine teardown.
    fn drop(&mut self)
    {
        self.stop();
    }
}

/// Spawns the background streaming PCM decode thread.
#[expect(clippy::too_many_arguments)]
fn spawn_streaming_worker(
    path: PathBuf,
    probe: AudioProbe,
    sample_rate: u32,
    cap_duration_secs: Option<f64>,
    ring: Arc<PcmRing>,
    sync: Arc<AudioSyncCore>,
    cancel: Arc<AtomicBool>,
    wakeup: Option<WakeupCallback>,
) -> Result<JoinHandle<()>, AudioError>
{
    thread::Builder::new()
        .name(String::from("aiw-audio-stream"))
        .spawn(move || {
            let on_ready = || {
                sync.mark_ready();
                if let Some(cb) = &wakeup
                {
                    cb();
                }
            };
            stream_pcm_to_ring(
                &path,
                &probe,
                sample_rate,
                TARGET_PCM_CHANNELS,
                cap_duration_secs,
                &ring,
                &cancel,
                on_ready,
            );
        })
        .map_err(|err| AudioError::StreamFailed(err.to_string()))
}

/// Spawns the background buffered PCM decode thread.
fn spawn_buffered_worker(
    path: PathBuf,
    probe: AudioProbe,
    sample_rate: u32,
    cap_duration_secs: Option<f64>,
    sync: Arc<AudioSyncCore>,
    cancel: Arc<AtomicBool>,
    wakeup: Option<WakeupCallback>,
) -> Result<JoinHandle<()>, AudioError>
{
    thread::Builder::new()
        .name(String::from("aiw-audio-load"))
        .spawn(move || {
            match decode_buffered_pcm(
                &path,
                &probe,
                sample_rate,
                TARGET_PCM_CHANNELS,
                cap_duration_secs,
                &cancel,
            )
            {
                Ok(samples) => sync.set_track(AudioTrackData::Buffered(samples)),
                Err(err) =>
                {
                    tracing::warn!(path = %path.display(), error = %err, "Buffered audio decode failed");
                    sync.set_track(AudioTrackData::Unavailable);
                }
            }
            if let Some(cb) = &wakeup
            {
                cb();
            }
        })
        .map_err(|err| AudioError::StreamFailed(err.to_string()))
}

// Export `write_cpal_output` for deterministic multi-format callback verification in tests.
/// Mixes `sync` stereo `f32` audio cleanly into any `cpal` interleaved device slice `T`.
pub fn write_cpal_output<T>(
    sync: &AudioSyncCore,
    output: &mut [T],
    out_channels: usize,
    scratch: &mut Vec<f32>,
    now: Instant,
) where
    T: Sample + FromSample<f32>,
{
    let out_ch = out_channels.max(1);
    let frames = output.len() / out_ch;
    if frames == 0
    {
        return;
    }

    let needed = frames * usize::from(TARGET_PCM_CHANNELS);
    if scratch.len() < needed
    {
        scratch.resize(needed, 0.0);
    }
    let mix_buf = &mut scratch[..needed];
    sync.render_interleaved_f32(mix_buf, now);

    for (frame_idx, dst_frame) in output.chunks_exact_mut(out_ch).enumerate()
    {
        let left = mix_buf[frame_idx * 2];
        let right = mix_buf[frame_idx * 2 + 1];
        if out_ch == 1
        {
            dst_frame[0] = T::from_sample((left + right) * 0.5);
            continue;
        }

        dst_frame[0] = T::from_sample(left);
        dst_frame[1] = T::from_sample(right);
        for extra in &mut dst_frame[2..]
        {
            *extra = T::from_sample(0.0);
        }
    }
}

/// Queries the default system output device and its native stream configuration safely.
fn query_default_output() -> (Option<(Device, cpal::SupportedStreamConfig)>, u32, u16)
{
    let host = cpal::default_host();
    let Some(device) = host.default_output_device()
    else
    {
        tracing::warn!("No default audio output device detected; continuing visuals silently");
        return (None, FALLBACK_SAMPLE_RATE, TARGET_PCM_CHANNELS);
    };

    match device.default_output_config()
    {
        Ok(cfg) =>
        {
            let rate = cfg.sample_rate();
            let channels = cfg.channels();
            (Some((device, cfg)), rate, channels)
        }
        Err(err) =>
        {
            tracing::warn!(error = %err, "Failed querying default audio config; continuing silently");
            (None, FALLBACK_SAMPLE_RATE, TARGET_PCM_CHANNELS)
        }
    }
}

/// Builds and starts a `cpal::Stream` matching the hardware native sample format cleanly.
fn build_and_start_stream(
    device: &Device,
    supported: &cpal::SupportedStreamConfig,
    sync: &Arc<AudioSyncCore>,
) -> Result<Stream, AudioError>
{
    let fmt = supported.sample_format();
    let cfg: StreamConfig = (*supported).into();

    let stream = match fmt
    {
        SampleFormat::I8 => spawn_typed_stream::<i8>(device, &cfg, sync)?,
        SampleFormat::I16 => spawn_typed_stream::<i16>(device, &cfg, sync)?,
        SampleFormat::I24 => spawn_typed_stream::<I24>(device, &cfg, sync)?,
        SampleFormat::I32 => spawn_typed_stream::<i32>(device, &cfg, sync)?,
        SampleFormat::I64 => spawn_typed_stream::<i64>(device, &cfg, sync)?,
        SampleFormat::U8 => spawn_typed_stream::<u8>(device, &cfg, sync)?,
        SampleFormat::U16 => spawn_typed_stream::<u16>(device, &cfg, sync)?,
        SampleFormat::U32 => spawn_typed_stream::<u32>(device, &cfg, sync)?,
        SampleFormat::U64 => spawn_typed_stream::<u64>(device, &cfg, sync)?,
        SampleFormat::F32 => spawn_typed_stream::<f32>(device, &cfg, sync)?,
        SampleFormat::F64 => spawn_typed_stream::<f64>(device, &cfg, sync)?,
        other =>
        {
            return Err(AudioError::DeviceUnavailable(format!(
                "Unsupported output sample format: {other}"
            )));
        }
    };

    stream
        .play()
        .map_err(|err| AudioError::StreamFailed(err.to_string()))?;
    Ok(stream)
}

/// Builds one concrete `cpal` output stream driving `write_cpal_output::<T>`.
fn spawn_typed_stream<T>(
    device: &Device,
    cfg: &StreamConfig,
    sync: &Arc<AudioSyncCore>,
) -> Result<Stream, AudioError>
where
    T: Sample + FromSample<f32> + cpal::SizedSample,
{
    let channels = usize::from(cfg.channels);
    let callback_sync = Arc::clone(sync);
    let mut scratch = Vec::new();
    device
        .build_output_stream(
            *cfg,
            move |buf: &mut [T], _| {
                write_cpal_output(&callback_sync, buf, channels, &mut scratch, Instant::now());
            },
            |err| {
                tracing::warn!(error = %err, "Audio output stream callback warning");
            },
            None,
        )
        .map_err(|err| AudioError::StreamFailed(err.to_string()))
}
