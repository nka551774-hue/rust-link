//! The metadata catalogues.
//!
//! Spotify and Deezer know what a track is called but do not serve it, so a
//! track loaded from either is a description whose audio is mirrored from a
//! YouTube search for the same name. SoundCloud serves its own audio, and is
//! resolved natively when the configuration asks for it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{encode_track_owned, LoadResult, Playlist, Track, TrackInfo};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::{deadline, netguard, SourceError};

/// Bounds any single catalogue response.
const MAX_BODY: u64 = 4 << 20;

/// How many pages of a collection are read before stopping.
const MAX_PAGES: usize = 10;

/// Provider credentials live as long as the source service, independently of
/// the short-lived catalogue view used for one resolution.
#[derive(Default)]
pub struct State {
    spotify_token: Arc<CredentialCache>,
    soundcloud_client_id: Arc<CredentialCache>,
    pub(crate) youtube_visitor: Arc<CredentialCache>,
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
        let held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        held.value
            .as_ref()
            .and_then(|(value, expires)| (*expires > Instant::now()).then(|| value.clone()))
    }

    pub(crate) fn store_if_empty(&self, fresh: &str, ttl: Duration) {
        let fresh = fresh.trim();
        if fresh.is_empty() {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        let valid = held.value.as_ref().is_some_and(|(_, expires)| *expires > Instant::now());
        if !valid {
            held.value = Some((fresh.to_owned(), Instant::now() + ttl));
        }
    }

    pub(crate) fn get_or_refresh(
        &self,
        fetch: impl FnOnce() -> Result<(String, Duration), SourceError>,
    ) -> Result<String, SourceError> {
        let wait_until = Instant::now() + deadline::timeout(Duration::from_secs(20))?;
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some((value, expires)) = held.value.as_ref() {
                if *expires > Instant::now() {
                    return Ok(value.clone());
                }
            }
            if !held.refreshing {
                break;
            }
            let remaining = wait_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SourceError::Http(
                    "timed out waiting for catalogue credentials".into(),
                ));
            }
            held = self
                .ready
                .wait_timeout(held, remaining)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
        held.refreshing = true;
        drop(held);
        struct Refresh<'a> {
            cache: &'a CredentialCache,
            done: bool,
        }
        impl Drop for Refresh<'_> {
            fn drop(&mut self) {
                if !self.done {
                    let mut held =
                        self.cache.held.lock().unwrap_or_else(|error| error.into_inner());
                    held.refreshing = false;
                    drop(held);
                    self.cache.ready.notify_all();
                }
            }
        }
        let mut refresh = Refresh { cache: self, done: false };
        let (value, ttl) = fetch()?;
        {
            let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
            held.value = Some((value.clone(), Instant::now() + ttl));
            held.refreshing = false;
            refresh.done = true;
        }
        self.ready.notify_all();
        Ok(value)
    }

    pub(crate) fn invalidate_if(&self, stale: &str) {
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        if held.value.as_ref().is_some_and(|(current, _)| current == stale) {
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
    let mut request = agent.get(url);
    if accept_json {
        request = request.header("Accept", "application/json");
    }
    if let Some(token) = bearer {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let response = request
        .config()
        .timeout_global(Some(deadline::timeout(Duration::from_secs(20))?))
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| SourceError::Http(format!("{url} could not be reached: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(SourceError::Media { status });
    }
    Ok(response.into_body())
}

/// Reads and deserializes a JSON answer from a catalogue directly from bytes.
fn get_json<T: DeserializeOwned>(
    agent: &ureq::Agent,
    url: &str,
    bearer: Option<&str>,
) -> Result<T, SourceError> {
    let raw = read_checked_body(agent, url, bearer, true)?
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|error| SourceError::Http(error.to_string()))?;
    serde_json::from_slice(&raw).map_err(|error| {
        SourceError::Http(format!("the catalogue sent an unreadable answer: {error}"))
    })
}

/// Reads a text answer, which is how a client id is scraped out of a page.
fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, SourceError> {
    read_checked_body(agent, url, None, false)?
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|error| SourceError::Http(error.to_string()))
}

/// Resolves a relative API path or pagination URL while ensuring it stays on
/// `base_endpoint`'s origin and path prefix without URL fragments.
fn scoped_api_url(
    base_endpoint: &str,
    path: &str,
    provider: &str,
) -> Result<url::Url, SourceError> {
    if path.starts_with("//") {
        return Err(SourceError::Url(format!("invalid {provider} pagination URL")));
    }
    let base_str = base_endpoint.trim_end_matches('/');
    let base = url::Url::parse(base_str).map_err(|error| SourceError::Url(error.to_string()))?;
    let full = if path.starts_with('/') {
        Cow::Owned(format!("{base_str}{path}"))
    } else {
        Cow::Borrowed(path)
    };
    let parsed = crate::http_url(&full)
        .ok_or_else(|| SourceError::Url(format!("invalid {provider} pagination URL")))?;
    let base_prefix = base.path().trim_end_matches('/');
    let outside_prefix = !base_prefix.is_empty()
        && parsed.path().strip_prefix(base_prefix).is_none_or(|rest| !rest.starts_with('/'));
    if parsed.origin() != base.origin() || outside_prefix || parsed.fragment().is_some() {
        return Err(SourceError::Url(format!("{provider} pagination left the catalogue API")));
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Spotify
// ---------------------------------------------------------------------------

/// Spotify's catalogue, which needs an account to answer at all.
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
        Spotify {
            agent,
            config,
            token: Arc::clone(&state.spotify_token),
            api_endpoint: "https://api.spotify.com/v1",
            token_endpoint: "https://accounts.spotify.com/api/token",
        }
    }
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
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
    /// Whether the node has the credentials to ask at all.
    pub fn enabled(&self) -> bool {
        !self.config.spotify_client_id.trim().is_empty()
            && !self.config.spotify_client_secret.trim().is_empty()
    }

    /// Fetches an access token, reusing the one already held.
    ///
    /// A refresh token is preferred when the operator has one, because a client
    /// credentials token cannot read a private playlist and an operator who
    /// configured a refresh token did so to be able to.
    fn token(&self) -> Result<String, SourceError> {
        if !self.enabled() {
            return Err(SourceError::Disabled(
                "spotify needs spotifyClientId and spotifyClientSecret".into(),
            ));
        }
        self.token.get_or_refresh(|| self.fetch_token())
    }

    fn token_form(&self) -> Vec<(&'static str, &str)> {
        let mut form = Vec::with_capacity(4);
        let refresh = self.config.spotify_refresh_token.trim();
        if refresh.is_empty() {
            form.push(("grant_type", "client_credentials"));
        } else {
            form.push(("grant_type", "refresh_token"));
            form.push(("refresh_token", refresh));
        }
        form.push(("client_id", self.config.spotify_client_id.trim()));
        form.push(("client_secret", self.config.spotify_client_secret.trim()));
        form
    }

    fn fetch_token(&self) -> Result<(String, Duration), SourceError> {
        let response = self
            .agent
            .post(self.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .config()
            .timeout_global(Some(deadline::timeout(Duration::from_secs(20))?))
            .http_status_as_error(false)
            .build()
            .send_form(self.token_form())
            .map_err(|error| SourceError::Http(format!("spotify could not be reached: {error}")))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(SourceError::Media { status });
        }
        let raw = response
            .into_body()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|error| SourceError::Http(error.to_string()))?;
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            expires_in: Option<u64>,
        }
        let value: TokenResponse = serde_json::from_slice(&raw).map_err(|error| {
            SourceError::Http(format!("spotify sent an unreadable answer: {error}"))
        })?;
        let token = value
            .access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| SourceError::Http("spotify sent no token".into()))?;
        let expires = value.expires_in.unwrap_or(3600).min(86_400);
        let ttl = Duration::from_secs(expires.saturating_sub(30.min(expires / 10)));
        Ok((token, ttl))
    }

    fn api<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        // Pagination is supplied by the remote response. Never forward our
        // bearer token to an unrelated host or outside the catalogue API.
        let parsed = scoped_api_url(self.api_endpoint, path, "Spotify")?;
        let token = self.token()?;
        match get_json(self.agent, parsed.as_str(), Some(&token)) {
            Err(SourceError::Media { status: 401 }) => {
                self.token.invalidate_if(&token);
                let fresh = self.token()?;
                get_json(self.agent, parsed.as_str(), Some(&fresh))
            }
            other => other,
        }
    }

    /// Loads whatever a Spotify identifier names.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("spsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let limit = self.config.search_results.min(10);
            let path = format!(
                "/search?type=track&limit={limit}&market={}&q={}",
                encode_query(self.config.spotify_market.trim()),
                encode_query(query)
            );
            let answer: SpotifySearchResponse = self.api(&path)?;
            let tracks: Vec<Track> = answer
                .tracks
                .map(|page| page.items.into_iter().filter_map(|t| self.track(t)).collect())
                .unwrap_or_default();
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
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
                let album_artwork = album
                    .images
                    .into_iter()
                    .max_by_key(|image| image.width)
                    .map(|image| image.url)
                    .filter(|url| !url.is_empty());
                let tracks = self.page_with(
                    album.tracks,
                    &format!("/albums/{key}/tracks"),
                    album_artwork.as_deref(),
                    Some,
                )?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(album.name, -1, tracks))))
            }
            "playlist" => {
                // A Spotify editorial playlist cannot be read without a user's
                // token, and the failure a client gets otherwise is a confusing
                // one, so it is named here.
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
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(playlist.name, -1, tracks))))
            }
            other => {
                Err(SourceError::Url(format!("spotify {other} is not a kind this node loads")))
            }
        }
    }

    /// Reads a paged collection, following the next link until the node has
    /// enough tracks.
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
        initial: Option<SpotifyPage<T>>,
        fallback_path: &str,
        fallback_artwork: Option<&str>,
        mut extract: impl FnMut(T) -> Option<SpotifyTrack>,
    ) -> Result<Vec<Track>, SourceError> {
        let limit = self.config.max_playlist_tracks as usize;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tracks = Vec::new();
        let mut pages = 0;
        let mut next = match initial {
            Some(first) => {
                for item in first.items {
                    if let Some(entry) = extract(item) {
                        if let Some(track) = self.track_with_artwork(entry, fallback_artwork) {
                            tracks.push(track);
                        }
                    }
                    if tracks.len() >= limit {
                        return Ok(tracks);
                    }
                }
                pages = 1;
                if pages >= MAX_PAGES || first.total == 0 {
                    None
                } else {
                    first.next
                }
            }
            None => Some(fallback_path.to_string()),
        };
        while let Some(path) = next.take() {
            if pages >= MAX_PAGES {
                break;
            }
            let page: SpotifyPage<T> = self.api(&path)?;
            for item in page.items {
                if let Some(entry) = extract(item) {
                    if let Some(track) = self.track_with_artwork(entry, fallback_artwork) {
                        tracks.push(track);
                    }
                }
                if tracks.len() >= limit {
                    return Ok(tracks);
                }
            }
            pages += 1;
            if pages >= MAX_PAGES || page.total == 0 {
                break;
            }
            next = page.next;
        }
        Ok(tracks)
    }

    /// Splits an identifier or an open.spotify.com URL into its kind and key.
    fn parse(&self, id: &str) -> Result<(&'static str, String), SourceError> {
        let id = crate::identifier_text(id);
        let match_kind = |kind: &str, key: &str| -> Option<&'static str> {
            if key.len() != 22 || !key.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                return None;
            }
            match kind {
                "track" => Some("track"),
                "album" => Some("album"),
                "playlist" => Some("playlist"),
                _ => None,
            }
        };
        if let Some(rest) = id.strip_prefix("spotify:") {
            if let Some((kind, key)) = rest.split_once(':') {
                if let Some(static_kind) = match_kind(kind, key) {
                    return Ok((static_kind, key.to_string()));
                }
            }
        }
        if let Some(url) = crate::http_url(id) {
            if url.host_str() == Some("open.spotify.com") {
                let mut parts = url.path().trim_matches('/').split('/');
                let mut kind = parts.next().unwrap_or_default();
                if kind.starts_with("intl-") {
                    kind = parts.next().unwrap_or_default();
                }
                if kind == "embed" {
                    kind = parts.next().unwrap_or_default();
                }
                let key = parts.next().unwrap_or_default();
                if parts.next().is_none() {
                    if let Some(static_kind) = match_kind(kind, key) {
                        return Ok((static_kind, key.to_string()));
                    }
                }
            }
        }
        Err(SourceError::Url(format!("{id} is not a Spotify track, album or playlist")))
    }

    /// Turns a catalogue entry into a track, moving strings out without cloning.
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
            .and_then(|album| album.images.into_iter().max_by_key(|image| image.width))
            .map(|image| image.url)
            .filter(|url| !url.is_empty())
            .or_else(|| fallback_artwork.filter(|url| !url.is_empty()).map(str::to_owned));
        let isrc = entry.external_ids.map(|ids| ids.isrc).filter(|isrc| !isrc.is_empty());
        let uri = Some(format!("https://open.spotify.com/track/{}", entry.id));
        let info = TrackInfo {
            identifier: entry.id,
            title: entry.name,
            author: if author.is_empty() { "Unknown artist".into() } else { author },
            length: entry.duration_ms.max(0),
            is_seekable: true,
            uri,
            artwork_url: artwork,
            isrc,
            source_name: "spotify".into(),
            ..Default::default()
        };
        encode_track_owned(info).ok()
    }
}
// ---------------------------------------------------------------------------
// Deezer
// ---------------------------------------------------------------------------

