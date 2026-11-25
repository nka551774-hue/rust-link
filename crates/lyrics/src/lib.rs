//! Native Rustlink lyrics supplied by LRCLIB and YouTube Music.

mod lrc;
mod lrclib;
mod ytmusic;

pub use lrclib::{LRCLIB, LRCLIB_BASE};
pub use ytmusic::{VisitorBridge, VisitorReader, VisitorWriter, YTMusic, YTMUSIC_ENDPOINT};

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{Object, Track};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    #[serde(rename = "timestamp_ms")]
    pub timestamp: i64,
    #[serde(default)]
    #[serde(rename = "duration_ms")]
    pub duration: Option<i64>,
    #[serde(rename = "text")]
    pub line: String,
    #[serde(default)]
    #[serde(rename = "extra")]
    pub plugin: Object,
}

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
        Self {
            source_name: source_name.into(),
            provider: provider.into(),
            text: None,
            lines: None,
            plugin: Object::new(),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum LyricsError {
    #[error("lyrics: no lyrics for this track")]
    NotFound,
    #[error("{0}")]
    Failed(String),
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError>;
}

pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
const CACHE_LIMIT: usize = 256;
const CACHE_BYTES: usize = 8 << 20;
const MAX_ENTRY_BYTES: usize = 1 << 20;

#[inline]
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn trim_owned(mut text: String) -> String {
    let trimmed_len = text.trim().len();
    if trimmed_len != text.len() {
        let leading = text.len() - text.trim_start().len();
        if leading > 0 {
            text.drain(..leading);
        }
        text.truncate(trimmed_len);
    }
    text
}

pub(crate) fn validate_base_url(raw: &str, label: &str) -> Result<String, LyricsError> {
    let bad = || LyricsError::Failed(format!("lyrics: invalid {label} endpoint URL"));
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty()
        || !base.bytes().all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'\\' | b'?' | b'#'))
    {
        return Err(bad());
    }
    let after = ["http://", "https://"]
        .into_iter()
        .find_map(|s| {
            base.get(..s.len()).filter(|p| p.eq_ignore_ascii_case(s)).map(|_| &base[s.len()..])
        })
        .ok_or_else(bad)?;
    let authority = after.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(bad());
    }
    let ok_port = |p: &str| p.parse::<u16>().is_ok_and(|n| n > 0);
    let valid_host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').is_some_and(|(v6, tail)| {
            v6.parse::<Ipv6Addr>().is_ok()
                && (tail.is_empty() || tail.strip_prefix(':').is_some_and(ok_port))
        }),
        None => match authority.split_once(':') {
            None => !authority.contains(['[', ']']),
            Some((h, p)) => !h.is_empty() && !h.contains(['[', ']', ':']) && ok_port(p),
        },
    };
    if !valid_host || base.parse::<ureq::http::Uri>().is_err() {
        return Err(bad());
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
        let mut held = lock(&self.outcome);
        loop {
            if let Some(outcome) = held.as_ref() {
                return outcome.clone();
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(LyricsError::Failed("lyrics: timed out waiting for lookup".into()));
            }
            held = self.ready.wait_timeout(held, rem).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn publish(&self, outcome: Outcome) {
        let mut held = lock(&self.outcome);
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
        let mut inflight = lock(self.inflight);
        if inflight.get(self.key).is_some_and(|f| Arc::ptr_eq(f, &self.flight)) {
            inflight.remove(self.key);
        }
    }
}

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
    bytes: usize,
}

struct Entry {
    key: Arc<str>,
    lyrics: Option<Arc<Lyrics>>,
    at: Instant,
    weight: usize,
    prev: Option<usize>,
    next: Option<usize>,
}

impl Cache {
    fn get(&mut self, key: &str, ttl: Duration) -> Option<Option<Arc<Lyrics>>> {
        let index = *self.entries.get(key)?;
        let entry = self.slots[index].as_ref()?;
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
        let reused = self.entries.remove(key).map(|idx| self.take_slot(idx).key);
        if limit == 0 || entry_weight > MAX_ENTRY_BYTES {
            return;
        }
        while self.entries.len() >= limit || self.bytes.saturating_add(entry_weight) > CACHE_BYTES {
            let Some(oldest) = self.first else { return };
            self.remove(oldest);
        }
        let index = self.vacant.pop().unwrap_or_else(|| {
            self.slots.push(None);
            self.slots.len() - 1
        });
        let shared_key = reused.unwrap_or_else(|| Arc::from(key));
        self.entries.insert(Arc::clone(&shared_key), index);
        self.slots[index] = Some(Entry {
            key: shared_key,
            lyrics,
            at: Instant::now(),
            weight: entry_weight,
            prev: None,
            next: None,
        });
        self.bytes = self.bytes.saturating_add(entry_weight);
        self.append(index);
    }

    fn detach(&mut self, index: usize) {
        let e = self.slots[index].as_ref().unwrap();
        let (prev, next) = (e.prev, e.next);
        match prev {
            Some(p) => self.slots[p].as_mut().unwrap().next = next,
            None => self.first = next,
        }
        match next {
            Some(n) => self.slots[n].as_mut().unwrap().prev = prev,
            None => self.last = prev,
        }
    }

