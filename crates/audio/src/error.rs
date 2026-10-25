//! The errors the audio pipeline reports.

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
    /// The track stopped because the connection carrying it failed. The
    /// pipeline has no opinion about voice, but a track that cannot be sent
    /// anywhere has ended badly, and the reason has to reach the client.
    #[error("{0}")]
    Transport(String),
}

impl AudioError {
    /// Whether the caller should reopen the track and try FFmpeg instead.
    ///
    /// The in-process pipeline reads before it can reject, so a decline is only
    /// recoverable if the body can be fetched again.
    pub fn needs_ffmpeg(&self) -> bool {
        matches!(self, AudioError::NeedsFfmpeg(_) | AudioError::FfmpegMissing)
    }
}
