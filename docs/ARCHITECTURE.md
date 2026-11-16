# Architecture

This document covers the Wayland surface stack, on-disk `.lraw` cache format, JSON configuration persistence, and runtime scheduling contracts for `animated_image_widget`. Detailed rationale and trade-offs live in [ADR-001](decisions/001-display-cache-and-resampling.md) (display cache and resampling), [ADR-002](decisions/002-microsecond-timing-and-compositor-sync.md) (microsecond timing, compositor subsurface sync, and telemetry), and [ADR-003](decisions/003-parallel-apng-decode-pipeline.md) (parallel APNG decode pipeline).

## 1. Wayland Layer-Shell and Subsurface Scene

Startup (`wayland::app::run_overlay` in `src/wayland/app.rs`) connects via `Connection::connect_to_env()` and requires five compositor globals: `wl_compositor`, `wl_subcompositor`, `zwlr_layer_shell_v1`, `wp_viewporter`, and `wl_shm`. Two `event_queue.roundtrip` passes resolve `wl_output` and `xdg_output` metadata before `MonitorDescriptor` and `resolve_monitor_index` (`src/wayland/monitor.rs`) select the target output.

### Parent Surface and Subsurface Stack

Moving a compact layer surface dynamically shifts the pointer coordinate origin while compositors apply margin updates asynchronously, producing runaway drag lag. To keep coordinates stable, `OverlayScene` (`src/wayland/scene.rs`, delegating buffer staging to `FrameStage` in `src/wayland/frame_stage.rs`) anchors one stationary fullscreen parent and moves child subsurfaces within it:

```text
parent_layer (Layer::Overlay, namespace "apng-overlay", Anchor TOP|BOTTOM|LEFT|RIGHT, exclusive_zone -1)
  ├─ 1x1 transparent wl_shm buffer stretched to monitor size via parent_viewport (wp_viewporter)
  ├─ image_subsurface / image_surface (set_desync, empty input_region, place default)
  │    └─ ImageBufferRing (2..3 ARGB8888 slots), scaled via image_viewport + wl_surface::set_buffer_transform
  ├─ handle_subsurface / handle_surface (place_above image_surface, sync, empty input_region)
  │    └─ 28x28 rounded pill at integer HiDPI buffer scale (3 pre-rendered opacity buffers)
  └─ 4x OutlineEdge subsurfaces (place_below handle_surface, sync, empty input_region)
       └─ 1 px dashed strips dynamically cropped via wp_viewport (mapped only in edit mode)
```

All child subsurfaces carry empty Wayland input regions so pointer focus and cursor themes stay centralized on `parent_layer`.

### Orientation, HiDPI Chrome, and Mode Switching

- **Quarter-Turn Transform Mapping (`wl_transform_for_orientation`):** Wayland compositor `set_buffer_transform` displays the inverse of `wl_output::Transform` (applying horizontal flip around the vertical axis before counter-clockwise rotation), so clockwise visual `(rotation, flipped)` pairs map via `inverse(R_ccw * F) = F * R_cw` onto `Transform::{Normal, _90, _180, _270, Flipped, Flipped90, Flipped180, Flipped270}` when `image_surface.version() >= 2`. Fractional angles use `Transform::Normal` with rotated buffers rendered by `decode::rotate_argb8888_into`.
- **Corner Handle and Edit Borders (`src/wayland/chrome.rs`, `src/wayland/chrome_raster.rs`):** `HandleBuffers` pre-renders three premultiplied white pill textures (`HANDLE_SIZE = 28`, `HANDLE_CORNER_RADIUS = 8`) in one `SlotPool` sized for the compositor's integer buffer scale (`scale_factor_changed`): `HANDLE_FADED_ALPHA = 38` (`~15%` opacity after `HANDLE_FADE_MS = 2000`), `HANDLE_IDLE_ALPHA = 64` (`~25%`), and `HANDLE_HOVER_ALPHA = 217` (`~85%`). Edit borders pre-render four `1 px` dashed strips (`OUTLINE_DASH_PERIOD = 12`, `OUTLINE_ALPHA = 140`) up to monitor dimensions via `chrome_raster.rs` and crop them per layout sync with `wp_viewport` (`ChromeGeometry::edge_viewport_crop`), growing and re-allocating the chrome pool if screen bounds expand (`needs_chrome_rebuild`). Both handle and edge chrome subsurfaces stay synchronized (`sync`) and cache their last committed placement/appearance so redundant attaches, viewport updates, and hidden-edge detach commits are skipped.
- **Passive vs Edit Input (`OverlayScene::sync_layout_inner`):** Delegates viewport sizing (`sync_parent_viewport`), image transforms and viewport destination (`prepare_image_surface_state`), and mode switching (`sync_interactivity_and_input_region`). `sync_layout_inner` commits `parent_layer` before `image_surface`, and `prepare_image_surface_state` emits `1x1` buffer damage so pure position moves commit `image_surface` cleanly. Passive mode sets `KeyboardInteractivity::None` and restricts `parent_layer` input to `layout.input_rect(false)` (the `28x28` bottom-right handle). Edit mode switches to `KeyboardInteractivity::OnDemand` and expands `parent_layer` input to `layout.input_rect(true)` (`overlay_rect()`).

