//! The YouTube Music provider.
//!
//! The catalogue answers the same page in two shapes: the web app is given the
//! words as one run inside a description shelf, and the mobile app as timed
//! lines inside the elements framework. The mobile answer is asked for first
//! because a client can follow timed lines and cannot follow a wall of text.

use std::time::Duration;

use rustlink_protocol::{Object, Track};
use serde_json::{json, Value};

use crate::{Line, Lyrics, LyricsError, Provider};

/// The catalogue host both clients post to.
pub const YTMUSIC_ENDPOINT: &str = "https://music.youtube.com";

/// The web client version, the same one the source package searches the
/// catalogue with. Such versions age quickly, so the configuration overrides it.
const WEB_VERSION: &str = "1.20250101.01.00";

/// The Android app version that is still answered with timed lines.
///
/// The current app build is not required for this: the catalogue keeps answering
/// the version the library clients send.
const MOBILE_VERSION: &str = "7.21.50";

const WEB_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MOBILE_AGENT: &str =
    "com.google.android.apps.youtube.music/7.21.50 (Linux; U; Android 11) gzip";

/// Bounds one request. The mobile answer is a few hundred kilobytes, and a
/// client waiting for lyrics would rather have no answer than a slow one.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds a response, so a hostile or broken answer cannot fill memory.
const BODY_LIMIT: u64 = 8 << 20;

/// Marks the tab that holds the words. The title of the tab is localised, so the
/// page type is what identifies it.
const LYRICS_PAGE: &str = "MUSIC_PAGE_TYPE_TRACK_LYRICS";

/// Fetches lyrics from YouTube Music's own catalogue.
///
/// It only answers for YouTube tracks, because a video id is the only handle the
/// catalogue has.
pub struct YTMusic {
    pub agent: ureq::Agent,
    /// The catalogue host, so a test can point it at its own server.
    pub endpoint: String,
    /// Overrides the web client version.
    pub version: String,
    /// Attached when the node has one, the same way the source package attaches
    /// it.
    pub visitor_data: String,
}

/// One of the two identities the lyrics page is asked with. They are the same
/// endpoint and the same page; only the answer differs.
struct Client {
    name: &'static str,
    version: &'static str,
    agent: &'static str,
    /// The numeric client id YouTube expects in `X-YouTube-Client-Name`.
    id: &'static str,
    /// Adds the Android fields the app sends, which is what selects the answer
    /// that carries the timings.
    mobile: bool,
}

const WEB: Client =
    Client { name: "WEB_REMIX", version: WEB_VERSION, agent: WEB_AGENT, id: "67", mobile: false };
const MOBILE: Client = Client {
    name: "ANDROID_MUSIC",
    version: MOBILE_VERSION,
    agent: MOBILE_AGENT,
    id: "21",
    mobile: true,
};

impl Provider for YTMusic {
    fn name(&self) -> &'static str {
        "ytmusic"
    }

    /// Returns the lyrics the catalogue has for a track, or
    /// [`LyricsError::NotFound`] when it has none.
    ///
    /// The timed answer is tried first and the plain one is the fallback, so a
    /// track whose timings the catalogue withholds still gets its words.
    fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError> {
        let Some(video_id) = video_id(track) else {
            return Err(LyricsError::NotFound);
        };
        let page = self.page(&video_id)?;
        if page.is_empty() {
            return Err(LyricsError::NotFound);
        }
        // A shape that failed is not a reason to stop: the other one is a
        // separate request and may still answer. Only when neither has anything
        // is the failure raised, and the first one is the one worth reporting.
        let mut last: Option<LyricsError> = None;
        match self.ask(&MOBILE, &page) {
            Ok(answer) => {
                if let Some(lyrics) = from_mobile(track, &answer) {
                    return Ok(lyrics);
                }
            }
            Err(LyricsError::NotFound) => {}
            Err(error) => last = Some(error),
        }
        match self.ask(&WEB, &page) {
            Ok(answer) => {
                if let Some(lyrics) = from_web(track, &answer) {
                    return Ok(lyrics);
                }
            }
            Err(LyricsError::NotFound) => {}
            Err(error) => {
                if last.is_none() {
                    last = Some(error);
                }
            }
        }
        match last {
            Some(error) => Err(error),
            None => Err(LyricsError::NotFound),
        }
    }
}

