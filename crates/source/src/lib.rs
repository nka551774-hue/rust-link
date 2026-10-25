//! Track sources.
//!
//! A client names a track, and the node turns that name into something it can
//! play. The name may be a search query, a video id, a catalogue link or a plain
//! URL, and each of those is a different conversation with a different service.
//! What they have in common is the shape they answer in: a Rustlink resolution result.
//!
//! Everything here blocks. The caller runs resolution on its own worker so
//! DNS, TLS and catalogue requests cannot stall the audio transport.

mod cache;
pub mod catalog;
mod deadline;
mod flight;
pub mod innertube;
pub mod matching;
pub mod netguard;
pub mod probe;
#[cfg(test)]
mod test_http;

use cache::Cache;
use flight::{Flight, Leader};
use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{Track, TrackInfo};
use url::Url;

pub use rustlink_protocol::LoadResult;

/// What went wrong while resolving a track.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SourceError {
    /// The identifier is not something any source can be asked about.
    #[error("{0}")]
    Url(String),
    /// A source the configuration switched off was asked for.
    #[error("{0}")]
    Disabled(String),
    /// A request could not be made or answered.
    #[error("{0}")]
    Http(String),
    /// A media server refused the request, which carries the status so the
    /// caller can tell a stale URL from a permanent failure.
    #[error("media server returned HTTP {status}")]
    Media { status: u16 },
    /// The URL is a page or a container the node cannot play.
    #[error("the URL contains no supported audio stream")]
    NoAudio,
}

impl SourceError {
    /// Whether resolving the track again may yield a usable URL.
    ///
    /// A video host rejects a share of otherwise valid URLs, and an expired URL
    /// surfaces as 404 or 410; both are cured by resolving a fresh one, which is
    /// what the caller does when this says yes.
    pub fn retryable(&self) -> bool {
        match self {
            SourceError::Media { status } => {
                matches!(*status, 403 | 404 | 410 | 429) || *status >= 500
            }
            _ => false,
        }
    }
}

/// The largest identifier the node will look at.
const MAX_IDENTIFIER: usize = 8192;

/// The largest load result worth keeping, in bytes.
const MAX_CACHED_RESULT: usize = 2 << 20;

/// The largest number of bytes the cache may hold.
const MAX_CACHE_BYTES: usize = 32 << 20;

/// Whether an identifier is a bare YouTube video id.
pub fn youtube_id(id: &str) -> bool {
    id.len() == 11
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Discord can wrap a literal link in angle brackets to suppress its preview.
/// Only unwrap one complete HTTP(S) link, never arbitrary message/Markdown text.
fn identifier_text(identifier: &str) -> &str {
    let identifier = identifier.trim();
    if let Some(inner) = identifier.strip_prefix('<').and_then(|text| text.strip_suffix('>')) {
        let is_http = inner.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || inner.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"));
        if is_http && !inner.contains(['<', '>']) {
            return inner;
        }
    }
    identifier
}

fn http_url(identifier: &str) -> Option<Url> {
    let identifier = identifier_text(identifier);
    // URL parsers may normalize embedded control characters or backslashes.
    // Those are not a pasted literal link and must not change its authority.
    if identifier.bytes().any(|byte| byte.is_ascii_whitespace() || byte == b'\\') {
        return None;
    }
    let url = Url::parse(identifier).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.'))
}

/// Which source an identifier belongs to.
///
/// A prefix is the node's own shorthand and is checked first, because a search
/// query can contain anything, including something that looks like a URL.
pub fn source_kind(id: &str) -> &'static str {
    let id = identifier_text(id);
    if id.starts_with("spsearch:") || id.starts_with("spotify:") {
        return "spotify";
    }
    if id.starts_with("dzsearch:") || id.starts_with("dzisrc:") || id.starts_with("deezer:") {
        return "deezer";
    }
    if id.starts_with("ytsearch:")
        || id.starts_with("ytmsearch:")
        || id.starts_with("youtube:playlist:")
        || youtube_id(id)
    {
        return "youtube";
    }
    if id.starts_with("scsearch:") || id.starts_with("soundcloud:") {
        return "soundcloud";
    }
    let Some(url) = http_url(id) else {
        return "";
    };
    let host = url.host_str().expect("http_url requires a host");
    for (domain, name) in [
        ("youtube.com", "youtube"),
        ("youtu.be", "youtube"),
        ("soundcloud.com", "soundcloud"),
        ("open.spotify.com", "spotify"),
        ("deezer.com", "deezer"),
    ] {
        if host_matches(host, domain) {
            return name;
        }
    }
    "http"
}

/// A counter that lets a bounded number of loads run at once.
///
/// Resolution is the part of the node that costs someone else money, so the
/// number of loads in flight is a limit the operator sets rather than a function
/// of how many clients are connected.
struct Gate {
    free: Mutex<usize>,
    ready: Condvar,
}

impl Gate {
    fn new(permits: usize) -> Self {
        Gate { free: Mutex::new(permits.max(1)), ready: Condvar::new() }
    }

    fn enter(&self, deadline: Instant) -> Result<Permit<'_>, SourceError> {
        let mut free = self.free.lock().unwrap_or_else(|error| error.into_inner());
        if deadline <= Instant::now() {
            return Err(SourceError::Http("source resolution deadline exceeded".into()));
        }
        while *free == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SourceError::Http(
                    "source resolver capacity reached; retry shortly".into(),
                ));
            }
            let (held, timeout) =
                self.ready.wait_timeout(free, left).unwrap_or_else(|error| error.into_inner());
            free = held;
            if timeout.timed_out() && *free == 0 {
                return Err(SourceError::Http(
                    "source resolver capacity reached; retry shortly".into(),
                ));
            }
        }
        *free -= 1;
        Ok(Permit { gate: self })
    }
}

/// A load that holds one of the gate's permits until it is dropped.
struct Permit<'a> {
    gate: &'a Gate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut free = self.gate.free.lock().unwrap_or_else(|error| error.into_inner());
        *free += 1;
        self.gate.ready.notify_one();
    }
}
/// Resolves identifiers into tracks, and tracks into streams.
pub struct Service {
    config: Config,
    agent: ureq::Agent,
    cache: Mutex<Cache>,
    /// One lock per identifier being resolved, so a burst of clients asking for
    /// the same playlist turns into one conversation with the source and not
    /// one per client.
    inflight: Mutex<HashMap<String, Arc<Flight>>>,
    catalog_state: catalog::State,
    gate: Gate,
    hits: std::sync::atomic::AtomicU64,
    loads: std::sync::atomic::AtomicU64,
}

