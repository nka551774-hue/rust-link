//! An async Rust client for Rustlink's own API. No audio libraries are linked
//! into the bot: the server resolves tracks and sends audio to Discord.
//!
//! Create a session, keep its event stream open, and forward the bot's Discord
//! voice state to [`Session::set_voice`]. Explicitly call [`Session::destroy`]
//! at shutdown. Dropping a session leaves it available for its reconnect grace.

use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, protocol::WebSocketConfig, Message,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

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
    api_base: Url,
    auth: HeaderValue,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("api_base", &self.api_base.as_str())
            .field("auth", &"<redacted>")
            .finish()
    }
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
        let api_base = base.join("api/v1/").map_err(|error| Error::Invalid(error.to_string()))?;
        let password = password.into();
        let mut auth = (!password.is_empty())
            .then(|| HeaderValue::from_str(&format!("Bearer {password}")).ok())
            .flatten()
            .ok_or_else(|| {
                Error::Invalid("password is empty or contains invalid header characters".into())
            })?;
        auth.set_sensitive(true);
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, api_base, auth })
    }

    fn endpoint(&self, path: &str) -> Result<Url, Error> {
        self.api_base.join(path).map_err(|error| Error::Invalid(error.to_string()))
    }

    async fn request<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, Error> {
        let mut request = self
            .http
            .request(method, self.endpoint(path)?)
            .header(AUTHORIZATION, self.auth.clone());
        if let Some(body) = body {
            request =
                request.header(CONTENT_TYPE, "application/json").body(serde_json::to_vec(body)?);
        }
        let response = request.send().await?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(serde_json::from_value(Value::Null)?);
        }
        let bytes = read_response_body(response).await?;
        if !status.is_success() {
            return Err(api_error(status, &bytes, "request failed"));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn info(&self) -> Result<Value, Error> {
        self.request::<_, ()>(Method::GET, "info", None).await
    }
    pub async fn stats(&self) -> Result<Value, Error> {
        self.request::<_, ()>(Method::GET, "stats", None).await
    }
    pub async fn resolve(&self, query: &str) -> Result<LoadResult, Error> {
        #[derive(Serialize)]
        struct Body<'a> {
            query: &'a str,
        }
        self.request(Method::POST, "tracks/resolve", Some(&Body { query })).await
    }
    pub async fn inspect(&self, token: &str) -> Result<Track, Error> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
        }
        self.request(Method::POST, "tracks/inspect", Some(&Body { token })).await
    }
    pub async fn lyrics(&self, token: &str) -> Result<Option<Value>, Error> {
        #[derive(Serialize)]
        struct Body<'a> {
            token: &'a str,
        }
        self.request(Method::POST, "lyrics", Some(&Body { token })).await
    }

    pub async fn create_session(
        &self,
        bot_id: &str,
        resume_timeout_secs: u32,
    ) -> Result<Session, Error> {
        #[derive(Serialize)]
        struct Body<'a> {
            bot_id: &'a str,
            resume_timeout_secs: u32,
        }
        validate_snowflake(bot_id, "bot ID")?;
        let info: SessionInfo = self
            .request(Method::POST, "sessions", Some(&Body { bot_id, resume_timeout_secs }))
            .await?;
        validate_session_id(&info.session_id)?;
        Ok(Session { client: self.clone(), id: info.session_id })
    }

    /// Recover a saved session handle. Opening `events()` within its grace
    /// period resumes it; a server restart or expired session returns HTTP 404.
    pub async fn session(&self, id: &str) -> Result<Session, Error> {
        validate_session_id(id)?;
        let info: SessionInfo =
            self.request::<_, ()>(Method::GET, &format!("sessions/{id}"), None).await?;
        validate_session_id(&info.session_id)?;
        if info.session_id != id {
            return Err(Error::Invalid("server returned a different session ID".into()));
        }
        Ok(Session { client: self.clone(), id: info.session_id })
    }
}

fn api_error(status: StatusCode, bytes: &[u8], fallback: &str) -> Error {
    let body: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    Error::Api {
        status: status.as_u16(),
        code: body["error"]["code"].as_str().unwrap_or("http_error").into(),
        message: body["error"]["message"]
            .as_str()
            .unwrap_or(status.canonical_reason().unwrap_or(fallback))
            .into(),
    }
}

