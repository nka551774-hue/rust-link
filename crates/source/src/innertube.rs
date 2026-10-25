//! Native YouTube catalogue and media resolution over JSON HTTP endpoints.
//! Client identities are tried in order when a provider rejects one; failures
//! report the native API error without launching an external extractor.

use std::{borrow::Cow, collections::BTreeMap};

use rustlink_config::Config;
use rustlink_protocol::{encode_track, LoadResult, Playlist, Track, TrackInfo};
use serde_json::{json, Value};

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
    pub version: String,
    pub user_agent: Cow<'static, str>,
    pub sdk: Option<u32>,
    pub device_make: Option<&'static str>,
    pub device_model: Option<&'static str>,
    pub os_name: Option<&'static str>,
    pub os_version: Option<&'static str>,
}

/// The clients tried in order.
pub fn clients(config: &Config) -> Vec<Client> {
    let android_version = if config.youtube_client_version.trim().is_empty() {
        "20.10.38".to_string()
    } else {
        config.youtube_client_version.trim().to_string()
    };
    vec![
        Client {
            name: "ANDROID",
            // This version still returns directly usable audio for videos
            // newer Android identities expose only through SABR. Keep the
            // HTTP identity consistent with a configured version override.
            user_agent: Cow::Owned(format!(
                "com.google.android.youtube/{android_version} (Linux; U; Android 11) gzip"
            )),
            version: android_version,
            sdk: Some(30),
            device_make: None,
            device_model: None,
            os_name: Some("Android"),
            os_version: Some("11"),
        },
        Client {
            name: "ANDROID_VR",
            version: "1.65.10".into(),
            user_agent: Cow::Borrowed("com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip"),
            sdk: Some(32),
            device_make: Some("Oculus"),
            device_model: Some("Quest 3"),
            os_name: Some("Android"),
            os_version: Some("12L"),
        },
        Client {
            name: "IOS",
            version: "21.26.4".into(),
            user_agent: Cow::Borrowed("com.google.ios.youtube/21.26.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)"),
            sdk: None,
            device_make: Some("Apple"),
            device_model: Some("iPhone16,2"),
            os_name: Some("iPhone"),
            os_version: Some("18.3.2.22D82"),
        },
    ]
}

/// The client the catalogue is searched with. It needs no player script because
/// a search result carries a plain video id.
const MUSIC_CLIENT: Client = Client {
    name: "WEB_REMIX",
    version: String::new(),
    user_agent: Cow::Borrowed("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"),
    sdk: None,
    device_make: None,
    device_model: None,
    os_name: None,
    os_version: None,
};

const MUSIC_VERSION: &str = "1.20250101.01.00";

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

/// Posts one InnerTube request.
fn call(
    agent: &ureq::Agent,
    base: &str,
    client: &Client,
    version: &str,
    visitor_data: &str,
    endpoint: &str,
    mut body: Value,
) -> Result<Value, SourceError> {
    let mut identity = json!({
        "clientName": client.name,
        "clientVersion": version,
        "hl": "en",
        "gl": "US",
    });
    if !visitor_data.trim().is_empty() {
        identity["visitorData"] = json!(visitor_data.trim());
    }
    if let Some(sdk) = client.sdk {
        identity["androidSdkVersion"] = json!(sdk);
    }
    if let Some(make) = client.device_make {
        identity["deviceMake"] = json!(make);
    }
    if let Some(model) = client.device_model {
        identity["deviceModel"] = json!(model);
    }
    if let Some(os) = client.os_name {
        identity["osName"] = json!(os);
    }
    if let Some(os) = client.os_version {
        identity["osVersion"] = json!(os);
    }
    body["context"] = json!({ "client": identity });
    let url = format!("{}/youtubei/v1/{endpoint}?prettyPrint=false", base.trim_end_matches('/'));
    netguard::validate_url(&url)?;
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", client.user_agent.as_ref())
        .header("Accept-Language", "en-US,en;q=0.9");
    if !visitor_data.trim().is_empty() {
        request = request.header("X-Goog-Visitor-Id", visitor_data.trim());
    }
    let response = request
        .config()
        .timeout_global(Some(deadline::timeout(std::time::Duration::from_secs(20))?))
        .http_status_as_error(false)
        .build()
        .send_json(&body)
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

/// A catalogue request, which carries the configuration a call needs.
pub struct Catalog<'a> {
    pub agent: &'a ureq::Agent,
    pub config: &'a Config,
}