impl YTMusic {
    /// Returns the lyrics page a track has, or an empty string when it has none.
    ///
    /// The words are not in this answer, so an empty result is the catalogue
    /// saying the track has no lyrics rather than a failure.
    fn page(&self, video_id: &str) -> Result<String, LyricsError> {
        let answer: Value = self.call(&WEB, "next", json!({ "videoId": video_id }))?;
        let tabs = answer
            .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")
            .and_then(Value::as_array);
        for tab in tabs.into_iter().flatten() {
            let endpoint = &tab["tabRenderer"]["endpoint"]["browseEndpoint"];
            let page_type = endpoint
                .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(Value::as_str);
            if page_type == Some(LYRICS_PAGE) {
                let browse_id =
                    endpoint.get("browseId").and_then(Value::as_str).unwrap_or_default();
                return Ok(browse_id.to_string());
            }
        }
        Ok(String::new())
    }

    /// Posts one browse request for the lyrics page.
    fn ask(&self, client: &Client, page: &str) -> Result<Value, LyricsError> {
        self.call(client, "browse", json!({ "browseId": page }))
    }

    /// Posts one InnerTube request and decodes the answer.
    fn call(&self, client: &Client, endpoint: &str, mut body: Value) -> Result<Value, LyricsError> {
        let version = if client.name == WEB.name && !self.version.trim().is_empty() {
            self.version.trim()
        } else {
            client.version
        };
        body["context"] = json!({ "client": client_context(client, version) });
        let base =
            if self.endpoint.trim().is_empty() { YTMUSIC_ENDPOINT } else { self.endpoint.trim() };
        let url =
            format!("{}/youtubei/v1/{}?prettyPrint=false", base.trim_end_matches('/'), endpoint);
        let mut request = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("User-Agent", client.agent)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("X-YouTube-Client-Name", client.id)
            .header("X-YouTube-Client-Version", version);
        if !self.visitor_data.trim().is_empty() {
            request = request.header("X-Goog-Visitor-Id", self.visitor_data.trim());
        }
        let response = request
            .config()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .send_json(&body)
            .map_err(|error| {
                LyricsError::Failed(format!("lyrics: youtube music could not be reached: {error}"))
            })?;
        match response.status().as_u16() {
            404 => return Err(LyricsError::NotFound),
            status if (200..300).contains(&status) => {}
            status => {
                return Err(LyricsError::Failed(format!(
                    "lyrics: youtube music answered {status}"
                )));
            }
        }
        let raw = response.into_body().with_config().limit(BODY_LIMIT).read_to_vec().map_err(
            |error| {
                LyricsError::Failed(format!(
                    "lyrics: youtube music sent an unreadable answer: {error}"
                ))
            },
        )?;
        serde_json::from_slice(&raw).map_err(|error| {
            LyricsError::Failed(format!("lyrics: youtube music sent an unreadable answer: {error}"))
        })
    }
}

/// Builds the client identity. Only the body carries it, the same as the source
/// package: the endpoint needs no API key.
fn client_context(client: &Client, version: &str) -> Value {
    let mut context =
        json!({ "clientName": client.name, "clientVersion": version, "hl": "en", "gl": "US" });
    if client.mobile {
        context["androidSdkVersion"] = json!(30);
        context["osName"] = json!("Android");
        context["osVersion"] = json!("11");
    }
    context
}

/// The id a track can be asked about, or `None` when the track is not one the
/// catalogue knows.
fn video_id(track: &Track) -> Option<String> {
    if track.metadata.source_name != "youtube" {
        return None;
    }
    let id = track.metadata.identifier.trim();
    let valid =
        id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| id.to_string())
}

