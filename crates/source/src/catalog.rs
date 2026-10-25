//! The metadata catalogues.
//!
//! Spotify and Deezer know what a track is called but do not serve it, so a
//! track loaded from either is a description whose audio is mirrored from a
//! YouTube search for the same name. SoundCloud serves its own audio, and is
//! resolved natively when the configuration asks for it.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rustlink_config::Config;
use rustlink_protocol::{encode_track, LoadResult, Playlist, Track, TrackInfo};
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
}

#[derive(Default)]
struct CredentialCache {
    held: Mutex<Credential>,
    ready: Condvar,
}

#[derive(Default)]
struct Credential {
    value: Option<(String, Instant)>,
    refreshing: bool,
}

impl CredentialCache {
    fn get_or_refresh(
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
        struct Refresh<'a>(&'a CredentialCache);
        impl Drop for Refresh<'_> {
            fn drop(&mut self) {
                self.0.held.lock().unwrap_or_else(|error| error.into_inner()).refreshing = false;
                self.0.ready.notify_all();
            }
        }
        let _refresh = Refresh(self);
        let (value, ttl) = fetch()?;
        self.held.lock().unwrap_or_else(|error| error.into_inner()).value =
            Some((value.clone(), Instant::now() + ttl));
        Ok(value)
    }
}

/// Reads a JSON answer from a catalogue.
fn get_json(agent: &ureq::Agent, url: &str, bearer: Option<&str>) -> Result<Value, SourceError> {
    netguard::validate_url(url)?;
    let mut request = agent.get(url).header("Accept", "application/json");
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
    let raw = response
        .into_body()
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
    netguard::validate_url(url)?;
    let response = agent
        .get(url)
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
    response
        .into_body()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|error| SourceError::Http(error.to_string()))
}

/// Splits an identifier of the form `prefix:kind:id` into its parts.
fn parts_after(id: &str, prefix: &str) -> Vec<String> {
    id.trim_start_matches(prefix)
        .split(':')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Spotify
// ---------------------------------------------------------------------------

/// Spotify's catalogue, which needs an account to answer at all.
pub struct Spotify<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
    token: Arc<CredentialCache>,
}

impl<'a> Spotify<'a> {
    pub fn new(agent: &'a ureq::Agent, config: &'a Config) -> Self {
        Self::with_state(agent, config, &State::default())
    }

