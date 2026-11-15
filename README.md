# Animated Image Overlay

Self-contained transparent, click-through Wayland desktop widget for playing animated and static images (APNG, GIF, animated WebP, PNG, and other formats) above all windows using `wlr-layer-shell` directly over `wl_shm` without GTK or Python runtime dependencies.

## Requirements

- A Wayland compositor supporting `zwlr_layer_shell_v1`, `wl_subcompositor`, `wl_shm`, and `wp_viewporter` (such as Sway, Hyprland, River, or KDE KWin; not supported on X11 or GNOME Shell)
- Rust toolchain (stable or nightly for `rustfmt`) and a C toolchain for building the bundled `webp-animation` codec

## Build and Usage

Build the optimized release binary:

```bash
cargo build --release
```

Run the overlay:

```bash
./target/release/animated_image_widget FILE [-n MAX_FRAMES] [-w BASE_WIDTH] [-m MONITOR] [-s SCALE] [-r RAM_CACHE_MB] [-c ram=<MB>,display=<MB>]... [-e] [-d]
```

Options:
- `FILE`: Path to the image or animation file.
- `-n`, `--max-frames`: Maximum number of frames to decode (`0` decodes all frames).
- `-w`, `--base-width`: Width in pixels used for stored cache frames (default: `500`; never upscales smaller files).
- `-m`, `--monitor`: Target monitor index or connector name such as `DP-1` (default: `0`).
- `-s`, `--scale`: Initial display scale, overriding any saved scale for that file.
- `-r`, `--ram-cache-mb`: RAM limit in MiB for keeping short animations unpacked in memory; larger files read one frame at a time from disk (`0` always reads from disk; default: `128`; overridden if `-c ram=<MB>` is also given).
- `-c`, `--cap`: Repeatable comma-separated memory ceilings in MiB accepting `ram=<MB>` and `display=<MB>` individually or together (`-c ram=10`, `-c display=32`, `-c ram=10,display=32`, `-c ram=10 -c display=32`; `0` disables that tier; defaults: `ram=128,display=64`). When both `-r` and `-c ram=<MB>` are supplied, `-c ram` takes precedence regardless of argument order.
- `-e`, `--edit`: Launch directly into interactive edit mode.
- `-d`, `--debug`: Mirror `DEBUG`-level diagnostics to stdout alongside the rolling daily log files under `$XDG_CACHE_HOME/apng-overlay/logs/`.

### Environment Variables

- `WAYLAND_DISPLAY`: Target compositor socket used when connecting via `Connection::connect_to_env()`.
- `XDG_CONFIG_HOME`: Overrides `~/.config` for `apng-overlay.json` and `apng-overlay.json.lock` when set and non-empty.
- `XDG_CACHE_HOME`: Overrides `~/.cache` for `<cache>/apng-overlay/` frame caches and `<cache>/apng-overlay/logs/`.
- `OVERLAY_DEBUG`: Enables stdout `DEBUG` logging alongside disk logs for any non-empty value other than `0`, `false`, or `no` (case-insensitive).

## Controls

By default, only a `28x28 px` handle in the bottom-right corner accepts pointer input while the rest of the image passes clicks through to windows underneath (`KeyboardInteractivity::None`). The handle dims to a faint resting pill after 2 seconds of inactivity (`~15%` opacity) and brightens on hover or drag.

Pointer controls:
- **Left-drag**: Move the overlay.
- **Right-drag**: Resize anchored from the top-left corner toward the bottom-right.
- **Scroll wheel**: Scale up or down continuously.
- **Triple-click (left button, 400 ms / 4 px)**: Toggle edit mode on the corner handle (or anywhere on the overlay while editing), expanding pointer input to the full overlay with a 1 px dashed outline and enabling keyboard shortcuts (`KeyboardInteractivity::OnDemand`).
- **Middle-click on handle**: Quit cleanly (passive mode restricts pointer input to the handle; edit mode restricts middle-click quit strictly to the handle bounds).

Edit-mode keyboard shortcuts:
- `Esc` (or triple-click): Exit edit mode back to passive click-through playback.
- `q` / `Q`: Quit cleanly.
- Arrow keys or `h` / `j` / `k` / `l`: Nudge position by `12 px`.
- `+` / `=` / `_` / `-` (or keypad `+` / `-`): Step scale up or down.
- `r` / `R`: Rotate 90 degrees clockwise or counter-clockwise.
- `[` / `]`: Rotate 5 degrees counter-clockwise or clockwise (repeatable when held).
- `0`: Reset rotation back to `0` degrees upright.
- `f` / `F`: Flip horizontally or vertically.
- `Space`: Pause or resume animation playback.

