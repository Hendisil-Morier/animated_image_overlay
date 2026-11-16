//! Integration tests for locked JSON config persistence, atomic replacement, and field recovery.

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    config::{
        ConfigStore, DEFAULT_POS_X, DEFAULT_POS_Y, DEFAULT_SCALE, OverlayState,
        config_path_from_env,
    },
    error::{ConfigError, OverlayError},
};
use serde_json::{Map, Value};

struct TempConfigDir
{
    path: PathBuf,
}

impl TempConfigDir
{
    fn new(name: &str) -> Self
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aiw_cfg_{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp config test dir should be created");
        Self { path }
    }

    fn path(&self) -> &Path
    {
        &self.path
    }
}

impl Drop for TempConfigDir
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn config_recovers_valid_fields_and_ignores_corrupt_entries()
{
    let temp = TempConfigDir::new("field_validation");
    let cfg_path = temp.path().join("nested").join("apng-overlay.json");
    let store = ConfigStore::new(&cfg_path);

    let seed_json = r#"{"/a.png":{"x":250,"y":"bad","scale":true}}"#;
    fs::create_dir_all(cfg_path.parent().unwrap()).expect("nested dir created");
    fs::write(&cfg_path, seed_json).expect("seed config written");

    let loaded = store.load_state_entry("/a.png");
    assert_eq!(
        loaded,
        OverlayState {
            x: 250.0,
            y: DEFAULT_POS_Y,
            scale: DEFAULT_SCALE,
            rotation: 0.0,
            flipped: false,
        }
    );

    fs::write(&cfg_path, "{corrupt json").expect("corrupt config written");
    assert_eq!(store.load_state_entry("/a.png"), OverlayState::default());
}

#[test]
fn config_save_state_entry_serializes_concurrent_writes_via_flock()
{
    let temp = TempConfigDir::new("concurrent_flock");
    let cfg_path = temp.path().join("apng-overlay.json");
    let store = Arc::new(ConfigStore::new(&cfg_path));

    let worker_count = 8;
    let mut handles = Vec::with_capacity(worker_count);

    for idx in 0..worker_count
    {
        let store_ref = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            let key = format!("/images/item_{idx}.apng");
            let state = OverlayState {
                x: f64::from(idx as u32) * 10.0,
                y: f64::from(idx as u32) * 20.0,
                scale: 0.5 + f64::from(idx as u32) * 0.1,
                rotation: 0.0,
                flipped: false,
            };
            store_ref
                .save_state_entry(&key, state)
                .expect("concurrent save should succeed");
        }));
    }

    for handle in handles
    {
        handle.join().expect("worker thread should not panic");
    }

    let raw = fs::read_to_string(&cfg_path).expect("saved config should exist");
    let map: Map<String, Value> = serde_json::from_str(&raw).expect("saved config valid JSON");
    assert_eq!(map.len(), worker_count);

    for idx in 0..worker_count
    {
        let key = format!("/images/item_{idx}.apng");
        let loaded = store.load_state_entry(&key);
        assert_eq!(loaded.x, f64::from(idx as u32) * 10.0);
        assert_eq!(loaded.y, f64::from(idx as u32) * 20.0);
    }

    let leftovers: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "found leftover temp files: {leftovers:?}"
    );
}

