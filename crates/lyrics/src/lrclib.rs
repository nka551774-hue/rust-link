//! LRCLIB timed and plain lyrics provider.

use std::time::Duration;

use rustlink_protocol::Track;
use serde::Deserialize;

use crate::{lrc, Lyrics, LyricsError, Provider};

pub const LRCLIB_BASE: &str = "https://lrclib.net/api";
const AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(10);
const BODY_LIMIT: u64 = 1 << 20;
const RETRY_DELAY: Duration = Duration::from_millis(250);
const SLACK: i64 = 4000;
const MAX_DURATION_OFFSET: i128 = 15_000;

pub struct LRCLIB {
    pub agent: ureq::Agent,
    pub base: String,
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Entry {
    #[serde(deserialize_with = "null_default")]
    track_name: String,
    #[serde(deserialize_with = "null_default")]
    artist_name: String,
    #[serde(deserialize_with = "null_default")]
    duration: f64,
    #[serde(deserialize_with = "null_default")]
    instrumental: bool,
    #[serde(deserialize_with = "null_default")]
    synced_lyrics: String,
    #[serde(deserialize_with = "null_default")]
    plain_lyrics: String,
}

impl Provider for LRCLIB {
    fn name(&self) -> &'static str {
        "lrclib"
    }

    fn fetch(&self, track: &Track) -> Result<Lyrics, LyricsError> {
        let (artist, title) = names(track);
        if title.is_empty() {
            return Err(LyricsError::NotFound);
        }
        let mut last = None;
        if !artist.is_empty() {
            match self.get(&artist, &title, track.metadata.length) {
                Ok(entry) => {
                    if let Ok(lyrics) = build(track, &entry) {
                        return Ok(lyrics);
                    }
                    if entry.instrumental {
                        return Err(LyricsError::NotFound);
                    }
                }
                Err(LyricsError::NotFound) => {}
                Err(e) => last = Some(e),
            }
        }
        for query in queries(track, &artist, &title) {
            match self.search(&query) {
                Ok(found) => {
                    if let Some(lyrics) = best_for(&found, &title, &artist, track.metadata.length)
                        .and_then(|picked| build(track, picked).ok())
                    {
                        return Ok(lyrics);
                    }
                }
                Err(LyricsError::NotFound) => {}
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(LyricsError::NotFound))
    }
}

impl LRCLIB {
    fn get(&self, artist: &str, title: &str, length: i64) -> Result<Entry, LyricsError> {
        let url = format!("{}/get", self.validated_base()?);
        let dur = (length > 0).then(|| rounded_seconds(length).to_string());
        let mut params = vec![("artist_name", artist), ("track_name", title)];
        if let Some(d) = dur.as_deref() {
            params.push(("duration", d));
        }
        decode(&self.do_request(&url, &params)?)
    }

    fn search(&self, query: &str) -> Result<Vec<Entry>, LyricsError> {
        let url = format!("{}/search", self.validated_base()?);
        decode(&self.do_request(&url, &[("q", query)])?)
    }

    fn validated_base(&self) -> Result<String, LyricsError> {
        let raw = if self.base.trim().is_empty() { LRCLIB_BASE } else { &self.base };
        crate::validate_base_url(raw, "lrclib")
    }

    fn do_request(&self, url: &str, params: &[(&str, &str)]) -> Result<Vec<u8>, LyricsError> {
        match self.once(url, params) {
            Ok(body) => Ok(body),
            Err((e, false)) => Err(e),
            Err((e, true)) => {
                std::thread::sleep(RETRY_DELAY);
                self.once(url, params).map_err(|_| e)
            }
        }
    }

    fn once(&self, url: &str, params: &[(&str, &str)]) -> Result<Vec<u8>, (LyricsError, bool)> {
        let mut req = self.agent.get(url);
        for &(k, v) in params {
            req = req.query(k, v);
        }
        let resp = req
            .header("User-Agent", AGENT)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|e| {
                let retry = is_transient_ureq_error(&e);
                (LyricsError::Failed(format!("lyrics: lrclib could not be reached: {e}")), retry)
            })?;
        match resp.status().as_u16() {
            404 => return Err((LyricsError::NotFound, false)),
            200 => {}
            status => {
                return Err((
                    LyricsError::Failed(format!("lyrics: lrclib answered {status}")),
                    status == 429 || status >= 500,
                ));
            }
        }
        resp.into_body().with_config().limit(BODY_LIMIT).read_to_vec().map_err(|e| {
            let retry = is_transient_ureq_error(&e);
            (LyricsError::Failed(format!("lyrics: lrclib sent an unreadable answer: {e}")), retry)
        })
    }
}

