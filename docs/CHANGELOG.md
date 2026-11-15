# Changelog

All notable changes to this project are documented here.

## [Unreleased]

### Added
- Added `-c` / `--cap` CLI option (`ram=<MB>` and `display=<MB>` suboptions, accepted individually or comma-separated) and `DisplayFrameCache` (`src/wayland/display_cache.rs`, default `64 MiB`) to eliminate steady-state LZ4 decompression and resampling CPU overhead when display frames fit budget.
- Added edit-mode `[` and `]` repeatable shortcuts (`+/-5` degrees) and `0` reset shortcut (`KeyShortcut::Rotate`, `KeyShortcut::ResetRotation`) alongside bilinear premultiplied `ARGB8888` CPU rotation with a `1 px` transparent apron (`decode::rotate_argb8888_into`) and dynamic `ImageBufferRing` resizing for arbitrary angles.
- Added edit-mode `f` and `F` keyboard shortcuts (`FlipAxis`, `KeyShortcut::Flip`, and `Transform::Flipped*` via `wl_surface::set_buffer_transform`) to mirror the overlay horizontally or vertically using dihedral composition with `rotation`, persisted per image via `flipped` inside `apng-overlay.json`.

### Changed
- Kept the faded `28x28` corner handle subtly visible (`~15%` opacity via `HANDLE_FADED_ALPHA = 38`) after the 2-second idle timeout instead of detaching invisibly so the grab target stays easy to spot.
- Generalized `OverlayState::rotation` and `OverlayLayout::rotation` from discrete quarter-turn variants to normalized finite `f64` degrees in `[0.0, 360.0)` while preserving zero-copy Wayland `wl_surface::set_buffer_transform` fast paths on snapped 90-degree multiples.
- Resampled arbitrary-angle frames directly at physical display resolution via `decode::rotate_argb8888_into` (using 2-pixel `u64` SWAR interior scanlines) and cached them in `DisplayFrameCache` when fitting `-c display`, falling back to direct `ImageBufferRing::fill_and_attach` streaming for larger buffers.
- Deferred SHM ring resizes during pointer resize drags until button release (`InputState::is_resizing`) while resyncing physical buffer density on compositor scale changes and downscaling oversized quarter-turn animations (`decode::resize_premultiplied_argb8888`) via `OverlayScene::needs_buffer_resync`.

## [0.2.2] - 2026-11-15

### Added
- Added `-r` / `--ram-cache-mb` CLI flag (`FrameCache::open_with_ram_budget`) to set the RAM ceiling in MiB for keeping decoded frames in memory or pass `0` to read every animation from disk (default: `128`).
- Added `-d` / `--debug` CLI flag (`logging::init_logging_with_debug`) to print debug diagnostics to stdout alongside rolling daily log files, matching `OVERLAY_DEBUG=1`.
- Added edit-mode `r` and `R` keyboard shortcuts (`Rotation`, `KeyShortcut::Rotate`, and `wl_surface::set_buffer_transform`) to rotate the overlay in 90-degree clockwise or counter-clockwise increments around its visual center.

### Changed
- Persisted `rotation` (`0`, `90`, `180`, `270` degrees) per image inside `apng-overlay.json` via `OverlayState` (`src/config.rs`), scheduled debounced saves when rotating with `r` / `R`, and committed initial `set_buffer_transform` alongside frame 0 inside `OverlayScene::initialize_first_frame` so restored orientations start without an unrotated flash.

## [0.2.1] - 2026-11-15

### Changed
- Streamed `.lraw` cache chunks lazily from disk via positional reads (`LrawReader` and `ChunkSpan`) and a single reusable `FrameScratch` decompression buffer, removing permanent `compressed_frames` heap retention and background cache reader threads on warm opens.
- Skipped populating the 128 MiB uncompressed RAM LRU cache (`ram_slots`) when an animation's decoded footprint exceeds the configured budget, preserving LRU caching for smaller loops while passing borrowed frame pixels directly to Wayland SHM staging via `FrameCache::with_frame_argb`.

### Benchmark Comparison (`~/Pictures/M_Animation.apng`, `500x723`, 180 frames)
- **Warm Steady-State Memory:** `16,636 kB` `VmRSS` (`-90.8%` vs `v0.2.0` `181,180 kB`), `12,435 kB` `Pss` (`-93.0%` vs `176,745 kB`), and `7,036 kB` `RssAnon` (`-95.9%` vs `171,604 kB`) at `3.50%` 2s CPU load.

## [0.2.0] - 2026-11-15

### Added
- Added `-e` / `--edit` CLI flag to launch directly into interactive edit mode (`InputState::with_editing`).
- Added an interactive mouse and keyboard shortcut summary footer to `--help`.
- Added integer HiDPI chrome scaling in `CompositorHandler::scale_factor_changed` via `ChromeGeometry` and `OverlayChrome::ensure_scale_and_bounds` so corner handle and dashed outline buffers render crisply across scaled outputs and growing monitor dimensions.

