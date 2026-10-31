//! Discord main Gateway v10 for a Rust bot.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::{sleep_until, Instant};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;
use zeroize::Zeroizing;

use crate::{validate_snowflake, BotError};

/// Discord gateway intents used by native bot applications.
pub const GUILDS: u64 = 1 << 0;
pub const GUILD_VOICE_STATES: u64 = 1 << 7;
pub const GUILD_MESSAGES: u64 = 1 << 9;
pub const MESSAGE_CONTENT: u64 = 1 << 15;

const API_BASE: &str = "https://discord.com/api/v10";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_GATEWAY_MESSAGE: usize = 2 * 1024 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayInfo {
    pub url: String,
    pub shards: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub user_id: String,
    pub session_id: String,
    pub resume_gateway_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceStateUpdate {
    pub guild_id: Option<String>,
    pub user_id: String,
    pub channel_id: Option<String>,
    pub session_id: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct VoiceServerUpdate {
    pub guild_id: String,
    pub token: String,
    pub endpoint: Option<String>,
}

impl std::fmt::Debug for VoiceServerUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VoiceServerUpdate")
            .field("guild_id", &self.guild_id)
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCreate {
    pub id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub author_id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BotEvent {
    Ready(Ready),
    VoiceStateUpdate(VoiceStateUpdate),
    VoiceServerUpdate(VoiceServerUpdate),
    MessageCreate(MessageCreate),
    Other { name: String, data: Value },
}

#[derive(Debug, Deserialize)]
struct GatewayResponse {
    url: String,
    #[serde(default)]
    #[serde(rename = "shards")]
    _shards: u32,
}

/// A single Discord main-gateway connection.
pub struct Gateway {
    socket: Socket,
    token: Zeroizing<String>,
    intents: u64,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    heartbeat_acknowledged: bool,
    sequence: Option<u64>,
}

impl Gateway {
    /// Fetch Discord's gateway URL, receive HELLO, and send IDENTIFY.
    pub async fn connect(token: impl Into<String>, intents: u64) -> Result<Self, BotError> {
        let token = validate_gateway_token(&token.into())?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = http
            .get(format!("{API_BASE}/gateway/bot"))
            .header(reqwest::header::AUTHORIZATION, format!("Bot {token}"))
            .send()
            .await?;
        let status = response.status();
        let body = read_response_body(response, status).await?;
        if !status.is_success() {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                // Discord error bodies are not needed to decide whether to
                // retry, and must not become a token-bearing log message.
                message: format!("gateway endpoint returned {} bytes", body.len()),
            });
        }
        let info: GatewayResponse = serde_json::from_slice(&body)?;
        Self::connect_url(info.url, Zeroizing::new(token), intents).await
    }

    async fn connect_url(
        base: String,
        token: Zeroizing<String>,
        intents: u64,
    ) -> Result<Self, BotError> {
        let url = gateway_url(&base)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_GATEWAY_MESSAGE))
            .max_frame_size(Some(MAX_GATEWAY_MESSAGE));
        let (mut socket, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(&url, Some(config), false),
        )
        .await
        .map_err(|_| BotError::Protocol("gateway HELLO timed out".into()))??;
        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next())
            .await
            .map_err(|_| BotError::Protocol("gateway HELLO timed out".into()))?
            .ok_or_else(|| BotError::Protocol("gateway closed before HELLO".into()))??;
        let hello = text_json(hello)?;
        if hello.get("op").and_then(Value::as_u64) != Some(10) {
            return Err(BotError::Protocol("first gateway payload was not HELLO".into()));
        }
        let interval_ms = hello["d"]["heartbeat_interval"]
            .as_u64()
            .filter(|interval| *interval >= 500)
            .ok_or_else(|| BotError::Protocol("HELLO has invalid heartbeat interval".into()))?;
        let mut gateway = Self {
            socket,
            token,
            intents,
            heartbeat_interval: Duration::from_millis(interval_ms),
            next_heartbeat: Instant::now() + Duration::from_millis(interval_ms),
            heartbeat_acknowledged: true,
            sequence: None,
        };
        gateway.identify().await?;
        Ok(gateway)
    }

    async fn identify(&mut self) -> Result<(), BotError> {
        self.send(json!({
            "op": 2,
            "d": {
                "token": self.token.as_str(),
                "intents": self.intents,
                "properties": {"$os": "linux", "$browser": "rustlink", "$device": "rustlink"}
            }
        }))
        .await
    }

    async fn heartbeat(&mut self) -> Result<(), BotError> {
        if !self.heartbeat_acknowledged {
            return Err(BotError::Protocol("gateway heartbeat was not acknowledged".into()));
        }
        self.send_heartbeat().await
    }

    async fn send_heartbeat(&mut self) -> Result<(), BotError> {
        self.send(json!({"op": 1, "d": self.sequence})).await?;
        self.heartbeat_acknowledged = false;
        self.next_heartbeat = Instant::now() + self.heartbeat_interval;
        Ok(())
    }

    async fn send(&mut self, payload: Value) -> Result<(), BotError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.socket.send(Message::Text(payload.to_string().into())),
        )
        .await
        .map_err(|_| BotError::Protocol("gateway write timed out".into()))??;
        Ok(())
    }

    /// Sends OP 4 to join, move, or leave a guild voice channel.
    pub async fn set_voice_state(
        &mut self,
        guild_id: &str,
        channel_id: Option<&str>,
    ) -> Result<(), BotError> {
        self.set_voice_state_with_flags(guild_id, channel_id, false, false).await
    }

    /// Sends OP 4 with explicit self-mute/deaf flags.
    pub async fn set_voice_state_with_flags(
        &mut self,
        guild_id: &str,
        channel_id: Option<&str>,
        self_mute: bool,
        self_deaf: bool,
    ) -> Result<(), BotError> {
        let guild_id = validate_snowflake(guild_id, "guild ID")?;
        let channel_id =
            channel_id.map(|channel| validate_snowflake(channel, "channel ID")).transpose()?;
        self.send(json!({
            "op": 4,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf
            }
        }))
        .await
    }

    /// Polls one dispatch and services heartbeats while waiting.
    pub async fn next_event(&mut self) -> Result<Option<BotEvent>, BotError> {
        loop {
            tokio::select! {
                _ = sleep_until(self.next_heartbeat) => self.heartbeat().await?,
                message = self.socket.next() => {
                    let Some(message) = message else { return Ok(None); };
                    match message? {
                        Message::Text(text) => {
                            let payload: Value = serde_json::from_str(&text)?;
                            if let Some(sequence) = payload.get("s").and_then(Value::as_u64) {
                                self.sequence = Some(sequence);
                            }
                            match payload.get("op").and_then(Value::as_u64) {
                                Some(0) => return dispatch(&payload),
                                Some(1) => self.send_heartbeat().await?,
                                Some(7) => return Err(BotError::Reconnect),
                                Some(9) => return Err(BotError::InvalidSession),
                                Some(11) => self.heartbeat_acknowledged = true,
                                Some(10) => {},
                                Some(op) => return Err(BotError::Protocol(format!("unsupported gateway opcode {op}"))),
                                None => return Err(BotError::Protocol("gateway payload has no opcode".into())),
                            }
                        }
                        Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await?,
                        Message::Close(_) => return Ok(None),
                        Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
            }
        }
    }

    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }
}

