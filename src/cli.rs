//! Command-line argument parsing and validation for the Wayland image overlay.
//!
//! Preserves exact CLI parity with the Python launcher (`FILE`, `-n`/`--max-frames`,
//! `-w`/`--base-width`, `-m`/`--monitor`, and `-s`/`--scale`) while rejecting
//! non-positive widths, invalid scales, and missing image files before startup.

// Standard path and OS argument string types.
use std::{ffi::OsString, path::PathBuf};

// Clap derive macros and error classification for structured CLI parsing.
use clap::{Parser, error::ErrorKind};

// Top-level error enum returned on validation failure.
use crate::error::OverlayError;

// Default stored cache/display width in logical pixels when `--base-width` is omitted.
const DEFAULT_BASE_WIDTH: u32 = 500;

// Default target monitor selector when `--monitor` is omitted.
const DEFAULT_MONITOR: &str = "0";

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
    about = "Display an animated image as a click-through Wayland overlay."
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

    /// Verifies numeric constraints (`base_width >= 1`, finite positive `scale`, non-empty monitor).
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
