//! Rustlink API v1: REST resources and a per-session event stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, OriginalUri, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::{Json, Router};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rustlink_config::{Config, ConfigError};
use rustlink_lyrics::{LyricsError, Manager};
use rustlink_protocol::{decode_track, LoadResult, PlayerUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::cpu::Cpu;
use crate::player::{Deps, Player};
use crate::ratelimit::Limiter;
use crate::session::Session;
use crate::webhook::{self, Client};
use crate::{Counters, APIVERSION, VERSION};

/// How long a client may go quiet before its socket is closed. The client is
/// sent a ping well inside this, so silence means the connection is gone.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a ping is sent to a client.
const PING_INTERVAL: Duration = Duration::from_secs(25);

/// How long the node waits for a lyrics provider before giving up on it.
const LYRICS_TIMEOUT: Duration = Duration::from_secs(10);

/// The number of tracks one `tracks/inspect-batch` request may carry.
const MAX_DECODE_TRACKS: usize = 1000;

/// The response header identifying this service and its software version.
static SERVER_HEADER: LazyLock<HeaderValue> = LazyLock::new(|| {
    HeaderValue::from_str(&format!("Rustlink/{VERSION}"))
        .unwrap_or(HeaderValue::from_static("Rustlink"))
});

/// What the node needs to run. A node with no lyrics provider or no webhook is
/// still a node: those routes answer with no content and no event is posted.
pub struct Dependencies {
    pub loader: Arc<dyn crate::playback::Loader>,
    pub engine: Arc<dyn crate::playback::Engine>,
    pub voice: Arc<dyn rustlink_voice::Factory>,
    pub lyrics: Option<Arc<Manager>>,
    pub webhook: Option<Client>,
}

/// One token bucket per route, so a client that hammers one route does not
/// spend the allowance of another.
struct Limits {
    buckets: HashMap<&'static str, Limiter>,
}

/// Per-route request budgets, shared across authenticated clients.
const ROUTE_LIMITS: &[(&str, f64, usize)] = &[
    ("version", 2.0, 5),
    ("healthz", 5.0, 10),
    ("info", 2.0, 5),
    ("stats", 1.0, 3),
    ("metrics", 1.0, 2),
    ("websocket", 1.0, 3),
    ("resolve_tracks", 2.0, 4),
    ("inspect_track", 5.0, 10),
    ("inspect_tracks", 1.0, 2),
    ("lyrics", 2.0, 4),
    ("player_lyrics", 2.0, 4),
    ("update_session", 5.0, 10),
    ("get_players", 10.0, 15),
    ("get_player", 10.0, 15),
    ("update_player", 5.0, 10),
    ("delete_player", 3.0, 5),
    ("create_session", 2.0, 5),
    ("get_session", 10.0, 15),
    ("delete_session", 3.0, 5),
    ("root", 2.0, 5),
];

impl Limits {
    fn new() -> Limits {
        let mut buckets = HashMap::new();
        for &(name, rate, burst) in ROUTE_LIMITS {
            buckets.insert(name, Limiter::new(rate, burst));
        }
        Limits { buckets }
    }

    /// Spends a token for a route, or reports that the route is throttled.
    fn allow(&self, route: &str) -> bool {
        self.buckets.get(route).map(|limit| limit.allow()).unwrap_or(true)
    }
}

pub struct Server {
    config: Config,
    deps: Dependencies,
    /// The runtime the node runs on, kept so a thread that is not a task can
    /// still start one.
    handle: tokio::runtime::Handle,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    closed: AtomicBool,
    started: Instant,
    players: AtomicU64,
    requests: AtomicU64,
    counters: Arc<Counters>,
    /// What every player on the node is built with.
    player_deps: Arc<Deps>,
    cpu: Cpu,
    limits: Limits,
    /// Carries one pending request for a node-wide player report. A burst of
    /// activity asks for the same one report.
    reports: mpsc::Sender<()>,
    last_report: Mutex<Option<Instant>>,
    /// Wakes the background tasks when the node is shutting down.
    shutdown: tokio::sync::Notify,
}

impl Server {
    /// Builds the node. It has to be called from inside a Tokio runtime,
    /// because the node keeps the handle to start tasks from other threads.
    pub fn new(config: Config, deps: Dependencies) -> Result<Arc<Server>, ConfigError> {
        rustlink_config::validate(&config)?;
        let handle = tokio::runtime::Handle::current();
        let (reports, inbox) = mpsc::channel(1);
        let player_deps = Arc::new(Deps {
            loader: Arc::clone(&deps.loader),
            engine: Arc::clone(&deps.engine),
            voice: Arc::clone(&deps.voice),
        });
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
            cpu: Cpu::new(),
            limits: Limits::new(),
            reports,
            last_report: Mutex::new(None),
            shutdown: tokio::sync::Notify::new(),
        });
        server.start_background(inbox);
        Ok(server)
    }

    /// The router a caller serves.
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
            .route("/api/v1/tracks/resolve", axum::routing::post(load_tracks))
            .route("/api/v1/tracks/inspect", axum::routing::post(decode_track_route))
            .route("/api/v1/tracks/inspect-batch", axum::routing::post(decode_tracks))
            .route("/api/v1/lyrics", axum::routing::post(lyrics_for_track))
            .route(
                "/api/v1/sessions/{session_id}/players/{guild_id}/lyrics",
                get(lyrics_for_player),
            )
            .route("/api/v1/sessions", axum::routing::post(create_session))
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
            .layer(axum::extract::DefaultBodyLimit::max(1 << 20))
            .layer(axum::middleware::from_fn_with_state(Arc::clone(self), log_request))
            .layer(axum::middleware::map_response(stamp))
            .with_state(server)
    }

    /// Serves the node's routes on `address` until `shutdown` resolves.
    ///
    /// Binding is the last step that fails for a reason an operator has to fix,
    /// so the error is handed back rather than logged: a node that cannot
    /// listen did not start.
    pub async fn listen(
        self: Arc<Self>,
        address: &str,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        // A node that is not on the loopback address is reachable from
        // wherever the machine is, and the password is all that stands between
        // it and a stranger, so an operator who has not thought about the
        // firewall is told once, at startup. The bound address is used rather
        // than the configured one, so a name that resolves to several
        // addresses reports the one that is actually being served.
        if let Ok(bound) = listener.local_addr() {
            if !bound.ip().is_loopback() {
                log::warn!(
                    "reachable from other machines on {bound}; limit the firewall to the bot"
                );
            }
        }
        // Every connection that arrives is a client that is watching this
        // node, so each one is set up for a conversation rather than for bulk
        // throughput.
        let listener = listener.tap_io(|stream| configure_connection(stream));
        log::info!("Rustlink listening address={address} version={VERSION}");
        // The request log names the client, so the peer address has to travel
        // with the request.
        let routes = self.router().into_make_service_with_connect_info::<std::net::SocketAddr>();
        axum::serve(listener, routes).with_graceful_shutdown(shutdown).await
    }

    /// Stops every session and ends every track. The node does not come back
    /// from this: it is what a shutdown signal calls.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown.notify_waiters();
        let sessions = {
            let mut sessions = self.sessions.lock().unwrap();
            std::mem::take(&mut *sessions)
        };
        for (_, session) in sessions {
            session.destroy();
        }
    }

    /// Starts the tasks that sample the machine, expire sessions and post the
    /// node-wide reports.
    fn start_background(self: &Arc<Self>, reports: mpsc::Receiver<()>) {
        let reaper = Arc::clone(self);
        self.handle.spawn(async move { reaper.reap().await });
        if self.deps.webhook.is_some() {
            let reporter = Arc::clone(self);
            self.handle.spawn(async move { reporter.report_loop(reports).await });
        }
    }

    /// Runs a future on the node's runtime, from a thread that is not a task.
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future);
    }

    /// The configuration the node was built with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Sends an event to the webhook and asks for a fresh node-wide report.
    pub fn publish(&self, event: &Value) {
        if let Some(client) = &self.deps.webhook {
            client.send(event.clone());
        }
        self.request_report();
    }

    /// Asks for a report of every player. It never blocks and never queues up:
    /// a burst of activity asks for the same one report, and the configured
    /// latency decides whether that report is sent.
    pub fn request_report(&self) {
        if self.deps.webhook.is_none() {
            return;
        }
        let _ = self.reports.try_send(());
    }
}

