//! The LRCLIB provider, which is a free public database of timed lyrics.

use std::time::Duration;

use rustlink_protocol::Track;
use serde::Deserialize;

use crate::{lrc, Lyrics, LyricsError, Provider};

/// The public lyrics API. It needs no account and answers with timed lyrics,
/// which is what makes a line by line display possible rather than a wall of
/// text.
pub const LRCLIB_BASE: &str = "https://lrclib.net/api";

/// Identifies this node. The service asks callers to say who they are so it can
/// tell them apart.
const AGENT: &str = concat!("Rustlink/", env!("CARGO_PKG_VERSION"));

/// Bounds one lookup. A client waiting for lyrics would rather have no answer
/// than a slow one.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds a response, so a hostile or broken answer cannot fill memory.
const BODY_LIMIT: u64 = 1 << 20;

/// How long to wait before the one retry.
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// How far a search result's length may be from the track's before it is treated
/// as a different recording.
const SLACK: i64 = 4000;

/// Fetches lyrics from lrclib.net.
pub struct LRCLIB {
    pub agent: ureq::Agent,
    /// The API root, so a test can point it at its own server.
    pub base: String,
}

/// The part of a response this uses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Entry {
    track_name: String,
    artist_name: String,
    duration: f64,
    instrumental: bool,
    synced_lyrics: String,
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
        let mut last: Option<LyricsError> = None;
        // The exact lookup needs the same artist, title and length the database
        // was given, which a video title rarely matches. It is tried first
        // because it is the one that cannot pick the wrong recording.
        match self.get(&artist, &title, track.metadata.length) {
            Ok(entry) => return build(track, &entry),
            Err(LyricsError::NotFound) => {}
            Err(error) => last = Some(error),
        }
        // The name a video credits is often not the name the database filed the
        // track under, and an artist that matches nothing drags a combined query
        // down with it. So the title alone is tried too, and the length decides
        // between the recordings that share it.
        //
        // A query that fails is not the end of the search: the service answers
        // 503 for some queries and not others, and giving up on the first one
        // threw away the answer the next query would have found.
        for query in queries(track, &artist, &title) {
            let found = match self.search(&query) {
                Ok(found) => found,
                Err(LyricsError::NotFound) => continue,
                Err(error) => {
                    last = Some(error);
                    continue;
                }
            };
            if let Some(best) = best(&found, track.metadata.length) {
                return build(track, best);
            }
        }
        match last {
            Some(error) => Err(error),
            None => Err(LyricsError::NotFound),
        }
    }
}

impl LRCLIB {
    fn get(&self, artist: &str, title: &str, length: i64) -> Result<Entry, LyricsError> {
        let url = format!("{}/get", self.base_or_default());
        let mut params = vec![
            ("artist_name".to_string(), artist.to_string()),
            ("track_name".to_string(), title.to_string()),
        ];
        if length > 0 {
            // The database files a track to the second, so the length is
            // rounded to the nearest one rather than truncated.
            params.push(("duration".to_string(), rounded_seconds(length).to_string()));
        }
        decode(&self.do_request(&url, &params)?)
    }

    fn search(&self, query: &str) -> Result<Vec<Entry>, LyricsError> {
        let url = format!("{}/search", self.base_or_default());
        decode(&self.do_request(&url, &[("q".to_string(), query.to_string())])?)
    }

    fn base_or_default(&self) -> &str {
        if self.base.is_empty() {
            LRCLIB_BASE
        } else {
            &self.base
        }
    }

    /// Performs one lookup, retrying once.
    ///
    /// The service is free and shared, so a single dropped request should not
    /// become a track the client shows no lyrics for. A 404 is the service
    /// answering that it has nothing, which is not a failure and is not retried.
    fn do_request(&self, url: &str, params: &[(String, String)]) -> Result<Vec<u8>, LyricsError> {
        match self.once(url, params) {
            Err(LyricsError::NotFound) => Err(LyricsError::NotFound),
            Err(error) => {
                std::thread::sleep(RETRY_DELAY);
                self.once(url, params).map_err(|_| error)
            }
            Ok(body) => Ok(body),
        }
    }

