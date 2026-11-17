//! `calloop`-driven Wayland overlay initialization, coordinator supervision, and runner facades.

// Coordinator supervisor managing concurrent overlay threads and signal broadcast.
pub(crate) mod supervisor;

// Single-overlay Wayland protocol setup, resource initialization, and event dispatch.
pub(crate) mod runner;

// Re-exported top-level runner and signal preparation functions.
pub use runner::prepare_signals;
pub use supervisor::run_overlay;