#[test]
fn config_save_preserves_foreign_keys_and_reports_unwritable_dir()
{
    let temp = TempConfigDir::new("foreign_keys");
    let cfg_path = temp.path().join("apng-overlay.json");
    fs::write(
        &cfg_path,
        r#"{"/other.gif":{"x":42,"y":84,"scale":1.2,"custom":true}}"#,
    )
    .expect("foreign seed written");

    let store = ConfigStore::new(&cfg_path);
    store
        .save_state_entry(
            "/new.apng",
            OverlayState {
                x: DEFAULT_POS_X + 5.0,
                y: DEFAULT_POS_Y,
                scale: 0.9,
                rotation: 0.0,
                flipped: false,
            },
        )
        .expect("save should succeed");

    let raw = fs::read_to_string(&cfg_path).expect("updated config readable");
    let map: Map<String, Value> = serde_json::from_str(&raw).unwrap();
    assert_eq!(map["/other.gif"]["custom"], Value::Bool(true));
    assert_eq!(store.load_state_entry("/new.apng").scale, 0.9);

    let blocking_parent = temp.path().join("regular_file_not_dir");
    fs::write(&blocking_parent, b"x").expect("blocking file created");
    let bad_store = ConfigStore::new(blocking_parent.join("apng-overlay.json"));
    let err = bad_store
        .save_state_entry("/bad.png", OverlayState::default())
        .expect_err("unwritable parent should return ConfigError");
    assert!(matches!(err, OverlayError::Config(_)));
}

#[test]
fn config_state_key_produces_normalized_absolute_path()
{
    let key = ConfigStore::state_key(Path::new("./dir/../sample.apng"))
        .expect("state_key should normalize relative path");
    assert!(Path::new(&key).is_absolute());
    assert!(key.ends_with("/sample.apng"));
    assert!(!key.contains(".."));
}

#[test]
fn config_path_honors_xdg_override_and_home_fallback()
{
    let explicit = config_path_from_env(Some(OsStr::new("/xdg/cfg")), Some(OsStr::new("/home/u")))
        .expect("explicit XDG_CONFIG_HOME should resolve");
    assert_eq!(explicit, PathBuf::from("/xdg/cfg/apng-overlay.json"));

    let empty_xdg = config_path_from_env(Some(OsStr::new("")), Some(OsStr::new("/home/u")))
        .expect("empty XDG_CONFIG_HOME should fall back to HOME");
    assert_eq!(
        empty_xdg,
        PathBuf::from("/home/u/.config/apng-overlay.json")
    );

    let unset_xdg = config_path_from_env(None, Some(OsStr::new("/home/u")))
        .expect("unset XDG_CONFIG_HOME should fall back to HOME");
    assert_eq!(
        unset_xdg,
        PathBuf::from("/home/u/.config/apng-overlay.json")
    );

    let missing_both = config_path_from_env(Some(OsStr::new("")), Some(OsStr::new("")));
    assert!(matches!(
        missing_both,
        Err(ConfigError::DirectoryUnavailable)
    ));
}

