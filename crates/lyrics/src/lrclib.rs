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

/// Largest duration difference tolerated in search before rejecting a candidate.
const MAX_DURATION_OFFSET: i128 = 15_000;

/// Fetches lyrics from lrclib.net.
pub struct LRCLIB {
    pub agent: ureq::Agent,
    /// The API root, so a test can point it at its own server.
    pub base: String,
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// The part of a response this uses.
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
        let mut last: Option<LyricsError> = None;
        // The exact lookup needs the same artist, title and length the database
        // was given, which a video title rarely matches. It is tried first
        // because it is the one that cannot pick the wrong recording.
        // Without an artist the exact lookup cannot match, and the service's
        // refusal would turn a plain miss into an uncached failure.
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
                Err(error) => last = Some(error),
            }
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
            if let Some(picked) = best_for(&found, &title, &artist, track.metadata.length) {
                if let Ok(lyrics) = build(track, picked) {
                    return Ok(lyrics);
                }
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
        let url = format!("{}/get", self.validated_base()?);
        if length > 0 {
            // The database files a track to the second, so the length is
            // rounded to the nearest one rather than truncated.
            let duration = rounded_seconds(length).to_string();
            let params =
                [("artist_name", artist), ("track_name", title), ("duration", duration.as_str())];
            decode(&self.do_request(&url, &params)?)
        } else {
            let params = [("artist_name", artist), ("track_name", title)];
            decode(&self.do_request(&url, &params)?)
        }
    }

    fn search(&self, query: &str) -> Result<Vec<Entry>, LyricsError> {
        let url = format!("{}/search", self.validated_base()?);
        decode(&self.do_request(&url, &[("q", query)])?)
    }

    fn validated_base(&self) -> Result<String, LyricsError> {
        let raw = if self.base.trim().is_empty() { LRCLIB_BASE } else { &self.base };
        crate::validate_base_url(raw, "lrclib")
    }

    /// Performs one lookup, retrying once.
    ///
    /// The service is free and shared, so a single dropped request should not
    /// become a track the client shows no lyrics for. A 404 is the service
    /// answering that it has nothing, which is not a failure and is not retried.
    fn do_request(&self, url: &str, params: &[(&str, &str)]) -> Result<Vec<u8>, LyricsError> {
        match self.once(url, params) {
            Ok((body, _)) => Ok(body),
            Err((error, false)) => Err(error),
            Err((error, true)) => {
                std::thread::sleep(RETRY_DELAY);
                self.once(url, params).map(|(body, _)| body).map_err(|_| error)
            }
        }
    }

    fn once(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<(Vec<u8>, ()), (LyricsError, bool)> {
        let mut request = self.agent.get(url);
        for &(key, value) in params {
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
                let retryable = is_transient_ureq_error(&error);
                (
                    LyricsError::Failed(format!("lyrics: lrclib could not be reached: {error}")),
                    retryable,
                )
            })?;
        match response.status().as_u16() {
            404 => return Err((LyricsError::NotFound, false)),
            200 => {}
            status => {
                let retryable = status == 429 || status >= 500;
                return Err((
                    LyricsError::Failed(format!("lyrics: lrclib answered {status}")),
                    retryable,
                ));
            }
        }
        response
            .into_body()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map(|body| (body, ()))
            .map_err(|error| {
                let retryable = is_transient_ureq_error(&error);
                (
                    LyricsError::Failed(format!(
                        "lyrics: lrclib sent an unreadable answer: {error}"
                    )),
                    retryable,
                )
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
    let mut queries = vec![format!("{artist} {title}").trim().to_string()];
    // With no artist the combined query is the title, so it is not sent twice.
    for query in [title.to_string(), bracketed(&track.metadata.title)] {
        if !query.is_empty() && !queries.contains(&query) {
            queries.push(query);
        }
    }
    queries
}

/// The bracketed parts of a title that are not a credit, joined.
///
/// A bracket holding only a credit is skipped, because searching for it would
/// find nothing.
fn bracketed(title: &str) -> String {
    const PAIRS: [(&str, &str); 4] = [("【", "】"), ("（", "）"), ("(", ")"), ("[", "]")];
    let mut out = String::new();
    for (open, close) in PAIRS {
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
    candidate.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).any(|token| {
        if token.is_ascii() {
            want.iter().any(|w| token.eq_ignore_ascii_case(w))
        } else if want.iter().any(|w| w == token) {
            true
        } else {
            let lower = token.to_lowercase();
            want.iter().any(|w| w == &lower)
        }
    })
}

/// Picks the entry that is most likely to be the same recording.
///
/// A search for a song returns covers and live takes as well, and the length is
/// what tells them apart when the names are the same.
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
    let title_words = words(want_title);
    let artist_words = words(want_artist);
    let mut best: Option<&Entry> = None;
    let mut best_score = -1;
    for entry in found {
        if !title_words.is_empty()
            && !entry.track_name.trim().is_empty()
            && !shares_word(&title_words, &entry.track_name)
        {
            continue;
        }
        let synced = entry.synced_lyrics.trim();
        let has_synced = !synced.is_empty() && lrc::has_lines(synced);
        let has_plain = !entry.plain_lyrics.trim().is_empty();
        if !has_synced && !has_plain {
            continue;
        }
        let mut duration_bonus = 0;
        let mut tight_duration = false;
        if length > 0 && entry.duration != 0.0 {
            match duration_offset(entry.duration, length) {
                Some(offset) if offset <= 2000 => {
                    duration_bonus = 4;
                    tight_duration = true;
                }
                Some(offset) if offset <= i128::from(SLACK) => {
                    duration_bonus = 1;
                    tight_duration = true;
                }
                Some(offset) if offset <= MAX_DURATION_OFFSET => {}
                Some(_) | None => continue,
            }
        }
        let mut score = duration_bonus;
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

/// Returns the absolute duration difference in milliseconds when both values
/// can be represented without lossy float-to-integer saturation.
///
/// LRCLIB supplies a floating-point duration while the protocol carries an
/// integer number of milliseconds. Converting an overflowing float directly to
/// `i64` can saturate it to `i64::MAX`, making a malformed enormous duration
/// look like an exact match for an equally extreme track length. Keep the
/// conversion bounded and do the subtraction in a wider integer type so the
/// difference itself cannot overflow.
fn duration_offset(duration: f64, length: i64) -> Option<i128> {
    if length <= 0 || !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    let milliseconds = duration * 1000.0;
    if !milliseconds.is_finite() || milliseconds <= 0.0 || milliseconds >= i64::MAX as f64 {
        return None;
    }
    Some((milliseconds as i128 - i128::from(length)).abs())
}

/// The words that describe a video rather than a song, so a bracketed part of a
/// title that contains one of them is not part of the name the database was
/// given.
const NOISE: [&str; 17] = [
    "official",
    "video",
    "videos",
    "audio",
    "lyric",
    "lyrics",
    "mv",
    "m/v",
    "hd",
    "4k",
    "8k",
    "visualizer",
    "visualiser",
    "remaster",
    "remastered",
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
    let raw_artist = track.metadata.author.trim();
    let raw_artist = raw_artist
        .get(..raw_artist.len().saturating_sub(8))
        .filter(|prefix| {
            !prefix.trim().is_empty() && raw_artist[prefix.len()..].eq_ignore_ascii_case(" - topic")
        })
        .map_or(raw_artist, str::trim);
    let mut artist = if raw_artist.eq_ignore_ascii_case("unknown artist") {
        String::new()
    } else {
        raw_artist.to_string()
    };
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

/// Cuts a title at the featured artist, which is not part of the name the
/// database was given.
fn strip_featured(title: &str) -> String {
    // Credit markers are ASCII. Match on raw bytes with ASCII case folding so
    // Unicode characters (such as K or İ) never shift byte offsets.
    let bytes = title.as_bytes();
    let mut cut = title.len();
    for marker in FEATURED {
        let m = marker.as_bytes();
        if let Some(at) = bytes
            .windows(m.len())
            .enumerate()
            .skip(1)
            .find_map(|(idx, window)| window.eq_ignore_ascii_case(m).then_some(idx))
        {
            cut = cut.min(at);
        }
    }
    collapse_whitespace(&title[..cut])
}

/// Takes the description off the end of a title, such as the "Official Music
/// Video" that follows the name without brackets.
///
/// Words come off only while every one of them is description, so a title that
/// is nothing else is left as it is.
fn strip_trailer(title: &str) -> String {
    let mut rest = title.trim();
    loop {
        let Some(last) = rest.split_whitespace().next_back() else {
            return title.to_string();
        };
        if !TRAILER.iter().any(|trailer| last.eq_ignore_ascii_case(trailer)) {
            break;
        }
        let next = rest[..rest.len() - last.len()].trim_end();
        if next.is_empty() {
            return title.to_string();
        }
        rest = next;
    }
    collapse_whitespace(rest)
}

/// Removes the emoji and other decoration a video title carries, which a lyrics
/// database has never heard of.
///
/// Letters in any script are kept, so a title written in Japanese survives
/// intact.
fn strip_symbols(text: &str) -> String {
    const KEPT: &str = "-'&!?.,:;/()[]";
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for raw_char in text.chars() {
        let character = match raw_char {
            '\u{2013}' | '\u{2014}' => '-',
            other => other,
        };
        if character.is_alphanumeric() || KEPT.contains(character) {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(character);
        } else {
            // Whitespace and dropped decoration both collapse to a single
            // separator space between adjacent words.
            pending_space = true;
        }
    }
    out
}

/// Removes the bracketed parts of a title that describe the video.
///
/// A bracket that is part of the song, such as a remix or a reprise, is left
/// alone, because those words change what the lyrics are.
fn strip_title_noise(title: &str) -> String {
    const PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('【', '】'), ('（', '）')];
    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    while !rest.is_empty() {
        let mut chars = rest.chars();
        let ch = chars.next().unwrap();
        let Some((_, close)) = PAIRS.iter().copied().find(|(open, _)| *open == ch) else {
            out.push(ch);
            rest = chars.as_str();
            continue;
        };
        let after_open = chars.as_str();
        let Some(end) = after_open.find(close) else {
            out.push_str(rest);
            break;
        };
        let inner = &after_open[..end];
        let total = ch.len_utf8() + end + close.len_utf8();
        if !contains_noise(inner) {
            out.push_str(&rest[..total]);
        } else {
            out.push(' ');
        }
        rest = &rest[total..];
    }
    collapse_whitespace(&out)
}

fn contains_noise(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.windows(3).any(|w| w.eq_ignore_ascii_case(b"m/v"))
        || bytes.windows(11).any(|w| w.eq_ignore_ascii_case(b"color coded"))
    {
        return true;
    }
    for token in text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
        if NOISE
            .iter()
            .filter(|&&n| !matches!(n, "m/v" | "color coded"))
            .any(|noise| token.eq_ignore_ascii_case(noise))
        {
            return true;
        }
    }
    false
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
        let (_, birthday) = names(&track("Song (Birthday Version)", "Someone"));
        assert_eq!(birthday, "Song (Birthday Version)");
        let (_, fullwidth) = names(&track("夜に駆ける【Official MV】", "YOASOBI"));
        assert_eq!(fullwidth, "夜に駆ける");
    }

    #[test]
    fn a_featured_credit_is_cut() {
        let (_, title) = names(&track("A Song feat. Another", "Someone"));
        assert_eq!(title, "A Song");
    }

    #[test]
    fn a_featured_credit_keeps_unicode_byte_offsets() {
        for title in ["K", "İİ", "Ⱥ", "夜空"] {
            let (_, cleaned) = names(&track(&format!("{title} FeAt. Another"), "Someone"));
            assert_eq!(cleaned, title);
        }
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
        let (_, title) = names(&track("A\u{1f3b5}Song", "Someone"));
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
    fn a_track_without_an_artist_skips_the_exact_lookup_and_duplicate_queries() {
        use std::io::{Read, Write};
        assert_eq!(queries(&track("A Song", ""), "", "A Song"), ["A Song"]);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/api", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]")
                .unwrap();
            request
        });
        let provider = LRCLIB { agent: ureq::Agent::new_with_defaults(), base };
        let result = provider.fetch(&track("A Song", ""));
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /api/search?q=A"), "{request}");
        assert!(matches!(result, Err(LyricsError::NotFound)));
    }

    #[test]
    fn a_bracketed_credit_is_not_offered_as_a_query() {
        let found = queries(&track("A Song 【Official】", "Someone"), "Someone", "A Song");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn the_length_decides_between_recordings_of_the_same_name() {
        let found = vec![
            Entry { duration: 60.0, synced_lyrics: "[00:01.00]x".into(), ..Default::default() },
            Entry { duration: 200.0, synced_lyrics: "[00:01.00]x".into(), ..Default::default() },
        ];
        let picked = best(&found, 200_000).unwrap();
        assert_eq!(picked.duration, 200.0);
    }

    #[test]
    fn an_unrepresentable_duration_cannot_be_scored_as_an_exact_match() {
        let found = vec![
            Entry { synced_lyrics: "[00:01.00]timed".into(), ..Default::default() },
            Entry { duration: f64::MAX, plain_lyrics: "plain".into(), ..Default::default() },
        ];
        let picked = best(&found, i64::MAX).unwrap();
        assert_eq!(picked.synced_lyrics, "[00:01.00]timed");
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
        let picked = best(&found, 200_000).unwrap();
        assert_eq!(picked.plain_lyrics, "nearby");
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

    #[test]
    fn explicit_null_fields_from_lrclib_deserialize_cleanly() {
        let raw = r#"{
            "trackName": "Song",
            "artistName": null,
            "duration": null,
            "instrumental": null,
            "plainLyrics": "words",
            "syncedLyrics": null
        }"#;
        let entry: Entry = serde_json::from_str(raw).expect("nullable fields must deserialize");
        assert_eq!(entry.plain_lyrics, "words");
        assert!(entry.synced_lyrics.is_empty());
        assert_eq!(entry.duration, 0.0);
        assert!(!entry.instrumental);
    }

    #[test]
    fn best_for_rejects_wrong_artists_large_duration_mismatches_and_single_char_noise() {
        let wrong_artist = vec![Entry {
            track_name: "Stay".into(),
            artist_name: "Justin Bieber".into(),
            duration: 141.0,
            synced_lyrics: "[00:01.00]wrong".into(),
            ..Default::default()
        }];
        assert!(best_for(&wrong_artist, "Stay", "Zedd", 210_000).is_none());
        assert!(best_for(&wrong_artist, "Stay", "Zedd", 0).is_none());

        // Different artist naming (such as channel/script mismatch) with tight
        // duration proof within SLACK is still accepted.
        let tight_channel = vec![Entry {
            track_name: "Stay".into(),
            artist_name: "Zedd".into(),
            duration: 211.0,
            synced_lyrics: "[00:01.00]right".into(),
            ..Default::default()
        }];
        assert!(best_for(&tight_channel, "Stay", "Music Channel", 210_000).is_some());

        // Same artist with a > 15s duration mismatch is rejected.
        let far_duration = vec![Entry {
            track_name: "Stay".into(),
            artist_name: "Zedd".into(),
            duration: 60.0,
            synced_lyrics: "[00:01.00]snippet".into(),
            ..Default::default()
        }];
        assert!(best_for(&far_duration, "Stay", "Zedd", 210_000).is_none());

        // Single-character articles do not match unrelated titles.
        let article_only = vec![Entry {
            track_name: "A Different Tune".into(),
            artist_name: "Zedd".into(),
            duration: 210.0,
            synced_lyrics: "[00:01.00]other".into(),
            ..Default::default()
        }];
        assert!(best_for(&article_only, "A Song", "Zedd", 210_000).is_none());
    }

    #[test]
    fn unicode_dashes_split_artist_from_title_and_bad_endpoints_fail_fast() {
        let (artist, title) = names(&track("Someone \u{2013} A Song", ""));
        assert_eq!((artist.as_str(), title.as_str()), ("Someone", "A Song"));
        let (em_artist, em_title) = names(&track("Someone \u{2014} A Song", ""));
        assert_eq!((em_artist.as_str(), em_title.as_str()), ("Someone", "A Song"));

        let bad = LRCLIB {
            agent: ureq::Agent::new_with_defaults(),
            base: "https://user:pass@lrclib.net/api".into(),
        };
        assert!(matches!(bad.fetch(&track("A Song", "Someone")), Err(LyricsError::Failed(_))));
        assert!(!is_transient_ureq_error(&ureq::Error::HostNotFound));
        assert!(!is_transient_ureq_error(&ureq::Error::BodyExceedsLimit(1024)));
    }
}
