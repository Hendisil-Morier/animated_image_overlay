//! Shared recording sinks and re-exported temp directory utilities for decode integration tests.

mod temp_dir;

use std::sync::{Arc, Mutex};

use animated_image_widget::{
    decode::{DecodedFirstFrame, EncodedFrame, FrameStreamSink},
    error::CacheError,
};
pub use temp_dir::TempDecodeDir;

/// Captured frames, delays, and terminal stream status observed by `RecordingSink`.
#[derive(Default)]
pub struct RecordedStream
{
    pub frames: Vec<Vec<u8>>,
    pub delays: Vec<u32>,
    pub delays_us: Vec<u32>,
    pub finished: bool,
    pub cancelled: bool,
    pub finish_err: Option<String>,
}

/// Thread-safe test `FrameStreamSink` appending emitted frames into `RecordedStream`.
pub struct RecordingSink
{
    pub state: Arc<Mutex<RecordedStream>>,
}

impl FrameStreamSink for RecordingSink
{
    fn on_first_frame(&mut self, frame: &DecodedFirstFrame) -> Result<(), CacheError>
    {
        let mut guard = self.state.lock().unwrap();
        guard.frames.push(frame.lz4_payload.clone());
        guard.delays.push(frame.delay_ms);
        guard.delays_us.push(frame.delay_us);
        Ok(())
    }

    fn on_frame(&mut self, frame: EncodedFrame) -> Result<(), CacheError>
    {
        let mut guard = self.state.lock().unwrap();
        guard.frames.push(frame.lz4_payload);
        guard.delays.push(frame.delay_ms);
        guard.delays_us.push(frame.delay_us);
        Ok(())
    }

    fn on_finish(self: Box<Self>, cancelled: bool, result: Result<(), CacheError>)
    {
        let mut guard = self.state.lock().unwrap();
        guard.cancelled = cancelled;
        guard.finished = true;
        guard.finish_err = result.err().map(|e| e.to_string());
    }
}
