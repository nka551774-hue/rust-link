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
const USER_AGENT_VALUE: &str =
    concat!("DiscordBot (https://github.com/rustlink, ", env!("CARGO_PKG_VERSION"), ")");
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_GATEWAY_MESSAGE: usize = 16 * 1024 * 1024;
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
    /// The application that owns the bot, needed to register slash commands.
    /// Empty when Discord omits it.
    pub application_id: String,
    pub session_id: String,
    pub resume_gateway_url: String,
}

/// A slash-command invocation (`INTERACTION_CREATE` of type 2).
#[derive(Clone, PartialEq, Eq)]
pub struct Interaction {
    pub id: String,
    /// Short-lived credential for answering this interaction.
    pub token: String,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: String,
    pub command: String,
    /// Top-level string options as `(name, value)`.
    pub options: Vec<(String, String)>,
}

impl Interaction {
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

impl std::fmt::Debug for Interaction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Interaction")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("command", &self.command)
            .field("options", &self.options)
            .finish()
    }
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
    Interaction(Interaction),
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
    /// The code Discord closed the connection with, once it has.
    close_code: Option<u16>,
    sequence: Option<u64>,
    invalid_session_resumable: bool,
}

impl Gateway {
    /// Fetch Discord's gateway URL, receive HELLO, and send IDENTIFY.
    pub async fn connect(token: impl Into<String>, intents: u64) -> Result<Self, BotError> {
        let token = validate_gateway_token(&token.into())?;
        let mut auth =
            reqwest::header::HeaderValue::from_str(&format!("Bot {token}")).map_err(|_| {
                BotError::Invalid("bot token contains invalid header characters".into())
            })?;
        auth.set_sensitive(true);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = http
            .get(format!("{API_BASE}/gateway/bot"))
            .header(reqwest::header::AUTHORIZATION, auth)
            .header(reqwest::header::USER_AGENT, USER_AGENT_VALUE)
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
        let mut gateway = Self::connect_url(info.url, Zeroizing::new(token), intents).await?;
        // Per Discord Gateway v10, jitter the first heartbeat on production connections.
        let jitter_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let factor = 0.25 + (f64::from(jitter_nanos % 7_500) / 10_000.0);
        gateway.next_heartbeat = Instant::now() + gateway.heartbeat_interval.mul_f64(factor);
        Ok(gateway)
    }

    /// Reconnects to `resume_gateway_url`, receives HELLO, and sends RESUME (`op: 6`).
    pub async fn resume(
        resume_gateway_url: impl AsRef<str>,
        token: impl Into<String>,
        session_id: impl AsRef<str>,
        sequence: u64,
        intents: u64,
    ) -> Result<Self, BotError> {
        let token = Zeroizing::new(validate_gateway_token(&token.into())?);
        let session_id = crate::validate_text(session_id.as_ref(), "session ID")?;
        let mut gateway = Self::open_hello(resume_gateway_url.as_ref(), token, intents).await?;
        gateway.sequence = Some(sequence);
        gateway
            .send(json!({
                "op": 6,
                "d": {
                    "token": gateway.token.as_str(),
                    "session_id": session_id,
                    "seq": sequence
                }
            }))
            .await?;
        Ok(gateway)
    }

    async fn connect_url(
        base: String,
        token: Zeroizing<String>,
        intents: u64,
    ) -> Result<Self, BotError> {
        let mut gateway = Self::open_hello(&base, token, intents).await?;
        gateway.identify().await?;
        Ok(gateway)
    }