## 2. Disk Cache Wire Format (`v2-lraw-argb8888`) and Decode Pipeline

`FrameCache` (`src/cache/`) stores decoded frames under `$XDG_CACHE_HOME/apng-overlay` (default `~/.cache/apng-overlay`).

### Cache Key Derivation

`compute_cache_paths` (`src/cache/disk.rs`) formats deterministic `<digest>.lraw` and `<digest>.json` filenames by taking the first `16` hex characters (`CACHE_DIGEST_HEX_LEN = 16`) of SHA-256 over:

1. `b"v2-lraw-argb8888:"` format prefix (`CACHE_KEY_PREFIX`, isolating Rust `.lraw` blobs from legacy `.zraw`).
2. `ConfigStore::state_key(image_path)` UTF-8 bytes followed by `b":"`.
3. Source modification time in nanoseconds since `UNIX_EPOCH` (`u128::to_le_bytes`, `0` if unavailable).
4. Source file length in bytes (`u64::to_le_bytes`), `base_width` (`u32::to_le_bytes`), and `max_frames` (`u64::to_le_bytes`).

### Binary `.lraw` Layout and `.json` Sidecar Metadata

`.lraw` files contain no file header: compressed frame chunks are concatenated back-to-back starting at byte offset `0`. Each chunk is written by `lz4_flex::block::compress_prepend_size` (4-byte uncompressed length header followed by LZ4 block bytes) from premultiplied little-endian `ARGB8888` (`[B, G, R, A]`) pixels scaled without upscaling to `min(base_width, src_w)`.

Sibling `<digest>.json` stores `CacheMetadata`:

```json
{
  "w": 500,
  "h": 723,
  "delays_us": [33000, 33000, ...],
  "sizes": [418210, 419004, ...]
}
```

- `delays_us` holds per-frame display durations in microseconds cached with a `CACHE_MIN_FRAME_DELAY_US = 1_000` (`1000 fps`) guard (`DEFAULT_FRAME_DELAY_US = 33_000` when metadata omits timing) and clamped at runtime by `PlaybackClock` to the `-f` / `--max-fps` ceiling (`DEFAULT_MAX_FPS = 240`, `MIN_FRAME_DELAY_US = 4_167`).
- `sizes` lists exact byte lengths of each sequential chunk inside `.lraw`, reconstructed by `build_chunk_spans` (`src/cache/reader.rs`) into `ChunkSpan { offset, len }` for seek-free `FileExt::read_exact_at` (`pread`) reads inside `LrawReader`.

### Crash Safety, Validation, and Pruning

1. **Single-Open Ordered Parallel Streaming Decode (`src/decode/apng/`, `src/decode/codecs.rs`, `src/decode/stream.rs`, `src/decode/pipeline/`, `src/cache/sink.rs`):** Cold launches open the source container once (`OpenedSource` across APNG, static PNG, GIF, WebP, and stills). Frame 0 decodes synchronously on startup so the widget renders immediately. For APNG files, `aiw-decoder` splits raw `fcTL`/`fdAT` chunks sequentially via `ApngPacketReader` (`2 * workers` max in flight), `(available_parallelism - 2).clamp(1, 6)` scoped workers inflate patches via `decode_apng_packet` and encode `Lanczos3` + premultiplied ARGB + LZ4 frames in parallel (prioritizing ready raw frames to avoid backpressure stalls), a dedicated compositor thread runs `ApngCompositor::apply_patch` across blend and dispose operations, and an ordered sink thread feeds `DecodeCacheSink` (`EARLY_WAKEUP_FRAMES = 8` individual flushes on startup before `DECODE_BATCH_SIZE = 4`) into `SharedFrames` and pings `calloop`. GIF and WebP animations feed sequential container frames directly into the same parallel encode pool and ordered sink.
2. **Atomic Commit Ordering (`src/atomic_fs.rs`, `src/cache/sink.rs`):** `DecodeCacheSink` writes compressed chunks into a sibling temporary file (`.tmp-<pid>-<seq>.lraw` opened via `O_EXCL`). If cancelled or errored, `AtomicTempFile::drop` unlinks the incomplete temporary blob. Only after `.lraw` commits via `fs::rename` does `write_cache_metadata` atomically write `<digest>.json`.
3. **Warm Validation Fallback (`src/cache/disk.rs`, `src/cache/reader.rs`):** On cache hits, `try_open_cached_store` validates non-zero `(w, h)`, non-empty matching `delays_us`/`sizes` lengths, non-zero chunk sizes, sufficient `.lraw` length, and frame 0 LZ4 decompression via `read_verified_frame0`. Invalid caches log a warning and fall back to decoding cleanly from source.
4. **Timestamp Refresh and 30-Day Pruning:** Valid cache opens refresh both `.json` and `.lraw` mtimes (`touch_modified`). Both startup cache hits and completed cold decodes run `prune_cache` (`DEFAULT_PRUNE_AGE_DAYS = 30`), sorting `.json` markers before `.lraw`, `.zraw`, and `.raw` payloads when unlinking expired entries so orphaned blobs never appear valid.

