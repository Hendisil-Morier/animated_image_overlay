# Systems Programming Style, Error Handling, and Architectural Conventions

All codebase components must adhere to the following standards:

## 1. Allman Bracket Placement
Opening and closing braces must reside on their own separate lines across all function bodies, control flow structures, and type declarations.

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

## 4. Comprehensive Function and Symbol Documentation
Every function, method, struct, enum, and public constant must carry descriptive documentation explaining its operational purpose, invariants, and return values.

For `unsafe` operations, include a dedicated `// SAFETY:` block detailing the exact preconditions and invariants that prevent undefined behavior.

## 5. Error Handling Architecture

### 5.1. Internal Subsystems: Strongly Typed Domain Errors
Internal subsystems use strongly typed error enums powered by `thiserror`, propagating failures via standard `Result<T, E>`.

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError
{
    #[error("Host environment lacks required capability: {0}")]
    MissingCapability(&'static str),

    #[error("Failed to map resource buffer at address 0x{0:x}")]
    ResourceMappingFailed(usize),
}
```

### 5.2. ABI and FFI Boundary Error Translation
Routines exposed across external C ABI, foreign function interfaces, or guest execution boundaries must never panic. Internal Rust results are translated systematically into deterministic platform error codes.

```rust
pub fn translate_boundary_error(result: Result<(), EngineError>) -> i32
{
    match result
    {
        Ok(()) => 0,
        Err(EngineError::MissingCapability(_)) => -1,
        Err(EngineError::ResourceMappingFailed(_)) => -2,
    }
}
```

### 5.3. Hardware and Transient Resource Recovery
Distinguish between transient recoverable state and unrecoverable hardware loss. Transient failures (such as invalidated device swapchains or broken network pipes) trigger clean resource recreation, while unrecoverable faults halt with diagnostic dumps.

```rust
pub fn handle_device_result(status: Result<(), DeviceError>) -> Result<(), EngineError>
{
    match status
    {
        Ok(()) => Ok(()),
        Err(DeviceError::SuboptimalState) =>
        {
            recreate_device_resources();
            Ok(())
        }
        Err(err) => Err(EngineError::from(err)),
    }
}
```

## 6. Diagnostics and Logging Architecture

### 6.1. Dual-Output Logging Strategy
Diagnostic events are broadcast concurrently to both standard console streams and persistent disk log files with timestamps.

```rust
pub fn init_disk_logging(logs_dir: &std::path::Path) -> Result<tracing_appender::non_blocking::WorkerGuard, std::io::Error>
{
    std::fs::create_dir_all(logs_dir)?;
    let file_appender = tracing_appender::rolling::never(logs_dir, format!("{}.log", current_timestamp()));
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .init();

    Ok(guard)
}
```

### 6.2. Non-Blocking File Logging
Disk log writes utilize non-blocking ring buffers to decouple storage I/O latency from high-frequency simulation and execution threads.

### 6.3. Flush on Panic and Signal Abort
Fatal signals and unhandled panics trigger a synchronous flush of all log buffers to guarantee diagnostic traces remain intact on disk.

## 7. Line Wrapping and Method Chaining Conventions
Long lines of code must be broken into readable, multi-line segments. Long function calls, macro invocations, and error constructors place each argument on its own line.

```rust
return Err(EngineError::ResourceAllocationFailed(
    min_addr,
    max_addr,
    std::io::Error::last_os_error(),
));
```

Chained method calls place each stage on its own separate line with leading indentation.

```rust
let active_name = path
    .file_name()
    .and_then(|name| name.to_str())
    .map(str::to_ascii_lowercase);
```

## 8. File Size and Modular Decomposition
Source files must remain short, focused, and cohesive. Monolithic procedural code is split into small, domain-specific modules to reduce noise, control complexity, and improve readability.

This guideline targets procedural logic and control flow. Declarative registries, dispatch tables, and straightforward lookup dictionaries do not violate size guidelines and must not be artificially fragmented.

## 9. Struct Encapsulation, Statefulness, and Method Implementation (`impl`)
Prefer cohesive structs and `impl` blocks over loose, procedural free functions. Associated data, configuration state, and operations must be encapsulated within types.

Every struct must carry meaningful internal state and fields. Avoid empty dummy structs created solely to group static methods or act as namespaces.

```rust
pub struct DeviceCapabilities
{
    pub supports_compute: bool,
    pub max_workgroup_size: u32,
}

impl DeviceCapabilities
{
    pub fn query() -> Self
    {
        // Query hardware properties
        Self {
            supports_compute: true,
            max_workgroup_size: 1024,
        }
    }

    pub fn validate_requirements(&self) -> Result<(), EngineError>
    {
        // Check required capabilities
        Ok(())
    }
}
```

Types should expose clear constructors such as `new` or `with_capacity`. Extension traits should be used to provide domain-specific operations on primitive types.

## 10. Commit Message and Technical Changelog Strategy
Commit messages must follow conventional commits, remain focused and readable, and cross-reference persistent architectural documentation.

### 10.1. Commit Message Structure
* Use conventional commits (`feat(subsystem): concise summary`).
* Keep the commit body focused on a clear, descriptive summary of the changes.
* Cross-reference persistent engineering logs for architectural rationale, hardware contracts, and detailed verification logs.

### 10.2. Persistent Engineering Changelog
* Subsystem milestones, architectural bug fixes, and non-trivial features must be recorded in persistent engineering logs.
* Each entry documents the architectural rationale, underlying technical contracts, affected subsystems, and verification outcomes.

## 11. Code Reuse and DRY (Don't Repeat Yourself)
Do not duplicate logic, algorithms, constants, or data transformations across crates or modules. Common synchronization patterns, memory offsets, and boundary conversion helpers must be consolidated into reusable functions, traits, or modules.

## 12. Test Organization and Source Hygiene
Production source files under `src/` must remain completely free of test code.

All unit and integration tests must reside in dedicated test files under `tests/` directories. Never place `#[cfg(test)] mod tests` or `#[test]` attributes inside `src/` files.