/// Sets up one accepted connection.
///
/// Nagle is off because both halves of this surface are latency-sensitive: the
/// REST call that starts a track and the event that says it did should not wait
/// for a delayed acknowledgement. Keepalive is on because a client that was
/// killed without closing its socket is otherwise a connection the kernel holds
/// for two hours; the probes give up on it after a minute and a half of
/// silence, which is when its session is worth releasing.
fn configure_connection(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Stamps every response with the headers a client reads to tell one node from
/// another.
async fn stamp(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert("Rustlink-Api-Version", HeaderValue::from_static(APIVERSION));
    headers.insert("Server", SERVER_HEADER.clone());
    response
}

/// Accounts for every request: successful ones at debug, rejected ones at warn
/// and server faults at error, so an operator sees failures without turning on
/// debug logging.
async fn log_request(
    State(server): State<Arc<Server>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    server.requests.fetch_add(1, Ordering::Relaxed);
    let method = request.method().clone();
    // The pieces the line is made of are kept in the form that costs nothing
    // until a level that is on asks for them: the URI as a handle and the
    // address as the value itself, rather than their text. Every request
    // passes through here, so a request that is not logged allocates nothing.
    let uri = request.uri().clone();
    let remote = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    let start = Instant::now();
    let mut response = next.run(request).await;
    if (response.status().is_client_error() || response.status().is_server_error())
        && response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok())
            != Some("application/json")
    {
        let status = response.status();
        response =
            server.fail(status, status.canonical_reason().unwrap_or("request failed"), uri.path());
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

/// The address a request came from, printed the way the log line wants it: the
/// address, or nothing when the connection does not carry one. Holding the
/// address rather than its text is what lets a level that is off skip the
/// formatting along with the line.
struct Remote(Option<std::net::IpAddr>);

impl std::fmt::Display for Remote {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(address) => write!(formatter, "{address}"),
            None => Ok(()),
        }
    }
}

