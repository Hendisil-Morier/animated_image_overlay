//! Command-line argument parsing and validation for the Wayland image overlay.
//!
//! Preserves CLI flags (`FILE`, `-n`/`--max-frames`, `-w`/`--base-width`,
//! `-m`/`--monitor`, `-s`/`--scale`, `-c`/`--cap`, `-f`/`--max-fps`,
//! `--audio`, `--mute`, `--volume`, `--gpu`, `-e`/`--edit`, `--stats`, and `-d`/`--debug`)
//! while rejecting invalid numeric inputs and missing files.

// Standard path and OS argument string types.
use std::{ffi::OsString, path::PathBuf};

// Clap derive macros and error classification for structured CLI parsing.
use clap::{ArgAction, Parser, error::ErrorKind};

// Default uncompressed RAM cache budget, default/max FPS bounds, and validation error type.
use crate::{
    cache::DEFAULT_RAM_CACHE_BUDGET_BYTES,
    error::OverlayError,
    playback::{DEFAULT_MAX_FPS, MAX_CONFIGURABLE_FPS},
};

// Default stored cache/display width in logical pixels when `--base-width` is omitted.
const DEFAULT_BASE_WIDTH: u32 = 500;

// Default uncompressed RAM cache budget in MiB derived from the cache store ceiling.
const DEFAULT_RAM_CACHE_MB: usize = DEFAULT_RAM_CACHE_BUDGET_BYTES / (1024 * 1024);

// Default RAM limit in MiB for caching display-resolution frames (`64 MiB`).
const DEFAULT_DISPLAY_CACHE_MB: usize = 64;

// Default uncompressed MiB threshold (`512 MiB`) below which short videos cache into `.lraw`.
const DEFAULT_VIDEO_CACHE_MB: usize = 512;

// Default target monitor selector when `--monitor` is omitted.
const DEFAULT_MONITOR: &str = "0";

/// Supported GPU hardware acceleration backends.
pub const SUPPORTED_GPU_MODES: &[&str] = &[
    "auto",
    "none",
    "off",
    "cuda",
    "nvdec",
    "vaapi",
    "vdpau",
    "qsv",
    "drm",
    "opencl",
    "vulkan",
    "amf",
    "videotoolbox",
    "d3d11va",
    "d3d12va",
    "dxva2",
];

// Concise mouse and keyboard interaction summary printed in `--help`.
const CONTROLS_HELP: &str = "\
Controls:
  Left-drag / Right-drag / Scroll   Move, resize, or zoom (corner handle, or anywhere in edit mode)
  Triple-click                      Toggle interactive edit mode
  Middle-click                      Quit overlay (on corner handle)

Edit mode keys:
  Esc                               Exit edit mode back to click-through playback
  Arrows / hjkl                     Nudge overlay position (12 px)
  + / - (= / _ / numpad + / -)      Zoom scale up or down
  r / R                             Rotate 90 degrees clockwise or counter-clockwise
  [ / ]                             Rotate 5 degrees counter-clockwise or clockwise
  0                                 Reset rotation to 0 degrees
  f / F                             Flip horizontally or vertically
  m                                 Toggle audio mute
  , / . (< / >)                     Decrease or increase audio volume
  Space                             Pause or resume playback
  q                                 Quit overlay";

/// Outcome of parsing CLI flags before starting the Wayland runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum CliOutcome
{
    /// Validated overlay launch configuration ready for execution.
    Run(CliArgs),

    /// Help or version text produced cleanly by `clap` (exit status 0).
    EarlyExit(String),
}

/// Validated command-line parameters for launching an overlay instance.
#[derive(Debug, Clone, PartialEq, Parser)]
#[command(
    name = "animated_image_widget",
    version,
    about = "Display an animated image as a click-through Wayland overlay.",
    after_help = CONTROLS_HELP
)]
pub struct CliArgs
{
    /// Path to the image or animation file.
    #[arg(value_name = "FILE", help = "Path to the image or animation file.")]
    pub file: PathBuf,

