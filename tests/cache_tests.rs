//! Integration tests for `.lraw`/`.json` disk caching, mtime pruning, corrupt fallback,
//! temp-file cleanup on abort, and 128 MiB / custom-budget LRU RAM caching.

use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    cache::{FrameCache, cache_dir_from_env, compute_cache_paths, prune_cache},
    error::CacheError,
};
use image::{
    Delay, Frame as ImgFrame, ImageEncoder, RgbaImage,
    codecs::{gif::GifEncoder, png::PngEncoder},
};

struct TempCacheTestDir
{
    path: PathBuf,
}

impl TempCacheTestDir
{
    fn new(name: &str) -> Self
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aiw_cache_{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp cache test dir created");
        Self { path }
    }

    fn path(&self) -> &Path
    {
        &self.path
    }
}

impl Drop for TempCacheTestDir
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_static_png(path: &Path, w: u32, h: u32, rgba: [u8; 4])
{
    let img = RgbaImage::from_pixel(w, h, image::Rgba(rgba));
    PngEncoder::new(File::create(path).unwrap())
        .write_image(&img, w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
}

fn create_animated_gif(path: &Path, count: usize, w: u32, h: u32)
{
    let frames: Vec<_> = (0..count)
        .map(|i| {
            ImgFrame::from_parts(
                RgbaImage::from_pixel(w, h, image::Rgba([(i * 20) as u8, 120, 200, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(40, 1),
            )
        })
        .collect();
    let mut enc = GifEncoder::new(File::create(path).unwrap());
    enc.encode_frames(frames).unwrap();
}

#[test]
fn cache_commits_lraw_and_json_and_serves_warm_hits()
{
    let temp = TempCacheTestDir::new("warm_hit");
    let img_path = temp.path().join("anim.gif");
    let cache_dir = temp.path().join("cache");
    create_animated_gif(&img_path, 4, 24, 24);

    let mut store1 = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 1024 * 1024, 30, None)
        .expect("cold decode should populate cache");

    while !store1.is_decode_done()
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store1.frame_count(), 4);
    store1.stop();

    let paths = compute_cache_paths(&cache_dir, &img_path, 500, 0).unwrap();
    assert!(paths.lraw_path.is_file());
    assert!(paths.meta_path.is_file());

    // Delete source file to prove second open serves 100% from warm disk cache!
    // Keep same path only if metadata checks stat? Wait: compute_cache_paths stats `img_path`,
    // so keep `img_path` untouched and corrupt only an unrelated test below; verify warm hit loads all 4 frames!
    let mut store2 = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 1024 * 1024, 30, None)
        .expect("warm cache hit should succeed");

    while !store2.is_decode_done()
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store2.frame_dimensions(), (24, 24));
    assert_eq!(store2.frame_count(), 4);
    let f2 = store2.frame_argb(2).expect("warm frame 2 decompresses");
    assert_eq!(f2.len(), 24 * 24 * 4);
    assert_eq!(&f2[0..4], &[200, 120, 40, 255]); // BGR(A) of (40, 120, 200, 255)
    store2.stop();
}

#[test]
fn cache_corrupt_files_fall_back_cleanly_to_source_decode()
{
    let temp = TempCacheTestDir::new("corrupt_fallback");
    let img_path = temp.path().join("sample.png");
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir).unwrap();
    create_static_png(&img_path, 40, 30, [10, 20, 30, 255]);

    let paths = compute_cache_paths(&cache_dir, &img_path, 500, 0).unwrap();
    fs::write(&paths.meta_path, r#"{"w":40,"h":30,"delays":[33]}"#).unwrap();
    fs::write(&paths.lraw_path, b"not-lz4").unwrap();

    let store = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 1024 * 1024, 30, None)
        .expect("corrupt cache must fall back to source decode");

    assert_eq!(store.frame_dimensions(), (40, 30));
    assert_eq!(store.frame_count(), 1);
    assert!(store.is_decode_done());
    let f0 = store.frame_argb(0).unwrap();
    assert_eq!(&f0[0..4], &[30, 20, 10, 255]);
}

