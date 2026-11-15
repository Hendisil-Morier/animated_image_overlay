# ADR-001: Display Frame Cache, Budgeted Quarter-Turn Downscaling, and SWAR Rotation

## Status

Accepted

## Date

2026-11-15

## Context

Profiling a live overlay running `animated_image_widget ~/Pictures/M_Animation.apng -w 5000` (`1492x2158`, `180` frames, `33 ms` frame interval = `~30.3 fps`) at thumbnail scale (`scale=0.033188`, `rotation=310.0`) exposed `32.4%` single-core CPU usage (`~2.0%` to `2.5%` across `16` logical cores, `96.2%` in user space). Kernel I/O counters reported `rchar=1.33 GB` served from the Linux page cache with zero physical disk reads (`read_bytes=0`), showing CPU bottlenecks strictly inside frame decompression and pixel resampling.

A `perf record -F 999` capture across `600` `cpu_core` and `19` `cpu_atom` samples pinpointed two hot paths:

1. **`decompress_chunk_into` (`73.62%` `cpu_core`, `74.28%` `cpu_atom`):** Unpacked, the animation spans `12.88 MB` per frame (`2.32 GB` total), exceeding the `128 MiB` source RAM LRU ceiling (`DEFAULT_RAM_CACHE_BUDGET_BYTES`). Every `33 ms` tick re-read compressed chunks (`493 KB` to `1.56 MB`) via `LrawReader` and decompressed `~390 MB/s` of `ARGB8888` pixels into scratch RAM.
2. **`rotate_argb8888_into` (`16.42%` `cpu_core`):** Even though physical output bounds at `scale=0.033` and `2x` compositor scaling measured only `~172x168 px`, every frame tick ran scalar bilinear resampling with per-pixel `x`/`y` bounds checks (`copy_to_iter` and slice indexing accounted for most of the remaining `~10%` samples).

Meanwhile, exact quarter turns (`0`, `90`, `180`, `270` degrees) on oversized animations bypassed `DisplayFrameCache` and streamed full `1492x2158` frames into a triple-buffered `ImageBufferRing` (`Pss_Shmem = 12,656 kB` and `19.33%` to `25.66%` single-core CPU), forcing CPU LZ4 decompression of `12.88 MB` per frame just to display a small scaled thumbnail.

## Decision

Introduce a two-tier memory model separating source cache limits from display cache limits, accelerate fractional rotation with scanline-split SWAR arithmetic, and extend display-resolution caching to oversized quarter-turn thumbnails via premultiplied Lanczos3 downscaling.

### 1. Independent `-c ram` and `-c display` Memory Budgets (`src/cli.rs`)

Add `-c` / `--cap` (`apply_cap_overrides`) accepting `ram=<MB>` and `display=<MB>` suboptions individually (`-c ram=10`, `-c display=32`) or comma-separated/repeated (`-c ram=10,display=32`), defaulting to `ram=128` and `display=64` (`DEFAULT_DISPLAY_CACHE_MB`), where `0` disables that cache tier. Existing `-r` / `--ram-cache-mb` remains supported cleanly alongside `-c ram`.

Separating these budgets resolves competing workload needs cleanly: small loops stay unpacked in source `-c ram` for zero-copy streaming at all angles; long high-resolution animations rendered as small overlays cache compact display frames inside `-c display`; full-screen overlays exceeding both budgets drop display caching and stream straight from `.lraw` while capping heap memory.

### 2. `DisplayFrameCache` and `BufferTargetSpec` Routing (`src/wayland/display_cache.rs`, `src/wayland/scene.rs`)

`DisplayFrameCache` lazily caches final display-resolution `ARGB8888` buffers indexed by frame slot when `would_fit(width, height, frame_count, decode_done)` passes (`decode_done && frame_count > 1 && budget_bytes > 0 && frame_count * width * height * 4 <= budget_bytes`). `OverlayScene::target_spec` inspects `FrameCache::cache_profile() -> (frame_count, decode_done, fits_source_ram)` and selects one of three operating paths:

