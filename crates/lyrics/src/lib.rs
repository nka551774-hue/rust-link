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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
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

/// Bounds what the cache holds in bytes as well, because a count alone says
/// nothing about size: one answer can be megabytes of lines.
const CACHE_BYTES: usize = 8 << 20;

/// An answer larger than this is returned but not kept, so that one track with
/// an enormous lyric cannot push everything else out of the cache.
const MAX_ENTRY_BYTES: usize = 1 << 20;

/// Asks the configured providers in order and remembers what they said.
pub struct Manager {
    providers: Vec<Box<dyn Provider>>,
    ttl: Duration,
    limit: usize,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Arc<str>, Entry>,
    order: VecDeque<Arc<str>>,
    /// The sum of every entry's weight.
    bytes: usize,
}

struct Entry {
    lyrics: Option<Arc<Lyrics>>,
    at: Instant,
    /// What this entry counts against [`CACHE_BYTES`], key included.
    weight: usize,
}

fn object_weight(object: &Object) -> usize {
    object.iter().map(|(k, v)| k.len() + value_weight(v) + 32).fold(0usize, usize::saturating_add)
}

fn value_weight(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(items) => {
            items.iter().map(value_weight).fold(items.len() * 16, usize::saturating_add)
        }
        serde_json::Value::Object(map) => object_weight(map),
        _ => 16,
    }
}

/// What an answer costs the cache: its text, its lines and the key it is kept
/// under, with a little for the structure around them.
fn weight(key: &str, lyrics: Option<&Lyrics>) -> usize {
    let Some(lyrics) = lyrics else { return key.len() + 64 };
    let lines: usize = lyrics
        .lines
        .iter()
        .flatten()
        .map(|line| line.line.len() + object_weight(&line.plugin) + std::mem::size_of::<Line>())
        .fold(0usize, usize::saturating_add);
    key.len()
        .saturating_add(lyrics.source_name.len())
        .saturating_add(lyrics.provider.len())
        .saturating_add(lyrics.text.as_ref().map_or(0, String::len))
        .saturating_add(object_weight(&lyrics.plugin))
        .saturating_add(lines)
        .saturating_add(64)
}

