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

/// Normalizes user-entered text into a Rustlink query: URLs, 11-character
/// YouTube IDs, and explicit source prefixes borrow `text` directly; plain
/// search terms are prefixed with `ytsearch:`.
pub fn normalize_query(text: &str) -> Cow<'_, str> {
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
    let text = text.trim();
    let is_video_id = text.len() == 11
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if text.is_empty()
        || text.contains("://")
        || text.starts_with("<http")
        || is_video_id
        || PREFIXES.iter().any(|prefix| text.starts_with(prefix))
    {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("ytsearch:{text}"))
    }
}

/// Slices `text` at a UTF-8 character boundary to at most `max_chars` characters
/// without allocating.
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices().nth(max_chars).map_or(text, |(end, _)| &text[..end])
}
