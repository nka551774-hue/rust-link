//! Native YouTube catalogue and media resolution over JSON HTTP endpoints.
//! Client identities are tried in order when a provider rejects one; failures
//! report the native API error without launching an external extractor.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
};

use rustlink_config::Config;
use rustlink_protocol::{encode_track_owned, LoadResult, Playlist, Track, TrackInfo};
use serde::Serialize;
use serde_json::{json, Value};

use crate::catalog::{
    youtube_stream_ttl, CachedYoutubeStream, State as CatalogState, YoutubeStreamKey,
};
use crate::{deadline, netguard, youtube_id, SourceError};

/// One of the client identities YouTube answers.
///
/// The order matters: it is the order the clients are tried in, and the first
/// one that answers without demanding a proof of origin is the one that resolves
/// a track. It was set against the live API rather than from documentation,
/// because which clients are accepted changes often.
#[derive(Debug, Clone)]
pub struct Client {
    pub name: &'static str,
    pub version: Cow<'static, str>,
    pub user_agent: Cow<'static, str>,
    pub sdk: Option<u32>,
    pub device_make: Option<&'static str>,
    pub device_model: Option<&'static str>,
    pub os_name: Option<&'static str>,
    pub os_version: Option<&'static str>,
    pub needs_visitor_data: bool,
    pub throttled_without_pot: bool,
}

fn android_client(config: &Config) -> Client {
    // This version still returns directly usable audio for videos newer Android
    // identities expose only through SABR. Keep the HTTP identity consistent
    // with a configured version override.
    let custom = config.youtube_client_version.trim();
    let (version, user_agent) = if custom.is_empty() {
        (
            Cow::Borrowed("20.10.38"),
            Cow::Borrowed("com.google.android.youtube/20.10.38 (Linux; U; Android 11) gzip"),
        )
    } else {
        (
            Cow::Owned(custom.to_owned()),
            Cow::Owned(format!("com.google.android.youtube/{custom} (Linux; U; Android 11) gzip")),
        )
    };
    Client {
        name: "ANDROID",
        version,
        user_agent,
        sdk: Some(30),
        device_make: None,
        device_model: None,
        os_name: Some("Android"),
        os_version: Some("11"),
        needs_visitor_data: false,
        throttled_without_pot: true,
    }
}

/// Apple visionOS Safari client: with a valid `visitorData` session token,
/// serves full-length WebM Opus (`itag 251`) and AAC-LC (`itag 140`) without
/// a proof-of-origin token even on addresses where `ANDROID_VR` is bot-blocked.
pub fn visionos_client() -> Client {
    Client {
        name: "VISIONOS",
        version: Cow::Borrowed("0.1"),
        user_agent: Cow::Borrowed("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15"),
        sdk: None,
        device_make: Some("Apple"),
        device_model: Some("RealityDevice14,1"),
        os_name: Some("visionOS"),
        os_version: Some("1.3.21O771"),
        needs_visitor_data: true,
        throttled_without_pot: false,
    }
}

/// The clients tried in order.
pub fn clients(config: &Config) -> [Client; 4] {
    [
        Client {
            name: "IOS_PRODUCER",
            // Serves whole files with no proof of origin and no visitor session,
            // even to an address the other identities answer with a bot check.
            // It offers AAC-LC (itag 140).
            version: Cow::Borrowed("0.147.945"),
            user_agent: Cow::Borrowed("com.google.ios.youtubeproducer/0.147.945 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)"),
            sdk: None,
            device_make: Some("Apple"),
            device_model: Some("iPhone16,2"),
            os_name: Some("iPhone"),
            os_version: Some("18.3.2.22D82"),
            needs_visitor_data: false,
            throttled_without_pot: false,
        },
        visionos_client(),
        Client {
            name: "ANDROID_VR",
            // Serves whole files with no proof of origin to an address YouTube
            // trusts; newer VR versions stop after ~900 KiB like the others.
            version: Cow::Borrowed("1.60.19"),
            user_agent: Cow::Borrowed("com.google.android.apps.youtube.vr.oculus/1.60.19 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip"),
            sdk: Some(32),
            device_make: None,
            device_model: None,
            os_name: None,
            os_version: None,
            needs_visitor_data: false,
            throttled_without_pot: false,
        },
        android_client(config),
    ]
}

/// The client searches and playlists are read with: Android answers them in
/// the shape the parsers expect, whichever client plays best right now.
fn catalogue_client(config: &Config) -> Client {
    android_client(config)
}

/// The client the catalogue is searched with. It needs no player script because
/// a search result carries a plain video id.
const MUSIC_CLIENT: Client = Client {
    name: "WEB_REMIX",
    version: Cow::Borrowed("1.20250101.01.00"),
    user_agent: Cow::Borrowed("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"),
    sdk: None,
    device_make: None,
    device_model: None,
    os_name: None,
    os_version: None,
    needs_visitor_data: false,
    throttled_without_pot: false,
};

/// Restricts a search to videos. Without it YouTube mixes in channels,
/// playlists and shorts, which have no length and are dropped anyway.
const SEARCH_PARAMS: &str = "EgIQAfABAQ==";

/// Restricts a YouTube Music search to songs.
const MUSIC_SONGS_PARAMS: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA==";

/// Bounds an API response; playlist and search payloads are large.
const MAX_BODY: u64 = 8 << 20;

/// How many pages of a playlist are read before stopping.
const MAX_PLAYLIST_PAGES: usize = 6;

/// One audio stream the player offers.
#[derive(Debug, Clone, Default)]
pub struct Format {
    pub url: String,
    pub mime_type: String,
    pub bitrate: i64,
    pub content_length: i64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientIdentity<'a> {
    client_name: &'a str,
    client_version: &'a str,
    hl: &'static str,
    gl: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    visitor_data: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    android_sdk_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_make: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_version: Option<&'a str>,
}

#[derive(Serialize)]
struct RequestContext<'a> {
    client: ClientIdentity<'a>,
}

#[derive(Serialize)]
struct RequestEnvelope<'a, B: Serialize + ?Sized> {
    context: RequestContext<'a>,
    #[serde(flatten)]
    body: &'a B,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IntegrityDimensions<'a> {
    po_token: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayerRequest<'a> {
    video_id: &'a str,
    content_check_ok: bool,
    racy_check_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_integrity_dimensions: Option<IntegrityDimensions<'a>>,
}

#[derive(Serialize)]
struct SearchRequest<'a> {
    query: &'a str,
    params: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowseRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    browse_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    video_id: Option<&'a str>,
    playlist_id: &'a str,
}

/// Posts one InnerTube request without cloning or mutating a `serde_json::Value` tree.
fn call<B: Serialize + ?Sized>(
    agent: &ureq::Agent,
    base: &str,
    client: &Client,
    visitor_data: &str,
    endpoint: &str,
    body: &B,
) -> Result<Value, SourceError> {
    let visitor = visitor_data.trim();
    let payload = RequestEnvelope {
        context: RequestContext {
            client: ClientIdentity {
                client_name: client.name,
                client_version: client.version.as_ref(),
                hl: "en",
                gl: "US",
                visitor_data: visitor,
                android_sdk_version: client.sdk,
                device_make: client.device_make,
                device_model: client.device_model,
                os_name: client.os_name,
                os_version: client.os_version,
            },
        },
        body,
    };
    let url = format!("{}/youtubei/v1/{endpoint}?prettyPrint=false", base.trim_end_matches('/'));
    netguard::validate_url(&url)?;
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", client.user_agent.as_ref())
        .header("Accept-Language", "en-US,en;q=0.9");
    if !visitor.is_empty() {
        request = request.header("X-Goog-Visitor-Id", visitor);
    }
    let response = request
        .config()
        .timeout_global(Some(deadline::timeout(std::time::Duration::from_secs(20))?))
        .http_status_as_error(false)
        .build()
        .send_json(&payload)
        .map_err(|error| SourceError::Http(format!("youtube could not be reached: {error}")))?;
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
    serde_json::from_slice(&raw)
        .map_err(|error| SourceError::Http(format!("youtube sent an unreadable answer: {error}")))
}

const VISITOR_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

fn is_https(url: &str) -> bool {
    url.trim().get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn fetch_sw_visitor_data(
    agent: &ureq::Agent,
    host_base: &str,
) -> Result<(String, std::time::Duration), SourceError> {
    let sw_url = format!("{}/sw.js_data", host_base.trim().trim_end_matches('/'));
    netguard::validate_url(&sw_url)?;
    let timeout = deadline::timeout(std::time::Duration::from_secs(10))?;
    let resp = agent
        .get(&sw_url)
        .header("User-Agent", MUSIC_CLIENT.user_agent.as_ref())
        .config()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| SourceError::Http(format!("youtube visitor bootstrap failed: {e}")))?;
    if !(200..300).contains(&resp.status().as_u16()) {
        return Err(SourceError::Media { status: resp.status().as_u16() });
    }
    let text = resp
        .into_body()
        .with_config()
        .limit(512 << 10)
        .read_to_string()
        .map_err(|e| SourceError::Http(e.to_string()))?;
    let trimmed = text.strip_prefix(")]}'\n").unwrap_or(&text);
    let val: Value = serde_json::from_str(trimmed)
        .map_err(|e| SourceError::Http(format!("unreadable sw.js_data: {e}")))?;
    let vd = val
        .pointer("/0/2/0/0/13")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|vd| !vd.is_empty())
        .ok_or_else(|| SourceError::Http("youtube visitorData missing".into()))?;
    Ok((vd.to_owned(), VISITOR_TTL))
}

