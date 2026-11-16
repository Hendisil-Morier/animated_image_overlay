# ADR-002: Microsecond Frame Timing, Wayland Subsurface Sync, and Runtime Telemetry

## Status

Accepted

## Date

2026-11-16

## Context

Three runtime behavior issues appeared while testing high-refresh animations (`60 fps` to `240 fps`) and compositor compatibility on `niri` and KDE KWin:

1. **Millisecond Timing Quantization:** Rounding fractional APNG/WebP delays such as `1/60s` (`16.667 ms`) to `17 ms` slowed `60 fps` playback to `58.82 fps`, skipping every second refresh on `120 Hz` panels. Meanwhile, the legacy `10 ms` delay floor clipped animations faster than `100 fps` both when writing disk metadata and when arming runtime wait timers.
2. **Dragged Subsurface Lag on Compositors (`niri`):** During `Move` drags, calling `wl_subsurface.set_position` without committing `image_surface` or marking buffer damage left child frame movement delayed until the next animation tick (~`30 fps`). Unthrottled pointer motion events also emitted redundant chrome attachments and `wl_region` updates, and uninitialized cursor surfaces prevented seat pointer theme updates until later surface activity.
3. **Lack of Runtime Visibility:** Diagnosing whether stutter came from cold decode throughput, catch-up frame skips, or SHM staging latency required structured telemetry when requested (`--stats`) with zero overhead during normal execution.

## Decision

Store frame timing in microseconds independently of playback FPS limits, synchronize Wayland parent/subsurface commits and pointer frames, and gate 1-second metrics collection behind `--stats`.

### 1. Microsecond Delays (`delays_us`) and Configurable `-f`/`--max-fps` (`src/playback.rs`, `src/cache/disk.rs`, `src/cli.rs`)

- Persist frame timings in `CacheMetadata::delays_us` using a `CACHE_MIN_FRAME_DELAY_US = 1_000` (`1,000 fps`) guard (`DEFAULT_FRAME_DELAY_US = 33_000` fallback), while omitting legacy `delays_ms` so existing rounded caches fail validation cleanly in `try_open_cached_store` and re-decode.
- Enforce playback rate ceilings dynamically in `PlaybackClock::with_max_fps` via `-f` / `--max-fps` (`1..=1000`, defaulting to `DEFAULT_MAX_FPS = 240` / `MIN_FRAME_DELAY_US = 4_167`) while keeping disk metadata independent of per-launch `-f` values, and rebase deadlines past `STALL_RECOVERY_MS = 1000.0` (`STALL_RECOVERY_US`) after suspend gaps.

### 2. Coalesced Pointer Frames, Subsurface Commits, and Move Damage (`src/wayland/app.rs`, `src/wayland/scene.rs`, `src/wayland/chrome.rs`, `src/wayland/handlers.rs`, `src/wayland/input.rs`, `src/wayland/runtime/effects.rs`)

- Coalesce scene and cursor transitions once per `PointerHandler::pointer_frame` batch using `InputEffects::merge_scene_flags` while running immediate side effects (`OverlayRuntime::apply_immediate_effects`) per event so quits and pause toggles execute promptly.
- Run corner handle and edit border chrome on synchronized child subsurfaces that cache last committed coordinates, buffer scale, opacity appearance, and `wl_region` input rectangles (`OverlayChrome`, `OverlayScene::sync_layout_inner`).
- Commit `parent_layer` before `image_surface` during layout sync and emit `damage_buffer(0, 0, 1, 1)` on pure position changes (`prepare_image_surface_state`) so compositors repaint dragged child subsurfaces immediately.
- Eagerly create `cursor_surface` in `run_overlay` (`src/wayland/app.rs`) before binding `ThemedPointer` so pointer cursor updates succeed on initial seat capability events.

### 3. On-Demand `--stats` Telemetry (`src/cli.rs`, `src/playback.rs`, `src/wayland/stats.rs`, `src/wayland/runtime/state.rs`)

- Track `skipped_frames` and `frontier_held` per tick in `PlaybackStep` without extra allocation.
- Allocate `OverlayStats::with_cpu` and register its `STATS_INTERVAL_MS = 1000` `calloop` timer only when `--stats` is passed, sampling `ClockId::ProcessCPUTime` (`rustix::time::clock_gettime` via `OverlayStats::sample_cpu_time` and `OverlayStats::logical_cpu_count`) and `/proc/self/status` `VmRSS` (`OverlayStats::sample_process_ram_bytes`) to emit presented FPS, per-core (`cpu`) and host-normalized (`host`) CPU%, overall process RAM (`ram=...MiB`), catch-up skips, frontier holds, average/max microsecond staging latency, active cache tier (`ram(...)` vs `lraw`, plus `disp`), active `decode=decoding(...)` progress while decoding, and one-shot cold decode completion throughput (omitting `frame` cursor and post-decode `done`/`warm` labels).

## Alternatives Considered

- **Storing `-f` clamped delays directly inside `.json` metadata:** Rejected because launching the same image with `-f 60` and later `-f 120` would either replay stale slow timing or invalidate valid `.lraw` caches.
- **Full-surface buffer damage or continuous timer redraws during drags:** Rejected because extra SHM uploads raise idle and drag CPU load against `DisplayFrameCache` efficiency targets.
- **Always-active stats collection or background telemetry polling thread:** Rejected so default overlay runs keep zero measurement timers or stdout overhead.

## Consequences

- `60 fps` (`16_667 us`), `120 fps` (`8_333 us`), and `240 fps` (`4_167 us`) delays run at exact microsecond cadence under `calloop` timers while `-f` throttles playback cleanly on demand.
- Pointer dragging commits smoothly on `niri` and KDE KWin independently of animation frame interval.
- Pre-`0.3.1` `.json` cache sidecars missing `delays_us` automatically rebuild cleanly on startup.