impl Server {
    /// The rate limit and the password check every protected route starts with.
    /// The limiter runs first, so a throttled request is refused before
    /// authentication and a rejected one still spends a token.
    // A refusal is a whole response, which the lint counts as a large error.
    // Boxing it would only move that cost onto every route that uses `?`, and
    // the response is the type axum handlers already return.
    #[allow(clippy::result_large_err)]
    fn guard(&self, route: &str, headers: &HeaderMap, path: &str) -> Result<(), Response> {
        if self.config.ratelimits.enable && !self.limits.allow(route) {
            return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
        }
        let given = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token)
            .unwrap_or("");
        if !same(given.as_bytes(), self.config.password.as_bytes()) {
            return Err(self.fail(StatusCode::UNAUTHORIZED, "invalid authorization", path));
        }
        Ok(())
    }

    /// The rate limit alone, for the routes a monitoring system reads without
    /// the password.
    #[allow(clippy::result_large_err)]
    fn guard_public(&self, route: &str, path: &str) -> Result<(), Response> {
        if self.config.ratelimits.enable && !self.limits.allow(route) {
            return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
        }
        Ok(())
    }

    fn fail(&self, status: StatusCode, message: &str, path: &str) -> Response {
        let code = status
            .canonical_reason()
            .unwrap_or("unknown error")
            .to_ascii_lowercase()
            .replace(' ', "_");
        let body = json!({
            "error": { "code": code, "message": message, "path": path },
            "timestamp_ms": now_millis(),
        });
        (status, Json(body)).into_response()
    }

    fn json(&self, status: StatusCode, body: impl Serialize) -> Response {
        (status, Json(body)).into_response()
    }

    /// REST remains available during the event socket's reconnect grace period.
    #[allow(clippy::result_large_err)]
    fn session(&self, id: &str, path: &str) -> Result<Arc<Session>, Response> {
        let found = self.sessions.lock().unwrap().get(id).cloned();
        match found {
            Some(session) if !session.is_closed() && !session.expired() => Ok(session),
            _ => Err(self.fail(StatusCode::NOT_FOUND, "session not found", path)),
        }
    }

    /// Resolves an identifier into a load result, on a thread of its own so a
    /// slow source cannot stall the runtime, and inside the configured timeout
    /// so a client is answered either way.
    async fn load(&self, identifier: &str) -> Arc<LoadResult> {
        let loader = self.deps.loader.clone();
        let identifier = identifier.to_string();
        let work = tokio::task::spawn_blocking(move || loader.load_shared(&identifier));
        match tokio::time::timeout(self.config.load_timeout(), work).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(error))) => Arc::new(LoadResult::from_error(error)),
            Ok(Err(error)) => Arc::new(LoadResult::from_error(error)),
            Err(_) => Arc::new(LoadResult::from_error("the source did not answer in time")),
        }
    }

    /// Answers with the lyrics of a track, or with no content when there are
    /// none to send. A provider that failed is a bad gateway rather than no
    /// content, because the difference matters to a client deciding whether to
    /// ask again.
    async fn answer_lyrics(&self, track: rustlink_protocol::Track, path: &str) -> Response {
        let Some(manager) = self.deps.lyrics.clone() else {
            return StatusCode::NO_CONTENT.into_response();
        };
        if !manager.enabled() {
            return StatusCode::NO_CONTENT.into_response();
        }
        let work = tokio::task::spawn_blocking(move || manager.fetch(&track));
        match tokio::time::timeout(LYRICS_TIMEOUT, work).await {
            Ok(Ok(Ok(lyrics))) => self.json(StatusCode::OK, lyrics),
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

    /// The enabled native sources reported by `GET /api/v1/info`.
    fn sources(&self) -> Vec<&'static str> {
        let mut sources = vec!["http"];
        if self.config.youtube_native {
            sources.push("youtube");
        }
        if self.config.soundcloud_native {
            sources.push("soundcloud");
        }
        if !self.config.spotify_client_id.is_empty() {
            sources.push("spotify");
        }
        sources.push("deezer");
        sources
    }
}

