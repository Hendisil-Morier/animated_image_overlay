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
./target/release/animated_image_widget FILE [-n MAX_FRAMES] [-w BASE_WIDTH] [-m MONITOR] [-s SCALE] [-r RAM_CACHE_MB] [-e] [-d]
```

Options:
- `FILE`: Path to the image or animation file.
- `-n`, `--max-frames`: Maximum number of frames to decode (`0` decodes all frames).
- `-w`, `--base-width`: Width in pixels used for stored cache frames (default: `500`; never upscales smaller files).
- `-m`, `--monitor`: Target monitor index or connector name such as `DP-1` (default: `0`).
- `-s`, `--scale`: Initial display scale, overriding any saved scale for that file.
- `-r`, `--ram-cache-mb`: RAM limit in MiB for keeping short animations unpacked in memory; larger files read one frame at a time from disk (`0` always reads from disk; default: `128`).
- `-e`, `--edit`: Launch directly into interactive edit mode.
- `-d`, `--debug`: Print debug logs to the terminal alongside the daily log files in `~/.cache/apng-overlay/logs/` (same as setting `OVERLAY_DEBUG=1`).

Pass `-d` / `--debug` (or set `OVERLAY_DEBUG=1`) to mirror debug diagnostics to stdout alongside the rolling daily log files in `~/.cache/apng-overlay/logs/`.

## Controls

By default, only a `28x28 px` handle in the bottom-right corner accepts pointer input while the rest of the image passes clicks through to windows underneath (`KeyboardInteractivity::None`). The handle fades out after 2 seconds of inactivity and reappears on hover or drag.

Pointer controls:
- **Left-drag**: Move the overlay.
- **Right-drag**: Resize anchored from the top-left corner toward the bottom-right.
- **Scroll wheel**: Scale up or down continuously.
- **Triple-click (left button, 400 ms / 4 px)**: Toggle edit mode on the corner handle (or anywhere on the overlay while editing), expanding pointer input to the full overlay with a 1 px dashed outline and enabling keyboard shortcuts (`KeyboardInteractivity::OnDemand`).
- **Middle-click on handle**: Quit cleanly.

Edit-mode keyboard shortcuts:
- `Esc` (or triple-click): Exit edit mode back to passive click-through playback.
- `q`: Quit cleanly.
- Arrow keys or `h` / `j` / `k` / `l`: Nudge position by `10 px`.
- `+` / `=` / `-`: Step scale up or down.
- `r` / `R`: Rotate 90 degrees clockwise or counter-clockwise.
- `f` / `F`: Flip horizontally or vertically.
- `Space`: Pause or resume animation playback.

Signals:
- **`SIGINT` / `SIGTERM`**: Flush pending debounced config writes, abort active background decoders cleanly, flush logs, and exit with status `0`.

## Configuration, Cache, and Logs

- `~/.config/apng-overlay.json`: Stores `x`, `y`, `scale`, `rotation` (`0`, `90`, `180`, or `270`), and `flipped` (`true` or `false`) per canonical absolute file path using 500 ms debounced writes guarded by an exclusive `flock` on `~/.config/apng-overlay.json.lock` and atomic temp-file replacement.
- `~/.cache/apng-overlay/`: Stores `lz4_flex`-compressed premultiplied `ARGB8888` chunks (`.lraw`) and `.json` metadata written strictly after `.lraw`. Frame 0 displays immediately while remaining frames stream in the background. Active cache entries refresh timestamps on hits, entries older than 30 days (`.json`, `.raw`, `.zraw`, `.lraw`) are pruned automatically at startup, and uncompressed frames remain cached in RAM up to a 128 MiB LRU budget.
- `~/.cache/apng-overlay/logs/`: Daily rotating log files (`apng-overlay.*.log`, retaining up to 7 files).

## Structure and Tests

Rust crate modules under `src/`:
- `src/main.rs` & `src/cli.rs`: Signal masking setup, `clap` CLI validation, and runtime entry point.
- `src/logging.rs`: Dual-output `tracing` setup with panic flush hook.
- `src/error.rs` & `src/atomic_fs.rs`: Typed `thiserror` errors and RAII atomic temp-file writers.
- `src/config.rs`: `flock`-serialized JSON config persistence and corruption recovery.
- `src/layout.rs` & `src/playback.rs`: Pure monitor-clamped geometry calculations and drift/stall-resilient frame clock.
- `src/decode/`: Streaming APNG, GIF, animated WebP, and static decoding (`codecs.rs`, `pixel.rs`, `stream.rs`).
- `src/cache/`: Deterministic `.lraw`/`.json` disk storage and 30-day pruning (`disk.rs`), 128 MiB RAM LRU store (`memory.rs`), warm chunk reader (`reader.rs`), decode writer (`sink.rs`), and `FrameCache` facade (`store.rs`).
- `src/wayland/`: `calloop` event-loop bootstrap (`app.rs`), runtime timer/state coordination (`runtime.rs`), Wayland protocol dispatch (`handlers.rs`), layer-shell/subsurface scene (`scene.rs`), corner handle and dashed border chrome (`chrome.rs`), `SlotPool` frame ring (`shm_ring.rs`), monitor lookup (`monitor.rs`), pointer state (`input.rs`), and triple-click/shortcut mapping (`shortcuts.rs`).

Run formatting, linting, and Rust integration tests (all under `tests/`):

```bash
cargo +nightly fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The original Python + GTK 4 implementation and its test suite remain archived under `python_scripts/` for reference and comparison:

```bash
python3 -B -m unittest discover -s python_scripts/tests -v
```
