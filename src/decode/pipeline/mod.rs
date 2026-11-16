//! Bounded, order-preserving parallel decode and encode pipelines for multi-frame animations.

// Shared synchronization coordinator, work queues, and bounded backpressure state.
mod coordinator;

// Scoped worker pools, sequential readers, ordered APNG compositor, and sink delivery.
mod stages;

// Configuration passed from `FrameDecoder` stream setup into scoped pipeline runners.
pub(crate) use coordinator::PipelineParams;
// Scoped multi-frame stream runners for sequential codecs and parallel APNG containers.
pub(crate) use stages::{stream_apng_remaining, stream_remaining_frames};