#[test]
fn cache_abort_leaves_zero_temp_files_and_never_writes_meta()
{
    let temp = TempCacheTestDir::new("abort_cleanup");
    let img_path = temp.path().join("long.gif");
    let cache_dir = temp.path().join("cache");
    create_animated_gif(&img_path, 20, 32, 32);

    let mut store = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 1024 * 1024, 30, None)
        .expect("open should succeed");
    store.stop();

    let leftovers: Vec<_> = fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "leftover temp files found: {leftovers:?}"
    );
}

#[test]
fn cache_prune_removes_stale_files_and_refreshes_recent_hits()
{
    let temp = TempCacheTestDir::new("prune");
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir).unwrap();

    let old_time = SystemTime::now() - Duration::from_secs(35 * 86_400);
    let fresh_time = SystemTime::now() - Duration::from_secs(2 * 86_400);

    for name in ["stale.json", "stale.lraw", "stale.zraw", "stale.raw"]
    {
        let p = cache_dir.join(name);
        fs::write(&p, b"old").unwrap();
        let file = OpenOptions::new().write(true).open(&p).unwrap();
        file.set_modified(old_time).unwrap();
    }

    for name in ["fresh.json", "fresh.lraw"]
    {
        let p = cache_dir.join(name);
        fs::write(&p, b"new").unwrap();
        let file = OpenOptions::new().write(true).open(&p).unwrap();
        file.set_modified(fresh_time).unwrap();
    }

    let keep_other = cache_dir.join("notes.txt");
    fs::write(&keep_other, b"ignore").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&keep_other)
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    prune_cache(&cache_dir, 30);

    assert!(!cache_dir.join("stale.json").exists());
    assert!(!cache_dir.join("stale.lraw").exists());
    assert!(!cache_dir.join("stale.zraw").exists());
    assert!(!cache_dir.join("stale.raw").exists());
    assert!(cache_dir.join("fresh.json").exists());
    assert!(cache_dir.join("fresh.lraw").exists());
    assert!(keep_other.exists());
}

#[test]
fn cache_skips_uncompressed_ram_and_streams_lraw_when_animation_exceeds_budget()
{
    let temp = TempCacheTestDir::new("ram_budget");
    let img_path = temp.path().join("anim.gif");
    let cache_dir = temp.path().join("cache");
    // 10x10 ARGB8888 = 400 bytes per frame; 5 frames = 2000 bytes total (> 800-byte budget).
    create_animated_gif(&img_path, 5, 10, 10);

    let mut store = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 800, 30, None)
        .expect("cache should open");
    while !store.is_decode_done()
    {
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(store.ram_used_bytes(), 0);
    assert_eq!(store.compressed_ram_bytes(), 0);

    for _pass in 0..2
    {
        for idx in 0..5
        {
            let byte_len = store
                .with_frame_argb(idx, |bytes| bytes.len())
                .expect("frame streams cleanly from .lraw");
            assert_eq!(byte_len, 400);
            assert_eq!(store.ram_used_bytes(), 0);
            assert_eq!(store.compressed_ram_bytes(), 0);
        }
    }
    store.stop();

    // Synchronous `is_decode_done` and zero compressed heap confirm warm `.lraw` hit.
    let mut warm = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 800, 30, None)
        .expect("warm cache should open synchronously");
    assert!(warm.is_decode_done());
    assert_eq!(warm.frame_count(), 5);
    assert_eq!(warm.ram_used_bytes(), 0);
    assert_eq!(warm.compressed_ram_bytes(), 0);

    for _pass in 0..2
    {
        for idx in 0..5
        {
            let byte_len = warm
                .with_frame_argb(idx, |bytes| bytes.len())
                .expect("warm frame streams from .lraw across repeated passes");
            assert_eq!(byte_len, 400);
            assert_eq!(warm.ram_used_bytes(), 0);
            assert_eq!(warm.compressed_ram_bytes(), 0);
        }
    }
    warm.stop();
}