fn gateway_url(base: &str) -> Result<String, BotError> {
    let mut url = Url::parse(base.trim()).map_err(|error| {
        BotError::Invalid(format!("Discord gateway returned an invalid URL: {error}"))
    })?;
    let scheme = url.scheme();
    let host = url
        .host_str()
        .ok_or_else(|| BotError::Invalid("Discord gateway URL has no host".into()))?;
    if scheme != "wss" && scheme != "ws" {
        return Err(BotError::Invalid("Discord gateway URL must use ws or wss".into()));
    }
    let host_lower = host.to_ascii_lowercase();
    let discord_host = host_lower == "gateway.discord.gg"
        || (host_lower.ends_with(".discord.gg") && host_lower.len() > ".discord.gg".len());
    let loopback = matches!(host_lower.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]");
    if scheme == "wss" && !discord_host {
        return Err(BotError::Invalid("remote Discord gateway URL has an untrusted host".into()));
    }
    // Plaintext WebSockets are only acceptable for the in-process loopback
    // simulator. A production gateway URL must be encrypted.
    if scheme == "ws" && !loopback {
        return Err(BotError::Invalid("remote Discord gateway URL must use wss".into()));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(BotError::Invalid(
            "Discord gateway URL must not contain credentials, query, or fragment".into(),
        ));
    }
    url.set_query(Some("v=10&encoding=json"));
    Ok(url.to_string())
}

