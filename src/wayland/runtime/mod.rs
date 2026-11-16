//! Runtime state coordinator managing timers, playback ticks, input effects, and cleanup.

// Pointer event translation and scene/timer input effect dispatching.
mod effects;

// Animation deadline ticks, SHM staging, and `--stats` tick recording.
mod playback;

// Core runtime state struct, `calloop` timers, stats snapshot capture, and shutdown cleanup.
mod state;

// Central Wayland runtime coordinator exposed across the `wayland` module.
pub(crate) use state::OverlayRuntime;
