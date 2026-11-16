//! Multi-format streaming frame decoder converting images to Wayland `ARGB8888`.

// Streaming APNG chunk splitter, independent frame decoder, and sequential compositor.
pub mod apng;

// Container format detection and frame iterators across APNG, GIF, WebP, and still images.
pub mod codecs;

// Bounded, order-preserving parallel encode pool behind sequential decoding.
mod pipeline;

// Pixel buffer validation, straight RGBA -> premultiplied `ARGB8888`, and Lanczos scaling.
pub mod pixel;

// Synchronous frame-0 extraction and background worker streaming.
pub mod stream;

// Frame payload records, buffer validation, and premultiplied pixel conversion helpers.
pub use pixel::{
    BYTES_PER_PIXEL, DecodedFirstFrame, EncodedFrame, checked_argb_byte_len,
    resize_premultiplied_argb8888, rotate_argb8888_into, straight_rgba_to_premultiplied_argb8888,
};
// Streaming decoder entry point and frame sink trait.
pub use stream::{FrameDecoder, FrameStreamSink};
