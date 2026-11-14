# Animated Image Overlay

Transparent, click-through desktop widget for playing animated images (such as APNG or GIF) above all windows on Wayland using GTK 4 and `gtk4-layer-shell`.

## Requirements

- Python 3 with `PyGObject` (`gi`), `pycairo`, and `Pillow`
- `gtk4-layer-shell` (`libgtk4-layer-shell.so` or `.so.0`)
- A `wlroots`-compatible Wayland compositor (not supported on X11 or GNOME Shell)

## Usage

```bash
python overlay.py FILE [-n MAX_FRAMES] [-w BASE_WIDTH] [-m MONITOR] [-s SCALE]
```

Options:
- `FILE`: Path to the image or animation file.
- `-n`, `--max-frames`: Maximum number of frames to decode (`0` decodes all frames).
- `-w`, `--base-width`: Width in pixels used for stored cache frames (default: `500`; never upscales smaller files).
- `-m`, `--monitor`: Target monitor index or connector name such as `DP-1` (default: `0`).
- `-s`, `--scale`: Initial display scale, overriding any saved scale for that file.

Set `OVERLAY_DEBUG=1` to print timestamped startup, cache, layout, and save logs.

## Controls

The overlay keeps a 28 px handle in the bottom-right corner that fades out after 2 seconds of inactivity and returns on hover. Only the handle accepts pointer input by default, passing clicks on the rest of the image through to windows underneath.

- **Left-drag**: Move the overlay.
- **Right-drag**: Resize from the top-left corner toward the bottom-right.
- **Scroll wheel**: Scale up or down.
- **Middle-click on handle**: Quit cleanly.
- **`SIGUSR1`**: Toggle edit mode so the entire image accepts drag and scroll input (shown with a dashed outline).
- **`SIGINT` / `SIGTERM`**: Flush pending config saves, stop background decoding, and quit.

## Configuration and Cache

- `~/.config/apng-overlay.json`: Stores `x`, `y`, and `scale` per absolute file path using debounced (500 ms) atomic writes.
- `~/.cache/apng-overlay/`: Stores `zlib`-compressed frame data (`.zraw`) and metadata (`.json`). Frame 0 displays right away while remaining frames stream or decode in a background thread, and active cache entries refresh on use while stale files older than 30 days are pruned automatically.

## Structure and Tests

- `overlay.py`: CLI argument parsing and entry point.
- `config.py`: Debug logging and atomic JSON state persistence.
- `cache.py`: Pillow decoding, `.zraw` disk cache, 30-day pruning, and bounded LRU `Gdk.MemoryTexture` cache.
- `window.py`: `libgtk4-layer-shell` preload, Wayland overlay window, pointer/signal input, and `Gdk.FrameClock` playback.
- `tests/test_overlay.py`: Unit tests for CLI validation, config persistence, monitor resolution, clamping, cache fallback/pruning, and tick timing.

Run the unit tests with:

```bash
python3 -B -m unittest discover -s tests -v
```
