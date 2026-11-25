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
use std::net::Ipv6Addr;
use std::sync::{Arc, Condvar, Mutex};
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
#[derive(Debug, Clone, thiserror::Error)]
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

pub(crate) fn validate_base_url(raw: &str, label: &str) -> Result<String, LyricsError> {
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty()
        || !base.bytes().all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'\\' | b'?' | b'#'))
    {
        return Err(LyricsError::Failed(format!("lyrics: invalid {label} endpoint URL")));
    }
    let after_scheme = base
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &base[7..])
        .or_else(|| {
            base.get(..8).filter(|p| p.eq_ignore_ascii_case("https://")).map(|_| &base[8..])
        })
        .ok_or_else(|| LyricsError::Failed(format!("lyrics: invalid {label} endpoint URL")))?;
    let authority = after_scheme.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(LyricsError::Failed(format!("lyrics: invalid {label} endpoint URL")));
    }
    let valid_host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']').is_some_and(|(ipv6, tail)| {
            ipv6.parse::<Ipv6Addr>().is_ok()
                && match tail.strip_prefix(':') {
                    None => tail.is_empty(),
                    Some(port) => port.parse::<u16>().is_ok_and(|p| p > 0),
                }
        })
    } else {
        match authority.split_once(':') {
            None => !authority.contains(['[', ']']),
            Some((host, port)) => {
                !host.is_empty()
                    && !host.contains(['[', ']', ':'])
                    && port.parse::<u16>().is_ok_and(|p| p > 0)
            }
        }
    };
    if !valid_host || base.parse::<ureq::http::Uri>().is_err() {
        return Err(LyricsError::Failed(format!("lyrics: invalid {label} endpoint URL")));
    }
    Ok(base.to_owned())
}

pub(crate) fn valid_header_value(value: &str) -> bool {
    value.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && ureq::http::HeaderValue::try_from(value).is_ok()
}

type Outcome = Result<Arc<Lyrics>, LyricsError>;

#[derive(Default)]
struct Flight {
    outcome: Mutex<Option<Outcome>>,
    ready: Condvar,
}

impl Flight {
    fn wait(&self) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut held = self.outcome.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(outcome) = held.as_ref() {
                return outcome.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LyricsError::Failed("lyrics: timed out waiting for lookup".into()));
            }
            held = self
                .ready
                .wait_timeout(held, remaining)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }

    fn publish(&self, outcome: Outcome) {
        let mut held = self.outcome.lock().unwrap_or_else(|error| error.into_inner());
        if held.is_none() {
            *held = Some(outcome);
            drop(held);
            self.ready.notify_all();
        }
    }
}

struct Leader<'a> {
    flight: Arc<Flight>,
    key: &'a str,
    inflight: &'a Mutex<HashMap<String, Arc<Flight>>>,
    done: bool,
}

impl<'a> Leader<'a> {
    fn new(
        flight: Arc<Flight>,
        key: &'a str,
        inflight: &'a Mutex<HashMap<String, Arc<Flight>>>,
    ) -> Self {
        Self { flight, key, inflight, done: false }
    }

    fn finish(mut self, outcome: Outcome) -> Outcome {
        self.flight.publish(outcome.clone());
        self.done = true;
        outcome
    }
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.flight.publish(Err(LyricsError::Failed("lyrics: lookup was interrupted".into())));
        }
        let mut inflight = self.inflight.lock().unwrap_or_else(|error| error.into_inner());
        if inflight.get(self.key).is_some_and(|flight| Arc::ptr_eq(flight, &self.flight)) {
            inflight.remove(self.key);
        }
    }
}

/// Asks the configured providers in order and remembers what they said.
pub struct Manager {
    providers: Vec<Box<dyn Provider>>,
    ttl: Duration,
    limit: usize,
    cache: Mutex<Cache>,
    inflight: Mutex<HashMap<String, Arc<Flight>>>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Arc<str>, usize>,
    slots: Vec<Option<Entry>>,
    vacant: Vec<usize>,
    first: Option<usize>,
    last: Option<usize>,
    /// The sum of every entry's weight.
    bytes: usize,
}

struct Entry {
    key: Arc<str>,
    lyrics: Option<Arc<Lyrics>>,
    at: Instant,
    /// What this entry counts against [`CACHE_BYTES`], key included.
    weight: usize,
    previous: Option<usize>,
    next: Option<usize>,
}

impl Cache {
    fn get(&mut self, key: &str, ttl: Duration) -> Option<Option<Arc<Lyrics>>> {
        let index = *self.entries.get(key)?;
        let entry = self.slots[index].as_ref().expect("indexed entry");
        if entry.at.elapsed() > ttl {
            self.remove(index);
            return None;
        }
        let lyrics = entry.lyrics.clone();
        if self.last != Some(index) {
            self.detach(index);
            self.append(index);
        }
        Some(lyrics)
    }