    /// Maximum number of frames to decode (`0` = all frames).
    #[arg(
        short = 'n',
        long = "max-frames",
        default_value_t = 0,
        help = "Maximum number of frames to decode (0 = all frames)."
    )]
    pub max_frames: usize,

    /// Stored frame width in pixels (default: 500, minimum: 1).
    #[arg(
        short = 'w',
        long = "base-width",
        default_value_t = DEFAULT_BASE_WIDTH,
        help = "Stored frame width in pixels (default: 500)."
    )]
    pub base_width: u32,

    /// Target monitor zero-based index or connector name (default: `"0"`).
    #[arg(
        short = 'm',
        long = "monitor",
        default_value = DEFAULT_MONITOR,
        help = "Target monitor index or connector name (default: 0)."
    )]
    pub monitor: String,

    /// Optional initial display scale overriding persisted state.
    #[arg(
        short = 's',
        long = "scale",
        allow_hyphen_values = true,
        help = "Initial display scale (overrides saved scale)."
    )]
    pub scale: Option<f64>,

    /// Comma-separated cache limits in MiB (`ram=<MB>,display=<MB>,video=<MB>`), overriding defaults cleanly.
    #[arg(
        short = 'c',
        long = "cap",
        value_name = "SPEC",
        action = ArgAction::Append,
        help = "Cache MiB caps as key=value (repeatable; e.g. -c ram=128,display=64,video=512; 0 disables)."
    )]
    pub cap: Vec<String>,

    /// Resolved RAM limit in MiB for keeping source frames in memory (`0` disables, default: `128`).
    #[arg(skip = DEFAULT_RAM_CACHE_MB)]
    pub ram_cache_mb: usize,

    /// Resolved RAM limit in MiB for display-resolution frame caching (`0` disables, default: `64`).
    #[arg(skip = DEFAULT_DISPLAY_CACHE_MB)]
    pub display_cache_mb: usize,

    /// Uncompressed MiB threshold below which short videos cache into `.lraw` (`0` forces live streaming, default: `512`).
    #[arg(skip = DEFAULT_VIDEO_CACHE_MB)]
    pub video_cache_mb: usize,

    /// Hardware-accelerated GPU video decoding mode (`auto`, `none`/`off`, `vaapi`, `cuda`, `vulkan`, default: `auto`).
    #[arg(
        long = "gpu",
        default_value = "auto",
        value_name = "MODE",
        help = "GPU video decoding backend (auto, none, vaapi, cuda, vulkan; default: auto)."
    )]
    pub gpu: String,

    /// Optional external audio file paired with images or overriding embedded video audio.
    #[arg(
        long = "audio",
        value_name = "AUDIO_FILE",
        help = "Optional audio file paired with an image or overriding video audio."
    )]
    pub audio: Option<PathBuf>,

    /// Start playback with audio muted.
    #[arg(long = "mute", help = "Start with audio output muted.")]
    pub mute: bool,

    /// Optional initial audio gain in `0.0..=1.0`, overriding saved volume.
    #[arg(
        long = "volume",
        value_name = "LEVEL",
        allow_hyphen_values = true,
        help = "Initial audio volume level in 0.0..=1.0 (overrides saved volume)."
    )]
    pub volume: Option<f64>,

    /// Maximum playback frame rate ceiling in FPS (`1..=1000`, default: `240`).
    #[arg(
        short = 'f',
        long = "max-fps",
        value_name = "FPS",
        default_value_t = DEFAULT_MAX_FPS,
        help = "Maximum playback frame rate in FPS (1..=1000, default: 240)."
    )]
    pub max_fps: u32,

    /// Start directly in interactive edit mode with borders and keyboard focus enabled.
    #[arg(
        short = 'e',
        long = "edit",
        help = "Start in interactive edit mode (border and keyboard shortcuts enabled)."
    )]
    pub edit: bool,

    /// Print periodic 1-second playback, CPU, RAM usage, staging, and cache stats to stdout.
    #[arg(
        long = "stats",
        help = "Print 1-second playback FPS, CPU%, RAM usage, skip/hold, staging, and cache stats to stdout."
    )]
    pub stats: bool,

    /// Mirror debug diagnostics to stdout alongside rolling daily log files.
    #[arg(
        short = 'd',
        long = "debug",
        help = "Print debug logs to stdout alongside daily log files."
    )]
    pub debug: bool,
}