impl Catalog<'_> {
    fn visitor_data(&self) -> &str {
        &self.config.youtube_visitor_data
    }

    fn web_call(&self, client: &Client, endpoint: &str, body: Value) -> Result<Value, SourceError> {
        call(
            self.agent,
            &self.config.youtube_endpoint,
            client,
            &client.version,
            self.visitor_data(),
            endpoint,
            body,
        )
    }

    fn music_call(&self, endpoint: &str, body: Value) -> Result<Value, SourceError> {
        call(
            self.agent,
            &self.config.youtube_music_endpoint,
            &MUSIC_CLIENT,
            MUSIC_VERSION,
            self.visitor_data(),
            endpoint,
            body,
        )
    }

    /// Searches the catalogue for videos.
    pub fn search(&self, query: &str, want: usize) -> Result<Vec<Track>, SourceError> {
        let client = clients(self.config).remove(0);
        let body = json!({ "query": query, "params": SEARCH_PARAMS });
        let answer = self.web_call(&client, "search", body)?;
        let mut found = Vec::new();
        collect_videos(&answer, &mut found);
        Ok(found.into_iter().take(want).map(|media| media.track("youtube")).collect())
    }

    /// Searches YouTube Music, which finds the official recording where the
    /// ordinary catalogue finds a reupload.
    pub fn music_search(&self, query: &str, want: usize) -> Result<Vec<Track>, SourceError> {
        let body = json!({ "query": query, "params": MUSIC_SONGS_PARAMS });
        let answer = self.music_call("search", body)?;
        let mut found = Vec::new();
        collect_videos(&answer, &mut found);
        Ok(found.into_iter().take(want).map(|media| media.track("youtube")).collect())
    }

    /// Reads a playlist, up to the configured number of tracks.
    ///
    /// A playlist is paged, and each page names the next one, so the walk stops
    /// on a page count rather than on the playlist ending: a client asking for a
    /// thousand track playlist should not hold the load open while the node
    /// reads all of it.
    pub fn playlist(&self, playlist_id: &str) -> Result<LoadResult, SourceError> {
        let client = clients(self.config).remove(0);
        let mut videos = Vec::new();
        let mut title = String::new();
        let mut continuation = String::new();
        let mut pages = 0;
        loop {
            let answer = if continuation.is_empty() {
                self.web_call(&client, "browse", json!({ "browseId": format!("VL{playlist_id}") }))?
            } else {
                self.web_call(&client, "browse", json!({ "continuation": continuation }))?
            };
            if title.is_empty() {
                title = header_title(&answer).unwrap_or_default();
            }
            collect_videos(&answer, &mut videos);
            pages += 1;
            if videos.len() >= self.config.max_playlist_tracks as usize
                || pages >= MAX_PLAYLIST_PAGES
            {
                break;
            }
            match next_continuation(&answer) {
                Some(token) => continuation = token,
                None => break,
            }
        }
        dedupe(&mut videos);
        let tracks: Vec<Track> = videos
            .into_iter()
            .take(self.config.max_playlist_tracks as usize)
            .map(|media| media.track("youtube"))
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

    /// Tries the next client when a CDN rejects the URL a client returned.
    /// Resolving a URL alone does not prove that the audio can be opened.
    pub(crate) fn with_media_url<T>(
        &self,
        video_id: &str,
        mut open: impl FnMut(String, BTreeMap<String, String>) -> Result<T, SourceError>,
    ) -> Result<T, SourceError> {
        let mut first: Option<SourceError> = None;
        let mut stream_error = None;
        for client in clients(self.config) {
            match self.player(&client, video_id) {
                Ok(Some(format)) => {
                    let mut headers = format.headers;
                    headers.retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
                    headers.insert("User-Agent".into(), client.user_agent.to_string());
                    match open(format.url, headers) {
                        Ok(stream) => return Ok(stream),
                        Err(error) if error.retryable() => {
                            if stream_error.is_none() {
                                stream_error = Some(error);
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(None) => {}
                Err(error) => {
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

    fn player_body(&self, video_id: &str) -> Value {
        let mut body = json!({ "videoId": video_id, "contentCheckOk": true, "racyCheckOk": true });
        let token = self.config.youtube_po_token.trim();
        if !token.is_empty() {
            body["serviceIntegrityDimensions"] = json!({ "poToken": token });
        }
        body
    }

    /// Asks one client for a video's streams.
    pub fn player(&self, client: &Client, video_id: &str) -> Result<Option<Format>, SourceError> {
        let answer = self.web_call(client, "player", self.player_body(video_id))?;
        check_playability(&answer)?;
        check_video_id(&answer, video_id)?;
        Ok(pick_format(&answer))
    }

    /// Reads a video's own metadata, which is how a bare video id becomes a
    /// track without a search.
    pub fn video(&self, video_id: &str) -> Result<Track, SourceError> {
        let mut first = None;
        for client in clients(self.config) {
            match self.video_with_client(&client, video_id) {
                Ok(track) => return Ok(track),
                Err(error) => {
                    if first.is_none() {
                        first = Some(error);
                    }
                }
            }
        }
        Err(first.unwrap_or_else(|| SourceError::Http("youtube described no video".into())))
    }

    fn video_with_client(&self, client: &Client, video_id: &str) -> Result<Track, SourceError> {
        let answer = self.web_call(client, "player", self.player_body(video_id))?;
        check_playability(&answer)?;
        check_video_id(&answer, video_id)?;
        let details = answer
            .pointer("/videoDetails")
            .ok_or_else(|| SourceError::Http("youtube described no video".into()))?;
        let media = Media {
            id: details.get("videoId").and_then(Value::as_str).unwrap_or(video_id).to_string(),
            title: details.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            author: details.get("author").and_then(Value::as_str).unwrap_or_default().to_string(),
            seconds: details
                .get("lengthSeconds")
                .and_then(Value::as_str)
                .and_then(|text| text.parse().ok())
                .unwrap_or(0.0),
            live: details.get("isLiveContent").and_then(Value::as_bool).unwrap_or(false),
            artwork: thumbnails(answer.pointer("/videoDetails/thumbnail")),
        };
        Ok(media.track("youtube"))
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
    fn track(&self, source: &str) -> Track {
        let author =
            if self.author.is_empty() { "Unknown artist".to_string() } else { self.author.clone() };
        let info = TrackInfo {
            identifier: self.id.clone(),
            title: self.title.clone(),
            author,
            length: (self.seconds * 1000.0) as i64,
            is_stream: self.live,
            is_seekable: !self.live,
            uri: Some(format!("https://www.youtube.com/watch?v={}", self.id)),
            artwork_url: self.artwork.clone(),
            source_name: source.to_string(),
            ..Default::default()
        };
        // A track whose metadata cannot be encoded is one the node cannot send,
        // and an empty encoding is what a client reads as a broken track.
        encode_track(&info).unwrap_or_else(|_| Track::new(String::new(), info))
    }
}

/// Picks the best audio-only stream out of a player answer.
///
/// A progressive format carries audio and video together, which is twice the
/// bytes for the same sound; an adaptive audio-only format is what a node that
/// never shows a picture wants.
fn pick_format(answer: &Value) -> Option<Format> {
    let formats = answer.pointer("/streamingData/adaptiveFormats").and_then(Value::as_array)?;
    let mut best: Option<Format> = None;
    let mut best_rank = None;
    for entry in formats {
        let mime = entry.get("mimeType").and_then(Value::as_str).unwrap_or_default();
        if !mime.starts_with("audio/") || !has_audio_codec(mime) {
            continue;
        }
        let Some(url) = entry.get("url").and_then(Value::as_str) else {
            // A format with no URL is one YouTube will only deliver over its own
            // streaming protocol, which the node does not speak.
            continue;
        };
        let bitrate = entry.get("bitrate").and_then(Value::as_i64).unwrap_or(0);
        // The request locale can make an automatic dub the default track.
        // Preserve the original performance and dynamics before comparing
        // bitrate; never infer a language from YouTube's opaque track IDs.
        let dubbed =
            entry.pointer("/audioTrack/isAutoDubbed").and_then(Value::as_bool).unwrap_or(false);
        let original = entry
            .pointer("/audioTrack/displayName")
            .and_then(Value::as_str)
            .is_some_and(|name| name.ends_with(" original"));
        let drc = entry.get("isDrc").and_then(Value::as_bool).unwrap_or(false);
        let rank = (!dubbed, original, !drc, bitrate);
        if best_rank.is_some_and(|held| held >= rank) {
            continue;
        }
        best_rank = Some(rank);
        let mut headers = BTreeMap::new();
        if let Some(extra) = entry.get("httpHeaders").and_then(Value::as_object) {
            for (name, value) in extra {
                if let Some(value) = value.as_str() {
                    headers.insert(name.clone(), value.to_string());
                }
            }
        }
        best = Some(Format {
            url: url.to_string(),
            mime_type: mime.to_string(),
            bitrate,
            content_length: entry
                .get("contentLength")
                .and_then(|value| value.as_str())
                .and_then(|text| text.parse().ok())
                .unwrap_or(0),
            headers,
        });
    }
    best
}

/// Whether a MIME type names a codec the node can decode.
///
/// The player offers some formats in codecs nothing but a browser has, and
/// handing one to the decoder would fail a track that has a playable format next
/// to it.
fn has_audio_codec(mime: &str) -> bool {
    const PLAYABLE: [&str; 5] = ["opus", "vorbis", "mp4a", "flac", "aac"];
    let lower = mime.to_ascii_lowercase();
    PLAYABLE.iter().any(|codec| lower.contains(codec))
}

fn thumbnails(value: Option<&Value>) -> Option<String> {
    let list = value?.get("thumbnails")?.as_array()?;
    // The last entry is the largest.
    list.last()?.get("url")?.as_str().map(str::to_string)
}

/// Reads a written duration such as "3:45" or "1:02:03" into seconds.
fn length_seconds(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0f64;
    for part in parts {
        let value: f64 = part.trim().parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
        if !total.is_finite() {
            return None;
        }
    }
    Some(total)
}

/// Reads the words of a renderer field that may be one run or a simple string.
fn runs_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(runs) = value.get("runs").and_then(Value::as_array) {
        return runs
            .iter()
            .filter_map(|run| run.get("text").and_then(Value::as_str))
            .collect::<String>();
    }
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    String::new()
}

/// Walks an answer and collects every video it names.
///
/// The catalogue nests its results differently for every surface, and a search
/// for a song can come back as any of half a dozen renderer shapes. Looking for
/// the shapes rather than for the path they sit at is what keeps one parser
/// working across all of them.
pub fn collect_videos(value: &Value, out: &mut Vec<Media>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match key.as_str() {
                    // Promoted results can contain the same video renderer
                    // shape as organic results. They are never a song search
                    // result or a playlist entry, so skip the whole subtree.
                    "adSlotRenderer"
                    | "adPlacementRenderer"
                    | "inFeedAdLayoutRenderer"
                    | "promotedSparklesWebRenderer"
                    | "promotedVideoRenderer"
                    | "playerAds"
                    | "adPlacements"
                    | "adSlots" => continue,
                    "videoRenderer" | "compactVideoRenderer" | "playlistVideoRenderer" => {
                        if let Some(media) = video_from_renderer(child) {
                            out.push(media);
                        }
                    }
                    "musicResponsiveListItemRenderer" => {
                        if let Some(media) = video_from_music_item(child) {
                            out.push(media);
                        }
                    }
                    _ => {}
                }
                collect_videos(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_videos(item, out);
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
    let title = runs_text(value.get("title"));
    if title.is_empty() {
        return None;
    }
    let author = ["ownerText", "longBylineText", "shortBylineText"]
        .iter()
        .map(|field| runs_text(value.get(*field)))
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    let seconds = value
        .get("lengthText")
        .and_then(|text| text.get("simpleText"))
        .and_then(Value::as_str)
        .and_then(length_seconds)
        .unwrap_or(0.0);
    Some(Media {
        id: id.to_string(),
        title,
        author,
        seconds,
        live: value.get("isLiveNow").and_then(Value::as_bool).unwrap_or(false),
        artwork: thumbnails(value.get("thumbnail")),
    })
}

/// Reads a YouTube Music list item, whose fields are in flex columns.
fn video_from_music_item(value: &Value) -> Option<Media> {
    let columns = value.get("flexColumns").and_then(Value::as_array)?;
    let id = columns
        .iter()
        .filter_map(|column| {
            column
                .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs")
                .and_then(Value::as_array)
                .and_then(|runs| {
                    runs.iter().find_map(|run| {
                        run.pointer("/navigationEndpoint/watchEndpoint/videoId")
                            .and_then(Value::as_str)
                    })
                })
        })
        .next()?;
    if !youtube_id(id) {
        return None;
    }
    let title = columns
        .first()
        .map(|column| runs_text(column.pointer("/musicResponsiveListItemFlexColumnRenderer/text")))
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    // The second column lists the artist and then the album, separated by a
    // middle dot, so the artist is the part before it.
    let byline = columns
        .get(1)
        .map(|column| runs_text(column.pointer("/musicResponsiveListItemFlexColumnRenderer/text")))
        .unwrap_or_default();
    let author = byline.split('•').next().unwrap_or_default().trim().to_string();
    let seconds = value
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text")
        .and_then(Value::as_str)
        .and_then(length_seconds)
        .unwrap_or(0.0);
    Some(Media { id: id.to_string(), title, author, seconds, live: false, artwork: None })
}

/// Reads the title of a playlist page.
fn header_title(value: &Value) -> Option<String> {
    for path in [
        "/header/playlistHeaderRenderer/title",
        "/header/pageHeaderRenderer/pageTitle",
        "/metadata/playlistMetadataRenderer/title",
    ] {
        let text = runs_text(value.pointer(path));
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// Finds the token that names the next page of a list.
fn next_continuation(value: &Value) -> Option<String> {
    fn walk(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => {
                if let Some(token) = map
                    .get("nextContinuationData")
                    .and_then(|data| data.get("continuation"))
                    .and_then(Value::as_str)
                {
                    return Some(token.to_string());
                }
                if let Some(token) = map
                    .get("continuationCommand")
                    .and_then(|data| data.get("token"))
                    .and_then(Value::as_str)
                {
                    return Some(token.to_string());
                }
                map.values().find_map(walk)
            }
            Value::Array(items) => items.iter().find_map(walk),
            _ => None,
        }
    }
    walk(value)
}

fn dedupe(videos: &mut Vec<Media>) {
    let mut seen = std::collections::HashSet::new();
    videos.retain(|media| seen.insert(media.id.clone()));
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
                assert_eq!(body["context"]["client"]["clientName"], "ANDROID");
                json!({"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"client refused"}})
            } else {
                assert_eq!(body["context"]["client"]["clientName"], "ANDROID_VR");
                json!({"playabilityStatus":{"status":"OK"},"videoDetails":{
                    "videoId":"1CXveaq_vFU","title":"Shared video","author":"Artist","lengthSeconds":"180"
                }})
            }
        });
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let result = Catalog { agent: &agent, config: &config }.video("1CXveaq_vFU");
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
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let result = Catalog { agent: &agent, config: &config }.video("1CXveaq_vFU");
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
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let mut opened = 0;
        let result = Catalog { agent: &agent, config: &config }.with_media_url(
            "1CXveaq_vFU",
            |url, headers| {
                opened += 1;
                assert_eq!(
                    headers.keys().filter(|name| name.eq_ignore_ascii_case("user-agent")).count(),
                    1
                );
                assert_eq!(headers["X-Media-Fixture"], "preserved");
                if opened == 1 {
                    assert!(url.ends_with("/ANDROID"));
                    Err(SourceError::Media { status: 403 })
                } else {
                    assert!(url.ends_with("/ANDROID_VR"));
                    assert!(headers["User-Agent"].contains("youtube.vr"));
                    Ok(url)
                }
            },
        );
        server.join().unwrap();
        assert!(result.unwrap().ends_with("/ANDROID_VR"));
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
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let mut opened = 0;
        let result =
            Catalog { agent: &agent, config: &config }.with_media_url("1CXveaq_vFU", |url, _| {
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
        let (base, server) = crate::test_http::serve(3, move |_, _, _| {
            request += 1;
            if request == 1 {
                json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{
                    "mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/audio","bitrate":160000
                }]}})
            } else {
                json!({"playabilityStatus":{"status":"ERROR","reason":"client refused"}})
            }
        });
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let result = Catalog { agent: &agent, config: &config }
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
        let config = Config { youtube_endpoint: base, ..Config::default() };
        let agent = netguard::agent(true);
        let result = Catalog { agent: &agent, config: &config }
            .with_media_url::<()>("1CXveaq_vFU", |_, _| {
                Err(SourceError::Url("invalid media URL".into()))
            });
        server.join().unwrap();
        assert!(matches!(result, Err(SourceError::Url(_))));
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
                Some("ANDROID")
            );
            assert_eq!(
                body.pointer("/serviceIntegrityDimensions/poToken").and_then(Value::as_str),
                Some("player-token-fixture")
            );
            json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[
                {"mimeType":"audio/webm; codecs=\"opus\"","url":"https://cdn.example/audio","bitrate":160000}
            ]}})
        });
        let config = Config {
            youtube_endpoint: base,
            youtube_po_token: "player-token-fixture".into(),
            youtube_visitor_data: "visitor-fixture".into(),
            ..Config::default()
        };
        let agent = netguard::agent(true);
        let result =
            Catalog { agent: &agent, config: &config }.player(&clients(&config)[0], "dQw4w9WgXcQ");
        server.join().unwrap();
        assert_eq!(result.unwrap().unwrap().url, "https://cdn.example/audio");
        let config = Config::default();
        assert!(Catalog { agent: &agent, config: &config }
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
        assert_eq!(next_continuation(&answer).as_deref(), Some("next"));
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
    fn the_android_client_is_tried_first() {
        let config = Config::default();
        let clients = clients(&config);
        assert_eq!(clients[0].name, "ANDROID");
        assert_eq!(clients[0].version, "20.10.38");
        assert!(clients[0].user_agent.contains("youtube/20.10.38 "));
        assert_eq!(clients.len(), 3);
    }

    #[test]
    fn a_configured_client_version_replaces_the_built_in_one() {
        let config = Config { youtube_client_version: "99.1.2".into(), ..Config::default() };
        assert_eq!(clients(&config)[0].version, "99.1.2");
        assert!(clients(&config)[0].user_agent.contains("youtube/99.1.2 "));
    }
}