fn fetch_visitor_data(
    agent: &ureq::Agent,
    base: &str,
    music_base: &str,
) -> Result<(String, std::time::Duration), SourceError> {
    let base = base.trim().trim_end_matches('/');
    let visitor_url = format!("{base}/youtubei/v1/visitor_id?prettyPrint=false");
    netguard::validate_url(&visitor_url)?;
    let timeout = deadline::timeout(std::time::Duration::from_secs(10))?;
    let body = json!({
        "context": {
            "client": {
                "clientName": "WEB",
                "clientVersion": "2.20250101.00.00",
                "hl": "en",
                "gl": "US",
            }
        }
    });
    if let Ok(resp) = agent
        .post(&visitor_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", MUSIC_CLIENT.user_agent.as_ref())
        .config()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .send_json(&body)
    {
        if (200..300).contains(&resp.status().as_u16()) {
            if let Ok(raw) = resp.into_body().with_config().limit(256 << 10).read_to_vec() {
                if let Ok(val) = serde_json::from_slice::<Value>(&raw) {
                    if let Some(vd) = val
                        .pointer("/responseContext/visitorData")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|vd| !vd.is_empty())
                    {
                        return Ok((vd.to_owned(), VISITOR_TTL));
                    }
                }
            }
        }
    }
    match fetch_sw_visitor_data(agent, base) {
        Ok(found) => Ok(found),
        Err(err) => {
            let music_base = music_base.trim().trim_end_matches('/');
            if !music_base.is_empty()
                && !music_base.eq_ignore_ascii_case(base)
                && (!is_https(base) || is_https(music_base))
            {
                fetch_sw_visitor_data(agent, music_base)
            } else {
                Err(err)
            }
        }
    }
}

fn probe_tail_byte(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    content_length: i64,
) -> Result<(), SourceError> {
    use std::io::Read as _;
    let last = content_length.saturating_sub(1).max(0);
    let _scope = deadline::enter(
        std::time::Instant::now() + deadline::timeout(std::time::Duration::from_secs(10))?,
    );
    let (mut body, _, partial) = crate::media::request(agent, url, headers, Some(last))?;
    if !partial {
        return Err(SourceError::Http("youtube tail probe did not return partial content".into()));
    }
    let mut byte = [0u8; 1];
    body.read(&mut byte)
        .map_err(|e| SourceError::Http(format!("youtube tail probe read failed: {e}")))?;
    Ok(())
}

/// A catalogue request, which carries the configuration a call needs.
pub struct Catalog<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
    state: Option<&'a CatalogState>,
}

const RECENTLY_RESOLVED_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

impl<'a> Catalog<'a> {
    pub fn new(agent: &'a ureq::Agent, config: &'a Config) -> Self {
        Self { agent, config, state: None }
    }

    pub fn with_state(agent: &'a ureq::Agent, config: &'a Config, state: &'a CatalogState) -> Self {
        Self { agent, config, state: Some(state) }
    }

