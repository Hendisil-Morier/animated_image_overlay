//! Integration tests covering MP4/WebM container probing, `ffmpeg` PAM/showinfo decoding, and `.lraw` caching.

#[path = "support/mod.rs"]
mod support;

use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use animated_image_widget::{
    cache::FrameCache,
    decode::FrameDecoder,
    error::{CacheError, OverlayError},
};
use support::{RecordedStream, RecordingSink, TempDecodeDir};

fn generate_test_video(path: &Path, args: &[&str])
{
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(path)
        .status()
        .expect("ffmpeg must be available in test environment");
    assert!(
        status.success(),
        "ffmpeg fixture command failed for {}: {status}",
        path.display()
    );
}

#[test]
fn mp4_cold_decode_scales_streams_vfr_timings_and_reuses_lraw_warm_cache()
{
    let temp = TempDecodeDir::new("mp4_short_cache");
    let mp4_path = temp.path().join("sample.mp4");
    generate_test_video(
        &mp4_path,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.4:size=160x80:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    );

    let cache_dir = temp.path().join("cache");
    let wakeup = Arc::new(AtomicBool::new(false));
    let cb = Arc::new({
        let wakeup = Arc::clone(&wakeup);
        move || wakeup.store(true, Ordering::Release)
    });

    let cache =
        FrameCache::open_in_dir(&mp4_path, 80, 0, &cache_dir, 64 * 1024 * 1024, 30, Some(cb))
            .expect("short mp4 should decode cleanly");

    assert_eq!(cache.frame_dimensions(), (80, 40));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !cache.is_decode_done() && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        cache.is_decode_done(),
        "background mp4 decode did not finish"
    );

    let count = cache.frame_count();
    assert!(
        (9..=11).contains(&count),
        "expected ~10 frames, got {count}"
    );
    cache.with_delays_us(|delays, done| {
        assert!(done);
        assert_eq!(delays.len(), count);
        for &us in delays
        {
            assert!(
                (35_000..=45_000).contains(&us),
                "expected ~40_000us delay at 25fps, got {us}"
            );
        }
    });

    for idx in 0..count
    {
        let argb = cache
            .frame_argb(idx)
            .expect("decoded mp4 frame must be readable");
        assert_eq!(argb.len(), 80 * 40 * 4);
    }

    drop(cache);

    // Warm reopen succeeds cleanly even if source path exists and cache JSON/LRAW persisted.
    let warm = FrameCache::open_in_dir(&mp4_path, 80, 0, &cache_dir, 64 * 1024 * 1024, 30, None)
        .expect("warm mp4 cache hit must open");
    assert!(warm.is_decode_done());
    assert_eq!(warm.frame_dimensions(), (80, 40));
    assert_eq!(warm.frame_count(), count);
}

#[test]
fn mp4_honors_max_frames_one_sync_and_rotation_metadata()
{
    let temp = TempDecodeDir::new("mp4_max1_rot");
    let raw_mp4 = temp.path().join("raw.mp4");
    let rotated_mp4 = temp.path().join("portrait.mp4");
    generate_test_video(
        &raw_mp4,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=0.2:size=120x60:rate=25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    generate_test_video(
        &rotated_mp4,
        &[
            "-display_rotation",
            "90",
            "-i",
            raw_mp4.to_str().unwrap(),
            "-c",
            "copy",
        ],
    );

    let recorded = Arc::new(Mutex::new(RecordedStream::default()));
    let sink = RecordingSink {
        state: Arc::clone(&recorded),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (first, worker) = FrameDecoder::open_stream(&rotated_mp4, 200, 1, cancel, Box::new(sink))
        .expect("single-frame rotated mp4 should decode synchronously");

    assert!(worker.is_none(), "-n 1 must decode synchronously");
    // Coded 120x60 rotated 90 degrees auto-rotates to upright 60x120 cleanly.
    assert_eq!((first.width, first.height), (60, 120));

    let state = recorded.lock().unwrap();
    assert_eq!(state.frames.len(), 1);
    assert!(state.finished);
}

#[test]
fn corrupt_mp4_returns_decode_error_without_hanging()
{
    let temp = TempDecodeDir::new("mp4_corrupt");
    let bad_mp4 = temp.path().join("broken.mp4");
    fs::write(&bad_mp4, b"\x00\x00\x00\x18ftypisomcorruptgarbagebytes").unwrap();

    let sink = RecordingSink {
        state: Arc::new(Mutex::new(RecordedStream::default())),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let err = match FrameDecoder::open_stream(&bad_mp4, 120, 0, cancel, Box::new(sink))
    {
        Ok(_) => panic!("corrupt mp4 must return decode error"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        OverlayError::Cache(CacheError::Decode { .. })
    ));
}
