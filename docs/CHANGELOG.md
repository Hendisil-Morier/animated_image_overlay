# Changelog

All notable changes to this project are documented here.

## [0.4.0] - 2026-11-16

### Added

- Direct playback support for video containers (`MP4`, `MOV`, `WebM`, `MKV`, `AVI`) via `ffprobe` metadata probing and `ffmpeg` PAM `P7` RGBA stream piping with `showinfo` microsecond PTS parsing (`src/decode/video/`).
- In-process audio subsystem powered by `cpal 0.18` (`src/audio/`), featuring 48 kHz stereo `f32le` PCM decoding via `ffmpeg`, `PcmRing` lock-free buffering, click-free linear slew rate volume transitions (`SLEW_RAMP_SAMPLES = 256`), and A/V coupled loop clock synchronization (`CoupledLoop`).
- Companion audio track playback via `--audio <FILE>` (`MP3`, `OGG`, `WAV`, `FLAC`, `AAC`, etc.) paired with animations or static images in `IndependentAudioLoop` mode.
- Interactive edit-mode audio controls (`m` for mute toggle, `,` and `.` for 5% volume steps) and `--mute` / `--volume <LEVEL>` CLI flags.
- Audio preferences (`AudioPrefs { volume, muted }`) persisted under an `audio` object in `$XDG_CONFIG_HOME/apng-overlay.json`.
- Hybrid video caching architecture (`BackingStore`): short video loops (`<= 1800` frames and uncompressed bytes within `-c video=<MB>`, defaulting to 512 MiB) cache to `.lraw` disk storage for zero-CPU looping, while longer videos stream through `LiveVideoRing` maintaining a 16-frame bounded lookahead.
- Added 1-second interval process CPU (`cpu=...%`, `host=...%` via `CLOCK_PROCESS_CPUTIME_ID`) and overall process RAM (`ram=...MiB` via `/proc/self/status` `VmRSS`) telemetry to `--stats` (`OverlayStats` and `StatsSnapshot` in `src/wayland/stats.rs`), and removed redundant `frame` cursor and post-decode `decode=done/warm` tags so `--stats` focuses on performance metrics.

### Performance

- Parallelized per-frame APNG PNG decompression across the bounded worker pool (`src/decode/apng/`, `src/decode/codecs.rs`, `src/decode/pipeline/`, `src/decode/stream.rs`), splitting raw `fcTL`/`fdAT` packets sequentially via `ApngPacketReader`, inflating per-frame patches in parallel via `decode_apng_packet`, compositing blend and dispose operations in strict sequence via `ApngCompositor`, and prioritizing ready raw canvas encoding ahead of packet inflation to keep memory bounded and prevent pipeline stalls.

### Changed

- Split multi-role source modules and oversized integration test suites along domain boundaries while preserving all public API paths via pure `mod.rs` facades: decomposed `src/decode/apng/` (`reader.rs`, `patch.rs`, `compositor.rs`), `src/decode/pipeline/` (`coordinator.rs`, `stages.rs`), `src/decode/pixel/` (`frame.rs`, `rotate.rs`), and `src/wayland/runtime/` (`state.rs`, `playback.rs`, `effects.rs`, moving `handle_pointer_event` out of `src/wayland/handlers.rs`), plus split `tests/decode_tests.rs` and `tests/wayland_input_tests.rs` across `tests/support/mod.rs` (unifying duplicate synthetic APNG fixtures), `tests/pixel_tests.rs`, `tests/apng_tests.rs`, and `tests/wayland_ui_tests.rs`.

### Removed

- Removed legacy `-r` / `--ram-cache-mb` CLI flag in favor of the unified `-c ram=<MB>` cap option.

### Documentation

- Added [ADR-004](decisions/004-mp4-video-and-audio-pipeline.md) capturing MP4 video decoding, lookahead streaming, and in-process audio architecture.
- Added [ADR-002](decisions/002-microsecond-timing-and-compositor-sync.md) and [ADR-003](decisions/003-parallel-apng-decode-pipeline.md) capturing microsecond timing, compositor subsurface synchronization, `--stats` telemetry, and bounded dual-stage parallel APNG decoding decisions, and linked them from `README.md` and `docs/ARCHITECTURE.md`.

## [0.3.2] - 2026-11-16

### Added
- Added `--stats` CLI flag and `OverlayStats` (`src/wayland/stats.rs`) to emit 1-second stdout telemetry reporting presented FPS, catch-up frame skips, `!decode_done` frontier holds, frame staging latency (`avg`/`max` `us`), source RAM / `DisplayFrameCache` usage, and one-shot cold decode completion throughput.
- Added `-f` / `--max-fps` CLI flag (`CliArgs::max_fps`, `1..=1000`, default `240` fps) and `PlaybackClock::with_max_fps` to configure the playback frame rate ceiling.

