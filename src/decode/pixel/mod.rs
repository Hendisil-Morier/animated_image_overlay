//! Frame pixel buffers, checked byte sizing, Lanczos3 downscaling, SWAR rotation, and LZ4 encoding.

// Frame conversion, Lanczos3 downscaling, checked sizing, and LZ4 compression.
pub mod frame;

// Fixed-point scanline-split 2-lane `u64` SWAR bilinear rotation.
pub mod rotate;

// Internal raw decoded RGBA frame carrier and frame encoding entry points.
// Public Wayland frame types, buffer size checks, premultiplication, and Lanczos3 resizer.
pub use frame::{
    BYTES_PER_PIXEL, DecodedFirstFrame, EncodedFrame, checked_argb_byte_len,
    resize_premultiplied_argb8888, straight_rgba_to_premultiplied_argb8888,
};
pub(crate) use frame::{RawRgbaFrame, encode_first_frame, encode_subsequent_frame};
// Public bilinear CPU rotation entry point for fractional overlay angles.
pub use rotate::rotate_argb8888_into;