/// Builds the payload from the mobile answer.
///
/// Timed lines are returned when every line carries a cue; an answer that has
/// the words without the timings is still worth returning as text, because a
/// wall of text beats nothing.
fn from_mobile(track: &Track, answer: &Value) -> Option<Lyrics> {
    let data = answer.pointer(
        "/contents/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData",
    )?;
    let lines = data.get("timedLyricsData").and_then(Value::as_array)?;
    if lines.is_empty() {
        return None;
    }
    let source = data.get("sourceMessage").and_then(Value::as_str).unwrap_or_default();
    let mut out = base(track, source);
    // Every line has to carry a cue for the answer to be the timed one. One line
    // without a cue means the catalogue has the words but not when they are
    // sung, which is still worth returning, as text.
    let timed = lines.iter().all(|line| line.get("cueRange").is_some_and(|cue| !cue.is_null()));
    if !timed {
        let words: Vec<&str> = lines
            .iter()
            .map(|line| line.get("lyricLine").and_then(Value::as_str).unwrap_or_default())
            .collect();
        let joined = words.join("\n");
        let joined = joined.trim();
        if joined.is_empty() {
            return None;
        }
        out.text = Some(joined.to_string());
        return Some(out);
    }
    let mut timed_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let start = millis(line.pointer("/cueRange/startTimeMilliseconds"));
        let end = millis(line.pointer("/cueRange/endTimeMilliseconds"));
        let duration = end.checked_sub(start).filter(|duration| *duration > 0);
        timed_lines.push(Line {
            timestamp: start,
            duration,
            line: line.get("lyricLine").and_then(Value::as_str).unwrap_or_default().to_string(),
            plugin: Object::new(),
        });
    }
    out.lines = Some(timed_lines);
    Some(out)
}

/// Builds the payload from all the text runs in the web description.
///
/// An answer with no run is the catalogue saying it has nothing, such as the
/// message it sends for a track whose lyrics are not licensed.
fn from_web(track: &Track, answer: &Value) -> Option<Lyrics> {
    let shelf =
        answer.pointer("/contents/sectionListRenderer/contents/0/musicDescriptionShelfRenderer")?;
    let runs = shelf.pointer("/description/runs").and_then(Value::as_array)?;
    let text: String =
        runs.iter().filter_map(|run| run.get("text").and_then(Value::as_str)).collect();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let footer = shelf.pointer("/footer/runs").and_then(Value::as_array);
    let source: String = footer
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect();
    let mut out = base(track, &source);
    out.text = Some(text.to_string());
    Some(out)
}

/// The payload both shapes share.
fn base(track: &Track, source: &str) -> Lyrics {
    let mut out = Lyrics::new(track.metadata.source_name.clone(), "ytmusic");
    out.plugin = source_object(source);
    out
}

/// Reports who supplied the words.
///
/// The catalogue credits the licensor, and a client showing lyrics should be
/// able to pass that on, so it travels in the plugin object rather than being
/// dropped.
fn source_object(message: &str) -> Object {
    let message = message.trim().trim_start_matches("Source:").trim();
    let mut object = Object::new();
    if !message.is_empty() {
        object.insert("source".into(), Value::String(message.to_string()));
    }
    object
}

