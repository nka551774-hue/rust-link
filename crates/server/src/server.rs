//! Rustlink API v1: REST resources, Prometheus metrics, and per-session WebSocket event streams.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, OriginalUri, Path, Request, State};
use axum::http::header::{ALLOW, AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rustlink_config::{Config, ConfigError};
use rustlink_lyrics::{LyricsError, Manager};
use rustlink_protocol::{decode_track_owned, LoadResult, PlayerUpdate, Track};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::cpu::Cpu;
use crate::player::{Deps, Player};
use crate::ratelimit::Keyed;
use crate::session::Session;
use crate::webhook::{self, Client};
use crate::{now_millis, Counters, APIVERSION, VERSION};

const READ_TIMEOUT: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(25);
const LYRICS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DECODE_TRACKS: usize = 1000;
const SMALL_BODY_LIMIT: usize = 16 << 10;
const SINGLE_TOKEN_BODY_LIMIT: usize = 512 << 10;
const LARGE_BODY_LIMIT: usize = 1 << 20;
const MAX_KEYS: usize = 10_000;
const FAILED_LOGINS: (f64, usize) = (2.0, 10);

static SERVER_HEADER: LazyLock<HeaderValue> = LazyLock::new(|| {
    HeaderValue::from_str(&format!("Rustlink/{VERSION}"))
        .unwrap_or(HeaderValue::from_static("Rustlink"))
});

pub struct Dependencies {
    pub loader: Arc<dyn crate::playback::Loader>,
    pub engine: Arc<dyn crate::playback::Engine>,
    pub voice: Arc<dyn rustlink_voice::Factory>,
    pub lyrics: Option<Arc<Manager>>,
    pub webhook: Option<Client>,
}

#[repr(usize)]
#[derive(Clone, Copy)]
enum Route {
    Info = 0,
    Stats,
    Metrics,
    Websocket,
    ResolveTracks,
    InspectTrack,
    InspectTracks,
    Lyrics,
    PlayerLyrics,
    UpdateSession,
    GetPlayers,
    GetPlayer,
    UpdatePlayer,
    DeletePlayer,
    CreateSession,
    GetSession,
    DeleteSession,
    UpdateYoutube,
    Root,
}

impl Route {
    const COUNT: usize = 19;
    const BUDGETS: [(f64, usize); Self::COUNT] = [
        (5.0, 10),   // Info
        (2.0, 5),    // Stats
        (2.0, 4),    // Metrics
        (1.0, 3),    // Websocket
        (10.0, 20),  // ResolveTracks
        (20.0, 40),  // InspectTrack
        (5.0, 10),   // InspectTracks
        (5.0, 10),   // Lyrics
        (5.0, 10),   // PlayerLyrics
        (5.0, 10),   // UpdateSession
        (20.0, 40),  // GetPlayers
        (20.0, 40),  // GetPlayer
        (50.0, 100), // UpdatePlayer
        (20.0, 40),  // DeletePlayer
        (5.0, 10),   // CreateSession
        (20.0, 40),  // GetSession
        (10.0, 20),  // DeleteSession
        (2.0, 5),    // UpdateYoutube
        (2.0, 5),    // Root
    ];
}

#[repr(usize)]
#[derive(Clone, Copy)]
enum PublicRoute {
    Version = 0,
    Healthz,
}

impl PublicRoute {
    const COUNT: usize = 2;
    const BUDGETS: [(f64, usize); Self::COUNT] = [(2.0, 5), (5.0, 10)];
}

struct Limits {
    authenticated: [Keyed; Route::COUNT],
    public: [Keyed; PublicRoute::COUNT],
    failed_logins: Keyed,
}

impl Limits {
    fn new() -> Limits {
        Limits {
            authenticated: std::array::from_fn(|idx| {
                let (rate, burst) = Route::BUDGETS[idx];
                Keyed::new(rate, burst, MAX_KEYS)
            }),
            public: std::array::from_fn(|idx| {
                let (rate, burst) = PublicRoute::BUDGETS[idx];
                Keyed::new(rate, burst, MAX_KEYS)
            }),
            failed_logins: Keyed::new(FAILED_LOGINS.0, FAILED_LOGINS.1, MAX_KEYS),
        }
    }

    fn allow_authenticated(&self, route: Route, session: &str) -> bool {
        self.authenticated[route as usize].allow(session)
    }

    fn allow_public(&self, route: PublicRoute, remote: &str) -> bool {
        self.public[route as usize].allow(remote)
    }

    fn allow_failed_login(&self, remote: &str) -> bool {
        self.failed_logins.allow(remote)
    }
}

tokio::task_local! {
    static REMOTE: Option<std::net::IpAddr>;
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct RemoteKey {
    buf: [u8; 24],
    len: u8,
}

impl RemoteKey {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("unknown")
    }
}

impl std::fmt::Debug for RemoteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RemoteKey").field(&self.as_str()).finish()
    }
}

impl PartialEq<&str> for RemoteKey {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Write for RemoteKey {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let len = usize::from(self.len);
        let bytes = s.as_bytes();
        let next =
            len.checked_add(bytes.len()).filter(|&n| n <= self.buf.len()).ok_or(std::fmt::Error)?;
        self.buf[len..next].copy_from_slice(bytes);
        self.len = next as u8;
        Ok(())
    }
}