/// Compares two secrets without letting the time taken say how much of one
/// matched.
fn same(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len() && bool::from(given.ct_eq(expected))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

async fn version(State(server): State<Arc<Server>>, _headers: HeaderMap) -> Response {
    if let Err(response) = server.guard_public("version", "/version") {
        return response;
    }
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], VERSION).into_response()
}

async fn healthz(State(server): State<Arc<Server>>, _headers: HeaderMap) -> Response {
    if let Err(response) = server.guard_public("healthz", "/healthz") {
        return response;
    }
    server.json(StatusCode::OK, json!({ "status": "ok", "name": "Rustlink", "version": VERSION }))
}

async fn not_found(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    if let Err(response) = server.guard("root", &headers, uri.path()) {
        return response;
    }
    server.fail(StatusCode::NOT_FOUND, "route not found", uri.path())
}

async fn info(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    if let Err(response) = server.guard("info", &headers, "/api/v1/info") {
        return response;
    }
    let (_, _, cpu_available) = server.cpu.values();
    server.json(StatusCode::OK, json!({
        "name": "Rustlink", "version": VERSION, "api_version": APIVERSION,
        "sources": server.sources(), "filters": rustlink_audio::FILTER_NAMES,
        "capabilities": { "track_tokens": "rlt1", "session_resume": true,
            "lyrics": server.deps.lyrics.as_ref().is_some_and(|manager| manager.enabled()),
            "cpu_stats": cpu_available, "discord_dave": true },
        "voice": { "gateway_version": rustlink_voice::GATEWAY_VERSION,
            "max_dave_protocol_version": rustlink_voice::MAX_DAVE_PROTOCOL_VERSION,
            "media_codecs": ["opus"],
            "transport_modes": [rustlink_voice::Mode::AeadAes256GcmRtpSize.name(),
                rustlink_voice::Mode::AeadXChaCha20Poly1305RtpSize.name()] },
        "limits": { "max_players": server.config.max_players, "max_sessions": server.config.max_sessions,
            "max_body_bytes": 1 << 20, "max_inspect_tokens": MAX_DECODE_TRACKS },
    }))
}

async fn stats(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    if let Err(response) = server.guard("stats", &headers, "/api/v1/stats") {
        return response;
    }
    server.json(StatusCode::OK, server.stats_body())
}

async fn metrics(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    if let Err(response) = server.guard("metrics", &headers, "/api/v1/metrics") {
        return response;
    }
    ([(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], server.metrics_body())
        .into_response()
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
    tracks: Vec<rustlink_protocol::Track>,
}

#[derive(Serialize)]
struct PlayersResponse {
    players: Vec<rustlink_protocol::Player>,
}

#[allow(clippy::result_large_err)]
fn read_json<T: serde::de::DeserializeOwned>(
    server: &Server,
    body: &[u8],
    path: &str,
) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|error| {
        server.fail(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {error}"), path)
    })
}

async fn load_tracks(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = "/api/v1/tracks/resolve";
    if let Err(response) = server.guard("resolve_tracks", &headers, path) {
        return response;
    }
    let request: ResolveRequest = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.query.trim().is_empty() || request.query.len() > 8192 {
        return server.fail(StatusCode::BAD_REQUEST, "query must contain 1..8192 bytes", path);
    }
    let result = server.load(request.query.trim()).await;
    if let LoadResult::Error(error) = result.as_ref() {
        return server.fail(StatusCode::BAD_GATEWAY, &error.message, path);
    }
    server.json(StatusCode::OK, result.as_ref())
}

async fn decode_track_route(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = "/api/v1/tracks/inspect";
    if let Err(response) = server.guard("inspect_track", &headers, path) {
        return response;
    }
    let request: TokenRequest = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match decode_track(&request.token) {
        Ok(track) => server.json(StatusCode::OK, track),
        Err(error) => server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path),
    }
}

async fn decode_tracks(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = "/api/v1/tracks/inspect-batch";
    if let Err(response) = server.guard("inspect_tracks", &headers, path) {
        return response;
    }
    let request: TokensRequest = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.tokens.len() > MAX_DECODE_TRACKS {
        return server.fail(StatusCode::BAD_REQUEST, "at most 1000 tokens per request", path);
    }
    let mut tracks = Vec::with_capacity(request.tokens.len());
    for token in request.tokens {
        match decode_track(&token) {
            Ok(track) => tracks.push(track),
            Err(error) => return server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path),
        }
    }
    server.json(StatusCode::OK, TracksResponse { tracks })
}