    fn once(&self, url: &str, params: &[(String, String)]) -> Result<Vec<u8>, LyricsError> {
        let mut request = self.agent.get(url);
        for (key, value) in params {
            request = request.query(key, value);
        }
        let response = request
            .header("User-Agent", AGENT)
            .header("Accept", "application/json")
            .config()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|error| {
                LyricsError::Failed(format!("lyrics: lrclib could not be reached: {error}"))
            })?;
        match response.status().as_u16() {
            404 => return Err(LyricsError::NotFound),
            200 => {}
            status => {
                return Err(LyricsError::Failed(format!("lyrics: lrclib answered {status}")));
            }
        }
        response.into_body().with_config().limit(BODY_LIMIT).read_to_vec().map_err(|error| {
            LyricsError::Failed(format!("lyrics: lrclib sent an unreadable answer: {error}"))
        })
    }
}

fn rounded_seconds(milliseconds: i64) -> i64 {
    milliseconds / 1000 + i64::from(milliseconds % 1000 >= 500)
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, LyricsError> {
    serde_json::from_slice(body).map_err(|error| {
        LyricsError::Failed(format!("lyrics: lrclib sent an unreadable answer: {error}"))
    })
}

/// Turns a database entry into the payload a client reads.
///
/// Timed lyrics are preferred, because a client can follow them; plain ones are
/// still worth returning, because a wall of text is better than nothing.
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
        out.text = Some(plain.to_string());
        return Ok(out);
    }
    Err(LyricsError::NotFound)
}

/// What to search for, in the order worth trying.
///
/// The first two are the obvious ones; the last is for the uploads that put the
/// song's own name in brackets after the artist, where the name on its own is
/// what the database knows it by.
fn queries(track: &Track, artist: &str, title: &str) -> Vec<String> {
    let mut queries = vec![format!("{artist} {title}").trim().to_string(), title.to_string()];
    let extra = bracketed(&track.metadata.title);
    if !extra.is_empty() && extra != title {
        queries.push(extra);
    }
    queries
}

/// The bracketed parts of a title that are not a credit, joined.
///
/// A bracket holding only a credit is skipped, because searching for it would
/// find nothing.
fn bracketed(title: &str) -> String {
    const PAIRS: [(&str, &str); 4] = [("【", "】"), ("（", "）"), ("(", ")"), ("[", "]")];
    let mut parts: Vec<String> = Vec::new();
    for (open, close) in PAIRS {
        let mut rest = title;
        while let Some(at) = rest.find(open) {
            let after = &rest[at + open.len()..];
            let Some(end) = after.find(close) else { break };
            let inner = after[..end].trim();
            if !inner.is_empty() && !contains_noise(inner) {
                parts.push(inner.to_string());
            }
            rest = &after[end + close.len()..];
        }
    }
    parts.join(" ")
}

/// Picks the entry that is most likely to be the same recording.
///
/// A search for a song returns covers and live takes as well, and the length is
/// what tells them apart when the names are the same.
fn best(found: &[Entry], length: i64) -> Option<&Entry> {
    let mut best: Option<&Entry> = None;
    let mut best_score = -1;
    for entry in found {
        if entry.synced_lyrics.trim().is_empty() && entry.plain_lyrics.trim().is_empty() {
            continue;
        }
        let mut score = 0;
        if entry.instrumental {
            score -= 1;
        }
        if !entry.synced_lyrics.trim().is_empty() {
            score += 2;
        }
        if length > 0 && entry.duration > 0.0 {
            let offset = ((entry.duration * 1000.0) as i64 - length).abs();
            if offset <= 2000 {
                score += 4;
            } else if offset <= SLACK {
                score += 1;
            } else {
                score -= 4;
            }
        }
        if score > best_score {
            best = Some(entry);
            best_score = score;
        }
    }
    best
}

