//! Command-line argument parsing and validation for the Wayland image overlay.
//!
//! Preserves exact CLI parity with the Python launcher (`FILE`, `-n`/`--max-frames`,
//! `-w`/`--base-width`, `-m`/`--monitor`, `-s`/`--scale`, `-r`/`--ram-cache-mb`, `-e`/`--edit`,
//! and `-d`/`--debug`) while rejecting invalid numeric inputs and missing files.

// Standard path and OS argument string types.
use std::{ffi::OsString, path::PathBuf};

// Clap derive macros and error classification for structured CLI parsing.
use clap::{Parser, error::ErrorKind};

// Default uncompressed RAM cache budget constant and validation error type.
use crate::{cache::DEFAULT_RAM_CACHE_BUDGET_BYTES, error::OverlayError};

// Default stored cache/display width in logical pixels when `--base-width` is omitted.
const DEFAULT_BASE_WIDTH: u32 = 500;

// Default uncompressed RAM cache budget in MiB derived from the cache store ceiling.
const DEFAULT_RAM_CACHE_MB: usize = DEFAULT_RAM_CACHE_BUDGET_BYTES / (1024 * 1024);

// Default target monitor selector when `--monitor` is omitted.
const DEFAULT_MONITOR: &str = "0";

// Concise mouse and keyboard interaction summary printed in `--help`.
const CONTROLS_HELP: &str = "\
Controls:
  Left-drag / Right-drag / Scroll   Move, resize, or zoom (corner handle, or anywhere in edit mode)
  Triple-click                      Toggle interactive edit mode
  Middle-click                      Quit overlay (corner handle only while in edit mode)

Edit mode keys:
  Esc                               Exit edit mode back to click-through playback
  Arrows / hjkl                     Nudge overlay position
  + / -                             Zoom scale up or down
  r / R                             Rotate 90 degrees clockwise or counter-clockwise
  Space                             Pause or resume animation
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

    /// Memory limit in MiB for keeping animations unpacked in RAM (`0` = read from disk, default: `128`).
    #[arg(
        short = 'r',
        long = "ram-cache-mb",
        default_value_t = DEFAULT_RAM_CACHE_MB,
        help = "RAM limit in MiB for keeping animations unpacked in memory (0 = read from disk)."
    )]
    pub ram_cache_mb: usize,

    /// Start directly in interactive edit mode with borders and keyboard focus enabled.
    #[arg(
        short = 'e',
        long = "edit",
        help = "Start in interactive edit mode (border and keyboard shortcuts enabled)."
    )]
    pub edit: bool,

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
        let parsed = match <Self as Parser>::try_parse_from(args)
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

    /// Converts `ram_cache_mb` (MiB) into bytes, rejecting overflowed counts.
    pub fn ram_cache_bytes(&self) -> Result<usize, OverlayError>
    {
        self.ram_cache_mb.checked_mul(1024 * 1024).ok_or_else(|| {
            OverlayError::CliValidation(String::from(
                "--ram-cache-mb overflows addressable memory byte count",
            ))
        })
    }

    /// Verifies numeric constraints (`base_width >= 1`, finite positive `scale`, valid RAM cap, monitor).
    pub fn validate_numbers(&self) -> Result<(), OverlayError>
    {
        if self.base_width < 1
        {
            return Err(OverlayError::CliValidation(String::from(
                "--base-width must be >= 1",
            )));
        }

        if let Some(scale) = self.scale
            && (!scale.is_finite() || scale <= 0.0)
        {
            return Err(OverlayError::CliValidation(String::from(
                "--scale must be a positive finite number",
            )));
        }

        if self.monitor.trim().is_empty()
        {
            return Err(OverlayError::CliValidation(String::from(
                "--monitor must not be empty",
            )));
        }

        let _ = self.ram_cache_bytes()?;

        Ok(())
    }

    /// Verifies that `self.file` points to an existing regular file on disk.
    pub fn validate_file_exists(&self) -> Result<(), OverlayError>
    {
        if !self.file.is_file()
        {
            return Err(OverlayError::MissingInputFile(self.file.clone()));
        }

        Ok(())
    }
}
