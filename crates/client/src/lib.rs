//! Async Rust client for the Rustlink v1 API. No audio libraries are linked
//! into the bot: the server resolves tracks and sends audio to Discord.
//!
//! Create a session, keep its event stream open, and forward the bot's Discord
//! voice state to [`Session::set_voice`]. Explicitly call [`Session::destroy`]
//! at shutdown; dropping a session leaves it resumable for its grace period.

use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, protocol::WebSocketConfig, Error as WsError, Message,
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
    WebSocket(Box<WsError>),
    #[error("invalid JSON response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("event connection timed out")]
    Timeout,
}

impl From<WsError> for Error {
    fn from(error: WsError) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

/// Cloneable HTTP client. The password is excluded from `Debug`.
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

struct SecretBuf(String);

impl Drop for SecretBuf {
    fn drop(&mut self) {
        // SAFETY: overwriting initialized bytes with NUL preserves UTF-8 validity before `clear()`.
        unsafe {
            for byte in self.0.as_bytes_mut() {
                std::ptr::write_volatile(byte, 0);
            }
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        self.0.clear();
    }
}

/// Runtime YouTube Proof-of-Origin token bundle for `PATCH /api/v1/youtube`.
#[derive(Default, Clone, Serialize)]
pub struct YoutubeTokens<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub po_token: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub gvs_po_token: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub player_po_token: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub subs_po_token: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub visitor_data: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub player_client: &'a str,
}

#[derive(Serialize)]
struct QueryBody<'a> {
    query: &'a str,
}

#[derive(Serialize)]
struct TokenBody<'a> {
    token: &'a str,
}

#[derive(Serialize)]
struct TrackPatch<T> {
    track: T,
}