async fn lyrics_for_track(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = "/api/v1/lyrics";
    if let Err(response) = server.guard("lyrics", &headers, path) {
        return response;
    }
    let request: TokenRequest = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match decode_track(&request.token) {
        Ok(track) => server.answer_lyrics(track, path).await,
        Err(error) => server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path),
    }
}

async fn lyrics_for_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/players/{guild_id}/lyrics";
    if let Err(response) = server.guard("player_lyrics", &headers, path) {
        return response;
    }
    let session = match server.session(&session_id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let Some(player) = session.player(&guild_id) else {
        return server.fail(StatusCode::NOT_FOUND, "player not found", path);
    };
    let Some(track) = player.snapshot().track else {
        return server.fail(StatusCode::NOT_FOUND, "no track is playing", path);
    };
    server.answer_lyrics(track, path).await
}

async fn get_players(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/players";
    if let Err(response) = server.guard("get_players", &headers, path) {
        return response;
    }
    let session = match server.session(&session_id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    server.json(StatusCode::OK, PlayersResponse { players: session.snapshots() })
}

async fn get_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/players/{guild_id}";
    if let Err(response) = server.guard("get_player", &headers, path) {
        return response;
    }
    let session = match server.session(&session_id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let Some(player) = session.player(&guild_id) else {
        return server.fail(StatusCode::NOT_FOUND, "player not found", path);
    };
    server.json(StatusCode::OK, player.snapshot())
}

async fn update_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((session_id, guild_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/players/{guild_id}";
    if let Err(response) = server.guard("update_player", &headers, path) {
        return response;
    }
    let session = match server.session(&session_id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    if session.is_closed() {
        return server.fail(StatusCode::NOT_FOUND, "session is closed", path);
    }
    if !rustlink_voice::valid_id(&guild_id) {
        return server.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path);
    }
    let update: PlayerUpdate = match serde_json::from_slice(&body) {
        Ok(update) => update,
        Err(error) => {
            return server.fail(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON body: {error}"),
                path,
            )
        }
    };
    if let Err(error) = crate::player::validate(&update) {
        return server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path);
    }
    if let Some(filters) = &update.filters {
        // Validate operator policy before mutating the player.
        let disabled = rustlink_audio::disabled_filters(filters, &server.config.filters);
        if !disabled.is_empty() {
            let message =
                format!("Following filters are disabled in the config: {}", disabled.join(", "));
            return server.fail(StatusCode::BAD_REQUEST, &message, path);
        }
    }
    let no_replace = match query.get("replace") {
        Some(raw) => match parse_bool(raw) {
            Some(value) => !value,
            None => {
                return server.fail(StatusCode::BAD_REQUEST, "replace must be true or false", path)
            }
        },
        None => false,
    };
    if query.keys().any(|key| key != "replace") {
        return server.fail(
            StatusCode::BAD_REQUEST,
            "unknown query parameter; expected replace",
            path,
        );
    }
    let player = match session.get_or_insert(&guild_id, || {
        if !server.reserve_player() {
            return Err(());
        }
        let player = Player::new(
            session.user.clone(),
            guild_id.clone(),
            Arc::clone(&server.player_deps),
            session.emitter(),
            Arc::clone(&server.counters),
        );
        log::info!(
            "player created guild={guild_id} session={} players={}",
            session.id,
            server.players.load(Ordering::SeqCst)
        );
        server.request_report();
        Ok(player)
    }) {
        Ok(Some(player)) => player,
        Ok(None) => return server.fail(StatusCode::NOT_FOUND, "session is closed", path),
        Err(()) => {
            return server.fail(StatusCode::SERVICE_UNAVAILABLE, "player capacity reached", path)
        }
    };
    match player.update(update, no_replace).await {
        Ok(body) => server.json(StatusCode::OK, body),
        Err(error) => server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path),
    }
}

