//! Streaming APNG chunk splitter, standalone frame decoder, and sequential canvas compositor.
//!
//! Separates independent per-frame zlib decompression and scanline unfiltering from sequential
//! `DisposeOp`/`BlendOp` canvas compositing so worker pools can decode APNG frames in parallel.

// Ordered full-canvas compositing across APNG dispose and blend operations.
pub mod compositor;

// Parallel single-frame PNG synthesis and straight-RGBA patch decoding.
pub mod patch;

// Sequential PNG/APNG chunk scanner yielding per-frame compressed packets.
pub mod reader;

// Stateful APNG canvas compositor.
pub use compositor::ApngCompositor;
// Standalone per-frame PNG packet decoder and decoded patch payload.
pub use patch::{DecodedApngPatch, decode_apng_packet};
// Single-open APNG packet reader and container frame metadata records.
pub use reader::{ApngHeader, ApngPacketReader, FrameControl, RawApngPacket};
