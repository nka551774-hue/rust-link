//! Native Rustlink lyrics, supplied by LRCLIB and YouTube Music.
//!
//! Providers are asked over the network, so every call in this crate blocks. The
//! caller runs one off the thread that serves a request, which keeps a slow
//! public service from stalling the node.

mod lrc;
mod lrclib;
mod ytmusic;

pub use lrclib::{LRCLIB, LRCLIB_BASE};
pub use ytmusic::{YTMusic, YTMUSIC_ENDPOINT};

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{Object, Track};
use serde::{Deserialize, Serialize};

/// One line of lyrics, timed from the start of the track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    #[serde(rename = "timestamp_ms")]
    pub timestamp: i64,
    /// How long the line stays on screen, which the provider does not always
    /// say.
    #[serde(default)]
    #[serde(rename = "duration_ms")]
    pub duration: Option<i64>,
    #[serde(rename = "text")]
    pub line: String,
    #[serde(default)]
    #[serde(rename = "extra")]
    pub plugin: Object,
}

/// What a provider returns for a track.
///
/// `source_name` is where the track came from and `provider` is who supplied the
/// words, which is the pair the plugin reports to a client. A track has either
/// timed lines or plain text; the database answers with one and the catalogue
/// with the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Lyrics {
    pub source_name: String,
    pub provider: String,
    pub text: Option<String>,
    pub lines: Option<Vec<Line>>,
    #[serde(default)]
    #[serde(rename = "extra")]
    pub plugin: Object,
}

impl Lyrics {
    pub fn new(source_name: impl Into<String>, provider: impl Into<String>) -> Self {
        Lyrics {
            source_name: source_name.into(),
            provider: provider.into(),
            text: None,
            lines: None,
            plugin: Object::new(),
        }
    }
}

/// What a provider can answer.
#[derive(Debug, thiserror::Error)]
pub enum LyricsError {
    /// The provider has no lyrics for this track. This is an answer rather than
    /// a failure, and the caller replies with no content.
    #[error("lyrics: no lyrics for this track")]
    NotFound,
    /// The lookup could not be made, which is different from a provider that has
    /// nothing: the client may want to ask again.
    #[error("{0}")]
    Failed(String),
}

/// Fetches the lyrics of one track.
pub trait Provider: Send + Sync {
    /// The provider name reported to the client.
    fn name(&self) -> &'static str;
    /// Returns the lyrics, or [`LyricsError::NotFound`] when there are none.
    fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError>;
}

/// How long a fetched result is reused.
///
/// Lyrics do not change, so the cache is there to keep a client that asks twice
/// from turning into two calls to a free public service, not to keep up with
/// anything.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Bounds the cache, so a node that runs for weeks does not hold every track it
/// ever played.
const CACHE_LIMIT: usize = 256;

/// Asks the configured providers in order and remembers what they said.
pub struct Manager {
    providers: Vec<Box<dyn Provider>>,
    ttl: Duration,
    limit: usize,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    order: Vec<String>,
}

struct Entry {
    lyrics: Option<Lyrics>,
    at: Instant,
}