async fn delete_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/players/{guild_id}";
    if let Err(response) = server.guard("delete_player", &headers, path) {
        return response;
    }
    let session = match server.session(&session_id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
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
    StatusCode::NO_CONTENT.into_response()
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

async fn create_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = "/api/v1/sessions";
    if let Err(response) = server.guard("create_session", &headers, path) {
        return response;
    }
    let request: CreateSession = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if !rustlink_voice::valid_id(&request.bot_id)
        || request.resume_timeout_secs.is_some_and(|timeout| timeout == 0 || timeout > 86_400)
    {
        return server.fail(
            StatusCode::BAD_REQUEST,
            "valid bot_id and resume_timeout_secs of 1..86400 are required",
            path,
        );
    }
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        return server.fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not generate session ID",
            path,
        );
    }
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut sessions = server.sessions.lock().unwrap();
    if server.closed.load(Ordering::SeqCst) || sessions.len() >= server.config.max_sessions as usize
    {
        return server.fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "session capacity reached or server shutting down",
            path,
        );
    }
    let session = Session::new(
        id.clone(),
        request.bot_id,
        Arc::downgrade(&server),
        server.config.event_buffer as usize,
    );
    session.update(Some(true), request.resume_timeout_secs);
    let body = session.snapshot();
    sessions.insert(id.clone(), session);
    let mut response = server.json(StatusCode::CREATED, body);
    response
        .headers_mut()
        .insert("location", HeaderValue::from_str(&format!("/api/v1/sessions/{id}")).unwrap());
    response
}

async fn get_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}";
    if let Err(response) = server.guard("get_session", &headers, path) {
        return response;
    }
    match server.session(&id, path) {
        Ok(session) => server.json(StatusCode::OK, session.snapshot()),
        Err(response) => response,
    }
}

async fn delete_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let path = "/api/v1/sessions/{session_id}";
    if let Err(response) = server.guard("delete_session", &headers, path) {
        return response;
    }
    server.remove_session(&id);
    StatusCode::NO_CONTENT.into_response()
}

async fn update_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let path = "/api/v1/sessions/{session_id}";
    if let Err(response) = server.guard("update_session", &headers, path) {
        return response;
    }
    let session = match server.session(&id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let update: UpdateSession = match read_json(&server, &body, path) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if update.resume_timeout_secs.is_some_and(|timeout| timeout == 0 || timeout > 86_400) {
        return server.fail(StatusCode::BAD_REQUEST, "resume_timeout_secs must be 1..86400", path);
    }
    session.update(update.resume, update.resume_timeout_secs);
    server.json(StatusCode::OK, session.snapshot())
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

async fn websocket(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let path = "/api/v1/sessions/{session_id}/events";
    let (mut parts, _) = request.into_parts();
    if let Err(response) = server.guard("websocket", &parts.headers, path) {
        return response;
    }
    let session = match server.session(&id, path) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => {
            return server.fail(
                StatusCode::BAD_REQUEST,
                "the request is not a websocket upgrade",
                path,
            )
        }
    };
    let (inbox, resumed) = {
        // Coordinate with expiry and deletion while claiming the only subscriber.
        let sessions = server.sessions.lock().unwrap();
        if !sessions.contains_key(&id) || session.is_closed() || session.expired() {
            return server.fail(StatusCode::NOT_FOUND, "session not found", path);
        }
        if session.is_connected() {
            return server.fail(
                StatusCode::CONFLICT,
                "session already has an event subscriber",
                path,
            );
        }
        let Some(inbox) = session.take_inbox() else {
            return server.fail(StatusCode::CONFLICT, "previous subscriber is disconnecting", path);
        };
        let resumed = session.has_connected();
        session.connect();
        (inbox, resumed)
    };
    let failed_server = Arc::clone(&server);
    let failed_session = Arc::clone(&session);
    // An aborted upgrade cannot leave an immortal connected session behind.
    upgrade
        .max_message_size(64 << 10)
        .max_frame_size(64 << 10)
        .on_failed_upgrade(move |_| {
            failed_server.remove_session(&failed_session.id);
        })
        .on_upgrade(move |socket| server.serve(session, inbox, socket, resumed))
        .into_response()
}