/// What a lookup depends on. The providers search by title, author and length,
/// so the key has to say all of them: a key of only the source and identifier
/// would let a track token that names a real video but carries other metadata
/// leave its lyrics in the cache for everyone who asks about the real one.
fn cache_key(track: &Track) -> String {
    let info = &track.metadata;
    format!(
        "{}:{}{}:{}{}:{}{}:{}{}",
        info.source_name.len(),
        info.source_name,
        info.identifier.len(),
        info.identifier,
        info.title.len(),
        info.title,
        info.author.len(),
        info.author,
        info.length,
    )
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
        let youtube_agent = config.youtube_native.then(|| agent.clone());
        Self::from_config_with_youtube(config, agent, youtube_agent)
    }

    /// Builds the manager using a dedicated (potentially proxied) HTTP client
    /// for YouTube Music when `youtubeNative` is enabled.
    pub fn from_config_with_youtube(
        config: &Config,
        agent: ureq::Agent,
        youtube_agent: Option<ureq::Agent>,
    ) -> Option<Self> {
        if !config.lyrics_enabled() {
            return None;
        }
        let mut providers: Vec<Box<dyn Provider>> =
            vec![Box::new(LRCLIB { agent, base: LRCLIB_BASE.to_string() })];
        if config.youtube_native {
            if let Some(yt_agent) = youtube_agent {
                providers.push(Box::new(YTMusic {
                    agent: yt_agent,
                    endpoint: config.youtube_music_endpoint.clone(),
                    // `youtubeClientVersion` configures the Android player client,
                    // not WEB_REMIX; keep YTMusic's built-in web client version.
                    version: String::new(),
                    visitor_data: config.youtube_visitor_data.clone(),
                }));
            }
        }
        Some(Manager::new(providers))
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
        let shared = self.fetch_shared(track)?;
        Ok(Arc::unwrap_or_clone(shared))
    }

    /// Returns shared cached lyrics without deep-cloning the lines vector on hits.
    pub fn fetch_shared(&self, track: &Track) -> Result<Arc<Lyrics>, LyricsError> {
        if !self.enabled() {
            return Err(LyricsError::NotFound);
        }
        let key = cache_key(track);
        if let Some(cached) = self.cached(&key) {
            return cached.ok_or(LyricsError::NotFound);
        }
        let mut last: Option<LyricsError> = None;
        for provider in &self.providers {
            match provider.fetch(track) {
                Ok(found) => {
                    let shared = Arc::new(found);
                    self.remember(&key, Some(Arc::clone(&shared)));
                    return Ok(shared);
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

    fn cached(&self, key: &str) -> Option<Option<Arc<Lyrics>>> {
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let entry = cache.entries.get(key)?;
        if entry.at.elapsed() > self.ttl {
            if let Some(expired) = cache.entries.remove(key) {
                cache.bytes = cache.bytes.saturating_sub(expired.weight);
            }
            cache.order.retain(|held| held.as_ref() != key);
            return None;
        }
        Some(entry.lyrics.clone())
    }

    /// Stores a result, including a miss, so a track nobody has lyrics for is
    /// not looked up again on every request.
    ///
    /// Entries leave in the order they arrived, which is enough to keep the map
    /// bounded.
    fn remember(&self, key: &str, lyrics: Option<Arc<Lyrics>>) {
        let weight = weight(key, lyrics.as_deref());
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        // An answer this large is not worth what it would push out.
        if weight > MAX_ENTRY_BYTES {
            if let Some(stale) = cache.entries.remove(key) {
                cache.bytes = cache.bytes.saturating_sub(stale.weight);
                cache.order.retain(|held| held.as_ref() != key);
            }
            return;
        }
        let shared_key = if let Some((existing_key, previous)) = cache.entries.remove_entry(key) {
            cache.bytes = cache.bytes.saturating_sub(previous.weight);
            cache.order.retain(|held| held.as_ref() != key);
            existing_key
        } else {
            Arc::from(key)
        };
        // Oldest first, until both the count and the bytes are within bounds.
        while cache.entries.len() >= self.limit || cache.bytes + weight > CACHE_BYTES {
            let Some(oldest) = cache.order.pop_front() else { break };
            if let Some(evicted) = cache.entries.remove(&oldest) {
                cache.bytes = cache.bytes.saturating_sub(evicted.weight);
            }
        }
        cache.order.push_back(Arc::clone(&shared_key));
        cache.bytes += weight;
        cache.entries.insert(shared_key, Entry { lyrics, at: Instant::now(), weight });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn titled(identifier: &str, title: &str) -> Track {
        let mut track = track("youtube", identifier);
        track.metadata.title = title.into();
        track
    }

    fn lyrics_of(bytes: usize) -> Result<Lyrics, LyricsError> {
        let mut lyrics = found("big");
        lyrics.text = Some("x".repeat(bytes));
        Ok(lyrics)
    }

    #[test]
    fn a_track_with_other_metadata_is_not_served_the_lyrics_of_the_real_one() {
        let (provider, calls) = scripted("db", || Ok(found("db")));
        let manager = Manager::new(vec![provider]);
        manager.fetch(&titled("abc", "the real song")).unwrap();
        manager.fetch(&titled("abc", "the real song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1, "the same track is answered from the cache");
        // A hand-made token with the same video ID and another title is a
        // different question, so it is asked rather than answered from the cache.
        manager.fetch(&titled("abc", "something forged")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        // And the length and the author are part of the question too.
        let mut longer = titled("abc", "the real song");
        longer.metadata.length = 99_000;
        manager.fetch(&longer).unwrap();
        let mut other = titled("abc", "the real song");
        other.metadata.author = "another".into();
        manager.fetch(&other).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn the_cache_is_bounded_in_bytes_and_not_only_in_entries() {
        let (provider, calls) = scripted("db", || lyrics_of(600 << 10));
        let manager = Manager::new(vec![provider]);
        for number in 0..40 {
            manager.fetch(&titled(&format!("track{number}"), "song")).unwrap();
        }
        let held = manager.cache.lock().unwrap().bytes;
        assert!(held <= CACHE_BYTES, "{held} bytes held against a budget of {CACHE_BYTES}");
        assert!(manager.cache.lock().unwrap().entries.len() < 40, "the oldest were pushed out");
        let before = calls.load(Ordering::Relaxed);
        manager.fetch(&titled("track0", "song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1, "the first track was evicted");
        manager.fetch(&titled("track39", "song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1, "the latest is still cached");
    }

    #[test]
    fn an_answer_too_large_to_keep_is_returned_but_not_cached() {
        let (provider, calls) = scripted("db", || lyrics_of(2 << 20));
        let manager = Manager::new(vec![provider]);
        assert!(manager.fetch(&titled("huge", "song")).is_ok());
        assert!(manager.fetch(&titled("huge", "song")).is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 2, "an enormous answer is asked for again");
        let cache = manager.cache.lock().unwrap();
        assert_eq!((cache.bytes, cache.entries.len()), (0, 0));
    }

    #[test]
    fn byte_accounting_survives_replacement_and_expiry() {
        let (provider, _) = scripted("db", || lyrics_of(1000));
        let mut manager = Manager::new(vec![provider]);
        manager.ttl = Duration::from_millis(10);
        let track = titled("abc", "song");
        manager.fetch(&track).unwrap();
        let once = manager.cache.lock().unwrap().bytes;
        assert!(once > 1000);
        std::thread::sleep(Duration::from_millis(30));
        manager.fetch(&track).unwrap();
        assert_eq!(
            manager.cache.lock().unwrap().bytes,
            once,
            "an expired entry is not counted twice"
        );
    }

    #[test]
    fn a_manager_with_no_provider_has_nothing() {
        let manager = Manager::new(Vec::new());
        assert!(!manager.enabled());
        assert!(matches!(manager.fetch(&track("youtube", "abc")), Err(LyricsError::NotFound)));
    }
}
