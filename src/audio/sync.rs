//! Lock-free PCM mixer, smooth pause/mute slew filter, and monotonic A/V sync clock.

// Synchronized audio state, sample atomics, and timing snapshots.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};

// Bounded lookahead PCM ring used for multi-minute streaming audio.
use super::decode::{PcmRing, TARGET_PCM_CHANNELS};

// Maximum intra-callback monotonic interpolation horizon in microseconds (`25 ms`).
const MAX_INTERP_PERIOD_US: u64 = 25_000;

// Maximum time visual playback waits at frame 0 for initial audio prefill before proceeding (`150 ms`).
const MAX_AUDIO_PREFILL_WAIT_MS: u128 = 150;

// Ramp duration divisor (`sample_rate / 200` -> `5 ms`) preventing clicks across pause/mute/reset.
const FADE_RAMP_DIVISOR: u32 = 200;

/// Defines how audio timeline interacts with overlay visual looping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AVSyncMode
{
    /// Embedded video audio: hardware sample clock paces visual frames and visual wrap aligns audio.
    CoupledLoop,

    /// External companion `--audio`: audio loops cleanly on its own duration alongside images.
    IndependentAudioLoop,
}

/// Snapshot of live audio preferences, hardware availability, and sync mode for `--stats`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStatsSnapshot
{
    /// Current volume multiplier in `0.0..=1.0`.
    pub volume: f64,

    /// Whether audio output is muted.
    pub muted: bool,

    /// Whether `cpal` opened an active output device.
    pub device_available: bool,

    /// Active audio clock coupling mode.
    pub sync_mode: AVSyncMode,
}

/// Decoded audio track backing consumed by `AudioSyncCore::render_interleaved_f32`.
pub enum AudioTrackData
{
    /// Background `ffmpeg` thread is still extracting or prefilling initial PCM samples.
    Loading,

    /// Complete short track preloaded cleanly in interleaved stereo `f32` PCM (`channels = 2`).
    Buffered(Arc<Vec<f32>>),

    /// Long track streamed incrementally through a bounded ~1-second PCM ring buffer.
    Streaming(Arc<PcmRing>),

    /// Audio stream absent or failed to decode; engine stays silent without blocking visuals.
    Unavailable,
}

/// Cursor and smooth gain state updated exclusively by the consumer mixing pass.
struct MixerCursor
{
    // Active backing data slot swapped cleanly once background decode finishes.
    track: AudioTrackData,

    // Frame offset inside `AudioTrackData::Buffered` (`0..total_frames`).
    cursor_frames: usize,

    // Last observed `loop_reset_epoch` from `CoupledLoop` wraps.
    last_epoch: u64,

    // Smoothed linear gain `[0.0, 1.0]` preventing clicks on mute/pause/volume transitions.
    gain_smooth: f32,
}

/// Thread-safe lock-free control plane and sample mixer shared across UI, decoder, and `cpal`.
pub struct AudioSyncCore
{
    // Hardware or fallback PCM playback sample rate in Hz (e.g. `48_000`).
    sample_rate: u32,

    // Source PCM channel count (`2` = stereo).
    channels: u16,

    // Sync relationship between audio samples and visual playback.
    sync_mode: AVSyncMode,

    // Monotonic application start instant anchoring callback interpolation timestamps.
    start_instant: Instant,

    // True when a live audio device output stream was opened successfully.
    device_available: bool,

    // True when user paused overlay playback (`Space`).
    paused: AtomicBool,

    // True when user muted audio (`m` / `--mute`).
    muted: AtomicBool,

    // Bit-encoded `f32` output gain multiplier clamped in `0.0..=1.0`.
    volume_bits: AtomicU32,

    // Set once initial buffered load or ring prefill completes (or fails).
    audio_ready: AtomicBool,

    // Cumulative sample frames rendered while unpaused and track-ready (strictly monotonic).
    total_rendered_frames: AtomicU64,

    // Microseconds since `start_instant` observed on the latest rendered hardware callback.
    last_callback_us: AtomicU64,

    // Monotonic high-water mark guaranteeing `coupled_media_time_us()` never steps backward.
    high_water_us: AtomicU64,

    // Incremented when coupled video wraps from end back to frame 0 so audio stays phase-locked.
    loop_reset_epoch: AtomicU64,

    // Guarded track backing and cursor; realtime callbacks use `try_lock` to avoid blocking.
    mixer: Mutex<MixerCursor>,
}

