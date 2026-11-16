//! Video container inspection via `ffprobe` and `ffmpeg` PAM/RGBA frame streaming.

// Subprocess `ffmpeg` PAM + `showinfo` frame iterator cleanly stopped on drop.
mod ffmpeg;

// Video stream probing, container magic matching, and short-vs-long cache policy.
mod probe;

// Re-export video probing, caching policy, and frame stream iterator cleanly inside decode.
pub(crate) use ffmpeg::{VideoFrameIterator, open_video_frames};
pub(crate) use probe::{
    DEFAULT_VIDEO_CACHE_BUDGET_BYTES, VideoProbe, has_video_signature, probe_video,
};
