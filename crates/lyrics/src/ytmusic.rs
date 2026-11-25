//! YouTube Music synced and plain lyrics provider.

use std::sync::Arc;
use std::time::Duration;

use rustlink_protocol::{Object, Track};
use serde_json::{json, Value};

use crate::{trim_owned, Line, Lyrics, LyricsError, Provider};

pub const YTMUSIC_ENDPOINT: &str = "https://music.youtube.com";
const WEB_VERSION: &str = "1.20250101.01.00";
const MOBILE_VERSION: &str = "7.21.50";
const WEB_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MOBILE_AGENT: &str =
    "com.google.android.apps.youtube.music/7.21.50 (Linux; U; Android 11) gzip";
const TIMEOUT: Duration = Duration::from_secs(10);
const BODY_LIMIT: u64 = 8 << 20;
const MAX_TIMED_LINES: usize = 10_000;
const LYRICS_PAGE: &str = "MUSIC_PAGE_TYPE_TRACK_LYRICS";

pub type VisitorReader = Arc<dyn Fn() -> Option<String> + Send + Sync>;
pub type VisitorWriter = Arc<dyn Fn(&str) + Send + Sync>;
pub type VisitorBridge = (VisitorReader, VisitorWriter);

pub struct YTMusic {
    pub agent: ureq::Agent,
    pub endpoint: String,
    pub version: String,
    pub visitor_data: String,
    pub visitor_reader: Option<VisitorReader>,
    pub visitor_writer: Option<VisitorWriter>,
}

struct Client {
    name: &'static str,
    version: &'static str,
    agent: &'static str,
    id: &'static str,
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

    fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError> {
        let video_id = video_id(track).ok_or(LyricsError::NotFound)?;
        let (page, visitor_data) = self.page(video_id)?.ok_or(LyricsError::NotFound)?;
        let mut last = None;
        for (client, parse) in
            [(&MOBILE, from_mobile as fn(&Track, &Value) -> Option<Lyrics>), (&WEB, from_web)]
        {
            match self.ask(client, &page, &visitor_data) {
                Ok(answer) => {
                    if let Some(lyrics) = parse(track, &answer) {
                        return Ok(lyrics);
                    }
                }
                Err(LyricsError::NotFound) => {}
                Err(e) => {
                    last.get_or_insert(e);
                }
            }
        }
        Err(last.unwrap_or(LyricsError::NotFound))
    }
}

