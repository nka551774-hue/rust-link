//! Native Rust Discord bot gateway and Rustlink voice-state bridge.
//!
//! The bot gateway is deliberately separate from `rustlink-voice`: the bot
//! token authenticates Discord's main gateway and REST API, while the voice
//! token received from Discord is the only credential handed to the voice
//! transport.

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

use rustlink_client::Error as RustlinkError;

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
        Self::Http(Box::new(error))
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

pub(crate) fn validate_snowflake<'a>(value: &'a str, label: &str) -> Result<&'a str, BotError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 20
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || !value.parse::<u64>().is_ok_and(|id| id > 0)
    {
        return Err(BotError::Invalid(format!("{label} must be a nonzero Discord snowflake")));
    }
    Ok(value)
}

pub(crate) fn validate_text<'a>(value: &'a str, label: &str) -> Result<&'a str, BotError> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(BotError::Invalid(format!("{label} must be non-empty text")));
    }
    Ok(value)
}

pub(crate) async fn read_limited_body(
    mut response: reqwest::Response,
    status: reqwest::StatusCode,
    limit: usize,
    message: &str,
) -> Result<Vec<u8>, BotError> {
    if let Some(length) = response.content_length() {
        if length > limit as u64 {
            return Err(BotError::HttpStatus { status: status.as_u16(), message: message.into() });
        }
    }
    let initial = response.content_length().map_or(0, |len| (len as usize).min(64 << 10));
    let mut body = Vec::with_capacity(initial);
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BotError::HttpStatus { status: status.as_u16(), message: message.into() });
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

const KNOWN_MUSIC_HOSTS: [&str; 15] = [
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
    "on.soundcloud.com",
    "open.spotify.com",
    "deezer.com",
    "www.deezer.com",
    "deezer.page.link",
];

const DIRECT_AUDIO_EXTENSIONS: [&str; 9] =
    [".mp3", ".flac", ".ogg", ".opus", ".wav", ".m4a", ".aac", ".webm", ".mkv"];

fn is_known_music_host(host: &str) -> bool {
    KNOWN_MUSIC_HOSTS.iter().any(|candidate| host.eq_ignore_ascii_case(candidate))
}

fn has_direct_audio_extension(url_without_scheme: &str) -> bool {
    let path_end = url_without_scheme.find(['?', '#']).unwrap_or(url_without_scheme.len());
    let Some((_, path)) = url_without_scheme[..path_end].split_once('/') else {
        return false;
    };
    DIRECT_AUDIO_EXTENSIONS.iter().any(|ext| {
        path.len() >= ext.len()
            && path.as_bytes()[path.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes())
    })
}

fn unwrap_spoilers(mut text: &str) -> &str {
    text = text.trim();
    if let Some(inner) = text.strip_prefix("||").and_then(|rest| rest.strip_suffix("||")) {
        inner.trim()
    } else {
        text
    }
}

fn trim_url_tail(mut url: &str) -> &str {
    loop {
        if let Some(stripped) = url.strip_suffix([',', ';', '.', '!', '?']) {
            url = stripped;
            continue;
        }
        if url.ends_with(')')
            && url.bytes().filter(|&b| b == b')').count()
                > url.bytes().filter(|&b| b == b'(').count()
        {
            url = &url[..url.len() - 1];
            continue;
        }
        if url.ends_with(']')
            && url.bytes().filter(|&b| b == b']').count()
                > url.bytes().filter(|&b| b == b'[').count()
        {
            url = &url[..url.len() - 1];
            continue;
        }
        break;
    }
    url
}

fn extract_http_url(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let (start, scheme_len) = (0..bytes.len()).find_map(|idx| {
        if bytes[idx..].get(..8).is_some_and(|s| s.eq_ignore_ascii_case(b"https://")) {
            Some((idx, 8))
        } else if bytes[idx..].get(..7).is_some_and(|s| s.eq_ignore_ascii_case(b"http://")) {
            Some((idx, 7))
        } else {
            None
        }
    })?;
    let rest = &text[start..];
    let mut end = rest.len();
    for (offset, ch) in rest[scheme_len..].char_indices() {
        if ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, '<' | '>' | '"' | '\'' | '|' | '`' | '\\')
        {
            end = scheme_len + offset;
            break;
        }
    }
    let after_scheme = &rest[scheme_len..end];
    let host_end = after_scheme.find(['/', '?', '#', ':']).unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    if host.is_empty() {
        return None;
    }
    if is_known_music_host(host) {
        if let Some(non_ascii) = rest[scheme_len..end].find(|ch: char| !ch.is_ascii()) {
            end = scheme_len + non_ascii;
        }
    }
    let candidate = trim_url_tail(&rest[..end]);
    (candidate.len() > scheme_len).then_some(candidate)
}