impl Manager {
    /// Builds a manager over the providers, which are asked in the order given.
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Manager {
            providers,
            ttl: DEFAULT_TTL,
            limit: CACHE_LIMIT,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Builds the manager the configuration asks for, or `None` when lyrics are
    /// switched off.
    ///
    /// The database answers most tracks and the catalogue answers the ones it
    /// has nothing for, which is where timed lines come from.
    pub fn from_config(config: &Config, agent: ureq::Agent) -> Option<Self> {
        if !config.lyrics_enabled() {
            return None;
        }
        let catalogue = YTMusic {
            agent: agent.clone(),
            endpoint: config.youtube_music_endpoint.clone(),
            version: config.youtube_client_version.clone(),
            visitor_data: config.youtube_visitor_data.clone(),
        };
        Some(Manager::new(vec![
            Box::new(LRCLIB { agent: agent.clone(), base: LRCLIB_BASE.to_string() }),
            Box::new(catalogue),
        ]))
    }

    /// Whether any provider is configured, so a caller can answer without
    /// asking.
    pub fn enabled(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Returns the lyrics for a track, or [`LyricsError::NotFound`] when no
    /// provider has any.
    ///
    /// A provider that fails is passed over rather than reported straight away,
    /// because the next one may still answer; only when none of them do is the
    /// last failure raised.
    pub fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError> {
        if !self.enabled() {
            return Err(LyricsError::NotFound);
        }
        let key = format!("{}\u{0}{}", track.metadata.source_name, track.metadata.identifier);
        if let Some(cached) = self.cached(&key) {
            return cached.ok_or(LyricsError::NotFound);
        }
        let mut last: Option<LyricsError> = None;
        for provider in &self.providers {
            match provider.fetch(track) {
                Ok(found) => {
                    self.remember(&key, Some(found.clone()));
                    return Ok(found);
                }
                Err(LyricsError::NotFound) => {}
                Err(error) => last = Some(error),
            }
        }
        // A provider that failed is not the same as one that has nothing. The
        // failure may pass, so it is reported rather than remembered as a miss:
        // remembering it would answer every request for that track from the
        // cache for the rest of the hour, including the ones made after the
        // service came back.
        if let Some(error) = last {
            return Err(error);
        }
        self.remember(&key, None);
        Err(LyricsError::NotFound)
    }

    fn cached(&self, key: &str) -> Option<Option<Lyrics>> {
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let entry = cache.entries.get(key)?;
        if entry.at.elapsed() > self.ttl {
            cache.entries.remove(key);
            cache.order.retain(|held| held != key);
            return None;
        }
        Some(entry.lyrics.clone())
    }

    /// Stores a result, including a miss, so a track nobody has lyrics for is
    /// not looked up again on every request.
    ///
    /// Entries leave in the order they arrived, which is enough to keep the map
    /// bounded.
    fn remember(&self, key: &str, lyrics: Option<Lyrics>) {
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if !cache.entries.contains_key(key) {
            if cache.order.len() >= self.limit {
                let oldest = cache.order.remove(0);
                cache.entries.remove(&oldest);
            }
            cache.order.push(key.to_string());
        }
        cache.entries.insert(key.to_string(), Entry { lyrics, at: Instant::now() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn track(source: &str, identifier: &str) -> Track {
        Track::new(
            String::new(),
            TrackInfo {
                identifier: identifier.into(),
                source_name: source.into(),
                title: "a song".into(),
                author: "someone".into(),
                ..Default::default()
            },
        )
    }

    /// A provider that answers the same way every time and counts its asks, so a
    /// test can tell a cached answer from a fresh one.
    struct Scripted {
        name: &'static str,
        answer: fn() -> Result<Lyrics, LyricsError>,
        calls: Arc<AtomicUsize>,
    }

    impl Provider for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }
        fn fetch(&self, _track: &Track) -> Result<Lyrics, LyricsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            (self.answer)()
        }
    }

    fn scripted(
        name: &'static str,
        answer: fn() -> Result<Lyrics, LyricsError>,
    ) -> (Box<dyn Provider>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Box::new(Scripted { name, answer, calls: calls.clone() }), calls)
    }

    fn found(name: &str) -> Lyrics {
        Lyrics::new("youtube", name)
    }

    #[test]
    fn the_first_provider_with_an_answer_wins() {
        let manager = Manager::new(vec![
            scripted("empty", || Err(LyricsError::NotFound)).0,
            scripted("full", || Ok(found("full"))).0,
        ]);
        let answer = manager.fetch(&track("youtube", "abc")).unwrap();
        assert_eq!(answer.provider, "full");
    }

    #[test]
    fn a_failure_is_passed_over_for_a_provider_that_answers() {
        let manager = Manager::new(vec![
            scripted("broken", || Err(LyricsError::Failed("down".into()))).0,
            scripted("full", || Ok(found("full"))).0,
        ]);
        assert!(manager.fetch(&track("youtube", "abc")).is_ok());
    }

    #[test]
    fn a_failure_with_no_answer_after_it_is_reported() {
        let manager =
            Manager::new(vec![scripted("broken", || Err(LyricsError::Failed("down".into()))).0]);
        assert!(matches!(manager.fetch(&track("youtube", "abc")), Err(LyricsError::Failed(_))));
    }

    #[test]
    fn a_failure_is_not_remembered_so_the_next_ask_tries_again() {
        let (provider, calls) = scripted("broken", || Err(LyricsError::Failed("down".into())));
        let manager = Manager::new(vec![provider]);
        let _ = manager.fetch(&track("youtube", "abc"));
        let _ = manager.fetch(&track("youtube", "abc"));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_miss_is_remembered_so_it_is_only_looked_up_once() {
        let (provider, calls) = scripted("empty", || Err(LyricsError::NotFound));
        let manager = Manager::new(vec![provider]);
        assert!(manager.fetch(&track("youtube", "abc")).is_err());
        assert!(manager.fetch(&track("youtube", "abc")).is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_answer_is_reused_rather_than_asked_for_twice() {
        let (provider, calls) = scripted("full", || Ok(found("full")));
        let manager = Manager::new(vec![provider]);
        manager.fetch(&track("youtube", "abc")).unwrap();
        manager.fetch(&track("youtube", "abc")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn two_tracks_do_not_share_an_answer() {
        let (provider, calls) = scripted("full", || Ok(found("full")));
        let manager = Manager::new(vec![provider]);
        manager.fetch(&track("youtube", "abc")).unwrap();
        manager.fetch(&track("youtube", "xyz")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_manager_with_no_provider_has_nothing() {
        let manager = Manager::new(Vec::new());
        assert!(!manager.enabled());
        assert!(matches!(manager.fetch(&track("youtube", "abc")), Err(LyricsError::NotFound)));
    }
}