/// The words that describe a video rather than a song, so a bracketed part of a
/// title that contains one of them is not part of the name the database was
/// given.
const NOISE: [&str; 13] = [
    "official",
    "video",
    "audio",
    "lyric",
    "mv",
    "m/v",
    "hd",
    "4k",
    "visualizer",
    "visualiser",
    "remaster",
    "explicit",
    "color coded",
];

/// The words after which a title is naming who else is on the track rather than
/// what it is called. "with" is deliberately not one of them, because it is just
/// as likely to be part of the name.
const FEATURED: [&str; 5] = [" ft.", " ft ", " feat.", " feat ", " featuring "];

/// The words that describe the video rather than the song when they sit at the
/// end of a title, which is where a credit would normally be bracketed and is
/// not always.
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

/// Splits a track into the artist and title a lyrics database would have been
/// given.
///
/// A video title carries a lot that is not part of the song, so the parts that
/// only describe the video, the decoration and the list of who else is on the
/// track are all dropped first.
pub(crate) fn names(track: &Track) -> (String, String) {
    let mut artist = track.metadata.author.trim().to_string();
    let mut title = strip_symbols(&strip_title_noise(track.metadata.title.trim()));
    title = strip_featured(&title);
    title = strip_trailer(&title);
    // A source that reports no artist often puts one in the title, and the
    // database keeps the two apart.
    if artist.is_empty() {
        if let Some(at) = title.find(" - ").filter(|at| *at > 0) {
            artist = title[..at].trim().to_string();
            title = title[at + 3..].trim().to_string();
        }
    }
    (artist, title)
}