impl Client {
    /// `base` is the server origin, such as `http://127.0.0.1:2333`.
    pub fn new(base: &str, password: impl Into<String>) -> Result<Self, Error> {
        let password = SecretBuf(password.into());
        let mut base = Url::parse(base).map_err(|e| Error::Invalid(e.to_string()))?;
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
        let api_base = base.join("api/v1/").map_err(|e| Error::Invalid(e.to_string()))?;
        if password.0.is_empty() {
            return Err(Error::Invalid(
                "password is empty or contains invalid header characters".into(),
            ));
        }
        let bearer = SecretBuf(format!("Bearer {}", password.0));
        drop(password);
        let mut auth = HeaderValue::from_str(&bearer.0).map_err(|_| {
            Error::Invalid("password is empty or contains invalid header characters".into())
        })?;
        drop(bearer);
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
        self.api_base.join(path).map_err(|e| Error::Invalid(e.to_string()))
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response, Error> {
        let mut request = self
            .http
            .request(method, self.endpoint(path)?)
            .header(AUTHORIZATION, self.auth.clone());
        if let Some(body) = body {
            request =
                request.header(CONTENT_TYPE, "application/json").body(serde_json::to_vec(body)?);
        }
        Ok(request.send().await?)
    }

    async fn request_optional<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<T>, Error> {
        let response = self.send(method, path, body).await?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let bytes = read_response_body(response).await?;
        if !status.is_success() {
            return Err(api_error(status, &bytes, "request failed"));
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    async fn request<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, Error> {
        self.request_optional(method, path, body)
            .await?
            .ok_or_else(|| Error::Invalid("unexpected HTTP 204 response with no body".into()))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.request::<T, ()>(Method::GET, path, None).await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T, Error> {
        self.request(Method::POST, path, Some(body)).await
    }

    async fn patch_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T, Error> {
        self.request(Method::PATCH, path, Some(body)).await
    }

    async fn delete_empty(&self, path: &str) -> Result<(), Error> {
        let response = self.send::<()>(Method::DELETE, path, None).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = read_response_body(response).await?;
        Err(api_error(status, &bytes, "request failed"))
    }

    pub async fn info(&self) -> Result<Value, Error> {
        self.get_json("info").await
    }

    pub async fn stats(&self) -> Result<Value, Error> {
        self.get_json("stats").await
    }

    pub async fn resolve(&self, query: &str) -> Result<LoadResult, Error> {
        if query.trim().is_empty() || query.len() > 8192 {
            return Err(Error::Invalid("query must contain 1..8192 bytes".into()));
        }
        self.post_json("tracks/resolve", &QueryBody { query }).await
    }

    pub async fn inspect(&self, token: &str) -> Result<Track, Error> {
        self.post_json("tracks/inspect", &TokenBody { token }).await
    }

    pub async fn inspect_batch(&self, tokens: &[impl AsRef<str>]) -> Result<Vec<Track>, Error> {
        if tokens.len() > 1000 {
            return Err(Error::Invalid("at most 1000 tokens per request".into()));
        }
        #[derive(Serialize)]
        struct Body<'a> {
            tokens: Vec<&'a str>,
        }
        #[derive(Deserialize)]
        struct Tracks {
            tracks: Vec<Track>,
        }
        let tokens = tokens.iter().map(AsRef::as_ref).collect();
        Ok(self.post_json::<Tracks>("tracks/inspect-batch", &Body { tokens }).await?.tracks)
    }

    pub async fn lyrics(&self, token: &str) -> Result<Option<Value>, Error> {
        self.request_optional(Method::POST, "lyrics", Some(&TokenBody { token })).await
    }

    /// Hot-reloads YouTube `poToken` and `visitorData` on the node without restarting it.
    pub async fn set_youtube_tokens(
        &self,
        po_token: &str,
        visitor_data: &str,
    ) -> Result<Value, Error> {
        self.set_youtube_token_bundle(&YoutubeTokens {
            po_token,
            visitor_data,
            ..YoutubeTokens::default()
        })
        .await
    }

    /// Hot-reloads a full YTDLnis `GVS` / `Player` / `Subs` / `Visitor Data` bundle on the node.
    pub async fn set_youtube_token_bundle(
        &self,
        tokens: &YoutubeTokens<'_>,
    ) -> Result<Value, Error> {
        self.patch_json("youtube", tokens).await
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
        validate_resume_timeout(resume_timeout_secs)?;
        let info: SessionInfo =
            self.post_json("sessions", &Body { bot_id, resume_timeout_secs }).await?;
        validate_session_id(&info.session_id)?;
        Ok(Session { client: self.clone(), id: info.session_id })
    }

    /// Recover a saved session handle. Opening `events()` within its grace
    /// period resumes it; a server restart or expired session returns HTTP 404.
    pub async fn session(&self, id: &str) -> Result<Session, Error> {
        validate_session_id(id)?;
        let info: SessionInfo = self.get_json(&format!("sessions/{id}")).await?;
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
    if response.content_length().is_some_and(|len| len > MAX_RESPONSE_BYTES as u64) {
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
    if id.len() != 48 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("session ID must contain 48 hexadecimal characters".into()));
    }
    Ok(())
}

fn validate_resume_timeout(seconds: u32) -> Result<(), Error> {
    if !(1..=86_400).contains(&seconds) {
        return Err(Error::Invalid("resume_timeout_secs must be 1..86400".into()));
    }
    Ok(())
}

fn validate_snowflake(id: &str, label: &str) -> Result<(), Error> {
    if id.starts_with('0') || !id.bytes().all(|b| b.is_ascii_digit()) || id.parse::<u64>().is_err()
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
        self.client.get_json(&self.path()).await
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
        validate_resume_timeout(resume_timeout_secs)?;
        self.client.patch_json(&self.path(), &Body { resume, resume_timeout_secs }).await
    }

    pub async fn destroy(&self) -> Result<(), Error> {
        self.client.delete_empty(&self.path()).await
    }

    pub async fn players(&self) -> Result<Vec<Player>, Error> {
        #[derive(Deserialize)]
        struct Players {
            players: Vec<Player>,
        }
        Ok(self.client.get_json::<Players>(&format!("sessions/{}/players", self.id)).await?.players)
    }

    pub async fn player(&self, guild: &str) -> Result<Player, Error> {
        self.client.get_json(&self.player_path(guild)?).await
    }

    /// `replace = false` preserves an already loaded track.
    pub async fn update_player(
        &self,
        guild: &str,
        update: &PlayerUpdate,
        replace: bool,
    ) -> Result<Player, Error> {
        let path = format!("{}?replace={replace}", self.player_path(guild)?);
        self.client.patch_json(&path, update).await
    }

    async fn play_spec(&self, guild: &str, track: impl Serialize) -> Result<Player, Error> {
        let path = format!("{}?replace=true", self.player_path(guild)?);
        self.client.patch_json(&path, &TrackPatch { track }).await
    }

    pub async fn play(&self, guild: &str, token: &str) -> Result<Player, Error> {
        self.play_spec(guild, TokenBody { token }).await
    }

    /// A URL or source ID must resolve to exactly one track. For search results
    /// and playlists, call `Client::resolve`, choose a track, then `play`.
    pub async fn play_query(&self, guild: &str, query: &str) -> Result<Player, Error> {
        self.play_spec(guild, QueryBody { query }).await
    }

    async fn patch_player(&self, guild: &str, update: &PlayerUpdate) -> Result<Player, Error> {
        self.client.patch_json(&self.player_path(guild)?, update).await
    }

    pub async fn stop(&self, guild: &str) -> Result<Player, Error> {
        self.update_player(guild, &PlayerUpdate { track: Patch::Null, ..Default::default() }, true)
            .await
    }

    pub async fn pause(&self, guild: &str, paused: bool) -> Result<Player, Error> {
        self.patch_player(guild, &PlayerUpdate { paused: Some(paused), ..Default::default() }).await
    }

    pub async fn seek(&self, guild: &str, position_ms: i64) -> Result<Player, Error> {
        if position_ms < 0 {
            return Err(Error::Invalid("position_ms must be nonnegative".into()));
        }
        self.patch_player(
            guild,
            &PlayerUpdate { position: Some(position_ms.into()), ..Default::default() },
        )
        .await
    }

    pub async fn set_volume(&self, guild: &str, volume: i64) -> Result<Player, Error> {
        if !(0..=1000).contains(&volume) {
            return Err(Error::Invalid("volume must be 0..1000".into()));
        }
        self.patch_player(
            guild,
            &PlayerUpdate { volume: Some(volume.into()), ..Default::default() },
        )
        .await
    }

    pub async fn set_voice(&self, guild: &str, voice: VoiceState) -> Result<Player, Error> {
        self.patch_player(guild, &PlayerUpdate { voice: Some(voice), ..Default::default() }).await
    }

    pub async fn lyrics(&self, guild: &str) -> Result<Option<Value>, Error> {
        let path = format!("{}/lyrics", self.player_path(guild)?);
        self.client.request_optional::<_, ()>(Method::GET, &path, None).await
    }

    pub async fn destroy_player(&self, guild: &str) -> Result<(), Error> {
        self.client.delete_empty(&self.player_path(guild)?).await
    }

    /// Connect the single event subscriber. The first `next()` is
    /// `session.ready`, containing current players and the `resumed` flag.
    /// Call `next()` continuously so WebSocket pings are answered.
    pub async fn events(&self) -> Result<Events, Error> {
        // Build from `api_base` rather than `event_url` so reverse-proxy prefixes survive.
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
            Err(WsError::Http(response)) => {
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
                match tokio::time::timeout(Duration::from_secs(10), self.socket.flush()).await {
                    Ok(Ok(())) => self.needs_flush = false,
                    Ok(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                        self.needs_flush = false;
                        return Ok(None);
                    }
                    Ok(Err(WsError::Io(ref io_err)))
                        if matches!(
                            io_err.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::NotConnected
                        ) =>
                    {
                        self.needs_flush = false;
                    }
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => return Err(Error::Timeout),
                }
            }
            let message = tokio::time::timeout(Duration::from_secs(60), self.socket.next())
                .await
                .map_err(|_| Error::Timeout)?;
            match message {
                Some(Ok(Message::Text(text))) => return Ok(Some(serde_json::from_str(&text)?)),
                // Record Pong flush requirement so cancellation in `tokio::select!` cannot drop it.
                Some(Ok(Message::Ping(_))) => self.needs_flush = true,
                Some(Ok(Message::Close(_))) => {
                    self.needs_flush = false;
                    let _ = tokio::time::timeout(Duration::from_secs(2), self.socket.flush()).await;
                    return Ok(None);
                }
                None | Some(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => {
                    self.needs_flush = false;
                    return Ok(None);
                }
                Some(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        match tokio::time::timeout(Duration::from_secs(10), self.socket.close(None)).await {
            Ok(Ok(())) | Ok(Err(WsError::ConnectionClosed | WsError::AlreadyClosed)) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(Error::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn accept_json_request(listener: &TcpListener, status: &str, body: &str) -> String {
        let (mut http, _) = listener.accept().unwrap();
        http.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = http.read(&mut buffer).unwrap();
            assert!(read > 0, "HTTP request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
        }
        write_json_reply(&mut http, status, body);
        String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap().to_owned()
    }

    fn write_json_reply(http: &mut TcpStream, status: &str, body: &str) {
        write!(
            http,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        http.flush().unwrap();
    }

    #[test]
    fn session_ids_are_path_safe_and_fixed_length() {
        assert!(validate_session_id(&"a".repeat(48)).is_ok());
        assert!(validate_session_id(&"A".repeat(48)).is_ok());
        let short = "a".repeat(47);
        let long = "a".repeat(49);
        let separator = format!("{}/", "a".repeat(47));
        for value in ["", short.as_str(), long.as_str(), separator.as_str()] {
            assert!(validate_session_id(value).is_err(), "accepted session id {value:?}");
        }
    }

    #[tokio::test]
    async fn events_keep_the_base_path_whatever_event_url_the_server_returns() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::task::spawn_blocking(move || {
            let body = format!(
                "{{\"session_id\":\"{}\",\"bot_id\":\"123\",\"connected\":false,\"resume\":true,\"resume_timeout_secs\":60,\"event_url\":\"/custom-events\"}}",
                "a".repeat(48)
            );
            accept_json_request(&listener, "201 Created", &body);

            let (socket, _) = listener.accept().unwrap();
            let mut path = None;
            #[allow(clippy::result_large_err)]
            let callback = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                            response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                path = Some(request.uri().path().to_owned());
                Ok(response)
            };
            let mut ws = tokio_tungstenite::tungstenite::accept_hdr(socket, callback).unwrap();
            ws.send(Message::Text(r#"{"type":"session.ready"}"#.into())).unwrap();
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

    #[tokio::test]
    async fn client_validates_inputs_and_omits_replace_query_for_non_track_patches() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = Client::new(&format!("http://{address}"), "test-secret").unwrap();
        assert!(matches!(client.resolve("   ").await, Err(Error::Invalid(_))));
        assert!(matches!(client.resolve(&"x".repeat(8193)).await, Err(Error::Invalid(_))));

        let session = Session { client: client.clone(), id: "b".repeat(48) };
        assert!(matches!(session.seek("456", -1).await, Err(Error::Invalid(_))));
        assert!(matches!(session.set_volume("456", -1).await, Err(Error::Invalid(_))));
        assert!(matches!(session.set_volume("456", 1001).await, Err(Error::Invalid(_))));
        assert!(matches!(session.set_volume("+456", 80).await, Err(Error::Invalid(_))));

        let server = tokio::task::spawn_blocking(move || {
            let body = r#"{"guild_id":"456","track":null,"volume":80,"paused":false,"state":{"timestamp_ms":0,"position_ms":0,"connected":false,"ping_ms":-1},"voice":{"token":"","endpoint":"","session_id":"","channel_id":null},"filters":{}}"#;
            accept_json_request(&listener, "200 OK", body)
        });
        let player = session.set_volume("456", 80).await.unwrap();
        assert_eq!(player.volume, 80);
        assert_eq!(
            server.await.unwrap(),
            format!("/api/v1/sessions/{}/players/456", "b".repeat(48))
        );
    }

    #[tokio::test]
    async fn events_next_clears_needs_flush_on_close_and_maps_closed_socket_to_none() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::task::spawn_blocking(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut ws = tokio_tungstenite::tungstenite::accept(socket).unwrap();
            ws.send(Message::Ping(Vec::new().into())).unwrap();
            ws.send(Message::Close(None)).unwrap();
            let _ = ws.flush();
            while let Ok(msg) = ws.read() {
                if msg.is_close() {
                    break;
                }
            }
        });
        let (socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}")).await.unwrap();
        let mut events = Events { socket, needs_flush: false };
        assert_eq!(events.next().await.unwrap(), None);
        assert!(!events.needs_flush);
        assert_eq!(events.next().await.unwrap(), None);
        server.await.unwrap();
    }
}
