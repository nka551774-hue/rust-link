//! What a player needs from the rest of the node.
//!
//! Both traits exist so a player can be exercised without a network or a
//! decoder: the node hands over its source service and its audio engine, and a
//! test hands over stand-ins that answer from memory.

use std::sync::Arc;

use rustlink_audio::{AudioError, Body, Frames, Preference};
use rustlink_protocol::{Object, Track};
use rustlink_source::{LoadResult, SourceError};

/// Resolving identifiers and opening the bytes of a track.
pub trait Loader: Send + Sync {
    /// Turns an identifier into a load result, the way `tracks/resolve` does.
    fn load(&self, identifier: &str) -> Result<LoadResult, SourceError>;
    /// Shared results avoid copying cached playlists before HTTP serialization.
    fn load_shared(&self, identifier: &str) -> Result<Arc<LoadResult>, SourceError> {
        self.load(identifier).map(Arc::new)
    }
    /// Opens the stream a track plays from.
    fn open(&self, track: &Track) -> Result<Box<dyn std::io::Read + Send>, SourceError>;
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
}

/// Producing the Opus frames of a track.
pub trait Engine: Send + Sync {
    /// Opens the track and starts decoding it, so the caller only paces what
    /// comes back.
    fn start(
        &self,
        track: &Track,
        position_ms: i64,
        volume: i64,
        filters: &Object,
    ) -> Result<Box<dyn Frames>, AudioError>;
}

/// The audio engine and the source it plays from, which is what an engine needs
/// to open a body before it can decode one.
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
        let body = self.open(track)?;
        match self.engine.start(track, body, position_ms, volume, filters, Preference::Auto) {
            Ok(frames) => Ok(frames),
            // The in-process pipeline reads the body before it can decline a
            // stream, so the fall back starts from a fresh one rather than a
            // half-read one.
            Err(AudioError::NeedsFfmpeg(_)) => {
                if self.engine.ffmpeg.is_none() {
                    return Err(AudioError::FfmpegMissing);
                }
                let body = self.open(track)?;
                self.engine.start(track, body, position_ms, volume, filters, Preference::Ffmpeg)
            }
            Err(error) => Err(error),
        }
    }
}

impl Playback {
    fn open(&self, track: &Track) -> Result<Body, AudioError> {
        self.source
            .open(track)
            .map(Body::new)
            .map_err(|error| AudioError::Decode(error.to_string()))
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
