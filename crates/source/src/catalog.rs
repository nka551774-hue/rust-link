//! Metadata and native stream catalogues for Spotify, Deezer, and SoundCloud.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustlink_config::Config;
use rustlink_protocol::{encode_track_owned, LoadResult, Track, TrackInfo};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    deadline, finish_playlist, finish_search, host_matches, http_url, identifier_text, lock,
    netguard, SourceError,
};

const MAX_BODY: u64 = 4 << 20;
const MAX_PAGES: usize = 10;
const MAX_YOUTUBE_STREAM_CACHE: usize = 512;
const YOUTUBE_STREAM_MAX_TTL: Duration = Duration::from_secs(1800);
const YOUTUBE_STREAM_DEFAULT_TTL: Duration = Duration::from_secs(300);
const YOUTUBE_STREAM_EXPIRY_MARGIN_SECS: u64 = 120;
const CLIENT_COOLDOWN_BASE: Duration = Duration::from_secs(30);
const CLIENT_COOLDOWN_MAX: Duration = Duration::from_secs(300);

type FileStamp = Option<(Option<SystemTime>, u64)>;

#[derive(Default)]
pub(crate) struct PoTokenWatch {
    initialized: bool,
    stamp: FileStamp,
    manual_override: bool,
}

/// Long-lived provider credentials, YouTube stream cache, and client health state.
#[derive(Default)]
pub struct State {
    spotify_token: Arc<CredentialCache>,
    soundcloud_client_id: Arc<CredentialCache>,
    pub(crate) youtube_visitor: Arc<CredentialCache>,
    pub(crate) youtube_streams: Arc<YoutubeStreamCache>,
    pub(crate) youtube_health: Arc<YoutubeClientHealth>,
    pub(crate) youtube_auth: Mutex<Option<rustlink_config::PoTokens>>,
    pub(crate) youtube_pot_watch: Mutex<PoTokenWatch>,
}

fn potoken_path(config: &Config) -> Option<std::path::PathBuf> {
    let explicit = config.youtube_po_token_file.trim();
    if !explicit.is_empty() {
        Some(std::path::PathBuf::from(explicit))
    } else {
        rustlink_config::discover_potoken(None)
    }
}

fn stat_file(path: &std::path::Path) -> FileStamp {
    std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| (m.modified().ok(), m.len()))
}

impl State {
    pub(crate) fn record_manual_youtube_auth(
        &self,
        config: &Config,
        tokens: rustlink_config::PoTokens,
    ) {
        let stamp = potoken_path(config).and_then(|p| stat_file(&p));
        {
            let mut watch = lock(&self.youtube_pot_watch);
            watch.initialized = true;
            watch.stamp = stamp;
            watch.manual_override = true;
        }
        *lock(&self.youtube_auth) = Some(tokens);
        self.youtube_streams.clear();
        self.youtube_health.clear();
    }

    /// Checks the configured or discovered `potoken` file and reloads credentials if modified.
    /// Returns `true` when effective credentials changed and caches were invalidated.
    pub(crate) fn sync_potoken_file(&self, config: &Config) -> bool {
        let Some(path) = potoken_path(config) else { return false };
        let stamp = stat_file(&path);
        {
            let mut watch = lock(&self.youtube_pot_watch);
            if !watch.initialized {
                watch.initialized = true;
                watch.stamp = stamp;
                if stamp.is_none() || watch.manual_override {
                    return false;
                }
            } else if watch.stamp == stamp {
                return false;
            } else {
                watch.stamp = stamp;
                watch.manual_override = false;
            }
        }
        let next = if stamp.is_some() {
            let Ok(bundle) = rustlink_config::load_potoken_bundle(&path) else { return false };
            bundle
        } else {
            rustlink_config::PoTokens::from_config(config)
        };
        let changed = {
            let mut held = lock(&self.youtube_auth);
            let fallback = rustlink_config::PoTokens::from_config(config);
            let cur = held.as_ref().unwrap_or(&fallback);
            let diff = cur != &next;
            *held = Some(next);
            diff
        };
        if changed {
            self.youtube_streams.clear();
            self.youtube_health.clear();
        }
        changed
    }
}

