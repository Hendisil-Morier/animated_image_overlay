from collections import OrderedDict, deque
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
import hashlib
import json
import os
import tempfile
import threading
import time
import traceback
import zlib

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, GLib

from config import atomic_write_bytes, atomic_write_json, log

# Directory storing compressed frame payloads (.zraw) and metadata (.json).
CACHE_DIR = os.path.expanduser("~/.cache/apng-overlay")

# Maximum cached Gdk.MemoryTexture entries for short animations before capping at 2.
TEXTURE_CACHE_LIMIT = 32


def _write_cache_meta(meta_path, width, height, delays, sizes):
    """Atomically write frame dimensions, per-frame delays, and chunk byte sizes."""
    atomic_write_json(
        meta_path,
        {
            "w": width,
            "h": height,
            "delays": delays,
            "sizes": sizes,
        },
    )


def make_texture(width, height, data):
    """Wrap raw RGBA bytes in a Gdk.MemoryTexture."""
    return Gdk.MemoryTexture.new(
        width, height, Gdk.MemoryFormat.R8G8B8A8, GLib.Bytes.new(data), width * 4
    )


def frame_delay(frame):
    """Extract frame duration in milliseconds, clamped to a 10 ms minimum."""
    return max(int(round(frame.info.get("duration", 33))), 10)


def encode_rgba(rgba, target_w):
    """Downscale an RGBA Pillow frame when wider than target_w and zlib-compress it."""
    from PIL import Image

    target_h = round(rgba.height * target_w / rgba.width)
    if rgba.width != target_w:
        rgba = rgba.resize((target_w, target_h), Image.LANCZOS, reducing_gap=2.0)
    raw = rgba.tobytes()
    return target_h, raw, zlib.compress(raw, level=1)


def cache_paths(path, width, limit):
    """Return (.zraw, .json) cache paths keyed by file path, mtime, size, width, and limit."""
    stat = os.stat(path)
    digest = hashlib.sha1(
        f"{os.path.abspath(path)}{stat.st_mtime_ns}{stat.st_size}{width}{limit}".encode()
    ).hexdigest()[:16]
    os.makedirs(CACHE_DIR, exist_ok=True)
    return (
        os.path.join(CACHE_DIR, f"{digest}.zraw"),
        os.path.join(CACHE_DIR, f"{digest}.json"),
    )


def prune_cache(directory, max_age_days=30):
    """Remove stale cache files, deleting .json markers before data files."""
    cutoff = time.time() - max_age_days * 86400
    try:
        names = sorted(os.listdir(directory))  # .json sorts before .raw and .zraw
    except OSError:
        return
    for name in names:
        if not name.endswith((".json", ".raw", ".zraw")):
            continue
        path = os.path.join(directory, name)
        try:
            if os.path.getmtime(path) < cutoff:
                os.remove(path)
        except OSError:
            pass


