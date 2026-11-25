//! Native Rust Discord bot gateway and Rustlink voice-state bridge.
//!
//! The bot token authenticates Discord's main gateway and REST API, while only
//! Discord's per-guild voice token is handed to `rustlink-voice`.

mod gateway;
mod rest;
mod voice_state;

pub use gateway::{
    BotEvent, Gateway, GatewayInfo, Interaction, MessageCreate, Ready, VoiceServerUpdate,
    VoiceStateUpdate, GUILDS, GUILD_MESSAGES, GUILD_VOICE_STATES, MESSAGE_CONTENT,
};
pub use rest::{DiscordRest, SentMessage};
pub use voice_state::VoiceStatePairer;

use std::borrow::Cow;
use std::time::Duration;

use reqwest::header::HeaderValue;
use reqwest::redirect::Policy;
use rustlink_client::Error as RustlinkError;
use zeroize::Zeroizing;

pub(crate) const USER_AGENT_VALUE: &str =
    concat!("DiscordBot (https://github.com/rustlink, ", env!("CARGO_PKG_VERSION"), ")");

/// Errors from the native Discord bot boundary.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error("invalid Discord bot configuration: {0}")]
    Invalid(String),
    #[error("Discord HTTP request failed: {0}")]
    Http(Box<reqwest::Error>),
    #[error("Discord HTTP returned {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("Discord gateway WebSocket failed: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("Discord gateway protocol error: {0}")]
    Protocol(String),
    #[error("Discord gateway requested a reconnect")]
    Reconnect,
    #[error("Discord gateway invalidated the session")]
    InvalidSession,
    #[error("Discord payload JSON failed: {0}")]
    Json(Box<serde_json::Error>),
    #[error("Rustlink request failed: {0}")]
    Rustlink(Box<RustlinkError>),
}

impl From<reqwest::Error> for BotError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(Box::new(error.without_url()))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for BotError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

impl From<serde_json::Error> for BotError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(Box::new(error))
    }
}

impl From<RustlinkError> for BotError {
    fn from(error: RustlinkError) -> Self {
        Self::Rustlink(Box::new(error))
    }
}

pub(crate) fn validate_bot_token(token: &str) -> Result<(), BotError> {
    if token.is_empty() || token.len() > 512 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(BotError::Invalid(
            "bot token is empty, too long, or contains invalid characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn bot_auth_header(token: &str) -> Result<HeaderValue, BotError> {
    let auth_text = Zeroizing::new(format!("Bot {token}"));
    let mut auth = HeaderValue::from_str(&auth_text)
        .map_err(|_| BotError::Invalid("bot token contains invalid header characters".into()))?;
    auth.set_sensitive(true);
    Ok(auth)
}

pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::Client, BotError> {
    Ok(reqwest::Client::builder()
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .redirect(Policy::none())
        .build()?)
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1" | "[::1]")
}

pub(crate) fn host_is_or_subdomain(host: &str, domain: &str) -> bool {
    host.eq_ignore_ascii_case(domain)
        || (host.len() > domain.len() + 1
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            && host.as_bytes()[host.len() - domain.len()..].eq_ignore_ascii_case(domain.as_bytes()))
}

pub(crate) fn validate_snowflake<'a>(value: &'a str, label: &str) -> Result<&'a str, BotError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 20
        || value.starts_with('0')
        || !value.bytes().all(|b| b.is_ascii_digit())
        || !value.parse::<u64>().is_ok_and(|id| id > 0)
    {
        return Err(BotError::Invalid(format!("{label} must be a nonzero Discord snowflake")));
    }
    Ok(value)
}

pub(crate) fn validate_text<'a>(
    value: &'a str,
    label: &str,
    max_len: usize,
) -> Result<&'a str, BotError> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(BotError::Invalid(format!("{label} must be non-empty text")));
    }
    if value.len() > max_len {
        return Err(BotError::Invalid(format!("{label} is too long")));
    }
    Ok(value)
}