Signals:
- **`SIGINT` / `SIGTERM`**: Flush pending debounced config writes, abort active background decoders cleanly, flush logs, and exit with status `0`.

## Configuration, Cache, and Logs

- **`$XDG_CONFIG_HOME/apng-overlay.json` (default `~/.config/apng-overlay.json`)**: Stores `x`, `y`, `scale`, `rotation` (degrees in `[0.0, 360.0)`, using zero-copy compositor transforms on exact 90-degree multiples and smooth bilinear CPU resampling at fractional angles), and `flipped` (`true` or `false`) keyed by lexically normalized absolute file path (`. ` / `..` collapsed without dereferencing symlinks). Uses `500 ms` debounced saves serialized with an exclusive `flock` on `<config>.lock` and atomic temp-file replacement so concurrent instances preserve one another's entries.
- **`$XDG_CACHE_HOME/apng-overlay/` (default `~/.cache/apng-overlay/`)**: Stores headerless `lz4_flex`-compressed premultiplied `ARGB8888` chunks (`.lraw`) and sidecar `.json` metadata written strictly after `.lraw`. Frame 0 displays immediately while remaining frames stream in the background. Active cache entries refresh timestamps on hits, entries older than 30 days (`.json`, `.raw`, `.zraw`, `.lraw`) are pruned automatically at startup, and two independent RAM tiers control playback caching:
  - Source LRU (`-r` / `-c ram`, default `128 MiB`): Keeps decoded full-frame buffers in RAM for zero-copy streaming when the entire unpacked loop fits budget, switching dynamically to positional `.lraw` disk chunk streaming on larger files.
  - Display Cache (`-c display`, default `64 MiB`): Stores rendered fractional rotations and Lanczos3-downscaled oversized quarter-turn frames (`DisplayFrameCache` paired with compositor `set_buffer_transform`) at physical display resolution, dropping steady-state CPU from continuous LZ4 decompression (`~32%` core) to lightweight SHM `memcpy` (`< 1%` core) while bypassing caching cleanly on large overlays.
- **`$XDG_CACHE_HOME/apng-overlay/logs/` (default `~/.cache/apng-overlay/logs/`)**: Daily rotating log files (`apng-overlay.*.log`, retaining up to 7 files).

Architecture internals, wire formats, and CPU/memory benchmarks are documented in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/decisions/001-display-cache-and-resampling.md](docs/decisions/001-display-cache-and-resampling.md), [docs/CHANGELOG.md](docs/CHANGELOG.md), and [docs/CODING_STYLE.md](docs/CODING_STYLE.md).

## Structure and Tests

Rust crate modules under `src/`:
- `src/main.rs` & `src/cli.rs`: Signal masking setup, `clap` CLI validation, and runtime entry point.
- `src/logging.rs`: Dual-output `tracing` setup with panic flush hook.
- `src/error.rs`, `src/atomic_fs.rs`, & `src/xdg.rs`: Typed `thiserror` errors, RAII atomic temp-file writers, and shared XDG directory lookup.
- `src/config.rs`: `flock`-serialized JSON config persistence and per-field corruption recovery.
- `src/layout.rs` & `src/playback.rs`: Pure monitor-clamped geometry calculations and drift/stall-resilient frame clock.
- `src/decode/`: Streaming APNG, GIF, animated WebP, and static decoding (`codecs.rs`, `pixel.rs`, `stream.rs`).
- `src/cache/`: Deterministic `.lraw`/`.json` disk storage and 30-day pruning (`disk.rs`), configurable RAM LRU store (`memory.rs`), warm chunk reader (`reader.rs`), decode writer (`sink.rs`), and `FrameCache` facade (`store.rs`).
- `src/wayland/`: `calloop` event-loop bootstrap (`app.rs`), runtime timer/state coordination (`runtime.rs`), Wayland protocol dispatch (`handlers.rs`), layer-shell/subsurface scene (`scene.rs`), display-resolution frame cache (`display_cache.rs`), corner handle and dashed border chrome (`chrome.rs`), `SlotPool` frame ring (`shm_ring.rs`), monitor lookup (`monitor.rs`), pointer state (`input.rs`), and triple-click/shortcut mapping (`shortcuts.rs`).

Run formatting, linting, Rust integration tests (`tests/`), and API doc checks:

```bash
cargo +nightly fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo rustdoc --lib -- -D warnings
```

The original Python + GTK 4 implementation and its test suite remain archived under `python_scripts/` for reference and comparison:

```bash
python3 -B -m unittest discover -s python_scripts/tests -v
```