### Changed
- Replaced `FrameCache::delays_snapshot` allocations with closure-based `FrameCache::with_delays`, added `PlaybackClock::ms_until_next_frame`, and moved animation waits onto `calloop` timers that stage the next SHM frame ahead of the compositor callback and cancel cleanly on pause or shutdown.
- Unified cold startup image probing and decoder creation behind `OpenedSource` in `decode::codecs` and `decode::stream` so source files open once across APNG, static PNG, GIF, WebP, and still formats.

### Fixed
- Updated `config` to honor non-empty `XDG_CONFIG_HOME` overrides before falling back to `$HOME/.config` alongside shared `xdg::base_dir` resolution used by cache paths.
- Reset hover, active drag, and click tracking via `InputState::on_pointer_detach` when pointer capabilities detach, guarded chorded button presses during active drags, logged `LayerShellHandler::configure` layout sync failures, and scheduled a `100 ms` recovery timer when frame staging errors occur.

## [0.1.0] - 2026-11-15 (Rust Wayland Rewrite)

### Added
- Rewrote the overlay as a self-contained Rust binary (`animated_image_widget`, edition `2024`, `unsafe_code = "forbid"`) targeting `zwlr_layer_shell_v1`, `wl_subcompositor`, `wl_shm`, and `wp_viewporter` directly via `smithay-client-toolkit` and `calloop`.
- Added streaming decode support (`src/decode/`) for APNG, GIF, animated WebP (`webp-animation`), and static formats with `fast_image_resize` Lanczos3 downscaling and premultiplied `ARGB8888` conversion.
- Added LZ4 `.lraw` + `.json` cache (`src/cache/`) with atomic `.tmp-*` staging, 30-day pruning (`.json`, `.raw`, `.zraw`, `.lraw`), and a 128 MiB uncompressed RAM LRU cache.
- Decomposed decoder (`codecs`, `pixel`, `stream`), cache (`disk`, `memory`, `reader`, `sink`, `store`), and Wayland frontend (`app`, `runtime`, `handlers`, `scene`, `chrome`, `shm_ring`, `monitor`, `input`, `shortcuts`) into single-responsibility modules.
- Added triple-click edit mode toggling (`400 ms` / `4 px` threshold), `1 px` dashed viewport-cropped edit border, `KeyboardInteractivity::OnDemand` edit shortcuts (`Esc`, `q`, arrows/`hjkl`, `+`/`=`/`-`, `Space`), `28x28` auto-fading corner handle, and integration test suites under `tests/` (34 tests).
- Masked `SIGINT`/`SIGTERM` via `calloop::signals::Signals` in `main()` before spawning logging or decoder worker threads so signal delivery deterministically triggers cleanup, config flush, and exit status `0`.
- Archived the legacy Python + GTK 4 implementation and its 13 unit tests under `python_scripts/`.

### Benchmark Comparison (`~/Pictures/M_Animation.apng`, `500x723`, 180 frames, KDE KWin `eDP-1`)
- **Steady-state `VmRSS`:** `186,628 kB` vs Python `216,404 kB` (`-29,776 kB` / `-13.8%`; peak `VmHWM`: `190,512 kB`).
- **Steady-state `Pss`:** `182,228 kB` (`Pss_File: 3,676 kB`, `Pss_Anon: 177,124 kB` retaining the 128 MiB uncompressed LRU frame cache plus 35.4 MiB LZ4 frame store) vs Python `136,587 kB` (shared GTK4/Cairo `.so` pages).
- **10s Steady-State CPU (`perf stat`):** `1,061,937,340` `cpu_core/cycles/` (`96.51%`, vs Python `6.59B` `cpu_core` cycles, `-83.9%`) plus `1,070,253,341` `cpu_atom/cycles/` (`3.49%`).
- **Disk Cache Size:** `37,084,879` bytes (`35.4 MiB`) `.lraw` + `.json` vs Python `~98 MB` `.zraw`.
- **Startup & Decode:** `15 ms` warm startup to event loop; `126 ms` cold startup to frame 0 loop entry (`3.25 s` full 180-frame background decode); steady `2` open `memfd` FDs during 10s playback (`11,372 kB` `VmRSS` / `8,655 kB` `Pss` on static PNG).

## [Unreleased] - 2026-11-15 (Python Baseline)

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
- Serialized `save_state_entry` in `config.py` using an exclusive `fcntl.flock` on `f"{CONFIG_PATH}.lock"` across read, merge, and `atomic_write_json`, preventing concurrent `NON_UNIQUE` overlay instances from overwriting one another's updates across `os.replace`.
- Updated single-frame caching in `FrameStore.prepare_first_frame` (`cache.py`) to atomically write `.zraw` data via `atomic_write_bytes` before committing `.json` metadata via shared `_write_cache_meta`, matching `_decode_remaining_frames` and verified by new tests in `tests/test_overlay.py`.

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