/// Deezer's catalogue, which answers without an account.
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
        let url = format!("{}{path}", self.config.deezer_api_endpoint.trim_end_matches('/'));
        get_json(self.agent, &url, None)
    }

    /// Loads whatever a Deezer identifier names.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("dzsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let limit = self.config.search_results.min(25);
            let list: DeezerList =
                self.api(&format!("/search/track?limit={limit}&q={}", encode_query(query)))?;
            let tracks = self.tracks_from_list(list);
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
        }
        if let Some(isrc) = id.strip_prefix("dzisrc:") {
            let isrc = isrc.trim();
            if isrc.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let answer: DeezerResponse<DeezerTrack> =
                self.api(&format!("/track/isrc:{}", encode_query(isrc)))?;
            if answer.error.is_some() {
                return Ok(LoadResult::Empty);
            }
            let track = self.track(answer.payload).ok_or_else(|| {
                SourceError::Http("deezer described a track that cannot be played".into())
            })?;
            return Ok(LoadResult::Track(Box::new(track)));
        }
        let (kind, key) = self.parse(id)?;
        match kind {
            "track" => {
                let answer: DeezerResponse<DeezerTrack> = self.api(&format!("/track/{key}"))?;
                if answer.error.is_some() {
                    return Ok(LoadResult::Empty);
                }
                let track = self.track(answer.payload).ok_or_else(|| {
                    SourceError::Http("deezer described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "album" => {
                let answer: DeezerResponse<DeezerAlbumResponse> =
                    self.api(&format!("/album/{key}"))?;
                if answer.error.is_some() {
                    return Ok(LoadResult::Empty);
                }
                let album_artwork = if !answer.payload.cover_xl.is_empty() {
                    Some(answer.payload.cover_xl)
                } else if !answer.payload.cover_big.is_empty() {
                    Some(answer.payload.cover_big)
                } else {
                    None
                };
                let tracks = self.paged_with(
                    answer.payload.tracks,
                    &format!("/album/{key}/tracks"),
                    album_artwork.as_deref(),
                )?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(answer.payload.title, -1, tracks))))
            }
            "playlist" => {
                let answer: DeezerResponse<DeezerNamed> = self.api(&format!("/playlist/{key}"))?;
                if answer.error.is_some() {
                    return Ok(LoadResult::Empty);
                }
                let tracks = self.paged(&format!("/playlist/{key}/tracks"))?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(answer.payload.title, -1, tracks))))
            }
            other => Err(SourceError::Url(format!("deezer {other} is not a kind this node loads"))),
        }
    }

    fn tracks_from_list(&self, list: DeezerList) -> Vec<Track> {
        self.tracks_from_list_with_artwork(list, None)
    }

    fn tracks_from_list_with_artwork(
        &self,
        list: DeezerList,
        fallback_artwork: Option<&str>,
    ) -> Vec<Track> {
        list.data
            .into_iter()
            .filter_map(|entry| self.track_with_artwork(entry, fallback_artwork))
            .collect()
    }

    /// Reads a collection, following the next page the payload names.
    fn paged(&self, path: &str) -> Result<Vec<Track>, SourceError> {
        self.paged_with(None, path, None)
    }

    fn paged_with(
        &self,
        initial: Option<DeezerList>,
        fallback_path: &str,
        fallback_artwork: Option<&str>,
    ) -> Result<Vec<Track>, SourceError> {
        let limit = self.config.max_playlist_tracks as usize;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tracks = Vec::new();
        let mut pages = 0;
        let mut next = match initial {
            Some(mut first) => {
                if first.data.is_empty() {
                    None
                } else {
                    let cont = first.next.take();
                    tracks.extend(self.tracks_from_list_with_artwork(first, fallback_artwork));
                    pages = 1;
                    if tracks.len() >= limit || pages >= MAX_PAGES {
                        None
                    } else {
                        cont
                    }
                }
            }
            None => Some(fallback_path.to_string()),
        };
        while let Some(raw_next) = next.take() {
            if pages >= MAX_PAGES {
                break;
            }
            // The next link comes from the response; never follow it off the API.
            let parsed = scoped_api_url(&self.config.deezer_api_endpoint, &raw_next, "Deezer")?;
            let mut list: DeezerList = get_json(self.agent, parsed.as_str(), None)?;
            if list.data.is_empty() {
                break;
            }
            next = list.next.take();
            tracks.extend(self.tracks_from_list_with_artwork(list, fallback_artwork));
            pages += 1;
            if tracks.len() >= limit || pages >= MAX_PAGES {
                break;
            }
        }
        tracks.truncate(limit);
        Ok(tracks)
    }

    fn parse(&self, id: &str) -> Result<(&'static str, String), SourceError> {
        let id = crate::identifier_text(id);
        let match_kind = |kind: &str, key: &str| -> Option<&'static str> {
            if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            match kind {
                "track" => Some("track"),
                "album" => Some("album"),
                "playlist" => Some("playlist"),
                _ => None,
            }
        };
        if let Some(rest) = id.strip_prefix("deezer:") {
            let mut parts = rest.split(':').map(str::trim).filter(|part| !part.is_empty());
            if let (Some(kind), Some(key), None) = (parts.next(), parts.next(), parts.next()) {
                if let Some(static_kind) = match_kind(kind, key) {
                    return Ok((static_kind, key.to_string()));
                }
            }
        }
        if let Some(url) = crate::http_url(id) {
            if url.host_str().is_some_and(|host| crate::host_matches(host, "deezer.com")) {
                // Share links carry any locale first, such as /fr/ or /us/.
                let mut parts = url.path_segments().into_iter().flatten().filter(|p| !p.is_empty());
                let pair = match (parts.next(), parts.next(), parts.next(), parts.next()) {
                    (Some(kind), Some(key), None, None)
                    | (Some(_), Some(kind), Some(key), None) => Some((kind, key)),
                    _ => None,
                };
                if let Some((kind, key)) = pair {
                    if let Some(static_kind) = match_kind(kind, key) {
                        return Ok((static_kind, key.to_string()));
                    }
                }
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
            .map(|album| if !album.cover_xl.is_empty() { album.cover_xl } else { album.cover_big })
            .filter(|url| !url.is_empty())
            .or_else(|| fallback_artwork.filter(|url| !url.is_empty()).map(str::to_owned));
        let uri = Some(format!("https://www.deezer.com/track/{id}"));
        let info = TrackInfo {
            identifier: id,
            title: entry.title,
            author: entry
                .artist
                .map(|artist| artist.name)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration.max(0).saturating_mul(1000),
            is_seekable: true,
            uri,
            artwork_url: artwork,
            isrc: (!entry.isrc.is_empty()).then_some(entry.isrc),
            source_name: "deezer".into(),
            ..Default::default()
        };
        encode_track_owned(info).ok()
    }
}
// ---------------------------------------------------------------------------
// SoundCloud
// ---------------------------------------------------------------------------