impl Service {
    pub fn new(config: Config) -> Self {
        let agent = netguard::agent(config.allow_private_networks);
        let gate = Gate::new(config.max_concurrent_loads as usize);
        Service {
            config,
            agent,
            cache: Mutex::new(Cache::default()),
            inflight: Mutex::new(HashMap::new()),
            catalog_state: catalog::State::default(),
            gate,
            hits: std::sync::atomic::AtomicU64::new(0),
            loads: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The client every outbound request goes through.
    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// How many loads were answered from the cache.
    pub fn cache_hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many loads went out to a source.
    pub fn load_count(&self) -> u64 {
        self.loads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Resolves an identifier into an owned result. Call `load_shared` when
    /// the result only needs to be read or serialized, to avoid a deep clone.
    pub fn load(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        self.load_shared(identifier)
            .map(|result| Arc::try_unwrap(result).unwrap_or_else(|shared| shared.as_ref().clone()))
    }

    /// Resolves an identifier, sharing cached and concurrently loaded results.
    pub fn load_shared(&self, identifier: &str) -> Result<Arc<LoadResult>, SourceError> {
        let identifier = identifier_text(identifier);
        self.load_with(identifier, || self.resolve(identifier))
    }

    fn load_with(
        &self,
        identifier: &str,
        resolve: impl FnOnce() -> Result<LoadResult, SourceError>,
    ) -> Result<Arc<LoadResult>, SourceError> {
        let deadline = Instant::now() + self.config.load_timeout();
        let identifier = identifier_text(identifier);
        if identifier.is_empty() || identifier.len() > MAX_IDENTIFIER {
            return Err(SourceError::Url("identifier must contain 1..8192 bytes".into()));
        }
        if let Some(result) = self.cached(identifier) {
            return Ok(result);
        }
        let flight = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(flight) = inflight.get(identifier) {
                let flight = Arc::clone(flight);
                drop(inflight);
                return flight.wait(deadline);
            }
            // A leader may have published and removed its flight between our
            // first cache lookup and locking this map. Recheck atomically with
            // registration so that gap cannot create a second resolver.
            if let Some(result) = self.cached(identifier) {
                return Ok(result);
            }
            let flight = Arc::new(Flight::default());
            inflight.insert(identifier.to_owned(), Arc::clone(&flight));
            flight
        };
        let leader = Leader { flight, key: identifier, inflight: &self.inflight };
        let outcome = (|| {
            let _permit = self.gate.enter(deadline)?;
            let _scope = deadline::enter(deadline);
            self.loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = Arc::new(resolve()?);
            // Publish to the cache before releasing the flight. Failures and
            // oversized answers still reach existing waiters through Flight.
            if let Some(bytes) = cache::result_size(&result) {
                self.cache.lock().unwrap_or_else(|error| error.into_inner()).put(
                    identifier,
                    Arc::clone(&result),
                    bytes,
                    Duration::from_secs(self.config.cache_ttl_seconds as u64),
                    self.config.cache_entries as usize,
                );
            }
            Ok(result)
        })();
        leader.finish(outcome)
    }

    fn cached(&self, identifier: &str) -> Option<Arc<LoadResult>> {
        let result = self.cache.lock().unwrap_or_else(|error| error.into_inner()).get(identifier);
        if result.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Sends an identifier to the source that owns it.
    fn resolve(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        match source_kind(identifier) {
            "spotify" => {
                catalog::Spotify::with_state(&self.agent, &self.config, &self.catalog_state)
                    .load(identifier)
            }
            "deezer" => {
                catalog::Deezer { agent: &self.agent, config: &self.config }.load(identifier)
            }
            "youtube" => self.load_youtube(identifier),
            "soundcloud" => self.load_soundcloud(identifier),
            "http" => self.load_http(identifier),
            // An identifier no source recognises is not a failure: the client
            // asked a question the node has no answer to, which is what empty
            // means.
            _ => Ok(LoadResult::Empty),
        }
    }

    /// Loads YouTube directly through the native catalogue API.
    fn load_youtube(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        if !self.config.youtube_native {
            return Err(SourceError::Disabled("YouTube is disabled (youtubeNative: false)".into()));
        }
        let catalog = innertube::Catalog { agent: &self.agent, config: &self.config };
        if let Some(query) = identifier.strip_prefix("ytsearch:") {
            let tracks = catalog.search(query.trim(), self.config.search_results as usize)?;
            return Ok(finish_search(tracks));
        }
        if let Some(query) = identifier.strip_prefix("ytmsearch:") {
            let tracks = catalog.music_search(query.trim(), self.config.search_results as usize)?;
            return Ok(finish_search(tracks));
        }
        if youtube_id(identifier) {
            return Ok(LoadResult::Track(Box::new(catalog.video(identifier)?)));
        }
        // A shared watch URL often carries its surrounding playlist. A bot
        // playing that link should enqueue the selected video, not the list.
        if let Some(video) = youtube_video_from_url(identifier) {
            return Ok(LoadResult::Track(Box::new(catalog.video(&video)?)));
        }
        if let Some(playlist) = youtube_playlist_id(identifier) {
            return catalog.playlist(&playlist);
        }
        Err(SourceError::Url(format!("{identifier} names no YouTube video or playlist")))
    }

    /// Loads SoundCloud directly through the native catalogue API.
    fn load_soundcloud(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        if !self.config.soundcloud_native {
            return Err(SourceError::Disabled(
                "SoundCloud is disabled (soundcloudNative: false)".into(),
            ));
        }
        catalog::SoundCloud::with_state(&self.agent, &self.config, &self.catalog_state)
            .load(identifier)
    }

    /// Loads a plain URL by reading what its own header says about it.
    fn load_http(&self, identifier: &str) -> Result<LoadResult, SourceError> {
        let (mut body, size) = self.fetch(identifier, &Default::default())?;
        let mut head = Vec::new();
        let mut chunk = vec![0u8; probe::FIRST_PROBE];
        while head.len() < probe::MAX_PROBE {
            let read =
                body.read(&mut chunk).map_err(|error| SourceError::Http(error.to_string()))?;
            if read == 0 {
                break;
            }
            head.extend_from_slice(&chunk[..read]);
            // A container whose index sits at the end states nothing in its
            // head, so a little more is read before giving up on it. Reading
            // past what a header needs would waste the listener's time.
            let found = probe::probe(&head, size);
            if found.audio && (!found.title.is_empty() || found.duration_ms > 0) {
                break;
            }
            if head.len() >= probe::FIRST_PROBE {
                break;
            }
        }
        let found = probe::probe(&head, size);
        if !found.audio {
            return Err(SourceError::NoAudio);
        }
        let title = if found.title.is_empty() { path_name(identifier) } else { found.title };
        let info = TrackInfo {
            identifier: identifier.to_string(),
            title,
            author: if found.artist.is_empty() { "Unknown artist".into() } else { found.artist },
            length: found.duration_ms,
            is_stream: found.duration_ms <= 0,
            is_seekable: found.duration_ms > 0,
            uri: Some(identifier.to_string()),
            artwork_url: found.artwork,
            source_name: "http".into(),
            ..Default::default()
        };
        let track = rustlink_protocol::encode_track(&info)
            .map_err(|error| SourceError::Url(error.to_string()))?;
        Ok(LoadResult::Track(Box::new(track)))
    }
}

/// An empty search is a question with no answer, which the protocol calls empty
/// rather than a search that found nothing.
fn finish_search(tracks: Vec<Track>) -> LoadResult {
    if tracks.is_empty() {
        LoadResult::Empty
    } else {
        LoadResult::Search(tracks)
    }
}

/// The last part of a URL's path, which is the closest thing a bare file has to
/// a name.
fn path_name(identifier: &str) -> String {
    let path = identifier.split(['?', '#']).next().unwrap_or(identifier);
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.trim();
    if name.is_empty() {
        "Unknown title".to_string()
    } else {
        name.to_string()
    }
}

/// Reads the video id out of a YouTube URL of any of the shapes they come in.
fn youtube_video_from_url(identifier: &str) -> Option<String> {
    let url = http_url(identifier)?;
    let host = url.host_str()?;
    let mut parts = url.path_segments()?;
    if host_matches(host, "youtu.be") {
        let id = parts.next()?;
        return (youtube_id(id) && parts.all(str::is_empty)).then(|| id.to_owned());
    }
    if !host_matches(host, "youtube.com") {
        return None;
    }
    match parts.next() {
        Some("watch") if parts.all(str::is_empty) => {
            let (_, id) = url.query_pairs().find(|(key, _)| key == "v")?;
            youtube_id(&id).then(|| id.into_owned())
        }
        Some("shorts") | Some("embed") | Some("live") | Some("v") => {
            let id = parts.next()?;
            (youtube_id(id) && parts.all(str::is_empty)).then(|| id.to_owned())
        }
        _ => None,
    }
}

/// Reads the playlist id out of a YouTube URL or a `youtube:playlist:` prefix.
fn youtube_playlist_id(identifier: &str) -> Option<String> {
    let identifier = identifier_text(identifier);
    let valid = |id: &str| {
        !id.is_empty()
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if let Some(rest) = identifier.strip_prefix("youtube:playlist:") {
        let id = rest.trim();
        return valid(id).then(|| id.to_owned());
    }
    let url = http_url(identifier)?;
    if !host_matches(url.host_str()?, "youtube.com") {
        return None;
    }
    match url.path().trim_end_matches('/') {
        "/playlist" => {}
        // A watch link naming only a list is valid. A broken video id must not
        // unexpectedly enqueue the entire list that happened to accompany it.
        "/watch" if !url.query_pairs().any(|(key, _)| key == "v") => {}
        _ => return None,
    }
    let (_, id) = url.query_pairs().find(|(key, _)| key == "list")?;
    valid(&id).then(|| id.into_owned())
}
impl Service {
    /// Opens the audio of a track that has already been loaded.
    ///
    /// A direct stream URL is never stored in a track encoding, because the ones
    /// the sources hand out expire within hours and a queue may hold a track for
    /// days. Opening is therefore where a URL is resolved, and a track loaded a
    /// week ago plays because of it.
    pub fn open(&self, track: &Track) -> Result<Box<dyn Read + Send>, SourceError> {
        match track.metadata.source_name.as_str() {
            "http" => {
                let uri = track
                    .metadata
                    .uri
                    .clone()
                    .ok_or_else(|| SourceError::Url("track URI is missing".into()))?;
                self.open_seekable(&uri, &Default::default())
            }
            "spotify" | "deezer" => {
                // A catalogue supplies metadata only; the audio is mirrored from
                // a YouTube search for the same name.
                let name =
                    if track.metadata.source_name == "deezer" { "Deezer" } else { "Spotify" };
                let query =
                    format!("ytsearch:{} - {} audio", track.metadata.author, track.metadata.title);
                let result = self.load_shared(&query)?;
                let LoadResult::Search(candidates) = result.as_ref() else {
                    return Err(SourceError::Url(format!("no playable match for {name} track")));
                };
                let best = matching::match_track(track, candidates).ok_or_else(|| {
                    SourceError::Url(format!(
                        "no sufficiently close title, artist and duration match for {name} track"
                    ))
                })?;
                self.open(best)
            }
            "youtube" | "soundcloud" => {
                let identifier =
                    track.metadata.uri.as_deref().unwrap_or(&track.metadata.identifier);
                if source_kind(identifier) != track.metadata.source_name {
                    return Err(SourceError::Url("track URI does not match its source".into()));
                }
                let deadline = Instant::now() + self.config.load_timeout();
                let _permit = self.gate.enter(deadline)?;
                let _scope = deadline::enter(deadline);
                self.open_media(track)
            }
            _ => Err(SourceError::Url("unsupported track source".into())),
        }
    }

    /// Resolves a media URL and opens it, resolving again when the source
    /// rejects the URL it handed out.
    ///
    /// Resolution runs again only after a rejected URL, using the same deadline.
    fn open_media(&self, track: &Track) -> Result<Box<dyn Read + Send>, SourceError> {
        let mut last: Option<SourceError> = None;
        for attempt in 0..MEDIA_ATTEMPTS {
            deadline::timeout(Duration::from_secs(20))?;
            if attempt > 0 {
                std::thread::sleep(deadline::timeout(MEDIA_RETRY_DELAY)?);
            }
            let opened = match track.metadata.source_name.as_str() {
                "youtube" if self.config.youtube_native => {
                    if !youtube_id(&track.metadata.identifier) {
                        return Err(SourceError::Url("invalid YouTube video identifier".into()));
                    }
                    innertube::Catalog { agent: &self.agent, config: &self.config }
                        .with_media_url(&track.metadata.identifier, |url, headers| {
                            self.open_seekable(&url, &headers)
                        })
                }
                "youtube" => Err(SourceError::Disabled("youtube native source is disabled".into())),
                _ => {
                    let (url, headers) = self.resolve_media(track)?;
                    self.open_seekable(&url, &headers)
                }
            };
            match opened {
                Ok(body) => return Ok(body),
                Err(error) => {
                    if !error.retryable() {
                        return Err(error);
                    }
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| SourceError::Http("the media URL could not be opened".into())))
    }

    /// Resolves a direct audio URL using the track's native provider.
    fn resolve_media(
        &self,
        track: &Track,
    ) -> Result<(String, std::collections::BTreeMap<String, String>), SourceError> {
        match track.metadata.source_name.as_str() {
            "soundcloud" if self.config.soundcloud_native => {
                catalog::SoundCloud::with_state(&self.agent, &self.config, &self.catalog_state)
                    .media_url(&track.metadata.identifier)
            }
            source => Err(SourceError::Disabled(format!("{source} native source is disabled"))),
        }
    }

    /// Opens a playback body wrapped so that it can be read out of order.
    pub fn open_seekable(
        &self,
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<Box<dyn Read + Send>, SourceError> {
        Ok(Box::new(self.media_body(url, headers, MediaRange::for_url(url))?))
    }

    fn media_body(
        &self,
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        range: MediaRange,
    ) -> Result<MediaBody, SourceError> {
        let (body, size, _) = request_with_range(&self.agent, url, headers, Some(0), range)?;
        Ok(MediaBody {
            agent: self.agent.clone(),
            url: url.to_string(),
            headers: headers.clone(),
            range,
            reader: Some(body),
            offset: 0,
            response_start: 0,
            size,
            attempts: RESUME_ATTEMPTS,
        })
    }

    /// Fetches a URL for reading, without asking for a range.
    fn fetch(
        &self,
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<(Box<dyn Read + Send>, i64), SourceError> {
        let (body, size, _) = request(&self.agent, url, headers, None)?;
        Ok((body, size))
    }
}

/// Google video CDNs reject open-ended or large byte ranges for otherwise
/// playable URLs. Keep their requests at the verified 128 KiB chunk size.
const GOOGLEVIDEO_CHUNK_BYTES: i64 = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaRange {
    OpenEnded,
    Chunked,
}

impl MediaRange {
    fn for_url(url: &str) -> Self {
        if http_url(url).is_some_and(|url| {
            url.host_str().is_some_and(|host| host_matches(host, "googlevideo.com"))
        }) {
            Self::Chunked
        } else {
            Self::OpenEnded
        }
    }

    fn header(self, offset: i64) -> String {
        match self {
            Self::OpenEnded => format!("bytes={offset}-"),
            Self::Chunked => {
                let end = offset.saturating_add(GOOGLEVIDEO_CHUNK_BYTES - 1);
                format!("bytes={offset}-{end}")
            }
        }
    }
}

/// Makes one request and returns its body, the length the server declared, and
/// whether the range was honoured.
fn request(
    agent: &ureq::Agent,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    offset: Option<i64>,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    request_with_range(agent, url, headers, offset, MediaRange::for_url(url))
}

fn request_with_range(
    agent: &ureq::Agent,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    offset: Option<i64>,
    range: MediaRange,
) -> Result<(Box<dyn Read + Send>, i64, bool), SourceError> {
    netguard::validate_url(url)?;
    // Ureq appends headers instead of replacing them. Select the provider's
    // identity once so CDN requests cannot carry both Rustlink and client UAs.
    let user_agent = headers
        .iter()
        .find_map(|(name, value)| {
            (name.eq_ignore_ascii_case("User-Agent")
                && !value.trim().is_empty()
                && !value.contains(['\r', '\n']))
            .then_some(value.as_str())
        })
        .unwrap_or(USER_AGENT);
    let mut builder = agent.get(url).header("User-Agent", user_agent);
    for (name, value) in headers {
        // A header carrying a line break would let a source inject a second
        // request, so the pair is dropped rather than escaped.
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            continue;
        }
        if name.eq_ignore_ascii_case("User-Agent") {
            continue;
        }
        // Playback owns the requested offset. Provider headers must not pin a
        // resumed request to an earlier range, regardless of header casing.
        if offset.is_some()
            && (name.eq_ignore_ascii_case("Range") || name.eq_ignore_ascii_case("Accept-Encoding"))
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(offset) = offset {
        if offset < 0 {
            return Err(SourceError::Url("media byte offset cannot be negative".into()));
        }
        // A resume offset counts media bytes delivered to the decoder. Transfer
        // compression would make those differ from HTTP representation offsets.
        builder =
            builder.header("Range", &range.header(offset)).header("Accept-Encoding", "identity");
    }
    let budget = deadline::timeout(Duration::from_secs(30))?;
    let config = builder.config().http_status_as_error(false);
    // Range requests become long-lived playback bodies. Their connection and
    // headers are bounded, while netguard's transport enforces a fresh idle
    // timeout for each read. A whole-body deadline would cut off long tracks.
    let config = if offset.is_some() {
        config
            .timeout_global(None)
            .timeout_per_call(None)
            .timeout_recv_body(None)
            .timeout_resolve(Some(budget))
            .timeout_connect(Some(budget.min(Duration::from_secs(10))))
            .timeout_send_request(Some(budget))
            .timeout_recv_response(Some(budget))
    } else {
        config.timeout_global(Some(budget))
    };
    let response = config
        .build()
        .call()
        .map_err(|error| SourceError::Http(format!("{url} could not be reached: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SourceError::Media { status });
    }
    let partial = status == 206;
    if offset.is_some_and(|offset| offset > 0) && !partial {
        return Err(SourceError::Http(
            "media server ignored the requested resume byte range".into(),
        ));
    }
    if offset.is_some() && range == MediaRange::Chunked && !partial {
        return Err(SourceError::Http("bounded media ranges require a 206 response".into()));
    }
    let mut size = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(-1);
    // A partial response describes the slice it returned, so the whole length
    // only comes from the range it states.
    if partial {
        let content_range = response
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .ok_or_else(|| {
                SourceError::Http("media server returned an invalid Content-Range".into())
            })?;
        if content_range.0 != offset.unwrap_or(0) {
            return Err(SourceError::Http(
                "media server returned the wrong resume byte offset".into(),
            ));
        }
        if offset.is_some() && range == MediaRange::Chunked && content_range.1.is_none() {
            return Err(SourceError::Http(
                "bounded media ranges require a known total in Content-Range".into(),
            ));
        }
        size = content_range.1.unwrap_or(-1);
    }
    let reader = response.into_body().into_reader();
    Ok((Box::new(reader), size, partial))
}

/// The user agent the node makes its own requests with.
const USER_AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));

/// How often a rejected media URL is replaced by a fresh one.
const MEDIA_ATTEMPTS: usize = 3;

/// The pause before resolving a media URL again.
const MEDIA_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Consecutive continuation failures allowed without reading new media bytes.
const RESUME_ATTEMPTS: usize = 3;

/// Backoff for a stalled stream; normal chunk boundaries continue immediately.
const RESUME_DELAY: Duration = Duration::from_millis(250);

/// Validate a satisfied byte range and return its starting offset and total.
/// A range whose total is unknown must not use its slice length as file size.
fn parse_content_range(value: &str) -> Option<(i64, Option<i64>)> {
    let (unit, range) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = range.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end): (i64, i64) = (start.parse().ok()?, end.parse().ok()?);
    if start < 0 || end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        let total: i64 = total.parse().ok()?;
        if total <= end {
            return None;
        }
        Some(total)
    };
    Some((start, total))
}

/// A playback stream that keeps flowing when the connection behind it drops.
///
/// The decoder reads the media through this, so a dropped connection would
/// otherwise end the track. Both kinds of source the node streams from answer
/// byte ranges, so a read is continued by asking for the byte after the last one
/// delivered, and the decoder never notices.
pub struct MediaBody {
    agent: ureq::Agent,
    url: String,
    headers: std::collections::BTreeMap<String, String>,
    range: MediaRange,
    reader: Option<Box<dyn Read + Send>>,
    offset: i64,
    response_start: i64,
    size: i64,
    attempts: usize,
}

impl MediaBody {
    /// The number of bytes read so far.
    pub fn position(&self) -> i64 {
        self.offset
    }

    /// The whole length, or -1 when the source did not say.
    pub fn len(&self) -> i64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Asks for the rest of the stream, starting at the byte after the last one
    /// delivered.
    fn resume(&mut self) -> Result<(), SourceError> {
        let (body, size, _) = request_with_range(
            &self.agent,
            &self.url,
            &self.headers,
            Some(self.offset),
            self.range,
        )?;
        if self.size >= 0 && size >= 0 && self.size != size {
            return Err(SourceError::Http("media size changed while resuming the stream".into()));
        }
        if self.size < 0 {
            self.size = size;
        }
        self.reader = Some(body);
        self.response_start = self.offset;
        Ok(())
    }
}

impl Read for MediaBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(reader) = self.reader.as_mut() else {
                return Ok(0);
            };
            let (error, stalled) = match reader.read(buffer) {
                Ok(0) if self.size < 0 || self.offset >= self.size => return Ok(0),
                Ok(0) => (
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("media ended at {} of {} bytes", self.offset, self.size),
                    ),
                    self.offset == self.response_start,
                ),
                Ok(read) => {
                    self.offset += read as i64;
                    // The limit is for consecutive failures without progress.
                    // A server may legitimately answer an open range in many
                    // successful segments, each requiring another request.
                    self.attempts = RESUME_ATTEMPTS;
                    return Ok(read);
                }
                Err(error) => (error, true),
            };
            if stalled {
                if self.attempts == 0 {
                    return Err(error);
                }
                self.attempts -= 1;
                std::thread::sleep(RESUME_DELAY);
            }
            if let Err(resume_error) = self.resume() {
                return Err(io::Error::other(resume_error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranged_fixture(status: &str, content_range: Option<&str>) -> Result<i64, SourceError> {
        ranged_fixture_with_strategy(status, content_range, MediaRange::OpenEnded)
    }

    fn ranged_fixture_with_strategy(
        status: &str,
        content_range: Option<&str>,
        range: MediaRange,
    ) -> Result<i64, SourceError> {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let content_range =
            content_range.map(|value| format!("Content-Range: {value}\r\n")).unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: 3\r\n{content_range}Connection: close\r\n\r\nabc"
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut received = Vec::new();
            let mut buffer = [0; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
            }
            let headers = std::str::from_utf8(&received).unwrap().to_ascii_lowercase();
            let expected_range = format!("range: {}", range.header(5));
            assert_eq!(
                headers.lines().filter(|line| line.starts_with("range:")).collect::<Vec<_>>(),
                [expected_range.as_str()]
            );
            assert_eq!(
                headers
                    .lines()
                    .filter(|line| line.starts_with("accept-encoding:"))
                    .collect::<Vec<_>>(),
                ["accept-encoding: identity"]
            );
            socket.write_all(response.as_bytes()).unwrap();
        });
        let headers = std::collections::BTreeMap::from([
            ("Range".into(), "bytes=0-".into()),
            ("rAnGe".into(), "bytes=1-".into()),
            ("Accept-Encoding".into(), "gzip".into()),
            ("aCcEpT-EnCoDiNg".into(), "br".into()),
        ]);
        let result = request_with_range(&netguard::agent(true), &url, &headers, Some(5), range)
            .map(|(_, size, _)| size);
        server.join().unwrap();
        result
    }

    #[test]
    fn resumed_media_requires_the_exact_requested_byte_range() {
        assert!(ranged_fixture("200 OK", None).unwrap_err().to_string().contains("ignored"));
        assert!(ranged_fixture("206 Partial Content", Some("bytes 0-2/8"))
            .unwrap_err()
            .to_string()
            .contains("wrong"));
        assert!(ranged_fixture("206 Partial Content", None)
            .unwrap_err()
            .to_string()
            .contains("invalid"));
        assert_eq!(ranged_fixture("206 Partial Content", Some("bytes 5-7/8")).unwrap(), 8);
        assert_eq!(ranged_fixture("206 Partial Content", Some("bytes 5-7/*")).unwrap(), -1);
    }

    fn segmented_fixture(
        chunks: Vec<Vec<u8>>,
        total: usize,
    ) -> (MediaBody, std::thread::JoinHandle<()>) {
        segmented_fixture_with_strategy(chunks, total, MediaRange::OpenEnded)
    }

    fn segmented_fixture_with_strategy(
        chunks: Vec<Vec<u8>>,
        total: usize,
        range: MediaRange,
    ) -> (MediaBody, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut start = 0;
            for chunk in chunks {
                let until = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(socket) => break socket,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < until, "segment was never requested");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut received = Vec::new();
                let mut buffer = [0; 1024];
                while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    received.extend_from_slice(&buffer[..read]);
                }
                let headers = std::str::from_utf8(&received).unwrap().to_ascii_lowercase();
                assert!(headers.contains(&format!("\r\nrange: {}\r\n", range.header(start as i64))));
                assert!(headers.contains("\r\naccept-encoding: identity\r\n"));
                let end = start + chunk.len().saturating_sub(1);
                write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n", chunk.len()).unwrap();
                socket.write_all(&chunk).unwrap();
                start += chunk.len();
            }
        });
        let service = Service::new(Config { allow_private_networks: true, ..Config::default() });
        let headers = std::collections::BTreeMap::from([
            ("Range".into(), "bytes=0-999".into()),
            ("rAnGe".into(), "bytes=0-".into()),
        ]);
        let body = service.media_body(&url, &headers, range).unwrap();
        (body, server)
    }