fn extract_schemeless_music_url(text: &str) -> Option<&str> {
    for raw_token in text.split_whitespace() {
        let token = raw_token.trim_start_matches(['<', '(', '[', '|']);
        let token = token.trim_end_matches(['>', ')', ']', '|']);
        if token.contains("://") {
            continue;
        }
        let Some((host, _)) = token.split_once('/') else {
            continue;
        };
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

/// Normalizes user-entered text into a Rustlink query: URLs (including links
/// pasted inside mobile share text, Discord `<...>` / `||...||` wrappers, or
/// scheme-less music domains like `youtu.be/...`), 11-character YouTube IDs,
/// and explicit source prefixes are routed directly; plain search terms are
/// prefixed with `ytsearch:`.
pub fn normalize_query(text: &str) -> Cow<'_, str> {
    let text = unwrap_spoilers(text);
    if text.is_empty() {
        return Cow::Borrowed("");
    }
    if let Some(inner) = text.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')) {
        if !inner.chars().any(char::is_whitespace) && extract_http_url(inner) == Some(inner) {
            return Cow::Borrowed(text);
        }
    }
    if let Some(url) = extract_http_url(text) {
        return Cow::Borrowed(url);
    }
    if let Some(schemeless) = extract_schemeless_music_url(text) {
        return Cow::Owned(format!("https://{schemeless}"));
    }
    let is_video_id = text.len() == 11
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if text.contains("://")
        || is_video_id
        || PREFIXES.iter().any(|prefix| {
            text.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
    {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("ytsearch:{text}"))
    }
}

/// Inspects a Discord `MESSAGE_CREATE` payload for a playable query or link.
///
/// Returns `Some((normalized_query, mentioned))`:
/// - `mentioned == true`: the message explicitly `@`-mentioned `bot_user_id`, so
///   the remaining text (URL, share text, or search keywords) is normalized.
/// - `mentioned == false`: the message is a standalone direct music link (or
///   direct audio file URL) with no surrounding chat prose.
pub fn extract_message_query<'a>(
    content: &'a str,
    bot_user_id: &str,
) -> Option<(Cow<'a, str>, bool)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bot_id = bot_user_id.trim();
    if !bot_id.is_empty() {
        let plain_mention = format!("<@{bot_id}>");
        let nick_mention = format!("<@!{bot_id}>");
        if trimmed.contains(&plain_mention) || trimmed.contains(&nick_mention) {
            if let Some(rest) =
                trimmed.strip_prefix(&plain_mention).or_else(|| trimmed.strip_prefix(&nick_mention))
            {
                if !rest.contains(&plain_mention) && !rest.contains(&nick_mention) {
                    let query = normalize_query(rest);
                    return (!query.is_empty()).then_some((query, true));
                }
            }
            let stripped = trimmed.replace(&plain_mention, " ").replace(&nick_mention, " ");
            let query = normalize_query(&stripped).into_owned();
            return (!query.is_empty()).then_some((Cow::Owned(query), true));
        }
    }

    let unwrapped = unwrap_spoilers(trimmed);
    let bare = unwrapped
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .map_or(unwrapped, str::trim);
    if bare.is_empty() || bare.chars().any(char::is_whitespace) {
        return None;
    }
    if let Some(url) = extract_http_url(bare) {
        if url == bare {
            let scheme_len =
                if url.as_bytes().get(..8).is_some_and(|s| s.eq_ignore_ascii_case(b"https://")) {
                    8
                } else {
                    7
                };
            let after_scheme = &url[scheme_len..];
            let host_end = after_scheme.find(['/', '?', '#', ':']).unwrap_or(after_scheme.len());
            let host = &after_scheme[..host_end];
            let has_path =
                after_scheme[host_end..].strip_prefix('/').is_some_and(|path| !path.is_empty());
            if (is_known_music_host(host) && has_path) || has_direct_audio_extension(after_scheme) {
                return Some((Cow::Borrowed(url), false));
            }
        }
        return None;
    }
    if let Some(schemeless) = extract_schemeless_music_url(bare) {
        if schemeless == bare {
            return Some((Cow::Owned(format!("https://{schemeless}")), false));
        }
    }
    None
}

/// Slices `text` at a UTF-8 character boundary to at most `max_chars` characters
/// without allocating.
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices().nth(max_chars).map_or(text, |(end, _)| &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_query_handles_share_strings_wrappers_and_schemeless_links() {
        assert_eq!(
            normalize_query("https://youtu.be/dQw4w9WgXcQ?si=share"),
            "https://youtu.be/dQw4w9WgXcQ?si=share"
        );
        assert_eq!(
            normalize_query("<https://www.youtube.com/watch?v=dQw4w9WgXcQ>"),
            "<https://www.youtube.com/watch?v=dQw4w9WgXcQ>"
        );
        assert_eq!(
            normalize_query("||https://youtu.be/dQw4w9WgXcQ||"),
            "https://youtu.be/dQw4w9WgXcQ"
        );
        assert_eq!(
            normalize_query("Check this out https://youtu.be/dQw4w9WgXcQ?si=123"),
            "https://youtu.be/dQw4w9WgXcQ?si=123"
        );
        assert_eq!(
            normalize_query("[Song](https://www.youtube.com/watch?v=dQw4w9WgXcQ)"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(
            normalize_query("幫我播https://youtu.be/dQw4w9WgXcQ謝謝"),
            "https://youtu.be/dQw4w9WgXcQ"
        );
        assert_eq!(normalize_query("youtu.be/dQw4w9WgXcQ"), "https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(
            normalize_query("open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"),
            "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT"
        );
        assert_eq!(normalize_query("lofi hip hop"), "ytsearch:lofi hip hop");
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
        assert_eq!(extract_message_query("<@999>", "999"), None);
        assert_eq!(extract_message_query("今天天氣真好", "999"), None);
        assert_eq!(extract_message_query("https://github.com/rust-lang/rust", "999"), None);
        assert_eq!(extract_message_query("你看這個 https://youtu.be/dQw4w9WgXcQ", "999"), None);
    }
}