### Changed
- Parallelized cold per-frame `Lanczos3` downscaling, premultiplication, and LZ4 compression across `(available_parallelism - 2).clamp(1, 6)` scoped workers bounded at `2 * workers` in-flight frames behind sequential container decoding (`src/decode/pipeline.rs`, `src/decode/stream.rs`), and flushed early startup frames (`<= EARLY_WAKEUP_FRAMES = 8`) individually into `SharedFrames` before transitioning to `DECODE_BATCH_SIZE = 4` (`src/cache/sink.rs`) to smooth out initial loops on `60 fps` and `120 fps` animations.
- Lowered the default playback delay floor from `10_000 us` (`100 fps`) to `4_167 us` (`240 fps`) with a `1_000 us` (`CACHE_MIN_FRAME_DELAY_US`) decoded `.json` cache floor so `120 fps`, `144 fps`, and `240 fps` animations play natively while preserving `0 ms` quirk protection.
- Upgraded frame timing, `.json` cache metadata (`delays_us`), `PlaybackClock`, and `calloop` frame wait timers from millisecond to microsecond precision (`16_667 us` for `1/60`s APNG/WebP frames).
- Coalesced scene and pointer cursor synchronization once per `wl_pointer.frame` batch (`PointerHandler::pointer_frame`, `InputEffects::merge_scene_flags`, `OverlayRuntime::apply_immediate_effects`), skipped redundant `WlRegion` input-region updates when `input_rect` is unchanged, switched corner handle and edit outline subsurfaces to synchronized commits with cached placement/appearance diffing (`OverlayChrome::hide_outline`, `OutlineEdge::apply_placement`), and committed `parent_layer` before `image_surface` in `OverlayScene::sync_layout_inner`.

### Fixed
- Emitted minimal `1x1` buffer damage and committed `image_surface` when `image_subsurface.set_position` changes without a simultaneous scale, viewport, transform, or frame change (`OverlayScene::prepare_image_surface_state`), preventing pure Move drags from waiting for the animation timer cadence to repaint `image_surface` on compositors such as `niri`.

## [0.3.1] - 2026-11-15

### Changed
- Extracted software corner handle and dashed outline rasterization plus `ChromeGeometry` and `needs_chrome_rebuild` into `src/wayland/chrome_raster.rs`, keeping Wayland subsurface orchestration in `src/wayland/chrome.rs`.
- Extracted SHM ring staging, `DisplayFrameCache` routing, and Lanczos3/rotation resampling (`FrameStage`, `compute_buffer_target_spec`, `ring_needs_resync`) into `src/wayland/frame_stage.rs`, and decomposed `OverlayScene::sync_layout_inner` (`src/wayland/scene.rs`), `run_overlay` (`src/wayland/app.rs`), `apply_input_effects` (`src/wayland/runtime.rs`), `apply_cap_overrides` (`src/cli.rs`), and `with_frame_argb` (`src/cache/store.rs`).
- Unified startup frame-0 staging and steady-state animation staging through `FrameStage::stage_frame_slice` (`src/wayland/frame_stage.rs`), sizing startup `ImageBufferRing` allocations directly from `compute_buffer_target_spec`.

### Fixed
- Enabled startup and resize Lanczos3 downscaling (`resize_premultiplied_argb8888`) directly into SHM buffers for completed single-frame static images (`decode_done && frame_count == 1`) without allocating `DisplayFrameCache` slots.
- Checked `OverlayScene::needs_buffer_resync` inside post-startup `LayerShellHandler::configure` events (`src/wayland/handlers.rs`) so compositor output size shrinks refresh SHM buffers cleanly while leaving persisted scale untouched in `apng-overlay.json`.

## [0.3.0] - 2026-11-15

### Added
- Added [ARCHITECTURE.md](ARCHITECTURE.md) documenting the Wayland layer-shell/subsurface scene, headerless `v2-lraw-argb8888` `.lraw`/`.json` cache format, `apng-overlay.json` schema/concurrency lock, and `calloop`/logging lifecycle.
- Added `-c` / `--cap` CLI option (`ram=<MB>` and `display=<MB>` suboptions, accepted individually or comma-separated) and `DisplayFrameCache` (`src/wayland/display_cache.rs`, default `64 MiB`) to eliminate steady-state LZ4 decompression and resampling CPU overhead when display frames fit budget.
- Added edit-mode `[` and `]` repeatable shortcuts (`+/-5` degrees) and `0` reset shortcut (`KeyShortcut::Rotate`, `KeyShortcut::ResetRotation`) alongside bilinear premultiplied `ARGB8888` CPU rotation with a `1 px` transparent apron (`decode::rotate_argb8888_into`) and dynamic `ImageBufferRing` resizing for arbitrary angles.
- Added edit-mode `f` and `F` keyboard shortcuts (`FlipAxis`, `KeyShortcut::Flip`, and `Transform::Flipped*` via `wl_surface::set_buffer_transform`) to mirror the overlay horizontally or vertically using dihedral composition with `rotation`, persisted per image via `flipped` inside `apng-overlay.json`.

