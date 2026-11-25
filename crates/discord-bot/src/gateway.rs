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

use crate::{
    bot_auth_header, http_client, is_loopback_host, read_limited_body, validate_bot_token,
    validate_snowflake, validate_text, BotError, USER_AGENT_VALUE,
};

/// Discord gateway intents used by native bot applications.
pub const GUILDS: u64 = 1 << 0;
pub const GUILD_VOICE_STATES: u64 = 1 << 7;
pub const GUILD_MESSAGES: u64 = 1 << 9;
pub const MESSAGE_CONTENT: u64 = 1 << 15;

const API_BASE: &str = "https://discord.com/api/v10";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_GATEWAY_MESSAGE: usize = 16 << 20;
const MAX_HTTP_RESPONSE_BYTES: usize = 2 << 20;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GatewayInfo {
    pub url: String,
    #[serde(default)]
    pub shards: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub user_id: String,
    /// The application that owns the bot, needed to register slash commands.
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
        self.options.iter().find(|(key, _)| key == name).map(|(_, val)| val.as_str())
    }
}

impl std::fmt::Debug for Interaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interaction")
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VoiceStateUpdate {
    #[serde(default)]
    pub guild_id: Option<String>,
    pub user_id: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    pub session_id: String,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct VoiceServerUpdate {
    pub guild_id: String,
    pub token: String,
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl std::fmt::Debug for VoiceServerUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceServerUpdate")
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

impl BotEvent {
    /// Returns the guild ID associated with this event, if any.
    pub fn guild_id(&self) -> Option<&str> {
        match self {
            Self::VoiceStateUpdate(u) => u.guild_id.as_deref(),
            Self::VoiceServerUpdate(u) => Some(&u.guild_id),
            Self::MessageCreate(m) => m.guild_id.as_deref(),
            Self::Interaction(i) => i.guild_id.as_deref(),
            Self::Other { data, .. } => {
                data.get("guild_id").or_else(|| data.get("id")).and_then(Value::as_str)
            }
            Self::Ready(_) => None,
        }
    }
}

/// A single Discord main-gateway connection.
pub struct Gateway {
    socket: Socket,
    token: Zeroizing<String>,
    intents: u64,
    heartbeat_interval: Duration,
    next_heartbeat: Instant,
    heartbeat_acknowledged: bool,
    needs_flush: bool,
    close_code: Option<u16>,
    sequence: Option<u64>,
    session_id: Option<String>,
    resume_gateway_url: Option<String>,
    invalid_session_resumable: bool,
    resuming: bool,
    reconnect_attempts: u32,
}

fn reconnect_backoff(
    attempts: u32,
    close_code: Option<u16>,
    invalid_unresumable: bool,
) -> Duration {
    let base = if close_code == Some(4008) {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(u64::from(attempts.min(5)))
    };
    if invalid_unresumable {
        base.max(Duration::from_secs(2))
    } else {
        base
    }
}

fn prepare_token(token: impl Into<String>) -> Result<Zeroizing<String>, BotError> {
    let token = Zeroizing::new(token.into());
    validate_bot_token(&token)?;
    Ok(token)
}

impl Gateway {
    fn with_jitter(mut self) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let factor = 0.25 + f64::from((nanos / 100) % 7_500) / 10_000.0;
        self.next_heartbeat = Instant::now() + self.heartbeat_interval.mul_f64(factor);
        self
    }

    /// Fetch Discord's gateway URL, receive HELLO, and send IDENTIFY.
    pub async fn connect(token: impl Into<String>, intents: u64) -> Result<Self, BotError> {
        let token = prepare_token(token)?;
        let info = Self::fetch_gateway_info(&token).await?;
        Ok(Self::connect_url(&info.url, token, intents).await?.with_jitter())
    }

    /// Connects and awaits `READY` with `GUILDS | GUILD_VOICE_STATES | GUILD_MESSAGES | MESSAGE_CONTENT`,
    /// automatically falling back to unprivileged intents on close code `4014`.
    pub async fn connect_with_messages(
        token: impl Into<String>,
    ) -> Result<(Self, Ready, bool), BotError> {
        let token = prepare_token(token)?;
        let info = Self::fetch_gateway_info(&token).await?;
        Self::connect_url_with_messages(
            &info.url,
            token,
            GUILDS | GUILD_VOICE_STATES | GUILD_MESSAGES,
        )
        .await
    }

    async fn fetch_gateway_info(token: &str) -> Result<GatewayInfo, BotError> {
        let response = http_client(Duration::from_secs(20))?
            .get(format!("{API_BASE}/gateway/bot"))
            .header(reqwest::header::AUTHORIZATION, bot_auth_header(token)?)
            .header(reqwest::header::USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = response.status();
        let body = read_limited_body(
            response,
            status,
            MAX_HTTP_RESPONSE_BYTES,
            "Discord gateway response exceeded the configured limit",
        )
        .await?;
        if !status.is_success() {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                message: format!("gateway endpoint returned {} bytes", body.len()),
            });
        }
        serde_json::from_slice(&body).map_err(BotError::from)
    }

    async fn connect_url_with_messages(
        base: &str,
        token: Zeroizing<String>,
        base_intents: u64,
    ) -> Result<(Self, Ready, bool), BotError> {
        let mut gw = Self::connect_url(base, token.clone(), base_intents | MESSAGE_CONTENT)
            .await?
            .with_jitter();
        match gw.next_ready().await {
            Ok(ready) => Ok((gw, ready, true)),
            Err(_) if gw.close_code() == Some(4014) => {
                let mut fallback =
                    Self::connect_url(base, token, base_intents).await?.with_jitter();
                let ready = fallback.next_ready().await?;
                Ok((fallback, ready, false))
            }
            Err(error) => Err(error),
        }
    }

    /// Reconnects to `resume_gateway_url`, receives HELLO, and sends RESUME (`op: 6`).
    pub async fn resume(
        resume_gateway_url: impl AsRef<str>,
        token: impl Into<String>,
        session_id: impl AsRef<str>,
        sequence: u64,
        intents: u64,
    ) -> Result<Self, BotError> {
        let token = prepare_token(token)?;
        let session_id = validate_text(session_id.as_ref(), "session ID", 512)?;
        let url = resume_gateway_url.as_ref();
        let mut gw = Self::open_hello(url, token, intents).await?.with_jitter();
        gw.sequence = Some(sequence);
        gw.session_id = Some(session_id.to_owned());
        gw.resume_gateway_url = Some(url.to_owned());
        gw.resuming = true;
        gw.send(json!({
            "op": 6,
            "d": { "token": gw.token.as_str(), "session_id": session_id, "seq": sequence }
        }))
        .await?;
        Ok(gw)
    }

    /// Whether a gateway close code indicates a permanent configuration or auth error.
    pub fn is_fatal_close_code(code: u16) -> bool {
        matches!(code, 4004 | 4010..=4014)
    }

    /// Reconnects after `Reconnect`, `InvalidSession`, WebSocket error, or non-fatal
    /// close, resuming (`OP 6`) when valid and falling back to `IDENTIFY` (`OP 2`).
    pub async fn reconnect(&mut self, cause: Option<&BotError>) -> Result<(), BotError> {
        self.reconnect_inner(cause, None).await
    }

    async fn reconnect_inner(
        &mut self,
        cause: Option<&BotError>,
        identify_url: Option<&str>,
    ) -> Result<(), BotError> {
        let stripped_mc = self.close_code == Some(4014) && (self.intents & MESSAGE_CONTENT) != 0;
        if stripped_mc {
            self.intents &= !MESSAGE_CONTENT;
            self.close_code = None;
        }
        if let Some(code) = self.close_code.filter(|&c| Self::is_fatal_close_code(c)) {
            return Err(BotError::Protocol(format!("fatal Discord gateway close code {code}")));
        }
        let (close_code, was_resuming, attempts) =
            (self.close_code, self.resuming, self.reconnect_attempts);
        let next_attempts = attempts.saturating_add(1);
        self.reconnect_attempts = next_attempts;
        let _ = tokio::time::timeout(Duration::from_secs(1), self.socket.close(None)).await;

        let can_resume = !stripped_mc
            && !was_resuming
            && match cause {
                Some(BotError::InvalidSession) => self.invalid_session_resumable,
                _ => !matches!(close_code, Some(4007 | 4009)),
            };
        let delay = reconnect_backoff(
            attempts,
            close_code,
            !can_resume && matches!(cause, Some(BotError::InvalidSession)),
        );
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if can_resume {
            if let (Some(url), Some(sid), Some(seq)) =
                (self.resume_gateway_url.as_deref(), self.session_id.as_deref(), self.sequence)
            {
                if let Ok(mut resumed) =
                    Self::resume(url, self.token.as_str(), sid, seq, self.intents).await
                {
                    resumed.reconnect_attempts = next_attempts;
                    *self = resumed;
                    return Ok(());
                }
            }
        }
        let mut fresh = match identify_url {
            Some(url) => {
                Self::connect_url(url, self.token.clone(), self.intents).await?.with_jitter()
            }
            None => Self::connect(self.token.as_str(), self.intents).await?,
        };
        fresh.reconnect_attempts = next_attempts;
        *self = fresh;
        Ok(())
    }

    async fn connect_url(
        base: impl AsRef<str>,
        token: Zeroizing<String>,
        intents: u64,
    ) -> Result<Self, BotError> {
        let mut gw = Self::open_hello(base.as_ref(), token, intents).await?;
        gw.identify().await?;
        Ok(gw)
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
            tokio_tungstenite::connect_async_with_config(&url, Some(config), true),
        )
        .await
        .map_err(|_| BotError::Protocol("gateway HELLO timed out".into()))??;

        let mut needs_flush = false;
        let hello: Value = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                match socket
                    .next()
                    .await
                    .ok_or_else(|| BotError::Protocol("gateway closed before HELLO".into()))??
                {
                    Message::Ping(_) => needs_flush = true,
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        return Err(BotError::Protocol("gateway closed before HELLO".into()));
                    }
                    Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                    _ => return Err(BotError::Protocol("gateway HELLO was not text".into())),
                }
            }
        })
        .await
        .map_err(|_| BotError::Protocol("gateway HELLO timed out".into()))??;

        if hello.get("op").and_then(Value::as_u64) != Some(10) {
            return Err(BotError::Protocol("first gateway payload was not HELLO".into()));
        }
        let interval = hello["d"]["heartbeat_interval"]
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms >= 500.0)
            .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
            .ok_or_else(|| BotError::Protocol("HELLO has invalid heartbeat interval".into()))?;

        Ok(Self {
            socket,
            token,
            intents,
            heartbeat_interval: interval,
            next_heartbeat: Instant::now() + interval,
            heartbeat_acknowledged: true,
            needs_flush,
            close_code: None,
            sequence: None,
            session_id: None,
            resume_gateway_url: None,
            invalid_session_resumable: false,
            resuming: false,
            reconnect_attempts: 0,
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
        let text = match self.sequence {
            Some(seq) => format!(r#"{{"op":1,"d":{seq}}}"#),
            None => r#"{"op":1,"d":null}"#.to_owned(),
        };
        self.feed_text(text).await?;
        self.heartbeat_acknowledged = false;
        self.next_heartbeat = Instant::now() + self.heartbeat_interval;
        self.flush_socket().await
    }

    async fn send(&mut self, payload: Value) -> Result<(), BotError> {
        self.feed_text(payload.to_string()).await?;
        self.flush_socket().await
    }

    async fn feed_text(&mut self, text: String) -> Result<(), BotError> {
        tokio::time::timeout(WRITE_TIMEOUT, self.socket.feed(Message::Text(text.into())))
            .await
            .map_err(|_| BotError::Protocol("gateway write timed out".into()))??;
        self.needs_flush = true;
        Ok(())
    }

    async fn flush_socket(&mut self) -> Result<(), BotError> {
        tokio::time::timeout(WRITE_TIMEOUT, self.socket.flush())
            .await
            .map_err(|_| BotError::Protocol("gateway write timed out".into()))??;
        self.needs_flush = false;
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
        let channel_id = channel_id.map(|ch| validate_snowflake(ch, "channel ID")).transpose()?;
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

    /// Polls events until Discord sends `READY`.
    pub async fn next_ready(&mut self) -> Result<Ready, BotError> {
        loop {
            match self.next_event().await? {
                Some(BotEvent::Ready(ready)) => return Ok(ready),
                Some(_) => {}
                None => {
                    let detail = match self.close_code {
                        Some(code) => format!("gateway closed before READY (code {code})"),
                        None => "gateway closed before READY".into(),
                    };
                    return Err(BotError::Protocol(detail));
                }
            }
        }
    }

    /// Polls one dispatch and services heartbeats while waiting.
    pub async fn next_event(&mut self) -> Result<Option<BotEvent>, BotError> {
        loop {
            if self.needs_flush {
                self.flush_socket().await?;
            }
            tokio::select! {
                // Drain queued socket frames before firing a due heartbeat timer so
                // an already-received OP 11 ACK is never misread as a missed beat.
                biased;
                message = self.socket.next() => {
                    let Some(message) = message else { return Ok(None); };
                    match message? {
                        Message::Text(text) => {
                            let payload: Value = serde_json::from_str(&text)?;
                            if let Some(seq) = payload.get("s").and_then(Value::as_u64) {
                                self.sequence = Some(seq);
                            }
                            match payload.get("op").and_then(Value::as_u64) {
                                Some(0) => {
                                    self.resuming = false;
                                    self.reconnect_attempts = 0;
                                    let event = dispatch(payload)?;
                                    if let Some(BotEvent::Ready(ready)) = &event {
                                        self.session_id = Some(ready.session_id.clone());
                                        self.resume_gateway_url =
                                            Some(ready.resume_gateway_url.clone());
                                    }
                                    return Ok(event);
                                }
                                Some(1) => self.send_heartbeat().await?,
                                Some(7) => return Err(BotError::Reconnect),
                                Some(9) => {
                                    self.invalid_session_resumable =
                                        payload.get("d").and_then(Value::as_bool).unwrap_or(false);
                                    return Err(BotError::InvalidSession);
                                }
                                Some(11) => self.heartbeat_acknowledged = true,
                                Some(10) => {}
                                Some(op) => return Err(BotError::Protocol(format!("unsupported gateway opcode {op}"))),
                                None => return Err(BotError::Protocol("gateway payload has no opcode".into())),
                            }
                        }
                        Message::Ping(_) => self.needs_flush = true,
                        Message::Close(frame) => {
                            self.close_code = frame.map(|f| u16::from(f.code));
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

    /// Close code received from Discord once `next_event` returns `None`.
    pub fn close_code(&self) -> Option<u16> {
        self.close_code
    }

    /// Whether the latest `OP 9` (`Invalid Session`) permitted `Gateway::resume`.
    pub fn invalid_session_resumable(&self) -> bool {
        self.invalid_session_resumable
    }
}

fn gateway_url(base: &str) -> Result<String, BotError> {
    let mut url = Url::parse(base.trim())
        .map_err(|e| BotError::Invalid(format!("Discord gateway returned an invalid URL: {e}")))?;
    let scheme = url.scheme();
    let host = url
        .host_str()
        .ok_or_else(|| BotError::Invalid("Discord gateway URL has no host".into()))?;
    if !matches!(scheme, "wss" | "ws") {
        return Err(BotError::Invalid("Discord gateway URL must use ws or wss".into()));
    }
    let discord_host = host.len() > ".discord.gg".len()
        && host.as_bytes()[host.len() - ".discord.gg".len()..].eq_ignore_ascii_case(b".discord.gg");
    if scheme == "wss" && !discord_host {
        return Err(BotError::Invalid("remote Discord gateway URL has an untrusted host".into()));
    }
    if scheme == "ws" && !is_loopback_host(host) {
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
    Ok(url.into())
}

fn dispatch(payload: Value) -> Result<Option<BotEvent>, BotError> {
    let Value::Object(mut map) = payload else { return Ok(None) };
    let Some(Value::String(name)) = map.remove("t") else { return Ok(None) };
    if name.is_empty() {
        return Ok(None);
    }
    let data = map.remove("d").unwrap_or(Value::Null);
    let event = match name.as_str() {
        "READY" => {
            let ready: RawReady = serde_json::from_value(data)?;
            Some(BotEvent::Ready(Ready {
                user_id: ready.user.id,
                application_id: ready.application.map(|a| a.id).unwrap_or_default(),
                session_id: ready.session_id,
                resume_gateway_url: ready
                    .resume_gateway_url
                    .filter(|u| !u.trim().is_empty())
                    .unwrap_or_else(|| "wss://gateway.discord.gg".into()),
            }))
        }
        "VOICE_STATE_UPDATE" => Some(BotEvent::VoiceStateUpdate(serde_json::from_value(data)?)),
        "VOICE_SERVER_UPDATE" => Some(BotEvent::VoiceServerUpdate(serde_json::from_value(data)?)),
        "MESSAGE_CREATE" => {
            let msg: RawMessage = serde_json::from_value(data)?;
            if msg.author.bot || msg.webhook_id.is_some() {
                return Ok(None);
            }
            Some(BotEvent::MessageCreate(MessageCreate {
                id: msg.id,
                channel_id: msg.channel_id,
                guild_id: msg.guild_id,
                author_id: msg.author.id,
                content: msg.content,
            }))
        }
        "INTERACTION_CREATE" if data.get("type").and_then(Value::as_u64) == Some(2) => {
            let raw: RawInteraction = serde_json::from_value(data)?;
            let user_id = raw
                .member
                .and_then(|m| m.user)
                .or(raw.user)
                .ok_or_else(|| BotError::Protocol("interaction has no user".into()))?
                .id;
            let options = raw
                .data
                .options
                .into_iter()
                .filter_map(|o| match o.value {
                    Value::String(v) => Some((o.name, v)),
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
        _ => Some(BotEvent::Other { name, data }),
    };
    Ok(event)
}

#[derive(Deserialize)]
struct RawReady {
    user: RawUser,
    #[serde(default)]
    application: Option<RawUser>,
    session_id: String,
    #[serde(default)]
    resume_gateway_url: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    id: String,
    #[serde(default)]
    bot: bool,
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

#[derive(Deserialize)]
struct RawMessage {
    id: String,
    channel_id: String,
    guild_id: Option<String>,
    author: RawUser,
    #[serde(default)]
    webhook_id: Option<String>,
    #[serde(default)]
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

    type ServerSocket = WebSocketStream<TcpStream>;

    async fn bind_ws() -> (TcpListener, String) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let ws_url = format!("ws://{}", listener.local_addr().unwrap());
        (listener, ws_url)
    }

    async fn accept_ws(listener: &TcpListener) -> ServerSocket {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    }

    async fn send_json(socket: &mut ServerSocket, payload: Value) {
        socket.send(Message::Text(payload.to_string().into())).await.unwrap();
    }

    async fn recv_json(socket: &mut ServerSocket) -> Value {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    #[test]
    fn message_create_ignores_bots_and_webhooks_and_emits_human_messages() {
        let bot_msg = json!({"op": 0, "t": "MESSAGE_CREATE", "d": {
            "id": "1", "channel_id": "10", "guild_id": "20",
            "author": {"id": "30", "bot": true}, "content": "https://youtu.be/dQw4w9WgXcQ"
        }});
        assert_eq!(dispatch(bot_msg).unwrap(), None);

        let webhook_msg = json!({"op": 0, "t": "MESSAGE_CREATE", "d": {
            "id": "2", "channel_id": "10", "guild_id": "20",
            "author": {"id": "31"}, "webhook_id": "99", "content": "https://youtu.be/dQw4w9WgXcQ"
        }});
        assert_eq!(dispatch(webhook_msg).unwrap(), None);

        let user_msg = json!({"op": 0, "t": "MESSAGE_CREATE", "d": {
            "id": "3", "channel_id": "10", "guild_id": "20",
            "author": {"id": "32"}, "content": "https://youtu.be/dQw4w9WgXcQ"
        }});
        assert_eq!(
            dispatch(user_msg).unwrap(),
            Some(BotEvent::MessageCreate(MessageCreate {
                id: "3".into(),
                channel_id: "10".into(),
                guild_id: Some("20".into()),
                author_id: "32".into(),
                content: "https://youtu.be/dQw4w9WgXcQ".into(),
            }))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connect_url_with_messages_falls_back_when_message_content_is_disallowed() {
        let (listener, ws_url) = bind_ws().await;
        let base_intents = GUILDS | GUILD_VOICE_STATES | GUILD_MESSAGES;
        let server = tokio::spawn(async move {
            let hello = json!({"op": 10, "d": {"heartbeat_interval": 5000}});
            let mut s1 = accept_ws(&listener).await;
            send_json(&mut s1, hello.clone()).await;
            assert_eq!(
                recv_json(&mut s1).await["d"]["intents"].as_u64(),
                Some(base_intents | MESSAGE_CONTENT)
            );
            s1.close(Some(CloseFrame { code: CloseCode::from(4014), reason: "Disallowed".into() }))
                .await
                .unwrap();

            let mut s2 = accept_ws(&listener).await;
            send_json(&mut s2, hello).await;
            assert_eq!(recv_json(&mut s2).await["d"]["intents"].as_u64(), Some(base_intents));
            send_json(
                &mut s2,
                json!({
                    "op": 0, "t": "READY", "s": 1,
                    "d": {
                        "session_id": "sess-fallback",
                        "resume_gateway_url": "wss://gateway.discord.gg",
                        "user": {"id": "777"},
                        "application": {"id": "888"}
                    }
                }),
            )
            .await;
            s2.close(None).await.unwrap();
        });

        let (gw, ready, mc) = Gateway::connect_url_with_messages(
            &ws_url,
            Zeroizing::new("bot-token".into()),
            base_intents,
        )
        .await
        .unwrap();
        assert!(!mc && ready.user_id == "777" && ready.application_id == "888");
        assert_eq!(gw.intents, base_intents);
        server.await.unwrap();
    }

    #[test]
    fn voice_server_dispatch_is_decoded_without_exposing_gateway_token() {
        let event = dispatch(json!({
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
        assert!(!debug.contains("voice-secret") && !debug.contains("bot-token"));
    }

    #[test]
    fn slash_commands_and_the_application_id_are_decoded() {
        let ready = json!({"op": 0, "t": "READY", "d": {
            "user": {"id": "100"}, "application": {"id": "900"}, "session_id": "s"
        }});
        let Some(BotEvent::Ready(ready)) = dispatch(ready).unwrap() else { panic!() };
        assert_eq!(ready.application_id, "900");

        let interaction = json!({"op": 0, "t": "INTERACTION_CREATE", "d": {
            "id": "1", "type": 2, "token": "secret-interaction-token",
            "guild_id": "200", "channel_id": "300", "member": {"user": {"id": "400"}},
            "data": {"name": "play", "options": [
                {"name": "url", "type": 3, "value": "https://youtu.be/dQw4w9WgXcQ"},
                {"name": "volume", "type": 4, "value": 50}
            ]}
        }});
        let Some(BotEvent::Interaction(ix)) = dispatch(interaction).unwrap() else { panic!() };
        assert_eq!((ix.user_id.as_str(), ix.command.as_str()), ("400", "play"));
        assert_eq!(ix.option("url"), Some("https://youtu.be/dQw4w9WgXcQ"));
        assert_eq!(ix.option("volume"), None);
        assert!(!format!("{ix:?}").contains("secret-interaction-token"));

        let button = json!({"op": 0, "t": "INTERACTION_CREATE", "d": {"type": 3}});
        assert!(matches!(dispatch(button).unwrap(), Some(BotEvent::Other { .. })));
    }

    #[test]
    fn malformed_dispatch_is_rejected() {
        assert!(dispatch(json!({"op": 0, "t": "READY", "d": {}})).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_gateway_simulator_covers_hello_identify_ready_and_heartbeat() {
        let (listener, ws_url) = bind_ws().await;
        let server = tokio::spawn(async move {
            let mut socket = accept_ws(&listener).await;
            send_json(&mut socket, json!({"op": 10, "d": {"heartbeat_interval": 5000}})).await;
            let identify = recv_json(&mut socket).await;
            assert_eq!(identify["op"], 2);
            assert_eq!(identify["d"]["token"], "bot-token");
            assert_eq!(identify["d"]["intents"], 128);
            assert_eq!(identify["d"]["properties"]["$os"], "linux");
            assert_eq!(identify["d"]["properties"]["$browser"], "rustlink");
            assert_eq!(identify["d"]["properties"]["$device"], "rustlink");
            assert!(identify["d"]["properties"]["os"].is_null());
            send_json(
                &mut socket,
                json!({"op": 0, "t": "READY", "s": 1, "d": {
                    "session_id": "session",
                    "resume_gateway_url": "wss://gateway.discord.gg",
                    "user": {"id": "123"}
                }}),
            )
            .await;
            send_json(&mut socket, json!({"op": 1, "d": null})).await;
            let beat =
                tokio::time::timeout(Duration::from_secs(2), recv_json(&mut socket)).await.unwrap();
            assert_eq!((beat["op"].as_u64(), beat["d"].as_u64()), (Some(1), Some(1)));
            send_json(&mut socket, json!({"op": 11, "d": null})).await;
            socket.close(None).await.unwrap();
        });

        let mut gw =
            Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), 128).await.unwrap();
        let event = gw.next_event().await.unwrap().unwrap();
        assert!(matches!(event, BotEvent::Ready(Ready { user_id, .. }) if user_id == "123"));
        assert!(gw.next_event().await.unwrap().is_none());
        server.await.unwrap();
    }

    async fn slow_caller() -> Result<Option<BotEvent>, BotError> {
        let (listener, ws_url) = bind_ws().await;
        let server = tokio::spawn(async move {
            let mut socket = accept_ws(&listener).await;
            send_json(&mut socket, json!({"op": 10, "d": {"heartbeat_interval": 500}})).await;
            let _identify = socket.next().await;
            let _first_heartbeat = socket.next().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            send_json(&mut socket, json!({"op": 11, "d": null})).await;
            tokio::time::sleep(Duration::from_millis(600)).await;
            send_json(&mut socket, json!({"op": 0, "t": "SOMETHING", "s": 2, "d": {"x": 1}})).await;
            let _ = socket.next().await;
        });
        let mut gw =
            Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), 128).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(650), gw.next_event()).await.is_err());
        tokio::time::sleep(Duration::from_millis(600)).await;
        let event = gw.next_event().await;
        server.abort();
        event
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_acknowledgement_already_received_is_never_missed_for_a_late_heartbeat() {
        for outcome in futures_util::future::join_all((0..16).map(|_| slow_caller())).await {
            let event = outcome.expect("acknowledged heartbeat must not fail");
            assert!(matches!(event, Some(BotEvent::Other { .. })), "{event:?}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_close_code_is_kept_so_a_refused_token_can_be_told_from_a_dropped_link() {
        let (listener, ws_url) = bind_ws().await;
        let server = tokio::spawn(async move {
            let mut socket = accept_ws(&listener).await;
            send_json(&mut socket, json!({"op": 10, "d": {"heartbeat_interval": 5000}})).await;
            let _identify = socket.next().await;
            socket
                .close(Some(CloseFrame {
                    code: CloseCode::from(4004),
                    reason: "Auth failed".into(),
                }))
                .await
                .unwrap();
        });
        let mut gw =
            Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), 128).await.unwrap();
        assert_eq!(gw.close_code(), None);
        assert!(gw.next_event().await.unwrap().is_none());
        assert_eq!(gw.close_code(), Some(4004));
        server.await.unwrap();
    }

    #[test]
    fn gateway_urls_are_limited_to_discord_or_loopback_test_hosts() {
        assert!(gateway_url("wss://gateway.discord.gg").is_ok());
        assert!(gateway_url("wss://gateway-us-east1-b.discord.gg/").is_ok());
        assert!(gateway_url("ws://127.0.0.1:1234").is_ok());
        for bad in [
            "wss://example.test",
            "ws://example.test",
            "wss://gateway.discord.gg/?token=secret",
            "wss://user:pass@gateway.discord.gg",
        ] {
            assert!(gateway_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gateway_token_validation_rejects_header_injection_and_whitespace() {
        assert!(validate_bot_token("token-value").is_ok());
        for bad in ["", " token", "token ", "token\nvalue", "token\tvalue"] {
            assert!(validate_bot_token(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reconnect_backoff_bounds_delays_and_honours_rate_limit_close_code() {
        assert_eq!(reconnect_backoff(0, None, false), Duration::ZERO);
        assert_eq!(reconnect_backoff(1, None, false), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(3, None, false), Duration::from_secs(3));
        assert_eq!(reconnect_backoff(5, None, false), Duration::from_secs(5));
        assert_eq!(reconnect_backoff(99, None, false), Duration::from_secs(5));
        assert_eq!(reconnect_backoff(0, Some(4008), false), Duration::from_secs(5));
        assert_eq!(reconnect_backoff(2, Some(4008), false), Duration::from_secs(5));
        assert_eq!(reconnect_backoff(0, None, true), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(4, None, true), Duration::from_secs(4));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_hello_skips_ping_and_pong_frames_before_hello() {
        let (listener, ws_url) = bind_ws().await;
        let server = tokio::spawn(async move {
            let mut socket = accept_ws(&listener).await;
            socket.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
            socket.send(Message::Pong(Vec::new().into())).await.unwrap();
            send_json(&mut socket, json!({"op": 10, "d": {"heartbeat_interval": 5000}})).await;
            let (mut saw_pong, mut saw_identify) = (false, false);
            for _ in 0..2 {
                match socket.next().await.unwrap().unwrap() {
                    Message::Pong(p) => {
                        assert_eq!(p.as_ref(), &[1, 2, 3]);
                        saw_pong = true;
                    }
                    Message::Text(t) => {
                        assert_eq!(serde_json::from_str::<Value>(&t).unwrap()["op"], 2);
                        saw_identify = true;
                    }
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
            assert!(saw_pong && saw_identify);
            socket.close(None).await.unwrap();
        });

        let mut gw = Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), GUILDS)
            .await
            .unwrap();
        assert!(!gw.needs_flush);
        assert!(gw.next_event().await.unwrap().is_none());
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unconfirmed_resume_falls_back_to_identify_with_backoff() {
        let (listener, ws_url) = bind_ws().await;
        let server_ws_url = ws_url.clone();
        let server = tokio::spawn(async move {
            let hello = json!({"op": 10, "d": {"heartbeat_interval": 5000}});

            let mut s1 = accept_ws(&listener).await;
            send_json(&mut s1, hello.clone()).await;
            assert_eq!(recv_json(&mut s1).await["op"], 2);
            send_json(
                &mut s1,
                json!({
                    "op": 0, "t": "READY", "s": 7,
                    "d": {"session_id": "sess-1", "resume_gateway_url": server_ws_url, "user": {"id": "123"}}
                }),
            )
            .await;
            s1.close(None).await.unwrap();

            let mut s2 = accept_ws(&listener).await;
            send_json(&mut s2, hello.clone()).await;
            let second = recv_json(&mut s2).await;
            assert_eq!(second["op"], 6);
            assert_eq!(second["d"]["session_id"], "sess-1");
            assert_eq!(second["d"]["seq"], 7);
            s2.close(None).await.unwrap();

            let mut s3 = accept_ws(&listener).await;
            send_json(&mut s3, hello).await;
            assert_eq!(recv_json(&mut s3).await["op"], 2);
            send_json(
                &mut s3,
                json!({
                    "op": 0, "t": "READY", "s": 1,
                    "d": {"session_id": "sess-2", "resume_gateway_url": server_ws_url, "user": {"id": "123"}}
                }),
            )
            .await;
            s3.close(None).await.unwrap();
        });

        let mut gw = Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), GUILDS)
            .await
            .unwrap();
        assert_eq!(gw.next_ready().await.unwrap().session_id, "sess-1");
        assert!(!gw.resuming && gw.reconnect_attempts == 0);

        assert!(gw.next_event().await.unwrap().is_none());
        gw.reconnect_inner(None, Some(&ws_url)).await.unwrap();
        assert!(gw.resuming && gw.reconnect_attempts == 1);

        assert!(gw.next_event().await.unwrap().is_none());
        let started = Instant::now();
        gw.reconnect_inner(None, Some(&ws_url)).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(!gw.resuming && gw.reconnect_attempts == 2);

        assert_eq!(gw.next_ready().await.unwrap().session_id, "sess-2");
        assert!(!gw.resuming && gw.reconnect_attempts == 0);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_next_event_flushes_buffered_heartbeat_without_duplicating_it() {
        let (listener, ws_url) = bind_ws().await;
        let server = tokio::spawn(async move {
            let mut socket = accept_ws(&listener).await;
            send_json(&mut socket, json!({"op": 10, "d": {"heartbeat_interval": 500}})).await;
            let _identify = recv_json(&mut socket).await;
            assert_eq!(recv_json(&mut socket).await["op"], 1);
            send_json(&mut socket, json!({"op": 11, "d": null})).await;
            send_json(&mut socket, json!({"op": 0, "t": "PING_CHECK", "s": 1, "d": {}})).await;
            socket.close(None).await.unwrap();
        });

        let mut gw = Gateway::connect_url(&ws_url, Zeroizing::new("bot-token".into()), GUILDS)
            .await
            .unwrap();
        gw.feed_text(r#"{"op":1,"d":null}"#.into()).await.unwrap();
        gw.heartbeat_acknowledged = false;
        gw.next_heartbeat = Instant::now() + gw.heartbeat_interval;
        assert!(gw.needs_flush);

        let event = gw.next_event().await.unwrap().unwrap();
        assert!(!gw.needs_flush && gw.heartbeat_acknowledged);
        assert!(matches!(event, BotEvent::Other { name, .. } if name == "PING_CHECK"));
        assert!(gw.next_event().await.unwrap().is_none());
        server.await.unwrap();
    }
}