fn is_transient_ureq_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::ConnectProxyFailed(_)
            | ureq::Error::Protocol(_)
    )
}

fn rounded_seconds(ms: i64) -> i64 {
    ms / 1000 + i64::from(ms % 1000 >= 500)
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, LyricsError> {
    serde_json::from_slice(body)
        .map_err(|e| LyricsError::Failed(format!("lyrics: lrclib sent an unreadable answer: {e}")))
}

fn build(track: &Track, entry: &Entry) -> Result<Lyrics, LyricsError> {
    let mut out = Lyrics::new(track.metadata.source_name.clone(), "lrclib");
    let synced = entry.synced_lyrics.trim();
    if !synced.is_empty() {
        let lines = lrc::parse(synced);
        if !lines.is_empty() {
            out.lines = Some(lines);
            return Ok(out);
        }
    }
    let plain = entry.plain_lyrics.trim();
    if !plain.is_empty() {
        out.text = Some(plain.to_owned());
        return Ok(out);
    }
    Err(LyricsError::NotFound)
}

fn queries(track: &Track, artist: &str, title: &str) -> Vec<String> {
    let mut out = vec![format!("{artist} {title}").trim().to_owned()];
    for q in [title.to_owned(), bracketed(&track.metadata.title)] {
        if !q.is_empty() && !out.contains(&q) {
            out.push(q);
        }
    }
    out
}

fn bracketed(title: &str) -> String {
    let mut out = String::new();
    for (open, close) in [("【", "】"), ("（", "）"), ("(", ")"), ("[", "]")] {
        let mut rest = title;
        while let Some(at) = rest.find(open) {
            let after = &rest[at + open.len()..];
            let Some(end) = after.find(close) else { break };
            let inner = after[..end].trim();
            if !inner.is_empty() && !contains_noise(inner) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(inner);
            }
            rest = &after[end + close.len()..];
        }
    }
    out
}

fn words(text: &str) -> Vec<String> {
    let mut all: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    if all.iter().any(|w| w.chars().nth(1).is_some()) {
        all.retain(|w| w.chars().nth(1).is_some());
    }
    all
}

fn shares_word(want: &[String], candidate: &str) -> bool {
    candidate.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).any(|tok| {
        if tok.is_ascii() {
            want.iter().any(|w| tok.eq_ignore_ascii_case(w))
        } else {
            want.iter().any(|w| w == tok) || {
                let lower = tok.to_lowercase();
                want.iter().any(|w| w == &lower)
            }
        }
    })
}

#[cfg(test)]
fn best(found: &[Entry], length: i64) -> Option<&Entry> {
    best_for(found, "", "", length)
}

fn best_for<'a>(
    found: &'a [Entry],
    want_title: &str,
    want_artist: &str,
    length: i64,
) -> Option<&'a Entry> {
    let (title_words, artist_words) = (words(want_title), words(want_artist));
    let (mut best, mut best_score) = (None, -1);
    for entry in found {
        if !title_words.is_empty()
            && !entry.track_name.trim().is_empty()
            && !shares_word(&title_words, &entry.track_name)
        {
            continue;
        }
        let synced = entry.synced_lyrics.trim();
        let has_synced = !synced.is_empty() && lrc::has_lines(synced);
        if !has_synced && entry.plain_lyrics.trim().is_empty() {
            continue;
        }
        let (mut score, mut tight_duration) = (0, false);
        if length > 0 && entry.duration != 0.0 {
            match duration_offset(entry.duration, length) {
                Some(off) if off <= 2000 => {
                    score = 4;
                    tight_duration = true;
                }
                Some(off) if off <= i128::from(SLACK) => {
                    score = 1;
                    tight_duration = true;
                }
                Some(off) if off <= MAX_DURATION_OFFSET => {}
                _ => continue,
            }
        }
        if !artist_words.is_empty() && !entry.artist_name.trim().is_empty() {
            if shares_word(&artist_words, &entry.artist_name) {
                score += 2;
            } else if !tight_duration {
                continue;
            }
        }
        if entry.instrumental {
            score -= 1;
        }
        if has_synced {
            score += 2;
        }
        if score > best_score {
            best = Some(entry);
            best_score = score;
        }
    }
    best
}