class FrameStore:
    """Loads, decodes, compresses, and caches animation frames on demand."""

    def __init__(self, path, base_width, max_frames):
        self.path = path
        self.base_width = base_width
        self.max_frames = max_frames

        self.fw = 0
        self.fh = 0
        self.compressed_frames = []
        self.delays = []
        self.texture_cache = OrderedDict()
        self.decode_done = False

        self.stop_event = threading.Event()
        self.worker_thread = None

    def prepare_first_frame(self):
        """Load frame 0 immediately (from cache or source) and stream remaining frames."""
        raw_path, meta_path = cache_paths(self.path, self.base_width, self.max_frames)
        cache_dir = os.path.dirname(raw_path)
        if self._try_load_cached_first_frame(raw_path, meta_path):
            prune_cache(cache_dir)
            return

        from PIL import Image, ImageSequence

        im = Image.open(self.path)
        target_w = min(self.base_width, im.width)  # never upscale small files
        total = getattr(im, "n_frames", 1)
        limit = min(total, self.max_frames) if self.max_frames else total
        log(f"image {im.size}, {total} frames, decoding {limit}, storing at width {target_w}")

        it = ImageSequence.Iterator(im)
        first = next(it)
        first_delay = frame_delay(first)
        rgba = first if first.mode == "RGBA" else first.convert("RGBA")
        target_h, first_raw, first_compressed = encode_rgba(rgba, target_w)

        self.fw, self.fh = target_w, target_h
        self._append_frame(first_compressed, first_delay, raw_data=first_raw)

        if limit <= 1:
            im.close()
            atomic_write_bytes(raw_path, first_compressed)
            # Written last so an interrupted run never leaves a usable cache.
            _write_cache_meta(
                meta_path,
                target_w,
                target_h,
                [first_delay],
                [len(first_compressed)],
            )
            self.decode_done = True
            prune_cache(cache_dir)
            return

        prune_cache(cache_dir)
        self.worker_thread = threading.Thread(
            target=self._decode_remaining_frames,
            args=(im, target_w, limit, raw_path, meta_path, first_compressed, first_delay),
            daemon=True,
        )
        self.worker_thread.start()

    def get_texture(self, index):
        """Return a Gdk.MemoryTexture for frame index, using a bounded LRU cache."""
        tex = self.texture_cache.get(index)
        if tex is not None:
            self.texture_cache.move_to_end(index)
            return tex
        raw = zlib.decompress(self.compressed_frames[index])
        tex = make_texture(self.fw, self.fh, raw)
        self.texture_cache[index] = tex
        limit = 2 if len(self.compressed_frames) > TEXTURE_CACHE_LIMIT else TEXTURE_CACHE_LIMIT
        while len(self.texture_cache) > limit:
            self.texture_cache.popitem(last=False)
        return tex

    def stop(self):
        """Signal any background worker to abort and wait briefly for cleanup."""
        self.stop_event.set()
        if self.worker_thread is not None and self.worker_thread.is_alive():
            self.worker_thread.join(timeout=1.0)

    def _try_load_cached_first_frame(self, raw_path, meta_path):
        if not (os.path.exists(meta_path) and os.path.exists(raw_path)):
            return False
        try:
            with open(meta_path) as f:
                info = json.load(f)
            fw = int(info["w"])
            fh = int(info["h"])
            delays = info["delays"]
            sizes = info["sizes"]
            if fw <= 0 or fh <= 0 or not delays or len(delays) != len(sizes):
                raise ValueError("invalid cache metadata")

            with open(raw_path, "rb") as f:
                first_compressed = f.read(sizes[0])
            first_raw = zlib.decompress(first_compressed)
            if len(first_raw) != fw * fh * 4:
                raise ValueError("corrupt cached frame size")
        except (OSError, ValueError, KeyError, IndexError, TypeError, zlib.error) as exc:
            log(f"ignoring corrupt cache: {exc}")
            return False

        for cache_file in (meta_path, raw_path):
            try:
                os.utime(cache_file)  # mark as recently used
            except OSError:
                pass

        self.fw, self.fh = fw, fh
        log(f"cache hit: {len(delays)} frames of {fw}x{fh}")
        self._append_frame(first_compressed, delays[0], raw_data=first_raw)

        if len(delays) <= 1:
            self.decode_done = True
        else:
            self.worker_thread = threading.Thread(
                target=self._stream_cached_frames,
                args=(raw_path, delays, sizes),
                daemon=True,
            )
            self.worker_thread.start()
        return True

    def _append_frame(self, compressed, delay, raw_data=None):
        idx = len(self.compressed_frames)
        self.compressed_frames.append(compressed)
        self.delays.append(delay)
        if idx == 0:
            raw = raw_data if raw_data is not None else zlib.decompress(compressed)
            self.texture_cache[0] = make_texture(self.fw, self.fh, raw)

    def _on_batch_ready(self, batch, done, label="decoding done"):
        if self.stop_event.is_set():
            return GLib.SOURCE_REMOVE
        for compressed, delay in batch:
            self._append_frame(compressed, delay)
        if done:
            self.decode_done = True
            log(f"{label}: {len(self.compressed_frames)} frames of {self.fw}x{self.fh}")
        return GLib.SOURCE_REMOVE

    def _stream_cached_frames(self, raw_path, delays, sizes):
        try:
            batch = []
            with open(raw_path, "rb") as f:
                f.seek(sizes[0])
                for size, delay in zip(sizes[1:], delays[1:]):
                    if self.stop_event.is_set():
                        return
                    chunk = f.read(size)
                    if len(chunk) != size:
                        raise EOFError("truncated cached frame")
                    batch.append((chunk, delay))
                    if len(batch) >= 16:
                        GLib.idle_add(self._on_batch_ready, batch, False, "cache loaded")
                        batch = []
            GLib.idle_add(self._on_batch_ready, batch, True, "cache loaded")
        except Exception:
            traceback.print_exc()
            GLib.idle_add(self._on_batch_ready, [], True, "cache loaded")

    def _decode_remaining_frames(
        self, im, target_w, limit, raw_path, meta_path, first_compressed, first_delay
    ):
        from PIL import ImageSequence

        cache_dir = os.path.dirname(raw_path)
        fd, tmp_raw = tempfile.mkstemp(prefix=".tmp-", suffix=".zraw", dir=cache_dir)
        os.close(fd)

        all_delays = [first_delay]
        all_sizes = [len(first_compressed)]
        target_h = self.fh

        try:
            pending = deque()
            workers = min(6, os.cpu_count() or 4)
            pool_ctx = (
                ThreadPoolExecutor(max_workers=workers)
                if im.width > target_w
                else nullcontext()
            )
            with im, open(tmp_raw, "wb") as out, pool_ctx as pool:
                out.write(first_compressed)
                batch = []

                def emit(idx, h, compressed):
                    nonlocal target_h, batch
                    target_h = h
                    out.write(compressed)
                    all_sizes.append(len(compressed))
                    batch.append((compressed, all_delays[idx]))
                    if len(batch) >= 4:
                        GLib.idle_add(self._on_batch_ready, batch, False, "decoding done")
                        batch = []
                    if idx % 30 == 0:
                        log(f"decoded {idx + 1}/{limit}")

                for i, frame in enumerate(ImageSequence.Iterator(im)):
                    if i == 0:
                        continue
                    if i >= limit or self.stop_event.is_set():
                        break
                    all_delays.append(frame_delay(frame))
                    if pool is None:
                        rgba = frame if frame.mode == "RGBA" else frame.convert("RGBA")
                        h, _raw, compressed = encode_rgba(rgba, target_w)
                        emit(i, h, compressed)
                    else:
                        # Copy RGBA buffers before ImageSequence mutates the shared frame.
                        rgba = frame.copy() if frame.mode == "RGBA" else frame.convert("RGBA")
                        pending.append((i, pool.submit(encode_rgba, rgba, target_w)))
                        if len(pending) >= workers:
                            idx, fut = pending.popleft()
                            h, _raw, compressed = fut.result()
                            emit(idx, h, compressed)

                while pending and not self.stop_event.is_set():
                    idx, fut = pending.popleft()
                    h, _raw, compressed = fut.result()
                    emit(idx, h, compressed)

            if self.stop_event.is_set():
                return

            os.replace(tmp_raw, raw_path)
            # Written last so an interrupted run never leaves a usable cache.
            _write_cache_meta(
                meta_path,
                target_w,
                target_h,
                all_delays,
                all_sizes,
            )
            GLib.idle_add(self._on_batch_ready, batch, True, "decoding done")
        except Exception:
            traceback.print_exc()
            GLib.idle_add(self._on_batch_ready, [], True, "decoding done")
        finally:
            if os.path.exists(tmp_raw):
                os.unlink(tmp_raw)