fn wipe_string(secret: &mut String) {
    // SAFETY: NUL (0x00) is valid UTF-8 before clearing length.
    unsafe {
        for byte in secret.as_bytes_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    secret.clear();
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct YoutubeStreamKey {
    pub video_id: String,
    pub endpoint: String,
    pub client_version: String,
    pub has_po_token: bool,
}

#[derive(Clone)]
pub(crate) struct CachedYoutubeStream {
    pub url: String,
    pub content_length: i64,
    pub headers: BTreeMap<String, String>,
    pub user_agent: Cow<'static, str>,
    pub client_name: &'static str,
    pub throttled_without_pot: bool,
    pub tail_probed: bool,
}

impl Drop for CachedYoutubeStream {
    fn drop(&mut self) {
        wipe_string(&mut self.url);
    }
}

struct StreamCacheEntry {
    stream: CachedYoutubeStream,
    inserted_at: Instant,
    expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct YoutubeStreamCache {
    entries: Mutex<HashMap<YoutubeStreamKey, StreamCacheEntry>>,
}

pub(crate) fn youtube_stream_ttl(url: &str) -> Option<Duration> {
    let (_, query) = match url.split('#').next().unwrap_or(url).split_once('?') {
        Some(parts) => parts,
        None => return Some(YOUTUBE_STREAM_DEFAULT_TTL),
    };
    for pair in query.split('&') {
        if let Some(raw_exp) = pair.strip_prefix("expire=") {
            let exp_secs = raw_exp.parse::<u64>().ok()?;
            let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            let usable =
                exp_secs.checked_sub(now_secs.saturating_add(YOUTUBE_STREAM_EXPIRY_MARGIN_SECS))?;
            return (usable > 0).then(|| Duration::from_secs(usable).min(YOUTUBE_STREAM_MAX_TTL));
        }
    }
    Some(YOUTUBE_STREAM_DEFAULT_TTL)
}

impl YoutubeStreamCache {
    pub(crate) fn get(&self, key: &YoutubeStreamKey) -> Option<(CachedYoutubeStream, Duration)> {
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        if let Some(e) = entries.get(key).filter(|e| e.expires_at > now) {
            return Some((e.stream.clone(), now.saturating_duration_since(e.inserted_at)));
        }
        entries.remove(key);
        None
    }

    pub(crate) fn put(&self, key: YoutubeStreamKey, stream: CachedYoutubeStream, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut entries = lock(&self.entries);
        if entries.len() >= MAX_YOUTUBE_STREAM_CACHE && !entries.contains_key(&key) {
            entries.retain(|_, e| e.expires_at > now);
            if let Some(oldest) = (entries.len() >= MAX_YOUTUBE_STREAM_CACHE)
                .then(|| entries.iter().min_by_key(|(_, e)| e.inserted_at).map(|(k, _)| k.clone()))
                .flatten()
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, StreamCacheEntry { stream, inserted_at: now, expires_at: now + ttl });
    }

    pub(crate) fn mark_tail_probed(&self, key: &YoutubeStreamKey) {
        if let Some(entry) = lock(&self.entries).get_mut(key) {
            entry.stream.tail_probed = true;
        }
    }

    pub(crate) fn invalidate(&self, key: &YoutubeStreamKey) {
        lock(&self.entries).remove(key);
    }

    pub(crate) fn clear(&self) {
        lock(&self.entries).clear();
    }
}

struct ClientHealthSlot {
    name: &'static str,
    failures: u32,
    cooldown_until: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct YoutubeClientHealth {
    slots: Mutex<Vec<ClientHealthSlot>>,
}

impl YoutubeClientHealth {
    fn slot_healthy(slots: &[ClientHealthSlot], name: &str, now: Instant) -> bool {
        slots
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.cooldown_until)
            .is_none_or(|until| until <= now)
    }

    pub(crate) fn is_healthy(&self, name: &str) -> bool {
        Self::slot_healthy(&lock(&self.slots), name, Instant::now())
    }

    pub(crate) fn record_success(&self, name: &'static str) {
        if let Some(slot) = lock(&self.slots).iter_mut().find(|s| s.name == name) {
            slot.failures = 0;
            slot.cooldown_until = None;
        }
    }

    pub(crate) fn record_failure(&self, name: &'static str) {
        let now = Instant::now();
        let mut slots = lock(&self.slots);
        let slot = match slots.iter_mut().find(|s| s.name == name) {
            Some(s) => s,
            None => {
                slots.push(ClientHealthSlot { name, failures: 0, cooldown_until: None });
                slots.last_mut().unwrap()
            }
        };
        slot.failures = slot.failures.saturating_add(1);
        let backoff = (CLIENT_COOLDOWN_BASE * (1u32 << slot.failures.saturating_sub(1).min(4)))
            .min(CLIENT_COOLDOWN_MAX);
        slot.cooldown_until = Some(now + backoff);
    }

    pub(crate) fn partition_clients<T>(
        &self,
        list: [T; 4],
        name_of: impl Fn(&T) -> &str,
    ) -> [T; 4] {
        let now = Instant::now();
        let mask: [bool; 4] = {
            let slots = lock(&self.slots);
            std::array::from_fn(|i| Self::slot_healthy(&slots, name_of(&list[i]), now))
        };
        if mask.iter().all(|&ok| ok) || mask.iter().all(|&ok| !ok) {
            return list;
        }
        let mut opts = list.map(Some);
        let mut ordered = Vec::with_capacity(4);
        for pass in [true, false] {
            for (i, &ok) in mask.iter().enumerate() {
                if ok == pass {
                    ordered.extend(opts[i].take());
                }
            }
        }
        ordered.try_into().unwrap_or_else(|_| unreachable!())
    }

    pub(crate) fn clear(&self) {
        lock(&self.slots).clear();
    }
}

#[derive(Default)]
pub(crate) struct CredentialCache {
    held: Mutex<Credential>,
    ready: Condvar,
}

#[derive(Default)]
struct Credential {
    value: Option<(String, Instant)>,
    refreshing: bool,
}

impl CredentialCache {
    pub(crate) fn peek(&self) -> Option<String> {
        lock(&self.held)
            .value
            .as_ref()
            .and_then(|(v, exp)| (*exp > Instant::now()).then(|| v.clone()))
    }

    pub(crate) fn store_if_empty(&self, fresh: &str, ttl: Duration) {
        let fresh = fresh.trim();
        if fresh.is_empty() {
            return;
        }
        let mut held = lock(&self.held);
        if held.value.as_ref().is_none_or(|(_, exp)| *exp <= Instant::now()) {
            held.value = Some((fresh.to_owned(), Instant::now() + ttl));
        }
    }

    pub(crate) fn get_or_refresh(
        &self,
        fetch: impl FnOnce() -> Result<(String, Duration), SourceError>,
    ) -> Result<String, SourceError> {
        let wait_until = Instant::now() + deadline::timeout(Duration::from_secs(20))?;
        let mut held = lock(&self.held);
        loop {
            if let Some((v, _)) = held.value.as_ref().filter(|(_, exp)| *exp > Instant::now()) {
                return Ok(v.clone());
            }
            if !held.refreshing {
                break;
            }
            let rem = wait_until.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err(SourceError::Http(
                    "timed out waiting for catalogue credentials".into(),
                ));
            }
            held = self.ready.wait_timeout(held, rem).unwrap_or_else(|e| e.into_inner()).0;
        }
        held.refreshing = true;
        drop(held);

        struct Guard<'a>(&'a CredentialCache, bool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    lock(&self.0.held).refreshing = false;
                    self.0.ready.notify_all();
                }
            }
        }
        let mut guard = Guard(self, false);
        let (value, ttl) = fetch()?;
        {
            let mut held = lock(&self.held);
            held.value = Some((value.clone(), Instant::now() + ttl));
            held.refreshing = false;
            guard.1 = true;
        }
        self.ready.notify_all();
        Ok(value)
    }

    pub(crate) fn invalidate_if(&self, stale: &str) {
        let mut held = lock(&self.held);
        if held.value.as_ref().is_some_and(|(cur, _)| cur == stale) {
            held.value = None;
        }
    }
}

fn read_checked_body(
    agent: &ureq::Agent,
    url: &str,
    bearer: Option<&str>,
    accept_json: bool,
) -> Result<ureq::Body, SourceError> {
    netguard::validate_url(url)?;
    let mut req = agent.get(url);
    if accept_json {
        req = req.header("Accept", "application/json");
    }
    if let Some(token) = bearer {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    let resp = req
        .config()
        .timeout_global(Some(deadline::timeout(Duration::from_secs(20))?))
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| SourceError::Http(format!("{url} could not be reached: {e}")))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SourceError::Media { status });
    }
    Ok(resp.into_body())
}

fn get_json<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    bearer: Option<&str>,
) -> Result<T, SourceError> {
    let raw = read_checked_body(agent, url, bearer, true)?
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| SourceError::Http(e.to_string()))?;
    serde_json::from_slice(&raw)
        .map_err(|e| SourceError::Http(format!("the catalogue sent an unreadable answer: {e}")))
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, SourceError> {
    read_checked_body(agent, url, None, false)?
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| SourceError::Http(e.to_string()))
}

fn scoped_api_url(
    base_endpoint: &str,
    path: &str,
    provider: &str,
) -> Result<url::Url, SourceError> {
    if path.starts_with("//") {
        return Err(SourceError::Url(format!("invalid {provider} pagination URL")));
    }
    let base_str = base_endpoint.trim_end_matches('/');
    let base = url::Url::parse(base_str).map_err(|e| SourceError::Url(e.to_string()))?;
    let full = if path.starts_with('/') {
        Cow::Owned(format!("{base_str}{path}"))
    } else {
        Cow::Borrowed(path)
    };
    let parsed = http_url(&full)
        .ok_or_else(|| SourceError::Url(format!("invalid {provider} pagination URL")))?;
    let prefix = base.path().trim_end_matches('/');
    let outside = !prefix.is_empty()
        && parsed.path().strip_prefix(prefix).is_none_or(|rest| !rest.starts_with('/'));
    if parsed.origin() != base.origin() || outside || parsed.fragment().is_some() {
        return Err(SourceError::Url(format!("{provider} pagination left the catalogue API")));
    }
    Ok(parsed)
}

fn collection_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "track" => Some("track"),
        "album" => Some("album"),
        "playlist" => Some("playlist"),
        _ => None,
    }
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Spotify
// ---------------------------------------------------------------------------

pub struct Spotify<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
    token: Arc<CredentialCache>,
    api_endpoint: &'a str,
    token_endpoint: &'a str,
}

impl<'a> Spotify<'a> {
    pub fn new(agent: &'a ureq::Agent, config: &'a Config) -> Self {
        Self::with_state(agent, config, &State::default())
    }

