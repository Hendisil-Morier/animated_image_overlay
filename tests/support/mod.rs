//! Shared temporary directories, recording sinks, and synthetic APNG builders for decode integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use animated_image_widget::{
    decode::{DecodedFirstFrame, EncodedFrame, FrameStreamSink},
    error::CacheError,
};

/// Temporary directory cleaned up automatically when dropped.
pub struct TempDecodeDir
{
    path: PathBuf,
}

impl TempDecodeDir
{
    pub fn new(name: &str) -> Self
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aiw_dec_{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("temp decode dir created");
        Self { path }
    }

    pub fn path(&self) -> &Path
    {
        &self.path
    }
}

impl Drop for TempDecodeDir
{
    fn drop(&mut self)
    {
        let _ = fs::remove_dir_all(&self.path);
    }
}

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