impl YTMusic {
    fn page(&self, video_id: &str) -> Result<Option<(String, String)>, LyricsError> {
        let configured = self.visitor_data.trim();
        let shared = configured
            .is_empty()
            .then(|| {
                self.visitor_reader
                    .as_ref()
                    .and_then(|r| r())
                    .filter(|v| crate::valid_header_value(v))
            })
            .flatten()
            .unwrap_or_default();
        let initial_vd = if !configured.is_empty() { configured } else { shared.as_str() };
        let answer = self.call(&WEB, "next", initial_vd, &json!({ "videoId": video_id }))?;
        let harvested = answer
            .pointer("/responseContext/visitorData")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|vd| {
                !vd.is_empty()
                    && vd.len() <= 512
                    && vd.bytes().all(|b| (0x21..=0x7e).contains(&b))
                    && crate::valid_header_value(vd)
            });
        if let (true, Some(vd), Some(write)) =
            (configured.is_empty(), harvested, self.visitor_writer.as_ref())
        {
            write(vd);
        }
        let effective_vd = if !configured.is_empty() {
            configured.to_owned()
        } else if let Some(vd) = harvested {
            vd.to_owned()
        } else {
            shared
        };
        let tabs = answer
            .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")
            .and_then(Value::as_array);
        for tab in tabs.into_iter().flatten() {
            let endpoint = &tab["tabRenderer"]["endpoint"]["browseEndpoint"];
            if endpoint
                .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(Value::as_str)
                == Some(LYRICS_PAGE)
            {
                return Ok(endpoint
                    .get("browseId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(|id| (id.to_owned(), effective_vd)));
            }
        }
        Ok(None)
    }

    fn ask(&self, client: &Client, page: &str, visitor_data: &str) -> Result<Value, LyricsError> {
        self.call(client, "browse", visitor_data, &json!({ "browseId": page }))
    }

    fn call(
        &self,
        client: &Client,
        endpoint: &str,
        visitor_data: &str,
        body: &Value,
    ) -> Result<Value, LyricsError> {
        let version = if client.name == WEB.name && !self.version.trim().is_empty() {
            self.version.trim()
        } else {
            client.version
        };
        let visitor = visitor_data.trim();
        if !crate::valid_header_value(version) || !crate::valid_header_value(visitor) {
            return Err(LyricsError::Failed("lyrics: invalid YouTube Music header value".into()));
        }
        let raw_base =
            if self.endpoint.trim().is_empty() { YTMUSIC_ENDPOINT } else { self.endpoint.trim() };
        let base = crate::validate_base_url(raw_base, "youtube music")?;
        #[derive(serde::Serialize)]
        struct Envelope<'a> {
            context: Value,
            #[serde(flatten)]
            body: &'a Value,
        }
        let payload = Envelope {
            context: json!({ "client": client_context(client, version, visitor) }),
            body,
        };
        let mut req = self
            .agent
            .post(format!("{base}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("Content-Type", "application/json")
            .header("User-Agent", client.agent)
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("X-YouTube-Client-Name", client.id)
            .header("X-YouTube-Client-Version", version);
        if !visitor.is_empty() {
            req = req.header("X-Goog-Visitor-Id", visitor);
        }
        let resp = req
            .config()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .send_json(&payload)
            .map_err(|e| {
                LyricsError::Failed(format!("lyrics: youtube music could not be reached: {e}"))
            })?;
        match resp.status().as_u16() {
            404 => return Err(LyricsError::NotFound),
            s if (200..300).contains(&s) => {}
            s => return Err(LyricsError::Failed(format!("lyrics: youtube music answered {s}"))),
        }
        let raw = resp.into_body().with_config().limit(BODY_LIMIT).read_to_vec().map_err(|e| {
            LyricsError::Failed(format!("lyrics: youtube music sent an unreadable answer: {e}"))
        })?;
        serde_json::from_slice(&raw).map_err(|e| {
            LyricsError::Failed(format!("lyrics: youtube music sent an unreadable answer: {e}"))
        })
    }
}

fn client_context(client: &Client, version: &str, visitor_data: &str) -> Value {
    let mut ctx =
        json!({ "clientName": client.name, "clientVersion": version, "hl": "en", "gl": "US" });
    if client.mobile {
        ctx["androidSdkVersion"] = json!(30);
        ctx["osName"] = json!("Android");
        ctx["osVersion"] = json!("11");
    }
    if !visitor_data.is_empty() {
        ctx["visitorData"] = json!(visitor_data);
    }
    ctx
}

fn video_id(track: &Track) -> Option<&str> {
    let id = track.metadata.identifier.trim();
    (track.metadata.source_name == "youtube"
        && id.len() == 11
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
    .then_some(id)
}

fn find_key<'a>(value: &'a Value, target: &str, depth: usize) -> Option<&'a Value> {
    if depth == 0 {
        return None;
    }
    match value {
        Value::Object(map) => {
            map.get(target).or_else(|| map.values().find_map(|c| find_key(c, target, depth - 1)))
        }
        Value::Array(items) => items.iter().find_map(|c| find_key(c, target, depth - 1)),
        _ => None,
    }
}

fn from_mobile(track: &Track, answer: &Value) -> Option<Lyrics> {
    let data = answer
        .pointer("/contents/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData")
        .or_else(|| {
            answer
                .get("contents")
                .and_then(|c| find_key(c, "timedLyricsModel", 12))
                .and_then(|m| m.get("lyricsData"))
        })?;
    let lines = data.get("timedLyricsData").and_then(Value::as_array).filter(|l| !l.is_empty())?;
    let mut out =
        base(track, data.get("sourceMessage").and_then(Value::as_str).unwrap_or_default());
    let timed = lines.iter().all(|l| l.get("cueRange").is_some_and(|c| !c.is_null()))
        && lines.iter().any(|l| {
            millis(l.pointer("/cueRange/startTimeMilliseconds")) > 0
                || millis(l.pointer("/cueRange/endTimeMilliseconds")) > 0
        });
    if !timed {
        let mut joined = String::new();
        for (i, line) in lines.iter().take(MAX_TIMED_LINES).enumerate() {
            if i > 0 {
                joined.push('\n');
            }
            joined.push_str(line.get("lyricLine").and_then(Value::as_str).unwrap_or_default());
        }
        let joined = trim_owned(joined);
        if joined.is_empty() {
            return None;
        }
        out.text = Some(joined);
        return Some(out);
    }
    let mut timed_lines = Vec::with_capacity(lines.len().min(MAX_TIMED_LINES));
    for line in lines.iter().take(MAX_TIMED_LINES) {
        let start = millis(line.pointer("/cueRange/startTimeMilliseconds"));
        let end = millis(line.pointer("/cueRange/endTimeMilliseconds"));
        timed_lines.push(Line {
            timestamp: start,
            duration: end.checked_sub(start).filter(|&d| d > 0),
            line: line.get("lyricLine").and_then(Value::as_str).unwrap_or_default().to_owned(),
            plugin: Object::new(),
        });
    }
    out.lines = Some(timed_lines);
    Some(out)
}

fn from_web(track: &Track, answer: &Value) -> Option<Lyrics> {
    let shelf = answer
        .pointer("/contents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .and_then(|c| c.iter().find_map(|i| i.get("musicDescriptionShelfRenderer")))
        .or_else(|| {
            answer.get("contents").and_then(|c| find_key(c, "musicDescriptionShelfRenderer", 10))
        })?;
    let runs = shelf.pointer("/description/runs").and_then(Value::as_array)?;
    let text =
        trim_owned(runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)).collect());
    if text.is_empty() {
        return None;
    }
    let source: String = shelf
        .pointer("/footer/runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("text").and_then(Value::as_str))
        .collect();
    let mut out = base(track, &source);
    out.text = Some(text);
    Some(out)
}

fn base(track: &Track, source: &str) -> Lyrics {
    let mut out = Lyrics::new(track.metadata.source_name.clone(), "ytmusic");
    let msg = source.trim().trim_start_matches("Source:").trim();
    if !msg.is_empty() {
        out.plugin.insert("source".into(), Value::String(msg.to_owned()));
    }
    out
}

fn millis(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
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

    fn mobile_fixture(lyrics_data: Value) -> Value {
        json!({
            "contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {
                "timedLyricsModel": {"lyricsData": lyrics_data}
            }}}}}}
        })
    }

    #[test]
    fn only_a_youtube_video_id_is_worth_asking_about() {
        assert_eq!(video_id(&track("youtube", "dQw4w9WgXcQ")), Some("dQw4w9WgXcQ"));
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
        assert_eq!(
            from_web(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap().text.as_deref(),
            Some("first line\nsecond line")
        );
    }

    #[test]
    fn a_timed_mobile_answer_becomes_timed_lines() {
        let answer = mobile_fixture(json!({
            "sourceMessage": "Source: Someone",
            "timedLyricsData": [
                {"lyricLine": "one", "cueRange": {"startTimeMilliseconds": "1000", "endTimeMilliseconds": "2000"}},
                {"lyricLine": "two", "cueRange": {"startTimeMilliseconds": 2000, "endTimeMilliseconds": 3000}}
            ]
        }));
        let lyrics = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        let lines = lyrics.lines.unwrap();
        assert_eq!(
            (lines.len(), lines[0].timestamp, lines[0].duration, lines[1].timestamp),
            (2, 1000, Some(1000), 2000)
        );
        assert_eq!(lyrics.plugin.get("source").and_then(Value::as_str), Some("Someone"));
    }

    #[test]
    fn malformed_mobile_cues_do_not_overflow_or_produce_negative_timestamps() {
        let answer = mobile_fixture(json!({
            "timedLyricsData": [
                {"lyricLine": "one", "cueRange": {"startTimeMilliseconds": i64::MIN.to_string(), "endTimeMilliseconds": i64::MAX}},
                {"lyricLine": "two", "cueRange": {"startTimeMilliseconds": i64::MAX, "endTimeMilliseconds": -1}}
            ]
        }));
        let lines = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap().lines.unwrap();
        assert_eq!((lines[0].timestamp, lines[0].duration), (0, Some(i64::MAX)));
        assert_eq!((lines[1].timestamp, lines[1].duration), (i64::MAX, None));
    }

    #[test]
    fn a_mobile_answer_without_cues_is_still_text() {
        let answer = mobile_fixture(json!({
            "sourceMessage": "",
            "timedLyricsData": [{"lyricLine": "one"}, {"lyricLine": "two"}]
        }));
        let lyrics = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &answer).unwrap();
        assert_eq!(lyrics.text.as_deref(), Some("one\ntwo"));
        assert!(lyrics.lines.is_none());
    }

    #[test]
    fn an_empty_mobile_answer_is_not_lyrics() {
        assert!(from_mobile(
            &track("youtube", "dQw4w9WgXcQ"),
            &mobile_fixture(json!({"timedLyricsData": []}))
        )
        .is_none());
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
        assert!(from_web(
            &track("youtube", "dQw4w9WgXcQ"),
            &json!({"contents": {"sectionListRenderer": {"contents": []}}})
        )
        .is_none());
    }

    #[test]
    fn the_mobile_client_names_itself_as_the_android_app() {
        let ctx = client_context(&MOBILE, MOBILE_VERSION, "visitor-token");
        assert_eq!(ctx["clientName"], "ANDROID_MUSIC");
        assert_eq!(ctx["osName"], "Android");
        assert_eq!(ctx["visitorData"], "visitor-token");
        let web = client_context(&WEB, WEB_VERSION, "");
        assert!(web.get("osName").is_none() && web.get("visitorData").is_none());
    }

    #[test]
    fn single_line_zero_cue_becomes_plain_text_and_next_visitor_data_is_forwarded() {
        let zero_cue = mobile_fixture(json!({
            "timedLyricsData": [{
                "lyricLine": "untimed single line",
                "cueRange": {"startTimeMilliseconds": "0", "endTimeMilliseconds": "0"}
            }]
        }));
        let parsed = from_mobile(&track("youtube", "dQw4w9WgXcQ"), &zero_cue).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("untimed single line"));
        assert!(parsed.lines.is_none());

        let (endpoint, server) = crate::serve_http(2, |step, head, raw| {
            let lower = head.to_ascii_lowercase();
            let body: Value = serde_json::from_slice(raw).unwrap();
            let resp_json = if step == 0 {
                assert!(lower.starts_with("post /youtubei/v1/next?"));
                json!({
                    "responseContext": {"visitorData": "harvested-vd"},
                    "contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
                        "watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"endpoint": {
                            "browseEndpoint": {
                                "browseId": "MPLYt_1",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {"pageType": LYRICS_PAGE}
                                }
                            }
                        }}}]}
                    }}}
                })
            } else {
                assert!(lower.starts_with("post /youtubei/v1/browse?"));
                assert!(lower.contains("\r\nx-goog-visitor-id: harvested-vd\r\n"));
                assert_eq!(body["context"]["client"]["visitorData"], "harvested-vd");
                mobile_fixture(json!({
                    "timedLyricsData": [{
                        "lyricLine": "synced line",
                        "cueRange": {"startTimeMilliseconds": "0", "endTimeMilliseconds": "2500"}
                    }]
                }))
            };
            let encoded = serde_json::to_vec(&resp_json).unwrap();
            let mut resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                encoded.len()
            )
            .into_bytes();
            resp.extend_from_slice(&encoded);
            resp
        });
        let provider = YTMusic {
            agent: ureq::Agent::new_with_defaults(),
            endpoint,
            version: String::new(),
            visitor_data: String::new(),
            visitor_reader: None,
            visitor_writer: None,
        };
        let lyrics = provider.fetch(&track("youtube", "dQw4w9WgXcQ")).unwrap();
        server.join().unwrap();
        assert_eq!(lyrics.lines.unwrap()[0].line, "synced line");

        let bad_header = YTMusic {
            agent: ureq::Agent::new_with_defaults(),
            endpoint: "https://music.youtube.com".into(),
            version: "1.0\r\nX-Injected: 1".into(),
            visitor_data: String::new(),
            visitor_reader: None,
            visitor_writer: None,
        };
        assert!(matches!(
            bad_header.fetch(&track("youtube", "dQw4w9WgXcQ")),
            Err(LyricsError::Failed(_))
        ));
    }
}