pub(crate) async fn read_limited_body(
    mut response: reqwest::Response,
    status: reqwest::StatusCode,
    limit: usize,
    message: &str,
) -> Result<Vec<u8>, BotError> {
    let too_large = || BotError::HttpStatus { status: status.as_u16(), message: message.into() };
    let len = response.content_length().unwrap_or(0);
    if len > limit as u64 {
        return Err(too_large());
    }
    let mut body = Vec::with_capacity((len as usize).min(64 << 10));
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

const PREFIXES: [&str; 10] = [
    "ytsearch:",
    "ytmsearch:",
    "scsearch:",
    "spsearch:",
    "dzsearch:",
    "dzisrc:",
    "spotify:",
    "deezer:",
    "soundcloud:",
    "youtube:playlist:",
];

const KNOWN_MUSIC_HOSTS: [&str; 13] = [
    "youtu.be",
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtube-nocookie.com",
    "www.youtube-nocookie.com",
    "soundcloud.com",
    "www.soundcloud.com",
    "m.soundcloud.com",
    "open.spotify.com",
    "deezer.com",
    "www.deezer.com",
];

const DIRECT_AUDIO_EXTENSIONS: [&str; 7] =
    [".mp3", ".flac", ".ogg", ".opus", ".wav", ".m4a", ".aac"];

const SC_RESERVED_ROOTS: [&str; 22] = [
    "discover",
    "search",
    "stream",
    "upload",
    "you",
    "charts",
    "pages",
    "messages",
    "notifications",
    "settings",
    "people",
    "stations",
    "tags",
    "popular",
    "terms-of-use",
    "imprint",
    "jobs",
    "creators",
    "pro",
    "go",
    "mobile",
    "feed",
];

const SC_RESERVED_SUBPATHS: [&str; 10] = [
    "tracks",
    "albums",
    "playlists",
    "reposts",
    "likes",
    "followers",
    "following",
    "comments",
    "popular-tracks",
    "spotlight",
];

fn is_known_music_host(host: &str) -> bool {
    KNOWN_MUSIC_HOSTS.iter().any(|c| host.eq_ignore_ascii_case(c))
}

fn http_scheme_len(b: &[u8]) -> Option<usize> {
    if b.get(..8).is_some_and(|s| s.eq_ignore_ascii_case(b"https://")) {
        Some(8)
    } else if b.get(..7).is_some_and(|s| s.eq_ignore_ascii_case(b"http://")) {
        Some(7)
    } else {
        None
    }
}

fn split_url_parts(after_scheme: &str) -> Option<(&str, &str, &str, &str)> {
    let (no_frag, frag) = after_scheme.split_once('#').unwrap_or((after_scheme, ""));
    let (auth_path, query) = no_frag.split_once('?').unwrap_or((no_frag, ""));
    let (auth, path) = auth_path.split_once('/')?;
    if auth.contains('@') {
        return None;
    }
    let host = match auth.split_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        Some(_) => return None,
        None => auth,
    };
    Some((host, path, query, frag))
}

fn has_query_param(query: &str, keys: &[&str]) -> bool {
    query
        .split('&')
        .any(|p| p.split_once('=').is_some_and(|(k, v)| keys.contains(&k) && !v.is_empty()))
}

fn is_youtube_video(host: &str, path: &str, query: &str) -> bool {
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    if host.eq_ignore_ascii_case("youtu.be") {
        matches!((segs.next(), segs.next()), (Some(_), None))
    } else if host_is_or_subdomain(host, "youtube.com")
        || host_is_or_subdomain(host, "youtube-nocookie.com")
    {
        match segs.next() {
            Some("watch") => segs.next().is_none() && has_query_param(query, &["v"]),
            Some("shorts" | "live" | "embed" | "v" | "e") => {
                matches!((segs.next(), segs.next()), (Some(_), None))
            }
            _ => false,
        }
    } else {
        false
    }
}