impl CliArgs
{
    /// Parses OS process arguments and validates numeric flags plus input file existence.
    pub fn parse_env() -> Result<CliOutcome, OverlayError>
    {
        Self::parse_and_validate_from(std::env::args_os())
    }

    /// Parses an arbitrary argument iterator without requiring the target file to exist on disk.
    pub fn parse_flags_from<I, T>(args: I) -> Result<CliOutcome, OverlayError>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut parsed = match <Self as Parser>::try_parse_from(args)
        {
            Ok(cli) => cli,
            Err(err) =>
            {
                if matches!(
                    err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                )
                {
                    return Ok(CliOutcome::EarlyExit(err.to_string()));
                }
                return Err(OverlayError::CliValidation(err.to_string()));
            }
        };

        parsed.apply_cap_overrides()?;
        parsed.validate_numbers()?;
        Ok(CliOutcome::Run(parsed))
    }

    /// Parses arguments and enforces both numeric bounds and regular-file existence.
    pub fn parse_and_validate_from<I, T>(args: I) -> Result<CliOutcome, OverlayError>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        match Self::parse_flags_from(args)?
        {
            CliOutcome::Run(cli) =>
            {
                cli.validate_file_exists()?;
                Ok(CliOutcome::Run(cli))
            }
            early @ CliOutcome::EarlyExit(_) => Ok(early),
        }
    }

    /// Resolves `-c` / `--cap` (`ram=<MB>,display=<MB>,video=<MB>`) entries onto tier limits.
    fn apply_cap_overrides(&mut self) -> Result<(), OverlayError>
    {
        let mut ram_mb = DEFAULT_RAM_CACHE_MB;
        let mut display_mb = DEFAULT_DISPLAY_CACHE_MB;
        let mut video_mb = DEFAULT_VIDEO_CACHE_MB;

        for spec in &self.cap
        {
            if spec.trim().is_empty()
            {
                return Err(OverlayError::CliValidation(String::from(
                    "--cap value must not be empty (expected ram=<MB>, display=<MB>, or video=<MB>)",
                )));
            }
            for item in spec.split(',')
            {
                match parse_cap_entry(item)?
                {
                    (CapTarget::Ram, mb) => ram_mb = mb,
                    (CapTarget::Display, mb) => display_mb = mb,
                    (CapTarget::Video, mb) => video_mb = mb,
                }
            }
        }

        self.ram_cache_mb = ram_mb;
        self.display_cache_mb = display_mb;
        self.video_cache_mb = video_mb;
        Ok(())
    }

    /// Converts a MiB limit into bytes cleanly, rejecting overflowed counts.
    fn mb_to_bytes(mb: usize, label: &str) -> Result<usize, OverlayError>
    {
        mb.checked_mul(1024 * 1024).ok_or_else(|| {
            OverlayError::CliValidation(format!("{label} overflows addressable memory byte count"))
        })
    }

    /// Converts `ram_cache_mb` (MiB) into bytes, rejecting overflowed counts.
    pub fn ram_cache_bytes(&self) -> Result<usize, OverlayError>
    {
        Self::mb_to_bytes(self.ram_cache_mb, "--cap ram")
    }

    /// Converts `display_cache_mb` (MiB) into bytes, rejecting overflowed counts.
    pub fn display_cache_bytes(&self) -> Result<usize, OverlayError>
    {
        Self::mb_to_bytes(self.display_cache_mb, "--cap display")
    }

    /// Converts `video_cache_mb` (MiB) into bytes, rejecting overflowed counts.
    pub fn video_cache_bytes(&self) -> Result<usize, OverlayError>
    {
        Self::mb_to_bytes(self.video_cache_mb, "--cap video")
    }

    /// Verifies numeric constraints (`base_width >= 1`, `max_fps` in `1..=1000`, finite positive `scale`, `volume` in `0..=1`, valid cache caps, monitor).
    pub fn validate_numbers(&self) -> Result<(), OverlayError>
    {
        if self.base_width < 1
        {
            return Err(OverlayError::CliValidation(String::from(
                "--base-width must be >= 1",
            )));
        }

        if !(1..=MAX_CONFIGURABLE_FPS).contains(&self.max_fps)
        {
            return Err(OverlayError::CliValidation(format!(
                "--max-fps must be between 1 and {MAX_CONFIGURABLE_FPS}"
            )));
        }

        if let Some(scale) = self.scale
            && (!scale.is_finite() || scale <= 0.0)
        {
            return Err(OverlayError::CliValidation(String::from(
                "--scale must be a positive finite number",
            )));
        }

        if let Some(volume) = self.volume
            && (!volume.is_finite() || !(0.0..=1.0).contains(&volume))
        {
            return Err(OverlayError::CliValidation(String::from(
                "--volume must be a finite value between 0.0 and 1.0",
            )));
        }

        if self.monitor.trim().is_empty()
        {
            return Err(OverlayError::CliValidation(String::from(
                "--monitor must not be empty",
            )));
        }

        let gpu_mode = self.gpu.trim().to_ascii_lowercase();
        if gpu_mode.is_empty()
        {
            return Err(OverlayError::CliValidation(String::from(
                "--gpu must not be empty",
            )));
        }

        if !SUPPORTED_GPU_MODES.contains(&gpu_mode.as_str())
        {
            return Err(OverlayError::CliValidation(format!(
                "Unknown --gpu mode '{}'. Recognized options: {}",
                self.gpu.trim(),
                SUPPORTED_GPU_MODES.join(", ")
            )));
        }

        let _ = self.ram_cache_bytes()?;
        let _ = self.display_cache_bytes()?;
        let _ = self.video_cache_bytes()?;

        Ok(())
    }

    /// Verifies that `self.file` and optional `self.audio` point to existing regular files on disk.
    pub fn validate_file_exists(&self) -> Result<(), OverlayError>
    {
        if !self.file.is_file()
        {
            return Err(OverlayError::MissingInputFile(self.file.clone()));
        }

        if let Some(audio) = &self.audio
            && !audio.is_file()
        {
            return Err(OverlayError::MissingAudioFile(audio.clone()));
        }

        Ok(())
    }
}