    pub fn with_state(agent: &'a ureq::Agent, config: &'a Config, state: &State) -> Self {
        Spotify { agent, config, token: Arc::clone(&state.spotify_token) }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyArtist {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyImage {
    url: String,
    width: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyTrack {
    id: String,
    name: String,
    duration_ms: i64,
    artists: Vec<SpotifyArtist>,
    album: Option<SpotifyAlbum>,
    external_ids: Option<SpotifyExternalIds>,
    is_local: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyAlbum {
    images: Vec<SpotifyImage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SpotifyExternalIds {
    isrc: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpotifyPage {
    items: Vec<Value>,
    next: Option<String>,
    total: u64,
}

impl Spotify<'_> {
    /// Whether the node has the credentials to ask at all.
    pub fn enabled(&self) -> bool {
        !self.config.spotify_client_id.is_empty() && !self.config.spotify_client_secret.is_empty()
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

    fn token_form(&self) -> Vec<(&'static str, String)> {
        let mut form = if self.config.spotify_refresh_token.is_empty() {
            vec![("grant_type", "client_credentials".to_string())]
        } else {
            vec![
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", self.config.spotify_refresh_token.clone()),
            ]
        };
        form.push(("client_id", self.config.spotify_client_id.clone()));
        form.push(("client_secret", self.config.spotify_client_secret.clone()));
        form
    }

    fn fetch_token(&self) -> Result<(String, Duration), SourceError> {
        let response = self
            .agent
            .post("https://accounts.spotify.com/api/token")
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
        let value: Value = serde_json::from_slice(&raw).map_err(|error| {
            SourceError::Http(format!("spotify sent an unreadable answer: {error}"))
        })?;
        let token = value
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Http("spotify sent no token".into()))?
            .to_string();
        let expires = value.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).min(86_400);
        let ttl = Duration::from_secs(expires.saturating_sub(30.min(expires / 10)));
        Ok((token, ttl))
    }

    fn api(&self, path: &str) -> Result<Value, SourceError> {
        let token = self.token()?;
        let url = format!("https://api.spotify.com/v1{path}");
        get_json(self.agent, &url, Some(&token))
    }

    /// Loads whatever a Spotify identifier names.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("spsearch:") {
            let limit = self.config.search_results.min(10);
            let path = format!(
                "/search?type=track&limit={limit}&market={}&q={}",
                self.config.spotify_market,
                encode_query(query.trim())
            );
            let answer = self.api(&path)?;
            let tracks: Vec<Track> = answer
                .pointer("/tracks/items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            serde_json::from_value::<SpotifyTrack>(item.clone()).ok()
                        })
                        .filter_map(|track| self.track(&track))
                        .collect()
                })
                .unwrap_or_default();
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
        }
        let (kind, key) = self.parse(id)?;
        match kind.as_str() {
            "track" => {
                let track: SpotifyTrack =
                    serde_json::from_value(self.api(&format!("/tracks/{key}"))?)
                        .map_err(|error| SourceError::Http(error.to_string()))?;
                let track = self.track(&track).ok_or_else(|| {
                    SourceError::Http("spotify described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "album" => {
                let album = self.api(&format!("/albums/{key}"))?;
                let name =
                    album.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let tracks = self.page(&format!("/albums/{key}/tracks"))?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(name, -1, tracks))))
            }
            "playlist" => {
                // A Spotify editorial playlist cannot be read without a user's
                // token, and the failure a client gets otherwise is a confusing
                // one, so it is named here.
                if key.starts_with("37i9dQZ") && self.config.spotify_refresh_token.is_empty() {
                    return Err(SourceError::Disabled(
                        "spotify editorial playlists need spotifyRefreshToken".into(),
                    ));
                }
                let playlist = self.api(&format!("/playlists/{key}"))?;
                let name =
                    playlist.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let tracks = self.page(&format!("/playlists/{key}/tracks"))?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(name, -1, tracks))))
            }
            other => {
                Err(SourceError::Url(format!("spotify {other} is not a kind this node loads")))
            }
        }
    }

    /// Reads a paged collection, following the next link until the node has
    /// enough tracks.
    fn page(&self, path: &str) -> Result<Vec<Track>, SourceError> {
        let mut tracks = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(path) = next.take() {
            let answer = if path.starts_with("http") {
                let token = self.token()?;
                get_json(self.agent, &path, Some(&token))?
            } else {
                self.api(&path)?
            };
            let page: SpotifyPage = serde_json::from_value(answer)
                .map_err(|error| SourceError::Http(error.to_string()))?;
            for item in &page.items {
                // A playlist entry wraps its track, and a local file has no id
                // to play.
                let value = item.get("track").cloned().unwrap_or_else(|| item.clone());
                if let Ok(track) = serde_json::from_value::<SpotifyTrack>(value) {
                    if let Some(track) = self.track(&track) {
                        tracks.push(track);
                    }
                }
                if tracks.len() >= self.config.max_playlist_tracks as usize {
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
    fn parse(&self, id: &str) -> Result<(String, String), SourceError> {
        if let Some(rest) = id.strip_prefix("spotify:") {
            let parts = parts_after(rest, "");
            if parts.len() >= 2 {
                return Ok((parts[0].clone(), parts[1].clone()));
            }
        }
        if let Some(rest) = id.strip_prefix("https://open.spotify.com/") {
            // A shared link may carry a locale in front of the kind.
            let parts: Vec<&str> = rest.split(['/', '?']).filter(|part| !part.is_empty()).collect();
            let parts: Vec<&str> =
                parts.into_iter().filter(|part| !part.starts_with("intl-")).collect();
            if parts.len() >= 2 {
                return Ok((parts[0].to_string(), parts[1].to_string()));
            }
        }
        Err(SourceError::Url(format!("{id} is not a Spotify track, album or playlist")))
    }

    /// Turns a catalogue entry into a track.
    fn track(&self, entry: &SpotifyTrack) -> Option<Track> {
        if entry.is_local || entry.id.is_empty() {
            return None;
        }
        let author = entry
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let artwork = entry
            .album
            .as_ref()
            .and_then(|album| album.images.iter().max_by_key(|image| image.width))
            .map(|image| image.url.clone())
            .filter(|url| !url.is_empty());
        let isrc =
            entry.external_ids.as_ref().map(|ids| ids.isrc.clone()).filter(|isrc| !isrc.is_empty());
        let info = TrackInfo {
            identifier: entry.id.clone(),
            title: entry.name.clone(),
            author: if author.is_empty() { "Unknown artist".into() } else { author },
            length: entry.duration_ms,
            is_seekable: true,
            uri: Some(format!("https://open.spotify.com/track/{}", entry.id)),
            artwork_url: artwork,
            isrc,
            source_name: "spotify".into(),
            ..Default::default()
        };
        Some(encode_track(&info).unwrap_or_else(|_| Track::new(String::new(), info)))
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
    id: u64,
    title: String,
    duration: i64,
    isrc: String,
    artist: Option<DeezerArtist>,
    album: Option<DeezerAlbum>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerArtist {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerAlbum {
    cover_xl: String,
    cover_big: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeezerList {
    data: Vec<Value>,
}

impl Deezer<'_> {
    fn api(&self, path: &str) -> Result<Value, SourceError> {
        let url = format!("{}{path}", self.config.deezer_api_endpoint.trim_end_matches('/'));
        get_json(self.agent, &url, None)
    }

    /// Loads whatever a Deezer identifier names.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("dzsearch:") {
            let limit = self.config.search_results.min(25);
            let answer =
                self.api(&format!("/search/track?limit={limit}&q={}", encode_query(query.trim())))?;
            let tracks = self.list_tracks(answer)?;
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
        }
        if let Some(isrc) = id.strip_prefix("dzisrc:") {
            let answer = self.api(&format!("/track/isrc:{}", encode_query(isrc.trim())))?;
            if answer.get("error").is_some() {
                return Ok(LoadResult::Empty);
            }
            let entry: DeezerTrack = serde_json::from_value(answer)
                .map_err(|error| SourceError::Http(error.to_string()))?;
            let track = self.track(&entry).ok_or_else(|| {
                SourceError::Http("deezer described a track that cannot be played".into())
            })?;
            return Ok(LoadResult::Track(Box::new(track)));
        }
        let (kind, key) = self.parse(id)?;
        match kind.as_str() {
            "track" => {
                let answer = self.api(&format!("/track/{key}"))?;
                if answer.get("error").is_some() {
                    return Ok(LoadResult::Empty);
                }
                let entry: DeezerTrack = serde_json::from_value(answer)
                    .map_err(|error| SourceError::Http(error.to_string()))?;
                let track = self.track(&entry).ok_or_else(|| {
                    SourceError::Http("deezer described a track that cannot be played".into())
                })?;
                Ok(LoadResult::Track(Box::new(track)))
            }
            "album" | "playlist" => {
                let answer = self.api(&format!("/{kind}/{key}"))?;
                if answer.get("error").is_some() {
                    return Ok(LoadResult::Empty);
                }
                let name =
                    answer.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
                let tracks = self.paged(&format!("/{kind}/{key}/tracks"))?;
                if tracks.is_empty() {
                    return Ok(LoadResult::Empty);
                }
                Ok(LoadResult::Playlist(Box::new(Playlist::new(name, -1, tracks))))
            }
            other => Err(SourceError::Url(format!("deezer {other} is not a kind this node loads"))),
        }
    }

    fn list_tracks(&self, answer: Value) -> Result<Vec<Track>, SourceError> {
        let list: DeezerList =
            serde_json::from_value(answer).map_err(|error| SourceError::Http(error.to_string()))?;
        Ok(list
            .data
            .iter()
            .filter_map(|item| serde_json::from_value::<DeezerTrack>(item.clone()).ok())
            .filter_map(|entry| self.track(&entry))
            .collect())
    }

    /// Reads a collection, following the next page the payload names.
    fn paged(&self, path: &str) -> Result<Vec<Track>, SourceError> {
        let mut tracks = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(path) = next.take() {
            let answer = if path.starts_with("http") {
                netguard::validate_url(&path)?;
                get_json(self.agent, &path, None)?
            } else {
                self.api(&path)?
            };
            next = answer.get("next").and_then(Value::as_str).map(str::to_string);
            tracks.extend(self.list_tracks(answer)?);
            pages += 1;
            if tracks.len() >= self.config.max_playlist_tracks as usize || pages >= MAX_PAGES {
                break;
            }
        }
        tracks.truncate(self.config.max_playlist_tracks as usize);
        Ok(tracks)
    }

    fn parse(&self, id: &str) -> Result<(String, String), SourceError> {
        if let Some(rest) = id.strip_prefix("deezer:") {
            let parts = parts_after(rest, "");
            if parts.len() >= 2 {
                return Ok((parts[0].clone(), parts[1].clone()));
            }
        }
        if let Some(rest) = id.strip_prefix("https://www.deezer.com/") {
            let parts: Vec<&str> = rest.split(['/', '?']).filter(|part| !part.is_empty()).collect();
            let parts: Vec<&str> = parts.into_iter().filter(|part| *part != "en").collect();
            if parts.len() >= 2 {
                return Ok((parts[0].to_string(), parts[1].to_string()));
            }
        }
        Err(SourceError::Url(format!("{id} is not a Deezer track, album or playlist")))
    }

    fn track(&self, entry: &DeezerTrack) -> Option<Track> {
        if entry.id == 0 || entry.title.is_empty() {
            return None;
        }
        let id = entry.id.to_string();
        let artwork = entry
            .album
            .as_ref()
            .map(|album| {
                if !album.cover_xl.is_empty() {
                    album.cover_xl.clone()
                } else {
                    album.cover_big.clone()
                }
            })
            .filter(|url| !url.is_empty());
        let info = TrackInfo {
            identifier: id.clone(),
            title: entry.title.clone(),
            author: entry
                .artist
                .as_ref()
                .map(|artist| artist.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration.saturating_mul(1000),
            is_seekable: true,
            uri: Some(format!("https://www.deezer.com/track/{id}")),
            artwork_url: artwork,
            isrc: (!entry.isrc.is_empty()).then(|| entry.isrc.clone()),
            source_name: "deezer".into(),
            ..Default::default()
        };
        Some(encode_track(&info).unwrap_or_else(|_| Track::new(String::new(), info)))
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
    username: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundCloudTrack {
    id: u64,
    title: String,
    duration: i64,
    permalink_url: String,
    artwork_url: Option<String>,
    user: Option<SoundCloudUser>,
    streamable: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SoundCloudList {
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
            let id = scrape_client_id(&page).ok_or_else(|| {
                SourceError::Http("soundcloud no longer publishes a client id".into())
            })?;
            Ok((id, self.config.soundcloud_client_id_ttl()))
        })
    }

    fn api(&self, path: &str) -> Result<Value, SourceError> {
        let client = self.client_id()?;
        let base = self.config.soundcloud_api_endpoint.trim_end_matches('/');
        let separator = if path.contains('?') { '&' } else { '?' };
        let url = format!("{base}{path}{separator}client_id={}", encode_query(&client));
        get_json(self.agent, &url, None)
    }

    /// Loads a SoundCloud search, track or playlist.
    pub fn load(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(query) = id.strip_prefix("scsearch:") {
            let limit = self.config.search_results.min(20);
            let answer = self
                .api(&format!("/search/tracks?q={}&limit={limit}", encode_query(query.trim())))?;
            let list: SoundCloudList = serde_json::from_value(answer)
                .map_err(|error| SourceError::Http(error.to_string()))?;
            let tracks: Vec<Track> =
                list.collection.iter().filter_map(|entry| self.track(entry)).collect();
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Search(tracks));
        }
        let key = soundcloud_key(id);
        if key.is_empty() {
            return Err(SourceError::Url(format!("{id} is not a SoundCloud track or playlist")));
        }
        let answer = self.api(&format!("/resolve?url={}", encode_query(&key)))?;
        let kind = answer.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
        if kind == "track" {
            let entry: SoundCloudTrack = serde_json::from_value(answer)
                .map_err(|error| SourceError::Http(error.to_string()))?;
            let track = self.track(&entry).ok_or_else(|| {
                SourceError::Http("soundcloud described a track that cannot be played".into())
            })?;
            return Ok(LoadResult::Track(Box::new(track)));
        }
        if kind == "playlist" {
            let name = answer.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
            let tracks: Vec<Track> = answer
                .get("tracks")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            serde_json::from_value::<SoundCloudTrack>(entry.clone()).ok()
                        })
                        .filter_map(|entry| self.track(&entry))
                        .take(self.config.max_playlist_tracks as usize)
                        .collect()
                })
                .unwrap_or_default();
            if tracks.is_empty() {
                return Ok(LoadResult::Empty);
            }
            return Ok(LoadResult::Playlist(Box::new(Playlist::new(name, -1, tracks))));
        }
        Err(SourceError::Url(format!("{id} resolved to a {kind}, which cannot be played")))
    }

    /// Resolves a track's direct audio URL.
    pub fn media_url(&self, id: &str) -> Result<(String, BTreeMap<String, String>), SourceError> {
        let key = soundcloud_key(id);
        if key.is_empty() && (id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit())) {
            return Err(SourceError::Url("invalid SoundCloud track identifier".into()));
        }
        let answer = if key.is_empty() {
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
            self.api(&format!("/tracks/{track_id}"))?
        } else {
            answer
        };
        let transcodings = track
            .pointer("/media/transcodings")
            .and_then(Value::as_array)
            .ok_or_else(|| SourceError::Http("soundcloud offered no stream".into()))?;
        // A progressive stream is one request; an HLS stream is a playlist of
        // them, which the node would have to assemble.
        let chosen = transcodings
            .iter()
            .find(|entry| {
                entry.pointer("/format/protocol").and_then(Value::as_str) == Some("progressive")
            })
            .ok_or_else(|| SourceError::Http(
                "soundcloud offered no progressive audio stream; HLS playlists are not supported".into()
            ))?;
        let url = chosen
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Http("soundcloud offered no stream url".into()))?;
        // The transcoding endpoint is a complete URL, not a path to append to
        // the catalogue origin. It passes the same network guard as all media.
        netguard::validate_url(url)?;
        let separator = if url.contains('?') { '&' } else { '?' };
        let client = self.client_id()?;
        let resolved = get_json(
            self.agent,
            &format!("{url}{separator}client_id={}", encode_query(&client)),
            None,
        )?;
        let direct = resolved
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Http("soundcloud resolved to no stream".into()))?;
        Ok((direct.to_string(), BTreeMap::new()))
    }

    fn track(&self, entry: &SoundCloudTrack) -> Option<Track> {
        if entry.id == 0 || entry.title.is_empty() || !entry.streamable {
            return None;
        }
        let id = entry.id.to_string();
        let info = TrackInfo {
            identifier: id.clone(),
            title: entry.title.clone(),
            author: entry
                .user
                .as_ref()
                .map(|user| user.username.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unknown artist".into()),
            length: entry.duration,
            is_seekable: true,
            uri: Some(if entry.permalink_url.is_empty() {
                format!("https://soundcloud.com/tracks/{id}")
            } else {
                entry.permalink_url.clone()
            }),
            artwork_url: entry.artwork_url.clone(),
            source_name: "soundcloud".into(),
            ..Default::default()
        };
        Some(encode_track(&info).unwrap_or_else(|_| Track::new(String::new(), info)))
    }
}