impl AudioSyncCore
{
    /// Creates a new synchronized mixer core in `Loading` state with initial volume and mute prefs.
    pub fn new(
        sample_rate: u32,
        channels: u16,
        sync_mode: AVSyncMode,
        start_instant: Instant,
        device_available: bool,
        initial_volume: f64,
        initial_muted: bool,
    ) -> Arc<Self>
    {
        let clamped_vol = clamp_f32_volume(initial_volume);
        Arc::new(Self {
            sample_rate: sample_rate.max(8_000),
            channels: channels.max(1),
            sync_mode,
            start_instant,
            device_available,
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(initial_muted),
            volume_bits: AtomicU32::new(clamped_vol.to_bits()),
            audio_ready: AtomicBool::new(false),
            total_rendered_frames: AtomicU64::new(0),
            last_callback_us: AtomicU64::new(0),
            high_water_us: AtomicU64::new(0),
            loop_reset_epoch: AtomicU64::new(0),
            mixer: Mutex::new(MixerCursor {
                track: AudioTrackData::Loading,
                cursor_frames: 0,
                last_epoch: 0,
                gain_smooth: 0.0,
            }),
        })
    }

    /// Replaces the backing track data cleanly (typically transitioning from `Loading`).
    pub fn set_track(&self, track: AudioTrackData)
    {
        let ready = !matches!(track, AudioTrackData::Loading);
        let mut guard = match self.mixer.lock()
        {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.track = track;
        guard.cursor_frames = 0;
        drop(guard);
        if ready
        {
            self.audio_ready.store(true, Ordering::Release);
        }
    }

    /// Marks audio prefill readiness cleanly (`Streaming` ring signals once prefill buffer fills).
    pub fn mark_ready(&self)
    {
        self.audio_ready.store(true, Ordering::Release);
    }

    /// Updates pause flag immediately without blocking.
    pub fn set_paused(&self, paused: bool)
    {
        self.paused.store(paused, Ordering::Release);
    }

    /// Sets mute toggle and returns previous state.
    pub fn set_muted(&self, muted: bool)
    {
        self.muted.store(muted, Ordering::Release);
    }

    /// Updates target output gain multiplier clamped in `0.0..=1.0`.
    pub fn set_volume(&self, volume: f64)
    {
        self.volume_bits
            .store(clamp_f32_volume(volume).to_bits(), Ordering::Release);
    }

    /// Signals a coupled video loop wrap to realign buffered or streaming audio back to sample 0.
    pub fn notify_loop_restart(&self)
    {
        if self.sync_mode == AVSyncMode::CoupledLoop
        {
            self.loop_reset_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Returns snapshot telemetry for `--stats` formatting.
    pub fn stats_snapshot(&self) -> AudioStatsSnapshot
    {
        AudioStatsSnapshot {
            volume: f64::from(f32::from_bits(self.volume_bits.load(Ordering::Acquire))),
            muted: self.muted.load(Ordering::Acquire),
            device_available: self.device_available,
            sync_mode: self.sync_mode,
        }
    }

    /// Reports whether initial audio load/prefill completed or passed startup wait grace period.
    pub fn is_startup_ready(&self, now: Instant) -> bool
    {
        self.audio_ready.load(Ordering::Acquire)
            || now.saturating_duration_since(self.start_instant).as_millis()
                >= MAX_AUDIO_PREFILL_WAIT_MS
    }

    /// Returns monotonic cumulative media clock in microseconds for `CoupledLoop` A/V sync.
    ///
    /// Returns `None` when running `IndependentAudioLoop` or without an active hardware stream,
    /// allowing `PlaybackClock` to use standard monotonic system time.
    pub fn coupled_media_time_us(&self, now: Instant) -> Option<f64>
    {
        if self.sync_mode != AVSyncMode::CoupledLoop
            || !self.device_available
            || !self.audio_ready.load(Ordering::Acquire)
        {
            return None;
        }

        let has_track = match self.mixer.try_lock()
        {
            Ok(guard) => !matches!(guard.track, AudioTrackData::Unavailable),
            Err(_) => true,
        };
        if !has_track
        {
            return None;
        }

        let rendered = self.total_rendered_frames.load(Ordering::Acquire);
        let sample_us = ((rendered as u128).saturating_mul(1_000_000) / u128::from(self.sample_rate))
            .min(u128::from(u64::MAX)) as u64;
        let now_us = now
            .saturating_duration_since(self.start_instant)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let cb_us = self.last_callback_us.load(Ordering::Acquire);
        let interp_us = if rendered > 0 && !self.paused.load(Ordering::Acquire)
        {
            now_us.saturating_sub(cb_us).min(MAX_INTERP_PERIOD_US)
        }
        else
        {
            0
        };

        let raw_us = sample_us.saturating_add(interp_us);
        let prev = self.high_water_us.fetch_max(raw_us, Ordering::AcqRel);
        Some(prev.max(raw_us) as f64)
    }

    /// Renders `frame_count` stereo `f32` PCM frames into `out_pcm` and updates sample clocks cleanly.
    ///
    /// Designed to be lock-free under steady playback and usable identically by both `cpal` output
    /// callbacks and deterministic headless audio tests.
    pub fn render_interleaved_f32(&self, out_pcm: &mut [f32], now: Instant)
    {
        let src_channels = usize::from(TARGET_PCM_CHANNELS);
        let frame_count = out_pcm.len() / src_channels;
        if frame_count == 0
        {
            return;
        }

        let paused = self.paused.load(Ordering::Acquire);
        let muted = self.muted.load(Ordering::Acquire);
        let target_gain = if paused || muted
        {
            0.0
        }
        else
        {
            f32::from_bits(self.volume_bits.load(Ordering::Acquire))
        };

        let Ok(mut guard) = self.mixer.try_lock()
        else
        {
            out_pcm.fill(0.0);
            return;
        };

        let cur_epoch = self.loop_reset_epoch.load(Ordering::Acquire);
        if cur_epoch != guard.last_epoch
        {
            guard.last_epoch = cur_epoch;
            guard.cursor_frames = 0;
            guard.gain_smooth = 0.0;
            if let AudioTrackData::Streaming(ring) = &guard.track
            {
                ring.request_rewind();
            }
        }

        let fade_steps = (self.sample_rate / FADE_RAMP_DIVISOR).max(1) as f32;
        let gain_step = 1.0 / fade_steps;
        let track_active = !matches!(
            guard.track,
            AudioTrackData::Loading | AudioTrackData::Unavailable
        );

        if paused || !track_active
        {
            out_pcm.fill(0.0);
            guard.gain_smooth = slew_toward(guard.gain_smooth, 0.0, gain_step * frame_count as f32);
            return;
        }

        let mut cursor_frames = guard.cursor_frames;
        let mut gain = guard.gain_smooth;
        match &guard.track
        {
            AudioTrackData::Buffered(samples) =>
            {
                let total_frames = samples.len() / src_channels;
                if total_frames == 0
                {
                    out_pcm.fill(0.0);
                }
                else
                {
                    for dst_frame in out_pcm.chunks_exact_mut(src_channels)
                    {
                        gain = slew_toward(gain, target_gain, gain_step);
                        let base = (cursor_frames % total_frames) * src_channels;
                        dst_frame[0] = samples[base] * gain;
                        dst_frame[1] = samples[base + 1] * gain;
                        cursor_frames = (cursor_frames + 1) % total_frames;
                    }
                }
            }
            AudioTrackData::Streaming(ring) =>
            {
                let popped = ring.try_pop_interleaved(out_pcm);
                for dst_frame in out_pcm[..popped].chunks_exact_mut(src_channels)
                {
                    gain = slew_toward(gain, target_gain, gain_step);
                    dst_frame[0] *= gain;
                    dst_frame[1] *= gain;
                }
                out_pcm[popped..].fill(0.0);
            }
            AudioTrackData::Loading | AudioTrackData::Unavailable => out_pcm.fill(0.0),
        }

        guard.cursor_frames = cursor_frames;
        guard.gain_smooth = gain;
        drop(guard);

        let now_us = now
            .saturating_duration_since(self.start_instant)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        self.last_callback_us.store(now_us, Ordering::Release);
        self.total_rendered_frames
            .fetch_add(frame_count as u64, Ordering::AcqRel);
    }

    /// Exposes configured sample rate cleanly.
    pub fn sample_rate(&self) -> u32
    {
        self.sample_rate
    }

    /// Exposes configured device channel count cleanly.
    pub fn channels(&self) -> u16
    {
        self.channels
    }
}

/// Linearly glides `current` toward `target` by at most `max_delta` per step cleanly.
fn slew_toward(current: f32, target: f32, max_delta: f32) -> f32
{
    if current < target
    {
        (current + max_delta).min(target)
    }
    else if current > target
    {
        (current - max_delta).max(target)
    }
    else
    {
        current
    }
}

/// Clamps finite floating-point volume into `0.0..=1.0`, falling back to `1.0`.
fn clamp_f32_volume(volume: f64) -> f32
{
    if volume.is_finite()
    {
        volume.clamp(0.0, 1.0) as f32
    }
    else
    {
        1.0
    }
}
