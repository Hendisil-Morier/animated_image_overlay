# Changelog

All notable changes to this project are documented here.

## [Unreleased] - 2026-11-15

### Added
- Split the single-file script into `overlay.py`, `config.py`, `cache.py`, and `window.py`, backed by unit tests in `tests/test_overlay.py`.
- Added `argparse` CLI flags for `--max-frames`, `--base-width`, `--monitor` (index or connector name), and `--scale` with input validation.
- Added `SIGUSR1` edit mode to toggle full-image drag/resize input with a dashed outline, handle auto-fade after 2 seconds of inactivity, and middle-click or `SIGINT`/`SIGTERM` clean quit.
- Added 30-day cache pruning (`prune_cache`) for `.json`, `.raw`, and `.zraw` files, refreshing timestamps on valid cache hits.

### Changed
- Replaced blocking startup decoding with immediate frame 0 display and background worker streaming over `GLib.idle_add`.
- Switched frame caching to `zlib`-compressed `.zraw` chunks decompressed on demand behind a bounded LRU `Gdk.MemoryTexture` cache (32 entries for short loops, 2 entries for longer loops).
- Replaced chained `GLib.timeout_add` timers with `Gdk.FrameClock` tick callbacks, skipping timers on single-frame images and recovering cleanly from clock stalls.
- Debounced config writes (500 ms) using atomic temp-file replacement (`os.replace`) and numeric type checks on load.
- Updated `preload_layer_shell` to fall back from `libgtk4-layer-shell.so` to `.so.0`, and used `Gtk.STYLE_PROVIDER_PRIORITY_USER` with a `load_from_data` fallback for older GTK 4 releases.

### Fixed
- Added `Gio.ApplicationFlags.NON_UNIQUE` so launching a second overlay opens that file independently instead of duplicating the first window.
- Added fail-fast exits when `LayerShell.is_supported()` is false, the target monitor is missing, or `on_activate` raises an exception.
- Added corrupt-cache fallback to re-decode from source instead of crashing, and joined active decode workers on exit to clean up temporary `.tmp-*.zraw` files.

## Prehistory (Pre-Git)

13. **Fullscreen transparent surface**
    Fixed the dragging lag root cause: moving a layer surface shifts the pointer reference frame while compositors apply margin updates late. Anchored a stationary fullscreen surface and moved the animation inside `Gtk.Fixed` so drag offsets match the pointer, with the input region tracking the handle.
12. **Per-file settings and robust clamping**
    Stored position and scale per file path, stopped upscaling small files, clamped coordinates after every move or resize against the pinned monitor, and gated debug logs behind `OVERLAY_DEBUG`.
11. **Left drag moves, right drag resizes**
    Split pointer drags by mouse button (anchoring resize at the top-left corner) while keeping scroll-wheel scaling.
10. **Raise CSS priority to 1000**
    Overrode opaque theme `gtk.css` window backgrounds by raising provider priority.
9. **Drag throttle and screen clamp**
    Throttled drag events to 30 ms and clamped coordinates on screen, treating the runaway-drag symptom before the `Gtk.Fixed` fix.
8. **Logging and frame limit**
    Added stage-by-stage logging and an optional frame limit, exposing that a runaway drag had parked the overlay off-screen at `x=9720, y=1109`.
7. **Preload layer-shell through ctypes**
    Loaded `libgtk4-layer-shell` before `libwayland` via `ctypes` so the surface stayed a layer window without `LD_PRELOAD`.
6. **GTK4 rewrite with GPU textures**
    Uploaded frames once as `Gdk.MemoryTexture` objects and restricted input to a corner handle region, making the rest of the image click-through.
5. **Edit mode with SIGUSR1**
    Added drag-to-move, scroll-to-resize, JSON persistence, and a `SIGUSR1` toggle (later dropped as too contrived before the `Gtk.Fixed` fix).
4. **Pillow decodes APNG directly**
    Decoded per-frame images and delays in Python with Pillow and resized frames at startup to keep memory bounded.
3. **Frame-folder workaround**
    Split APNG files into PNGs with `ffmpeg` and swapped them on a timer, preserving full alpha at a fixed frame rate.
2. **GTK3 + gtk-layer-shell overlay**
    Replaced Qt with a layer-shell overlay surface honored by Wayland compositors, limited by GIF's binary 1-bit alpha.
1. **PySide6 always-on-top window**
    Prototyped a frameless, translucent click-through `QLabel` playing GIFs via `QMovie`, limited by Wayland blocking applications from forcing always-on-top above fullscreen windows.