fn is_playable_music_url(after_scheme: &str) -> bool {
    let Some((host, path, query, _)) = split_url_parts(after_scheme) else { return false };
    if !is_known_music_host(host) {
        return false;
    }
    if is_youtube_video(host, path, query) {
        return true;
    }
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    if host_is_or_subdomain(host, "youtube.com")
        || host_is_or_subdomain(host, "youtube-nocookie.com")
    {
        return match (segs.next(), segs.next()) {
            (Some("watch" | "playlist"), None) => has_query_param(query, &["list"]),
            (Some("attribution_link"), None) => has_query_param(query, &["u"]),
            _ => false,
        };
    }
    if host.eq_ignore_ascii_case("open.spotify.com") {
        let mut seg = segs.next().unwrap_or_default();
        for _ in 0..2 {
            if seg.starts_with("intl-") || seg == "embed" {
                seg = segs.next().unwrap_or_default();
            }
        }
        return matches!(seg, "track" | "album" | "playlist")
            && matches!((segs.next(), segs.next()), (Some(_), None));
    }
    if host_is_or_subdomain(host, "deezer.com") {
        return match (segs.next(), segs.next(), segs.next(), segs.next()) {
            (Some("track" | "album" | "playlist"), Some(id), None, None) => !id.is_empty(),
            (Some(loc), Some("track" | "album" | "playlist"), Some(id), None) => {
                !id.is_empty()
                    && loc.len() <= 8
                    && loc.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
            }
            _ => false,
        };
    }
    if host_is_or_subdomain(host, "soundcloud.com") {
        let (Some(user), Some(second)) = (segs.next(), segs.next()) else { return false };
        if SC_RESERVED_ROOTS.iter().any(|r| user.eq_ignore_ascii_case(r)) {
            return false;
        }
        if second.eq_ignore_ascii_case("sets") {
            if segs.next().is_none() {
                return false;
            }
        } else if SC_RESERVED_SUBPATHS.iter().any(|s| second.eq_ignore_ascii_case(s)) {
            return false;
        }
        return segs.next().is_none_or(|s| s.starts_with("s-") && segs.next().is_none());
    }
    false
}

fn has_direct_audio_extension(after_scheme: &str) -> bool {
    let end = after_scheme.find(['?', '#']).unwrap_or(after_scheme.len());
    let Some((_, path)) = after_scheme[..end].split_once('/') else { return false };
    DIRECT_AUDIO_EXTENSIONS.iter().any(|ext| {
        path.len() >= ext.len()
            && path.as_bytes()[path.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes())
    })
}

fn unwrap_spoilers(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix("||").and_then(|r| r.strip_suffix("||")).map_or(text, str::trim)
}

fn unwrap_angle(text: &str) -> &str {
    text.strip_prefix('<').and_then(|r| r.strip_suffix('>')).map_or(text, str::trim)
}

fn trim_url_tail(mut url: &str) -> &str {
    let extra_close = |s: &str, open: u8, close: u8| {
        s.as_bytes().last() == Some(&close)
            && s.bytes().filter(|&b| b == close).count() > s.bytes().filter(|&b| b == open).count()
    };
    loop {
        if let Some(s) = url.strip_suffix([',', ';', '.', '!', '?']) {
            url = s;
        } else if extra_close(url, b'(', b')') || extra_close(url, b'[', b']') {
            url = &url[..url.len() - 1];
        } else {
            return url;
        }
    }
}

fn extract_http_url(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let (start, scheme_len) =
        (0..bytes.len()).find_map(|i| http_scheme_len(&bytes[i..]).map(|len| (i, len)))?;
    let rest = &text[start..];
    let mut end = rest[scheme_len..]
        .find(|ch: char| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, '<' | '>' | '"' | '\'' | '|' | '`' | '\\')
        })
        .map_or(rest.len(), |off| scheme_len + off);
    let after = &rest[scheme_len..end];
    let host = &after[..after.find(['/', '?', '#', ':']).unwrap_or(after.len())];
    if host.is_empty() {
        return None;
    }
    if is_known_music_host(host) {
        if let Some(off) = after.find(|ch: char| !ch.is_ascii()) {
            end = scheme_len + off;
        }
    }
    let candidate = trim_url_tail(&rest[..end]);
    (candidate.len() > scheme_len).then_some(candidate)
}

fn extract_schemeless_music_url(text: &str) -> Option<&str> {
    for raw in text.split_whitespace() {
        let token =
            raw.trim_start_matches(['<', '(', '[', '|']).trim_end_matches(['>', ')', ']', '|']);
        if token.contains("://") {
            continue;
        }
        let Some((host, _)) = token.split_once('/') else { continue };
        if host.contains(':') || !is_known_music_host(host) {
            continue;
        }
        let ascii_end = token.find(|ch: char| !ch.is_ascii()).unwrap_or(token.len());
        let clean = trim_url_tail(&token[..ascii_end]);
        if clean.split_once('/').is_some_and(|(_, path)| !path.is_empty()) {
            return Some(clean);
        }
    }
    None
}