impl Server {
    /// Serves one socket: the ready message, the events and the ticks that go
    /// out, and the messages that come in.
    async fn serve(
        self: Arc<Self>,
        session: Arc<Session>,
        inbox: mpsc::Receiver<Value>,
        mut socket: WebSocket,
        resumed: bool,
    ) {
        let ready = json!({ "type": "session.ready", "timestamp_ms": now_millis(), "api_version": APIVERSION,
            "resumed": resumed, "session_id": session.id, "players": session.snapshots() });
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(10),
                socket.send(Message::Text(ready.to_string().into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            self.detach(&session);
            session.restore_inbox(inbox);
            return;
        }
        let (sink, mut stream) = socket.split();
        let (control, commands) = mpsc::channel(8);
        // A client arriving is one more moment worth reporting, and it goes
        // through the same latency as every other one.
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
                    Ok(Some(Ok(Message::Pong(_)))) => {},
                    Ok(Some(Ok(Message::Text(_)))) | Ok(Some(Ok(Message::Binary(_)))) => {
                        let error = json!({"type":"request.rejected", "session_id":session.id, "timestamp_ms":now_millis(),
                            "error":{"code":"rest_required", "message":"use REST to change a player or session"}});
                        if control.send(Message::Text(error.to_string().into())).await.is_err() { break None; }
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
        self.detach(&session);
        if let Ok(inbox) = result {
            session.restore_inbox(inbox);
        }
    }

    /// Detaches a socket from its session. A session that asked to be resumed
    /// is kept for its timeout; every other one is closed now.
    fn detach(&self, session: &Arc<Session>) {
        session.disconnect();
        if session.is_closed() {
            return;
        }
        if session.resuming() && session.timeout() > 0 {
            log::info!(
                "session disconnected, awaiting resume session={} timeoutSeconds={} players={}",
                session.id,
                session.timeout(),
                session.players().len()
            );
            return;
        }
        log::info!("session closed session={} players={}", session.id, session.players().len());
        self.remove_session(&session.id);
    }

    /// Drops a session and everything it owns.
    pub fn remove_session(&self, id: &str) {
        let session = self.sessions.lock().unwrap().remove(id);
        if let Some(session) = session {
            session.destroy();
        }
    }

    /// Counts a player as gone, which is what the capacity check reads.
    pub fn player_destroyed(&self) {
        let mut count = self.players.load(Ordering::SeqCst);
        while count > 0 {
            match self.players.compare_exchange_weak(
                count,
                count - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(current) => count = current,
            }
        }
    }

    fn reserve_player(&self) -> bool {
        let mut count = self.players.load(Ordering::SeqCst);
        while count < self.config.max_players as u64 {
            match self.players.compare_exchange_weak(
                count,
                count + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(current) => count = current,
            }
        }
        false
    }

    /// What `GET /api/v1/stats` reports.
    fn stats_body(&self) -> Value {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        let mut playing = 0;
        for session in &sessions {
            for player in session.players() {
                if player.playing() {
                    playing += 1;
                }
            }
        }
        let (system_load, process_load, _) = self.cpu.values();
        let (used, total) = self.cpu.memory();
        json!({
            "players": self.players.load(Ordering::SeqCst),
            "sessions": sessions.len(),
            "playing_players": playing,
            "uptime_ms": self.started.elapsed().as_millis() as i64,
            "memory": { "process_bytes": used, "system_total_bytes": total },
            "cpu": {
                "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
                "system_load": system_load,
                "process_load": process_load,
            },
        })
    }

    /// What `GET /metrics` reports, in the text exposition format Prometheus
    /// reads.
    fn metrics_body(&self) -> String {
        let sessions = self.sessions.lock().unwrap().len();
        let uptime = self.started.elapsed().as_secs_f64();
        let samples: [(&str, &str, &str, String); 9] = [
            (
                "rustlink_players",
                "gauge",
                "Current players",
                self.players.load(Ordering::SeqCst).to_string(),
            ),
            ("rustlink_sessions", "gauge", "Current sessions", sessions.to_string()),
            (
                "rustlink_http_requests_total",
                "counter",
                "HTTP requests",
                self.requests.load(Ordering::SeqCst).to_string(),
            ),
            (
                "rustlink_audio_frames_total",
                "counter",
                "Opus frames sent",
                self.counters.sent.load(Ordering::Relaxed).to_string(),
            ),
            (
                "rustlink_audio_deficit_total",
                "counter",
                "Missed frame deadlines",
                self.counters.deficit.load(Ordering::Relaxed).to_string(),
            ),
            (
                "rustlink_audio_errors_total",
                "counter",
                "Playback failures",
                self.counters.errors.load(Ordering::Relaxed).to_string(),
            ),
            (
                "rustlink_heap_bytes",
                "gauge",
                "Bytes this process holds; excludes media subprocesses",
                self.cpu.memory().0.to_string(),
            ),
            ("rustlink_tasks", "gauge", "Background tasks", "0".into()),
            ("rustlink_uptime_seconds", "gauge", "Uptime in seconds", format!("{uptime:.3}")),
        ];
        let mut out = String::new();
        for (name, kind, help, value) in samples {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"));
        }
        out
    }

    /// Samples the machine and expires the sessions that have waited too long,
    /// which is what keeps a node that is left running from filling up with
    /// sessions nobody is coming back for.
    async fn reap(self: Arc<Self>) {
        self.cpu.sample();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => return,
                _ = ticker.tick() => {
                    self.cpu.sample();
                    let expired: Vec<Arc<Session>> = {
                        let mut sessions = self.sessions.lock().unwrap();
                        let ids: Vec<String> = sessions.values().filter(|session| session.expired()).map(|session| session.id.clone()).collect();
                        ids.into_iter().filter_map(|id| sessions.remove(&id)).collect()
                    };
                    for session in expired {
                        log::info!(
                            "session expired session={} user={} resumable={} timeoutSeconds={} players={}",
                            session.id,
                            session.user,
                            session.resuming(),
                            session.timeout(),
                            session.players().len()
                        );
                        session.destroy();
                    }
                }
            }
        }
    }