    fn visitor_cache(&self) -> Option<&'a crate::catalog::CredentialCache> {
        self.state.map(|s| s.youtube_visitor.as_ref())
    }

    fn ordered_clients(&self) -> [Client; 4] {
        let base = clients(self.config);
        match self.state.filter(|_| is_https(&self.config.youtube_endpoint)) {
            Some(state) => state.youtube_health.partition_clients(base, |client| client.name),
            None => base,
        }
    }

    fn ordered_catalogue_clients(&self) -> [Client; 2] {
        let android = catalogue_client(self.config);
        let visionos = visionos_client();
        if let Some(state) = self.state.filter(|_| is_https(&self.config.youtube_endpoint)) {
            if !state.youtube_health.is_healthy(android.name)
                && state.youtube_health.is_healthy(visionos.name)
            {
                return [visionos, android];
            }
        }
        [android, visionos]
    }

    fn record_client_outcome(&self, name: &'static str, ok: bool) {
        let Some(state) = self.state.filter(|_| is_https(&self.config.youtube_endpoint)) else {
            return;
        };
        if ok {
            state.youtube_health.record_success(name);
        } else {
            state.youtube_health.record_failure(name);
        }
    }

    fn stream_key(&self, video_id: &str) -> YoutubeStreamKey {
        YoutubeStreamKey {
            video_id: video_id.to_owned(),
            endpoint: self.config.youtube_endpoint.trim().to_owned(),
            client_version: self.config.youtube_client_version.trim().to_owned(),
            has_po_token: !self.config.youtube_po_token.trim().is_empty(),
        }
    }

    fn store_cached_stream(
        &self,
        video_id: &str,
        client: &Client,
        format: Format,
        tail_probed: bool,
    ) {
        let Some(state) = self.state else {
            return;
        };
        let Some(ttl) = youtube_stream_ttl(&format.url) else {
            return;
        };
        state.youtube_streams.put(
            self.stream_key(video_id),
            CachedYoutubeStream {
                url: format.url,
                content_length: format.content_length,
                headers: format.headers,
                user_agent: client.user_agent.clone(),
                client_name: client.name,
                throttled_without_pot: client.throttled_without_pot,
                tail_probed,
            },
            ttl,
        );
    }

    fn effective_visitor_data(&self, bootstrap: bool) -> String {
        let configured = self.config.youtube_visitor_data.trim();
        if !configured.is_empty() {
            return configured.to_owned();
        }
        if let Some(cached) = self.visitor_cache().and_then(|c| c.peek()) {
            return cached;
        }
        if !bootstrap || !is_https(&self.config.youtube_endpoint) {
            return String::new();
        }
        match self.visitor_cache() {
            Some(cache) => cache
                .get_or_refresh(|| {
                    fetch_visitor_data(
                        self.agent,
                        &self.config.youtube_endpoint,
                        &self.config.youtube_music_endpoint,
                    )
                })
                .unwrap_or_default(),
            None => fetch_visitor_data(
                self.agent,
                &self.config.youtube_endpoint,
                &self.config.youtube_music_endpoint,
            )
            .map(|(vd, _)| vd)
            .unwrap_or_default(),
        }
    }

    fn harvest_visitor_data(&self, answer: &Value) {
        if !self.config.youtube_visitor_data.trim().is_empty() {
            return;
        }
        if let Some(cache) = self.visitor_cache() {
            if let Some(vd) = answer
                .pointer("/responseContext/visitorData")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|vd| !vd.is_empty())
            {
                cache.store_if_empty(vd, VISITOR_TTL);
            }
        }
    }

    fn invalidate_visitor_if_stale(&self, used_vd: &str, answer: &Value) {
        if used_vd.is_empty() || !self.config.youtube_visitor_data.trim().is_empty() {
            return;
        }
        if let Some(cache) = self.visitor_cache() {
            let reason = answer
                .pointer("/playabilityStatus/reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if reason.contains("not a bot")
                || reason.contains("reloaded")
                || reason.contains("sign in")
                || reason.contains("try again later")
            {
                cache.invalidate_if(used_vd);
            }
        }
    }

    fn web_call<B: Serialize + ?Sized>(
        &self,
        client: &Client,
        endpoint: &str,
        body: &B,
    ) -> Result<Value, SourceError> {
        let vd = self.effective_visitor_data(client.needs_visitor_data);
        let answer = call(self.agent, &self.config.youtube_endpoint, client, &vd, endpoint, body)?;
        self.harvest_visitor_data(&answer);
        Ok(answer)
    }

    fn player_request<'b>(&'b self, video_id: &'b str) -> PlayerRequest<'b> {
        let token = self.config.youtube_po_token.trim();
        PlayerRequest {
            video_id,
            content_check_ok: true,
            racy_check_ok: true,
            service_integrity_dimensions: (!token.is_empty())
                .then_some(IntegrityDimensions { po_token: token }),
        }
    }

    fn player_call(&self, client: &Client, video_id: &str) -> Result<Value, SourceError> {
        let req = self.player_request(video_id);
        let vd = self.effective_visitor_data(client.needs_visitor_data);
        let answer = call(self.agent, &self.config.youtube_endpoint, client, &vd, "player", &req)?;
        if let Err(err) = check_playability(&answer) {
            self.invalidate_visitor_if_stale(&vd, &answer);
            if client.needs_visitor_data
                && !vd.is_empty()
                && self.config.youtube_visitor_data.trim().is_empty()
                && self.visitor_cache().is_some()
                && is_https(&self.config.youtube_endpoint)
            {
                let fresh = self.effective_visitor_data(true);
                if !fresh.is_empty() && fresh != vd {
                    let retried = call(
                        self.agent,
                        &self.config.youtube_endpoint,
                        client,
                        &fresh,
                        "player",
                        &req,
                    )?;
                    if let Err(retry_err) = check_playability(&retried) {
                        self.invalidate_visitor_if_stale(&fresh, &retried);
                        return Err(retry_err);
                    }
                    self.harvest_visitor_data(&retried);
                    return Ok(retried);
                }
            }
            return Err(err);
        }
        self.harvest_visitor_data(&answer);
        Ok(answer)
    }

    fn music_call<B: Serialize + ?Sized>(
        &self,
        endpoint: &str,
        body: &B,
    ) -> Result<Value, SourceError> {
        let vd = self.effective_visitor_data(false);
        let answer = call(
            self.agent,
            &self.config.youtube_music_endpoint,
            &MUSIC_CLIENT,
            &vd,
            endpoint,
            body,
        )?;
        self.harvest_visitor_data(&answer);
        Ok(answer)
    }

    /// Searches the catalogue for videos.
    pub fn search(&self, query: &str, want: usize) -> Result<Vec<Track>, SourceError> {
        if query.trim().is_empty() || want == 0 {
            return Ok(Vec::new());
        }
        let [primary, fallback] = self.ordered_catalogue_clients();
        let req = SearchRequest { query, params: SEARCH_PARAMS };
        let answer = match self.web_call(&primary, "search", &req) {
            Ok(answer) => {
                self.record_client_outcome(primary.name, true);
                answer
            }
            Err(_) if is_https(&self.config.youtube_endpoint) => {
                self.record_client_outcome(primary.name, false);
                let answer = self.web_call(&fallback, "search", &req)?;
                self.record_client_outcome(fallback.name, true);
                answer
            }
            Err(error) => return Err(error),
        };
        let mut found = Vec::with_capacity(want.min(64));
        collect_videos_up_to(&answer, &mut found, want);
        Ok(found.into_iter().filter_map(|media| media.into_track("youtube")).take(want).collect())
    }

    /// Searches YouTube Music, which finds the official recording where the
    /// ordinary catalogue finds a reupload.
    pub fn music_search(&self, query: &str, want: usize) -> Result<Vec<Track>, SourceError> {
        if query.trim().is_empty() || want == 0 {
            return Ok(Vec::new());
        }
        let req = SearchRequest { query, params: MUSIC_SONGS_PARAMS };
        let answer = self.music_call("search", &req)?;
        let mut found = Vec::with_capacity(want.min(64));
        collect_videos_up_to(&answer, &mut found, want);
        Ok(found.into_iter().filter_map(|media| media.into_track("youtube")).take(want).collect())
    }

    /// Reads a playlist, up to the configured number of tracks.
    ///
    /// A playlist is paged, and each page names the next one, so the walk stops
    /// on a page count rather than on the playlist ending: a client asking for a
    /// thousand track playlist should not hold the load open while the node
    /// reads all of it.
    pub fn playlist(&self, playlist_id: &str) -> Result<LoadResult, SourceError> {
        let limit = self.config.max_playlist_tracks as usize;
        if limit == 0 {
            return Ok(LoadResult::Empty);
        }
        let [mut client, fallback_client] = self.ordered_catalogue_clients();
        let browse_id = format!("VL{playlist_id}");
        let mut videos = Vec::new();
        let mut page_videos = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut seen_continuations = HashSet::new();
        let mut title = String::new();
        let mut continuation = String::new();
        let mut pages = 0;
        let mut last_error: Option<SourceError> = None;
        let mut saw_ok = false;
        loop {
            let req = if continuation.is_empty() {
                BrowseRequest { browse_id: Some(&browse_id), continuation: None }
            } else {
                BrowseRequest { browse_id: None, continuation: Some(&continuation) }
            };
            let answer = match self.web_call(&client, "browse", &req) {
                Ok(answer) => {
                    self.record_client_outcome(client.name, true);
                    saw_ok = true;
                    answer
                }
                Err(error) if pages == 0 && playlist_id.starts_with("RD") => {
                    last_error = Some(error);
                    break;
                }
                Err(error) if pages == 0 && is_https(&self.config.youtube_endpoint) => {
                    self.record_client_outcome(client.name, false);
                    last_error = Some(error);
                    client = fallback_client.clone();
                    match self.web_call(&client, "browse", &req) {
                        Ok(answer) => {
                            self.record_client_outcome(client.name, true);
                            saw_ok = true;
                            answer
                        }
                        Err(error) if playlist_id.starts_with("RD") => {
                            last_error = Some(error);
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
            if title.is_empty() {
                title = header_title(&answer).unwrap_or_default();
            }
            page_videos.clear();
            collect_videos_up_to(&answer, &mut page_videos, limit);
            for media in page_videos.drain(..) {
                if seen_ids.insert(media.id.clone()) {
                    videos.push(media);
                    if videos.len() >= limit {
                        break;
                    }
                }
            }
            pages += 1;
            if videos.len() >= limit || pages >= MAX_PLAYLIST_PAGES {
                break;
            }
            match next_continuation(&answer) {
                Some(token) if !seen_continuations.contains(token) => {
                    seen_continuations.insert(token.to_owned());
                    continuation.clear();
                    continuation.push_str(token);
                }
                _ => break,
            }
        }
        if videos.is_empty() && playlist_id.starts_with("RD") {
            let next_req = NextRequest {
                video_id: playlist_id.strip_prefix("RD").filter(|seed| youtube_id(seed)),
                playlist_id,
            };
            let https = is_https(&self.config.youtube_endpoint);
            for attempt in 0..3 {
                let outcome = match attempt {
                    0 => self.web_call(&client, "next", &next_req),
                    1 if https => self.web_call(&visionos_client(), "next", &next_req),
                    2 if https => self.music_call("next", &next_req),
                    _ => break,
                };
                match outcome {
                    Ok(answer) => {
                        saw_ok = true;
                        if title.is_empty() {
                            title = header_title(&answer).unwrap_or_default();
                        }
                        page_videos.clear();
                        collect_videos_up_to(&answer, &mut page_videos, limit);
                        for media in page_videos.drain(..) {
                            if seen_ids.insert(media.id.clone()) {
                                videos.push(media);
                                if videos.len() >= limit {
                                    break;
                                }
                            }
                        }
                        if !videos.is_empty() {
                            if title.is_empty() {
                                title = "YouTube Mix".into();
                            }
                            break;
                        }
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }
        }
        if videos.is_empty() && !saw_ok {
            if let Some(error) = last_error {
                return Err(error);
            }
        }
        let tracks: Vec<Track> = videos
            .into_iter()
            .filter_map(|media| media.into_track("youtube"))
            .take(limit)
            .collect();
        if tracks.is_empty() {
            return Ok(LoadResult::Empty);
        }
        Ok(LoadResult::Playlist(Box::new(Playlist::new(title, -1, tracks))))
    }

    /// Resolves a direct audio URL for a video.
    ///
    /// Every client is tried, because YouTube answers the ones it trusts with a
    /// plain URL and the ones it does not with a format that carries none. The
    /// failure that is reported is the first one, which is the one for the
    /// client that usually works.
    pub fn media_url(
        &self,
        video_id: &str,
    ) -> Result<(String, BTreeMap<String, String>), SourceError> {
        self.with_media_url(video_id, |url, headers| Ok((url, headers)))
    }

    /// Tries a pre-resolved stream from `YoutubeStreamCache` first (populated when
    /// `video()` fetched metadata, or on an earlier playback/seek), falling back
    /// to live client iteration when the cache is empty or the CDN rejects the URL.
    pub(crate) fn with_media_url<T>(
        &self,
        video_id: &str,
        mut open: impl FnMut(String, BTreeMap<String, String>) -> Result<T, SourceError>,
    ) -> Result<T, SourceError> {
        let mut first: Option<SourceError> = None;
        let mut stream_error = None;
        let mut skip_recent_client: Option<&'static str> = None;

        if let Some(state) = self.state {
            let key = self.stream_key(video_id);
            if let Some((cached, age)) = state.youtube_streams.get(&key) {
                netguard::validate_url(&cached.url)?;
                let mut headers = cached.headers.clone();
                headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
                headers.insert("User-Agent".into(), cached.user_agent.to_string());
                let url = with_po_token(cached.url.clone(), &self.config.youtube_po_token);
                let mut probe_failed = false;
                if cached.throttled_without_pot
                    && !cached.tail_probed
                    && self.config.youtube_po_token.trim().is_empty()
                    && cached.content_length > 1_048_576
                {
                    match probe_tail_byte(self.agent, &url, &headers, cached.content_length) {
                        Ok(()) => state.youtube_streams.mark_tail_probed(&key),
                        Err(error) if error.retryable() => {
                            state.youtube_streams.invalidate(&key);
                            self.record_client_outcome(cached.client_name, false);
                            stream_error = Some(error);
                            if age < RECENTLY_RESOLVED_WINDOW {
                                skip_recent_client = Some(cached.client_name);
                            }
                            probe_failed = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if !probe_failed {
                    match open(url, headers) {
                        Ok(stream) => {
                            self.record_client_outcome(cached.client_name, true);
                            return Ok(stream);
                        }
                        Err(error) if error.retryable() => {
                            state.youtube_streams.invalidate(&key);
                            self.record_client_outcome(cached.client_name, false);
                            stream_error = Some(error);
                            if age < RECENTLY_RESOLVED_WINDOW {
                                skip_recent_client = Some(cached.client_name);
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }

        for client in self.ordered_clients() {
            if skip_recent_client == Some(client.name) {
                continue;
            }
            match self.player(&client, video_id) {
                Ok(Some(format)) => {
                    netguard::validate_url(&format.url)?;
                    let cache_candidate = Format {
                        url: format.url.clone(),
                        mime_type: String::new(),
                        bitrate: format.bitrate,
                        content_length: format.content_length,
                        headers: format.headers.clone(),
                    };
                    let mut headers = format.headers;
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
                    headers.insert("User-Agent".into(), client.user_agent.to_string());
                    let url = with_po_token(format.url, &self.config.youtube_po_token);
                    if client.throttled_without_pot
                        && self.config.youtube_po_token.trim().is_empty()
                        && format.content_length > 1_048_576
                    {
                        if let Err(error) =
                            probe_tail_byte(self.agent, &url, &headers, format.content_length)
                        {
                            if error.retryable() {
                                self.record_client_outcome(client.name, false);
                                if stream_error.is_none() {
                                    stream_error = Some(error);
                                }
                                continue;
                            }
                            return Err(error);
                        }
                    }
                    match open(url, headers) {
                        Ok(stream) => {
                            self.store_cached_stream(video_id, &client, cache_candidate, true);
                            self.record_client_outcome(client.name, true);
                            return Ok(stream);
                        }
                        Err(error) if error.retryable() => {
                            self.record_client_outcome(client.name, false);
                            if stream_error.is_none() {
                                stream_error = Some(error);
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.record_client_outcome(client.name, false);
                    if first.is_none() {
                        first = Some(error);
                    }
                }
            }
        }
        Err(stream_error.or(first).unwrap_or_else(|| SourceError::Http(
            "youtube offered no directly usable audio URL; native resolution does not execute player JavaScript or assemble HLS".into()
        )))
    }

    #[cfg(test)]
    fn player_body(&self, video_id: &str) -> Value {
        serde_json::to_value(self.player_request(video_id)).expect("valid player request")
    }

    /// Asks one client for a video's streams.
    pub fn player(&self, client: &Client, video_id: &str) -> Result<Option<Format>, SourceError> {
        let answer = self.player_call(client, video_id)?;
        check_video_id(&answer, video_id)?;
        Ok(pick_format(&answer))
    }

    /// Reads a video's own metadata, which is how a bare video id becomes a
    /// track without a search. When the player response also carries usable
    /// `streamingData`, caches the picked stream so `open()` starts with zero
    /// additional InnerTube `/player` round-trips.
    pub fn video(&self, video_id: &str) -> Result<Track, SourceError> {
        let mut first = None;
        for client in self.ordered_clients() {
            match self.video_with_client(&client, video_id) {
                Ok(track) => {
                    self.record_client_outcome(client.name, true);
                    return Ok(track);
                }
                Err(error) => {
                    self.record_client_outcome(client.name, false);
                    if first.is_none() {
                        first = Some(error);
                    }
                }
            }
        }
        Err(first.unwrap_or_else(|| SourceError::Http("youtube described no video".into())))
    }

    fn video_with_client(&self, client: &Client, video_id: &str) -> Result<Track, SourceError> {
        let answer = self.player_call(client, video_id)?;
        check_video_id(&answer, video_id)?;
        let details = answer
            .pointer("/videoDetails")
            .ok_or_else(|| SourceError::Http("youtube described no video".into()))?;
        let seconds = details
            .get("lengthSeconds")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())
            .unwrap_or(0.0);
        let live = details.get("isLive").and_then(Value::as_bool).unwrap_or_else(|| {
            details.get("isLiveContent").and_then(Value::as_bool).unwrap_or(false) && seconds <= 0.0
        });
        let media = Media {
            id: details.get("videoId").and_then(Value::as_str).unwrap_or(video_id).to_string(),
            title: details.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            author: details.get("author").and_then(Value::as_str).unwrap_or_default().to_string(),
            seconds,
            live,
            artwork: thumbnails(answer.pointer("/videoDetails/thumbnail")),
        };
        let track = media.into_track("youtube").ok_or_else(|| {
            SourceError::Http("youtube described a video that cannot be encoded".into())
        })?;
        if !live {
            if let Some(format) = pick_format(&answer) {
                self.store_cached_stream(video_id, client, format, false);
            }
        }
        Ok(track)
    }
}

fn check_video_id(answer: &Value, requested: &str) -> Result<(), SourceError> {
    if answer
        .pointer("/videoDetails/videoId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != requested)
    {
        return Err(SourceError::Http("youtube returned a different video".into()));
    }
    Ok(())
}

fn check_playability(answer: &Value) -> Result<(), SourceError> {
    let status = answer.pointer("/playabilityStatus/status").and_then(Value::as_str);
    if status.is_some_and(|status| status != "OK") {
        let reason = answer
            .pointer("/playabilityStatus/reason")
            .and_then(Value::as_str)
            .unwrap_or("the native client was refused");
        return Err(SourceError::Http(format!("youtube: {reason}")));
    }
    Ok(())
}

/// One video, as the catalogue describes it.
#[derive(Debug, Clone, Default)]
pub struct Media {
    pub id: String,
    pub title: String,
    pub author: String,
    pub seconds: f64,
    pub live: bool,
    pub artwork: Option<String>,
}

impl Media {
    fn into_track(self, source: &str) -> Option<Track> {
        let author =
            if self.author.is_empty() { "Unknown artist".to_string() } else { self.author };
        let length = duration_millis(self.seconds);
        let uri = Some(format!("https://www.youtube.com/watch?v={}", self.id));
        let info = TrackInfo {
            identifier: self.id,
            title: self.title,
            author,
            length,
            // Search and playlist items often omit their length, so an unknown
            // length is still a video: only the live flag makes a stream.
            is_stream: self.live,
            is_seekable: !self.live,
            uri,
            artwork_url: self.artwork,
            source_name: source.to_string(),
            ..Default::default()
        };
        encode_track_owned(info).ok()
    }

    #[cfg(test)]
    fn track(&self, source: &str) -> Track {
        self.clone().into_track(source).expect("valid test media")
    }
}

/// Adds the configured proof-of-origin token to a Google video URL. Without it
/// the CDN answers only the first ~768 KiB of audio and then refuses with 403.
/// Other hosts never receive the token.
fn with_po_token(url: String, token: &str) -> String {
    let token = token.trim();
    let Some(mut parsed) = crate::http_url(&url) else { return url };
    if token.is_empty()
        || !parsed.host_str().is_some_and(|host| crate::host_matches(host, "googlevideo.com"))
        || parsed.query_pairs().any(|(name, _)| name == "pot")
    {
        return url;
    }
    parsed.query_pairs_mut().append_pair("pot", token);
    parsed.into()
}

/// Converts an external duration to protocol milliseconds without allowing
/// non-finite, negative, or unrepresentable values to become metadata. A zero
/// result means that the source did not provide a usable duration.
fn duration_millis(seconds: f64) -> i64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let milliseconds = seconds * 1000.0;
    if !milliseconds.is_finite() || milliseconds <= 0.0 || milliseconds >= i64::MAX as f64 {
        return 0;
    }
    milliseconds as i64
}

/// Picks the best audio-only stream out of a player answer.
///
/// A progressive format carries audio and video together, which is twice the
/// bytes for the same sound; an adaptive audio-only format is what a node that
/// never shows a picture wants.
fn pick_format(answer: &Value) -> Option<Format> {
    #[derive(Clone, Copy)]
    struct Candidate<'a> {
        entry: &'a Value,
        url: &'a str,
        mime: &'a str,
        bitrate: i64,
        rank: (bool, bool, bool, bool, bool, i64),
    }

    fn select<'a>(formats: &'a [Value], audio_only: bool) -> Option<Candidate<'a>> {
        let mut best: Option<Candidate<'a>> = None;
        for entry in formats {
            let mime = entry.get("mimeType").and_then(Value::as_str).unwrap_or_default();
            if (audio_only && !mime.starts_with("audio/")) || !has_audio_codec(mime) {
                continue;
            }
            let Some(url) = entry.get("url").and_then(Value::as_str).filter(|u| !u.is_empty())
            else {
                continue;
            };
            let bitrate = entry
                .get("bitrate")
                .or_else(|| entry.get("averageBitrate"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let dubbed =
                entry.pointer("/audioTrack/isAutoDubbed").and_then(Value::as_bool).unwrap_or(false);
            let display = entry
                .pointer("/audioTrack/displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_end_matches(')')
                .trim_end();
            let original = display
                .get(display.len().saturating_sub(8)..)
                .is_some_and(|s| s.eq_ignore_ascii_case("original"))
                && display
                    .as_bytes()
                    .get(display.len().saturating_sub(9))
                    .is_some_and(|b| matches!(b, b' ' | b'('));
            let default_track = entry
                .pointer("/audioTrack/audioIsDefault")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let he_aac = mime_contains(mime, b"mp4a.40.5") || mime_contains(mime, b"mp4a.40.29");
            let drc = entry.get("isDrc").and_then(Value::as_bool).unwrap_or(false);
            let rank = (!dubbed, original, default_track, !he_aac, !drc, bitrate);
            if best.is_some_and(|held| held.rank >= rank) {
                continue;
            }
            best = Some(Candidate { entry, url, mime, bitrate, rank });
        }
        best
    }

    let best = answer
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
        .and_then(|formats| select(formats, true))
        .or_else(|| {
            answer
                .pointer("/streamingData/formats")
                .and_then(Value::as_array)
                .and_then(|formats| select(formats, false))
        })?;
    let Candidate { entry, url, mime, bitrate, .. } = best;
    let mut headers = BTreeMap::new();
    if let Some(extra) = entry.get("httpHeaders").and_then(Value::as_object) {
        for (name, value) in extra {
            if let Some(value) = value.as_str() {
                headers.insert(name.clone(), value.to_owned());
            }
        }
    }
    Some(Format {
        url: url.to_owned(),
        mime_type: mime.to_owned(),
        bitrate,
        content_length: entry
            .get("contentLength")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())
            .unwrap_or(0),
        headers,
    })
}

/// Whether a MIME type names a codec the node can decode.
///
/// The player offers some formats in codecs nothing but a browser has, and
/// handing one to the decoder would fail a track that has a playable format next
/// to it.
fn mime_contains(mime: &str, needle: &[u8]) -> bool {
    mime.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn has_audio_codec(mime: &str) -> bool {
    const PLAYABLE: [&[u8]; 5] = [b"opus", b"vorbis", b"mp4a", b"flac", b"aac"];
    PLAYABLE.iter().any(|codec| mime_contains(mime, codec))
}

fn thumbnails(value: Option<&Value>) -> Option<String> {
    let list = value?.get("thumbnails")?.as_array()?;
    // The last entry is the largest.
    list.last()?.get("url")?.as_str().map(str::to_string)
}

/// Reads a written duration such as "3:45" or "1:02:03" into seconds.
fn length_seconds(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut count = 0usize;
    let mut total = 0u64;
    for part in trimmed.split(':') {
        count += 1;
        if count > 3 {
            return None;
        }
        let part = part.trim();
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: u64 = part.parse().ok()?;
        if count > 1 && value >= 60 {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(value)?;
    }
    if count < 2 {
        return None;
    }
    Some(total as f64)
}

/// Reads the words of a renderer field, borrowing when possible.
fn runs_cow(value: Option<&Value>) -> Cow<'_, str> {
    let Some(value) = value else {
        return Cow::Borrowed("");
    };
    if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
        return Cow::Borrowed(text);
    }
    if let Some(runs) = value.get("runs").and_then(Value::as_array) {
        return match runs.as_slice() {
            [] => Cow::Borrowed(""),
            [single] => Cow::Borrowed(single.get("text").and_then(Value::as_str).unwrap_or("")),
            many => Cow::Owned(
                many.iter().filter_map(|run| run.get("text").and_then(Value::as_str)).collect(),
            ),
        };
    }
    if let Some(text) = value.as_str() {
        return Cow::Borrowed(text);
    }
    Cow::Borrowed("")
}

fn runs_text(value: Option<&Value>) -> String {
    runs_cow(value).into_owned()
}

/// Walks an answer and collects every video it names.
///
/// The catalogue nests its results differently for every surface, and a search
/// for a song can come back as any of half a dozen renderer shapes. Looking for
/// the shapes rather than for the path they sit at is what keeps one parser
/// working across all of them.
pub fn collect_videos(value: &Value, out: &mut Vec<Media>) {
    collect_videos_up_to(value, out, usize::MAX);
}

fn collect_videos_up_to(value: &Value, out: &mut Vec<Media>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if out.len() >= limit {
                    return;
                }
                match key.as_str() {
                    // Promoted results and metadata/entity envelopes are never
                    // song search results or playlist entries, so skip them.
                    "responseContext"
                    | "frameworkUpdates"
                    | "adSlotRenderer"
                    | "adPlacementRenderer"
                    | "inFeedAdLayoutRenderer"
                    | "promotedSparklesWebRenderer"
                    | "promotedVideoRenderer"
                    | "playerAds"
                    | "adPlacements"
                    | "adSlots" => continue,
                    "videoRenderer"
                    | "compactVideoRenderer"
                    | "playlistVideoRenderer"
                    | "playlistPanelVideoRenderer"
                    | "reelItemRenderer" => {
                        if let Some(media) = video_from_renderer(child) {
                            out.push(media);
                        }
                        continue;
                    }
                    "shortsLockupViewModel" => {
                        if let Some(media) = video_from_shorts_lockup(child) {
                            out.push(media);
                        }
                        continue;
                    }
                    "musicResponsiveListItemRenderer" => {
                        if let Some(media) = video_from_music_item(child) {
                            out.push(media);
                        }
                        continue;
                    }
                    _ => {}
                }
                collect_videos_up_to(child, out, limit);
            }
        }
        Value::Array(items) => {
            for item in items {
                if out.len() >= limit {
                    return;
                }
                collect_videos_up_to(item, out, limit);
            }
        }
        _ => {}
    }
}

fn video_from_renderer(value: &Value) -> Option<Media> {
    let id = value.get("videoId").and_then(Value::as_str)?;
    if !youtube_id(id) {
        return None;
    }
    let title = runs_text(value.get("title").or_else(|| value.get("headline")));
    if title.is_empty() {
        return None;
    }
    let author = ["ownerText", "longBylineText", "shortBylineText"]
        .into_iter()
        .map(|field| runs_cow(value.get(field)))
        .find(|text| !text.is_empty())
        .map_or_else(String::new, Cow::into_owned);
    let length_raw = runs_cow(value.get("lengthText"));
    let seconds = length_seconds(&length_raw).unwrap_or(0.0);
    let live = value.get("isLiveNow").and_then(Value::as_bool).unwrap_or(false)
        || value.get("badges").and_then(Value::as_array).is_some_and(|badges| {
            badges.iter().any(|b| {
                b.pointer("/metadataBadgeRenderer/style").and_then(Value::as_str)
                    == Some("BADGE_STYLE_TYPE_LIVE_NOW")
            })
        })
        || value.get("thumbnailOverlays").and_then(Value::as_array).is_some_and(|overlays| {
            overlays.iter().any(|o| {
                o.pointer("/thumbnailOverlayTimeStatusRenderer/style").and_then(Value::as_str)
                    == Some("LIVE")
            })
        });
    Some(Media {
        id: id.to_string(),
        title,
        author,
        seconds,
        live,
        artwork: thumbnails(value.get("thumbnail")),
    })
}

fn video_from_shorts_lockup(value: &Value) -> Option<Media> {
    let id = value
        .pointer("/onTap/innertubeCommand/reelWatchEndpoint/videoId")
        .or_else(|| value.pointer("/onTap/innertubeCommand/watchEndpoint/videoId"))
        .and_then(Value::as_str)
        .filter(|id| youtube_id(id))?;
    let title = value
        .pointer("/overlayMetadata/primaryText/content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let artwork = value
        .pointer("/thumbnailViewModel/thumbnailViewModel/image/sources")
        .or_else(|| value.pointer("/thumbnail/sources"))
        .and_then(Value::as_array)
        .and_then(|list| list.last())
        .and_then(|item| item.get("url"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Media {
        id: id.to_owned(),
        title,
        author: String::new(),
        seconds: 0.0,
        live: false,
        artwork,
    })
}

/// Reads a YouTube Music list item, whose fields are in flex columns.
fn video_from_music_item(value: &Value) -> Option<Media> {
    let columns = value.get("flexColumns").and_then(Value::as_array)?;
    let id = value
        .pointer("/playlistItemData/videoId")
        .and_then(Value::as_str)
        .filter(|id| youtube_id(id))
        .or_else(|| {
            columns.iter().find_map(|column| {
                column
                    .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs")
                    .and_then(Value::as_array)?
                    .iter()
                    .find_map(|run| {
                        run.pointer("/navigationEndpoint/watchEndpoint/videoId")
                            .and_then(Value::as_str)
                            .filter(|id| youtube_id(id))
                    })
            })
        })
        .or_else(|| {
            value
                .pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId")
                .and_then(Value::as_str)
                .filter(|id| youtube_id(id))
        })?;
    let title = columns
        .first()
        .map(|column| runs_text(column.pointer("/musicResponsiveListItemFlexColumnRenderer/text")))
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    // The second column lists the artist and then the album (and sometimes
    // duration), separated by middle dots, so the artist is the part before it.
    let byline = columns
        .get(1)
        .map(|column| runs_cow(column.pointer("/musicResponsiveListItemFlexColumnRenderer/text")))
        .unwrap_or_default();
    let author = byline.split('•').next().unwrap_or_default().trim().to_string();
    let fixed_len =
        runs_cow(value.pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text"));
    let seconds = length_seconds(&fixed_len)
        .or_else(|| byline.rsplit('•').next().and_then(length_seconds))
        .unwrap_or(0.0);
    let artwork = thumbnails(value.pointer("/thumbnail/musicThumbnailRenderer/thumbnail"))
        .or_else(|| thumbnails(value.get("thumbnail")));
    Some(Media { id: id.to_string(), title, author, seconds, live: false, artwork })
}

/// Reads the title of a playlist page.
fn header_title(value: &Value) -> Option<String> {
    [
        "/header/playlistHeaderRenderer/title",
        "/header/pageHeaderRenderer/pageTitle",
        "/metadata/playlistMetadataRenderer/title",
        "/contents/twoColumnWatchNextResults/playlist/playlist/title",
        "/contents/singleColumnWatchNextResults/playlist/playlist/title",
    ]
    .into_iter()
    .map(|path| runs_cow(value.pointer(path)))
    .find(|text| !text.is_empty())
    .map(Cow::into_owned)
}

/// Finds the token that names the next page of a list.
fn next_continuation(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => {
            if let Some(token) = map
                .get("nextContinuationData")
                .and_then(|data| data.get("continuation"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                return Some(token);
            }
            if let Some(token) = map
                .get("continuationCommand")
                .and_then(|data| data.get("token"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                return Some(token);
            }
            map.iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "responseContext"
                            | "frameworkUpdates"
                            | "header"
                            | "sidebar"
                            | "topbar"
                            | "microformat"
                            | "menu"
                            | "sortFilterSubMenuRenderer"
                            | "subMenuItems"
                            | "chipCloudRenderer"
                            | "adSlotRenderer"
                            | "adPlacementRenderer"
                    )
                })
                .find_map(|(_, child)| next_continuation(child))
        }
        Value::Array(items) => items.iter().find_map(next_continuation),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promoted_video_subtrees_are_not_returned_as_music_results() {
        let video = |id: &str| {
            json!({"videoRenderer": {
                "videoId": id, "title":{"simpleText":"A Song"},
                "ownerText":{"simpleText":"Someone"}, "lengthText":{"simpleText":"3:20"}
            }})
        };
        for key in [
            "adSlotRenderer",
            "adPlacementRenderer",
            "inFeedAdLayoutRenderer",
            "promotedSparklesWebRenderer",
            "promotedVideoRenderer",
            "playerAds",
            "adPlacements",
            "adSlots",
        ] {
            let answer = json!({"contents":[{(key):video("advertisemt1")},video("dQw4w9WgXcQ")]});
            let mut videos = Vec::new();
            collect_videos(&answer, &mut videos);
            assert_eq!(videos.len(), 1, "{key}");
            assert_eq!(videos[0].id, "dQw4w9WgXcQ");
        }
    }

    #[test]
    fn video_metadata_tries_another_client_after_a_refusal() {
        let mut request = 0;
        let (base, server) = crate::test_http::serve(2, move |_, _, body| {
            request += 1;
            assert_eq!(body["videoId"], "1CXveaq_vFU");
            if request == 1 {
                assert_eq!(body["context"]["client"]["clientName"], "IOS_PRODUCER");
                json!({"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"client refused"}})
            } else {
                assert_eq!(body["context"]["client"]["clientName"], "VISIONOS");
                json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                    "videoId":"1CXveaq_vFU","title":"Shared video","author":"Artist","lengthSeconds":"180"
                }})
            }
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config).video("1CXveaq_vFU");
        server.join().unwrap();
        let track = result.unwrap();
        assert_eq!(track.metadata.identifier, "1CXveaq_vFU");
        assert_eq!(track.metadata.title, "Shared video");
        assert_eq!(track.metadata.length, 180_000);
    }

    #[test]
    fn video_metadata_never_returns_an_unrelated_video() {
        let mut request = 0;
        let (base, server) = crate::test_http::serve(3, move |_, _, _| {
            request += 1;
            match request {
                1 => json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                    "videoId":"dQw4w9WgXcQ","title":"Wrong video"
                }}),
                2 => json!({"playabilityStatus":{"status":"OK"}}),
                _ => json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                    "videoId":"1CXveaq_vFU","title":"Requested video"
                }}),
            }
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config).video("1CXveaq_vFU");
        server.join().unwrap();
        assert_eq!(result.unwrap().metadata.title, "Requested video");
    }

    #[test]
    fn a_cdn_rejection_tries_the_next_client_and_keeps_its_user_agent() {
        let (base, server) = crate::test_http::serve(2, |_, _, body| {
            let client = body["context"]["client"]["clientName"].as_str().unwrap();
            json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{
                "mimeType":"audio/webm; codecs=\"opus\"",
                "url":format!("https://cdn.example/{client}"),"bitrate":160000,
                "httpHeaders":{"user-agent":"wrong agent","X-Media-Fixture":"preserved"}
            }]}})
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let mut opened = 0;
        let result = Catalog::new(&agent, &config).with_media_url("1CXveaq_vFU", |url, headers| {
            opened += 1;
            assert_eq!(
                headers.keys().filter(|name| name.eq_ignore_ascii_case("user-agent")).count(),
                1
            );
            assert_eq!(headers["X-Media-Fixture"], "preserved");
            if opened == 1 {
                assert!(url.ends_with("/IOS_PRODUCER"));
                Err(SourceError::Media { status: 403 })
            } else {
                assert!(url.ends_with("/VISIONOS"));
                assert!(headers["User-Agent"].contains("AppleWebKit/605.1.15"));
                Ok(url)
            }
        });
        server.join().unwrap();
        assert!(result.unwrap().ends_with("/VISIONOS"));
        assert_eq!(opened, 2);
    }

    #[test]
    fn a_different_videos_audio_is_never_opened() {
        let mut request = 0;
        let (base, server) = crate::test_http::serve(2, move |_, _, body| {
            request += 1;
            assert_eq!(body["videoId"], "1CXveaq_vFU");
            let id = if request == 1 { "dQw4w9WgXcQ" } else { "1CXveaq_vFU" };
            json!({"playabilityStatus":{"status":"OK"},"videoDetails":{"videoId":id},
            "streamingData":{"adaptiveFormats":[{
                "mimeType":"audio/webm; codecs=\"opus\"",
                "url":format!("https://cdn.example/{id}"),"bitrate":160000
            }]}})
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let mut opened = 0;
        let result = Catalog::new(&agent, &config).with_media_url("1CXveaq_vFU", |url, _| {
            opened += 1;
            assert!(url.ends_with("/1CXveaq_vFU"));
            Ok(url)
        });
        server.join().unwrap();
        assert!(result.is_ok());
        assert_eq!(opened, 1);
    }

    #[test]
    fn exhausted_clients_preserve_the_retryable_cdn_error() {
        let mut request = 0;
        let (base, server) = crate::test_http::serve(4, move |_, _, _| {
            request += 1;
            if request == 1 {
                json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{
                    "mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/audio","bitrate":160000
                }]}})
            } else {
                json!({"playabilityStatus":{"status":"ERROR","reason":"client refused"}})
            }
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config)
            .with_media_url::<()>("1CXveaq_vFU", |_, _| Err(SourceError::Media { status: 403 }));
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Media { status: 403 })));
    }

    #[test]
    fn a_nonretryable_stream_error_stops_client_fallback() {
        let (base, server) = crate::test_http::serve(1, |_, _, _| {
            json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{
                "mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/audio","bitrate":160000
            }]}})
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config).with_media_url::<()>("1CXveaq_vFU", |_, _| {
            Err(SourceError::Url("invalid media URL".into()))
        });
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Url(_))));
    }

    #[test]
    fn the_po_token_reaches_google_video_urls_only() {
        let audio = "https://rr1---sn-x.googlevideo.com/videoplayback?itag=251&c=ANDROID";
        assert_eq!(
            with_po_token(audio.into(), " a+b/c= "),
            format!("{audio}&pot=a%2Bb%2Fc%3D"),
            "appended once, trimmed and encoded"
        );
        assert_eq!(with_po_token(audio.into(), ""), audio, "no token leaves the URL alone");
        let tokened = format!("{audio}&pot=existing");
        assert_eq!(with_po_token(tokened.clone(), "new"), tokened, "an existing pot is kept");
        let other = "https://cdn.example/audio?id=1";
        assert_eq!(with_po_token(other.into(), "secret"), other, "other hosts never see it");
    }

    #[test]
    fn native_player_sends_configured_token_and_visitor_context() {
        let (base, server) = crate::test_http::serve(1, |_, headers, body| {
            assert!(headers.starts_with("POST /youtubei/v1/player?prettyPrint=false "));
            assert!(headers.to_lowercase().contains("x-goog-visitor-id: visitor-fixture"));
            assert_eq!(
                body.pointer("/context/client/visitorData").and_then(Value::as_str),
                Some("visitor-fixture")
            );
            assert_eq!(
                body.pointer("/context/client/clientName").and_then(Value::as_str),
                Some("IOS_PRODUCER")
            );
            assert_eq!(
                body.pointer("/serviceIntegrityDimensions/poToken").and_then(Value::as_str),
                Some("player-token-fixture")
            );
            json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[
                {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/audio","bitrate":160000}
            ]}})
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        config.youtube_po_token = "player-token-fixture".into();
        config.youtube_visitor_data = "visitor-fixture".into();
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config).player(&clients(&config)[0], "dQw4w9WgXcQ");
        server.join().unwrap();
        assert_eq!(result.unwrap().unwrap().url, "https://cdn.example/audio");
        let config = Config::default();
        assert!(Catalog::new(&agent, &config)
            .player_body("dQw4w9WgXcQ")
            .get("serviceIntegrityDimensions")
            .is_none());
    }

    #[test]
    fn a_written_length_becomes_seconds() {
        assert_eq!(length_seconds("3:45"), Some(225.0));
        assert_eq!(length_seconds("1:02:03"), Some(3723.0));
        assert_eq!(length_seconds("0:07"), Some(7.0));
        assert_eq!(length_seconds("not a length"), None);
        assert_eq!(length_seconds("-1:00"), None);
        assert_eq!(length_seconds("NaN"), None);
        assert_eq!(length_seconds("inf"), None);
    }

    #[test]
    fn missing_or_invalid_media_durations_become_unknown_but_stay_seekable_videos() {
        // 0.0 is what a search or playlist item without `lengthText` carries.
        for seconds in [0.0, f64::NAN, -1.0, f64::INFINITY, f64::MAX] {
            let track = Media {
                id: "dQw4w9WgXcQ".into(),
                title: "A Song".into(),
                author: "Someone".into(),
                seconds,
                ..Default::default()
            }
            .track("youtube");
            assert_eq!(track.metadata.length, 0, "seconds={seconds:?}");
            assert!(!track.metadata.is_stream, "seconds={seconds:?}");
            assert!(track.metadata.is_seekable, "seconds={seconds:?}");
        }
    }

    #[test]
    fn a_valid_media_duration_remains_seekable() {
        let track = Media {
            id: "dQw4w9WgXcQ".into(),
            title: "A Song".into(),
            author: "Someone".into(),
            seconds: 180.0,
            ..Default::default()
        }
        .track("youtube");
        assert_eq!(track.metadata.length, 180_000);
        assert!(!track.metadata.is_stream);
        assert!(track.metadata.is_seekable);
    }

    #[test]
    fn a_video_renderer_becomes_a_track() {
        let answer = json!({
            "contents": {"itemSectionRenderer": {"contents": [{"videoRenderer": {
                "videoId": "dQw4w9WgXcQ",
                "title": {"runs": [{"text": "A Song"}]},
                "ownerText": {"runs": [{"text": "Someone"}]},
                "lengthText": {"simpleText": "3:45"},
                "thumbnail": {"thumbnails": [{"url": "small"}, {"url": "large"}]}
            }}]}}
        });
        let mut found = Vec::new();
        collect_videos(&answer, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "A Song");
        assert_eq!(found[0].author, "Someone");
        assert_eq!(found[0].seconds, 225.0);
        assert_eq!(found[0].artwork.as_deref(), Some("large"));
    }

    #[test]
    fn a_result_that_is_not_a_video_is_skipped() {
        let answer = json!({"videoRenderer": {"videoId": "short", "title": {"simpleText": "x"}}});
        let mut found = Vec::new();
        collect_videos(&answer, &mut found);
        assert!(found.is_empty());
    }

    #[test]
    fn a_music_item_is_read_from_its_columns() {
        let answer = json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": "A Song", "navigationEndpoint": {"watchEndpoint": {"videoId": "dQw4w9WgXcQ"}}}
                    ]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": "Someone"}, {"text": " • "}, {"text": "An Album"}
                    ]}}}
                ],
                "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "3:45"}]}}}]
            }
        });
        let mut found = Vec::new();
        collect_videos(&answer, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "A Song");
        assert_eq!(found[0].author, "Someone");
        assert_eq!(found[0].seconds, 225.0);
    }

    #[test]
    fn an_audio_only_format_is_preferred_over_a_progressive_one() {
        let answer = json!({"streamingData": {"adaptiveFormats": [
            {"mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"", "url": "https://a", "bitrate": 900000},
            {"mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://b", "bitrate": 128000},
            {"mimeType": "audio/webm; codecs=\"opus\"", "url": "https://c", "bitrate": 160000}
        ]}});
        let picked = pick_format(&answer).unwrap();
        assert_eq!(picked.url, "https://c");
        assert_eq!(picked.bitrate, 160_000);
    }

    #[test]
    fn original_audio_is_preferred_over_default_dubbing_and_drc() {
        let answer = json!({"streamingData":{"adaptiveFormats":[
            {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/dub","bitrate":192000,
             "audioTrack":{"displayName":"English (US)","audioIsDefault":true,"isAutoDubbed":true}},
            {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/drc","bitrate":151132,"isDrc":true,
             "audioTrack":{"displayName":"Chinese (Traditional) original","audioIsDefault":false}},
            {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/original","bitrate":150937,
             "audioTrack":{"displayName":"Chinese (Traditional) original","audioIsDefault":false}}
        ]}});
        assert_eq!(pick_format(&answer).unwrap().url, "https://cdn.example/original");
    }

    #[test]
    fn a_direct_dub_is_usable_when_original_audio_has_no_url() {
        let answer = json!({"streamingData":{"adaptiveFormats":[
            {"mimeType":"audio/webm; codecs=\"opus\"","bitrate":160000,
             "audioTrack":{"displayName":"Chinese (Traditional) original","audioIsDefault":false}},
            {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/dub","bitrate":128000,
             "audioTrack":{"displayName":"English (US)","audioIsDefault":true,"isAutoDubbed":true}}
        ]}});
        assert_eq!(pick_format(&answer).unwrap().url, "https://cdn.example/dub");
    }

    #[test]
    fn a_format_with_no_url_is_not_usable() {
        let answer = json!({"streamingData": {"adaptiveFormats": [
            {"mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 160000}
        ]}});
        assert!(pick_format(&answer).is_none());
    }

    #[test]
    fn a_codec_the_node_cannot_decode_is_not_picked() {
        let answer = json!({"streamingData": {"adaptiveFormats": [
            {"mimeType": "audio/mp4; codecs=\"ec-3\"", "url": "https://a", "bitrate": 500000}
        ]}});
        assert!(pick_format(&answer).is_none());
    }

    #[test]
    fn the_token_that_names_the_next_page_is_found_wherever_it_sits() {
        let answer =
            json!({"contents": {"a": [{"b": {"continuationCommand": {"token": "next"}}}]}});
        assert_eq!(next_continuation(&answer), Some("next"));
    }

    #[test]
    fn a_page_with_no_next_token_ends_the_walk() {
        assert!(next_continuation(&json!({"contents": {}})).is_none());
    }

    #[test]
    fn a_playlist_page_states_its_name() {
        let answer =
            json!({"header": {"playlistHeaderRenderer": {"title": {"simpleText": "My Playlist"}}}});
        assert_eq!(header_title(&answer).as_deref(), Some("My Playlist"));
    }

    #[test]
    fn the_clients_that_serve_whole_files_are_tried_first() {
        let config = Config::default();
        let clients = clients(&config);
        let names: Vec<_> = clients.iter().map(|client| client.name).collect();
        assert_eq!(names, ["IOS_PRODUCER", "VISIONOS", "ANDROID_VR", "ANDROID"]);
        assert_eq!(clients[1].version, "0.1");
        assert!(clients[1].needs_visitor_data);
        assert_eq!(clients[2].version, "1.60.19");
        assert!(clients[2].user_agent.contains("oculus/1.60.19 "));
        assert_eq!(catalogue_client(&config).name, "ANDROID");
    }

    #[test]
    fn a_configured_client_version_replaces_the_built_in_android_one() {
        let mut config = Config::default();
        config.youtube_client_version = "99.1.2".into();
        let android = clients(&config).into_iter().find(|client| client.name == "ANDROID").unwrap();
        assert_eq!(android.version, "99.1.2");
        assert!(android.user_agent.contains("youtube/99.1.2 "));
    }

    #[test]
    fn failed_player_does_not_harvest_tainted_visitor_data_and_stale_reasons_invalidate() {
        let mut request = 0;
        let (base, server) = crate::test_http::serve(2, move |_, headers, body| {
            request += 1;
            if request == 1 {
                assert_eq!(body["context"]["client"]["clientName"], "IOS_PRODUCER");
                json!({
                    "responseContext": {"visitorData": "tainted-visitor"},
                    "playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm you're not a bot"}
                })
            } else {
                assert_eq!(body["context"]["client"]["clientName"], "VISIONOS");
                assert!(!headers.to_ascii_lowercase().contains("tainted-visitor"));
                assert!(body["context"]["client"].get("visitorData").is_none());
                json!({
                    "responseContext": {"visitorData": "clean-visitor"},
                    "playabilityStatus": {"status": "OK"},
                    "videoDetails": {"videoId": "1CXveaq_vFU", "title": "Clean", "lengthSeconds": "120"}
                })
            }
        });
        let state = crate::catalog::State::default();
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let catalog = Catalog::with_state(&agent, &config, &state);
        let track = catalog.video("1CXveaq_vFU").unwrap();
        server.join().unwrap();
        assert_eq!(track.metadata.title, "Clean");
        assert_eq!(state.youtube_visitor.peek().as_deref(), Some("clean-visitor"));

        catalog.invalidate_visitor_if_stale(
            "clean-visitor",
            &json!({"playabilityStatus": {"reason": "Please try again later."}}),
        );
        assert!(state.youtube_visitor.peek().is_none());
    }

    #[test]
    fn rd_mix_playlist_propagates_network_errors_instead_of_returning_empty() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = socket.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(split) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..split]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")?.trim().parse::<usize>().ok()
                            })
                            .unwrap_or(0);
                        if req.len() >= split + 4 + len {
                            break;
                        }
                    }
                }
                let _ = socket.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = socket.flush();
            }
        });
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let result = Catalog::new(&agent, &config).playlist("RDdQw4w9WgXcQ");
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Media { status: 503 })));
    }

    #[test]
    fn visitor_bootstrap_falls_back_to_music_sw_js_data_and_tail_probe_follows_redirects() {
        use std::io::{Read, Write};
        let api_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", api_listener.local_addr().unwrap());
        let api_server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = api_listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf);
                let _ = socket.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });

        let music_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let music_base = format!("http://{}", music_listener.local_addr().unwrap());
        let music_server = std::thread::spawn(move || {
            let (mut socket, _) = music_listener.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut buf = [0u8; 2048];
            let read = socket.read(&mut buf).unwrap();
            let req = std::str::from_utf8(&buf[..read]).unwrap();
            assert!(req.starts_with("GET /sw.js_data "), "{req}");
            let mut slot = vec![Value::Null; 14];
            slot[13] = json!("music-sw-visitor");
            let payload = format!(")]}}'\n{}", json!([[null, null, [[slot]]]]));
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .unwrap();
        });

        let agent = netguard::agent(true);
        let (vd, _) = fetch_visitor_data(&agent, &api_base, &music_base).unwrap();
        api_server.join().unwrap();
        music_server.join().unwrap();
        assert_eq!(vd, "music-sw-visitor");

        let cdn_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let cdn_base = format!("http://{}", cdn_listener.local_addr().unwrap());
        let cdn_server = std::thread::spawn(move || {
            let (mut s1, _) = cdn_listener.accept().unwrap();
            s1.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut b1 = [0u8; 2048];
            let _ = s1.read(&mut b1);
            let _ = s1.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /edge\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );

            let (mut s2, _) = cdn_listener.accept().unwrap();
            s2.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut b2 = [0u8; 2048];
            let n = s2.read(&mut b2).unwrap();
            let head = std::str::from_utf8(&b2[..n]).unwrap().to_ascii_lowercase();
            assert!(head.starts_with("get /edge "));
            assert!(head.contains("\r\nrange: bytes=2097151-\r\n"));
            let _ = s2.write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2097151-2097151/2097152\r\nContent-Length: 1\r\nConnection: close\r\n\r\nZ",
            );
        });
        probe_tail_byte(&agent, &format!("{cdn_base}/start"), &BTreeMap::new(), 2_097_152).unwrap();
        cdn_server.join().unwrap();
    }

    #[test]
    fn collect_videos_stops_at_limit_and_skips_framework_updates() {
        let video = |id: &str| {
            json!({"videoRenderer": {
                "videoId": id,
                "title": {"simpleText": "Song"},
                "ownerText": {"simpleText": "Artist"}
            }})
        };
        let payload = json!({
            "responseContext": video("ignored0001"),
            "frameworkUpdates": video("ignored0002"),
            "contents": [video("dQw4w9WgXcQ"), video("1CXveaq_vFU"), video("f2u9F-IT9wY")]
        });
        let mut out = Vec::new();
        collect_videos_up_to(&payload, &mut out, 2);
        let ids: Vec<_> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["dQw4w9WgXcQ", "1CXveaq_vFU"]);
        assert!(next_continuation(&json!({
            "frameworkUpdates": {"continuationCommand": {"token": "wrong"}}
        }))
        .is_none());
    }

    #[test]
    fn video_metadata_caches_stream_so_open_and_seek_skip_duplicate_player_calls() {
        let future_expire =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
                + 7200;
        let (base, server) = crate::test_http::serve(1, move |_, _, body| {
            assert_eq!(body["videoId"], "1CXveaq_vFU");
            json!({
                "playabilityStatus": {"status": "OK"},
                "videoDetails": {
                    "videoId": "1CXveaq_vFU",
                    "title": "Cached Song",
                    "author": "Artist",
                    "lengthSeconds": "200"
                },
                "streamingData": {"adaptiveFormats": [{
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "bitrate": 160000,
                    "contentLength": "3200000",
                    "url": format!("https://rr1---sn-test.googlevideo.com/videoplayback?expire={future_expire}&id=1"),
                    "httpHeaders": {"X-Test": "cached"}
                }]}
            })
        });
        let state = CatalogState::default();
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let catalog = Catalog::with_state(&agent, &config, &state);
        let track = catalog.video("1CXveaq_vFU").unwrap();
        assert_eq!(track.metadata.title, "Cached Song");

        let mut opens = 0;
        for _ in 0..2 {
            let url = catalog
                .with_media_url("1CXveaq_vFU", |url, headers| {
                    opens += 1;
                    assert_eq!(headers["X-Test"], "cached");
                    Ok(url)
                })
                .unwrap();
            assert!(url.contains("googlevideo.com/videoplayback"));
        }
        server.join().unwrap();
        assert_eq!(opens, 2, "both initial open and seek used the cached stream");
        assert!(youtube_stream_ttl("https://rr1.googlevideo.com/videoplayback?expire=1").is_none());
    }

    #[test]
    fn rejected_cached_stream_skips_same_client_and_falls_back_cleanly() {
        let (base, server) = crate::test_http::serve(2, |_, _, body| {
            let client = body["context"]["client"]["clientName"].as_str().unwrap();
            json!({
                "playabilityStatus": {"status": "OK"},
                "videoDetails": {
                    "videoId": "1CXveaq_vFU",
                    "title": "Fallback Song",
                    "lengthSeconds": "180"
                },
                "streamingData": {"adaptiveFormats": [{
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "bitrate": 160000,
                    "url": format!("https://cdn.example/{client}")
                }]}
            })
        });
        let state = CatalogState::default();
        let mut config = Config::default();
        config.youtube_endpoint = base;
        let agent = netguard::agent(true);
        let catalog = Catalog::with_state(&agent, &config, &state);
        let _ = catalog.video("1CXveaq_vFU").unwrap();

        let mut attempts = Vec::new();
        let chosen = catalog
            .with_media_url("1CXveaq_vFU", |url, _| {
                attempts.push(url.clone());
                if url.ends_with("/IOS_PRODUCER") {
                    Err(SourceError::Media { status: 403 })
                } else {
                    Ok(url)
                }
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(attempts, ["https://cdn.example/IOS_PRODUCER", "https://cdn.example/VISIONOS"]);
        assert_eq!(chosen, "https://cdn.example/VISIONOS");
    }

    #[test]
    fn client_health_promotes_healthy_identities_while_retaining_cooled_down_fallbacks() {
        let state = CatalogState::default();
        let config = Config::default();
        state.youtube_health.record_failure("IOS_PRODUCER");
        let partitioned =
            state.youtube_health.partition_clients(clients(&config), |client| client.name);
        let names: Vec<_> = partitioned.iter().map(|c| c.name).collect();
        assert_eq!(names, ["VISIONOS", "ANDROID_VR", "ANDROID", "IOS_PRODUCER"]);

        state.youtube_health.record_success("IOS_PRODUCER");
        let restored =
            state.youtube_health.partition_clients(clients(&config), |client| client.name);
        let restored_names: Vec<_> = restored.iter().map(|c| c.name).collect();
        assert_eq!(restored_names, ["IOS_PRODUCER", "VISIONOS", "ANDROID_VR", "ANDROID"]);
    }

    #[test]
    fn shorts_lockup_and_reel_renderers_become_tracks() {
        let payload = json!({
            "contents": [
                {"reelItemRenderer": {
                    "videoId": "dQw4w9WgXcQ",
                    "headline": {"simpleText": "Short One"},
                    "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/1.jpg"}]}
                }},
                {"shortsLockupViewModel": {
                    "onTap": {"innertubeCommand": {"reelWatchEndpoint": {"videoId": "1CXveaq_vFU"}}},
                    "overlayMetadata": {"primaryText": {"content": "Short Two"}},
                    "thumbnailViewModel": {"thumbnailViewModel": {"image": {"sources": [{"url": "https://i.ytimg.com/2.jpg"}]}}}
                }}
            ]
        });
        let mut found = Vec::new();
        collect_videos(&payload, &mut found);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "dQw4w9WgXcQ");
        assert_eq!(found[0].title, "Short One");
        assert_eq!(found[1].id, "1CXveaq_vFU");
        assert_eq!(found[1].title, "Short Two");
    }
}