/// Target cache tier parsed from one `-c` / `--cap` `key=value` suboption.
enum CapTarget
{
    // Uncompressed source RAM LRU budget (`-c ram=<MB>`).
    Ram,

    // Display-resolution frame cache budget (`-c display=<MB>`).
    Display,

    // Maximum uncompressed MiB cached into `.lraw` for short video loops (`-c video=<MB>`).
    Video,
}

/// Parses one `ram=<MB>`, `display=<MB>`, or `video=<MB>` entry cleanly with validation errors.
fn parse_cap_entry(entry: &str) -> Result<(CapTarget, usize), OverlayError>
{
    let item = entry.trim();
    let Some((key, val)) = item.split_once('=')
    else
    {
        return Err(OverlayError::CliValidation(format!(
            "Invalid --cap entry '{item}' (expected ram=<MB>, display=<MB>, or video=<MB>)"
        )));
    };

    let key = key.trim();
    let mb = val.trim().parse::<usize>().map_err(|_| {
        OverlayError::CliValidation(format!("Invalid --cap MiB count '{val}' for '{key}'"))
    })?;

    match key
    {
        "ram" => Ok((CapTarget::Ram, mb)),
        "display" => Ok((CapTarget::Display, mb)),
        "video" => Ok((CapTarget::Video, mb)),
        _ => Err(OverlayError::CliValidation(format!(
            "Unknown --cap key '{key}' (expected ram, display, or video)"
        ))),
    }
}