fn remote_key() -> RemoteKey {
    let mut key = RemoteKey { buf: [0u8; 24], len: 0 };
    match REMOTE.try_with(|remote| remote.map(|ip| ip.to_canonical())).ok().flatten() {
        Some(std::net::IpAddr::V4(addr)) => {
            let _ = write!(key, "{addr}");
        }
        Some(std::net::IpAddr::V6(addr)) => {
            let s = addr.segments();
            let _ = write!(key, "{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3]);
        }
        None => {
            let _ = key.write_str("unknown");
        }
    }
    key
}

fn session_key(path: &str) -> Cow<'_, str> {
    let Some(raw) = path
        .split_once("/sessions/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .filter(|id| !id.is_empty())
    else {
        return Cow::Borrowed("node");
    };
    if !raw.contains('%') {
        let trimmed = raw.char_indices().nth(64).map_or(raw, |(end, _)| &raw[..end]);
        return Cow::Borrowed(trimmed);
    }
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len().min(256));
    let mut index = 0;
    while index < bytes.len() && decoded.len() < 256 {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                ((bytes[index + 1] as char).to_digit(16), (bytes[index + 2] as char).to_digit(16))
            {
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    if decoded.is_empty() {
        return Cow::Borrowed("node");
    }
    match String::from_utf8(decoded) {
        Ok(mut text) => {
            if let Some((end, _)) = text.char_indices().nth(64) {
                text.truncate(end);
            }
            Cow::Owned(text)
        }
        Err(err) => Cow::Owned(String::from_utf8_lossy(err.as_bytes()).chars().take(64).collect()),
    }
}

fn skip_interval(period: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker
}

pub struct Server {
    config: Config,
    deps: Dependencies,
    handle: tokio::runtime::Handle,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    closed: AtomicBool,
    started: Instant,
    players: AtomicU64,
    requests: AtomicU64,
    counters: Arc<Counters>,
    player_deps: Arc<Deps>,
    load_gate: Arc<tokio::sync::Semaphore>,
    lyrics_gate: Arc<tokio::sync::Semaphore>,
    cpu: Arc<Cpu>,
    limits: Limits,
    reports: mpsc::Sender<()>,
    last_report: Mutex<Option<Instant>>,
    ws_active: AtomicUsize,
    ws_drained: tokio::sync::Notify,
    shutdown: tokio::sync::Notify,
}

impl Server {
    pub fn new(config: Config, deps: Dependencies) -> Result<Arc<Server>, ConfigError> {
        rustlink_config::validate(&config)?;
        let handle = tokio::runtime::Handle::current();
        let (reports, inbox) = mpsc::channel(1);
        let player_deps = Arc::new(Deps {
            loader: Arc::clone(&deps.loader),
            engine: Arc::clone(&deps.engine),
            voice: Arc::clone(&deps.voice),
        });
        let max_loads = (config.max_concurrent_loads as usize).max(1);
        let server = Arc::new(Server {
            config,
            deps,
            handle,
            sessions: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            started: Instant::now(),
            players: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            counters: Arc::new(Counters::default()),
            player_deps,
            load_gate: Arc::new(tokio::sync::Semaphore::new(max_loads)),
            lyrics_gate: Arc::new(tokio::sync::Semaphore::new(max_loads)),
            cpu: Arc::new(Cpu::new()),
            limits: Limits::new(),
            reports,
            last_report: Mutex::new(None),
            ws_active: AtomicUsize::new(0),
            ws_drained: tokio::sync::Notify::new(),
            shutdown: tokio::sync::Notify::new(),
        });
        server.start_background(inbox);
        Ok(server)
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let server = Arc::clone(self);
        Router::new()
            .route("/version", get(version))
            .route("/healthz", get(healthz))
            .route("/api/v1/info", get(info))
            .route("/api/v1/stats", get(stats))
            .route("/api/v1/metrics", get(metrics))
            .route("/metrics", get(metrics))
            .route("/api/v1/sessions/{session_id}/events", get(websocket))
            .route("/api/v1/tracks/resolve", post(load_tracks))
            .route("/api/v1/tracks/inspect", post(decode_track_route))
            .route("/api/v1/tracks/inspect-batch", post(decode_tracks))
            .route("/api/v1/youtube", axum::routing::patch(update_youtube))
            .route("/api/v1/lyrics", post(lyrics_for_track))
            .route(
                "/api/v1/sessions/{session_id}/players/{guild_id}/lyrics",
                get(lyrics_for_player),
            )
            .route("/api/v1/sessions", post(create_session))
            .route(
                "/api/v1/sessions/{session_id}",
                get(get_session).patch(update_session).delete(delete_session),
            )
            .route("/api/v1/sessions/{session_id}/players", get(get_players))
            .route(
                "/api/v1/sessions/{session_id}/players/{guild_id}",
                get(get_player).patch(update_player).delete(delete_player),
            )
            .fallback(not_found)
            .layer(axum::extract::DefaultBodyLimit::max(LARGE_BODY_LIMIT))
            .layer(axum::middleware::from_fn_with_state(Arc::clone(self), log_request))
            .layer(axum::middleware::map_response(stamp))
            .with_state(server)
    }

    pub async fn listen(
        self: Arc<Self>,
        address: &str,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        if let Ok(bound) = listener.local_addr() {
            if !bound.ip().is_loopback() {
                log::warn!(
                    "reachable from other machines on {bound}; limit the firewall to the bot"
                );
            }
        }
        let listener = listener.tap_io(configure_connection);
        log::info!("Rustlink listening address={address} version={VERSION}");
        let closer = Arc::clone(&self);
        let signal_closer = Arc::clone(&self);
        let routes = self.router().into_make_service_with_connect_info::<std::net::SocketAddr>();
        let result = axum::serve(listener, routes)
            .with_graceful_shutdown(async move {
                shutdown.await;
                signal_closer.close();
            })
            .await;
        closer.close();
        closer.wait_ws_drained(Duration::from_secs(2)).await;
        result
    }

    async fn wait_ws_drained(&self, limit: Duration) {
        let _ = tokio::time::timeout(limit, async {
            loop {
                let notified = self.ws_drained.notified();
                if self.ws_active.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await;
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown.notify_waiters();
        let sessions = std::mem::take(&mut *self.sessions.lock().unwrap());
        for (_, session) in sessions {
            session.destroy();
        }
    }

    async fn shutdown_requested(&self) {
        let notified = self.shutdown.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.closed.load(Ordering::SeqCst) {
            notified.await;
        }
    }

    fn start_background(self: &Arc<Self>, reports: mpsc::Receiver<()>) {
        let reaper = Arc::clone(self);
        self.handle.spawn(async move { reaper.reap().await });
        let sampler = Arc::clone(self);
        self.handle.spawn(async move { sampler.cpu_loop().await });
        if self.deps.webhook.is_some() {
            let reporter = Arc::clone(self);
            self.handle.spawn(async move { reporter.report_loop(reports).await });
        }
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn publish(&self, event: &Value) {
        if let Some(client) = &self.deps.webhook {
            client.send_ref(event);
        }
        self.request_report();
    }

    pub fn request_report(&self) {
        if self.deps.webhook.is_some() {
            let _ = self.reports.try_send(());
        }
    }
}

fn cas_update(slot: &AtomicU64, mut f: impl FnMut(u64) -> Option<u64>) -> bool {
    let mut cur = slot.load(Ordering::SeqCst);
    while let Some(next) = f(cur) {
        match slot.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
    false
}

fn configure_connection(stream: &mut tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    let _ = socket2::SockRef::from(&*stream).set_tcp_keepalive(&keepalive);
}

async fn stamp(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert("Rustlink-Api-Version", HeaderValue::from_static(APIVERSION));
    headers.insert("Server", SERVER_HEADER.clone());
    response
}

async fn log_request(
    State(server): State<Arc<Server>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    server.requests.fetch_add(1, Ordering::Relaxed);
    let method = request.method().clone();
    let uri = request.uri().clone();
    let remote = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    let start = Instant::now();
    let server_for_scope = Arc::clone(&server);
    let mut response = REMOTE
        .scope(remote, async move {
            let preflight = match request.uri().path() {
                "/version" => server_for_scope.guard_public(PublicRoute::Version, "/version"),
                "/healthz" => server_for_scope.guard_public(PublicRoute::Healthz, "/healthz"),
                path => server_for_scope.verify_auth(request.headers(), path),
            };
            match preflight {
                Ok(()) => next.run(request).await,
                Err(response) => response,
            }
        })
        .await;
    if (response.status().is_client_error() || response.status().is_server_error())
        && response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
            != Some("application/json")
    {
        let status = response.status();
        let allow = response.headers().get(ALLOW).cloned();
        response =
            server.fail(status, status.canonical_reason().unwrap_or("request failed"), uri.path());
        if let Some(allow) = allow {
            response.headers_mut().insert(ALLOW, allow);
        }
    }
    let status = response.status().as_u16();
    let millis = start.elapsed().as_millis();
    let path = uri.path();
    let remote = Remote(remote);
    if status >= 500 {
        log::error!("request {method} {path} {status} {millis}ms remote={remote}");
    } else if status >= 400 {
        log::warn!("request {method} {path} {status} {millis}ms remote={remote}");
    } else {
        log::debug!("request {method} {path} {status} {millis}ms remote={remote}");
    }
    response
}

struct Remote(Option<std::net::IpAddr>);

impl std::fmt::Display for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(addr) => write!(f, "{addr}"),
            None => Ok(()),
        }
    }
}

impl Server {
    #[allow(clippy::result_large_err)]
    fn verify_auth(&self, headers: &HeaderMap, path: &str) -> Result<(), Response> {
        let given = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map_or("", |(_, token)| token);
        if !same(given.as_bytes(), self.config.password.as_bytes()) {
            if self.config.ratelimits.enable
                && !self.limits.allow_failed_login(remote_key().as_str())
            {
                return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
            }
            return Err(self.fail(StatusCode::UNAUTHORIZED, "invalid authorization", path));
        }
        Ok(())
    }

    fn rate_limit_key<'a>(&self, route: Route, path: &'a str) -> Cow<'a, str> {
        if matches!(route, Route::Root) {
            return Cow::Borrowed("node");
        }
        let key = session_key(path);
        if key == "node" {
            return Cow::Borrowed("node");
        }
        if self.sessions.lock().unwrap().get(key.as_ref()).is_some_and(|s| s.is_usable()) {
            key
        } else {
            Cow::Borrowed("unknown_session")
        }
    }

    #[allow(clippy::result_large_err)]
    fn rate_limit(&self, route: Route, path: &str) -> Result<(), Response> {
        if self.config.ratelimits.enable
            && !self.limits.allow_authenticated(route, &self.rate_limit_key(route, path))
        {
            return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
        }
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    fn guard_public(&self, route: PublicRoute, path: &str) -> Result<(), Response> {
        if self.config.ratelimits.enable && !self.limits.allow_public(route, remote_key().as_str())
        {
            return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
        }
        Ok(())
    }

    fn fail(&self, status: StatusCode, message: &str, path: &str) -> Response {
        #[derive(Serialize)]
        struct ErrorFields<'a> {
            code: &'a str,
            message: &'a str,
            path: &'a str,
        }
        #[derive(Serialize)]
        struct ErrorEnvelope<'a> {
            error: ErrorFields<'a>,
            timestamp_ms: i64,
        }
        let code: Cow<'static, str> = match status {
            StatusCode::BAD_REQUEST => Cow::Borrowed("bad_request"),
            StatusCode::UNAUTHORIZED => Cow::Borrowed("unauthorized"),
            StatusCode::FORBIDDEN => Cow::Borrowed("forbidden"),
            StatusCode::NOT_FOUND => Cow::Borrowed("not_found"),
            StatusCode::METHOD_NOT_ALLOWED => Cow::Borrowed("method_not_allowed"),
            StatusCode::CONFLICT => Cow::Borrowed("conflict"),
            StatusCode::PAYLOAD_TOO_LARGE => Cow::Borrowed("payload_too_large"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Cow::Borrowed("unsupported_media_type"),
            StatusCode::UNPROCESSABLE_ENTITY => Cow::Borrowed("unprocessable_entity"),
            StatusCode::TOO_MANY_REQUESTS => Cow::Borrowed("too_many_requests"),
            StatusCode::INTERNAL_SERVER_ERROR => Cow::Borrowed("internal_server_error"),
            StatusCode::BAD_GATEWAY => Cow::Borrowed("bad_gateway"),
            StatusCode::SERVICE_UNAVAILABLE => Cow::Borrowed("service_unavailable"),
            _ => Cow::Owned(
                status
                    .canonical_reason()
                    .unwrap_or("unknown error")
                    .to_ascii_lowercase()
                    .replace(' ', "_"),
            ),
        };
        (
            status,
            Json(ErrorEnvelope {
                error: ErrorFields { code: &code, message, path },
                timestamp_ms: now_millis(),
            }),
        )
            .into_response()
    }

    fn json(&self, status: StatusCode, body: impl Serialize) -> Response {
        (status, Json(body)).into_response()
    }

    #[allow(clippy::result_large_err)]
    fn session(&self, id: &str, path: &str) -> Result<Arc<Session>, Response> {
        let found = self.sessions.lock().unwrap().get(id).cloned();
        match found {
            Some(session) if session.is_usable() => Ok(session),
            _ => Err(self.fail(StatusCode::NOT_FOUND, "session not found", path)),
        }
    }

    #[allow(clippy::result_large_err)]
    fn session_and_guild(
        &self,
        session_id: &str,
        guild_id: &str,
        path: &str,
    ) -> Result<Arc<Session>, Response> {
        let session = self.session(session_id, path)?;
        if !rustlink_voice::valid_id(guild_id) {
            return Err(self.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path));
        }
        Ok(session)
    }

    #[allow(clippy::result_large_err)]
    fn require_player(
        &self,
        route: Route,
        session_id: &str,
        guild_id: &str,
        path: &str,
    ) -> Result<Arc<Player>, Response> {
        self.rate_limit(route, path)?;
        let session = self.session_and_guild(session_id, guild_id, path)?;
        session
            .player(guild_id)
            .ok_or_else(|| self.fail(StatusCode::NOT_FOUND, "player not found", path))
    }

    fn active_sessions(&self) -> Vec<Arc<Session>> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        sessions.into_iter().filter(|s| s.is_usable()).collect()
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions.lock().unwrap().values().filter(|s| s.is_usable()).count()
    }

    pub fn playing_count(&self) -> usize {
        self.sessions.lock().unwrap().values().map(|s| s.playing_player_count()).sum()
    }

    fn session_and_playing_counts(&self) -> (usize, usize) {
        let mut sessions = 0usize;
        let mut playing = 0usize;
        for session in self.sessions.lock().unwrap().values() {
            if let Some(count) = session.usable_and_playing() {
                sessions += 1;
                playing += count;
            }
        }
        (sessions, playing)
    }

    async fn load(&self, identifier: &str) -> Arc<LoadResult> {
        let deadline = tokio::time::Instant::now() + self.config.load_timeout();
        let permit =
            match tokio::time::timeout_at(deadline, Arc::clone(&self.load_gate).acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Arc::new(LoadResult::from_error("server is shutting down")),
                Err(_) => {
                    return Arc::new(LoadResult::from_error("the source did not answer in time"))
                }
            };
        let loader = self.deps.loader.clone();
        let identifier = identifier.to_string();
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            loader.load_shared(&identifier)
        });
        match tokio::time::timeout_at(deadline, work).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(error))) => Arc::new(LoadResult::from_error(error)),
            Ok(Err(error)) => Arc::new(LoadResult::from_error(error)),
            Err(_) => Arc::new(LoadResult::from_error("the source did not answer in time")),
        }
    }

    async fn answer_lyrics(&self, track: Track, path: &str) -> Response {
        let Some(manager) = self.deps.lyrics.as_ref().filter(|m| m.enabled()).cloned() else {
            return StatusCode::NO_CONTENT.into_response();
        };
        let deadline = tokio::time::Instant::now() + LYRICS_TIMEOUT;
        let permit =
            match tokio::time::timeout_at(deadline, Arc::clone(&self.lyrics_gate).acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => {
                    return self.fail(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "server is shutting down",
                        path,
                    )
                }
                Err(_) => {
                    return self.fail(
                        StatusCode::BAD_GATEWAY,
                        "lyrics lookup failed: the provider did not answer in time",
                        path,
                    )
                }
            };
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            manager.fetch_shared(&track)
        });
        match tokio::time::timeout_at(deadline, work).await {
            Ok(Ok(Ok(lyrics))) => self.json(StatusCode::OK, lyrics.as_ref()),
            Ok(Ok(Err(LyricsError::NotFound))) => StatusCode::NO_CONTENT.into_response(),
            Ok(Ok(Err(error))) => {
                self.fail(StatusCode::BAD_GATEWAY, &format!("lyrics lookup failed: {error}"), path)
            }
            Ok(Err(error)) => self.fail(StatusCode::BAD_GATEWAY, &error.to_string(), path),
            Err(_) => self.fail(
                StatusCode::BAD_GATEWAY,
                "lyrics lookup failed: the provider did not answer in time",
                path,
            ),
        }
    }

    fn sources(&self) -> Vec<&'static str> {
        let mut sources = vec!["http"];
        if self.config.youtube_native {
            sources.push("youtube");
        }
        if self.config.soundcloud_native {
            sources.push("soundcloud");
        }
        if self.config.spotify_enabled() {
            sources.push("spotify");
        }
        sources.push("deezer");
        sources
    }
}

