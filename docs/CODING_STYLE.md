# Systems Programming Style, Error Handling, and Architectural Conventions

All codebase components must adhere to the following standards:

## 1. Allman Bracket Placement and Formatting
Opening and closing braces must reside on their own separate lines across all function bodies, control flow structures, and type declarations. `rustfmt.toml` enforces `brace_style = "AlwaysNextLine"`, `control_brace_style = "AlwaysNextLine"`, `reorder_imports = true`, `imports_granularity = "Crate"`, and `group_imports = "StdExternalCrate"` via `cargo +nightly fmt --check`.

```rust
pub fn example_routine(value: u32) -> bool
{
    if value == 0
    {
        return false;
    }

    true
}
```

## 2. Indentation Depth Limit
Functions must maintain a maximum of four levels of indentation. Complex logic must be flattened through guard clauses, early returns, and dedicated helper functions.

## 3. Single-Responsibility Principle
Each function and module must own exactly one cohesive domain responsibility. Monolithic routines are broken down into small, modular units along domain boundaries.

## 4. Comprehensive Function, Module, and Crate Safety Lints
Every module, function, method, struct, enum, field, and constant must carry descriptive documentation explaining its operational purpose, invariants, and return values.

`Cargo.toml` enforces `[lints.rust]` `unsafe_code = "forbid"` and `missing_docs = "warn"`, while `src/lib.rs` and `src/main.rs` enforce `#![deny(clippy::unwrap_used, clippy::expect_used)]`. Verify clean API and private documentation with:

```bash
cargo rustdoc --lib -- -D warnings
cargo rustdoc --lib -- --document-private-items -W missing_docs
```

## 5. Error Handling Architecture

