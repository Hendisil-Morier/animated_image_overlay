//! Integration tests verifying audio probing, lock-free PCM mixing, A/V clock sync, and engine teardown.

#[path = "support/temp_dir.rs"]
mod temp_dir;

use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use animated_image_widget::{
    audio::{
        AVSyncMode, AudioEngine, AudioProbe, AudioStorageMode, AudioSyncCore, AudioTrackData,
    },
    error::AudioError,
};
use temp_dir::TempDecodeDir;

fn run_ffmpeg_fixture(path: &Path, args: &[&str])
{
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(path)
        .status()
        .expect("ffmpeg must be available in test environment");
    assert!(status.success(), "ffmpeg fixture failed: {status}");
}

#[test]
fn audio_probe_detects_tracks_handles_silent_videos_and_selects_storage_mode()
{
    let temp = TempDecodeDir::new("audio_probe");
    let silent_mp4 = temp.path().join("silent.mp4");
    let av_mp4 = temp.path().join("with_audio.mp4");
    let wav_path = temp.path().join("tone.wav");

    run_ffmpeg_fixture(
        &silent_mp4,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.2:size=64x48:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    run_ffmpeg_fixture(
        &av_mp4,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.3:size=64x48:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.3",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
    );
    run_ffmpeg_fixture(&wav_path, &["-f", "lavfi", "-i", "sine=frequency=880:duration=0.25"]);

    assert_eq!(
        AudioProbe::probe_optional(&silent_mp4).expect("silent mp4 should probe cleanly"),
        None
    );
    assert!(matches!(
        AudioProbe::probe_required(&silent_mp4),
        Err(AudioError::ProbeFailed { .. })
    ));

    let probed_av = AudioProbe::probe_required(&av_mp4).expect("mp4 audio should probe");
    assert_eq!(probed_av.stream_index, 1);
    assert_eq!(probed_av.storage_mode(48_000, 2, None), AudioStorageMode::Buffered);

    let probed_wav = AudioProbe::probe_required(&wav_path).expect("wav audio should probe");
    assert_eq!(probed_wav.stream_index, 0);
    assert_eq!(probed_wav.storage_mode(48_000, 2, None), AudioStorageMode::Buffered);

    let long_probe = AudioProbe {
        stream_index: 0,
        duration_secs: Some(600.0),
    };
    assert_eq!(
        long_probe.storage_mode(48_000, 2, None),
        AudioStorageMode::Streaming
    );
    assert_eq!(
        long_probe.storage_mode(48_000, 2, Some(2.0)),
        AudioStorageMode::Buffered
    );
}

#[test]
fn audio_sync_core_mixes_samples_smoothly_and_maintains_monotonic_coupled_clock()
{
    let t0 = Instant::now();
    let core = AudioSyncCore::new(48_000, 2, AVSyncMode::CoupledLoop, t0, true, 0.5, false);
    assert_eq!(core.sample_rate(), 48_000);
    assert_eq!(core.channels(), 2);

    // Build constant unit-amplitude PCM loop so ramp envelope and cursor resets are directly observable.
    let mut loop_pcm = Vec::with_capacity(480 * 2);
    for idx in 0..480
    {
        let marker = if idx < 240 { 0.8_f32 } else { 0.4_f32 };
        loop_pcm.push(marker);
        loop_pcm.push(marker);
    }
    core.set_track(AudioTrackData::Buffered(Arc::new(loop_pcm)));
    assert!(core.is_startup_ready(t0));

    // Render 480 frames (10 ms @ 48 kHz): click-free ramp rises smoothly from near 0 toward target gain 0.5.
    let mut out = vec![0.0_f32; 480 * 2];
    core.render_interleaved_f32(&mut out, t0 + Duration::from_millis(10));
    assert!(out[0].abs() < 0.05, "first frame must fade in cleanly from 0");
    assert!((out[479 * 2] - (0.4 * 0.5)).abs() < 0.01);

    let media_after_first = core
        .coupled_media_time_us(t0 + Duration::from_millis(10))
        .expect("coupled clock active with device and track");
    assert!((media_after_first - 10_000.0).abs() < 50.0);

    // Muting silences output cleanly while hardware clock continues advancing in lockstep.
    core.set_muted(true);
    core.render_interleaved_f32(&mut out, t0 + Duration::from_millis(20));
    assert!(out.iter().all(|&sample| sample.abs() < 0.4));
    core.render_interleaved_f32(&mut out, t0 + Duration::from_millis(30));
    assert!(out.iter().all(|&sample| sample == 0.0));
    let media_while_muted = core
        .coupled_media_time_us(t0 + Duration::from_millis(30))
        .unwrap();
    assert!((media_while_muted - 30_000.0).abs() < 50.0);

    // Pausing freezes rendered frame clock completely.
    core.set_paused(true);
    core.render_interleaved_f32(&mut out, t0 + Duration::from_millis(100));
    let media_paused = core
        .coupled_media_time_us(t0 + Duration::from_millis(100))
        .unwrap();
    assert_eq!(media_paused, media_while_muted);

    // Resuming + unmuting + coupled loop wrap realigns cursor to index 0 (`0.8` for `0..240`, `0.4` for `240..`) without time going backward.
    core.set_paused(false);
    core.set_muted(false);
    core.notify_loop_restart();
    let mut tiny = vec![0.0_f32; 260 * 2];
    core.render_interleaved_f32(&mut tiny, t0 + Duration::from_millis(105));
    assert!((tiny[239 * 2] - (0.8 * 0.5)).abs() < 0.02);
    assert!((tiny[250 * 2] - (0.4 * 0.5)).abs() < 0.02);
    let media_resumed = core
        .coupled_media_time_us(t0 + Duration::from_millis(105))
        .unwrap();
    assert!(media_resumed > media_paused);

    let mut i16_buf = vec![0_i16; 64 * 2];
    let mut scratch = Vec::new();
    animated_image_widget::audio::write_cpal_output(
        &core,
        &mut i16_buf,
        2,
        &mut scratch,
        t0 + Duration::from_millis(110),
    );
    assert!(i16_buf.iter().any(|&s| s > 0));

    // Independent mode intentionally declines driving `PlaybackClock`.
    let independent = AudioSyncCore::new(
        48_000,
        2,
        AVSyncMode::IndependentAudioLoop,
        t0,
        true,
        1.0,
        false,
    );
    independent.set_track(AudioTrackData::Buffered(Arc::new(vec![0.1; 100])));
    assert_eq!(independent.coupled_media_time_us(t0), None);
}

#[test]
fn audio_engine_opens_decodes_real_mp4_audio_and_stops_cleanly_even_headless()
{
    let temp = TempDecodeDir::new("audio_engine_live");
    let mp4_path = temp.path().join("av_clip.mp4");
    run_ffmpeg_fixture(
        &mp4_path,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.25:size=64x48:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=520:duration=0.25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
    );

    let probe = AudioProbe::probe_required(&mp4_path).expect("av clip should carry audio");
    let woken = Arc::new(AtomicBool::new(false));
    let cb = Arc::new({
        let woken = Arc::clone(&woken);
        move || woken.store(true, Ordering::Release)
    });

    let mut engine = AudioEngine::open(
        &mp4_path,
        probe,
        AVSyncMode::CoupledLoop,
        None,
        Instant::now(),
        0.75,
        false,
        Some(cb),
    )
    .expect("audio engine open must tolerate both live and headless environments");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !engine.is_startup_ready(Instant::now()) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(woken.load(Ordering::Acquire) || engine.is_startup_ready(Instant::now()));

    engine.set_muted(true);
    engine.set_volume(0.25);
    engine.set_paused(true);
    engine.notify_loop_restart();
    let snap = engine.stats_snapshot();
    assert!(snap.muted);
    assert!((snap.volume - 0.25).abs() < 1e-4);

    engine.stop();
}