    pub fn with_state(agent: &'a ureq::Agent, config: &'a Config, state: &State) -> Self {
        Self {
            agent,
            config,
            token: Arc::clone(&state.spotify_token),
            api_endpoint: "https://api.spotify.com/v1",
            token_endpoint: "https://accounts.spotify.com/api/token",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyArtist {
    #[serde(deserialize_with = "null_default")]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyImage {
    #[serde(deserialize_with = "null_default")]
    url: String,
    #[serde(deserialize_with = "null_default")]
    width: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyTrack {
    #[serde(deserialize_with = "null_default")]
    id: String,
    #[serde(deserialize_with = "null_default")]
    name: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    kind: String,
    #[serde(deserialize_with = "null_default")]
    duration_ms: i64,
    #[serde(deserialize_with = "null_default")]
    artists: Vec<SpotifyArtist>,
    album: Option<SpotifyAlbum>,
    external_ids: Option<SpotifyExternalIds>,
    #[serde(deserialize_with = "null_default")]
    is_local: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyAlbum {
    #[serde(deserialize_with = "null_default")]
    images: Vec<SpotifyImage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SpotifyExternalIds {
    #[serde(deserialize_with = "null_default")]
    isrc: String,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct SpotifyPage<T> {
    #[serde(default, deserialize_with = "null_default")]
    items: Vec<T>,
    #[serde(default)]
    next: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    total: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyPlaylistItem {
    track: Option<SpotifyTrack>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifySearchResponse {
    tracks: Option<SpotifyPage<SpotifyTrack>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyAlbumResponse {
    #[serde(deserialize_with = "null_default")]
    name: String,
    #[serde(deserialize_with = "null_default")]
    images: Vec<SpotifyImage>,
    tracks: Option<SpotifyPage<SpotifyTrack>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyPlaylistResponse {
    #[serde(deserialize_with = "null_default")]
    name: String,
    tracks: Option<SpotifyPage<Option<SpotifyPlaylistItem>>>,
}

impl Spotify<'_> {
    pub fn enabled(&self) -> bool {
        !self.config.spotify_client_id.trim().is_empty()
            && !self.config.spotify_client_secret.trim().is_empty()
    }

    fn token(&self) -> Result<String, SourceError> {
        if !self.enabled() {
            return Err(SourceError::Disabled(
                "spotify needs spotifyClientId and spotifyClientSecret".into(),
            ));
        }
        self.token.get_or_refresh(|| self.fetch_token())
    }

    fn token_form(&self) -> Vec<(&'static str, &str)> {
        let refresh = self.config.spotify_refresh_token.trim();
        let mut form = if refresh.is_empty() {
            vec![("grant_type", "client_credentials")]
        } else {
            vec![("grant_type", "refresh_token"), ("refresh_token", refresh)]
        };
        form.push(("client_id", self.config.spotify_client_id.trim()));
        form.push(("client_secret", self.config.spotify_client_secret.trim()));
        form
    }

    fn fetch_token(&self) -> Result<(String, Duration), SourceError> {
        let resp = self
            .agent
            .post(self.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .config()
            .timeout_global(Some(deadline::timeout(Duration::from_secs(20))?))
            .http_status_as_error(false)
            .build()
            .send_form(self.token_form())
            .map_err(|e| SourceError::Http(format!("spotify could not be reached: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(SourceError::Media { status });
        }
        let raw = resp
            .into_body()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| SourceError::Http(e.to_string()))?;
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            expires_in: Option<u64>,
        }
        let val: TokenResponse = serde_json::from_slice(&raw)
            .map_err(|e| SourceError::Http(format!("spotify sent an unreadable answer: {e}")))?;
        let token = val
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| SourceError::Http("spotify sent no token".into()))?;
        let expires = val.expires_in.unwrap_or(3600).min(86_400);
        Ok((token, Duration::from_secs(expires.saturating_sub(30.min(expires / 10)))))
    }

    fn api<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        let parsed = scoped_api_url(self.api_endpoint, path, "Spotify")?;
        let token = self.token()?;
        match get_json(self.agent, parsed.as_str(), Some(&token)) {
            Err(SourceError::Media { status: 401 }) => {
                self.token.invalidate_if(&token);
                get_json(self.agent, parsed.as_str(), Some(&self.token()?))
            }
            other => other,
        }
    }

    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("spsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let path = format!(
                "/search?type=track&limit={}&market={}&q={}",
                self.config.search_results.min(10),
                encode_query(self.config.spotify_market.trim()),
                encode_query(query)
            );
            let answer: SpotifySearchResponse = self.api(&path)?;
            let tracks = answer
                .tracks
                .map(|p| p.items.into_iter().filter_map(|t| self.track(t)).collect())
                .unwrap_or_default();
            return Ok(finish_search(tracks));
        }
        let (kind, key) = self.parse(id)?;
        match kind {
            "track" => {
                let entry: SpotifyTrack = self.api(&format!("/tracks/{key}"))?;
                let track = self.track(entry).ok_or_else(|| {
                    SourceError::Http("spotify described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "album" => {
                let album: SpotifyAlbumResponse = self.api(&format!("/albums/{key}"))?;
                let art = best_spotify_image(album.images);
                let tracks = self.page_with(
                    album.tracks,
                    &format!("/albums/{key}/tracks"),
                    art.as_deref(),
                    Some,
                )?;
                Ok(finish_playlist(album.name, tracks))
            }
            "playlist" => {
                if key.starts_with("37i9dQZ") && self.config.spotify_refresh_token.trim().is_empty()
                {
                    return Err(SourceError::Disabled(
                        "spotify editorial playlists need spotifyRefreshToken".into(),
                    ));
                }
                let playlist: SpotifyPlaylistResponse = self.api(&format!("/playlists/{key}"))?;
                let tracks = self.page_with(
                    playlist.tracks,
                    &format!("/playlists/{key}/tracks"),
                    None,
                    |item| item?.track,
                )?;
                Ok(finish_playlist(playlist.name, tracks))
            }
            other => {
                Err(SourceError::Url(format!("spotify {other} is not a kind this node loads")))
            }
        }
    }

    #[cfg(test)]
    fn page<T: DeserializeOwned + Default>(
        &self,
        path: &str,
        extract: impl FnMut(T) -> Option<SpotifyTrack>,
    ) -> Result<Vec<Track>, SourceError> {
        self.page_with(None, path, None, extract)
    }

    fn page_with<T: DeserializeOwned + Default>(
        &self,
        mut buffered: Option<SpotifyPage<T>>,
        fallback_path: &str,
        fallback_artwork: Option<&str>,
        mut extract: impl FnMut(T) -> Option<SpotifyTrack>,
    ) -> Result<Vec<Track>, SourceError> {
        let limit = self.config.max_playlist_tracks as usize;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tracks = Vec::new();
        let mut next = buffered.is_none().then(|| fallback_path.to_owned());
        for _ in 0..MAX_PAGES {
            let page = match buffered.take() {
                Some(first) => first,
                None => match next.take() {
                    Some(path) => self.api(&path)?,
                    None => break,
                },
            };
            for item in page.items {
                if let Some(track) =
                    extract(item).and_then(|e| self.track_with_artwork(e, fallback_artwork))
                {
                    tracks.push(track);
                    if tracks.len() >= limit {
                        return Ok(tracks);
                    }
                }
            }
            if page.total == 0 {
                break;
            }
            next = page.next;
        }
        Ok(tracks)
    }

    fn parse(&self, id: &str) -> Result<(&'static str, String), SourceError> {
        let id = identifier_text(id);
        let valid = |kind: &str, key: &str| {
            (key.len() == 22 && key.bytes().all(|b| b.is_ascii_alphanumeric()))
                .then(|| collection_kind(kind))
                .flatten()
        };
        if let Some((k, key)) = id
            .strip_prefix("spotify:")
            .and_then(|r| r.split_once(':'))
            .and_then(|(kind, key)| valid(kind, key).map(|k| (k, key)))
        {
            return Ok((k, key.to_owned()));
        }
        if let Some(url) = http_url(id).filter(|u| u.host_str() == Some("open.spotify.com")) {
            let mut parts = url.path().trim_matches('/').split('/');
            let mut kind = parts.next().unwrap_or_default();
            if kind.starts_with("intl-") {
                kind = parts.next().unwrap_or_default();
            }
            if kind == "embed" {
                kind = parts.next().unwrap_or_default();
            }
            let key = parts.next().unwrap_or_default();
            if let Some(k) = parts.next().is_none().then(|| valid(kind, key)).flatten() {
                return Ok((k, key.to_owned()));
            }
        }
        Err(SourceError::Url(format!("{id} is not a Spotify track, album or playlist")))
    }

    fn track(&self, entry: SpotifyTrack) -> Option<Track> {
        self.track_with_artwork(entry, None)
    }

    fn track_with_artwork(
        &self,
        entry: SpotifyTrack,
        fallback_artwork: Option<&str>,
    ) -> Option<Track> {
        if entry.is_local
            || entry.id.is_empty()
            || (!entry.kind.is_empty() && entry.kind != "track")
        {
            return None;
        }
        let mut author = String::new();
        for artist in entry.artists {
            if !artist.name.is_empty() {
                if author.is_empty() {
                    author = artist.name;
                } else {
                    author.push_str(", ");
                    author.push_str(&artist.name);
                }
            }
        }
        let artwork = entry
            .album
            .and_then(|a| best_spotify_image(a.images))
            .or_else(|| fallback_artwork.filter(|u| !u.is_empty()).map(str::to_owned));
        let uri = Some(format!("https://open.spotify.com/track/{}", entry.id));
        encode_track_owned(TrackInfo {
            identifier: entry.id,
            title: entry.name,
            author: if author.is_empty() { "Unknown artist".into() } else { author },
            length: entry.duration_ms.max(0),
            is_seekable: true,
            uri,
            artwork_url: artwork,
            isrc: entry.external_ids.map(|i| i.isrc).filter(|s| !s.is_empty()),
            source_name: "spotify".into(),
            ..Default::default()
        })
        .ok()
    }
}

fn best_spotify_image(images: Vec<SpotifyImage>) -> Option<String> {
    images.into_iter().max_by_key(|i| i.width).map(|i| i.url).filter(|u| !u.is_empty())
}

// ---------------------------------------------------------------------------
// Deezer
// ---------------------------------------------------------------------------

pub struct Deezer<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerTrack {
    #[serde(deserialize_with = "null_default")]
    id: u64,
    #[serde(deserialize_with = "null_default")]
    title: String,
    #[serde(deserialize_with = "null_default")]
    duration: i64,
    #[serde(deserialize_with = "null_default")]
    isrc: String,
    artist: Option<DeezerArtist>,
    album: Option<DeezerAlbum>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerArtist {
    #[serde(deserialize_with = "null_default")]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerAlbum {
    #[serde(deserialize_with = "null_default")]
    cover_xl: String,
    #[serde(deserialize_with = "null_default")]
    cover_big: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerList {
    #[serde(deserialize_with = "null_default")]
    data: Vec<DeezerTrack>,
    next: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerResponse<T> {
    error: Option<serde::de::IgnoredAny>,
    #[serde(flatten)]
    payload: T,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerNamed {
    #[serde(deserialize_with = "null_default")]
    title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerAlbumResponse {
    #[serde(deserialize_with = "null_default")]
    title: String,
    #[serde(deserialize_with = "null_default")]
    cover_xl: String,
    #[serde(deserialize_with = "null_default")]
    cover_big: String,
    tracks: Option<DeezerList>,
}

impl Deezer<'_> {
    fn api<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        get_json(
            self.agent,
            &format!("{}{path}", self.config.deezer_api_endpoint.trim_end_matches('/')),
            None,
        )
    }

    fn load_track(&self, path: &str) -> Result<LoadResult, SourceError> {
        let answer: DeezerResponse<DeezerTrack> = self.api(path)?;
        if answer.error.is_some() {
            return Ok(LoadResult::Empty);
        }
        let track = self.track(answer.payload).ok_or_else(|| {
            SourceError::Http("deezer described a track that cannot be played".into())
        })?;
        Ok(LoadResult::Track(Box::new(track)))
    }

    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("dzsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let list: DeezerList = self.api(&format!(
                "/search/track?limit={}&q={}",
                self.config.search_results.min(25),
                encode_query(query)
            ))?;
            return Ok(finish_search(self.tracks_from_list(list, None)));
        }
        if let Some(isrc) = id.strip_prefix("dzisrc:") {
            let isrc = isrc.trim();
            return if isrc.is_empty() {
                Ok(LoadResult::Empty)
            } else {
                self.load_track(&format!("/track/isrc:{}", encode_query(isrc)))
            };
        }
        let (kind, key) = self.parse(id)?;
        match kind {
            "track" => self.load_track(&format!("/track/{key}")),
            "album" => {
                let answer: DeezerResponse<DeezerAlbumResponse> =
                    self.api(&format!("/album/{key}"))?;
                if answer.error.is_some() {
                    return Ok(LoadResult::Empty);
                }
                let art = [answer.payload.cover_xl, answer.payload.cover_big]
                    .into_iter()
                    .find(|u| !u.is_empty());
                let tracks = self.paged_with(
                    answer.payload.tracks,
                    &format!("/album/{key}/tracks"),
                    art.as_deref(),
                )?;
                Ok(finish_playlist(answer.payload.title, tracks))
            }
            "playlist" => {
                let answer: DeezerResponse<DeezerNamed> = self.api(&format!("/playlist/{key}"))?;
                if answer.error.is_some() {
                    return Ok(LoadResult::Empty);
                }
                Ok(finish_playlist(
                    answer.payload.title,
                    self.paged(&format!("/playlist/{key}/tracks"))?,
                ))
            }
            other => Err(SourceError::Url(format!("deezer {other} is not a kind this node loads"))),
        }
    }

    fn tracks_from_list(&self, list: DeezerList, fallback_artwork: Option<&str>) -> Vec<Track> {
        list.data.into_iter().filter_map(|e| self.track_with_artwork(e, fallback_artwork)).collect()
    }

    fn paged(&self, path: &str) -> Result<Vec<Track>, SourceError> {
        self.paged_with(None, path, None)
    }

    fn paged_with(
        &self,
        mut buffered: Option<DeezerList>,
        fallback_path: &str,
        fallback_artwork: Option<&str>,
    ) -> Result<Vec<Track>, SourceError> {
        let limit = self.config.max_playlist_tracks as usize;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tracks = Vec::new();
        let mut next = buffered.is_none().then(|| fallback_path.to_owned());
        for _ in 0..MAX_PAGES {
            let mut list = match buffered.take() {
                Some(first) => first,
                None => match next.take() {
                    Some(raw) => {
                        let parsed =
                            scoped_api_url(&self.config.deezer_api_endpoint, &raw, "Deezer")?;
                        get_json(self.agent, parsed.as_str(), None)?
                    }
                    None => break,
                },
            };
            if list.data.is_empty() {
                break;
            }
            next = list.next.take();
            tracks.extend(self.tracks_from_list(list, fallback_artwork));
            if tracks.len() >= limit {
                tracks.truncate(limit);
                break;
            }
        }
        Ok(tracks)
    }

    fn parse(&self, id: &str) -> Result<(&'static str, String), SourceError> {
        let id = identifier_text(id);
        let valid = |kind: &str, key: &str| {
            (!key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()))
                .then(|| collection_kind(kind))
                .flatten()
        };
        if let Some(rest) = id.strip_prefix("deezer:") {
            let mut parts = rest.split(':').map(str::trim).filter(|p| !p.is_empty());
            if let Some((k, key)) = match (parts.next(), parts.next(), parts.next()) {
                (Some(kind), Some(key), None) => valid(kind, key).map(|k| (k, key)),
                _ => None,
            } {
                return Ok((k, key.to_owned()));
            }
        }
        if let Some(url) =
            http_url(id).filter(|u| u.host_str().is_some_and(|h| host_matches(h, "deezer.com")))
        {
            let mut parts = url.path_segments().into_iter().flatten().filter(|p| !p.is_empty());
            let pair = match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(kind), Some(key), None, None) | (Some(_), Some(kind), Some(key), None) => {
                    Some((kind, key))
                }
                _ => None,
            };
            if let Some((k, key)) = pair.and_then(|(kind, key)| valid(kind, key).map(|k| (k, key)))
            {
                return Ok((k, key.to_owned()));
            }
        }
        Err(SourceError::Url(format!("{id} is not a Deezer track, album or playlist")))
    }

    fn track(&self, entry: DeezerTrack) -> Option<Track> {
        self.track_with_artwork(entry, None)
    }

    fn track_with_artwork(
        &self,
        entry: DeezerTrack,
        fallback_artwork: Option<&str>,
    ) -> Option<Track> {
        if entry.id == 0 || entry.title.is_empty() {
            return None;
        }
        let id = entry.id.to_string();
        let artwork = entry
            .album
            .and_then(|a| [a.cover_xl, a.cover_big].into_iter().find(|u| !u.is_empty()))
            .or_else(|| fallback_artwork.filter(|u| !u.is_empty()).map(str::to_owned));
        let uri = Some(format!("https://www.deezer.com/track/{id}"));
        encode_track_owned(TrackInfo {
            identifier: id,
            title: entry.title,
            author: entry
                .artist
                .map(|a| a.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration.max(0).saturating_mul(1000),
            is_seekable: true,
            uri,
            artwork_url: artwork,
            isrc: (!entry.isrc.is_empty()).then_some(entry.isrc),
            source_name: "deezer".into(),
            ..Default::default()
        })
        .ok()
    }
}

// ---------------------------------------------------------------------------
// SoundCloud
// ---------------------------------------------------------------------------

pub struct SoundCloud<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
    client_id: Arc<CredentialCache>,
}

impl<'a> SoundCloud<'a> {
    pub fn new(agent: &'a ureq::Agent, config: &'a Config) -> Self {
        Self::with_state(agent, config, &State::default())
    }

    pub fn with_state(agent: &'a ureq::Agent, config: &'a Config, state: &State) -> Self {
        Self { agent, config, client_id: Arc::clone(&state.soundcloud_client_id) }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundCloudUser {
    #[serde(deserialize_with = "null_default")]
    username: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundCloudTrack {
    #[serde(deserialize_with = "null_default")]
    id: u64,
    #[serde(deserialize_with = "null_default")]
    title: String,
    #[serde(deserialize_with = "null_default")]
    duration: i64,
    #[serde(deserialize_with = "null_default")]
    permalink_url: String,
    artwork_url: Option<String>,
    user: Option<SoundCloudUser>,
    #[serde(deserialize_with = "null_default")]
    streamable: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundCloudList {
    #[serde(deserialize_with = "null_default")]
    collection: Vec<SoundCloudTrack>,
}

fn allowed_sc_url(parsed: &url::Url, origin: Option<&url::Url>) -> bool {
    parsed.host_str().is_some_and(|h| {
        host_matches(h, "soundcloud.com")
            || host_matches(h, "sndcdn.com")
            || origin.is_some_and(|b| b.origin() == parsed.origin())
    })
}

impl SoundCloud<'_> {
    fn client_id(&self) -> Result<String, SourceError> {
        let configured = self.config.soundcloud_client_id.trim();
        if !configured.is_empty() {
            return Ok(configured.to_owned());
        }
        self.client_id.get_or_refresh(|| {
            let base = self.config.soundcloud_web_endpoint.trim_end_matches('/');
            let page = get_text(self.agent, base)?;
            if let Some(id) = scrape_client_id(&page) {
                return Ok((id, self.config.soundcloud_client_id_ttl()));
            }
            for script_url in scrape_script_urls(&page, base) {
                if let Some(id) =
                    get_text(self.agent, &script_url).ok().and_then(|s| scrape_client_id(&s))
                {
                    return Ok((id, self.config.soundcloud_client_id_ttl()));
                }
            }
            Err(SourceError::Http("soundcloud no longer publishes a client id".into()))
        })
    }

    fn get_with_client_id<T: DeserializeOwned>(
        &self,
        url_for: impl Fn(&str) -> String,
    ) -> Result<T, SourceError> {
        let cid = self.client_id()?;
        match get_json(self.agent, &url_for(&cid), None) {
            Err(SourceError::Media { status: 401 | 403 })
                if self.config.soundcloud_client_id.trim().is_empty() =>
            {
                self.client_id.invalidate_if(&cid);
                get_json(self.agent, &url_for(&self.client_id()?), None)
            }
            other => other,
        }
    }

    fn api<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        let base = self.config.soundcloud_api_endpoint.trim_end_matches('/');
        let sep = if path.contains('?') { '&' } else { '?' };
        self.get_with_client_id(|cid| format!("{base}{path}{sep}client_id={}", encode_query(cid)))
    }

    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("scsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let list: SoundCloudList = self.api(&format!(
                "/search/tracks?q={}&limit={}",
                encode_query(query),
                self.config.search_results.min(20)
            ))?;
            return Ok(finish_search(
                list.collection.into_iter().filter_map(|e| self.track(e)).collect(),
            ));
        }
        let key = soundcloud_key(id);
        if key.is_empty() {
            return Err(SourceError::Url(format!("{id} is not a SoundCloud track or playlist")));
        }
        let answer: Value = self.api(&format!("/resolve?url={}", encode_query(&key)))?;
        match answer.get("kind").and_then(Value::as_str).unwrap_or_default() {
            "track" => {
                let entry: SoundCloudTrack =
                    serde_json::from_value(answer).map_err(|e| SourceError::Http(e.to_string()))?;
                let track = self.track(entry).ok_or_else(|| {
                    SourceError::Http("soundcloud described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "playlist" => {
                let name =
                    answer.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                let raw_entries: Vec<SoundCloudTrack> = answer
                    .get("tracks")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .take(self.config.max_playlist_tracks as usize)
                            .filter_map(|e| SoundCloudTrack::deserialize(e).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let stubs: Vec<u64> = raw_entries
                    .iter()
                    .filter(|e| e.id != 0 && e.title.is_empty())
                    .map(|e| e.id)
                    .collect();
                let mut hydrated = HashMap::new();
                for chunk in stubs.chunks(50).take(MAX_PAGES) {
                    let ids = chunk.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
                    if let Ok(fetched) =
                        self.api::<Vec<SoundCloudTrack>>(&format!("/tracks?ids={ids}"))
                    {
                        hydrated.extend(fetched.into_iter().map(|item| (item.id, item)));
                    }
                }
                let tracks = raw_entries
                    .into_iter()
                    .filter_map(|e| {
                        self.track(if e.title.is_empty() { hydrated.remove(&e.id)? } else { e })
                    })
                    .collect();
                Ok(finish_playlist(name, tracks))
            }
            kind => {
                Err(SourceError::Url(format!("{id} resolved to a {kind}, which cannot be played")))
            }
        }
    }

    pub fn media_url(&self, id: &str) -> Result<(String, BTreeMap<String, String>), SourceError> {
        let key = soundcloud_key(id);
        if key.is_empty() && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit())) {
            return Err(SourceError::Url("invalid SoundCloud track identifier".into()));
        }
        let answer: Value = if key.is_empty() {
            self.api(&format!("/tracks/{id}"))?
        } else {
            self.api(&format!("/resolve?url={}", encode_query(&key)))?
        };
        let track_id = answer
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| SourceError::Http("soundcloud named no track".into()))?;
        let track = if !key.is_empty() && answer.pointer("/media/transcodings").is_none() {
            self.api::<Value>(&format!("/tracks/{track_id}"))?
        } else {
            answer
        };
        let transcodings = track
            .pointer("/media/transcodings")
            .and_then(Value::as_array)
            .ok_or_else(|| SourceError::Http("soundcloud offered no stream".into()))?;
        let api_origin = url::Url::parse(self.config.soundcloud_api_endpoint.trim()).ok();
        let (mut chosen, mut fallback) = (None, None);
        for entry in transcodings {
            if entry.pointer("/format/protocol").and_then(Value::as_str) != Some("progressive") {
                continue;
            }
            let Some(u) = entry.get("url").and_then(Value::as_str).filter(|u| {
                !u.is_empty()
                    && http_url(u).is_some_and(|p| allowed_sc_url(&p, api_origin.as_ref()))
            }) else {
                continue;
            };
            if !entry.get("snipped").and_then(Value::as_bool).unwrap_or(false) {
                chosen = Some(u);
                break;
            }
            fallback.get_or_insert(u);
        }
        let url = chosen.or(fallback).ok_or_else(|| {
            SourceError::Http(
                "soundcloud offered no progressive audio stream; HLS playlists are not supported"
                    .into(),
            )
        })?;
        netguard::validate_url(url)?;
        let auth = track
            .get("track_authorization")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && !url.contains("track_authorization="));
        let sep = if url.contains('?') { '&' } else { '?' };
        #[derive(Deserialize)]
        struct StreamUrl {
            url: Option<String>,
        }
        let resolved: StreamUrl = self.get_with_client_id(|cid| match auth {
            Some(a) => format!(
                "{url}{sep}client_id={}&track_authorization={}",
                encode_query(cid),
                encode_query(a)
            ),
            None => format!("{url}{sep}client_id={}", encode_query(cid)),
        })?;
        let direct = resolved
            .url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| SourceError::Http("soundcloud resolved to no stream".into()))?;
        netguard::validate_url(&direct)?;
        Ok((direct, BTreeMap::new()))
    }

    fn track(&self, entry: SoundCloudTrack) -> Option<Track> {
        if entry.id == 0 || entry.title.is_empty() || !entry.streamable {
            return None;
        }
        let id = entry.id.to_string();
        let uri = Some(if entry.permalink_url.is_empty() {
            format!("https://soundcloud.com/tracks/{id}")
        } else {
            entry.permalink_url
        });
        encode_track_owned(TrackInfo {
            identifier: id,
            title: entry.title,
            author: entry
                .user
                .map(|u| u.username)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration.max(0),
            is_seekable: true,
            uri,
            artwork_url: entry.artwork_url,
            source_name: "soundcloud".into(),
            ..Default::default()
        })
        .ok()
    }
}

fn soundcloud_key(id: &str) -> String {
    let trimmed = identifier_text(id);
    if let Some(mut url) = http_url(trimmed) {
        if !url.host_str().is_some_and(|h| host_matches(h, "soundcloud.com")) {
            return String::new();
        }
        if url.host_str().is_some_and(|h| h.eq_ignore_ascii_case("m.soundcloud.com")) {
            let _ = url.set_host(Some("soundcloud.com"));
        }
        url.set_fragment(None);
        return url.into();
    }
    if let Some(rest) = trimmed.strip_prefix("soundcloud:") {
        let rest = rest.trim().trim_start_matches('/');
        if !rest.is_empty()
            && !rest.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\' || b == b'@')
            && !rest.split('/').any(|s| s == "..")
        {
            return format!("https://soundcloud.com/{rest}");
        }
    }
    String::new()
}

fn scrape_script_urls(page: &str, base: &str) -> Vec<String> {
    let base_origin = url::Url::parse(base).ok();
    let mut urls = Vec::new();
    for part in page.rsplit("<script") {
        let Some(tag_end) = part.find('>') else { continue };
        let tag = &part[..tag_end];
        let candidate = [("src=\"", '"'), ("src='", '\'')].into_iter().find_map(|(pat, q)| {
            let rest = &tag[tag.find(pat)? + pat.len()..];
            Some(&rest[..rest.find(q)?])
        });
        let Some(candidate) = candidate else { continue };
        let resolved = if candidate.starts_with("http://") || candidate.starts_with("https://") {
            url::Url::parse(candidate).ok()
        } else {
            base_origin.as_ref().and_then(|b| b.join(candidate).ok())
        };
        let Some(parsed) = resolved.filter(|p| p.path().ends_with(".js")) else { continue };
        if allowed_sc_url(&parsed, base_origin.as_ref())
            && netguard::validate_url(parsed.as_str()).is_ok()
        {
            urls.push(String::from(parsed));
            if urls.len() >= 10 {
                break;
            }
        }
    }
    urls
}

fn scrape_client_id(page: &str) -> Option<String> {
    let mut rest = page;
    while let Some(pos) = rest.find("client_id") {
        rest = &rest[pos + "client_id".len()..];
        let Some(after) = rest.trim_start_matches(['"', '\'', ' ', '\t']).strip_prefix([':', '='])
        else {
            continue;
        };
        let cand = after.trim_start_matches([' ', '\t', '"', '\'']);
        let bytes = cand.as_bytes();
        if bytes.get(..32).is_some_and(|id| id.iter().all(u8::is_ascii_alphanumeric))
            && !bytes.get(32).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Some(cand[..32].to_owned());
        }
    }
    None
}

fn encode_query(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => {
                out.push('%');
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp_cfg() -> Config {
        let mut c = Config::default();
        c.spotify_client_id = "id".into();
        c.spotify_client_secret = "secret".into();
        c
    }

    #[test]
    fn shared_credentials_refresh_once_and_expired_credentials_refresh_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = CredentialCache::default();
        let fetches = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cache
                            .get_or_refresh(|| {
                                fetches.fetch_add(1, Ordering::Relaxed);
                                std::thread::sleep(Duration::from_millis(20));
                                Ok(("token".into(), Duration::from_secs(60)))
                            })
                            .unwrap()
                    })
                })
                .collect();
            for task in tasks {
                assert_eq!(task.join().unwrap(), "token");
            }
        });
        assert_eq!(fetches.load(Ordering::Relaxed), 1);
        cache.held.lock().unwrap().value.as_mut().unwrap().1 = Instant::now();
        assert_eq!(
            cache.get_or_refresh(|| Ok(("new".into(), Duration::from_secs(60)))).unwrap(),
            "new"
        );
    }

    #[test]
    fn credential_refresh_failure_does_not_poison_future_requests() {
        let cache = CredentialCache::default();
        assert!(cache.get_or_refresh(|| Err(SourceError::Http("offline".into()))).is_err());
        assert_eq!(
            cache.get_or_refresh(|| Ok(("ok".into(), Duration::from_secs(60)))).unwrap(),
            "ok"
        );
    }

    #[test]
    fn credentials_survive_temporary_provider_views() {
        let state = State::default();
        let agent = netguard::agent(false);
        let config = sp_cfg();
        state
            .spotify_token
            .get_or_refresh(|| Ok(("cached".into(), Duration::from_secs(60))))
            .unwrap();
        state
            .soundcloud_client_id
            .get_or_refresh(|| Ok(("public-id".into(), Duration::from_secs(60))))
            .unwrap();
        for _ in 0..2 {
            assert_eq!(Spotify::with_state(&agent, &config, &state).token().unwrap(), "cached");
            assert_eq!(
                SoundCloud::with_state(&agent, &config, &state).client_id().unwrap(),
                "public-id"
            );
        }
    }

    #[test]
    fn spotify_credentials_and_api_field_names_are_preserved() {
        let agent = netguard::agent(false);
        let mut config = sp_cfg();
        config.spotify_refresh_token = "refresh".into();
        let spotify = Spotify::new(&agent, &config);
        let form = spotify.token_form();
        assert!(form.contains(&("client_id", "id")));
        assert!(form.contains(&("client_secret", "secret")));
        assert!(form.contains(&("refresh_token", "refresh")));
        let entry: SpotifyTrack = serde_json::from_value(serde_json::json!({
            "id":"track", "name":"name", "duration_ms":12345, "is_local":false,
            "external_ids":{"isrc":"CODE"}
        }))
        .unwrap();
        let track = spotify.track(entry).unwrap();
        assert_eq!((track.metadata.length, track.metadata.isrc.as_deref()), (12345, Some("CODE")));
    }

    #[test]
    fn soundcloud_opens_a_progressive_stream_without_fetching_track_twice() {
        let mut requests = 0;
        let (base, server) = crate::test_http::serve(2, move |base, headers, _| {
            requests += 1;
            if requests == 1 {
                assert!(headers.starts_with("GET /tracks/42?client_id=configured "));
                serde_json::json!({"id":42,"media":{"transcodings":[
                    {"url":format!("{base}/stream"),"format":{"protocol":"progressive"}}
                ]}})
            } else {
                assert!(headers.starts_with("GET /stream?client_id=configured "));
                serde_json::json!({"url":"https://cdn.example/audio.mp3"})
            }
        });
        let mut config = Config::default();
        config.soundcloud_api_endpoint = base;
        config.soundcloud_client_id = "configured".into();
        let result = SoundCloud::new(&netguard::agent(true), &config).media_url("42");
        server.join().unwrap();
        assert_eq!(result.unwrap().0, "https://cdn.example/audio.mp3");
    }

    #[test]
    fn soundcloud_hls_only_reports_an_actionable_native_error() {
        let (base, server) = crate::test_http::serve(1, |_, _, _| {
            serde_json::json!({
                "id":42,"media":{"transcodings":[{"url":"https://example.com/hls","format":{"protocol":"hls"}}]}
            })
        });
        let mut config = Config::default();
        config.soundcloud_api_endpoint = base;
        config.soundcloud_client_id = "id".into();
        let result = SoundCloud::new(&netguard::agent(true), &config).media_url("42");
        server.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("HLS"));
    }

    #[test]
    fn a_spotify_track_id_splits_into_its_kind_and_key() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert_eq!(
            Spotify::new(&agent, &cfg).parse("spotify:track:4cOdK2wGLETKBW3PvgPWqT").unwrap(),
            ("track", "4cOdK2wGLETKBW3PvgPWqT".into())
        );
    }

    #[test]
    fn a_shared_spotify_link_splits_the_same_way() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert_eq!(
            Spotify::new(&agent, &cfg)
                .parse("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc")
                .unwrap(),
            ("track", "4cOdK2wGLETKBW3PvgPWqT".into())
        );
    }

    #[test]
    fn a_spotify_link_with_a_locale_still_splits() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert_eq!(
            Spotify::new(&agent, &cfg)
                .parse("https://open.spotify.com/intl-ja/album/1ATL5GLyefJaxhQzSPVrLX")
                .unwrap(),
            ("album", "1ATL5GLyefJaxhQzSPVrLX".into())
        );
    }

    #[test]
    fn spotify_shared_embedded_and_fragment_links_keep_only_the_resource_id() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        let spotify = Spotify::new(&agent, &cfg);
        for link in [
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT#share",
            "https://open.spotify.com/intl-zh-TW/track/4cOdK2wGLETKBW3PvgPWqT?si=share#song",
            "https://open.spotify.com/embed/track/4cOdK2wGLETKBW3PvgPWqT?utm_source=generator",
            "<https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT>",
            "https://OPEN.SPOTIFY.COM:443/track/4cOdK2wGLETKBW3PvgPWqT/",
        ] {
            assert_eq!(crate::source_kind(link), "spotify", "{link}");
            assert_eq!(
                spotify.parse(link).unwrap(),
                ("track", "4cOdK2wGLETKBW3PvgPWqT".into()),
                "{link}"
            );
        }
        for link in [
            "https://open.spotify.com.evil.example/track/4cOdK2wGLETKBW3PvgPWqT",
            "https://user:secret@open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT/extra",
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT%3Fmarket=US",
            "spotify:track:4cOdK2wGLETKBW3PvgPWqT:extra",
            "spotify:track:../me",
            "spotify:episode:4cOdK2wGLETKBW3PvgPWqT",
        ] {
            assert!(spotify.parse(link).is_err(), "{link}");
        }
    }

    #[test]
    fn spotify_track_album_and_playlist_links_resolve_full_metadata() {
        let (base, server) = crate::test_http::serve(5, |_, headers, _| {
            assert!(headers.to_ascii_lowercase().contains("authorization: bearer test-token\r\n"));
            let track = serde_json::json!({
                "id":"4cOdK2wGLETKBW3PvgPWqT", "name":"A Song", "duration_ms":200000,
                "artists":[{"name":"Someone"}], "preview_url":"https://preview.invalid/30-seconds.mp3"
            });
            if headers.starts_with("GET /v1/tracks/") {
                track
            } else if headers.contains("/tracks HTTP/") {
                let item = if headers.starts_with("GET /v1/playlists/") {
                    serde_json::json!({"track":track})
                } else {
                    track
                };
                serde_json::json!({"items":[item],"total":1,"next":null})
            } else {
                serde_json::json!({"name":"Collection"})
            }
        });
        let (agent, config) = (netguard::agent(true), sp_cfg());
        let endpoint = format!("{base}/v1");
        let spotify = Spotify { api_endpoint: &endpoint, ..Spotify::new(&agent, &config) };
        spotify
            .token
            .get_or_refresh(|| Ok(("test-token".into(), Duration::from_secs(60))))
            .unwrap();
        for kind in ["track", "album", "playlist"] {
            let result = spotify
                .load(&format!(
                    "https://open.spotify.com/intl-zh-TW/{kind}/4cOdK2wGLETKBW3PvgPWqT?si=share#fragment"
                ))
                .unwrap();
            let tracks = match &result {
                LoadResult::Track(t) => std::slice::from_ref(t.as_ref()),
                LoadResult::Playlist(p) => p.tracks.as_slice(),
                other => panic!("unexpected resolution: {other:?}"),
            };
            assert_eq!(tracks.len(), 1);
            assert_eq!(tracks[0].metadata.length, 200_000);
            assert_eq!(tracks[0].metadata.author, "Someone");
            assert_eq!(tracks[0].metadata.source_name, "spotify");
            assert_eq!(
                tracks[0].metadata.uri.as_deref(),
                Some("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT")
            );
            assert!(!tracks[0].token.contains("preview"));
        }
        server.join().unwrap();
    }

    #[test]
    fn spotify_pagination_cannot_leak_credentials_outside_its_api() {
        let (base, server) = crate::test_http::serve(
            1,
            |_, _, _| serde_json::json!({"items":[],"total":2,"next":"https://untrusted.invalid/v1/tracks"}),
        );
        let (agent, config) = (netguard::agent(true), sp_cfg());
        let endpoint = format!("{base}/v1");
        let spotify = Spotify { api_endpoint: &endpoint, ..Spotify::new(&agent, &config) };
        spotify
            .token
            .get_or_refresh(|| Ok(("test-token".into(), Duration::from_secs(60))))
            .unwrap();
        assert!(matches!(spotify.page::<SpotifyTrack>("/tracks", Some), Err(SourceError::Url(_))));
        server.join().unwrap();
        for path in
            ["https://api.spotify.com/v1/tracks", "/../outside", "//outside", "/tracks#fragment"]
        {
            assert!(matches!(spotify.api::<Value>(path), Err(SourceError::Url(_))), "{path}");
        }
    }

    #[test]
    fn a_deezer_identifier_splits_into_its_kind_and_key() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert_eq!(
            Deezer { agent: &agent, config: &cfg }.parse("deezer:track:3135556").unwrap(),
            ("track", "3135556".into())
        );
    }

    #[test]
    fn deezer_links_with_any_locale_parse_and_keys_must_be_numeric() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        let deezer = Deezer { agent: &agent, config: &cfg };
        for link in [
            "https://www.deezer.com/track/3135556",
            "https://www.deezer.com/en/track/3135556",
            "https://www.deezer.com/fr/track/3135556?utm=share",
            "http://deezer.com/us/track/3135556/",
        ] {
            assert_eq!(crate::source_kind(link), "deezer", "{link}");
            assert_eq!(deezer.parse(link).unwrap(), ("track", "3135556".into()), "{link}");
        }
        for bad in [
            "deezer:track:1/../../user/me",
            "deezer:artist:27",
            "https://www.deezer.com/track/abc",
            "https://www.deezer.com/fr/artist/27",
            "https://evil.example/track/1",
        ] {
            assert!(deezer.parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deezer_pagination_stays_on_the_catalogue_api() {
        let track = |id: u64| serde_json::json!({"id": id, "title": "A Song"});
        let mut requests = 0;
        let (base, server) = crate::test_http::serve(2, move |base, headers, _| {
            requests += 1;
            if requests == 1 {
                assert!(headers.starts_with("GET /playlist/1/tracks "));
                serde_json::json!({"data":[track(1)], "next": format!("{base}/playlist/1/tracks?index=1")})
            } else {
                assert!(headers.starts_with("GET /playlist/1/tracks?index=1 "));
                serde_json::json!({"data":[track(2)], "next": "https://evil.example/next"})
            }
        });
        let mut config = Config::default();
        config.deezer_api_endpoint = base;
        let agent = netguard::agent(true);
        let result = Deezer { agent: &agent, config: &config }.paged("/playlist/1/tracks");
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Url(_))));
    }

    #[test]
    fn a_spotify_entry_becomes_a_track_that_names_its_isrc() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        let track = Spotify::new(&agent, &cfg)
            .track(SpotifyTrack {
                id: "abc".into(),
                name: "A Song".into(),
                kind: "track".into(),
                duration_ms: 200_000,
                artists: vec![SpotifyArtist { name: "Someone".into() }],
                album: Some(SpotifyAlbum {
                    images: vec![
                        SpotifyImage { url: "small".into(), width: 64 },
                        SpotifyImage { url: "large".into(), width: 640 },
                    ],
                }),
                external_ids: Some(SpotifyExternalIds { isrc: "ISRC123".into() }),
                is_local: false,
            })
            .unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.author, "Someone");
        assert_eq!(track.metadata.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(track.metadata.artwork_url.as_deref(), Some("large"));
        assert_eq!(track.metadata.uri.as_deref(), Some("https://open.spotify.com/track/abc"));
    }

    #[test]
    fn a_local_spotify_file_is_not_a_track() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert!(Spotify::new(&agent, &cfg)
            .track(SpotifyTrack {
                id: "abc".into(),
                name: "A Song".into(),
                is_local: true,
                ..Default::default()
            })
            .is_none());
    }

    #[test]
    fn a_deezer_entry_becomes_a_track() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        let track = Deezer { agent: &agent, config: &cfg }
            .track(DeezerTrack {
                id: 3135556,
                title: "A Song".into(),
                duration: 200,
                isrc: "ISRC123".into(),
                artist: Some(DeezerArtist { name: "Someone".into() }),
                album: Some(DeezerAlbum { cover_xl: "large".into(), cover_big: String::new() }),
            })
            .unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.length, 200_000);
        assert_eq!(track.metadata.artwork_url.as_deref(), Some("large"));
    }

    #[test]
    fn a_soundcloud_entry_becomes_a_track() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        let track = SoundCloud::new(&agent, &cfg)
            .track(SoundCloudTrack {
                id: 42,
                title: "A Song".into(),
                duration: 200_000,
                permalink_url: "https://soundcloud.com/someone/a-song".into(),
                user: Some(SoundCloudUser { username: "Someone".into() }),
                streamable: true,
                artwork_url: None,
            })
            .unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.source_name, "soundcloud");
        assert_eq!(track.metadata.uri.as_deref(), Some("https://soundcloud.com/someone/a-song"));
    }

    #[test]
    fn a_soundcloud_track_that_cannot_be_streamed_is_not_a_track() {
        let (agent, cfg) = (netguard::agent(false), Config::default());
        assert!(SoundCloud::new(&agent, &cfg)
            .track(SoundCloudTrack { id: 42, title: "A Song".into(), ..Default::default() })
            .is_none());
    }

    #[test]
    fn a_client_id_is_found_in_a_page_of_javascript() {
        let page =
            r#"<script>var x={client_id:"a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",other:1}</script>"#;
        assert_eq!(scrape_client_id(page).as_deref(), Some("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6"));
    }

    #[test]
    fn a_page_without_a_client_id_yields_none() {
        assert!(scrape_client_id("<html></html>").is_none());
    }

    #[test]
    fn a_query_value_is_encoded_the_way_a_form_encodes_it() {
        assert_eq!(encode_query("a song & more"), "a+song+%26+more");
        assert_eq!(encode_query("plain"), "plain");
    }

    #[test]
    fn spotify_and_deezer_albums_use_embedded_tracks_and_propagate_album_artwork() {
        let (sp_base, sp_server) = crate::test_http::serve(1, |_, headers, _| {
            assert!(headers.starts_with("GET /v1/albums/1ATL5GLyefJaxhQzSPVrLX "));
            serde_json::json!({
                "name": "Embedded Album",
                "images": [
                    {"url": "https://img.example/small.jpg", "width": 64},
                    {"url": "https://img.example/cover.jpg", "width": 640}
                ],
                "tracks": {
                    "items": [{"id": "4cOdK2wGLETKBW3PvgPWqT", "name": "Album Track", "duration_ms": 180000, "artists": [{"name": "Artist"}]}],
                    "total": 1,
                    "next": null
                }
            })
        });
        let (agent, sp_config) = (netguard::agent(true), sp_cfg());
        let sp_endpoint = format!("{sp_base}/v1");
        let spotify = Spotify { api_endpoint: &sp_endpoint, ..Spotify::new(&agent, &sp_config) };
        spotify.token.get_or_refresh(|| Ok(("token".into(), Duration::from_secs(60)))).unwrap();
        let LoadResult::Playlist(sp_album) =
            spotify.load("spotify:album:1ATL5GLyefJaxhQzSPVrLX").unwrap()
        else {
            panic!("expected Spotify album playlist");
        };
        sp_server.join().unwrap();
        assert_eq!(sp_album.metadata.name, "Embedded Album");
        assert_eq!(sp_album.tracks.len(), 1);
        assert_eq!(
            sp_album.tracks[0].metadata.artwork_url.as_deref(),
            Some("https://img.example/cover.jpg")
        );

        let (dz_base, dz_server) = crate::test_http::serve(1, |_, headers, _| {
            assert!(headers.starts_with("GET /album/302127 "));
            serde_json::json!({
                "title": "Discovery",
                "cover_xl": "https://img.example/dz-xl.jpg",
                "tracks": {"data": [{"id": 3135556, "title": "One More Time", "duration": 320, "artist": {"name": "Daft Punk"}}]}
            })
        });
        let mut dz_config = Config::default();
        dz_config.deezer_api_endpoint = dz_base;
        let LoadResult::Playlist(dz_album) =
            Deezer { agent: &agent, config: &dz_config }.load("deezer:album:302127").unwrap()
        else {
            panic!("expected Deezer album playlist");
        };
        dz_server.join().unwrap();
        assert_eq!(dz_album.metadata.name, "Discovery");
        assert_eq!(dz_album.tracks.len(), 1);
        assert_eq!(
            dz_album.tracks[0].metadata.artwork_url.as_deref(),
            Some("https://img.example/dz-xl.jpg")
        );
    }

    #[test]
    fn soundcloud_validates_direct_stream_url_and_origin_scheme() {
        let mut req = 0;
        let (base, server) = crate::test_http::serve(2, move |base, _, _| {
            req += 1;
            if req == 1 {
                serde_json::json!({"id": 42, "media": {"transcodings": [
                    {"url": format!("{base}/stream"), "format": {"protocol": "progressive"}}
                ]}})
            } else {
                serde_json::json!({"url": "https://user:pass@cdn.example/audio.mp3"})
            }
        });
        let mut cfg = Config::default();
        cfg.soundcloud_api_endpoint = base;
        cfg.soundcloud_client_id = "configured".into();
        let err = SoundCloud::new(&netguard::agent(true), &cfg).media_url("42").unwrap_err();
        server.join().unwrap();
        assert!(matches!(err, SourceError::Url(_)));

        assert!(scrape_script_urls(
            "<script src=\"http://custom.example/app.js\"></script>",
            "https://custom.example"
        )
        .is_empty());
        assert_eq!(
            scrape_script_urls(
                "<script src=\"https://custom.example:443/app.js\"></script>",
                "https://custom.example"
            )
            .len(),
            1
        );
    }
}