| Animation Regime | Angle Mode | SHM Buffer Dimensions | `DisplayFrameCache` Usage | Pixel Path |
| :--- | :--- | :--- | :--- | :--- |
| Within `-c ram`, multi/single frame | Quarter turn (`0/90/180/270`) | Source `(frame_w, frame_h)` | Bypassed and cleared | Source RAM LRU copy + compositor `set_buffer_transform` |
| Oversized (`!fits_source_ram`), small scale within `-c display` | Quarter turn (`0/90/180/270`) | `quarter.orient_dimensions(ov_w, ov_h) * buf_scale` clamped to `(frame_w, frame_h)` | Active under canonical `(w, h, angle=0.0, flipped=false)` key | First-loop Lanczos3 downscale via `resize_premultiplied_argb8888` + compositor `set_buffer_transform`; steady-state SHM `memcpy` |
| Fits `-c display`, multi-frame, decode done | Fractional (`quarter_turn().is_none()`) | `overlay_size * buf_scale` clamped to `rotated_bounds` | Active under `(w, h, angle_bits, flipped)` key | First-loop SWAR rotation; steady-state SHM `memcpy` |
| Exceeds `-c display`, streaming (`!decode_done`), or single frame | Fractional or oversized quarter | Fractional physical size or source `(frame_w, frame_h)` | Bypassed and cleared | Per-tick `.lraw` chunk read + LZ4 decompress + direct SHM fill / compositor GPU scale |

Key design invariants for `target_spec` and `needs_buffer_resync`:

- **Canonical Quarter-Turn Key:** Quarter-turn downscales always key under `angle_bits: angle_key(0.0), flipped: false` while `OverlayScene::sync_layout_inner` applies `wl_surface::set_buffer_transform` for dihedral orientation. Rotating `0` -> `90` -> `180` -> `270` or toggling `f` / `F` reuses identical cached buffers across all eight transforms, with `quarter.orient_dimensions` converting logical screen dimensions back to unrotated buffer aspect ratios.
- **Double Gate on Quarter-Turn Downscaling:** Both `!fits_source_ram` and `self.display_cache.would_fit(...)` must hold before engaging Lanczos3. Animations already inside source RAM LRU stream faster via direct `memcpy`, and huge overlays exceeding `-c display` avoid running CPU Lanczos3 every tick by letting KWin sample raw source buffers on GPU cleanly.
- **Hysteresis on Ring Resizes:** `OverlayScene::needs_buffer_resync` triggers immediate SHM ring refreshes when target dimensions grow along either axis, when shrunk pixel area reaches `want_pixels * 3 <= cur_pixels * 2`, or upon compositor `scale_factor_changed`, while pointer resize drags still defer reallocation until release (`InputState::is_resizing`).

### 3. Premultiplied Lanczos3 Contract (`src/decode/pixel.rs`)

Decode-time downscaling (`downscale_if_needed` inside `encode_first_frame`) operates on straight RGBA before `straight_rgba_to_premultiplied_argb8888`, so it passes `use_alpha: true` (`ResizeOptions::mul_div_alpha = true`) via shared helper `resize_u8x4`. By contrast, `resize_premultiplied_argb8888` operates on cached premultiplied `[B, G, R, A]` pixels (`ResizeOptions::default().use_alpha(false)`).

Leaving `mul_div_alpha` enabled on premultiplied `ARGB8888` caused two defects measured empirically on `0`-degree runs: redundant premultiply/unpremultiply arithmetic introducing rounding loss on semi-transparent borders, plus a persistent `src_w * src_h * 4` byte (`12.88 MB`) `Resizer::alpha_buffer` retained inside `OverlayScene::resizer`. Switching `resize_premultiplied_argb8888` to `use_alpha(false)` performs exact linear Lanczos3 convolution across premultiplied channels and trimmed steady-state `0`-degree `Pss_Anon` by `12.25 MB` (`61,496 kB` -> `49,244 kB`).

### 4. Scanline-Split 2-Lane `u64` SWAR Rotation (`src/decode/pixel.rs`)

When `DisplayFrameCache` is warming up, disabled (`-c display=0`), streaming, or oversized, `rotate_argb8888_into` accelerates CPU rotation using two changes:

