//! Self-contained Wayland `wlr-layer-shell` overlay frontend, input handling, and event loop.
//!
//! Exposes input state machines, monitor resolution, surface compositing helpers, and the
//! `calloop` application runner.

// `calloop` event loop initialization integrating Wayland, signals, timers, and wakeups.
pub mod app;

// Corner handle (`28x28` rounded pill) and edit-mode dashed outline subsurfaces.
pub mod chrome;

// Pure handle/border geometry math and `ARGB8888` pixel rasterization helpers.
pub mod chrome_raster;

// Bounded in-memory display-resolution `ARGB8888` frame cache.
pub mod display_cache;

// SCTK and raw Wayland protocol dispatch implementations for `OverlayRuntime`.
pub mod handlers;

// Pointer drag, hover fade, scroll, and interaction state machine.
pub mod input;

// Monitor enumeration metadata and `--monitor` resolution.
pub mod monitor;

// Runtime timer, cursor, and clean-shutdown coordination.
pub mod runtime;

// Parent transparent layer surface, child image subsurface, and input-region updates.
pub mod scene;

// Reusable double/triple-buffered Wayland `SlotPool` frame ring.
pub mod shm_ring;

// Triple-click detection, edit-mode keyboard shortcuts, and scroll normalization.
pub mod shortcuts;

// Top-level Wayland overlay execution entry point and early signal masker.
pub use app::{prepare_signals, run_overlay};
// Pure pointer interaction state machine exports for application and integration tests.
pub use input::{HANDLE_FADE_MS, InputEffects, InputState, PointerCursor, SAVE_DEBOUNCE_MS};
// Monitor resolution exports for runtime selection and isolated tests.
pub use monitor::{MonitorDescriptor, resolve_monitor_index};
// Parent layer/image surface orientation mapper exported for integration tests.
pub use scene::wl_transform_for_orientation;
// Triple-click and edit-mode keyboard shortcut exports for integration tests.
pub use shortcuts::{
    ClickTracker, KeyShortcut, TRIPLE_CLICK_DISTANCE_PX, TRIPLE_CLICK_WINDOW_MS, map_edit_keysym,
};