fn duration_offset(duration: f64, length: i64) -> Option<i128> {
    let ms = duration * 1000.0;
    (length > 0
        && duration.is_finite()
        && duration > 0.0
        && ms.is_finite()
        && ms > 0.0
        && ms < i64::MAX as f64)
        .then(|| (ms as i128 - i128::from(length)).abs())
}

const NOISE_WORDS: [&str; 15] = [
    "official",
    "video",
    "videos",
    "audio",
    "lyric",
    "lyrics",
    "mv",
    "hd",
    "4k",
    "8k",
    "visualizer",
    "visualiser",
    "remaster",
    "remastered",
    "explicit",
];

const FEATURED: [&str; 5] = [" ft.", " ft ", " feat.", " feat ", " featuring "];

const TRAILER: [&str; 21] = [
    "official",
    "music",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "mv",
    "m/v",
    "pv",
    "hd",
    "4k",
    "8k",
    "full",
    "ver",
    "version",
    "visualizer",
    "visualiser",
    "remaster",
    "remastered",
    "explicit",
    "color coded",
];

pub(crate) fn names(track: &Track) -> (String, String) {
    let raw = track.metadata.author.trim();
    let raw = raw
        .get(..raw.len().saturating_sub(8))
        .filter(|p| !p.trim().is_empty() && raw[p.len()..].eq_ignore_ascii_case(" - topic"))
        .map_or(raw, str::trim);
    let mut artist =
        if raw.eq_ignore_ascii_case("unknown artist") { String::new() } else { raw.to_owned() };
    let mut title = strip_trailer(&strip_featured(&strip_symbols(&strip_title_noise(
        track.metadata.title.trim(),
    ))));
    if let Some((a, t)) =
        artist.is_empty().then(|| title.split_once(" - ")).flatten().filter(|(a, _)| !a.is_empty())
    {
        artist = a.trim().to_owned();
        title = t.trim().to_owned();
    }
    (artist, title)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn strip_featured(title: &str) -> String {
    let bytes = title.as_bytes();
    let cut = FEATURED
        .iter()
        .filter_map(|m| {
            let mb = m.as_bytes();
            bytes
                .windows(mb.len())
                .enumerate()
                .skip(1)
                .find_map(|(i, w)| w.eq_ignore_ascii_case(mb).then_some(i))
        })
        .min()
        .unwrap_or(title.len());
    collapse_whitespace(&title[..cut])
}

fn strip_trailer(title: &str) -> String {
    let mut rest = title.trim();
    while let Some(last) = rest.split_whitespace().next_back() {
        if !TRAILER.iter().any(|t| last.eq_ignore_ascii_case(t)) {
            return collapse_whitespace(rest);
        }
        let next = rest[..rest.len() - last.len()].trim_end();
        if next.is_empty() {
            return title.to_owned();
        }
        rest = next;
    }
    title.to_owned()
}

fn strip_symbols(text: &str) -> String {
    const KEPT: &str = "-'&!?.,:;/()[]";
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for raw_ch in text.chars() {
        let ch = if matches!(raw_ch, '\u{2013}' | '\u{2014}') { '-' } else { raw_ch };
        if ch.is_alphanumeric() || KEPT.contains(ch) {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        } else {
            space = true;
        }
    }
    out
}

fn strip_title_noise(title: &str) -> String {
    const PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('【', '】'), ('（', '）')];
    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    while !rest.is_empty() {
        let mut chars = rest.chars();
        let ch = chars.next().unwrap();
        let Some((_, close)) = PAIRS.iter().copied().find(|&(open, _)| open == ch) else {
            out.push(ch);
            rest = chars.as_str();
            continue;
        };
        let after = chars.as_str();
        let Some(end) = after.find(close) else {
            out.push_str(rest);
            break;
        };
        let total = ch.len_utf8() + end + close.len_utf8();
        if contains_noise(&after[..end]) {
            out.push(' ');
        } else {
            out.push_str(&rest[..total]);
        }
        rest = &rest[total..];
    }
    collapse_whitespace(&out)
}