/// Reads a millisecond count the catalogue sends as a string, and reads one sent
/// as a number just as well.
///
/// A negative or unreadable value is zero rather than an error, because one
/// unreadable timestamp should not cost the whole track its lyrics.
fn millis(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        _ => 0,
    }
    .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;

    fn track(source: &str, identifier: &str) -> Track {
        Track::new(
            String::new(),
            TrackInfo {
                identifier: identifier.into(),
                source_name: source.into(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn only_a_youtube_video_id_is_worth_asking_about() {
        assert_eq!(video_id(&track("youtube", "dQw4w9WgXcQ")).as_deref(), Some("dQw4w9WgXcQ"));
        assert!(video_id(&track("youtube", "too short")).is_none());
        assert!(video_id(&track("soundcloud", "dQw4w9WgXcQ")).is_none());
    }

    #[test]
    fn web_lyrics_keep_every_text_run() {
        let answer = json!({
            "contents": {"sectionListRenderer": {"contents": [{
                "musicDescriptionShelfRenderer": {"description": {"runs": [
                    {"text": " first line\n"}, {"text": "second "}, {"text": "line "}
                ]}}
            }]}}
        });
        let lyrics = from_web(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        assert_eq!(lyrics.text.as_deref(), Some("first line\nsecond line"));
    }

    #[test]
    fn a_timed_mobile_answer_becomes_timed_lines() {
        let answer = json!({
            "contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {
                "timedLyricsModel": {"lyricsData": {
                    "sourceMessage": "Source: Someone",
                    "timedLyricsData": [
                        {"lyricLine": "one", "cueRange": {"startTimeMilliseconds": "1000", "endTimeMilliseconds": "2000"}},
                        {"lyricLine": "two", "cueRange": {"startTimeMilliseconds": 2000, "endTimeMilliseconds": 3000}}
                    ]
                }}
            }}}}}}
        });
        let lyrics = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        let lines = lyrics.lines.unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].timestamp, 1000);
        assert_eq!(lines[0].duration, Some(1000));
        assert_eq!(lines[1].timestamp, 2000);
        assert_eq!(lyrics.plugin.get("source").and_then(Value::as_str), Some("Someone"));
    }

    #[test]
    fn malformed_mobile_cues_do_not_overflow_or_produce_negative_timestamps() {
        let answer = json!({
            "contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {
                "timedLyricsModel": {"lyricsData": {
                    "timedLyricsData": [
                        {"lyricLine": "one", "cueRange": {
                            "startTimeMilliseconds": i64::MIN.to_string(), "endTimeMilliseconds": i64::MAX}},
                        {"lyricLine": "two", "cueRange": {
                            "startTimeMilliseconds": i64::MAX, "endTimeMilliseconds": -1}}
                    ]
                }}
            }}}}}}
        });
        let lines = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap().lines.unwrap();
        assert_eq!(lines[0].timestamp, 0);
        assert_eq!(lines[0].duration, Some(i64::MAX));
        assert_eq!(lines[1].timestamp, i64::MAX);
        assert_eq!(lines[1].duration, None);
    }

    #[test]
    fn a_mobile_answer_without_cues_is_still_text() {
        let answer = json!({
            "contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {
                "timedLyricsModel": {"lyricsData": {
                    "sourceMessage": "",
                    "timedLyricsData": [{"lyricLine": "one"}, {"lyricLine": "two"}]
                }}
            }}}}}}
        });
        let lyrics = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        assert_eq!(lyrics.text.as_deref(), Some("one\ntwo"));
        assert!(lyrics.lines.is_none());
    }

    #[test]
    fn an_empty_mobile_answer_is_not_lyrics() {
        let answer = json!({
            "contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {
                "timedLyricsModel": {"lyricsData": {"timedLyricsData": []}}
            }}}}}}
        });
        assert!(from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).is_none());
    }

    #[test]
    fn a_web_answer_becomes_text() {
        let answer = json!({
            "contents": {"sectionListRenderer": {"contents": [{"musicDescriptionShelfRenderer": {
                "description": {"runs": [{"text": "the words"}]},
                "footer": {"runs": [{"text": "Source: "}, {"text": "Someone"}]}
            }}]}}
        });
        let lyrics = from_web(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        assert_eq!(lyrics.text.as_deref(), Some("the words"));
        assert_eq!(lyrics.plugin.get("source").and_then(Value::as_str), Some("Someone"));
    }

    #[test]
    fn a_web_answer_with_no_words_is_not_lyrics() {
        let answer = json!({"contents": {"sectionListRenderer": {"contents": []}}});
        assert!(from_web(&track("youtube", "dQw4w9WgXcQ"), &answer).is_none());
    }

    #[test]
    fn the_mobile_client_names_itself_as_the_android_app() {
        let context = client_context(&MOBILE, MOBILE_VERSION);
        assert_eq!(context["clientName"], "ANDROID_MUSIC");
        assert_eq!(context["osName"], "Android");
        assert!(client_context(&WEB, WEB_VERSION).get("osName").is_none());
    }
}