## 3. Persistent Config Schema (`apng-overlay.json`)

`ConfigStore` (`src/config.rs`) reads and updates `$XDG_CONFIG_HOME/apng-overlay.json` (fallback `~/.config/apng-overlay.json`), preserving entries for other image files across concurrent overlay processes.

### Schema and Recovery

Top-level JSON maps normalized path keys produced by `ConfigStore::state_key` (`std::path::absolute` passed through `normalize_lexical`, collapsing `.` and `..` components without dereferencing symlinks) to `OverlayState` objects:

```json
{
  "/home/user/Pictures/example.apng": {
    "x": 100.0,
    "y": 100.0,
    "scale": 0.6,
    "rotation": 0.0,
    "flipped": false
  }
}
```

| Field | Type | Default | Constraints and Fallback Behavior |
| :--- | :--- | :--- | :--- |
| `x` | `f64` | `100.0` | Monitor-clamped logical X coordinate; missing or non-finite values fall back independently to `100.0`. |
| `y` | `f64` | `100.0` | Monitor-clamped logical Y coordinate; missing or non-finite values fall back independently to `100.0`. |
| `scale` | `f64` | `0.6` | Display multiplier clamped to `>= 84 px` (`3 * HANDLE_SIZE`) short axis and monitor bounds; missing or non-finite values fall back to `0.6`. |
| `rotation` | `f64` | `0.0` | Clockwise degrees normalized to `[0.0, 360.0)` and snapped within `1e-6` degrees of multiples of `90`; non-finite values fail field parse and fall back to `0.0`. |
| `flipped` | `bool` | `false` | Horizontal mirror flag applied across visual bounds; missing or non-boolean values fall back to `false`. |

`load_state_entry` reads each field individually from a `Value::Object`, falling back per field on corrupt or missing properties while leaving invalid top-level files mapped as empty rather than failing startup.

### Multi-Instance Write Protocol

Pointer drags, wheel/keyboard scaling, rotation, and flips mark `dirty_save = true` and schedule a `500 ms` (`SAVE_DEBOUNCE_MS`) timer in `OverlayRuntime`, flushing synchronously on clean exit or signal (`shutdown_cleanup`). `save_state_entry` serializes concurrent overlay instances cleanly:

1. Create parent directories if absent and open sibling `<config>.lock`.
2. Acquire an exclusive advisory lock (`File::lock`, released by `ExclusiveLockGuard::drop`).
3. Re-read `<config>` from disk as a `serde_json::Map<String, Value>` (preserving all sibling image entries and unrecognized JSON keys).
4. Insert the serialized `OverlayState` under this instance's key and atomically replace `<config>` via `atomic_fs::write_bytes` (`.tmp-<pid>-<seq>.json` + `fs::rename`).

## 4. Event Loop, Playback Clock, and Diagnostics

- **Event Loop (`src/wayland/app.rs`, `src/wayland/runtime/`, `src/wayland/stats.rs`):** `prepare_signals` masks `SIGINT` and `SIGTERM` in `main()` before spawning logging or decoder worker threads so signal delivery hits `calloop::signals::Signals` deterministically. The loop coordinates `WaylandSource`, background decode wakeups (`make_ping`), animation wait timers, `SAVE_DEBOUNCE_MS` (`500 ms`), `HANDLE_FADE_MS` (`2000 ms`), `STATS_INTERVAL_MS` (`1000 ms` when `--stats` is set, sampling `CLOCK_PROCESS_CPUTIME_ID` for CPU% and `/proc/self/status` `VmRSS` for overall RAM usage), and `FRAME_ERROR_RETRY_MS` (`100 ms`) recovery retries after transient SHM/cache staging failures.
- **Playback Clock (`src/playback.rs`):** `PlaybackClock` maintains monotonic microsecond deadlines (`next_deadline_us`) clamped to the configurable `--max-fps` ceiling (`DEFAULT_MAX_FPS = 240`, `4_167 us`), holds on the final buffered frame while `!decode_done`, stops Wayland callbacks on static or paused images, and rebases stale deadlines if elapsed drift exceeds `STALL_RECOVERY_US = 1_000_000.0` (e.g. after system suspend) instead of spinning catch-up frames.
- **Dual-Output Diagnostics (`src/logging.rs`):** `build_subscriber_in_dir` writes `DEBUG`-level records through a non-blocking `RollingFileAppender` (`Rotation::DAILY`, `apng-overlay.*.log`, `max_log_files(7)`) under `$XDG_CACHE_HOME/apng-overlay/logs/`, mirrors `DEBUG` output to stdout when `--debug` or truthy `OVERLAY_DEBUG` is active, and flushes pending buffers both in `main()` shutdown (`LoggingGuard::flush`) and in a chained panic hook (`install_panic_flush_hook`).
