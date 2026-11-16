//! Persistent per-image overlay position, scale, and rotation storage.
//!
//! Reads and updates `$XDG_CONFIG_HOME/apng-overlay.json` (or `~/.config/apng-overlay.json`)
//! while preserving entries written by other image instances via an exclusive `flock` on
//! `<config>.lock` and atomic sibling temporary file replacement.

// Standard library paths, OS environment strings, file handles, and lexical normalization.
use std::{
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
};

// Serialization traits and dynamic map representation preserving foreign JSON keys.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

// Shared crash-safe atomic writer, angle normalizer, XDG lookup helper, and domain errors.
use crate::{
    atomic_fs,
    error::{ConfigError, OverlayError},
    layout::normalized_angle_deg,
    xdg,
};

// Shared JSON configuration filename placed under the user XDG config directory.
const CONFIG_FILE_NAME: &str = "apng-overlay.json";

/// Default horizontal coordinate when an image has no valid saved entry.
pub const DEFAULT_POS_X: f64 = 100.0;

/// Default vertical coordinate when an image has no valid saved entry.
pub const DEFAULT_POS_Y: f64 = 100.0;

/// Default scale multiplier when an image has no valid saved entry.
pub const DEFAULT_SCALE: f64 = 0.6;

/// Default audio gain multiplier applied when an entry has no persisted volume.
pub const DEFAULT_AUDIO_VOLUME: f64 = 1.0;

/// Persisted overlay placement, scale, and orientation relative to stored base dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverlayState
{
    /// Left offset in logical compositor pixels from the target monitor origin.
    pub x: f64,

    /// Top offset in logical compositor pixels from the target monitor origin.
    pub y: f64,

    /// Display multiplier applied to the stored base frame dimensions.
    pub scale: f64,

    /// Clockwise orientation persisted as normalized degrees in `[0.0, 360.0)`.
    #[serde(
        default,
        serialize_with = "serialize_rotation",
        deserialize_with = "deserialize_rotation"
    )]
    pub rotation: f64,

    /// Whether the rotated visual overlay is mirrored horizontally across its center axis.
    #[serde(default)]
    pub flipped: bool,
}

/// Persisted per-file audio playback volume (`0.0..=1.0`) and mute toggle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AudioPrefs
{
    /// Output gain multiplier clamped cleanly inside `0.0..=1.0`.
    pub volume: f64,

    /// Whether audio output starts muted for this media key.
    pub muted: bool,
}

impl Default for AudioPrefs
{
    /// Returns unmuted playback at full nominal gain (`1.0`).
    fn default() -> Self
    {
        Self {
            volume: DEFAULT_AUDIO_VOLUME,
            muted: false,
        }
    }
}

impl AudioPrefs
{
    /// Clamps finite `volume` into `0.0..=1.0`, falling back to `DEFAULT_AUDIO_VOLUME` otherwise.
    pub fn clamped_volume(volume: f64) -> f64
    {
        if volume.is_finite()
        {
            volume.clamp(0.0, 1.0)
        }
        else
        {
            DEFAULT_AUDIO_VOLUME
        }
    }
}

impl Default for OverlayState
{
    /// Returns the baseline startup placement, scale, and orientation matching defaults.
    fn default() -> Self
    {
        Self {
            x: DEFAULT_POS_X,
            y: DEFAULT_POS_Y,
            scale: DEFAULT_SCALE,
            rotation: 0.0,
            flipped: false,
        }
    }
}

/// RAII guard holding an exclusive advisory file lock via `std::fs::File::lock`.
struct ExclusiveLockGuard
{
    // Locked descriptor released explicitly on drop.
    file: File,
}

impl Drop for ExclusiveLockGuard
{
    /// Unlocks the advisory lock before closing the file descriptor.
    fn drop(&mut self)
    {
        let _ = self.file.unlock();
    }
}

/// Manages locked reads and atomic saves of per-file overlay geometry in JSON format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStore
{
    // Path to the shared JSON configuration file (e.g. `~/.config/apng-overlay.json`).
    path: PathBuf,
}

impl ConfigStore
{
    /// Creates a store targeting an explicit JSON configuration path.
    pub fn new(path: impl Into<PathBuf>) -> Self
    {
        Self { path: path.into() }
    }

    /// Resolves and constructs the default XDG configuration file store.
    pub fn open_default() -> Result<Self, OverlayError>
    {
        Ok(Self::new(default_config_path()?))
    }

    /// Returns the path of the JSON configuration file.
    pub fn path(&self) -> &Path
    {
        &self.path
    }

    /// Returns the sibling lock file path (`<config>.lock`).
    pub fn lock_path(&self) -> PathBuf
    {
        let mut os_path = self.path.as_os_str().to_os_string();
        os_path.push(".lock");
        PathBuf::from(os_path)
    }

    /// Computes a normalized absolute path key compatible with Python `os.path.abspath`.
    pub fn state_key(image_path: &Path) -> Result<String, OverlayError>
    {
        let abs = std::path::absolute(image_path).map_err(|source| ConfigError::Io {
            path: image_path.to_path_buf(),
            source,
        })?;

        Ok(normalize_lexical(&abs).to_string_lossy().into_owned())
    }

    /// Loads saved state for `key`, falling back per-field to defaults when missing or corrupt.
    pub fn load_state_entry(&self, key: &str) -> OverlayState
    {
        let mut state = OverlayState::default();
        let data = self.read_config_map();

        let Some(Value::Object(entry)) = data.get(key)
        else
        {
            return state;
        };

        if let Some(x) = entry.get("x").and_then(extract_finite_number)
        {
            state.x = x;
        }

        if let Some(y) = entry.get("y").and_then(extract_finite_number)
        {
            state.y = y;
        }

        if let Some(scale) = entry.get("scale").and_then(extract_finite_number)
        {
            state.scale = scale;
        }

        if let Some(rotation) = entry
            .get("rotation")
            .and_then(extract_finite_number)
            .map(normalized_angle_deg)
        {
            state.rotation = rotation;
        }

        if let Some(flipped) = entry.get("flipped").and_then(Value::as_bool)
        {
            state.flipped = flipped;
        }

        state
    }