/// Turns a SoundCloud identifier or URL into the page URL the API resolves.
fn soundcloud_key(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("soundcloud:") {
        return format!("https://soundcloud.com/{rest}");
    }
    String::new()
}

/// Finds `client_id:"..."` in a page of JavaScript.
///
/// The id is 32 alphanumeric characters and the page states it in more than one
/// place, so the first value that looks like one is used.
fn scrape_client_id(page: &str) -> Option<String> {
    for marker in ["client_id:\"", "client_id=\"", "client_id\":\""] {
        let mut at = 0usize;
        while let Some(found) = page[at..].find(marker) {
            let start = at + found + marker.len();
            let rest = &page[start..];
            let end = rest
                .find(|character: char| !character.is_ascii_alphanumeric())
                .unwrap_or(rest.len());
            let candidate = &rest[..end];
            if candidate.len() == 32 && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some(candidate.to_string());
            }
            at = start;
        }
    }
    None
}

/// Percent-encodes a query value the way a form submission does.
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
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
        let config = Config {
            spotify_client_id: "id".into(),
            spotify_client_secret: "secret".into(),
            ..Config::default()
        };
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
        let config = Config {
            spotify_client_id: "id".into(),
            spotify_client_secret: "secret".into(),
            spotify_refresh_token: "refresh".into(),
            ..Config::default()
        };
        let spotify = Spotify::new(&agent, &config);
        let form = spotify.token_form();
        assert!(form.contains(&("client_id", "id".into())));
        assert!(form.contains(&("client_secret", "secret".into())));
        assert!(form.contains(&("refresh_token", "refresh".into())));
        let entry: SpotifyTrack = serde_json::from_value(serde_json::json!({
            "id":"track", "name":"name", "duration_ms":12345, "is_local":false,
            "external_ids":{"isrc":"CODE"}
        }))
        .unwrap();
        let track = spotify.track(&entry).unwrap();
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
        let config = Config {
            soundcloud_api_endpoint: base,
            soundcloud_client_id: "configured".into(),
            ..Config::default()
        };
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
        let config = Config {
            soundcloud_api_endpoint: base,
            soundcloud_client_id: "id".into(),
            ..Config::default()
        };
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
        assert_eq!((kind.as_str(), key.as_str()), ("track", "4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn a_shared_spotify_link_splits_the_same_way() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let (kind, key) =
            spotify.parse("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc").unwrap();
        assert_eq!((kind.as_str(), key.as_str()), ("track", "4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn a_spotify_link_with_a_locale_still_splits() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let (kind, key) =
            spotify.parse("https://open.spotify.com/intl-ja/album/1ATL5GLyefJaxhQzSPVrLX").unwrap();
        assert_eq!((kind.as_str(), key.as_str()), ("album", "1ATL5GLyefJaxhQzSPVrLX"));
    }

    #[test]
    fn a_deezer_identifier_splits_into_its_kind_and_key() {
        let agent = netguard::agent(false);
        let deezer = Deezer { agent: &agent, config: &config() };
        let (kind, key) = deezer.parse("deezer:track:3135556").unwrap();
        assert_eq!((kind.as_str(), key.as_str()), ("track", "3135556"));
    }

    #[test]
    fn a_spotify_entry_becomes_a_track_that_names_its_isrc() {
        let agent = netguard::agent(false);
        let config = config();
        let spotify = Spotify::new(&agent, &config);
        let entry = SpotifyTrack {
            id: "abc".into(),
            name: "A Song".into(),
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
        let track = spotify.track(&entry).unwrap();
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
        assert!(spotify.track(&entry).is_none());
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
        let track = deezer.track(&entry).unwrap();
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
        let track = soundcloud.track(&entry).unwrap();
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
        assert!(soundcloud.track(&entry).is_none());
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
}