fn same(given: &[u8], expected: &[u8]) -> bool {
    let len_eq = (given.len() as u64).ct_eq(&(expected.len() as u64));
    let mut diff = 0u8;
    for (index, &exp) in expected.iter().enumerate() {
        diff |= given.get(index).copied().unwrap_or(0) ^ exp;
    }
    bool::from(len_eq & diff.ct_eq(&0))
}

type RouteResult = Result<Response, Response>;

async fn version() -> Response {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], VERSION).into_response()
}

async fn healthz(State(server): State<Arc<Server>>) -> Response {
    #[derive(Serialize)]
    struct Health {
        status: &'static str,
        name: &'static str,
        version: &'static str,
    }
    server.json(StatusCode::OK, Health { status: "ok", name: "Rustlink", version: VERSION })
}

#[allow(clippy::result_large_err)]
async fn not_found(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
) -> RouteResult {
    server.rate_limit(Route::Root, uri.path())?;
    Ok(server.fail(StatusCode::NOT_FOUND, "route not found", uri.path()))
}

#[allow(clippy::result_large_err)]
async fn info(State(server): State<Arc<Server>>) -> RouteResult {
    server.rate_limit(Route::Info, "/api/v1/info")?;
    let (_, _, cpu_available) = server.cpu.values();
    Ok(server.json(
        StatusCode::OK,
        json!({
            "name": "Rustlink",
            "version": VERSION,
            "api_version": APIVERSION,
            "sources": server.sources(),
            "filters": rustlink_audio::FILTER_NAMES,
            "capabilities": {
                "track_tokens": "rlt1",
                "session_resume": true,
                "lyrics": server.deps.lyrics.as_ref().is_some_and(|m| m.enabled()),
                "cpu_stats": cpu_available,
                "discord_dave": true,
            },
            "voice": {
                "gateway_version": rustlink_voice::GATEWAY_VERSION,
                "max_dave_protocol_version": rustlink_voice::MAX_DAVE_PROTOCOL_VERSION,
                "media_codecs": ["opus"],
                "transport_modes": [
                    rustlink_voice::Mode::AeadAes256GcmRtpSize.name(),
                    rustlink_voice::Mode::AeadXChaCha20Poly1305RtpSize.name(),
                ],
            },
            "limits": {
                "max_players": server.config.max_players,
                "max_sessions": server.config.max_sessions,
                "max_body_bytes": LARGE_BODY_LIMIT,
                "max_inspect_tokens": MAX_DECODE_TRACKS,
            },
        }),
    ))
}