async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BYTES as u64) {
        return Err(Error::Invalid("Rustlink response body exceeded the configured limit".into()));
    }
    let initial = response.content_length().map_or(0, |len| len.min(64 << 10) as usize);
    let mut body = Vec::with_capacity(initial);
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Error::Invalid(
                "Rustlink response body exceeded the configured limit".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_session_id(id: &str) -> Result<(), Error> {
    if id.len() != 48 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Invalid("session ID must contain 48 hexadecimal characters".into()));
    }
    Ok(())
}

fn validate_snowflake(id: &str, label: &str) -> Result<(), Error> {
    if id.is_empty()
        || id.len() > 20
        || id.starts_with('0')
        || !id.bytes().all(|byte| byte.is_ascii_digit())
        || !id.parse::<u64>().is_ok_and(|value| value > 0)
    {
        return Err(Error::Invalid(format!("{label} must be a nonzero Discord snowflake")));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub bot_id: String,
    pub connected: bool,
    pub resume: bool,
    pub resume_timeout_secs: u32,
    #[serde(default)]
    pub event_url: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    client: Client,
    id: String,
}

impl Session {
    #[inline]
    pub fn id(&self) -> &str {
        &self.id
    }
    fn path(&self) -> String {
        format!("sessions/{}", self.id)
    }
    fn player_path(&self, guild: &str) -> Result<String, Error> {
        validate_snowflake(guild, "guild ID")?;
        Ok(format!("sessions/{}/players/{guild}", self.id))
    }
    pub async fn info(&self) -> Result<SessionInfo, Error> {
        self.client.request::<_, ()>(Method::GET, &self.path(), None).await
    }
    pub async fn configure(
        &self,
        resume: bool,
        resume_timeout_secs: u32,
    ) -> Result<SessionInfo, Error> {
        #[derive(Serialize)]
        struct Body {
            resume: bool,
            resume_timeout_secs: u32,
        }
        self.client
            .request(Method::PATCH, &self.path(), Some(&Body { resume, resume_timeout_secs }))
            .await
    }
    pub async fn destroy(&self) -> Result<(), Error> {
        self.client.request::<_, ()>(Method::DELETE, &self.path(), None).await
    }
    pub async fn players(&self) -> Result<Vec<Player>, Error> {
        #[derive(Deserialize)]
        struct Players {
            players: Vec<Player>,
        }
        let body: Players = self
            .client
            .request::<_, ()>(Method::GET, &format!("sessions/{}/players", self.id), None)
            .await?;
        Ok(body.players)
    }
    pub async fn player(&self, guild: &str) -> Result<Player, Error> {
        self.client.request::<_, ()>(Method::GET, &self.player_path(guild)?, None).await
    }
    /// `replace = false` preserves an already loaded track.
    pub async fn update_player(
        &self,
        guild: &str,
        update: &PlayerUpdate,
        replace: bool,
    ) -> Result<Player, Error> {
        validate_snowflake(guild, "guild ID")?;
        self.client
            .request(
                Method::PATCH,
                &format!("sessions/{}/players/{guild}?replace={replace}", self.id),
                Some(update),
            )
            .await
    }
    pub async fn play(&self, guild: &str, token: &str) -> Result<Player, Error> {
        #[derive(Serialize)]
        struct TrackRef<'a> {
            token: &'a str,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            track: TrackRef<'a>,
        }
        validate_snowflake(guild, "guild ID")?;
        self.client
            .request(
                Method::PATCH,
                &format!("sessions/{}/players/{guild}?replace=true", self.id),
                Some(&Body { track: TrackRef { token } }),
            )
            .await
    }
    /// A URL or source ID must resolve to exactly one track. For search results
    /// and playlists, call `Client::resolve`, choose a track, then `play`.
    pub async fn play_query(&self, guild: &str, query: &str) -> Result<Player, Error> {
        #[derive(Serialize)]
        struct TrackRef<'a> {
            query: &'a str,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            track: TrackRef<'a>,
        }
        validate_snowflake(guild, "guild ID")?;
        self.client
            .request(
                Method::PATCH,
                &format!("sessions/{}/players/{guild}?replace=true", self.id),
                Some(&Body { track: TrackRef { query } }),
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
        self.client.request::<_, ()>(Method::DELETE, &self.player_path(guild)?, None).await
    }

    /// Connect the single event subscriber. The first `next()` is
    /// `session.ready`, containing current players and the `resumed` flag.
    /// Call `next()` continuously so WebSocket pings are answered.
    pub async fn events(&self) -> Result<Events, Error> {
        // The server's `event_url` is root-absolute, so it would drop a base
        // path such as a reverse-proxy prefix; the client builds its own.
        let mut url = self.client.endpoint(&format!("sessions/{}/events", self.id))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| Error::Invalid("invalid WebSocket scheme".into()))?;
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert(AUTHORIZATION, self.client.auth.clone());
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_RESPONSE_BYTES))
            .max_frame_size(Some(MAX_RESPONSE_BYTES));
        let connected = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async_with_config(request, Some(config), true),
        )
        .await
        .map_err(|_| Error::Timeout)?;
        let (socket, _) = match connected {
            Ok(pair) => pair,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let status = response.status();
                let bytes = response.body().as_deref().unwrap_or_default();
                return Err(api_error(status, bytes, "websocket upgrade failed"));
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Events { socket, needs_flush: false })
    }
}