/// Normalizes user-entered text into a Rustlink query: URLs (including mobile
/// share text, `<...>` / `||...||` wrappers, or scheme-less music links), 11-char
/// YouTube IDs, and explicit source prefixes pass through; plain words become `ytsearch:`.
pub fn normalize_query(text: &str) -> Cow<'_, str> {
    let text = unwrap_spoilers(text);
    if text.is_empty() {
        return Cow::Borrowed("");
    }
    if let Some(inner) = text.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        if !inner.chars().any(char::is_whitespace) && extract_http_url(inner) == Some(inner) {
            return Cow::Borrowed(text);
        }
    }
    if let Some(&prefix) = PREFIXES
        .iter()
        .find(|&&p| text.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)))
    {
        return if text.starts_with(prefix) {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(format!("{prefix}{}", &text[prefix.len()..]))
        };
    }
    if let Some(url) = extract_http_url(text) {
        return Cow::Borrowed(url);
    }
    if let Some(schemeless) = extract_schemeless_music_url(text) {
        return Cow::Owned(format!("https://{schemeless}"));
    }
    let is_video_id = text.len() == 11
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if text.contains("://") || is_video_id {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("ytsearch:{text}"))
    }
}

/// Inspects a Discord `MESSAGE_CREATE` payload for a playable query or link.
///
/// Returns `Some((normalized_query, mentioned))`: `mentioned` is `true` for
/// explicit `@bot` mentions, or `false` for standalone playable music/audio URLs.
pub fn extract_message_query<'a>(
    content: &'a str,
    bot_user_id: &str,
) -> Option<(Cow<'a, str>, bool)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bot_id = bot_user_id.trim();
    if !bot_id.is_empty() && trimmed.contains("<@") {
        let plain = format!("<@{bot_id}>");
        let nick = format!("<@!{bot_id}>");
        if trimmed.contains(&plain) || trimmed.contains(&nick) {
            if let Some(rest) = trimmed.strip_prefix(&plain).or_else(|| trimmed.strip_prefix(&nick))
            {
                if !rest.contains(&plain) && !rest.contains(&nick) {
                    let query = normalize_query(rest);
                    return (!query.is_empty()).then_some((query, true));
                }
            }
            let stripped = trimmed.replace(&plain, " ").replace(&nick, " ");
            let query = normalize_query(&stripped).into_owned();
            return (!query.is_empty()).then_some((Cow::Owned(query), true));
        }
    }

    let bare = unwrap_angle(unwrap_spoilers(trimmed));
    if bare.is_empty() || bare.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some(url) = extract_http_url(bare) {
        let after = &url[http_scheme_len(url.as_bytes())?..];
        return (url == bare
            && (is_playable_music_url(after) || has_direct_audio_extension(after)))
        .then_some((Cow::Borrowed(url), false));
    }
    let schemeless = extract_schemeless_music_url(bare)?;
    (schemeless == bare && is_playable_music_url(schemeless))
        .then(|| (Cow::Owned(format!("https://{schemeless}")), false))
}

/// Slices `text` at a UTF-8 character boundary to at most `max_chars` characters.
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices().nth(max_chars).map_or(text, |(end, _)| &text[..end])
}

fn parse_timestamp_token_ms(raw: &str) -> Option<i64> {
    const MAX_SECS: u64 = 7 * 86_400;
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 32 {
        return None;
    }
    if raw.bytes().all(|b| b.is_ascii_digit()) {
        let secs = raw.parse::<u64>().ok()?;
        return (1..=MAX_SECS).contains(&secs).then_some((secs as i64) * 1000);
    }
    let (mut total, mut start, mut stage) = (0u64, 0usize, 0u8);
    for (idx, b) in raw.bytes().enumerate() {
        if b.is_ascii_digit() {
            continue;
        }
        if idx == start {
            return None;
        }
        let val = raw[start..idx].parse::<u64>().ok()?;
        let (next_stage, mult) = match b.to_ascii_lowercase() {
            b'h' if stage < 1 => (1, 3600),
            b'm' if stage < 2 => (2, 60),
            b's' if stage < 3 => (3, 1),
            _ => return None,
        };
        stage = next_stage;
        total = total.checked_add(val.checked_mul(mult)?)?;
        start = idx + 1;
    }
    if start < raw.len() {
        let mult = match stage {
            1 => 60,
            2 => 1,
            _ => return None,
        };
        total = total.checked_add(raw[start..].parse::<u64>().ok()?.checked_mul(mult)?)?;
    }
    (1..=MAX_SECS).contains(&total).then_some((total as i64) * 1000)
}