    /// Posts the reports the node asks for. The node announces itself as soon
    /// as it is up, which is how a restart shows in a channel before anything
    /// is playing.
    async fn report_loop(self: Arc<Self>, mut reports: mpsc::Receiver<()>) {
        self.request_report();
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => return,
                request = reports.recv() => {
                    if request.is_none() {
                        return;
                    }
                    self.report_player_state();
                }
            }
        }
    }

    /// Posts the state of every player on the node. Clients keep receiving
    /// their own `player.updated` per session; this is the node-wide summary an
    /// operator watches. Reports are spaced by the configured latency, and a
    /// request that arrives sooner than that is dropped rather than queued, so
    /// a busy node posts one on that interval and an idle one goes quiet.
    fn report_player_state(&self) {
        let latency = self.config.webhook.state_latency_seconds;
        if self.deps.webhook.is_none() || latency < 1 {
            return;
        }
        {
            let mut last = self.last_report.lock().unwrap();
            if let Some(at) = *last {
                if at.elapsed() < Duration::from_secs(latency as u64) {
                    return;
                }
            }
            *last = Some(Instant::now());
        }
        let report = self.player_state_report();
        if let Some(client) = &self.deps.webhook {
            client.send(report);
        }
    }

    /// Summarises every player on the node. It is meant for a channel other
    /// people may read, so it carries only what describes playback: the session
    /// and user that own a player are left out, along with the encoded track
    /// and every other field a reader has no business seeing.
    fn player_state_report(&self) -> Value {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        let mut states = Vec::new();
        let mut playing = 0;
        for session in &sessions {
            for player in session.players() {
                let snapshot = player.snapshot();
                if snapshot.track.is_some() && !snapshot.paused && snapshot.state.connected {
                    playing += 1;
                }
                let info =
                    snapshot.track.as_ref().map(|track| track.metadata.clone()).unwrap_or_default();
                states.push(json!({
                    "guild_id": snapshot.guild_id,
                    "title": info.title,
                    "author": info.author,
                    "source": info.source_name,
                    "duration_ms": info.length,
                    "position_ms": snapshot.state.position,
                    "endpoint": snapshot.voice.endpoint,
                    "volume": snapshot.volume,
                    "paused": snapshot.paused,
                    "connected": snapshot.state.connected,
                    "ping_ms": snapshot.state.ping,
                }));
            }
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

/// Writes everything that goes to a client: its events, its periodic player
/// updates and stats, and the pings that keep the connection alive. The
/// receiver is handed back so a session that is resumed can keep reading the
/// events that arrive while nobody is listening.
async fn write_loop(
    server: Arc<Server>,
    session: Arc<Session>,
    mut sink: SplitSink<WebSocket, Message>,
    mut inbox: mpsc::Receiver<Value>,
    mut control: mpsc::Receiver<Message>,
) -> mpsc::Receiver<Value> {
    let update_seconds = server.config.player_update_seconds.max(1) as u64;
    let stats_seconds = server.config.stats_seconds.max(1) as u64;
    let mut updates = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(update_seconds),
        Duration::from_secs(update_seconds),
    );
    let mut stats = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(stats_seconds),
        Duration::from_secs(stats_seconds),
    );
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut previous_sent = server.counters.sent.load(Ordering::Relaxed);
    let mut previous_deficit = server.counters.deficit.load(Ordering::Relaxed);
    loop {
        tokio::select! {
            _ = session.stopped() => break,
            command = control.recv() => match command {
                Some(command) => if send_frame(&mut sink, command).await.is_err() { break },
                None => break,
            },
            event = inbox.recv() => match event {
                Some(event) => {
                    if send_frame(&mut sink, Message::Text(event.to_string().into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            _ = updates.tick() => {
                for player in session.snapshots() {
                    let message = json!({ "type": "player.updated", "session_id": session.id, "timestamp_ms": now_millis(), "guild_id": player.guild_id, "state": player.state });
                    if send_frame(&mut sink, Message::Text(message.to_string().into())).await.is_err() {
                        return inbox;
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
                    "sent": sent - previous_sent,
                    "deficit": deficit - previous_deficit,
                });
                previous_sent = sent;
                previous_deficit = deficit;
                if send_frame(&mut sink, Message::Text(body.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = ping.tick() => {
                if send_frame(&mut sink, Message::Ping(Vec::new().into())).await.is_err() {
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