/// SoundCloud's catalogue and its own audio.
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
        SoundCloud { agent, config, client_id: Arc::clone(&state.soundcloud_client_id) }
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

impl SoundCloud<'_> {
    /// Finds the client id the web app is currently using.
    ///
    /// SoundCloud hands its own front end an id and expects it back on every API
    /// call; it rotates, so it is read from the page rather than configured.
    fn client_id(&self) -> Result<String, SourceError> {
        if !self.config.soundcloud_client_id.trim().is_empty() {
            return Ok(self.config.soundcloud_client_id.trim().to_string());
        }
        self.client_id.get_or_refresh(|| {
            let base = self.config.soundcloud_web_endpoint.trim_end_matches('/');
            let page = get_text(self.agent, base)?;
            if let Some(id) = scrape_client_id(&page) {
                return Ok((id, self.config.soundcloud_client_id_ttl()));
            }
            for script_url in scrape_script_urls(&page, base) {
                if let Ok(script) = get_text(self.agent, &script_url) {
                    if let Some(id) = scrape_client_id(&script) {
                        return Ok((id, self.config.soundcloud_client_id_ttl()));
                    }
                }
            }
            Err(SourceError::Http("soundcloud no longer publishes a client id".into()))
        })
    }

    fn api<T: DeserializeOwned>(&self, path: &str) -> Result<T, SourceError> {
        let client = self.client_id()?;
        let base = self.config.soundcloud_api_endpoint.trim_end_matches('/');
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!("{base}{path}{separator}client_id={}", encode_query(&client));
        match get_json(self.agent, &url, None) {
            Err(SourceError::Media { status: 401 | 403 })
                if self.config.soundcloud_client_id.trim().is_empty() =>
            {
                self.client_id.invalidate_if(&client);
                let fresh = self.client_id()?;
                let url = format!("{base}{path}{separator}client_id={}", encode_query(&fresh));
                get_json(self.agent, &url, None)
            }
            other => other,
        }
    }

    /// Loads a SoundCloud search, track or playlist.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("scsearch:") {
            let query = query.trim();
            if query.is_empty() {
                return Ok(LoadResult::Empty);
            }
            let limit = self.config.search_results.min(20);
            let list: SoundCloudList =
                self.api(&format!("/search/tracks?q={}&limit={limit}", encode_query(query)))?;
            let tracks: Vec<Track> =
                list.collection.into_iter().filter_map(|entry| self.track(entry)).collect();
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
        }
        let key = soundcloud_key(id);
        if key.is_empty() {
            return Err(SourceError::Url(format!("{id} is not a SoundCloud track or playlist")));
        }
        let answer: Value = self.api(&format!("/resolve?url={}", encode_query(&key)))?;
        match answer.get("kind").and_then(Value::as_str).unwrap_or_default() {
            "track" => {
                let entry: SoundCloudTrack = serde_json::from_value(answer)
                    .map_err(|error| SourceError::Http(error.to_string()))?;
                let track = self.track(entry).ok_or_else(|| {
                    SourceError::Http("soundcloud described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "playlist" => {
                let name =
                    answer.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
                let raw_entries: Vec<SoundCloudTrack> = answer
                    .get("tracks")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .take(self.config.max_playlist_tracks as usize)
                            .filter_map(|entry| SoundCloudTrack::deserialize(entry).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let stub_ids: Vec<u64> = raw_entries
                    .iter()
                    .filter(|entry| entry.id != 0 && entry.title.is_empty())
                    .map(|entry| entry.id)
                    .collect();
                let mut hydrated = std::collections::HashMap::new();
                for chunk in stub_ids.chunks(50).take(MAX_PAGES) {
                    let mut ids = String::new();
                    for (idx, stub_id) in chunk.iter().enumerate() {
                        if idx > 0 {
                            ids.push(',');
                        }
                        use std::fmt::Write as _;
                        let _ = write!(ids, "{stub_id}");
                    }
                    if let Ok(fetched) =
                        self.api::<Vec<SoundCloudTrack>>(&format!("/tracks?ids={ids}"))
                    {
                        for item in fetched {
                            hydrated.insert(item.id, item);
                        }
                    }
                }
                let tracks: Vec<Track> = raw_entries
                    .into_iter()
                    .filter_map(|entry| {
                        let full = if entry.title.is_empty() {
                            hydrated.remove(&entry.id)?
                        } else {
                            entry
                        };
                        self.track(full)
                    })
                    .collect();
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(name, -1, tracks))))
            }
            kind => {
                Err(SourceError::Url(format!("{id} resolved to a {kind}, which cannot be played")))
            }
        }
    }

    /// Resolves a track's direct audio URL.
    pub fn media_url(&self, id: &str) -> Result<(String, BTreeMap<String, String>), SourceError> {
        let key = soundcloud_key(id);
        if key.is_empty() && (id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit())) {
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
        // A full track/resolve response already carries transcodings. Fetch a
        // second time only when resolve returned an abbreviated track object.
        let track = if !key.is_empty() && answer.pointer("/media/transcodings").is_none() {
            self.api::<Value>(&format!("/tracks/{track_id}"))?
        } else {
            answer
        };
        let transcodings = track
            .pointer("/media/transcodings")
            .and_then(Value::as_array)
            .ok_or_else(|| SourceError::Http("soundcloud offered no stream".into()))?;
        // A progressive stream is one request; an HLS stream is a playlist of
        // them, which the node would have to assemble.
        let api_origin = url::Url::parse(self.config.soundcloud_api_endpoint.trim()).ok();
        let valid_transcoding = |raw: &str| {
            crate::http_url(raw).is_some_and(|parsed| {
                parsed.host_str().is_some_and(|host| {
                    crate::host_matches(host, "soundcloud.com")
                        || crate::host_matches(host, "sndcdn.com")
                        || api_origin.as_ref().is_some_and(|b| b.origin() == parsed.origin())
                })
            })
        };
        let mut fallback_url: Option<&str> = None;
        let mut chosen_url: Option<&str> = None;
        for entry in transcodings {
            if entry.pointer("/format/protocol").and_then(Value::as_str) != Some("progressive") {
                continue;
            }
            let Some(candidate_url) = entry
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty() && valid_transcoding(u))
            else {
                continue;
            };
            if !entry.get("snipped").and_then(Value::as_bool).unwrap_or(false) {
                chosen_url = Some(candidate_url);
                break;
            }
            if fallback_url.is_none() {
                fallback_url = Some(candidate_url);
            }
        }
        let url = chosen_url.or(fallback_url).ok_or_else(|| {
            SourceError::Http(
                "soundcloud offered no progressive audio stream; HLS playlists are not supported"
                    .into(),
            )
        })?;
        // The transcoding endpoint is a complete URL, not a path to append to
        // the catalogue origin. It passes the same network guard as all media.
        netguard::validate_url(url)?;
        let auth_param = track
            .get("track_authorization")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && !url.contains("track_authorization="));
        let separator = if url.contains('?') { '&' } else { '?' };
        #[derive(Deserialize)]
        struct StreamUrl {
            url: Option<String>,
        }
        let build_stream_req = |cid: &str| match auth_param {
            Some(auth) => format!(
                "{url}{separator}client_id={}&track_authorization={}",
                encode_query(cid),
                encode_query(auth)
            ),
            None => format!("{url}{separator}client_id={}", encode_query(cid)),
        };
        let client = self.client_id()?;
        let resolved: StreamUrl = match get_json(self.agent, &build_stream_req(&client), None) {
            Err(SourceError::Media { status: 401 | 403 })
                if self.config.soundcloud_client_id.trim().is_empty() =>
            {
                self.client_id.invalidate_if(&client);
                let fresh = self.client_id()?;
                get_json(self.agent, &build_stream_req(&fresh), None)?
            }
            other => other?,
        };
        let direct = resolved
            .url
            .filter(|url| !url.is_empty())
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
        let info = TrackInfo {
            identifier: id,
            title: entry.title,
            author: entry
                .user
                .map(|user| user.username)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration.max(0),
            is_seekable: true,
            uri,
            artwork_url: entry.artwork_url,
            source_name: "soundcloud".into(),
            ..Default::default()
        };
        encode_track_owned(info).ok()
    }
}

