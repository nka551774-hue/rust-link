//! Source loading and Opus frame production traits used by players and routes.

use std::sync::Arc;

use rustlink_audio::{AudioError, Body, Frames, Preference};
use rustlink_protocol::{Object, Track};
use rustlink_source::{LoadResult, SourceError};

/// Resolves track identifiers and opens track audio streams.
pub trait Loader: Send + Sync {
    fn load(&self, identifier: &str) -> Result<LoadResult, SourceError>;

    fn load_shared(&self, identifier: &str) -> Result<Arc<LoadResult>, SourceError> {
        self.load(identifier).map(Arc::new)
    }

    fn open(&self, track: &Track) -> Result<Box<dyn std::io::Read + Send>, SourceError>;

    fn set_youtube_tokens(
        &self,
        po_token: String,
        visitor_data: String,
    ) -> Result<(bool, bool), SourceError> {
        let (pot, vd) = (po_token.trim(), visitor_data.trim());
        Ok((!pot.is_empty(), !vd.is_empty() || (!pot.contains("://") && pot.contains(':'))))
    }
}

impl Loader for rustlink_source::Service {
    fn load(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        rustlink_source::Service::load(self, identifier)
    }

    fn load_shared(&self, identifier: &str) -> Result<Arc<LoadResult>, SourceError> {
        rustlink_source::Service::load_shared(self, identifier)
    }

    fn open(&self, track: &Track) -> Result<Box<dyn std::io::Read + Send>, SourceError> {
        rustlink_source::Service::open(self, track)
    }

    fn set_youtube_tokens(
        &self,
        po_token: String,
        visitor_data: String,
    ) -> Result<(bool, bool), SourceError> {
        rustlink_source::Service::set_youtube_tokens(self, po_token, visitor_data)
    }
}

/// Opens a track and produces paced Opus frames.
pub trait Engine: Send + Sync {
    fn start(
        &self,
        track: &Track,
        position_ms: i64,
        volume: i64,
        filters: &Object,
    ) -> Result<Box<dyn Frames>, AudioError>;
}

/// Pairs [`rustlink_audio::Engine`] with a [`Loader`] and falls back to FFmpeg
/// on a fresh stream when the in-process decoder declines a container/codec.
pub struct Playback {
    pub engine: rustlink_audio::Engine,
    pub source: Arc<dyn Loader>,
}

impl Engine for Playback {
    fn start(
        &self,
        track: &Track,
        position_ms: i64,
        volume: i64,
        filters: &Object,
    ) -> Result<Box<dyn Frames>, AudioError> {
        if !self.engine.in_process && self.engine.ffmpeg.is_none() {
            return Err(AudioError::FfmpegMissing);
        }
        match self.engine.start(
            track,
            self.open(track)?,
            position_ms,
            volume,
            filters,
            Preference::Auto,
        ) {
            Err(AudioError::NeedsFfmpeg(_)) => {
                if self.engine.ffmpeg.is_none() {
                    return Err(AudioError::FfmpegMissing);
                }
                self.engine.start(
                    track,
                    self.open(track)?,
                    position_ms,
                    volume,
                    filters,
                    Preference::Ffmpeg,
                )
            }
            other => other,
        }
    }
}

impl Playback {
    fn open(&self, track: &Track) -> Result<Body, AudioError> {
        self.source.open(track).map(Body::new).map_err(|e| AudioError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::{encode_track, TrackInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingLoader(AtomicUsize);

    impl Loader for CountingLoader {
        fn load(&self, _: &str) -> Result<LoadResult, SourceError> {
            Ok(LoadResult::Empty)
        }

        fn open(&self, _: &Track) -> Result<Box<dyn std::io::Read + Send>, SourceError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(std::io::empty()))
        }
    }

    #[test]
    fn missing_ffmpeg_without_in_process_audio_fails_before_opening_source() {
        let loader = Arc::new(CountingLoader(AtomicUsize::new(0)));
        let playback = Playback {
            engine: rustlink_audio::Engine {
                ffmpeg: None,
                in_process: false,
                bitrate: 192_000,
                complexity: 5,
            },
            source: loader.clone(),
        };
        let track = encode_track(&TrackInfo {
            identifier: "id".into(),
            source_name: "http".into(),
            ..Default::default()
        })
        .unwrap();
        let result = playback.start(&track, 0, 100, &Object::new());
        assert!(matches!(result, Err(AudioError::FfmpegMissing)));
        assert_eq!(loader.0.load(Ordering::SeqCst), 0);
    }
}