    #[test]
    fn googlevideo_ranges_are_bounded_and_recomputed_from_each_offset() {
        for url in [
            "https://googlevideo.com/videoplayback",
            "https://rr1---sn-example.googlevideo.com/videoplayback?id=video",
            "https://RR1.GOOGLEVIDEO.COM:443/videoplayback",
        ] {
            assert_eq!(MediaRange::for_url(url), MediaRange::Chunked, "{url}");
        }
        for url in [
            "https://notgooglevideo.com/videoplayback",
            "https://googlevideo.com.evil.example/videoplayback",
            "https://googlevideo.com@evil.example/videoplayback",
            "https://evil.example/?host=googlevideo.com",
            "ftp://googlevideo.com/videoplayback",
            "https://example.com/audio.mp3",
        ] {
            assert_eq!(MediaRange::for_url(url), MediaRange::OpenEnded, "{url}");
        }
        assert_eq!(MediaRange::Chunked.header(0), "bytes=0-131071");
        assert_eq!(MediaRange::Chunked.header(131_072), "bytes=131072-262143");
        assert_eq!(MediaRange::Chunked.header(123), "bytes=123-131194");
        assert_eq!(MediaRange::Chunked.header(i64::MAX), format!("bytes={0}-{0}", i64::MAX));
        assert_eq!(MediaRange::OpenEnded.header(131_072), "bytes=131072-");
    }