    fn put(&mut self, key: &str, lyrics: Option<Arc<Lyrics>>, limit: usize) {
        let entry_weight = weight(key, lyrics.as_deref());
        let reused_key = self.entries.remove(key).map(|index| self.take_slot(index).key);
        if limit == 0 || entry_weight > MAX_ENTRY_BYTES {
            return;
        }
        while self.entries.len() >= limit || self.bytes.saturating_add(entry_weight) > CACHE_BYTES {
            let Some(oldest) = self.first else {
                return;
            };
            self.remove(oldest);
        }
        let index = self.vacant.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        let shared_key = reused_key.unwrap_or_else(|| Arc::from(key));
        self.entries.insert(Arc::clone(&shared_key), index);
        self.slots[index] = Some(Entry {
            key: shared_key,
            lyrics,
            at: Instant::now(),
            weight: entry_weight,
            previous: None,
            next: None,
        });
        self.bytes = self.bytes.saturating_add(entry_weight);
        self.append(index);
    }

    fn detach(&mut self, index: usize) {
        let entry = self.slots[index].as_ref().expect("indexed entry");
        let (previous, next) = (entry.previous, entry.next);
        if let Some(previous) = previous {
            self.slots[previous].as_mut().expect("linked entry").next = next;
        } else {
            self.first = next;
        }
        if let Some(next) = next {
            self.slots[next].as_mut().expect("linked entry").previous = previous;
        } else {
            self.last = previous;
        }
    }

    fn append(&mut self, index: usize) {
        let entry = self.slots[index].as_mut().expect("indexed entry");
        entry.previous = self.last;
        entry.next = None;
        if let Some(last) = self.last {
            self.slots[last].as_mut().expect("linked entry").next = Some(index);
        } else {
            self.first = Some(index);
        }
        self.last = Some(index);
    }

    fn take_slot(&mut self, index: usize) -> Entry {
        self.detach(index);
        let entry = self.slots[index].take().expect("indexed entry");
        self.bytes = self.bytes.saturating_sub(entry.weight);
        self.vacant.push(index);
        entry
    }

    fn remove(&mut self, index: usize) {
        let entry = self.take_slot(index);
        self.entries.remove(&entry.key);
    }
}

fn object_weight(object: &Object) -> usize {
    const NODE_OVERHEAD: usize =
        size_of::<String>() + size_of::<serde_json::Value>() + size_of::<[usize; 8]>();
    object.iter().fold(0usize, |acc, (k, v)| {
        acc.saturating_add(k.capacity())
            .saturating_add(NODE_OVERHEAD)
            .saturating_add(value_weight(v))
    })
}

fn value_weight(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.capacity(),
        serde_json::Value::Array(items) => items
            .capacity()
            .saturating_mul(size_of::<serde_json::Value>())
            .saturating_add(items.iter().map(value_weight).fold(0usize, usize::saturating_add)),
        serde_json::Value::Object(map) => object_weight(map),
        _ => 0,
    }
}