#[allow(clippy::result_large_err)]
async fn stats(State(server): State<Arc<Server>>) -> RouteResult {
    server.rate_limit(Route::Stats, "/api/v1/stats")?;
    Ok(server.json(StatusCode::OK, server.stats_body()))
}

#[allow(clippy::result_large_err)]
async fn metrics(State(server): State<Arc<Server>>, OriginalUri(uri): OriginalUri) -> RouteResult {
    server.rate_limit(Route::Metrics, uri.path())?;
    Ok(([(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], server.metrics_body())
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveRequest {
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokensRequest {
    tokens: Vec<String>,
}

#[derive(Serialize)]
struct TracksResponse {
    tracks: Vec<Track>,
}

#[derive(Serialize)]
struct PlayersResponse {
    players: Vec<rustlink_protocol::Player>,
}

fn is_length_limit_error(error: &axum::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(err) = current {
        if err.to_string().contains("length limit") {
            return true;
        }
        current = err.source();
    }
    false
}

#[allow(clippy::result_large_err)]
async fn read_body(
    server: &Server,
    body: Body,
    limit: usize,
    path: &str,
) -> Result<Bytes, Response> {
    to_bytes(body, limit).await.map_err(|error| {
        if is_length_limit_error(&error) {
            server.fail(StatusCode::PAYLOAD_TOO_LARGE, "request body too large", path)
        } else {
            server.fail(StatusCode::BAD_REQUEST, "could not read request body", path)
        }
    })
}

#[allow(clippy::result_large_err)]
async fn read_body_json<T: serde::de::DeserializeOwned>(
    server: &Server,
    body: Body,
    limit: usize,
    path: &str,
) -> Result<T, Response> {
    let bytes = read_body(server, body, limit, path).await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        server.fail(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {error}"), path)
    })
}

#[allow(clippy::result_large_err)]
async fn read_track_body(server: &Server, body: Body, path: &str) -> Result<Track, Response> {
    let request: TokenRequest = read_body_json(server, body, SINGLE_TOKEN_BODY_LIMIT, path).await?;
    decode_track_owned(request.token)
        .map_err(|error| server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct YoutubeTokensRequest {
    #[serde(default, alias = "poToken")]
    po_token: String,
    #[serde(default, alias = "visitorData")]
    visitor_data: String,
}

#[allow(clippy::result_large_err)]
async fn update_youtube(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/youtube";
    server.rate_limit(Route::UpdateYoutube, path)?;
    let req: YoutubeTokensRequest = read_body_json(&server, body, SMALL_BODY_LIMIT, path).await?;
    let (po_token_configured, visitor_data_configured) = server
        .deps
        .loader
        .set_youtube_tokens(req.po_token, req.visitor_data)
        .map_err(|e| server.fail(StatusCode::BAD_REQUEST, &e.to_string(), path))?;
    log::info!(
        "youtube runtime credentials updated poToken={} visitorData={}",
        po_token_configured,
        visitor_data_configured
    );
    Ok(server.json(
        StatusCode::OK,
        json!({
            "po_token_configured": po_token_configured,
            "visitor_data_configured": visitor_data_configured,
        }),
    ))
}

#[allow(clippy::result_large_err)]
async fn load_tracks(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/tracks/resolve";
    server.rate_limit(Route::ResolveTracks, path)?;
    let request: ResolveRequest = read_body_json(&server, body, SMALL_BODY_LIMIT, path).await?;
    if request.query.trim().is_empty() || request.query.len() > 8192 {
        return Err(server.fail(StatusCode::BAD_REQUEST, "query must contain 1..8192 bytes", path));
    }
    let result = server.load(request.query.trim()).await;
    if let LoadResult::Error(error) = result.as_ref() {
        return Err(server.fail(StatusCode::BAD_GATEWAY, &error.message, path));
    }
    Ok(server.json(StatusCode::OK, result.as_ref()))
}

#[allow(clippy::result_large_err)]
async fn decode_track_route(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/tracks/inspect";
    server.rate_limit(Route::InspectTrack, path)?;
    Ok(server.json(StatusCode::OK, read_track_body(&server, body, path).await?))
}

#[allow(clippy::result_large_err)]
async fn decode_tracks(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/tracks/inspect-batch";
    server.rate_limit(Route::InspectTracks, path)?;
    let request: TokensRequest = read_body_json(&server, body, LARGE_BODY_LIMIT, path).await?;
    if request.tokens.len() > MAX_DECODE_TRACKS {
        return Err(server.fail(StatusCode::BAD_REQUEST, "at most 1000 tokens per request", path));
    }
    let tracks = request
        .tokens
        .into_iter()
        .map(decode_track_owned)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path))?;
    Ok(server.json(StatusCode::OK, TracksResponse { tracks }))
}

#[allow(clippy::result_large_err)]
async fn lyrics_for_track(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/lyrics";
    server.rate_limit(Route::Lyrics, path)?;
    let track = read_track_body(&server, body, path).await?;
    Ok(server.answer_lyrics(track, path).await)
}

#[allow(clippy::result_large_err)]
async fn lyrics_for_player(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    let path = uri.path();
    let player = server.require_player(Route::PlayerLyrics, &session_id, &guild_id, path)?;
    let track = player
        .track()
        .ok_or_else(|| server.fail(StatusCode::NOT_FOUND, "no track is playing", path))?;
    Ok(server.answer_lyrics(track, path).await)
}

#[allow(clippy::result_large_err)]
async fn get_players(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path(session_id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::GetPlayers, path)?;
    let session = server.session(&session_id, path)?;
    Ok(server.json(StatusCode::OK, PlayersResponse { players: session.snapshots() }))
}

#[allow(clippy::result_large_err)]
async fn get_player(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    let path = uri.path();
    let player = server.require_player(Route::GetPlayer, &session_id, &guild_id, path)?;
    Ok(server.json(StatusCode::OK, player.snapshot()))
}

struct PlayerCreation<'a> {
    session: &'a Session,
    server: &'a Server,
    player: &'a Arc<Player>,
    pending: bool,
}

impl Drop for PlayerCreation<'_> {
    fn drop(&mut self) {
        if self.pending {
            if let Some(player) = self.session.remove_if_same(self.player.guild(), self.player) {
                player.destroy();
                self.server.player_destroyed();
                self.server.request_report();
            }
        }
    }
}

#[allow(clippy::result_large_err)]
fn parse_no_replace(server: &Server, query: Option<&str>, path: &str) -> Result<bool, Response> {
    let mut replace_param = None;
    let mut has_unknown_param = false;
    if let Some(raw_query) = query {
        for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
            if key == "replace" {
                replace_param = Some(value);
            } else {
                has_unknown_param = true;
            }
        }
    }
    let no_replace = match replace_param.as_deref() {
        Some("true") => false,
        Some("false") => true,
        Some(_) => {
            return Err(server.fail(StatusCode::BAD_REQUEST, "replace must be true or false", path))
        }
        None => false,
    };
    if has_unknown_param {
        return Err(server.fail(
            StatusCode::BAD_REQUEST,
            "unknown query parameter; expected replace",
            path,
        ));
    }
    Ok(no_replace)
}