1. **Exact Interior Row Partitioning:** `linear_in_bounds` solves the fixed-point (`40.24`) affine inequalities `0 <= start_fp + i * step <= max_fp` in `i128` once per scanline (`xl..xr` intersected with `yl..yr`). Every destination row loops cleanly through outer apron `[..in_start]` (`write_apron_pixel`), bounds-guaranteed interior `[in_start..in_end]` (`write_interior_pixel_swar`), and outer apron `[in_end..]`, eliding per-pixel apron branches inside the interior loop.
2. **Carry-Free 12-Bit Dual-Channel SWAR:** Bilinear weights (`ROTATE_WEIGHT_BITS = 12`, `one = 4096`) keep maximum weighted sum at `255 * 4096^2 + 2^23 = 0xFF80_0000 < u32::MAX`. Packing alternating channels `[B, R]` and `[G, A]` into low and high 32-bit lanes of two `u64` registers inside `write_interior_pixel_swar` evaluates four bilinear samples in two `u64` accumulators completely free of inter-lane carry (`unsafe_code = "forbid"`).

## Alternatives Considered

- **Multi-resolution `.lraw` mipmaps stored on disk:** Rejected because duplicating scaled `.lraw` files multiplies disk footprint, changes on-disk cache serialization, and locks downscales to fixed discrete widths rather than adapting seamlessly to continuous Wayland fractional scale and pinch/scroll zoom.
- **GPU compositor shaders / Vulkan compute:** Rejected to keep the Wayland client self-contained over core `wl_shm` + `wp_viewporter` (`unsafe_code = "forbid"`) across lightweight compositors cleanly.
- **CPU Lanczos3 downscaling even when uncached (`!would_fit`):** Rejected because uncached large overlays would re-run Lanczos3 on every animation tick (`> 50%` core load) rather than handing full source buffers cleanly to compositor GPU sampling.

## Consequences and Verification

All workspace checks pass cleanly (`cargo +nightly fmt --check`, `cargo clippy --all-targets --offline -- -D warnings`, all `58` integration tests across `9` suites, `cargo doc --no-deps --offline`, and `cargo build --release --offline`). Live KWin runs on `M_Animation.apng -w 5000` (`1492x2158`, `180` frames) confirm:

| Scenario (`M_Animation.apng -w 5000`) | Core CPU | Host CPU (16 cores) | `Rss` | `Pss` | `Pss_Anon` | `Pss_Shmem` |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| Baseline PID `347530` (`310.0`, `scale=0.033`) | `32.4%` | `~2.0–2.5%` | `26,212 kB` | — | — | `192 kB` |
| SWAR only (`310.0`, `scale=0.033`, `-c display=0`) | `17.66%` | `1.10%` | `26,108 kB` | `23,112 kB` | `19,140 kB` | `192 kB` |
| Default `-c display=64` (`310.0`, `scale=0.033`) | `0.67%` | `0.04%` | `46,792 kB` | `43,818 kB` | `39,456 kB` | `192 kB` |
| Quarter-turn streaming (`0.0`, `scale=0.033`, `-c display=0`) | `19.33%` | `1.21%` | `51,124 kB` | `35,776 kB` | `19,144 kB` | `12,656 kB` |
| Downscaled quarter cache (`0.0`, `scale=0.033`, `-c display=64`) | `0.67%` | `0.04%` | `56,908 kB` | `53,950 kB` | `49,244 kB` | `238 kB` |
| Large overlay cap bypass (`0.0`, `scale=0.24`, `-c display=64`) | — | — | `50,928 kB` | `35,616 kB` | `19,116 kB` | `12,656 kB` |
| Large overlay cap bypass (`310.0`, `scale=0.24`, `-c display=64`) | — | — | `38,124 kB` | `29,315 kB` | `19,116 kB` | `6,026 kB` |

Tradeoffs remain reversible cleanly: small loops (`scale=0.033`) trade `~20.3 MiB` (`310.0`) or `~30.1 MiB` (`0.0`) of display-cache heap RAM inside the `64 MiB` `-c display` budget to cut steady-state CPU by `~48x`, and setting `-c display=0` restores the `~26 MiB` streaming RSS baseline immediately.