#[test]
fn config_persists_arbitrary_rotations_and_recovers_invalid_values()
{
    let temp = TempConfigDir::new("rotation_persistence");
    let cfg_path = temp.path().join("apng-overlay.json");
    let store = ConfigStore::new(&cfg_path);

    for (key, rotation, flipped) in [
        ("/deg0.png", 0.0, false),
        ("/deg90.png", 90.0, true),
        ("/deg180.png", 180.0, false),
        ("/deg270.png", 270.0, true),
        ("/fractional.png", 42.5, true),
        ("/wrapped.png", 345.0, false),
    ]
    {
        let state = OverlayState {
            x: 120.0,
            y: 240.0,
            scale: 0.75,
            rotation,
            flipped,
        };
        store
            .save_state_entry(key, state)
            .expect("rotation entry should save");
        assert_eq!(store.load_state_entry(key), state);

        let raw = fs::read_to_string(&cfg_path).expect("saved config readable");
        let parsed: Map<String, Value> = serde_json::from_str(&raw).expect("valid JSON object");
        assert_eq!(parsed[key]["rotation"], Value::from(rotation));
        assert_eq!(parsed[key]["flipped"], Value::Bool(state.flipped));
    }

    let seed = r#"{
        "/legacy.png": {"x": 300, "y": 400, "scale": 1.25},
        "/flipped_only.png": {"x": 305, "y": 405, "scale": 1.1, "flipped": true},
        "/bad_flip.png": {"x": 306, "y": 406, "scale": 1.1, "rotation": 90, "flipped": "yes"},
        "/fractional.png": {"x": 310, "y": 410, "scale": 1.5, "rotation": 45.25},
        "/negative.png": {"x": 320, "y": 420, "scale": 0.8, "rotation": -15.5},
        "/over_full.png": {"x": 325, "y": 425, "scale": 0.8, "rotation": 405},
        "/string_rot.png": {"x": 330, "y": 430, "scale": 0.9, "rotation": "90"},
        "/bool_rot.png": {"x": 340, "y": 440, "scale": 0.9, "rotation": true}
    }"#;
    fs::write(&cfg_path, seed).expect("seed config written");

    let flipped_only = store.load_state_entry("/flipped_only.png");
    assert_eq!(flipped_only.rotation, 0.0);
    assert!(flipped_only.flipped);

    let bad_flip = store.load_state_entry("/bad_flip.png");
    assert_eq!(bad_flip.rotation, 90.0);
    assert!(!bad_flip.flipped);

    let fractional = store.load_state_entry("/fractional.png");
    assert_eq!(fractional.rotation, 45.25);

    let negative = store.load_state_entry("/negative.png");
    assert_eq!(negative.rotation, 344.5);

    let over_full = store.load_state_entry("/over_full.png");
    assert_eq!(over_full.rotation, 45.0);

    for (key, x) in [
        ("/legacy.png", 300.0),
        ("/string_rot.png", 330.0),
        ("/bool_rot.png", 340.0),
    ]
    {
        let loaded = store.load_state_entry(key);
        assert_eq!(loaded.x, x);
        assert_eq!(loaded.rotation, 0.0);
        assert!(!loaded.flipped);
    }

    let valid_serde: OverlayState =
        serde_json::from_str(r#"{"x":10,"y":20,"scale":1.0,"rotation":-30.5}"#)
            .expect("arbitrary finite angle should deserialize");
    assert_eq!(valid_serde.rotation, 329.5);

    let invalid_serde: Result<OverlayState, _> =
        serde_json::from_str(r#"{"x":10,"y":20,"scale":1.0,"rotation":"bad"}"#);
    assert!(invalid_serde.is_err());
}

#[test]
fn config_persists_audio_prefs_alongside_overlay_state_without_clobbering()
{
    let temp = TempConfigDir::new("audio_prefs");
    let cfg_path = temp.path().join("apng-overlay.json");
    let store = ConfigStore::new(&cfg_path);

    let key = "/videos/sample.mp4";
    assert_eq!(
        store.load_audio_prefs(key),
        animated_image_widget::config::AudioPrefs::default()
    );

    let geom = OverlayState {
        x: 50.0,
        y: 80.0,
        scale: 1.25,
        rotation: 90.0,
        flipped: true,
    };
    let prefs = animated_image_widget::config::AudioPrefs {
        volume: 0.42,
        muted: true,
    };
    store
        .save_overlay_and_audio(key, geom, Some(prefs))
        .expect("combined overlay + audio save should succeed");

    assert_eq!(store.load_state_entry(key), geom);
    assert_eq!(store.load_audio_prefs(key), prefs);

    // Geometry-only save preserves persisted audio prefs cleanly.
    let shifted = OverlayState { x: 120.0, ..geom };
    store
        .save_state_entry(key, shifted)
        .expect("geometry-only save should succeed");
    assert_eq!(store.load_state_entry(key), shifted);
    assert_eq!(store.load_audio_prefs(key), prefs);

    fs::write(
        &cfg_path,
        r#"{"/bad_audio.mp4":{"x":10,"y":20,"scale":1.0,"volume":1.5,"muted":"no"}}"#,
    )
    .expect("corrupt audio prefs fixture written");
    assert_eq!(
        store.load_audio_prefs("/bad_audio.mp4"),
        animated_image_widget::config::AudioPrefs::default()
    );
}