### Changed
- Kept the faded `28x28` corner handle subtly visible (`~15%` opacity via `HANDLE_FADED_ALPHA = 38`) after the 2-second idle timeout so the grab target stays easy to spot while passive.
- Generalized `OverlayState::rotation` and `OverlayLayout::rotation` from discrete quarter-turn variants to normalized finite `f64` degrees in `[0.0, 360.0)` while preserving zero-copy Wayland `wl_surface::set_buffer_transform` fast paths on snapped 90-degree multiples.
- Partitioned fractional rotation (`decode::rotate_argb8888_into`) per scanline using `linear_in_bounds` into unchecked interior spans (`write_interior_pixel_swar` with `12`-bit bilinear weights packed as dual `[B, R]` / `[G, A]` `u64` SWAR lanes) and `1 px` transparent apron tails (`write_apron_pixel`).
- Cached display-resolution rotated and downscaled quarter-turn buffers lazily inside `DisplayFrameCache` (`-c display`, `64 MiB` default, normalized via `angle_key`), pairing canonical `(angle=0.0, flipped=false)` keys on oversized quarter turns (`!fits_source_ram`) with `wl_surface::set_buffer_transform` so all eight dihedral quarter-flip orientations share cached Lanczos3 downscales while oversized overlays (`!would_fit`) fall back to `.lraw` streaming (see [ADR-001](decisions/001-display-cache-and-resampling.md)).
- Deferred SHM ring resizes during pointer resize drags until button release (`InputState::is_resizing`) while resyncing physical buffer density on compositor scale changes and shrinking hysteresis (`want_pixels * 3 <= cur_pixels * 2`) via `OverlayScene::needs_buffer_resync`.
- Aligned [CODING_STYLE.md](CODING_STYLE.md) with `Cargo.toml` lints (`unsafe_code = "forbid"`, `missing_docs = "warn"`), `src/lib.rs` / `src/main.rs` unwrap denials, and daily rotating logging, and removed outdated hardcoded `128 MiB` / magic-header doc comments from `src/lib.rs`, `src/cache/`, and `src/wayland/runtime.rs`.

### Fixed
- Corrected [README.md](../README.md) and `--help` documentation (`12 px` nudge distance, zoom key variants `_`/`=`/numpad, consistent corner-handle middle-click wording, `-c ram` overriding `-r`, deduplicated `OVERLAY_DEBUG`, and `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` overrides).
- Separated alpha-premultiplication handling inside `decode::pixel::resize_u8x4` so `resize_premultiplied_argb8888` passes `ResizeOptions::use_alpha(false)` on premultiplied `ARGB8888` cache frames while `downscale_if_needed` retains `use_alpha(true)` for straight RGBA decode input, fixing double premultiplication on semi-transparent edges and dropping `Resizer::alpha_buffer` (`12.25 MB` steady `Pss_Anon` savings on `M_Animation.apng`).

### Benchmark Comparison (`~/Pictures/M_Animation.apng -w 5000`, `1492x2158`, `180` frames, KDE KWin `eDP-1` `2x`)
- **Pre-optimization Profiler Hotspots (PID `347530`, `310.0` deg, `scale=0.033`):** `32.4%` single-core CPU (`~2.0–2.5%` host across 16 cores, `26,212 kB` `Rss`), split across `73.62%` `cpu_core` / `74.28%` `cpu_atom` samples in `decompress_chunk_into` (`~390 MB/s` LZ4 page-cache reads) and `16.42%` in `rotate_argb8888_into`.
- **Rotated Thumbnail (`310.0` deg, `scale=0.033`):** `-c display=0` (`SWAR` streaming) measures `17.66%` single-core (`1.10%` host, `26,108 kB` `Rss`, `19,140 kB` `Pss_Anon`, `192 kB` `Pss_Shmem`); default `-c display=64` drops CPU to `0.67%` single-core (`0.04%` host, `-48x` single-core CPU vs baseline) at `46,792 kB` `Rss` (`39,456 kB` `Pss_Anon`, `192 kB` `Pss_Shmem`).
- **Quarter-Turn Thumbnail (`0.0` deg, `scale=0.033`):** `-c display=0` measures `19.33%` single-core (`1.21%` host, `51,124 kB` `Rss`, `19,144 kB` `Pss_Anon`, `12,656 kB` `Pss_Shmem`); default `-c display=64` downscales to physical overlay size and drops CPU to `0.67%` single-core (`0.04%` host, `56,908 kB` `Rss`, `49,244 kB` `Pss_Anon`, `238 kB` `Pss_Shmem`).
- **Large Overlay Budget Guard (`scale=0.24`, exceeds `64 MiB` display cap):** Disables `DisplayFrameCache` cleanly and streams directly, keeping `Pss_Anon` at `19,116 kB` (`50,928 kB` `Rss` at `0.0` deg; `38,124 kB` `Rss` and `6,026 kB` `Pss_Shmem` at `310.0` deg).

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