    fn append(&mut self, index: usize) {
        let prev = self.last.replace(index);
        let e = self.slots[index].as_mut().unwrap();
        e.prev = prev;
        e.next = None;
        match prev {
            Some(p) => self.slots[p].as_mut().unwrap().next = Some(index),
            None => self.first = Some(index),
        }
    }

    fn take_slot(&mut self, index: usize) -> Entry {
        self.detach(index);
        let entry = self.slots[index].take().unwrap();
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
    const NODE: usize =
        size_of::<String>() + size_of::<serde_json::Value>() + size_of::<[usize; 8]>();
    object.iter().fold(0usize, |acc, (k, v)| {
        acc.saturating_add(k.capacity()).saturating_add(NODE).saturating_add(value_weight(v))
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

fn weight(key: &str, lyrics: Option<&Lyrics>) -> usize {
    let base = key.len().saturating_add(size_of::<Entry>()).saturating_add(4 * size_of::<usize>());
    let Some(lyrics) = lyrics else { return base };
    let lines = lyrics.lines.as_ref().map_or(0usize, |vec| {
        vec.capacity().saturating_mul(size_of::<Line>()).saturating_add(
            vec.iter().fold(0usize, |acc, l| {
                acc.saturating_add(l.line.capacity()).saturating_add(object_weight(&l.plugin))
            }),
        )
    });
    base.saturating_add(size_of::<Lyrics>())
        .saturating_add(lyrics.source_name.capacity())
        .saturating_add(lyrics.provider.capacity())
        .saturating_add(lyrics.text.as_ref().map_or(0, String::capacity))
        .saturating_add(object_weight(&lyrics.plugin))
        .saturating_add(lines)
}

fn cache_key(track: &Track) -> String {
    let m = &track.metadata;
    format!(
        "{}:{}{}:{}{}:{}{}:{}{}",
        m.source_name.len(),
        m.source_name,
        m.identifier.len(),
        m.identifier,
        m.title.len(),
        m.title,
        m.author.len(),
        m.author,
        m.length,
    )
}

impl Manager {
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Self {
            providers,
            ttl: DEFAULT_TTL,
            limit: CACHE_LIMIT,
            cache: Mutex::new(Cache::default()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_config(config: &Config, agent: ureq::Agent) -> Option<Self> {
        let youtube_agent = (config.youtube_native && config.youtube_proxy.trim().is_empty())
            .then(|| agent.clone());
        Self::from_config_with_youtube(config, agent, youtube_agent)
    }

    pub fn from_config_with_youtube(
        config: &Config,
        agent: ureq::Agent,
        youtube_agent: Option<ureq::Agent>,
    ) -> Option<Self> {
        Self::from_config_with_youtube_visitor(config, agent, youtube_agent, None)
    }

    pub fn from_config_with_youtube_visitor(
        config: &Config,
        agent: ureq::Agent,
        youtube_agent: Option<ureq::Agent>,
        visitor_bridge: Option<VisitorBridge>,
    ) -> Option<Self> {
        Self::from_config_with_youtube_auth(config, agent, youtube_agent, visitor_bridge, None)
    }

    pub fn from_config_with_youtube_auth(
        config: &Config,
        agent: ureq::Agent,
        youtube_agent: Option<ureq::Agent>,
        visitor_bridge: Option<VisitorBridge>,
        po_token_reader: Option<VisitorReader>,
    ) -> Option<Self> {
        if !config.lyrics_enabled() {
            return None;
        }
        let mut providers: Vec<Box<dyn Provider>> =
            vec![Box::new(LRCLIB { agent, base: LRCLIB_BASE.to_owned() })];
        if let Some(yt_agent) = youtube_agent.filter(|_| config.youtube_native) {
            let (visitor_reader, visitor_writer) =
                visitor_bridge.map_or((None, None), |(r, w)| (Some(r), Some(w)));
            let (pot, vd) = rustlink_config::split_pot_pair(
                &config.youtube_po_token,
                &config.youtube_visitor_data,
            );
            providers.push(Box::new(YTMusic {
                agent: yt_agent,
                endpoint: config.youtube_music_endpoint.clone(),
                version: String::new(),
                visitor_data: vd,
                po_token: pot,
                require_po_token: config.youtube_require_po_token
                    || !config.youtube_po_token_file.trim().is_empty(),
                visitor_reader,
                visitor_writer,
                po_token_reader,
            }));
        }
        Some(Self::new(providers))
    }

    pub fn enabled(&self) -> bool {
        !self.providers.is_empty()
    }

    pub fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError> {
        self.fetch_shared(track).map(Arc::unwrap_or_clone)
    }

    pub fn fetch_shared(&self, track: &Track) -> Result<Arc<Lyrics>, LyricsError> {
        if !self.enabled() {
            return Err(LyricsError::NotFound);
        }
        let key = cache_key(track);
        if let Some(cached) = self.cached(&key) {
            return cached.ok_or(LyricsError::NotFound);
        }
        let flight = {
            let mut inflight = lock(&self.inflight);
            if let Some(flight) = inflight.get(&key).cloned() {
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
            let mut last = None;
            for provider in &self.providers {
                match provider.fetch(track) {
                    Ok(found) => {
                        let shared = Arc::new(found);
                        self.remember(&key, Some(Arc::clone(&shared)));
                        return Ok(shared);
                    }
                    Err(LyricsError::NotFound) => {}
                    Err(e) => last = Some(e),
                }
            }
            if let Some(e) = last {
                return Err(e);
            }
            self.remember(&key, None);
            Err(LyricsError::NotFound)
        })();
        leader.finish(outcome)
    }

    fn cached(&self, key: &str) -> Option<Option<Arc<Lyrics>>> {
        lock(&self.cache).get(key, self.ttl)
    }

    fn remember(&self, key: &str, lyrics: Option<Arc<Lyrics>>) {
        lock(&self.cache).put(key, lyrics, self.limit);
    }
}

#[cfg(test)]
pub(crate) fn serve_http(
    count: usize,
    mut respond: impl FnMut(usize, &str, &[u8]) -> Vec<u8> + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for step in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
            let (hdr_end, content_len) = loop {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                req.extend_from_slice(&buf[..n]);
                if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let len = std::str::from_utf8(&req[..pos])
                        .unwrap()
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .map_or(0, |(_, v)| v.trim().parse::<usize>().unwrap());
                    break (pos + 4, len);
                }
            };
            while req.len() < hdr_end + content_len {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                req.extend_from_slice(&buf[..n]);
            }
            let head = std::str::from_utf8(&req[..hdr_end]).unwrap();
            let raw = respond(step, head, &req[hdr_end..hdr_end + content_len]);
            let _ = socket.write_all(&raw);
        }
    });
    (base, server)
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

    fn titled(identifier: &str, title: &str) -> Track {
        let mut t = track("youtube", identifier);
        t.metadata.title = title.into();
        t
    }

    fn lyrics_of(bytes: usize) -> Result<Lyrics, LyricsError> {
        let mut l = found("big");
        l.text = Some("x".repeat(bytes));
        Ok(l)
    }

    #[test]
    fn the_first_provider_with_an_answer_wins() {
        let manager = Manager::new(vec![
            scripted("empty", || Err(LyricsError::NotFound)).0,
            scripted("full", || Ok(found("full"))).0,
        ]);
        assert_eq!(manager.fetch(&track("youtube", "abc")).unwrap().provider, "full");
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
    fn a_track_with_other_metadata_is_not_served_the_lyrics_of_the_real_one() {
        let (provider, calls) = scripted("db", || Ok(found("db")));
        let manager = Manager::new(vec![provider]);
        manager.fetch(&titled("abc", "the real song")).unwrap();
        manager.fetch(&titled("abc", "the real song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        manager.fetch(&titled("abc", "something forged")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
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
        for n in 0..40 {
            manager.fetch(&titled(&format!("track{n}"), "song")).unwrap();
        }
        assert!(manager.cache.lock().unwrap().bytes <= CACHE_BYTES);
        assert!(manager.cache.lock().unwrap().entries.len() < 40);
        let before = calls.load(Ordering::Relaxed);
        manager.fetch(&titled("track0", "song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1);
        manager.fetch(&titled("track39", "song")).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1);
    }

    #[test]
    fn an_answer_too_large_to_keep_is_returned_but_not_cached() {
        let (provider, calls) = scripted("db", || lyrics_of(2 << 20));
        let manager = Manager::new(vec![provider]);
        assert!(manager.fetch(&titled("huge", "song")).is_ok());
        assert!(manager.fetch(&titled("huge", "song")).is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let cache = manager.cache.lock().unwrap();
        assert_eq!((cache.bytes, cache.entries.len()), (0, 0));
    }

    #[test]
    fn byte_accounting_survives_replacement_and_expiry() {
        let (provider, _) = scripted("db", || lyrics_of(1000));
        let mut manager = Manager::new(vec![provider]);
        manager.ttl = Duration::from_millis(10);
        let t = titled("abc", "song");
        manager.fetch(&t).unwrap();
        let once = manager.cache.lock().unwrap().bytes;
        assert!(once > 1000);
        std::thread::sleep(Duration::from_millis(30));
        manager.fetch(&t).unwrap();
        assert_eq!(manager.cache.lock().unwrap().bytes, once);
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
        assert!(unwound.is_err() && panic_mgr.inflight.lock().unwrap().is_empty());
        assert_eq!(panic_mgr.fetch(&item).unwrap().provider, "recovered");
    }

    #[test]
    fn lru_cache_promotes_recent_hits_and_charges_excess_vector_capacity() {
        let (provider, calls) = scripted("db", || Ok(found("db")));
        let mut manager = Manager::new(vec![provider]);
        manager.limit = 2;
        let (a, b, c) = (titled("a", "song"), titled("b", "song"), titled("c", "song"));
        manager.fetch(&a).unwrap();
        manager.fetch(&b).unwrap();
        manager.fetch(&a).unwrap();
        manager.fetch(&c).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        manager.fetch(&a).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        manager.fetch(&b).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 4);

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