    /// Loads saved audio volume and mute preferences for `key`, falling back to defaults.
    pub fn load_audio_prefs(&self, key: &str) -> AudioPrefs
    {
        let mut prefs = AudioPrefs::default();
        let data = self.read_config_map();

        let Some(Value::Object(entry)) = data.get(key)
        else
        {
            return prefs;
        };

        if let Some(volume) = entry
            .get("volume")
            .and_then(extract_finite_number)
            .filter(|v| (0.0..=1.0).contains(v))
        {
            prefs.volume = volume;
        }

        if let Some(muted) = entry.get("muted").and_then(Value::as_bool)
        {
            prefs.muted = muted;
        }

        prefs
    }

    /// Saves `state` under `key` while holding an exclusive `flock` and preserving audio prefs.
    pub fn save_state_entry(&self, key: &str, state: OverlayState) -> Result<(), OverlayError>
    {
        self.save_overlay_and_audio(key, state, None)
    }

    /// Saves geometry `state` and optional `audio` prefs atomically under `<config>.lock`.
    pub fn save_overlay_and_audio(
        &self,
        key: &str,
        state: OverlayState,
        audio: Option<AudioPrefs>,
    ) -> Result<(), OverlayError>
    {
        let parent = self
            .path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .ok_or_else(|| ConfigError::MissingParent {
                path: self.path.clone(),
            })?;

        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        let lock_path = self.lock_path();
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| ConfigError::Lock {
                path: lock_path.clone(),
                source,
            })?;

        lock_file.lock().map_err(|source| ConfigError::Lock {
            path: lock_path.clone(),
            source,
        })?;

        let _lock_guard = ExclusiveLockGuard { file: lock_file };

        let mut data = self.read_config_map();
        let mut entry = match data.remove(key)
        {
            Some(Value::Object(existing)) => existing,
            _ => Map::new(),
        };

        let Value::Object(geom_fields) =
            serde_json::to_value(state).map_err(|source| ConfigError::Serialize {
                path: self.path.clone(),
                source,
            })?
        else
        {
            return Err(ConfigError::Serialize {
                path: self.path.clone(),
                source: serde::ser::Error::custom("expected JSON object for overlay state"),
            }
            .into());
        };
        for (field, value) in geom_fields
        {
            entry.insert(field, value);
        }

        if let Some(prefs) = audio
        {
            entry.insert(
                String::from("volume"),
                Value::from(AudioPrefs::clamped_volume(prefs.volume)),
            );
            entry.insert(String::from("muted"), Value::Bool(prefs.muted));
        }

        data.insert(key.to_owned(), Value::Object(entry));

        let bytes = serde_json::to_vec(&data).map_err(|source| ConfigError::Serialize {
            path: self.path.clone(),
            source,
        })?;

        atomic_fs::write_bytes(&self.path, ".json", &bytes).map_err(|source| ConfigError::Io {
            path: self.path.clone(),
            source,
        })?;

        tracing::debug!(
            key,
            x = state.x,
            y = state.y,
            scale = state.scale,
            rotation = state.rotation,
            flipped = state.flipped,
            audio = ?audio,
            "Saved overlay state"
        );
        Ok(())
    }

    /// Reads the root JSON object map, returning an empty map when absent or corrupt.
    fn read_config_map(&self) -> Map<String, Value>
    {
        let Ok(raw) = fs::read(&self.path)
        else
        {
            return Map::new();
        };

        match serde_json::from_slice::<Value>(&raw)
        {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }
}

/// Resolves the shared config path from explicit `XDG_CONFIG_HOME` and `HOME` values.
pub fn config_path_from_env(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, ConfigError>
{
    xdg::base_dir(xdg_config_home, home, ".config")
        .map(|dir| dir.join(CONFIG_FILE_NAME))
        .ok_or(ConfigError::DirectoryUnavailable)
}

/// Resolves the shared config path (`$XDG_CONFIG_HOME/apng-overlay.json` or `~/.config/...`).
pub fn default_config_path() -> Result<PathBuf, ConfigError>
{
    let xdg = env::var_os("XDG_CONFIG_HOME");
    let home = env::var_os("HOME");
    config_path_from_env(xdg.as_deref(), home.as_deref())
}

/// Validates that a JSON value represents a finite non-boolean numeric value.
fn extract_finite_number(value: &Value) -> Option<f64>
{
    match value
    {
        Value::Number(num) => num.as_f64().filter(|val| val.is_finite()),
        _ => None,
    }
}

/// Serializes normalized clockwise degrees into `[0.0, 360.0)`.
fn serialize_rotation<S>(rotation: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_f64(normalized_angle_deg(*rotation))
}

/// Deserializes finite numeric degrees into canonical `[0.0, 360.0)`.
fn deserialize_rotation<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let degrees = f64::deserialize(deserializer)?;
    if !degrees.is_finite()
    {
        return Err(serde::de::Error::custom(
            "rotation must be a finite number of degrees",
        ));
    }
    Ok(normalized_angle_deg(degrees))
}

/// Collapses `.` and `..` components without dereferencing filesystem symlinks.
fn normalize_lexical(path: &Path) -> PathBuf
{
    let mut normalized = PathBuf::new();
    for component in path.components()
    {
        match component
        {
            Component::CurDir =>
            {}
            Component::ParentDir =>
            {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
