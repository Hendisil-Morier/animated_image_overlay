//! Core library facade for the Wayland animated image overlay.
//!
//! Exposes domain error types, diagnostics, CLI parsing, configuration persistence,
//! image decoding, frame caching, layout math, and Wayland rendering modules.

#![deny(clippy::unwrap_used, clippy::expect_used)]

// Crash-safe atomic sibling temporary file writes shared across config and cache.
pub(crate) mod atomic_fs;

// LZ4 disk + 128 MiB RAM frame cache with 30-day pruning and fallback recovery.
pub mod cache;

// Command-line flag parsing and input validation.
pub mod cli;

// Locked JSON persistence for per-image overlay position and scale.
pub mod config;

// Streaming APNG, GIF, animated WebP, and static frame decoder.
pub mod decode;

// Domain error hierarchy powered by `thiserror`.
pub mod error;

// Monitor bounds clamping, handle hit-testing, and drag/scroll geometry.
pub mod layout;

// Dual-output console and non-blocking disk logging diagnostics.
pub mod logging;

// Animation frame deadline scheduling and suspend/stall recovery.
pub mod playback;

// Self-contained `wlr-layer-shell` + `wl_shm` Wayland overlay runtime and input handling.
pub mod wayland;