/// Cuts a title at the featured artist, which is not part of the name the
/// database was given.
fn strip_featured(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut cut = title.len();
    for marker in FEATURED {
        if let Some(at) = lower.find(marker).filter(|at| *at > 0) {
            cut = cut.min(at);
        }
    }
    title[..cut].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Takes the description off the end of a title, such as the "Official Music
/// Video" that follows the name without brackets.
///
/// Words come off only while every one of them is description, so a title that
/// is nothing else is left as it is.
fn strip_trailer(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().collect();
    let mut keep = words.len();
    while keep > 0 && TRAILER.contains(&words[keep - 1].to_lowercase().as_str()) {
        keep -= 1;
    }
    if keep == 0 {
        return title.to_string();
    }
    words[..keep].join(" ")
}

/// Removes the emoji and other decoration a video title carries, which a lyrics
/// database has never heard of.
///
/// Letters in any script are kept, so a title written in Japanese survives
/// intact.
fn strip_symbols(text: &str) -> String {
    const KEPT: &str = "-'&!?.,:;/()[]";
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_alphanumeric() || character.is_whitespace() || KEPT.contains(character) {
            out.push(character);
        } else {
            // A dropped character becomes a space rather than nothing, so
            // decoration sitting between two words does not run them together.
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Removes the bracketed parts of a title that describe the video.
///
/// A bracket that is part of the song, such as a remix or a reprise, is left
/// alone, because those words change what the lyrics are.
fn strip_title_noise(title: &str) -> String {
    let bytes = title.as_bytes();
    let mut out = String::with_capacity(title.len());
    let mut index = 0;
    while index < bytes.len() {
        let open = bytes[index];
        if open != b'(' && open != b'[' {
            // A multi-byte character is copied whole rather than byte by byte.
            let rest = &title[index..];
            let character = rest.chars().next().unwrap_or_default();
            out.push(character);
            index += character.len_utf8();
            continue;
        }
        let close = if open == b'(' { b')' } else { b']' };
        let Some(end) = bytes[index..].iter().position(|byte| *byte == close) else {
            out.push_str(&title[index..]);
            break;
        };
        if !contains_noise(&title[index + 1..index + end]) {
            out.push_str(&title[index..index + end + 1]);
        }
        index += end + 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_noise(text: &str) -> bool {
    let lower = text.to_lowercase();
    NOISE.iter().any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;

    #[test]
    fn duration_rounding_accepts_the_full_protocol_integer_range() {
        assert_eq!(rounded_seconds(499), 0);
        assert_eq!(rounded_seconds(500), 1);
        assert_eq!(rounded_seconds(1499), 1);
        assert_eq!(rounded_seconds(1500), 2);
        assert_eq!(rounded_seconds(i64::MAX), 9_223_372_036_854_776);
    }

    fn track(title: &str, author: &str) -> Track {
        Track::new(
            String::new(),
            TrackInfo {
                identifier: "abc".into(),
                source_name: "youtube".into(),
                title: title.into(),
                author: author.into(),
                length: 0,
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_video_title_loses_the_parts_that_describe_the_video() {
        let (artist, title) = names(&track("A Song (Official Music Video)", "Someone"));
        assert_eq!(artist, "Someone");
        assert_eq!(title, "A Song");
    }

    #[test]
    fn a_bracket_that_is_part_of_the_song_is_kept() {
        let (_, title) = names(&track("A Song (Remix)", "Someone"));
        assert_eq!(title, "A Song (Remix)");
    }

    #[test]
    fn a_featured_credit_is_cut() {
        let (_, title) = names(&track("A Song feat. Another", "Someone"));
        assert_eq!(title, "A Song");
    }

    #[test]
    fn a_trailing_credit_without_brackets_comes_off() {
        let (_, title) = names(&track("A Song Official Video", "Someone"));
        assert_eq!(title, "A Song");
    }

    #[test]
    fn a_title_that_is_only_a_credit_is_left_alone() {
        let (_, title) = names(&track("Official Video", "Someone"));
        assert_eq!(title, "Official Video");
    }

    #[test]
    fn an_artist_missing_from_the_field_is_read_from_the_title() {
        let (artist, title) = names(&track("Someone - A Song", ""));
        assert_eq!(artist, "Someone");
        assert_eq!(title, "A Song");
    }

    #[test]
    fn decoration_becomes_a_space_rather_than_nothing() {
        let (_, title) = names(&track("A🎵Song", "Someone"));
        assert_eq!(title, "A Song");
    }

    #[test]
    fn a_title_in_another_script_survives() {
        let (_, title) = names(&track("夜に駆ける", "Someone"));
        assert_eq!(title, "夜に駆ける");
    }

    #[test]
    fn a_bracketed_name_is_offered_as_its_own_query() {
        let found = queries(&track("A Song 【Extra Name】", "Someone"), "Someone", "A Song");
        assert!(found.contains(&"Extra Name".to_string()));
    }

    #[test]
    fn a_bracketed_credit_is_not_offered_as_a_query() {
        let found = queries(&track("A Song 【Official】", "Someone"), "Someone", "A Song");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn the_length_decides_between_recordings_of_the_same_name() {
        let found = vec![
            Entry { duration: 60.0, synced_lyrics: "x".into(), ..Default::default() },
            Entry { duration: 200.0, synced_lyrics: "x".into(), ..Default::default() },
        ];
        let picked = best(&found, 200_000).unwrap();
        assert_eq!(picked.duration, 200.0);
    }

    #[test]
    fn an_instrumental_is_not_preferred_over_a_sung_take() {
        let found = vec![
            Entry {
                instrumental: true,
                synced_lyrics: "x".into(),
                duration: 200.0,
                ..Default::default()
            },
            Entry {
                instrumental: false,
                synced_lyrics: "x".into(),
                duration: 200.0,
                ..Default::default()
            },
        ];
        assert!(!best(&found, 200_000).unwrap().instrumental);
    }

    #[test]
    fn an_entry_with_neither_words_nor_timings_is_not_a_result() {
        let found = vec![Entry { duration: 200.0, ..Default::default() }];
        assert!(best(&found, 200_000).is_none());
    }

    #[test]
    fn a_timed_entry_becomes_timed_lines() {
        let entry =
            Entry { synced_lyrics: "[00:01.00]a\n[00:02.00]b".into(), ..Default::default() };
        let lyrics = build(&track("a", "b"), &entry).unwrap();
        let lines = lyrics.lines.unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].timestamp, 1_000);
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
}