    #[test]
    fn bounded_media_ranges_reject_unknown_totals_instead_of_truncating_audio() {
        let result = ranged_fixture_with_strategy(
            "206 Partial Content",
            Some("bytes 5-7/*"),
            MediaRange::Chunked,
        );
        assert!(result.unwrap_err().to_string().contains("known total"));
        assert_eq!(
            ranged_fixture_with_strategy(
                "206 Partial Content",
                Some("bytes 5-7/8"),
                MediaRange::Chunked,
            )
            .unwrap(),
            8
        );
    }

    #[test]
    fn bounded_media_chunks_continue_without_retry_delays_or_budget_loss() {
        let chunks = vec![
            vec![1; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![2; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![3; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![4; GOOGLEVIDEO_CHUNK_BYTES as usize],
            vec![5; 17],
        ];
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
        let (mut body, server) =
            segmented_fixture_with_strategy(chunks, expected.len(), MediaRange::Chunked);
        let start = Instant::now();
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        let elapsed = start.elapsed();
        server.join().unwrap();
        result.unwrap();
        assert_eq!(received, expected);
        assert_eq!(body.position(), expected.len() as i64);
        assert_eq!(body.attempts, RESUME_ATTEMPTS);
        // Four ordinary chunk boundaries must not each incur the 250 ms
        // failure backoff. Allow ample slack for the local TCP fixture itself.
        assert!(elapsed < RESUME_DELAY * 3, "normal chunks incurred retry delays: {elapsed:?}");
    }

    #[test]
    fn media_reads_more_segments_than_the_failure_retry_limit() {
        let (mut body, server) = segmented_fixture(
            [b"ab", b"cd", b"ef", b"gh", b"ij"].iter().map(|chunk| chunk.to_vec()).collect(),
            10,
        );
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        server.join().unwrap();
        result.unwrap();
        assert_eq!(received, b"abcdefghij");
    }

    #[test]
    fn media_stops_after_bounded_retries_without_progress() {
        let (mut body, server) = segmented_fixture(vec![Vec::new(); RESUME_ATTEMPTS + 1], 10);
        let mut received = Vec::new();
        let result = body.read_to_end(&mut received);
        server.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(received.is_empty());
    }

    #[test]
    fn playback_body_outlives_the_resolution_deadline() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut received = Vec::new();
            let mut buffer = [0; 1024];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).unwrap();
                assert!(read > 0);
                received.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n")
                .unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            socket.write_all(b"abc").unwrap();
        });
        let agent = netguard::agent(true);
        let mut body = {
            let _scope = deadline::enter(Instant::now() + Duration::from_millis(100));
            request(&agent, &url, &Default::default(), Some(0)).unwrap().0
        };
        std::thread::sleep(Duration::from_millis(150));
        release.send(()).unwrap();
        let mut received = Vec::new();
        body.read_to_end(&mut received).unwrap();
        server.join().unwrap();
        assert_eq!(received, b"abc");
    }

    fn await_waiters(service: &Service, key: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let joined = service
                .inflight
                .lock()
                .unwrap()
                .get(key)
                .is_some_and(|flight| Arc::strong_count(flight) >= count + 2);
            if joined {
                return;
            }
            assert!(Instant::now() < deadline, "waiters did not join the active resolution");
            std::thread::yield_now();
        }
    }

    fn concurrent_result(outcome: Result<LoadResult, SourceError>) -> Service {
        let service = Service::new(Config::default());
        std::thread::scope(|scope| {
            let service = &service;
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let leader = scope.spawn(move || {
                service.load_with("same query", || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    outcome
                })
            });
            started_rx.recv().unwrap();
            let waiters: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        service.load_with("same query", || panic!("a duplicate resolver started"))
                    })
                })
                .collect();
            await_waiters(service, "same query", waiters.len());
            release_tx.send(()).unwrap();
            let expected = leader.join().unwrap();
            for waiter in waiters {
                match (&expected, waiter.join().unwrap()) {
                    (Ok(expected), Ok(result)) => assert!(Arc::ptr_eq(expected, &result)),
                    (Err(expected), Err(error)) => {
                        assert_eq!(expected.to_string(), error.to_string())
                    }
                    _ => panic!("waiters saw different outcomes"),
                }
            }
        });
        assert_eq!(service.load_count(), 1);
        assert!(service.inflight.lock().unwrap().is_empty());
        service
    }

    #[test]
    fn concurrent_callers_share_one_result_and_reuse_the_published_cache() {
        let service = concurrent_result(Ok(LoadResult::Empty));
        assert!(matches!(
            service.load_with("same query", || panic!("cache missed")).unwrap().as_ref(),
            LoadResult::Empty
        ));
        assert_eq!(service.load_count(), 1);
    }

    #[test]
    fn concurrent_failures_are_shared_but_a_later_request_retries() {
        let service = concurrent_result(Err(SourceError::Http("temporary failure".into())));
        assert!(service.load_with("same query", || Ok(LoadResult::Empty)).is_ok());
        assert_eq!(service.load_count(), 2);
    }

    #[test]
    fn oversized_results_are_shared_without_entering_the_cache() {
        let track = Track::new("x".repeat(MAX_CACHED_RESULT + 1), TrackInfo::default());
        let service = concurrent_result(Ok(LoadResult::Track(Box::new(track))));
        assert!(service.cached("same query").is_none());
        assert!(service.load_with("same query", || Ok(LoadResult::Empty)).is_ok());
        assert_eq!(service.load_count(), 2);
    }

    #[test]
    fn a_resolver_panic_releases_its_flight_and_capacity() {
        let service = Service::new(Config::default());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = service.load_with("panic", || panic!("resolver interrupted"));
        }));
        assert!(panic.is_err());
        assert!(service.inflight.lock().unwrap().is_empty());
        assert!(service.load_with("panic", || Ok(LoadResult::Empty)).is_ok());
    }

    #[test]
    fn disabled_native_sources_fail_for_resolution_and_playback() {
        let service = Service::new(Config {
            youtube_native: false,
            soundcloud_native: false,
            ..Config::default()
        });
        for query in ["ytsearch:a song", "scsearch:a song", "youtube:playlist:PL123"] {
            assert!(matches!(service.load(query), Err(SourceError::Disabled(_))));
        }
        for (source, identifier, uri) in [
            ("youtube", "dQw4w9WgXcQ", "https://youtu.be/dQw4w9WgXcQ"),
            ("soundcloud", "42", "https://soundcloud.com/tracks/42"),
        ] {
            let track = Track::new(
                String::new(),
                TrackInfo {
                    source_name: source.into(),
                    identifier: identifier.into(),
                    uri: Some(uri.into()),
                    ..Default::default()
                },
            );
            assert!(matches!(service.open(&track), Err(SourceError::Disabled(_))));
        }
    }

    #[test]
    fn a_search_prefix_names_the_source_it_belongs_to() {
        assert_eq!(source_kind("ytsearch:a song"), "youtube");
        assert_eq!(source_kind("ytmsearch:a song"), "youtube");
        assert_eq!(source_kind("scsearch:a song"), "soundcloud");
        assert_eq!(source_kind("spsearch:a song"), "spotify");
        assert_eq!(source_kind("dzsearch:a song"), "deezer");
    }

    #[test]
    fn a_bare_video_id_is_a_youtube_identifier() {
        assert_eq!(source_kind("dQw4w9WgXcQ"), "youtube");
        assert_eq!(source_kind("too short"), "");
    }

    #[test]
    fn a_link_is_named_by_its_host() {
        assert_eq!(source_kind("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), "youtube");
        assert_eq!(source_kind("https://youtu.be/dQw4w9WgXcQ"), "youtube");
        assert_eq!(source_kind("https://soundcloud.com/someone/a-song"), "soundcloud");
        assert_eq!(source_kind("https://open.spotify.com/track/abc"), "spotify");
        assert_eq!(source_kind("https://www.deezer.com/track/1"), "deezer");
        assert_eq!(source_kind("https://example.com/a.mp3"), "http");
    }

    #[test]
    fn a_host_that_merely_contains_a_name_is_not_that_source() {
        assert_eq!(source_kind("https://notyoutube.com/a"), "http");
        assert_eq!(source_kind("https://youtube.com.evil.example/a"), "http");
    }

    #[test]
    fn youtube_share_urls_preserve_video_ids_across_queries_and_fragments() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&si=share&t=43#bookmark",
            "https://music.youtube.com/watch?si=share&v=dQw4w9WgXcQ&list=PL123#t=43",
            "http://m.youtube.com/watch?v=dQw4w9WgXcQ#fragment",
            "https://youtu.be/dQw4w9WgXcQ?si=share&t=43#fragment",
            "https://youtu.be/dQw4w9WgXcQ#fragment",
            "https://youtube.com/shorts/dQw4w9WgXcQ?si=share#fragment",
            "https://youtube.com/live/dQw4w9WgXcQ?t=43#fragment",
            "https://youtube.com/embed/dQw4w9WgXcQ?start=43#fragment",
            "https://youtube.com/v/dQw4w9WgXcQ?list=PL123#fragment",
            "https://www.youtube.com:443/watch?%76=%64Qw4w9WgXcQ",
            "HTTPS://WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ",
            "<https://youtu.be/dQw4w9WgXcQ?si=discord#fragment>",
        ] {
            assert_eq!(source_kind(url), "youtube", "{url}");
            assert_eq!(youtube_video_from_url(url).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
        // For a short/live/embed URL the path names the selected video.
        assert_eq!(
            youtube_video_from_url("https://youtube.com/shorts/dQw4w9WgXcQ?v=abcdefghijk")
                .as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn youtube_url_parsing_rejects_fake_hosts_credentials_and_non_http_urls() {
        for url in [
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ&list=PL123",
            "https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ&list=PL123",
            "https://youtu.be.evil.example/dQw4w9WgXcQ",
            "https://youtube.com@evil.example/watch?v=dQw4w9WgXcQ",
            "https://user:secret@youtube.com/watch?v=dQw4w9WgXcQ",
            "ftp://youtube.com/watch?v=dQw4w9WgXcQ&list=PL123",
            "javascript://youtube.com/watch?v=dQw4w9WgXcQ",
            "//youtube.com/watch?v=dQw4w9WgXcQ",
            "https://you\ntube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com\\@evil.example/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(youtube_video_from_url(url).is_none(), "{url}");
            assert!(youtube_playlist_id(url).is_none(), "{url}");
            assert_ne!(source_kind(url), "youtube", "{url}");
        }
        assert!(youtube_video_from_url("https://youtube.com/shorts/dQw4w9WgXcQ/extra").is_none());
        assert!(youtube_video_from_url("https://youtube.com/watch?v=dQw4w9WgXcQ%23extra").is_none());
    }

    #[test]
    fn discord_link_unwrapping_requires_one_complete_literal_link() {
        assert_eq!(
            identifier_text(" <https://youtu.be/dQw4w9WgXcQ> "),
            "https://youtu.be/dQw4w9WgXcQ"
        );
        for text in [
            "<<https://youtu.be/dQw4w9WgXcQ>>",
            "play <https://youtu.be/dQw4w9WgXcQ>",
            "<https://youtu.be/dQw4w9WgXcQ> extra",
            "[song](https://youtu.be/dQw4w9WgXcQ)",
            "<ytsearch:a song>",
            "< https://youtu.be/dQw4w9WgXcQ >",
        ] {
            assert_ne!(source_kind(text), "youtube", "{text}");
            assert!(youtube_video_from_url(text).is_none(), "{text}");
        }
    }

    #[test]
    fn shared_video_urls_with_lists_request_only_the_selected_video() {
        let urls = [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&index=7&si=watch",
            "https://youtu.be/dQw4w9WgXcQ?list=PL123&t=43#short-link",
            "https://music.youtube.com/watch?list=PL123&v=dQw4w9WgXcQ#music",
            "https://youtube.com/shorts/dQw4w9WgXcQ?list=PL123&si=short#fragment",
            "https://youtube.com/live/dQw4w9WgXcQ?list=PL123&t=43",
            "https://youtube.com/embed/dQw4w9WgXcQ?list=PL123&start=43",
            "<https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&si=discord>",
        ];
        let (base, server) = test_http::serve(urls.len(), |_, headers, body| {
            assert!(headers.starts_with("POST /youtubei/v1/player?prettyPrint=false "));
            assert_eq!(body["videoId"], "dQw4w9WgXcQ");
            assert!(body.get("browseId").is_none());
            serde_json::json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                "videoId":"dQw4w9WgXcQ","title":"Fixture song","author":"Fixture artist","lengthSeconds":"120"
            }})
        });
        let service = Service::new(Config {
            youtube_endpoint: base,
            allow_private_networks: true,
            ..Config::default()
        });
        for url in urls {
            let result = service.load(url).unwrap();
            let LoadResult::Track(track) = result else {
                panic!("single video became a playlist: {url}")
            };
            assert_eq!(track.metadata.identifier, "dQw4w9WgXcQ");
            assert_eq!(track.metadata.position, 0, "URL timestamps are not implicit seek requests");
        }
        // The preview-suppressing wrapper must share the exact same cache key.
        assert!(service.load(&format!("<{}>", urls[0])).is_ok());
        assert_eq!(service.cache_hits(), 1);
        server.join().unwrap();
    }

    #[test]
    fn playlist_only_links_use_browse_and_invalid_video_ids_do_not_expand_lists() {
        let (base, server) = test_http::serve(3, |_, headers, body| {
            assert!(headers.starts_with("POST /youtubei/v1/browse?prettyPrint=false "));
            assert_eq!(body["browseId"], "VLPL123");
            serde_json::json!({"header":{"playlistHeaderRenderer":{"title":{"simpleText":"Fixture playlist"}}},"contents":{
                "playlistVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Fixture song"},"lengthText":{"simpleText":"2:00"}}
            }})
        });
        let service = Service::new(Config {
            youtube_endpoint: base,
            allow_private_networks: true,
            ..Config::default()
        });
        for url in [
            "https://www.youtube.com/playlist?list=PL123&si=share#fragment",
            "https://music.youtube.com/watch?list=PL123#fragment",
            "youtube:playlist:PL123",
        ] {
            let LoadResult::Playlist(playlist) = service.load(url).unwrap() else {
                panic!("playlist link was not a playlist: {url}")
            };
            assert_eq!(playlist.tracks.len(), 1);
        }
        server.join().unwrap();
        assert!(matches!(
            service.load("https://youtube.com/watch?v=broken&list=PL123"),
            Err(SourceError::Url(_))
        ));
    }

    #[test]
    fn media_request_sends_only_one_provider_user_agent() {
        let (base, server) = test_http::serve(1, |_, headers, _| {
            let lower = headers.to_ascii_lowercase();
            let agents: Vec<_> =
                lower.lines().filter(|line| line.starts_with("user-agent:")).collect();
            assert_eq!(agents, ["user-agent: provider-client/1.0"]);
            serde_json::Value::Null
        });
        let headers = std::collections::BTreeMap::from([
            ("User-Agent".into(), "provider-client/1.0".into()),
            ("user-agent".into(), "duplicate-client/2.0".into()),
        ]);
        let result = request(&netguard::agent(true), &base, &headers, Some(0));
        server.join().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn spotify_metadata_opens_the_full_matching_recording_without_ad_requests() {
        let mut calls = 0;
        let (base, server) = test_http::serve(3, move |base, headers, body| {
            calls += 1;
            match calls {
                1 => {
                    assert!(headers.starts_with("POST /youtubei/v1/search?"));
                    assert_eq!(body["query"], "Someone - A Song audio");
                    let video = |id: &str, title: &str, duration: &str| {
                        serde_json::json!({"videoRenderer":{
                            "videoId":id,"title":{"simpleText":title},"ownerText":{"simpleText":"Someone"},"lengthText":{"simpleText":duration}
                        }})
                    };
                    serde_json::json!({"contents":[
                        {"adSlotRenderer":video("advertisemt1", "A Song", "3:20")},
                        video("preview0001", "A Song Preview", "0:30"),
                        video("cover000001", "A Song Cover", "3:20"),
                        video("dQw4w9WgXcQ", "A Song (Official Audio)", "3:21")
                    ]})
                }
                2 => {
                    assert!(headers.starts_with("POST /youtubei/v1/player?"));
                    assert_eq!(body["videoId"], "dQw4w9WgXcQ");
                    serde_json::json!({
                        "playabilityStatus":{"status":"OK"},
                        "adPlacements":[{"url":format!("{base}/advertisement")}],
                        "streamingData":{"adaptiveFormats":[
                            {"mimeType":"audio/webm; codecs=\"opus\"","bitrate":160000,"url":format!("{base}/recording")}
                        ]}
                    })
                }
                _ => {
                    assert!(headers.starts_with("GET /recording "));
                    serde_json::json!("full-recording-fixture")
                }
            }
        });
        let service = Service::new(Config {
            youtube_endpoint: base,
            allow_private_networks: true,
            ..Config::default()
        });
        let track = rustlink_protocol::encode_track(&TrackInfo {
            identifier: "4cOdK2wGLETKBW3PvgPWqT".into(),
            title: "A Song".into(),
            author: "Someone".into(),
            length: 200_000,
            source_name: "spotify".into(),
            uri: Some("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT".into()),
            ..Default::default()
        })
        .unwrap();
        let mut bytes = Vec::new();
        service.open(&track).unwrap().read_to_end(&mut bytes).unwrap();
        server.join().unwrap();
        assert_eq!(serde_json::from_slice::<String>(&bytes).unwrap(), "full-recording-fixture");
    }

    #[test]
    fn a_pasted_video_link_opens_the_next_client_when_the_first_cdn_returns_403() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cdn = format!("http://{}", listener.local_addr().unwrap());
        let media_server = std::thread::spawn(move || {
            for (path, expected_agent, response) in [
                ("/blocked", "com.google.android.youtube/", "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                ("/allowed", "com.google.android.apps.youtube.vr.oculus/", "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/3\r\nConnection: close\r\n\r\nabc"),
            ] {
                let until = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < until, "next native client never opened its media");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).unwrap();
                    assert!(read > 0 && request.len() < 65_536);
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = std::str::from_utf8(&request).unwrap();
                assert!(request.starts_with(&format!("GET {path} ")));
                let lower = request.to_ascii_lowercase();
                let agents: Vec<_> = lower.lines().filter(|line| line.starts_with("user-agent:")).collect();
                assert_eq!(agents.len(), 1);
                assert!(agents[0].contains(expected_agent), "{}", agents[0]);
                assert!(lower.contains("\r\naccept-encoding: identity\r\n"));
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let mut call = 0;
        let (base, api_server) = test_http::serve(3, move |_, headers, body| {
            assert!(headers.starts_with("POST /youtubei/v1/player?prettyPrint=false "));
            assert_eq!(body["videoId"], "dQw4w9WgXcQ");
            call += 1;
            if call == 1 {
                return serde_json::json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                    "videoId":"dQw4w9WgXcQ","title":"Fixture song","author":"Fixture artist","lengthSeconds":"120"
                }});
            }
            let (client, path) =
                if call == 2 { ("ANDROID", "/blocked") } else { ("ANDROID_VR", "/allowed") };
            assert_eq!(
                body.pointer("/context/client/clientName").and_then(serde_json::Value::as_str),
                Some(client)
            );
            serde_json::json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[
                {"mimeType":"audio/webm; codecs=\"opus\"","bitrate":160000,"url":format!("{cdn}{path}")}
            ]}})
        });
        let service = Service::new(Config {
            youtube_endpoint: base,
            allow_private_networks: true,
            ..Config::default()
        });
        let LoadResult::Track(track) = service
            .load("<https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&si=paste>")
            .unwrap()
        else {
            panic!("pasted single-video link did not resolve to a track");
        };
        let mut audio = service.open(&track).unwrap();
        let mut bytes = Vec::new();
        audio.read_to_end(&mut bytes).unwrap();
        api_server.join().unwrap();
        media_server.join().unwrap();
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn a_video_id_is_read_from_every_shape_of_youtube_link() {
        assert_eq!(
            youtube_video_from_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_video_from_url("https://youtu.be/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_video_from_url("https://www.youtube.com/watch?list=PL123&v=dQw4w9WgXcQ")
                .as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            youtube_video_from_url("https://www.youtube.com/shorts/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert!(youtube_video_from_url("https://example.com/watch?v=dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn a_playlist_id_is_read_from_a_link() {
        assert_eq!(
            youtube_playlist_id("https://www.youtube.com/playlist?list=PL123").as_deref(),
            Some("PL123")
        );
        assert_eq!(youtube_playlist_id("youtube:playlist:PL123").as_deref(), Some("PL123"));
        assert!(youtube_playlist_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn a_url_with_no_name_in_its_path_still_gets_a_title() {
        assert_eq!(path_name("https://example.com/"), "Unknown title");
        assert_eq!(path_name("https://example.com/a%20song.mp3?x=1"), "a%20song.mp3");
    }

    #[test]
    fn a_total_is_read_out_of_a_content_range() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, Some(1234))));
        assert_eq!(parse_content_range("bytes */1234"), None);
        assert_eq!(parse_content_range("nonsense"), None);
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, None)));
        assert_eq!(parse_content_range("bytes 100-99/1234"), None);
        assert_eq!(parse_content_range("bytes 0-99/99"), None);
        assert_eq!(parse_content_range("items 0-99/1234"), None);
    }

    #[test]
    fn a_source_that_is_not_recognised_is_an_empty_answer_rather_than_a_failure() {
        let service = Service::new(Config::default());
        assert!(matches!(service.load("just some words").unwrap(), LoadResult::Empty));
    }

    #[test]
    fn an_identifier_longer_than_the_limit_is_refused() {
        let service = Service::new(Config::default());
        let long = "a".repeat(MAX_IDENTIFIER + 1);
        assert!(service.load(&long).is_err());
        assert!(service.load("   ").is_err());
    }

    #[test]
    fn an_answer_is_reused_rather_than_resolved_twice() {
        let service = Service::new(Config::default());
        // An empty answer is cached like any other, which is what keeps a client
        // asking the same unanswerable question from reaching a source.
        assert!(matches!(service.load("just some words").unwrap(), LoadResult::Empty));
        assert!(matches!(service.load("just some words").unwrap(), LoadResult::Empty));
        assert_eq!(service.load_count(), 1);
        assert_eq!(service.cache_hits(), 1);
    }

    #[test]
    fn the_gate_lets_a_bounded_number_of_loads_run() {
        let gate = Gate::new(1);
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = gate.enter(deadline).unwrap();
        // A second load cannot start while the first holds the only permit.
        assert!(gate.enter(Instant::now()).is_err());
        drop(first);
        assert!(gate.enter(deadline).is_ok());
    }
}