pub struct Events {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    needs_flush: bool,
}
impl Events {
    pub async fn next(&mut self) -> Result<Option<Value>, Error> {
        loop {
            if self.needs_flush {
                tokio::time::timeout(Duration::from_secs(10), self.socket.flush())
                    .await
                    .map_err(|_| Error::Timeout)??;
                self.needs_flush = false;
            }
            let message = tokio::time::timeout(Duration::from_secs(60), self.socket.next())
                .await
                .map_err(|_| Error::Timeout)?;
            match message {
                Some(Ok(Message::Text(text))) => return Ok(Some(serde_json::from_str(&text)?)),
                // tungstenite queues the Pong automatically; record flush requirement so
                // dropping this future in `tokio::select!` never loses the Pong flush.
                Some(Ok(Message::Ping(_))) => {
                    self.needs_flush = true;
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }
    pub async fn close(&mut self) -> Result<(), Error> {
        tokio::time::timeout(Duration::from_secs(10), self.socket.close(None))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn session_ids_are_path_safe_and_fixed_length() {
        assert!(validate_session_id(&"a".repeat(48)).is_ok());
        assert!(validate_session_id(&"A".repeat(48)).is_ok());
        let short = "a".repeat(47);
        let long = "a".repeat(49);
        let mut separator = "a".repeat(47);
        separator.push('/');
        for value in ["", short.as_str(), long.as_str(), separator.as_str()] {
            assert!(validate_session_id(value).is_err(), "accepted session id {value:?}");
        }
    }

    #[tokio::test]
    async fn events_keep_the_base_path_whatever_event_url_the_server_returns() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::task::spawn_blocking(move || {
            let (mut http, _) = listener.accept().unwrap();
            http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = http.read(&mut buffer).unwrap();
                assert!(read > 0, "HTTP request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            let body = format!(
                "{{\"session_id\":\"{}\",\"bot_id\":\"123\",\"connected\":false,\"resume\":true,\"resume_timeout_secs\":60,\"event_url\":\"/custom-events\"}}",
                "a".repeat(48)
            );
            write!(
                http,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            http.flush().unwrap();

            let (socket, _) = listener.accept().unwrap();
            let mut path = None;
            // Tungstenite's external `Callback` trait fixes this closure's
            // error type to the full HTTP error response.
            #[allow(clippy::result_large_err)]
            let callback = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                            response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                path = Some(request.uri().path().to_owned());
                Ok(response)
            };
            let mut websocket =
                tokio_tungstenite::tungstenite::accept_hdr(socket, callback).unwrap();
            websocket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    r#"{"type":"session.ready"}"#.into(),
                ))
                .unwrap();
            path.unwrap()
        });

        let client = Client::new(&format!("http://{address}/base"), "test-secret").unwrap();
        let session = client.create_session("123", 60).await.unwrap();
        let mut events = session.events().await.unwrap();
        assert_eq!(events.next().await.unwrap().unwrap()["type"], "session.ready");
        assert_eq!(
            server.await.unwrap(),
            format!("/base/api/v1/sessions/{}/events", "a".repeat(48))
        );
    }
}