#[allow(clippy::result_large_err)]
async fn update_player(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
    body: Body,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::UpdatePlayer, path)?;
    let session = server.session_and_guild(&session_id, &guild_id, path)?;
    let no_replace = parse_no_replace(&server, uri.query(), path)?;
    let update: PlayerUpdate = read_body_json(&server, body, LARGE_BODY_LIMIT, path).await?;
    let prepared = crate::player::prepare(update)
        .map_err(|error| server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path))?;
    if let Some(filters) = &prepared.filters {
        let disabled = rustlink_audio::disabled_filters(filters, &server.config.filters);
        if !disabled.is_empty() {
            let message =
                format!("Following filters are disabled in the config: {}", disabled.join(", "));
            return Err(server.fail(StatusCode::BAD_REQUEST, &message, path));
        }
    }
    let gate = session.update_gate(&guild_id);
    let _serialized = gate.lock().await;
    let (player, created) = if let Some(existing) = session.player(&guild_id) {
        if session.is_closed() {
            return Err(server.fail(StatusCode::NOT_FOUND, "session is closed", path));
        }
        (existing, false)
    } else {
        if !server.reserve_player() {
            return Err(server.fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "player capacity reached",
                path,
            ));
        }
        match session.get_or_insert(&guild_id, || {
            Ok::<_, std::convert::Infallible>(Player::with_limits(
                session.user.clone(),
                guild_id.clone(),
                Arc::clone(&server.player_deps),
                session.emitter(),
                Arc::clone(&server.counters),
                server.config.load_timeout(),
                Arc::clone(&server.load_gate),
            ))
        }) {
            Ok(Some((player, true))) => {
                log::info!(
                    "player created guild={guild_id} session={} players={}",
                    session.id,
                    server.players.load(Ordering::SeqCst)
                );
                server.request_report();
                (player, true)
            }
            Ok(Some((player, false))) => {
                server.player_destroyed();
                (player, false)
            }
            Ok(None) => {
                server.player_destroyed();
                return Err(server.fail(StatusCode::NOT_FOUND, "session is closed", path));
            }
            Err(infallible) => match infallible {},
        }
    };
    let mut creation =
        PlayerCreation { session: &session, server: &server, player: &player, pending: created };
    match player.apply(prepared, no_replace).await {
        Ok(body) => {
            creation.pending = false;
            Ok(server.json(StatusCode::OK, body))
        }
        Err(crate::player::PlayerError::Destroyed) => {
            Err(server.fail(StatusCode::NOT_FOUND, "player is destroyed", path))
        }
        Err(error) => Err(server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path)),
    }
}

#[allow(clippy::result_large_err)]
async fn delete_player(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::DeletePlayer, path)?;
    let session = server.session_and_guild(&session_id, &guild_id, path)?;
    let gate = session.update_gate(&guild_id);
    let _serialized = gate.lock().await;
    if let Some(player) = session.remove(&guild_id) {
        player.destroy();
        server.player_destroyed();
        log::info!(
            "player destroyed guild={guild_id} session={} players={}",
            session.id,
            server.players.load(Ordering::SeqCst)
        );
        server.request_report();
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    bot_id: String,
    #[serde(default)]
    resume_timeout_secs: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateSession {
    #[serde(default)]
    resume: Option<bool>,
    #[serde(default)]
    resume_timeout_secs: Option<u32>,
}

fn valid_resume_timeout(secs: Option<u32>) -> bool {
    secs.is_none_or(|timeout| (1..=86_400).contains(&timeout))
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn take_expired(sessions: &mut HashMap<String, Arc<Session>>) -> Vec<Arc<Session>> {
    let mut expired = Vec::new();
    sessions.retain(|_, session| {
        if session.expired() {
            expired.push(Arc::clone(session));
            false
        } else {
            true
        }
    });
    expired
}

#[allow(clippy::result_large_err)]
async fn create_session(State(server): State<Arc<Server>>, body: Body) -> RouteResult {
    let path = "/api/v1/sessions";
    server.rate_limit(Route::CreateSession, path)?;
    let request: CreateSession = read_body_json(&server, body, SMALL_BODY_LIMIT, path).await?;
    if !rustlink_voice::valid_id(&request.bot_id)
        || !valid_resume_timeout(request.resume_timeout_secs)
    {
        return Err(server.fail(
            StatusCode::BAD_REQUEST,
            "valid bot_id and resume_timeout_secs of 1..86400 are required",
            path,
        ));
    }
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        return Err(server.fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not generate session ID",
            path,
        ));
    }
    let id = hex_encode_lower(&bytes);
    let mut expired_sessions = Vec::new();
    let response_body = {
        let mut sessions = server.sessions.lock().unwrap();
        if sessions.len() >= server.config.max_sessions as usize {
            expired_sessions = take_expired(&mut sessions);
        }
        if server.closed.load(Ordering::SeqCst)
            || sessions.len() >= server.config.max_sessions as usize
        {
            None
        } else {
            let session = Session::new(
                id.clone(),
                request.bot_id,
                Arc::downgrade(&server),
                server.config.event_buffer as usize,
            );
            session.configure(Some(true), request.resume_timeout_secs);
            let body = session.snapshot();
            sessions.insert(id.clone(), session);
            Some(body)
        }
    };
    for session in expired_sessions {
        session.destroy();
    }
    let Some(body) = response_body else {
        return Err(server.fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "session capacity reached or server shutting down",
            path,
        ));
    };
    let mut response = server.json(StatusCode::CREATED, body);
    response
        .headers_mut()
        .insert("location", HeaderValue::from_str(&format!("/api/v1/sessions/{id}")).unwrap());
    Ok(response)
}