/// Extracts a positive start timestamp in milliseconds (`?t=90`, `?t=1m30s`,
/// `&start=43`, `&time_continue=75`, or `#t=45s`) from a YouTube single-video URL.
pub fn extract_url_timestamp_ms(text: &str) -> Option<i64> {
    let bare = unwrap_angle(unwrap_spoilers(text));
    let after = match extract_http_url(bare) {
        Some(url) => &url[http_scheme_len(url.as_bytes())?..],
        None => extract_schemeless_music_url(bare)?,
    };
    let (host, path, query, frag) = split_url_parts(after)?;
    if !is_youtube_video(host, path, query) {
        return None;
    }
    let find_ts = |s: &str, keys: &[&str]| {
        s.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            keys.contains(&k).then(|| parse_timestamp_token_ms(v)).flatten()
        })
    };
    find_ts(query, &["t", "start", "time_continue"]).or_else(|| find_ts(frag, &["t", "start"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn reqwest_error_conversion_strips_url_and_interaction_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dropper = tokio::spawn(async move { drop(listener.accept().await.unwrap()) });
        let secret = "super-secret-interaction-token-987654";
        let url = format!("http://{addr}/api/v10/webhooks/123/{secret}/messages/@original");
        let raw_error = reqwest::Client::new().patch(&url).send().await.unwrap_err();
        assert!(raw_error.to_string().contains(secret));
        let bot_error = BotError::from(raw_error);
        let (display, debug) = (bot_error.to_string(), format!("{bot_error:?}"));
        assert!(
            !display.contains(secret) && !debug.contains(secret) && !display.contains("webhooks")
        );
        dropper.await.unwrap();
    }

    #[test]
    fn normalize_query_handles_share_strings_wrappers_and_schemeless_links() {
        for (input, expected) in [
            ("https://youtu.be/dQw4w9WgXcQ?si=share", "https://youtu.be/dQw4w9WgXcQ?si=share"),
            (
                "<https://www.youtube.com/watch?v=dQw4w9WgXcQ>",
                "<https://www.youtube.com/watch?v=dQw4w9WgXcQ>",
            ),
            ("||https://youtu.be/dQw4w9WgXcQ||", "https://youtu.be/dQw4w9WgXcQ"),
            (
                "Check this out https://youtu.be/dQw4w9WgXcQ?si=123",
                "https://youtu.be/dQw4w9WgXcQ?si=123",
            ),
            (
                "[Song](https://www.youtube.com/watch?v=dQw4w9WgXcQ)",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            ),
            ("幫我播https://youtu.be/dQw4w9WgXcQ謝謝", "https://youtu.be/dQw4w9WgXcQ"),
            ("youtu.be/dQw4w9WgXcQ", "https://youtu.be/dQw4w9WgXcQ"),
            (
                "open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
                "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT",
            ),
            ("lofi hip hop", "ytsearch:lofi hip hop"),
        ] {
            assert_eq!(normalize_query(input), expected, "{input}");
        }
    }

    #[test]
    fn normalize_query_lowercases_uppercase_prefixes_without_allocating_lowercase_ones() {
        assert!(matches!(normalize_query("ytsearch:lofi"), Cow::Borrowed("ytsearch:lofi")));
        assert!(
            matches!(normalize_query("YTSEARCH:lofi"), Cow::Owned(ref v) if v == "ytsearch:lofi")
        );
        assert!(
            matches!(normalize_query("YouTube:Playlist:PL123"), Cow::Owned(ref v) if v == "youtube:playlist:PL123")
        );
        assert!(
            matches!(normalize_query("SoundCloud:artist/song"), Cow::Owned(ref v) if v == "soundcloud:artist/song")
        );
        assert_eq!(normalize_query("on.soundcloud.com/short"), "ytsearch:on.soundcloud.com/short");
        assert_eq!(normalize_query("deezer.page.link/short"), "ytsearch:deezer.page.link/short");
    }

    #[test]
    fn extract_message_query_accepts_bare_music_links_and_mentions_without_hijacking_chat() {
        assert_eq!(
            extract_message_query("https://youtu.be/dQw4w9WgXcQ?si=abc", "999"),
            Some((Cow::Borrowed("https://youtu.be/dQw4w9WgXcQ?si=abc"), false))
        );
        assert_eq!(
            extract_message_query("<https://www.youtube.com/watch?v=dQw4w9WgXcQ>", "999"),
            Some((Cow::Borrowed("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), false))
        );
        assert_eq!(
            extract_message_query("||youtu.be/dQw4w9WgXcQ||", "999"),
            Some((Cow::Owned("https://youtu.be/dQw4w9WgXcQ".into()), false))
        );
        assert_eq!(
            extract_message_query("https://cdn.example.com/track.opus?dl=1", "999"),
            Some((Cow::Borrowed("https://cdn.example.com/track.opus?dl=1"), false))
        );
        assert_eq!(
            extract_message_query("<@999> lofi beats", "999"),
            Some((Cow::Owned("ytsearch:lofi beats".into()), true))
        );
        assert_eq!(
            extract_message_query("<@!999> 幫我播 https://youtu.be/dQw4w9WgXcQ", "999"),
            Some((Cow::Borrowed("https://youtu.be/dQw4w9WgXcQ"), true))
        );
        for ignored in [
            "<@999>",
            "今天天氣真好",
            "https://github.com/rust-lang/rust",
            "你看這個 https://youtu.be/dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_message_query(ignored, "999"), None, "{ignored}");
        }
    }

    #[test]
    fn extract_message_query_checks_playable_paths_and_ignores_video_clips_or_profiles() {
        for playable in [
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/v/dQw4w9WgXcQ",
            "https://www.youtube.com/e/dQw4w9WgXcQ",
            "https://www.youtube.com/attribution_link?u=%2Fwatch%3Fv%3DdQw4w9WgXcQ",
            "https://open.spotify.com/intl-zh-TW/track/4cOdK2wGLETKBW3PvgPWqT",
            "https://open.spotify.com/embed/album/1ATL5GLyefJaxhQzSPVrLX",
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "https://www.deezer.com/track/3135556",
            "https://www.deezer.com/fr/album/302127",
            "https://soundcloud.com/artist/track-slug",
            "https://soundcloud.com/artist/sets/ep-slug",
            "https://soundcloud.com/artist/track-slug/s-secret",
        ] {
            assert_eq!(
                extract_message_query(playable, "999"),
                Some((Cow::Borrowed(playable), false)),
                "expected playable: {playable}"
            );
        }

        for non_playable in [
            "https://cdn.discordapp.com/attachments/1/2/clip.webm",
            "https://cdn.discordapp.com/attachments/1/2/movie.mkv",
            "https://on.soundcloud.com/shortlink",
            "https://deezer.page.link/shortlink",
            "https://youtu.be/",
            "https://www.youtube.com/@SomeChannel",
            "https://www.youtube.com/channel/UC123456",
            "https://www.youtube.com/results?search_query=lofi",
            "https://www.youtube.com/shorts",
            "https://www.youtube.com/watch",
            "https://open.spotify.com/artist/4Z8W4fKeB5YxbusRsdQVPb",
            "https://open.spotify.com/user/spotify",
            "https://open.spotify.com/episode/4cOdK2wGLETKBW3PvgPWqT",
            "https://www.deezer.com/artist/27",
            "https://www.deezer.com/us/artist/27",
            "https://soundcloud.com/artist",
            "https://soundcloud.com/artist/tracks",
            "https://soundcloud.com/artist/sets",
            "https://soundcloud.com/artist/likes",
            "https://soundcloud.com/discover",
            "https://soundcloud.com/search/sounds?q=lofi",
            "youtube.com/@SomeChannel",
            "soundcloud.com/artist",
        ] {
            assert_eq!(extract_message_query(non_playable, "999"), None, "{non_playable}");
        }
    }

    #[test]
    fn extract_url_timestamp_ms_parses_youtube_query_and_fragment_cues() {
        for (input, expected) in [
            ("https://youtu.be/dQw4w9WgXcQ?si=abc&t=43", Some(43_000)),
            ("<https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1m30s>", Some(90_000)),
            ("||youtu.be/dQw4w9WgXcQ?t=1h2m3s||", Some(3_723_000)),
            ("幫我播 https://www.youtube.com/embed/dQw4w9WgXcQ?start=75", Some(75_000)),
            ("https://music.youtube.com/watch?v=dQw4w9WgXcQ#t=2m", Some(120_000)),
            ("https://youtu.be/dQw4w9WgXcQ?t=0", None),
            ("https://www.youtube.com/playlist?list=PL123&t=45", None),
            ("https://example.com/audio.mp3?t=45", None),
            ("lofi hip hop t=45", None),
        ] {
            assert_eq!(extract_url_timestamp_ms(input), expected, "{input}");
        }
    }
}