/// What an answer costs the cache: its text, its lines, container capacities
/// and the key it is kept under.
fn weight(key: &str, lyrics: Option<&Lyrics>) -> usize {
    let base = key.len().saturating_add(size_of::<Entry>()).saturating_add(4 * size_of::<usize>());
    let Some(lyrics) = lyrics else {
        return base;
    };
    let lines = lyrics.lines.as_ref().map_or(0usize, |vec| {
        vec.capacity().saturating_mul(size_of::<Line>()).saturating_add(vec.iter().fold(
            0usize,
            |acc, line| {
                acc.saturating_add(line.line.capacity()).saturating_add(object_weight(&line.plugin))
            },
        ))
    });
    base.saturating_add(size_of::<Lyrics>())
        .saturating_add(lyrics.source_name.capacity())
        .saturating_add(lyrics.provider.capacity())
        .saturating_add(lyrics.text.as_ref().map_or(0, String::capacity))
        .saturating_add(object_weight(&lyrics.plugin))
        .saturating_add(lines)
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
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Builds the manager the configuration asks for, or `None` when lyrics are
    /// switched off.
    ///
    /// The database answers most tracks and the catalogue answers the ones it
    /// has nothing for, which is where timed lines come from.
    pub fn from_config(config: &Config, agent: ureq::Agent) -> Option<Self> {
        let youtube_agent = (config.youtube_native && config.youtube_proxy.trim().is_empty())
            .then(|| agent.clone());
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
        let flight = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(flight) = inflight.get(&key) {
                let flight = Arc::clone(flight);
                drop(inflight);
                return flight.wait();
            }
            if let Some(cached) = self.cached(&key) {
                return cached.ok_or(LyricsError::NotFound);
            }
            let flight = Arc::new(Flight::default());
            inflight.insert(key.clone(), Arc::clone(&flight));
            flight
        };
        let leader = Leader::new(flight, &key, &self.inflight);
        let outcome = (|| {
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
            // A provider that failed is not the same as one that has nothing.
            // Report the failure rather than caching it as a miss.
            if let Some(error) = last {
                return Err(error);
            }
            self.remember(&key, None);
            Err(LyricsError::NotFound)
        })();
        leader.finish(outcome)
    }

    fn cached(&self, key: &str) -> Option<Option<Arc<Lyrics>>> {
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        cache.get(key, self.ttl)
    }

    /// Stores a result, including a miss, so a track nobody has lyrics for is
    /// not looked up again on every request.
    fn remember(&self, key: &str, lyrics: Option<Arc<Lyrics>>) {
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        cache.put(key, lyrics, self.limit);
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

    #[test]
    fn singleflight_shares_concurrent_lookups_and_recovers_after_panic() {
        struct Blocking {
            calls: Arc<AtomicUsize>,
            started: Mutex<Option<std::sync::mpsc::Sender<()>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl Provider for Blocking {
            fn name(&self) -> &'static str {
                "blocking"
            }
            fn fetch(&self, _track: &Track) -> Result<Lyrics, LyricsError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                if let Some(tx) = self.started.lock().unwrap().take() {
                    tx.send(()).unwrap();
                }
                self.release.lock().unwrap().recv().unwrap();
                Ok(found("blocking"))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let manager = Manager::new(vec![Box::new(Blocking {
            calls: Arc::clone(&calls),
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
        })]);
        let item = track("youtube", "coalesced");
        std::thread::scope(|scope| {
            let leader = scope.spawn(|| manager.fetch_shared(&item));
            started_rx.recv().unwrap();
            let waiters: Vec<_> =
                (0..6).map(|_| scope.spawn(|| manager.fetch_shared(&item))).collect();
            let deadline = Instant::now() + Duration::from_secs(5);
            while manager
                .inflight
                .lock()
                .unwrap()
                .values()
                .next()
                .is_none_or(|f| Arc::strong_count(f) < waiters.len() + 2)
            {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            release_tx.send(()).unwrap();
            let first = leader.join().unwrap().unwrap();
            for waiter in waiters {
                assert!(Arc::ptr_eq(&first, &waiter.join().unwrap().unwrap()));
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(manager.inflight.lock().unwrap().is_empty());

        struct PanicsOnce(AtomicUsize);
        impl Provider for PanicsOnce {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn fetch(&self, _track: &Track) -> Result<Lyrics, LyricsError> {
                if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    panic!("provider crashed");
                }
                Ok(found("recovered"))
            }
        }
        let panic_mgr = Manager::new(vec![Box::new(PanicsOnce(AtomicUsize::new(0)))]);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = panic_mgr.fetch_shared(&item);
        }));
        assert!(unwound.is_err());
        assert!(panic_mgr.inflight.lock().unwrap().is_empty());
        assert_eq!(panic_mgr.fetch(&item).unwrap().provider, "recovered");
    }

    #[test]
    fn lru_cache_promotes_recent_hits_and_charges_excess_vector_capacity() {
        let (provider, calls) = scripted("db", || Ok(found("db")));
        let mut manager = Manager::new(vec![provider]);
        manager.limit = 2;
        let a = titled("a", "song");
        let b = titled("b", "song");
        let c = titled("c", "song");
        manager.fetch(&a).unwrap();
        manager.fetch(&b).unwrap();
        // Touch `a` so `b` becomes least-recently-used.
        manager.fetch(&a).unwrap();
        manager.fetch(&c).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        manager.fetch(&a).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3, "promoted entry remained cached");
        manager.fetch(&b).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 4, "least-recently-used entry was evicted");

        let mut oversized_cap = found("cap");
        oversized_cap.lines = Some(Vec::with_capacity(MAX_ENTRY_BYTES / size_of::<Line>() + 16));
        assert!(weight("k", Some(&oversized_cap)) > MAX_ENTRY_BYTES);
    }

    #[test]
    fn from_config_omits_unproxied_youtube_when_youtube_proxy_is_configured() {
        let mut proxied_cfg = Config::default();
        proxied_cfg.youtube_native = true;
        proxied_cfg.youtube_proxy = "socks5://127.0.0.1:40000".into();
        let mgr = Manager::from_config(&proxied_cfg, ureq::Agent::new_with_defaults()).unwrap();
        assert_eq!(mgr.providers.len(), 1);
        assert_eq!(mgr.providers[0].name(), "lrclib");

        let mut direct_cfg = Config::default();
        direct_cfg.youtube_native = true;
        let direct_mgr =
            Manager::from_config(&direct_cfg, ureq::Agent::new_with_defaults()).unwrap();
        assert_eq!(direct_mgr.providers.len(), 2);
    }
}