fn validate_gateway_token(token: &str) -> Result<String, BotError> {
    if token.is_empty()
        || token.len() > 512
        || token.trim() != token
        || token.bytes().any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        return Err(BotError::Invalid(
            "bot token is empty, too long, or contains whitespace".into(),
        ));
    }
    Ok(token.to_owned())
}

async fn read_response_body(
    mut response: reqwest::Response,
    status: reqwest::StatusCode,
) -> Result<Vec<u8>, BotError> {
    if response.content_length().is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64) {
        return Err(BotError::HttpStatus {
            status: status.as_u16(),
            message: "Discord gateway response exceeded the configured limit".into(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                message: "Discord gateway response exceeded the configured limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn text_json(message: Message) -> Result<Value, BotError> {
    match message {
        Message::Text(text) => Ok(serde_json::from_str(&text)?),
        Message::Close(_) => Err(BotError::Protocol("gateway closed before HELLO".into())),
        _ => Err(BotError::Protocol("gateway HELLO was not a text payload".into())),
    }
}

fn dispatch(payload: &Value) -> Result<Option<BotEvent>, BotError> {
    let name = payload.get("t").and_then(Value::as_str).unwrap_or_default();
    let data = payload.get("d").cloned().unwrap_or(Value::Null);
    let event = match name {
        "READY" => {
            let ready: RawReady = serde_json::from_value(data)?;
            Some(BotEvent::Ready(Ready {
                user_id: ready.user.id,
                session_id: ready.session_id,
                resume_gateway_url: ready.resume_gateway_url,
            }))
        }
        "VOICE_STATE_UPDATE" => {
            let update: RawVoiceState = serde_json::from_value(data)?;
            Some(BotEvent::VoiceStateUpdate(VoiceStateUpdate {
                guild_id: update.guild_id,
                user_id: update.user_id,
                channel_id: update.channel_id,
                session_id: update.session_id,
            }))
        }
        "VOICE_SERVER_UPDATE" => {
            let update: RawVoiceServer = serde_json::from_value(data)?;
            Some(BotEvent::VoiceServerUpdate(VoiceServerUpdate {
                guild_id: update.guild_id,
                token: update.token,
                endpoint: update.endpoint,
            }))
        }
        "MESSAGE_CREATE" => {
            let message: RawMessage = serde_json::from_value(data)?;
            Some(BotEvent::MessageCreate(MessageCreate {
                id: message.id,
                channel_id: message.channel_id,
                guild_id: message.guild_id,
                author_id: message.author.id,
                content: message.content,
            }))
        }
        _ if name.is_empty() => None,
        _ => Some(BotEvent::Other { name: name.into(), data }),
    };
    Ok(event)
}

#[derive(Debug, Deserialize)]
struct RawReady {
    user: RawUser,
    session_id: String,
    #[serde(default = "default_resume_url")]
    resume_gateway_url: String,
}

fn default_resume_url() -> String {
    "wss://gateway.discord.gg".into()
}

#[derive(Debug, Deserialize)]
struct RawUser {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawVoiceState {
    guild_id: Option<String>,
    user_id: String,
    channel_id: Option<String>,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct RawVoiceServer {
    guild_id: String,
    token: String,
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    id: String,
    channel_id: String,
    guild_id: Option<String>,
    author: RawUser,
    #[serde(default)]
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;

    #[test]
    fn voice_server_dispatch_is_decoded_without_exposing_gateway_token() {
        let event = dispatch(&json!({
            "op": 0,
            "t": "VOICE_SERVER_UPDATE",
            "d": {"guild_id": "123", "token": "voice-secret", "endpoint": "voice.discord.media:443"}
        }))
        .unwrap();
        assert_eq!(
            event,
            Some(BotEvent::VoiceServerUpdate(VoiceServerUpdate {
                guild_id: "123".into(),
                token: "voice-secret".into(),
                endpoint: Some("voice.discord.media:443".into()),
            }))
        );
        let debug = format!("{event:?}");
        assert!(!debug.contains("voice-secret"));
        assert!(!debug.contains("bot-token"));
    }

    #[test]
    fn malformed_dispatch_is_rejected() {
        let result = dispatch(&json!({"op": 0, "t": "READY", "d": {}}));
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_gateway_simulator_covers_hello_identify_ready_and_heartbeat() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"op":10,"d":{"heartbeat_interval":5000}}).to_string().into(),
                ))
                .await
                .unwrap();
            let identify = socket.next().await.unwrap().unwrap();
            let identify: Value = match identify {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                other => panic!("unexpected identify payload: {other:?}"),
            };
            assert_eq!(identify["op"], 2);
            assert_eq!(identify["d"]["token"], "bot-token");
            assert_eq!(identify["d"]["intents"], 128);
            assert_eq!(identify["d"]["properties"]["$os"], "linux");
            assert_eq!(identify["d"]["properties"]["$browser"], "rustlink");
            assert_eq!(identify["d"]["properties"]["$device"], "rustlink");
            assert!(identify["d"]["properties"]["os"].is_null());
            socket
                .send(Message::Text(
                    json!({"op":0,"t":"READY","s":1,"d":{"session_id":"session","resume_gateway_url":"wss://gateway.discord.gg","user":{"id":"123"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket.send(Message::Text(json!({"op":1,"d":null}).to_string().into())).await.unwrap();
            let heartbeat = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let heartbeat: Value = match heartbeat {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                other => panic!("unexpected heartbeat payload: {other:?}"),
            };
            assert_eq!(heartbeat["op"], 1);
            assert_eq!(heartbeat["d"], 1);
            socket.send(Message::Text(json!({"op":11,"d":null}).to_string().into())).await.unwrap();
            socket.close(None).await.unwrap();
        });

        let mut gateway = Gateway::connect_url(
            format!("ws://{address}"),
            Zeroizing::new("bot-token".to_owned()),
            128,
        )
        .await
        .unwrap();
        let event = gateway.next_event().await.unwrap().unwrap();
        assert!(matches!(event, BotEvent::Ready(Ready { user_id, .. }) if user_id == "123"));
        assert!(gateway.next_event().await.unwrap().is_none());
        server.await.unwrap();
    }

    #[test]
    fn gateway_urls_are_limited_to_discord_or_loopback_test_hosts() {
        assert!(gateway_url("wss://gateway.discord.gg").is_ok());
        assert!(gateway_url("wss://gateway-us-east1-b.discord.gg/").is_ok());
        assert!(gateway_url("ws://127.0.0.1:1234").is_ok());
        for value in [
            "wss://example.test",
            "ws://example.test",
            "wss://gateway.discord.gg/?token=secret",
            "wss://user:pass@gateway.discord.gg",
        ] {
            assert!(gateway_url(value).is_err(), "accepted untrusted gateway URL {value}");
        }
    }

    #[test]
    fn gateway_token_validation_rejects_header_injection_and_whitespace() {
        assert!(validate_gateway_token("token-value").is_ok());
        for value in ["", " token", "token ", "token\nvalue", "token\tvalue"] {
            assert!(validate_gateway_token(value).is_err(), "accepted token {value:?}");
        }
    }
}