#[allow(clippy::result_large_err)]
async fn get_session(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::GetSession, path)?;
    let session = server.session(&id, path)?;
    Ok(server.json(StatusCode::OK, session.snapshot()))
}

#[allow(clippy::result_large_err)]
async fn delete_session(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::DeleteSession, path)?;
    server.remove_session(&id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[allow(clippy::result_large_err)]
async fn update_session(
    State(server): State<Arc<Server>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    body: Body,
) -> RouteResult {
    let path = uri.path();
    server.rate_limit(Route::UpdateSession, path)?;
    let session = server.session(&id, path)?;
    let update: UpdateSession = read_body_json(&server, body, SMALL_BODY_LIMIT, path).await?;
    if !valid_resume_timeout(update.resume_timeout_secs) {
        return Err(server.fail(
            StatusCode::BAD_REQUEST,
            "resume_timeout_secs must be 1..86400",
            path,
        ));
    }
    session.configure(update.resume, update.resume_timeout_secs);
    let snapshot = session.snapshot();
    if session.expired() {
        server.remove_session(&id);
    }
    Ok(server.json(StatusCode::OK, snapshot))
}

async fn websocket(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let path = parts.uri.path().to_owned();
    if let Err(response) = server.rate_limit(Route::Websocket, &path) {
        return response;
    }
    let session = match server.session(&id, &path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => {
            return server.fail(
                StatusCode::BAD_REQUEST,
                "the request is not a websocket upgrade",
                &path,
            )
        }
    };
    let (inbox, resumed) = {
        let sessions = server.sessions.lock().unwrap();
        if !sessions.contains_key(&id) {
            return server.fail(StatusCode::NOT_FOUND, "session not found", &path);
        }
        let had_connected = match session.claim_subscriber() {
            Ok(Some(had_connected)) => had_connected,
            Ok(None) => return server.fail(StatusCode::NOT_FOUND, "session not found", &path),
            Err(()) => {
                return server.fail(
                    StatusCode::CONFLICT,
                    "session already has an event subscriber",
                    &path,
                )
            }
        };
        let Some(inbox) = session.take_inbox() else {
            session.rollback_connect(had_connected);
            return server.fail(
                StatusCode::CONFLICT,
                "previous subscriber is disconnecting",
                &path,
            );
        };
        (inbox, had_connected)
    };
    let pending = Arc::new(Mutex::new(Some(inbox)));
    let failed_pending = Arc::clone(&pending);
    let failed_session = Arc::clone(&session);
    upgrade
        .max_message_size(64 << 10)
        .max_frame_size(64 << 10)
        .on_failed_upgrade(move |_| {
            restore_failed_upgrade(&failed_session, &failed_pending, resumed);
        })
        .on_upgrade(move |socket| async move {
            let inbox = pending.lock().unwrap().take();
            if let Some(inbox) = inbox {
                server.serve(session, inbox, socket, resumed).await;
            }
        })
        .into_response()
}

fn restore_failed_upgrade(
    session: &Arc<Session>,
    pending: &Mutex<Option<mpsc::Receiver<Value>>>,
    had_connected: bool,
) {
    let inbox = pending.lock().unwrap().take();
    if let Some(inbox) = inbox {
        session.rollback_connect(had_connected);
        session.restore_inbox(inbox);
    }
}

impl Server {
    async fn serve(
        self: Arc<Self>,
        session: Arc<Session>,
        inbox: mpsc::Receiver<Value>,
        mut socket: WebSocket,
        resumed: bool,
    ) {
        struct ActiveStream<'a>(&'a Server);
        impl Drop for ActiveStream<'_> {
            fn drop(&mut self) {
                if self.0.ws_active.fetch_sub(1, Ordering::Release) == 1 {
                    self.0.ws_drained.notify_waiters();
                }
            }
        }
        self.ws_active.fetch_add(1, Ordering::Relaxed);
        let _active_stream = ActiveStream(&self);
        let ready = json!({
            "type": "session.ready",
            "timestamp_ms": now_millis(),
            "api_version": APIVERSION,
            "resumed": resumed,
            "session_id": session.id,
            "players": session.snapshots(),
        });
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(10),
                socket.send(Message::Text(ready.to_string().into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            session.restore_inbox(inbox);
            self.detach(&session);
            return;
        }
        let (sink, mut stream) = socket.split();
        let (control, commands) = mpsc::channel(8);
        self.request_report();
        let mut writer = tokio::spawn(write_loop(
            Arc::clone(&self),
            Arc::clone(&session),
            sink,
            inbox,
            commands,
        ));
        let completed = loop {
            tokio::select! {
                result = &mut writer => break Some(result),
                incoming = tokio::time::timeout(READ_TIMEOUT, stream.next()) => match incoming {
                    Ok(Some(Ok(Message::Ping(payload)))) => {
                        if control.send(Message::Pong(payload)).await.is_err() { break None; }
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => {}
                    Ok(Some(Ok(Message::Text(_) | Message::Binary(_)))) => {
                        let error = json!({
                            "type": "request.rejected",
                            "session_id": session.id,
                            "timestamp_ms": now_millis(),
                            "error": {
                                "code": "rest_required",
                                "message": "use REST to change a player or session",
                            },
                        });
                        if control.send(Message::Text(error.to_string().into())).await.is_err() {
                            break None;
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) => {
                        let _ = control.send(Message::Close(None)).await;
                        break None;
                    }
                    _ => break None,
                }
            }
        };
        drop(control);
        let result = match completed {
            Some(result) => result,
            None => writer.await,
        };
        if let Ok(inbox) = result {
            session.restore_inbox(inbox);
        }
        self.detach(&session);
    }

    fn detach(&self, session: &Arc<Session>) {
        let (closed, player_count, resuming, timeout) = session.detach_info();
        if closed {
            return;
        }
        if resuming && timeout > 0 {
            log::info!(
                "session disconnected, awaiting resume session={} timeoutSeconds={timeout} players={player_count}",
                session.id,
            );
            return;
        }
        log::info!("session closed session={} players={player_count}", session.id);
        self.remove_session(&session.id);
    }

    pub fn remove_session(&self, id: &str) {
        let session = self.sessions.lock().unwrap().remove(id);
        if let Some(session) = session {
            session.destroy();
        }
    }

    pub fn player_destroyed(&self) {
        let _ = cas_update(&self.players, |n| n.checked_sub(1));
    }

    fn try_increment_player(&self) -> bool {
        let limit = u64::from(self.config.max_players);
        cas_update(&self.players, |n| (n < limit).then_some(n + 1))
    }

    fn reserve_player(&self) -> bool {
        self.try_increment_player()
            || (self.reap_expired_sessions() > 0 && self.try_increment_player())
    }

    fn reap_expired_sessions(&self) -> usize {
        let expired = take_expired(&mut self.sessions.lock().unwrap());
        let count = expired.len();
        for session in expired {
            log::info!(
                "session expired session={} user={} resumable={} timeoutSeconds={} players={}",
                session.id,
                session.user,
                session.resuming(),
                session.timeout(),
                session.player_count()
            );
            session.destroy();
        }
        count
    }

    fn stats_body(&self) -> Value {
        let (session_count, playing) = self.session_and_playing_counts();
        let (system_load, process_load, _) = self.cpu.values();
        let (used, total) = self.cpu.memory();
        json!({
            "players": self.players.load(Ordering::SeqCst),
            "sessions": session_count,
            "playing_players": playing,
            "uptime_ms": self.started.elapsed().as_millis() as i64,
            "memory": { "process_bytes": used, "system_total_bytes": total },
            "cpu": {
                "cores": self.cpu.cores(),
                "system_load": system_load,
                "process_load": process_load,
            },
        })
    }

    fn metrics_body(&self) -> String {
        let (sessions, playing) = self.session_and_playing_counts();
        let (system_load, process_load, _) = self.cpu.values();
        let uptime = self.started.elapsed().as_secs_f64();
        let mut out = String::with_capacity(1664);
        for (name, kind, help, value) in [
            ("rustlink_players", "gauge", "Current players", self.players.load(Ordering::SeqCst)),
            ("rustlink_playing_players", "gauge", "Currently playing players", playing as u64),
            ("rustlink_sessions", "gauge", "Current sessions", sessions as u64),
            ("rustlink_cpu_cores", "gauge", "Logical CPU cores", self.cpu.cores() as u64),
            (
                "rustlink_http_requests_total",
                "counter",
                "HTTP requests",
                self.requests.load(Ordering::Relaxed),
            ),
            (
                "rustlink_audio_frames_total",
                "counter",
                "Opus frames sent",
                self.counters.sent.load(Ordering::Relaxed),
            ),
            (
                "rustlink_audio_deficit_total",
                "counter",
                "Missed frame deadlines",
                self.counters.deficit.load(Ordering::Relaxed),
            ),
            (
                "rustlink_audio_errors_total",
                "counter",
                "Playback failures",
                self.counters.errors.load(Ordering::Relaxed),
            ),
            (
                "rustlink_webhook_dropped_total",
                "counter",
                "Webhook events dropped",
                self.deps.webhook.as_ref().map_or(0, Client::dropped),
            ),
            (
                "rustlink_heap_bytes",
                "gauge",
                "Bytes this process holds; excludes media subprocesses",
                self.cpu.memory().0,
            ),
            (
                "rustlink_tasks",
                "gauge",
                "Background tasks",
                self.handle.metrics().num_alive_tasks() as u64,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}");
        }
        let _ = writeln!(
            out,
            "# HELP rustlink_cpu_system_load System CPU load fraction\n# TYPE rustlink_cpu_system_load gauge\nrustlink_cpu_system_load {system_load:.6}\n\
             # HELP rustlink_cpu_process_load Process CPU load fraction\n# TYPE rustlink_cpu_process_load gauge\nrustlink_cpu_process_load {process_load:.6}\n\
             # HELP rustlink_uptime_seconds Uptime in seconds\n# TYPE rustlink_uptime_seconds gauge\nrustlink_uptime_seconds {uptime:.3}"
        );
        out
    }

    async fn reap(self: Arc<Self>) {
        let mut ticker = skip_interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown_requested() => return,
                _ = ticker.tick() => {
                    self.reap_expired_sessions();
                }
            }
        }
    }

    async fn cpu_loop(self: Arc<Self>) {
        if !self.sample_cpu().await {
            return;
        }
        let mut ticker = skip_interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = self.shutdown_requested() => return,
                _ = ticker.tick() => {
                    if !self.sample_cpu().await {
                        return;
                    }
                }
            }
        }
    }

    async fn sample_cpu(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let cpu = Arc::clone(&self.cpu);
        tokio::select! {
            _ = self.shutdown_requested() => false,
            _ = tokio::task::spawn_blocking(move || cpu.sample()) => true,
        }
    }

    async fn report_loop(self: Arc<Self>, mut reports: mpsc::Receiver<()>) {
        self.request_report();
        loop {
            tokio::select! {
                _ = self.shutdown_requested() => return,
                request = reports.recv() => {
                    if request.is_none() {
                        return;
                    }
                    if let Some(wait) = self.report_cooldown_remaining() {
                        tokio::select! {
                            _ = self.shutdown_requested() => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        let _ = reports.try_recv();
                    }
                    self.report_player_state();
                }
            }
        }
    }

    fn report_cooldown_remaining(&self) -> Option<Duration> {
        let latency = self.config.webhook.state_latency_seconds;
        if self.deps.webhook.is_none() || latency < 1 {
            return None;
        }
        let window = Duration::from_secs(latency as u64);
        let last = *self.last_report.lock().unwrap();
        last.and_then(|at| window.checked_sub(at.elapsed())).filter(|wait| !wait.is_zero())
    }

    fn report_player_state(&self) {
        let latency = self.config.webhook.state_latency_seconds;
        if self.deps.webhook.is_none() || latency < 1 {
            return;
        }
        {
            let mut last = self.last_report.lock().unwrap();
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(latency as u64)) {
                return;
            }
            *last = Some(Instant::now());
        }
        let report = self.player_state_report();
        if let Some(client) = &self.deps.webhook {
            client.send_ref(&report);
        }
    }

    fn player_state_report(&self) -> Value {
        let sessions = self.active_sessions();
        let mut states = Vec::new();
        let mut playing = 0;
        for session in &sessions {
            playing += session.collect_reports(&mut states);
        }
        json!({
            "type": webhook::PLAYER_STATE_TYPE,
            "timestamp_ms": now_millis(),
            "sessions": sessions.len(),
            "players": states.len(),
            "playing_players": playing,
            "states": states,
        })
    }
}