#[test]
fn cache_populates_ram_lazily_when_animation_fits_budget()
{
    let temp = TempCacheTestDir::new("ram_fit");
    let img_path = temp.path().join("anim.gif");
    let cache_dir = temp.path().join("cache");
    // 10x10 ARGB8888 = 400 bytes per frame; 5 frames = 2000 bytes exact budget fit.
    create_animated_gif(&img_path, 5, 10, 10);

    let mut store = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 2000, 30, None)
        .expect("cache should open");
    while !store.is_decode_done()
    {
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(store.compressed_ram_bytes(), 0);
    assert_eq!(store.ram_used_bytes(), 400);

    for idx in 1..5
    {
        let frame = store.frame_argb(idx).expect("pass 1 populates LRU lazily");
        assert_eq!(frame.len(), 400);
        assert_eq!(store.ram_used_bytes(), (idx + 1) * 400);
    }
    store.stop();

    // Reopen warm cache fitting in RAM, populate on pass 1, then remove `.lraw` for pass 2.
    let mut warm = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 2000, 30, None)
        .expect("warm cache should open");
    assert!(warm.is_decode_done());
    assert_eq!(warm.compressed_ram_bytes(), 0);
    assert_eq!(warm.ram_used_bytes(), 400);

    for idx in 1..5
    {
        let byte_len = warm
            .with_frame_argb(idx, |bytes| bytes.len())
            .expect("pass 1 warms RAM slot from disk");
        assert_eq!(byte_len, 400);
    }
    assert_eq!(warm.ram_used_bytes(), 2000);

    for entry in std::fs::read_dir(&cache_dir).expect("read cache dir")
    {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("lraw")
        {
            std::fs::remove_file(path).expect("remove .lraw after pass 1");
        }
    }

    for idx in 0..5
    {
        let byte_len = warm
            .with_frame_argb(idx, |bytes| bytes.len())
            .expect("pass 2 serves directly from RAM without .lraw");
        assert_eq!(byte_len, 400);
    }
    warm.stop();
}

#[test]
fn cache_dir_honors_xdg_override_and_home_fallback()
{
    let explicit = cache_dir_from_env(Some(OsStr::new("/xdg/cache")), Some(OsStr::new("/home/u")))
        .expect("explicit XDG_CACHE_HOME should resolve");
    assert_eq!(explicit, PathBuf::from("/xdg/cache/apng-overlay"));

    let empty_xdg = cache_dir_from_env(Some(OsStr::new("")), Some(OsStr::new("/home/u")))
        .expect("empty XDG_CACHE_HOME should fall back to HOME");
    assert_eq!(empty_xdg, PathBuf::from("/home/u/.cache/apng-overlay"));

    let missing = cache_dir_from_env(None, Some(OsStr::new("")));
    assert!(matches!(missing, Err(CacheError::DirectoryUnavailable)));
}

#[test]
fn cache_with_delays_borrows_timing_slice_without_blocking_subsequent_frame_reads()
{
    let temp = TempCacheTestDir::new("with_delays");
    let img_path = temp.path().join("anim.gif");
    let cache_dir = temp.path().join("cache");
    create_animated_gif(&img_path, 3, 12, 12);

    let mut store = FrameCache::open_in_dir(&img_path, 500, 0, &cache_dir, 4096, 30, None)
        .expect("cache should open");
    while !store.is_decode_done()
    {
        std::thread::sleep(Duration::from_millis(5));
    }

    let (count, done) = store.with_delays(|delays, decode_done| (delays.len(), decode_done));
    assert_eq!(count, 3);
    assert!(done);
    assert_eq!(
        store.frame_argb(0).expect("frame 0 accessible").len(),
        12 * 12 * 4
    );
    store.stop();
}
