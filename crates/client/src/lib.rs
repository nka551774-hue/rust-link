//! An async Rust client for Rustlink's own API. No audio libraries are linked
//! into the bot: the server resolves tracks and sends audio to Discord.
//!
//! Create a session, keep its event stream open, and forward the bot's Discord
//! voice state to [`Session::set_voice`]. Explicitly call [`Session::destroy`]
//! at shutdown. Dropping a session leaves it available for its reconnect grace.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub use rustlink_protocol::{
    Int64, LoadResult, Object, Patch, Player, PlayerUpdate, Track, TrackUpdate, VoiceState,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Rustlink URL or identifier: {0}")]
    Invalid(String),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Rustlink returned HTTP {status}: {code}: {message}")]
    Api { status: u16, code: String, message: String },
    #[error("event connection failed: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("invalid JSON response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("event connection timed out")]
    Timeout,
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

/// Cloneable HTTP client. The password is deliberately excluded from Debug.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    password: String,
}

impl Client {
    /// `base` is the server origin, such as `http://127.0.0.1:2333`.
    pub fn new(base: &str, password: impl Into<String>) -> Result<Self, Error> {
        let mut base = Url::parse(base).map_err(|error| Error::Invalid(error.to_string()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Invalid(
                "expected an HTTP(S) origin without credentials, query or fragment".into(),
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let password = password.into();
        if password.is_empty()
            || reqwest::header::HeaderValue::from_str(&format!("Bearer {password}")).is_err()
        {
            return Err(Error::Invalid(
                "password is empty or contains invalid header characters".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, base, password })
    }

    fn endpoint(&self, path: &str) -> Result<Url, Error> {
        self.base.join(&format!("api/v1/{path}")).map_err(|error| Error::Invalid(error.to_string()))
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, Error> {
        let mut request =
            self.http.request(method, self.endpoint(path)?).bearer_auth(&self.password);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(serde_json::from_value(Value::Null)?);
        }
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Err(Error::Api {
                status: status.as_u16(),
                code: body["error"]["code"].as_str().unwrap_or("http_error").into(),
                message: body["error"]["message"]
                    .as_str()
                    .unwrap_or(status.canonical_reason().unwrap_or("request failed"))
                    .into(),
            });
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn info(&self) -> Result<Value, Error> {
        self.request(Method::GET, "info", None).await
    }
    pub async fn stats(&self) -> Result<Value, Error> {
        self.request(Method::GET, "stats", None).await
    }
    pub async fn resolve(&self, query: &str) -> Result<LoadResult, Error> {
        self.request(Method::POST, "tracks/resolve", Some(json!({"query":query}))).await
    }
    pub async fn inspect(&self, token: &str) -> Result<Track, Error> {
        self.request(Method::POST, "tracks/inspect", Some(json!({"token":token}))).await
    }
    pub async fn lyrics(&self, token: &str) -> Result<Option<Value>, Error> {
        self.request(Method::POST, "lyrics", Some(json!({"token":token}))).await
    }

    pub async fn create_session(
        &self,
        bot_id: &str,
        resume_timeout_secs: u32,
    ) -> Result<Session, Error> {
        let info: SessionInfo = self
            .request(
                Method::POST,
                "sessions",
                Some(json!({
                    "bot_id":bot_id, "resume_timeout_secs":resume_timeout_secs
                })),
            )
            .await?;
        Ok(Session { client: self.clone(), id: info.session_id })
    }

    /// Recover a saved session handle. Opening `events()` within its grace
    /// period resumes it; a server restart or expired session returns HTTP 404.
    pub async fn session(&self, id: &str) -> Result<Session, Error> {
        if id.len() != 48 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Invalid("session ID must contain 48 hexadecimal characters".into()));
        }
        let info: SessionInfo = self.request(Method::GET, &format!("sessions/{id}"), None).await?;
        Ok(Session { client: self.clone(), id: info.session_id })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub bot_id: String,
    pub connected: bool,
    pub resume: bool,
    pub resume_timeout_secs: u32,
    pub event_url: String,
}

#[derive(Clone)]
pub struct Session {
    client: Client,
    id: String,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }
    fn path(&self) -> String {
        format!("sessions/{}", self.id)
    }
    fn player_path(&self, guild: &str) -> Result<String, Error> {
        if guild.is_empty()
            || !guild.bytes().all(|byte| byte.is_ascii_digit())
            || guild.parse::<u64>().map_or(true, |id| id == 0)
        {
            return Err(Error::Invalid("guild ID must be a nonzero Discord snowflake".into()));
        }
        Ok(format!("{}/players/{guild}", self.path()))
    }
    pub async fn info(&self) -> Result<SessionInfo, Error> {
        self.client.request(Method::GET, &self.path(), None).await
    }
    pub async fn configure(
        &self,
        resume: bool,
        resume_timeout_secs: u32,
    ) -> Result<SessionInfo, Error> {
        self.client
            .request(
                Method::PATCH,
                &self.path(),
                Some(json!({"resume":resume,"resume_timeout_secs":resume_timeout_secs})),
            )
            .await
    }
    pub async fn destroy(&self) -> Result<(), Error> {
        self.client.request(Method::DELETE, &self.path(), None).await
    }
    pub async fn players(&self) -> Result<Vec<Player>, Error> {
        #[derive(Deserialize)]
        struct Players {
            players: Vec<Player>,
        }
        let body: Players =
            self.client.request(Method::GET, &format!("{}/players", self.path()), None).await?;
        Ok(body.players)
    }
    pub async fn player(&self, guild: &str) -> Result<Player, Error> {
        self.client.request(Method::GET, &self.player_path(guild)?, None).await
    }
    /// `replace = false` preserves an already loaded track.
    pub async fn update_player(
        &self,
        guild: &str,
        update: &PlayerUpdate,
        replace: bool,
    ) -> Result<Player, Error> {
        self.client
            .request(
                Method::PATCH,
                &format!("{}?replace={replace}", self.player_path(guild)?),
                Some(serde_json::to_value(update)?),
            )
            .await
    }
    pub async fn play(&self, guild: &str, token: &str) -> Result<Player, Error> {
        self.update_player(
            guild,
            &PlayerUpdate {
                track: Patch::Value(TrackUpdate {
                    token: Patch::Value(token.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        )
        .await
    }
    /// A URL or source ID must resolve to exactly one track. For search results
    /// and playlists, call `Client::resolve`, choose a track, then `play`.
    pub async fn play_query(&self, guild: &str, query: &str) -> Result<Player, Error> {
        self.update_player(
            guild,
            &PlayerUpdate {
                track: Patch::Value(TrackUpdate {
                    query: Some(query.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        )
        .await
    }
    pub async fn stop(&self, guild: &str) -> Result<Player, Error> {
        self.update_player(guild, &PlayerUpdate { track: Patch::Null, ..Default::default() }, true)
            .await
    }
    pub async fn pause(&self, guild: &str, paused: bool) -> Result<Player, Error> {
        self.update_player(
            guild,
            &PlayerUpdate { paused: Some(paused), ..Default::default() },
            true,
        )
        .await
    }
    pub async fn seek(&self, guild: &str, position_ms: i64) -> Result<Player, Error> {
        self.update_player(
            guild,
            &PlayerUpdate { position: Some(position_ms.into()), ..Default::default() },
            true,
        )
        .await
    }
    pub async fn set_volume(&self, guild: &str, volume: i64) -> Result<Player, Error> {
        self.update_player(
            guild,
            &PlayerUpdate { volume: Some(volume.into()), ..Default::default() },
            true,
        )
        .await
    }
    pub async fn set_voice(&self, guild: &str, voice: VoiceState) -> Result<Player, Error> {
        self.update_player(guild, &PlayerUpdate { voice: Some(voice), ..Default::default() }, true)
            .await
    }
    pub async fn destroy_player(&self, guild: &str) -> Result<(), Error> {
        self.client.request(Method::DELETE, &self.player_path(guild)?, None).await
    }

    /// Connect the single event subscriber. The first `next()` is
    /// `session.ready`, containing current players and the `resumed` flag.
    /// Call `next()` continuously so WebSocket pings are answered.
    pub async fn events(&self) -> Result<Events, Error> {
        let mut url = self.client.endpoint(&format!("{}/events", self.path()))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| Error::Invalid("invalid WebSocket scheme".into()))?;
        let mut request = url.as_str().into_client_request()?;
        let mut auth = format!("Bearer {}", self.client.password)
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|error| Error::Invalid(error.to_string()))?;
        auth.set_sensitive(true);
        request.headers_mut().insert("authorization", auth);
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        Ok(Events { socket })
    }
}

pub struct Events {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}
impl Events {
    pub async fn next(&mut self) -> Result<Option<Value>, Error> {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(60), self.socket.next())
                .await
                .map_err(|_| Error::Timeout)?;
            match message {
                Some(Ok(Message::Text(text))) => return Ok(Some(serde_json::from_str(&text)?)),
                // tungstenite queues the Pong automatically; flush it now.
                Some(Ok(Message::Ping(_))) => self.socket.flush().await?,
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }
    pub async fn close(&mut self) -> Result<(), Error> {
        self.socket.close(None).await?;
        Ok(())
    }
}