/// Turns a SoundCloud identifier or URL into the page URL the API resolves.
fn soundcloud_key(id: &str) -> String {
    let trimmed = crate::identifier_text(id);
    if let Some(mut url) = crate::http_url(trimmed) {
        if url.host_str().is_some_and(|host| crate::host_matches(host, "soundcloud.com")) {
            if url.host_str().is_some_and(|host| host.eq_ignore_ascii_case("m.soundcloud.com")) {
                let _ = url.set_host(Some("soundcloud.com"));
            }
            url.set_fragment(None);
            return url.into();
        }
        return String::new();
    }
    if let Some(rest) = trimmed.strip_prefix("soundcloud:") {
        let rest = rest.trim().trim_start_matches('/');
        if !rest.is_empty()
            && !rest.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\' || b == b'@')
            && !rest.split('/').any(|seg| seg == "..")
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
        let candidate = tag
            .find("src=\"")
            .and_then(|at| {
                let rest = &tag[at + 5..];
                Some(&rest[..rest.find('"')?])
            })
            .or_else(|| {
                let at = tag.find("src='")?;
                let rest = &tag[at + 5..];
                Some(&rest[..rest.find('\'')?])
            });
        let Some(candidate) = candidate else { continue };
        let resolved = if candidate.starts_with("http://") || candidate.starts_with("https://") {
            url::Url::parse(candidate).ok()
        } else {
            base_origin.as_ref().and_then(|b| b.join(candidate).ok())
        };
        let Some(parsed) = resolved else { continue };
        if !parsed.path().ends_with(".js") {
            continue;
        }
        let allowed = parsed.host_str().is_some_and(|host| {
            crate::host_matches(host, "sndcdn.com")
                || crate::host_matches(host, "soundcloud.com")
                || base_origin.as_ref().is_some_and(|b| b.origin() == parsed.origin())
        });
        if allowed && netguard::validate_url(parsed.as_str()).is_ok() {
            urls.push(String::from(parsed));
            if urls.len() >= 10 {
                break;
            }
        }
    }
    urls
}

