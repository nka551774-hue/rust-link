//! Errors reported by the audio pipeline.

use crate::filters::FilterError;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Decode(String),
    #[error("this track needs FFmpeg, which is not installed")]
    FfmpegMissing,
    #[error("the audio decoder produced no complete frame for 10 seconds")]
    Stuck,
    #[error("{0}")]
    Filter(#[from] FilterError),
    #[error("{0}")]
    Opus(#[from] crate::opus::OpusError),
    #[error("the in-process pipeline declined the track: {0}")]
    NeedsFfmpeg(String),
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("{0}")]
    Transport(String),
}

impl AudioError {
    /// Whether the caller should reopen the track and retry with FFmpeg.
    #[inline]
    pub fn needs_ffmpeg(&self) -> bool {
        matches!(self, Self::NeedsFfmpeg(_) | Self::FfmpegMissing)
    }
}