### 5.1. Internal Subsystems: Strongly Typed Domain Errors
Internal subsystems use strongly typed error enums powered by `thiserror` (`ConfigError`, `CacheError`, `WaylandError`, and top-level `OverlayError` in `src/error.rs`), propagating failures via standard `Result<T, E>` without `.unwrap()` or `.expect()`.

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError
{
    #[error("Unable to resolve cache directory from environment")]
    DirectoryUnavailable,

    #[error("Cache I/O failure at {path}: {source}")]
    Io
    {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}
```

### 5.2. CLI and Process Exit Boundaries
CLI validation (`CliArgs::parse_env`) separates help/version exits (`CliOutcome::EarlyExit` -> `ExitCode::SUCCESS`) from user input errors (`OverlayError::CliValidation` / `OverlayError::MissingInputFile` -> `ExitCode::FAILURE`). Runtime errors in `src/main.rs` log structured diagnostics, print concise user messages to stderr, flush log buffers cleanly via `LoggingGuard::flush`, and return `ExitCode::FAILURE` without panicking.

### 5.3. Recoverable vs Fatal Failure Modes
Distinguish between recoverable transient/cache faults and fatal startup requirements:

- **Recoverable faults:** Corrupt `.lraw`/`.json` caches log warnings in `try_open_cached_store` and fall back to decoding cleanly from source; invalid JSON entries in `ConfigStore::load_state_entry` fall back per field to defaults; transient frame upload or cache read failures schedule a `100 ms` `FRAME_ERROR_RETRY_MS` timer in `OverlayRuntime` rather than crashing active playback.
- **Fatal requirements:** Missing required Wayland globals (`zwlr_layer_shell_v1`, `wl_subcompositor`, `wl_shm`, `wp_viewporter`), unmatched `--monitor` targets, unreadable source files, or failed cache/config directory setup return contextual `OverlayError` values and exit cleanly.

## 6. Diagnostics and Logging Architecture

### 6.1. Dual-Output Daily Rotating Strategy
`src/logging.rs` builds a dual-layer `tracing_subscriber` combining an optional `DEBUG` stdout layer (active when `--debug` or a truthy `OVERLAY_DEBUG` setting is present) with an always-on ANSI-free daily rotating file layer under `$XDG_CACHE_HOME/apng-overlay/logs/` (`apng-overlay.*.log`, retaining up to `7` files).

```rust
let appender = RollingFileAppender::builder()
    .rotation(Rotation::DAILY)
    .filename_prefix("apng-overlay")
    .filename_suffix("log")
    .max_log_files(7)
    .build(logs_dir)?;
```

### 6.2. Non-Blocking File Logging
Disk log writes use `tracing_appender::non_blocking` so storage I/O latency never stalls `calloop` frame pacing or decoder worker threads.

### 6.3. Flush on Panic and Shutdown
Unhandled panics execute a chained panic hook (`install_panic_flush_hook`) that logs the fault location/payload and drops the shared `WorkerGuard`, and `src/main.rs` calls `log_guard.flush()` before returning so trailing diagnostics reach disk intact.

## 7. Line Wrapping and Method Chaining Conventions
Long lines of code must be broken into readable, multi-line segments cleanly formatted by nightly `rustfmt`. Long function signatures, macro invocations, and error constructors place arguments on separate lines when exceeding width limits.

Chained iterator and option/result adapters place each stage on its own separate line with leading indentation:

```rust
let active_name = path
    .file_name()
    .and_then(|name| name.to_str())
    .map(str::to_ascii_lowercase);
```

## 8. File Size and Modular Decomposition
Source files must remain short, focused, and cohesive. Monolithic procedural code is split into small, domain-specific modules (for example, `decode::{codecs, pixel, stream}` and `cache::{disk, memory, reader, sink, store}`).

This guideline targets procedural logic and control flow. Declarative registries, protocol dispatch tables, and lookup dictionaries (`map_edit_keysym`, `wl_transform_for_orientation`) do not violate size guidelines and must not be artificially fragmented.

## 9. Struct Encapsulation, Statefulness, and Method Implementation (`impl`)
Prefer cohesive structs and `impl` blocks (`OverlayLayout`, `InputState`, `PlaybackClock`, `DisplayFrameCache`, `ConfigStore`) over loose, procedural free functions. Associated data, configuration state, and operations must be encapsulated within types.

Every struct must carry meaningful internal state and fields. Avoid empty dummy structs created solely to group static methods or act as namespaces, and expose clear constructors (`new`, `with_*`, `open_*`).

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDescriptor
{
    pub index: usize,
    pub name: Option<String>,
    pub width: i32,
    pub height: i32,
}
```

## 10. Commit Message, Architecture Docs, and Changelog Strategy
Commit messages must follow conventional commits, remain focused and readable, and stay consistent with persistent project documentation.

### 10.1. Commit Message Structure
* Use conventional commits (`feat(subsystem): concise summary`, `fix(subsystem): ...`, `perf(subsystem): ...`, `docs: ...`).
* Keep the commit body focused on a clear, descriptive summary of the changes and verified test/benchmark results.
* Cross-reference `docs/ARCHITECTURE.md` and `docs/decisions/` for architectural rationale and wire/protocol contracts.

### 10.2. Persistent Engineering Documentation
* Milestone changes, bug fixes, and performance measurements belong in `docs/CHANGELOG.md` (`[Unreleased]` until tagged).
* Non-trivial architectural choices belong in numbered ADRs under `docs/decisions/`, and settled wire formats, schemas, and Wayland scene invariants live in `docs/ARCHITECTURE.md`.

## 11. Code Reuse and DRY (Don't Repeat Yourself)
Do not duplicate logic, algorithms, constants, or data transformations across crates or modules. Common synchronization patterns, memory offsets, and boundary conversion helpers must be consolidated into reusable functions, traits, or modules.

## 12. Test Organization and Source Hygiene
Production source files under `src/` must remain completely free of test code.

All unit and integration tests must reside in dedicated test files under `tests/` directories. Never place `#[cfg(test)] mod tests` or `#[test]` attributes inside `src/` files.