/// Finds `client_id:"..."` in a page of JavaScript.
///
/// The id is 32 alphanumeric characters and the page states it in more than one
/// place, so the first value that looks like one is used.
fn scrape_client_id(page: &str) -> Option<String> {
    let mut rest = page;
    while let Some(pos) = rest.find("client_id") {
        rest = &rest[pos + "client_id".len()..];
        let trimmed = rest.trim_start_matches(['"', '\'', ' ', '\t']);
        let Some(after_sep) = trimmed.strip_prefix([':', '=']) else { continue };
        let candidate = after_sep.trim_start_matches([' ', '\t', '"', '\'']);
        let bytes = candidate.as_bytes();
        if bytes.get(..32).is_some_and(|id| id.iter().all(u8::is_ascii_alphanumeric))
            && !bytes.get(32).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Some(candidate[..32].to_owned());
        }
    }
    None
}

/// Percent-encodes a query value the way a form submission does.
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
        let mut config = Config::default();
        config.spotify_client_id = "id".into();
        config.spotify_client_secret = "secret".into();
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
        let mut config = Config::default();
        config.spotify_client_id = "id".into();
        config.spotify_client_secret = "secret".into();
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
        assert_eq!(track.metadata.length, 12345);
        assert_eq!(track.metadata.isrc.as_deref(), Some("CODE"));
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
        let agent = netguard::agent(true);
        let result = SoundCloud::new(&agent, &config).media_url("42");
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
        let agent = netguard::agent(true);
        let result = SoundCloud::new(&agent, &config).media_url("42");
        server.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("HLS"));
    }

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn a_spotify_track_id_splits_into_its_kind_and_key() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let (kind, key) = spotify.parse("spotify:track:4cOdK2wGLETKBW3PvgPWqT").unwrap();
        assert_eq!((kind, key.as_str()), ("track", "4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn a_shared_spotify_link_splits_the_same_way() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let (kind, key) =
            spotify.parse("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc").unwrap();
        assert_eq!((kind, key.as_str()), ("track", "4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn a_spotify_link_with_a_locale_still_splits() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let (kind, key) =
            spotify.parse("https://open.spotify.com/intl-ja/album/1ATL5GLyefJaxhQzSPVrLX").unwrap();
        assert_eq!((kind, key.as_str()), ("album", "1ATL5GLyefJaxhQzSPVrLX"));
    }

    #[test]
    fn spotify_shared_embedded_and_fragment_links_keep_only_the_resource_id() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
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
        let agent = netguard::agent(true);
        let mut config = config();
        config.spotify_client_id = "id".into();
        config.spotify_client_secret = "secret".into();
        let endpoint = format!("{base}/v1");
        let spotify = Spotify { api_endpoint: &endpoint, ..Spotify::new(&agent, &config) };
        spotify
            .token
            .get_or_refresh(|| Ok(("test-token".into(), Duration::from_secs(60))))
            .unwrap();
        for kind in ["track", "album", "playlist"] {
            let result = spotify.load(&format!("https://open.spotify.com/intl-zh-TW/{kind}/4cOdK2wGLETKBW3PvgPWqT?si=share#fragment")).unwrap();
            let tracks = match &result {
                LoadResult::Track(track) => std::slice::from_ref(track.as_ref()),
                LoadResult::Playlist(playlist) => playlist.tracks.as_slice(),
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
        let agent = netguard::agent(true);
        let mut config = config();
        config.spotify_client_id = "id".into();
        config.spotify_client_secret = "secret".into();
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
        let agent = netguard::agent(false);
        let deezer = Deezer { agent: &agent, config: &config() };
        let (kind, key) = deezer.parse("deezer:track:3135556").unwrap();
        assert_eq!((kind, key.as_str()), ("track", "3135556"));
    }

    #[test]
    fn deezer_links_with_any_locale_parse_and_keys_must_be_numeric() {
        let agent = netguard::agent(false);
        let deezer = Deezer { agent: &agent, config: &config() };
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
        let mut config = config();
        config.deezer_api_endpoint = base;
        let agent = netguard::agent(true);
        let result = Deezer { agent: &agent, config: &config }.paged("/playlist/1/tracks");
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Url(_))));
    }

    #[test]
    fn a_spotify_entry_becomes_a_track_that_names_its_isrc() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let entry = SpotifyTrack {
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
        };
        let track = spotify.track(entry).unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.author, "Someone");
        assert_eq!(track.metadata.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(track.metadata.artwork_url.as_deref(), Some("large"));
        assert_eq!(track.metadata.uri.as_deref(), Some("https://open.spotify.com/track/abc"));
    }

    #[test]
    fn a_local_spotify_file_is_not_a_track() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let entry = SpotifyTrack {
            id: "abc".into(),
            name: "A Song".into(),
            is_local: true,
            ..Default::default()
        };
        assert!(spotify.track(entry).is_none());
    }

    #[test]
    fn a_deezer_entry_becomes_a_track() {
        let agent = netguard::agent(false);
        let deezer = Deezer { agent: &agent, config: &config() };
        let entry = DeezerTrack {
            id: 3135556,
            title: "A Song".into(),
            duration: 200,
            isrc: "ISRC123".into(),
            artist: Some(DeezerArtist { name: "Someone".into() }),
            album: Some(DeezerAlbum { cover_xl: "large".into(), cover_big: String::new() }),
        };
        let track = deezer.track(entry).unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.length, 200_000);
        assert_eq!(track.metadata.artwork_url.as_deref(), Some("large"));
    }

    #[test]
    fn a_soundcloud_entry_becomes_a_track() {
        let agent = netguard::agent(false);
        let config = config();
        let soundcloud = SoundCloud::new(&agent, &config);
        let entry = SoundCloudTrack {
            id: 42,
            title: "A Song".into(),
            duration: 200_000,
            permalink_url: "https://soundcloud.com/someone/a-song".into(),
            user: Some(SoundCloudUser { username: "Someone".into() }),
            streamable: true,
            artwork_url: None,
        };
        let track = soundcloud.track(entry).unwrap();
        assert_eq!(track.metadata.title, "A Song");
        assert_eq!(track.metadata.source_name, "soundcloud");
        assert_eq!(track.metadata.uri.as_deref(), Some("https://soundcloud.com/someone/a-song"));
    }

    #[test]
    fn a_soundcloud_track_that_cannot_be_streamed_is_not_a_track() {
        let agent = netguard::agent(false);
        let config = config();
        let soundcloud = SoundCloud::new(&agent, &config);
        let entry = SoundCloudTrack {
            id: 42,
            title: "A Song".into(),
            streamable: false,
            ..Default::default()
        };
        assert!(soundcloud.track(entry).is_none());
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
                    "items": [{
                        "id": "4cOdK2wGLETKBW3PvgPWqT",
                        "name": "Album Track",
                        "duration_ms": 180000,
                        "artists": [{"name": "Artist"}]
                    }],
                    "total": 1,
                    "next": null
                }
            })
        });
        let agent = netguard::agent(true);
        let mut sp_config = config();
        sp_config.spotify_client_id = "id".into();
        sp_config.spotify_client_secret = "secret".into();
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
                "tracks": {
                    "data": [{
                        "id": 3135556,
                        "title": "One More Time",
                        "duration": 320,
                        "artist": {"name": "Daft Punk"}
                    }]
                }
            })
        });
        let mut dz_config = config();
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
        let mut cfg = config();
        cfg.soundcloud_api_endpoint = base;
        cfg.soundcloud_client_id = "configured".into();
        let agent = netguard::agent(true);
        let err = SoundCloud::new(&agent, &cfg).media_url("42").unwrap_err();
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