fn contains_noise(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(3).any(|w| w.eq_ignore_ascii_case(b"m/v"))
        || bytes.windows(11).any(|w| w.eq_ignore_ascii_case(b"color coded"))
        || text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .any(|tok| NOISE_WORDS.iter().any(|n| tok.eq_ignore_ascii_case(n)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;

    fn track(title: &str, author: &str) -> Track {
        Track::new(
            String::new(),
            TrackInfo {
                identifier: "abc".into(),
                source_name: "youtube".into(),
                title: title.into(),
                author: author.into(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn duration_rounding_accepts_the_full_protocol_integer_range() {
        assert_eq!(rounded_seconds(499), 0);
        assert_eq!(rounded_seconds(500), 1);
        assert_eq!(rounded_seconds(1499), 1);
        assert_eq!(rounded_seconds(1500), 2);
        assert_eq!(rounded_seconds(i64::MAX), 9_223_372_036_854_776);
    }

    #[test]
    fn a_video_title_loses_the_parts_that_describe_the_video() {
        assert_eq!(
            names(&track("A Song (Official Music Video)", "Someone")),
            ("Someone".into(), "A Song".into())
        );
    }

    #[test]
    fn a_bracket_that_is_part_of_the_song_is_kept() {
        assert_eq!(names(&track("A Song (Remix)", "Someone")).1, "A Song (Remix)");
        assert_eq!(
            names(&track("Song (Birthday Version)", "Someone")).1,
            "Song (Birthday Version)"
        );
        assert_eq!(names(&track("夜に駆ける【Official MV】", "YOASOBI")).1, "夜に駆ける");
    }

    #[test]
    fn a_featured_credit_is_cut() {
        assert_eq!(names(&track("A Song feat. Another", "Someone")).1, "A Song");
    }

    #[test]
    fn a_featured_credit_keeps_unicode_byte_offsets() {
        for title in ["\u{212a}", "\u{0130}\u{0130}", "\u{023a}", "夜空"] {
            assert_eq!(names(&track(&format!("{title} FeAt. Another"), "Someone")).1, title);
        }
    }

    #[test]
    fn a_trailing_credit_without_brackets_comes_off() {
        assert_eq!(names(&track("A Song Official Video", "Someone")).1, "A Song");
    }

    #[test]
    fn a_title_that_is_only_a_credit_is_left_alone() {
        assert_eq!(names(&track("Official Video", "Someone")).1, "Official Video");
    }

    #[test]
    fn an_artist_missing_from_the_field_is_read_from_the_title() {
        assert_eq!(names(&track("Someone - A Song", "")), ("Someone".into(), "A Song".into()));
    }

    #[test]
    fn decoration_becomes_a_space_rather_than_nothing() {
        assert_eq!(names(&track("A\u{1f3b5}Song", "Someone")).1, "A Song");
    }

    #[test]
    fn a_title_in_another_script_survives() {
        assert_eq!(names(&track("夜に駆ける", "Someone")).1, "夜に駆ける");
    }

    #[test]
    fn a_bracketed_name_is_offered_as_its_own_query() {
        let found = queries(&track("A Song 【Extra Name】", "Someone"), "Someone", "A Song");
        assert!(found.contains(&"Extra Name".to_string()));
    }

    #[test]
    fn a_track_without_an_artist_skips_the_exact_lookup_and_duplicate_queries() {
        assert_eq!(queries(&track("A Song", ""), "", "A Song"), ["A Song"]);
        let (host, server) = crate::serve_http(1, |_, head, _| {
            assert!(head.starts_with("GET /api/search?q=A"), "{head}");
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]".to_vec()
        });
        let provider =
            LRCLIB { agent: ureq::Agent::new_with_defaults(), base: format!("{host}/api") };
        let res = provider.fetch(&track("A Song", ""));
        server.join().unwrap();
        assert!(matches!(res, Err(LyricsError::NotFound)));
    }

    #[test]
    fn a_bracketed_credit_is_not_offered_as_a_query() {
        assert_eq!(queries(&track("A Song 【Official】", "Someone"), "Someone", "A Song").len(), 2);
    }

    #[test]
    fn the_length_decides_between_recordings_of_the_same_name() {
        let found = vec![
            Entry { duration: 60.0, synced_lyrics: "[00:01.00]x".into(), ..Default::default() },
            Entry { duration: 200.0, synced_lyrics: "[00:01.00]x".into(), ..Default::default() },
        ];
        assert_eq!(best(&found, 200_000).unwrap().duration, 200.0);
    }

    #[test]
    fn an_unrepresentable_duration_cannot_be_scored_as_an_exact_match() {
        let found = vec![
            Entry { synced_lyrics: "[00:01.00]timed".into(), ..Default::default() },
            Entry { duration: f64::MAX, plain_lyrics: "plain".into(), ..Default::default() },
        ];
        assert_eq!(best(&found, i64::MAX).unwrap().synced_lyrics, "[00:01.00]timed");
    }

    #[test]
    fn an_invalid_duration_is_worse_than_a_nearby_valid_duration() {
        let found = vec![
            Entry {
                duration: f64::NAN,
                synced_lyrics: "[00:01.00]invalid".into(),
                ..Default::default()
            },
            Entry { duration: 202.5, plain_lyrics: "nearby".into(), ..Default::default() },
        ];
        assert_eq!(best(&found, 200_000).unwrap().plain_lyrics, "nearby");
    }

    #[test]
    fn an_instrumental_is_not_preferred_over_a_sung_take() {
        let found = vec![
            Entry {
                instrumental: true,
                synced_lyrics: "[00:01.00]x".into(),
                duration: 200.0,
                ..Default::default()
            },
            Entry {
                instrumental: false,
                synced_lyrics: "[00:01.00]x".into(),
                duration: 200.0,
                ..Default::default()
            },
        ];
        assert!(!best(&found, 200_000).unwrap().instrumental);
    }

    #[test]
    fn an_entry_with_neither_words_nor_timings_is_not_a_result() {
        assert!(best(&[Entry { duration: 200.0, ..Default::default() }], 200_000).is_none());
    }

    #[test]
    fn a_timed_entry_becomes_timed_lines() {
        let entry =
            Entry { synced_lyrics: "[00:01.00]a\n[00:02.00]b".into(), ..Default::default() };
        let lines = build(&track("a", "b"), &entry).unwrap().lines.unwrap();
        assert_eq!((lines.len(), lines[0].timestamp), (2, 1_000));
    }

    #[test]
    fn a_plain_entry_becomes_text_when_there_are_no_timings() {
        let entry = Entry { plain_lyrics: "words".into(), ..Default::default() };
        let lyrics = build(&track("a", "b"), &entry).unwrap();
        assert_eq!(lyrics.text.as_deref(), Some("words"));
        assert!(lyrics.lines.is_none());
    }

    #[test]
    fn an_entry_with_nothing_is_not_lyrics() {
        assert!(build(&track("a", "b"), &Entry::default()).is_err());
    }

    #[test]
    fn explicit_null_fields_from_lrclib_deserialize_cleanly() {
        let raw = r#"{"trackName":"Song","artistName":null,"duration":null,"instrumental":null,"plainLyrics":"words","syncedLyrics":null}"#;
        let entry: Entry = serde_json::from_str(raw).unwrap();
        assert_eq!(entry.plain_lyrics, "words");
        assert!(entry.synced_lyrics.is_empty() && entry.duration == 0.0 && !entry.instrumental);
    }

    #[test]
    fn best_for_rejects_wrong_artists_large_duration_mismatches_and_single_char_noise() {
        let item = |track_name: &str, artist_name: &str, duration: f64| {
            [Entry {
                track_name: track_name.into(),
                artist_name: artist_name.into(),
                duration,
                synced_lyrics: "[00:01.00]line".into(),
                ..Default::default()
            }]
        };
        let wrong = item("Stay", "Justin Bieber", 141.0);
        assert!(best_for(&wrong, "Stay", "Zedd", 210_000).is_none());
        assert!(best_for(&wrong, "Stay", "Zedd", 0).is_none());
        assert!(best_for(&item("Stay", "Zedd", 211.0), "Stay", "Music Channel", 210_000).is_some());
        assert!(best_for(&item("Stay", "Zedd", 60.0), "Stay", "Zedd", 210_000).is_none());
        assert!(
            best_for(&item("A Different Tune", "Zedd", 210.0), "A Song", "Zedd", 210_000).is_none()
        );
    }

    #[test]
    fn unicode_dashes_split_artist_from_title_and_bad_endpoints_fail_fast() {
        assert_eq!(
            names(&track("Someone \u{2013} A Song", "")),
            ("Someone".into(), "A Song".into())
        );
        assert_eq!(
            names(&track("Someone \u{2014} A Song", "")),
            ("Someone".into(), "A Song".into())
        );
        let bad = LRCLIB {
            agent: ureq::Agent::new_with_defaults(),
            base: "https://user:pass@lrclib.net/api".into(),
        };
        assert!(matches!(bad.fetch(&track("A Song", "Someone")), Err(LyricsError::Failed(_))));
        assert!(!is_transient_ureq_error(&ureq::Error::HostNotFound));
        assert!(!is_transient_ureq_error(&ureq::Error::BodyExceedsLimit(1024)));
    }
}
