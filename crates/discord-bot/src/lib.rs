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
    BotEvent, Gateway, GatewayInfo, MessageCreate, Ready, VoiceServerUpdate, VoiceStateUpdate,
    GUILDS, GUILD_MESSAGES, GUILD_VOICE_STATES, MESSAGE_CONTENT,
};
pub use rest::{DiscordRest, SentMessage};
pub use voice_state::VoiceStatePairer;

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

pub(crate) fn validate_snowflake(value: &str, label: &str) -> Result<String, BotError> {
    let value = value.trim();
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.parse::<u64>().map_or(true, |id| id == 0)
    {
        return Err(BotError::Invalid(format!("{label} must be a nonzero Discord snowflake")));
    }
    Ok(value.to_owned())
}

pub(crate) fn validate_text(value: &str, label: &str) -> Result<String, BotError> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(BotError::Invalid(format!("{label} must be non-empty text")));
    }
    Ok(value.to_owned())
}