async fn write_loop(
    server: Arc<Server>,
    session: Arc<Session>,
    mut sink: SplitSink<WebSocket, Message>,
    mut inbox: mpsc::Receiver<Value>,
    mut control: mpsc::Receiver<Message>,
) -> mpsc::Receiver<Value> {
    let mut updates =
        skip_interval(Duration::from_secs(server.config.player_update_seconds.max(1) as u64));
    let mut stats = skip_interval(Duration::from_secs(server.config.stats_seconds.max(1) as u64));
    let mut ping = skip_interval(PING_INTERVAL);
    let mut previous_sent = server.counters.sent.load(Ordering::Relaxed);
    let mut previous_deficit = server.counters.deficit.load(Ordering::Relaxed);
    if let Some(unsent) = session.take_unsent() {
        if send_frame(&mut sink, Message::Text(unsent.to_string().into())).await.is_err() {
            session.restore_unsent(unsent);
            return inbox;
        }
    }
    loop {
        tokio::select! {
            biased;
            _ = session.stopped() => {
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    while let Ok(event) = inbox.try_recv() {
                        if sink.feed(Message::Text(event.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    if sink.send(Message::Close(None)).await.is_ok() {
                        let _ = sink.close().await;
                    }
                })
                .await;
                break;
            }
            command = control.recv() => match command {
                Some(Message::Close(frame)) => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        let _ = sink.send(Message::Close(frame)).await;
                        let _ = sink.close().await;
                    })
                    .await;
                    break;
                }
                Some(command) => if send_frame(&mut sink, command).await.is_err() { break },
                None => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), sink.flush()).await;
                    break;
                }
            },
            _ = ping.tick() => {
                if send_frame(&mut sink, Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            event = inbox.recv() => match event {
                Some(event) => {
                    if send_frame(&mut sink, Message::Text(event.to_string().into())).await.is_err() {
                        session.restore_unsent(event);
                        break;
                    }
                }
                None => break,
            },
            _ = updates.tick() => {
                #[derive(Serialize)]
                struct PlayerUpdated<'a> {
                    #[serde(rename = "type")]
                    kind: &'static str,
                    session_id: &'a str,
                    timestamp_ms: i64,
                    guild_id: &'a str,
                    state: rustlink_protocol::PlayerState,
                }
                let players = session.players();
                if !players.is_empty() {
                    let batch = tokio::time::timeout(Duration::from_secs(10), async {
                        let mut fed = false;
                        for player in players {
                            let (guild_id, state) = player.state_snapshot();
                            let payload = PlayerUpdated {
                                kind: "player.updated",
                                session_id: &session.id,
                                timestamp_ms: now_millis(),
                                guild_id,
                                state,
                            };
                            let Ok(text) = serde_json::to_string(&payload) else { continue };
                            sink.feed(Message::Text(text.into())).await?;
                            fed = true;
                        }
                        if fed {
                            sink.flush().await?;
                        }
                        Ok::<(), axum::Error>(())
                    })
                    .await;
                    if !matches!(batch, Ok(Ok(()))) {
                        break;
                    }
                }
            }
            _ = stats.tick() => {
                let mut body = server.stats_body();
                body["type"] = json!("node.stats");
                body["session_id"] = json!(session.id);
                body["timestamp_ms"] = json!(now_millis());
                let sent = server.counters.sent.load(Ordering::Relaxed);
                let deficit = server.counters.deficit.load(Ordering::Relaxed);
                body["frames"] = json!({
                    "sent": sent.saturating_sub(previous_sent),
                    "deficit": deficit.saturating_sub(previous_deficit),
                });
                previous_sent = sent;
                previous_deficit = deficit;
                if send_frame(&mut sink, Message::Text(body.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    inbox
}

async fn send_frame(sink: &mut SplitSink<WebSocket, Message>, message: Message) -> Result<(), ()> {
    match tokio::time::timeout(Duration::from_secs(10), sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::Object;

    #[test]
    fn a_session_budget_is_keyed_by_the_session_in_the_path() {
        assert_eq!(session_key("/api/v1/sessions/abc123/players/42"), "abc123");
        assert_eq!(session_key("/api/v1/sessions/%61bc123/players/42"), "abc123");
        assert_eq!(session_key("/api/v1/sessions/abc123"), "abc123");
        assert_eq!(session_key("/api/v1/sessions"), "node");
        assert_eq!(session_key("/api/v1/stats"), "node");
        assert_eq!(session_key("/api/v1/sessions/"), "node");
        let long = format!("/api/v1/sessions/{}/players/1", "x".repeat(500));
        assert_eq!(session_key(&long).len(), 64);
        let wide = format!("/api/v1/sessions/{}é/players/1", "a".repeat(63));
        assert_eq!(session_key(&wide), format!("{}é", "a".repeat(63)));
    }

    #[test]
    fn the_caller_is_keyed_by_its_address_with_ipv6_reduced_to_its_prefix() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let key = |address: &str| {
            let address: std::net::IpAddr = address.parse().unwrap();
            rt.block_on(REMOTE.scope(Some(address), async { remote_key() }))
        };
        assert_eq!(key("203.0.113.9"), "203.0.113.9");
        assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), key("2001:db8:1:2::1"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(remote_key(), "unknown");
    }

    #[test]
    fn budgets_are_spent_per_session_and_failed_logins_have_their_own() {
        let limits = Limits::new();
        let burst = Route::BUDGETS[Route::Stats as usize].1;
        for _ in 0..burst {
            assert!(limits.allow_authenticated(Route::Stats, "session-a"));
        }
        assert!(!limits.allow_authenticated(Route::Stats, "session-a"));
        assert!(limits.allow_authenticated(Route::Stats, "session-b"));
        assert!(limits.allow_authenticated(Route::UpdatePlayer, "session-a"));
        for _ in 0..FAILED_LOGINS.1 {
            assert!(limits.allow_failed_login("198.51.100.1"));
        }
        assert!(!limits.allow_failed_login("198.51.100.1"));
        assert!(limits.allow_failed_login("198.51.100.2"));
        assert!(limits.allow_public(PublicRoute::Healthz, "198.51.100.1"));
    }

    fn pending_for(session: &Arc<Session>) -> Mutex<Option<mpsc::Receiver<Value>>> {
        Mutex::new(Some(session.take_inbox().expect("session inbox is available")))
    }

    #[test]
    fn failed_upgrade_restores_inbox_and_honours_resume_policy() {
        let session = Session::new("session-id".into(), "123".into(), std::sync::Weak::new(), 4);
        session.update(Some(true), Some(60));
        session.connect();
        let pending = pending_for(&session);
        session.emit(Object::new());

        restore_failed_upgrade(&session, &pending, true);
        assert!(!session.is_connected() && session.has_connected() && !session.expired());
        let mut inbox = session.take_inbox().expect("failed upgrade restored the inbox");
        assert!(inbox.try_recv().is_ok());
        restore_failed_upgrade(&session, &pending, false);
        assert!(session.take_inbox().is_none());

        let non_resumable =
            Session::new("session-id".into(), "123".into(), std::sync::Weak::new(), 4);
        non_resumable.update(Some(false), Some(60));
        non_resumable.connect();
        let pending = pending_for(&non_resumable);
        restore_failed_upgrade(&non_resumable, &pending, false);
        assert!(!non_resumable.is_connected() && !non_resumable.has_connected());
        assert!(non_resumable.expired() && non_resumable.take_inbox().is_some());
    }

    #[tokio::test]
    async fn fake_session_ids_and_not_found_routes_do_not_create_per_session_rate_keys() {
        struct StubMedia;
        impl crate::playback::Loader for StubMedia {
            fn load(&self, _: &str) -> Result<LoadResult, rustlink_source::SourceError> {
                Ok(LoadResult::Empty)
            }
            fn open(
                &self,
                _: &Track,
            ) -> Result<Box<dyn std::io::Read + Send>, rustlink_source::SourceError> {
                Ok(Box::new(std::io::empty()))
            }
        }
        impl crate::playback::Engine for StubMedia {
            fn start(
                &self,
                _: &Track,
                _: i64,
                _: i64,
                _: &Object,
            ) -> Result<Box<dyn rustlink_audio::Frames>, rustlink_audio::AudioError> {
                Err(rustlink_audio::AudioError::FfmpegMissing)
            }
        }
        impl rustlink_voice::Factory for StubMedia {
            fn connect(
                &self,
                _: String,
                _: String,
                _: rustlink_protocol::VoiceState,
                _: rustlink_voice::CloseHandler,
            ) -> futures_util::future::BoxFuture<
                'static,
                Result<Arc<dyn rustlink_voice::Connection>, rustlink_voice::VoiceError>,
            > {
                Box::pin(async { Err(rustlink_voice::VoiceError::NotReady) })
            }
        }
        let mut config = Config::default();
        config.password = "secret".into();
        let server = Server::new(
            config,
            Dependencies {
                loader: Arc::new(StubMedia),
                engine: Arc::new(StubMedia),
                voice: Arc::new(StubMedia),
                lyrics: None,
                webhook: None,
            },
        )
        .unwrap();
        assert_eq!(server.rate_limit_key(Route::Root, "/api/v1/sessions/fake-1/unknown"), "node");
        assert_eq!(
            server.rate_limit_key(Route::GetPlayer, "/api/v1/sessions/fake-1/players/1"),
            "unknown_session"
        );
        let active = Session::new("real-session".into(), "123".into(), Arc::downgrade(&server), 4);
        server.sessions.lock().unwrap().insert("real-session".into(), active);
        assert_eq!(
            server.rate_limit_key(Route::GetPlayer, "/api/v1/sessions/real-session/players/1"),
            "real-session"
        );
        server.close();
    }
}