    async fn open_hello(
        base: &str,
        token: Zeroizing<String>,
        intents: u64,
    ) -> Result<Self, BotError> {
        let url = gateway_url(base)?;
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
            .as_f64()
            .filter(|interval| interval.is_finite() && *interval >= 500.0)
            .ok_or_else(|| BotError::Protocol("HELLO has invalid heartbeat interval".into()))?;
        let interval = Duration::try_from_secs_f64(interval_ms / 1000.0)
            .map_err(|_| BotError::Protocol("HELLO has invalid heartbeat interval".into()))?;
        Ok(Self {
            socket,
            token,
            intents,
            heartbeat_interval: interval,
            next_heartbeat: Instant::now() + interval,
            heartbeat_acknowledged: true,
            close_code: None,
            sequence: None,
            invalid_session_resumable: false,
        })
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
                // What has already arrived is read before the heartbeat timer is
                // looked at. A caller that was busy for a moment leaves both the
                // acknowledgement of the last beat and the next beat's deadline
                // ready at once, and picking the timer first would call a beat
                // unanswered while its answer sits in the socket.
                biased;
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
                                Some(9) => {
                                    self.invalid_session_resumable =
                                        payload.get("d").and_then(Value::as_bool).unwrap_or(false);
                                    return Err(BotError::InvalidSession);
                                }
                                Some(11) => self.heartbeat_acknowledged = true,
                                Some(10) => {},
                                Some(op) => return Err(BotError::Protocol(format!("unsupported gateway opcode {op}"))),
                                None => return Err(BotError::Protocol("gateway payload has no opcode".into())),
                            }
                        }
                        Message::Ping(_) => {
                            tokio::time::timeout(Duration::from_secs(10), self.socket.flush())
                                .await
                                .map_err(|_| BotError::Protocol("gateway write timed out".into()))??;
                        }
                        Message::Close(frame) => {
                            self.close_code = frame.map(|frame| u16::from(frame.code));
                            return Ok(None);
                        }
                        Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                _ = sleep_until(self.next_heartbeat) => self.heartbeat().await?,
            }
        }
    }

    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    /// The code Discord closed the connection with, after `next_event` has
    /// returned `None` for it. 4004 means the token was refused and 4010 to 4014
    /// mean the bot's own configuration (shards, intents) is wrong: none of
    /// them can be cured by connecting again. Other codes, and a close with no
    /// code at all, are worth a retry.
    pub fn close_code(&self) -> Option<u16> {
        self.close_code
    }

    /// Whether the most recent `OP 9` (`Invalid Session`) payload indicated that
    /// resuming (`Gateway::resume`) is allowed (`d: true`).
    pub fn invalid_session_resumable(&self) -> bool {
        self.invalid_session_resumable
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
    if token.is_empty() || token.len() > 512 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
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
                application_id: ready
                    .application
                    .map(|application| application.id)
                    .unwrap_or_default(),
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
        // Type 2 is an application (slash) command; buttons and the rest stay `Other`.
        "INTERACTION_CREATE" if data.get("type").and_then(Value::as_u64) == Some(2) => {
            let raw: RawInteraction = serde_json::from_value(data)?;
            let user_id = raw
                .member
                .and_then(|member| member.user)
                .or(raw.user)
                .ok_or_else(|| BotError::Protocol("interaction has no user".into()))?
                .id;
            let options = raw
                .data
                .options
                .into_iter()
                .filter_map(|option| match option.value {
                    Value::String(value) => Some((option.name, value)),
                    _ => None,
                })
                .collect();
            Some(BotEvent::Interaction(Interaction {
                id: raw.id,
                token: raw.token,
                guild_id: raw.guild_id,
                channel_id: raw.channel_id,
                user_id,
                command: raw.data.name,
                options,
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
    #[serde(default)]
    application: Option<RawUser>,
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

#[derive(Deserialize)]
struct RawInteraction {
    id: String,
    token: String,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    member: Option<RawMember>,
    #[serde(default)]
    user: Option<RawUser>,
    data: RawCommand,
}

#[derive(Deserialize)]
struct RawMember {
    #[serde(default)]
    user: Option<RawUser>,
}

#[derive(Deserialize)]
struct RawCommand {
    name: String,
    #[serde(default)]
    options: Vec<RawOption>,
}

#[derive(Deserialize)]
struct RawOption {
    name: String,
    #[serde(default)]
    value: Value,
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
    fn slash_commands_and_the_application_id_are_decoded() {
        let ready = json!({"op": 0, "t": "READY", "d": {
            "user": {"id": "100"}, "application": {"id": "900"}, "session_id": "s"
        }});
        let Some(BotEvent::Ready(ready)) = dispatch(&ready).unwrap() else { panic!() };
        assert_eq!(ready.application_id, "900");

        let interaction = json!({"op": 0, "t": "INTERACTION_CREATE", "d": {
            "id": "1", "type": 2, "token": "secret-interaction-token",
            "guild_id": "200", "channel_id": "300", "member": {"user": {"id": "400"}},
            "data": {"name": "play", "options": [
                {"name": "url", "type": 3, "value": "https://youtu.be/dQw4w9WgXcQ"},
                {"name": "volume", "type": 4, "value": 50}
            ]}
        }});
        let Some(BotEvent::Interaction(interaction)) = dispatch(&interaction).unwrap() else {
            panic!()
        };
        assert_eq!(interaction.user_id, "400");
        assert_eq!(interaction.command, "play");
        assert_eq!(interaction.option("url"), Some("https://youtu.be/dQw4w9WgXcQ"));
        assert_eq!(interaction.option("volume"), None, "only string options are kept");
        assert!(!format!("{interaction:?}").contains("secret-interaction-token"));

        // A button click (type 3) is not a slash command.
        let button = json!({"op": 0, "t": "INTERACTION_CREATE", "d": {"type": 3}});
        assert!(matches!(dispatch(&button).unwrap(), Some(BotEvent::Other { .. })));
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

    /// One connection on which the caller is slow to poll: the first heartbeat
    /// is acknowledged, then the caller does not read until the next heartbeat
    /// has also fallen due, so the acknowledgement and the deadline are ready
    /// together.
    async fn slow_caller() -> Result<Option<BotEvent>, BotError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = json!({"op":10,"d":{"heartbeat_interval":500}});
            socket.send(Message::Text(hello.to_string().into())).await.unwrap();
            let _identify = socket.next().await;
            let _first_heartbeat = socket.next().await;
            // The acknowledgement comes after the caller has stopped reading (at
            // 650 ms), so it can only be found sitting in the socket.
            tokio::time::sleep(Duration::from_millis(200)).await;
            socket.send(Message::Text(json!({"op":11,"d":null}).to_string().into())).await.unwrap();
            // A dispatch for the caller to return on once it reads again.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let event = json!({"op":0,"t":"SOMETHING","s":2,"d":{"x":1}});
            socket.send(Message::Text(event.to_string().into())).await.unwrap();
            let _ = socket.next().await;
        });
        let mut gateway = Gateway::connect_url(
            format!("ws://{address}"),
            Zeroizing::new("bot-token".to_owned()),
            128,
        )
        .await
        .unwrap();
        // The first heartbeat goes out at 500 ms; nothing is dispatched, so this waits out.
        assert!(tokio::time::timeout(Duration::from_millis(650), gateway.next_event())
            .await
            .is_err());
        // The acknowledgement is now in the socket. Staying away lets the next beat fall due.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let event = gateway.next_event().await;
        server.abort();
        event
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_acknowledgement_already_received_is_never_missed_for_a_late_heartbeat() {
        // Each connection used to fail about half the time, so sixteen at once
        // make a return of the race all but certain to be seen.
        let outcomes = futures_util::future::join_all((0..16).map(|_| slow_caller())).await;
        for outcome in outcomes {
            let event = outcome.expect("a heartbeat that was acknowledged is not unanswered");
            assert!(matches!(event, Some(BotEvent::Other { .. })), "{event:?}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_close_code_is_kept_so_a_refused_token_can_be_told_from_a_dropped_link() {
        use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = json!({"op":10,"d":{"heartbeat_interval":5000}});
            socket.send(Message::Text(hello.to_string().into())).await.unwrap();
            let _identify = socket.next().await;
            let frame =
                CloseFrame { code: CloseCode::from(4004), reason: "Authentication failed".into() };
            socket.close(Some(frame)).await.unwrap();
        });
        let mut gateway = Gateway::connect_url(
            format!("ws://{address}"),
            Zeroizing::new("bot-token".to_owned()),
            128,
        )
        .await
        .unwrap();
        assert_eq!(gateway.close_code(), None, "nothing has closed yet");
        assert!(gateway.next_event().await.unwrap().is_none());
        assert_eq!(gateway.close_code(), Some(4004));
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
