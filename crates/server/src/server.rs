//! Rustlink API v1: REST resources and a per-session event stream.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, OriginalUri, Path, Request, State};
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
use rustlink_protocol::{decode_track_owned, LoadResult, PlayerUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::cpu::Cpu;
use crate::player::{Deps, Player};
use crate::ratelimit::Keyed;
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

/// How many distinct sessions or addresses each limiter tracks. The keys can be
/// invented by a caller, so the table is bounded.
const MAX_KEYS: usize = 10_000;

/// Budgets for authenticated requests as `(route, requests per second, burst)`.
/// Each session spends its own bucket for each route, so one busy bot cannot
/// starve another and an outsider cannot spend either. The player routes are the
/// ones a bot in many servers calls constantly, so their budgets are generous;
/// the limit is there to stop a runaway client, not to ration a healthy one.
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
    Root,
}

impl Route {
    const COUNT: usize = 18;
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

/// Failed password checks per remote address. A wrong password is the only thing
/// an outsider can send that reaches the limiter, so this is what slows a guess
/// without touching anyone's real allowance.
const FAILED_LOGINS: (f64, usize) = (2.0, 10);

/// The request limits. Authentication comes first: a request is only counted
/// against a session's budget once it has proved it may be, so nobody without
/// the password can use up what a legitimate bot depends on.
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

    /// Spends a token of `session`'s budget for a route that needs the password.
    fn allow_authenticated(&self, route: Route, session: &str) -> bool {
        self.authenticated[route as usize].allow(session)
    }

    /// Spends a token of the caller's address for a route open to everyone.
    fn allow_public(&self, route: PublicRoute, remote: &str) -> bool {
        self.public[route as usize].allow(remote)
    }

    /// Records a wrong password from `remote`, and reports whether it may try again.
    fn allow_failed_login(&self, remote: &str) -> bool {
        self.failed_logins.allow(remote)
    }
}

tokio::task_local! {
    /// The address the request being handled arrived from, set around every
    /// request so the guards can key a limit on it without every handler
    /// passing it along.
    static REMOTE: Option<std::net::IpAddr>;
}

/// Stack-resident rate-limit key for remote IPv4 or IPv6 `/64` prefixes (at most 24 ASCII bytes).
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

/// The limit key for the caller: its address, with IPv4-mapped IPv6 addresses
/// canonicalized to IPv4 and native IPv6 addresses reduced to their /64.
fn remote_key() -> RemoteKey {
    let mut key = RemoteKey { buf: [0u8; 24], len: 0 };
    match REMOTE.try_with(|remote| remote.map(|ip| ip.to_canonical())).ok().flatten() {
        Some(std::net::IpAddr::V4(address)) => {
            let _ = write!(key, "{address}");
        }
        Some(std::net::IpAddr::V6(address)) => {
            let segments = address.segments();
            let _ = write!(
                key,
                "{:x}:{:x}:{:x}:{:x}::/64",
                segments[0], segments[1], segments[2], segments[3]
            );
        }
        None => {
            let _ = key.write_str("unknown");
        }
    }
    key
}

/// The key a session's budget is kept under: the session ID in the path (with
/// ASCII percent-encoding normalized so `%61` and `a` share one bucket), or
/// `node` for a route that is not about one session.
fn session_key(path: &str) -> std::borrow::Cow<'_, str> {
    let Some(raw) = path
        .split_once("/sessions/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .filter(|id| !id.is_empty())
    else {
        return std::borrow::Cow::Borrowed("node");
    };
    if !raw.contains('%') {
        let trimmed = raw.char_indices().nth(64).map_or(raw, |(end, _)| &raw[..end]);
        return std::borrow::Cow::Borrowed(trimmed);
    }
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len().min(192));
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
        return std::borrow::Cow::Borrowed("node");
    }
    match String::from_utf8(decoded) {
        Ok(mut text) => {
            if let Some((end, _)) = text.char_indices().nth(64) {
                text.truncate(end);
            }
            std::borrow::Cow::Owned(text)
        }
        Err(err) => {
            let lossy = String::from_utf8_lossy(err.as_bytes());
            std::borrow::Cow::Owned(lossy.chars().take(64).collect())
        }
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
    cpu: Arc<Cpu>,
    limits: Limits,
    /// Carries one pending request for a node-wide player report. A burst of
    /// activity asks for the same one report.
    reports: mpsc::Sender<()>,
    last_report: Mutex<Option<Instant>>,
    /// Active WebSocket event streams, tracked so graceful shutdown lets them
    /// flush `track.ended` (`"cleanup"`) and `Close` before the runtime drops.
    ws_active: AtomicUsize,
    ws_drained: tokio::sync::Notify,
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

    /// Register before checking the persistent state so closing before a task
    /// starts, or between iterations, cannot lose the shutdown notification.
    async fn shutdown_requested(&self) {
        let notified = self.shutdown.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.closed.load(Ordering::SeqCst) {
            notified.await;
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
    let mut response = REMOTE.scope(remote, next.run(request)).await;
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
    /// The password check every protected route starts with, and then the rate
    /// limit. The password comes first, so a request that cannot prove who it is
    /// never spends a session's allowance: a wrong password is counted against
    /// the sender's address instead, which is what slows guessing down.
    // A refusal is a whole response, which the lint counts as a large error.
    // Boxing it would only move that cost onto every route that uses `?`, and
    // the response is the type axum handlers already return.
    #[allow(clippy::result_large_err)]
    fn guard(&self, route: Route, headers: &HeaderMap, path: &str) -> Result<(), Response> {
        let given = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token)
            .unwrap_or("");
        let limited = self.config.ratelimits.enable;
        if !same(given.as_bytes(), self.config.password.as_bytes()) {
            if limited && !self.limits.allow_failed_login(remote_key().as_str()) {
                return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
            }
            return Err(self.fail(StatusCode::UNAUTHORIZED, "invalid authorization", path));
        }
        if limited && !self.limits.allow_authenticated(route, &session_key(path)) {
            return Err(self.fail(StatusCode::TOO_MANY_REQUESTS, "too many requests", path));
        }
        Ok(())
    }

    /// The rate limit alone, for the routes a monitoring system reads without
    /// the password.
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
        let body = ErrorEnvelope {
            error: ErrorFields { code: &code, message, path },
            timestamp_ms: now_millis(),
        };
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
            Some(session) if session.is_usable() => Ok(session),
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
        let work = tokio::task::spawn_blocking(move || manager.fetch_shared(&track));
        match tokio::time::timeout(LYRICS_TIMEOUT, work).await {
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

    /// The enabled native sources reported by `GET /api/v1/info`.
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

/// Compares two secrets without letting the time taken say how much of one
/// matched or whether the candidate length matched `expected`.
fn same(given: &[u8], expected: &[u8]) -> bool {
    let len_eq = (given.len() as u64).ct_eq(&(expected.len() as u64));
    let mut diff = 0u8;
    for (index, &exp) in expected.iter().enumerate() {
        let got = given.get(index).copied().unwrap_or(0);
        diff |= got ^ exp;
    }
    bool::from(len_eq & diff.ct_eq(&0))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

type RouteResult = Result<Response, Response>;

#[allow(clippy::result_large_err)]
async fn version(State(server): State<Arc<Server>>, _headers: HeaderMap) -> RouteResult {
    server.guard_public(PublicRoute::Version, "/version")?;
    Ok(([(CONTENT_TYPE, "text/plain; charset=utf-8")], VERSION).into_response())
}

#[allow(clippy::result_large_err)]
async fn healthz(State(server): State<Arc<Server>>, _headers: HeaderMap) -> RouteResult {
    #[derive(Serialize)]
    struct Health {
        status: &'static str,
        name: &'static str,
        version: &'static str,
    }
    server.guard_public(PublicRoute::Healthz, "/healthz")?;
    Ok(server.json(StatusCode::OK, Health { status: "ok", name: "Rustlink", version: VERSION }))
}

#[allow(clippy::result_large_err)]
async fn not_found(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> RouteResult {
    server.guard(Route::Root, &headers, uri.path())?;
    Ok(server.fail(StatusCode::NOT_FOUND, "route not found", uri.path()))
}

#[allow(clippy::result_large_err)]
async fn info(State(server): State<Arc<Server>>, headers: HeaderMap) -> RouteResult {
    server.guard(Route::Info, &headers, "/api/v1/info")?;
    let (_, _, cpu_available) = server.cpu.values();
    Ok(server.json(StatusCode::OK, json!({
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
    })))
}

#[allow(clippy::result_large_err)]
async fn stats(State(server): State<Arc<Server>>, headers: HeaderMap) -> RouteResult {
    server.guard(Route::Stats, &headers, "/api/v1/stats")?;
    Ok(server.json(StatusCode::OK, server.stats_body()))
}

#[allow(clippy::result_large_err)]
async fn metrics(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> RouteResult {
    server.guard(Route::Metrics, &headers, uri.path())?;
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

#[allow(clippy::result_large_err)]
async fn load_tracks(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> RouteResult {
    let path = "/api/v1/tracks/resolve";
    server.guard(Route::ResolveTracks, &headers, path)?;
    let request: ResolveRequest = read_json(&server, &body, path)?;
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
async fn decode_track_route(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> RouteResult {
    let path = "/api/v1/tracks/inspect";
    server.guard(Route::InspectTrack, &headers, path)?;
    let request: TokenRequest = read_json(&server, &body, path)?;
    match decode_track_owned(request.token) {
        Ok(track) => Ok(server.json(StatusCode::OK, track)),
        Err(error) => Err(server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path)),
    }
}

#[allow(clippy::result_large_err)]
async fn decode_tracks(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> RouteResult {
    let path = "/api/v1/tracks/inspect-batch";
    server.guard(Route::InspectTracks, &headers, path)?;
    let request: TokensRequest = read_json(&server, &body, path)?;
    if request.tokens.len() > MAX_DECODE_TRACKS {
        return Err(server.fail(StatusCode::BAD_REQUEST, "at most 1000 tokens per request", path));
    }
    let mut tracks = Vec::with_capacity(request.tokens.len());
    for token in request.tokens {
        match decode_track_owned(token) {
            Ok(track) => tracks.push(track),
            Err(error) => {
                return Err(server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path))
            }
        }
    }
    Ok(server.json(StatusCode::OK, TracksResponse { tracks }))
}

#[allow(clippy::result_large_err)]
async fn lyrics_for_track(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> RouteResult {
    let path = "/api/v1/lyrics";
    server.guard(Route::Lyrics, &headers, path)?;
    let request: TokenRequest = read_json(&server, &body, path)?;
    match decode_track_owned(request.token) {
        Ok(track) => Ok(server.answer_lyrics(track, path).await),
        Err(error) => Err(server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path)),
    }
}

#[allow(clippy::result_large_err)]
async fn lyrics_for_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    // Keep the error path tied to this request, while excluding a query
    // string that could carry credentials or other sensitive values.
    let path = uri.path();
    server.guard(Route::PlayerLyrics, &headers, path)?;
    let session = server.session(&session_id, path)?;
    if !rustlink_voice::valid_id(&guild_id) {
        return Err(server.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path));
    }
    let Some(player) = session.player(&guild_id) else {
        return Err(server.fail(StatusCode::NOT_FOUND, "player not found", path));
    };
    let Some(track) = player.track() else {
        return Err(server.fail(StatusCode::NOT_FOUND, "no track is playing", path));
    };
    Ok(server.answer_lyrics(track, path).await)
}

#[allow(clippy::result_large_err)]
async fn get_players(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(session_id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::GetPlayers, &headers, path)?;
    let session = server.session(&session_id, path)?;
    Ok(server.json(StatusCode::OK, PlayersResponse { players: session.snapshots() }))
}

#[allow(clippy::result_large_err)]
async fn get_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::GetPlayer, &headers, path)?;
    let session = server.session(&session_id, path)?;
    if !rustlink_voice::valid_id(&guild_id) {
        return Err(server.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path));
    }
    let Some(player) = session.player(&guild_id) else {
        return Err(server.fail(StatusCode::NOT_FOUND, "player not found", path));
    };
    Ok(server.json(StatusCode::OK, player.snapshot()))
}

/// Roll back a first update on both error and future cancellation. This guard
/// must be dropped before the request releases its per-guild update gate.
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
async fn update_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
    body: Bytes,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::UpdatePlayer, &headers, path)?;
    let session = server.session(&session_id, path)?;
    if !rustlink_voice::valid_id(&guild_id) {
        return Err(server.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path));
    }
    let update: PlayerUpdate = read_json(&server, &body, path)?;
    let prepared = crate::player::prepare(update)
        .map_err(|error| server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path))?;
    if let Some(filters) = &prepared.filters {
        // Validate operator policy before mutating the player.
        let disabled = rustlink_audio::disabled_filters(filters, &server.config.filters);
        if !disabled.is_empty() {
            let message =
                format!("Following filters are disabled in the config: {}", disabled.join(", "));
            return Err(server.fail(StatusCode::BAD_REQUEST, &message, path));
        }
    }
    let mut replace_param = None;
    let mut has_unknown_param = false;
    if let Some(raw_query) = uri.query() {
        for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
            if key == "replace" {
                replace_param = Some(value);
            } else {
                has_unknown_param = true;
            }
        }
    }
    let no_replace = match replace_param.as_deref() {
        Some(raw) => match parse_bool(raw) {
            Some(value) => !value,
            None => {
                return Err(server.fail(
                    StatusCode::BAD_REQUEST,
                    "replace must be true or false",
                    path,
                ))
            }
        },
        None => false,
    };
    if has_unknown_param {
        return Err(server.fail(
            StatusCode::BAD_REQUEST,
            "unknown query parameter; expected replace",
            path,
        ));
    }
    let gate = session.update_gate(&guild_id);
    let _serialized = gate.lock().await;
    let (player, created) = match session.get_or_insert(&guild_id, || {
        if !server.reserve_player() {
            return Err(());
        }
        let player = Player::with_load_timeout(
            session.user.clone(),
            guild_id.clone(),
            Arc::clone(&server.player_deps),
            session.emitter(),
            Arc::clone(&server.counters),
            server.config.load_timeout(),
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
        Ok(None) => return Err(server.fail(StatusCode::NOT_FOUND, "session is closed", path)),
        Err(()) => {
            return Err(server.fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "player capacity reached",
                path,
            ))
        }
    };
    let mut creation =
        PlayerCreation { session: &session, server: &server, player: &player, pending: created };
    match player.apply(prepared, no_replace).await {
        Ok(body) => {
            creation.pending = false;
            Ok(server.json(StatusCode::OK, body))
        }
        Err(error) => Err(server.fail(StatusCode::BAD_REQUEST, &error.to_string(), path)),
    }
}

#[allow(clippy::result_large_err)]
async fn delete_player(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((session_id, guild_id)): Path<(String, String)>,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::DeletePlayer, &headers, path)?;
    let session = server.session(&session_id, path)?;
    if !rustlink_voice::valid_id(&guild_id) {
        return Err(server.fail(StatusCode::BAD_REQUEST, "invalid guild ID", path));
    }
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

fn hex_encode_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[allow(clippy::result_large_err)]
async fn create_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> RouteResult {
    let path = "/api/v1/sessions";
    server.guard(Route::CreateSession, &headers, path)?;
    let request: CreateSession = read_json(&server, &body, path)?;
    if !rustlink_voice::valid_id(&request.bot_id)
        || request.resume_timeout_secs.is_some_and(|timeout| timeout == 0 || timeout > 86_400)
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
            sessions.retain(|_, session| {
                if session.expired() {
                    expired_sessions.push(Arc::clone(session));
                    false
                } else {
                    true
                }
            });
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
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::GetSession, &headers, path)?;
    let session = server.session(&id, path)?;
    Ok(server.json(StatusCode::OK, session.snapshot()))
}

#[allow(clippy::result_large_err)]
async fn delete_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::DeleteSession, &headers, path)?;
    server.remove_session(&id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[allow(clippy::result_large_err)]
async fn update_session(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    body: Bytes,
) -> RouteResult {
    let path = uri.path();
    server.guard(Route::UpdateSession, &headers, path)?;
    let session = server.session(&id, path)?;
    let update: UpdateSession = read_json(&server, &body, path)?;
    if update.resume_timeout_secs.is_some_and(|timeout| timeout == 0 || timeout > 86_400) {
        return Err(server.fail(
            StatusCode::BAD_REQUEST,
            "resume_timeout_secs must be 1..86400",
            path,
        ));
    }
    session.configure(update.resume, update.resume_timeout_secs);
    Ok(server.json(StatusCode::OK, session.snapshot()))
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
    let (mut parts, _) = request.into_parts();
    // Use only the path component so query parameters (which may contain
    // credentials) are never copied into an error response.
    let path = parts.uri.path().to_owned();
    if let Err(response) = server.guard(Route::Websocket, &parts.headers, &path) {
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
        // Coordinate with expiry and deletion while claiming the only subscriber.
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
    // The upgrade future runs in a background task.  Keep the receiver in a
    // one-shot cell shared by both outcomes so a failed handshake can return
    // it to the session instead of dropping the queue (and the players with
    // it).  `on_upgrade` and `on_failed_upgrade` are mutually exclusive; the
    // cell makes that ownership explicit and also protects us if an adapter
    // invokes either callback later than expected.
    let pending = Arc::new(Mutex::new(Some(inbox)));
    let failed_pending = Arc::clone(&pending);
    let failed_session = Arc::clone(&session);
    upgrade
        .max_message_size(64 << 10)
        .max_frame_size(64 << 10)
        .on_failed_upgrade(move |_| {
            // A failed upgrade is not a failed session.  Return the queued
            // events and let the normal resume/expiry policy decide whether
            // it should remain in the map.  In particular, removing it here
            // would destroy a resumable player merely because a proxy dropped
            // the HTTP upgrade before the WebSocket task started.
            restore_failed_upgrade(&failed_session, &failed_pending, resumed);
        })
        .on_upgrade(move |socket| async move {
            let inbox = { pending.lock().unwrap().take() };
            let Some(inbox) = inbox else {
                // The failure callback won the one-shot cell.  Axum does not
                // call both callbacks, so this is only a defensive guard for
                // a custom upgrade adapter.
                return;
            };
            server.serve(session, inbox, socket, resumed).await;
        })
        .into_response()
}

/// Returns a claimed event receiver to a session when the HTTP upgrade failed
/// before [`Server::serve`] could take ownership of it.  The session is marked
/// disconnected first, so `resume: false` becomes immediately expired while a
/// resumable session keeps its configured grace period.
fn restore_failed_upgrade(
    session: &Arc<Session>,
    pending: &Mutex<Option<mpsc::Receiver<Value>>>,
    had_connected: bool,
) {
    let Some(inbox) = pending.lock().unwrap().take() else {
        return;
    };
    session.rollback_connect(had_connected);
    session.restore_inbox(inbox);
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
            session.restore_inbox(inbox);
            self.detach(&session);
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
        if let Ok(inbox) = result {
            session.restore_inbox(inbox);
        }
        self.detach(&session);
    }

    /// Detaches a socket from its session. A session that asked to be resumed
    /// is kept for its timeout; every other one is closed now.
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
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
    }

    fn reserve_player(&self) -> bool {
        let limit = u64::from(self.config.max_players);
        let mut count = self.players.load(Ordering::SeqCst);
        while count < limit {
            match self.players.compare_exchange_weak(
                count,
                count + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => count = actual,
            }
        }
        false
    }

    /// What `GET /api/v1/stats` reports.
    fn stats_body(&self) -> Value {
        let (session_count, playing) = {
            let sessions = self.sessions.lock().unwrap();
            let playing: usize =
                sessions.values().map(|session| session.playing_player_count()).sum();
            (sessions.len(), playing)
        };
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

    /// What `GET /metrics` reports, in the text exposition format Prometheus
    /// reads.
    fn metrics_body(&self) -> String {
        let sessions = self.sessions.lock().unwrap().len();
        let uptime = self.started.elapsed().as_secs_f64();
        let mut out = String::with_capacity(768);
        let mut metric = |name: &str, kind: &str, help: &str, value: u64| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}");
        };
        metric("rustlink_players", "gauge", "Current players", self.players.load(Ordering::SeqCst));
        metric("rustlink_sessions", "gauge", "Current sessions", sessions as u64);
        metric(
            "rustlink_http_requests_total",
            "counter",
            "HTTP requests",
            self.requests.load(Ordering::SeqCst),
        );
        metric(
            "rustlink_audio_frames_total",
            "counter",
            "Opus frames sent",
            self.counters.sent.load(Ordering::Relaxed),
        );
        metric(
            "rustlink_audio_deficit_total",
            "counter",
            "Missed frame deadlines",
            self.counters.deficit.load(Ordering::Relaxed),
        );
        metric(
            "rustlink_audio_errors_total",
            "counter",
            "Playback failures",
            self.counters.errors.load(Ordering::Relaxed),
        );
        metric(
            "rustlink_heap_bytes",
            "gauge",
            "Bytes this process holds; excludes media subprocesses",
            self.cpu.memory().0,
        );
        metric("rustlink_tasks", "gauge", "Background tasks", 0);
        let _ = writeln!(
            out,
            "# HELP rustlink_uptime_seconds Uptime in seconds\n# TYPE rustlink_uptime_seconds gauge\nrustlink_uptime_seconds {uptime:.3}"
        );
        out
    }

    /// Samples the machine and expires the sessions that have waited too long,
    /// which is what keeps a node that is left running from filling up with
    /// sessions nobody is coming back for.
    async fn reap(self: Arc<Self>) {
        if !self.sample_cpu().await {
            return;
        }
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = self.shutdown_requested() => return,
                _ = ticker.tick() => {
                    if !self.sample_cpu().await {
                        return;
                    }
                    let mut expired = Vec::new();
                    self.sessions.lock().unwrap().retain(|_, session| {
                        if session.expired() {
                            expired.push(Arc::clone(session));
                            false
                        } else {
                            true
                        }
                    });
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
                }
            }
        }
    }

    /// Sampling reads the process table, which is blocking system work (the
    /// first sample takes seconds on Windows) that must not stall a runtime
    /// worker. Returns false when the node shut down meanwhile.
    async fn sample_cpu(&self) -> bool {
        let cpu = Arc::clone(&self.cpu);
        tokio::select! {
            _ = self.shutdown_requested() => false,
            _ = tokio::task::spawn_blocking(move || cpu.sample()) => true,
        }
    }

    /// Posts the reports the node asks for. The node announces itself as soon
    /// as it is up, which is how a restart shows in a channel before anything
    /// is playing.
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
                        // Drain any coalesced signal arrived during the wait.
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

    /// Posts the state of every player on the node. Clients keep receiving
    /// their own `player.updated` per session; this is the node-wide summary an
    /// operator watches. Reports are spaced by the configured latency.
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
    if let Some(unsent) = session.take_unsent() {
        if send_frame(&mut sink, Message::Text(unsent.to_string().into())).await.is_err() {
            session.restore_unsent(unsent);
            return inbox;
        }
    }
    loop {
        tokio::select! {
            _ = session.stopped() => {
                while let Ok(event) = inbox.try_recv() {
                    if send_frame(&mut sink, Message::Text(event.to_string().into())).await.is_err() {
                        break;
                    }
                }
                let _ = send_frame(&mut sink, Message::Close(None)).await;
                break;
            }
            command = control.recv() => match command {
                Some(command) => if send_frame(&mut sink, command).await.is_err() { break },
                None => break,
            },
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
                for player in session.players() {
                    let (guild_id, state) = player.state_snapshot();
                    let payload = PlayerUpdated {
                        kind: "player.updated",
                        session_id: &session.id,
                        timestamp_ms: now_millis(),
                        guild_id,
                        state,
                    };
                    let Ok(text) = serde_json::to_string(&payload) else { continue };
                    if send_frame(&mut sink, Message::Text(text.into())).await.is_err() {
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
                    "sent": sent.saturating_sub(previous_sent),
                    "deficit": deficit.saturating_sub(previous_deficit),
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::Object;

    #[test]
    fn a_session_budget_is_keyed_by_the_session_in_the_path() {
        assert_eq!(session_key("/api/v1/sessions/abc123/players/42"), "abc123");
        assert_eq!(session_key("/api/v1/sessions/%61bc123/players/42"), "abc123");
        assert_eq!(session_key("/api/v1/sessions/abc123"), "abc123");
        assert_eq!(session_key("/api/v1/sessions"), "node", "creating a session has none yet");
        assert_eq!(session_key("/api/v1/stats"), "node");
        assert_eq!(session_key("/api/v1/sessions/"), "node");
        let long = format!("/api/v1/sessions/{}/players/1", "x".repeat(500));
        assert_eq!(session_key(&long).len(), 64, "an invented key stays short");
        let wide = format!("/api/v1/sessions/{}é/players/1", "a".repeat(63));
        assert_eq!(session_key(&wide), format!("{}é", "a".repeat(63)), "cut by characters");
    }

    #[test]
    fn the_caller_is_keyed_by_its_address_with_ipv6_reduced_to_its_prefix() {
        let key = |address: &str| {
            let address: std::net::IpAddr = address.parse().unwrap();
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(REMOTE.scope(Some(address), async { remote_key() }))
        };
        assert_eq!(key("203.0.113.9"), "203.0.113.9");
        assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), key("2001:db8:1:2::1"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        // Outside a request there is no address to key on.
        assert_eq!(remote_key(), "unknown");
    }

    #[test]
    fn budgets_are_spent_per_session_and_failed_logins_have_their_own() {
        let limits = Limits::new();
        let burst = Route::BUDGETS[Route::Stats as usize].1;
        for _ in 0..burst {
            assert!(limits.allow_authenticated(Route::Stats, "session-a"));
        }
        assert!(!limits.allow_authenticated(Route::Stats, "session-a"), "session-a is spent");
        assert!(limits.allow_authenticated(Route::Stats, "session-b"), "session-b is not");
        assert!(
            limits.allow_authenticated(Route::UpdatePlayer, "session-a"),
            "another route is not"
        );
        // Wrong passwords draw on a separate table, so they never touched the above.
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
    fn failed_upgrade_restores_a_resumable_session_and_its_inbox() {
        let session = Session::new("session-id".into(), "123".into(), std::sync::Weak::new(), 4);
        session.update(Some(true), Some(60));
        session.connect();
        let pending = pending_for(&session);
        session.emit(Object::new());

        restore_failed_upgrade(&session, &pending, true);

        assert!(!session.is_connected());
        assert!(session.has_connected());
        assert!(!session.expired());
        let mut inbox = session.take_inbox().expect("failed upgrade restored the inbox");
        assert!(inbox.try_recv().is_ok(), "events queued during the upgrade were lost");
        // The one-shot ownership cell is empty after the restoration.
        restore_failed_upgrade(&session, &pending, false);
        assert!(session.take_inbox().is_none());
    }

    #[test]
    fn failed_upgrade_keeps_resume_false_expired_for_normal_reaping() {
        let session = Session::new("session-id".into(), "123".into(), std::sync::Weak::new(), 4);
        session.update(Some(false), Some(60));
        session.connect();
        let pending = pending_for(&session);

        restore_failed_upgrade(&session, &pending, false);

        assert!(!session.is_connected());
        assert!(!session.has_connected());
        assert!(session.expired());
        assert!(session.take_inbox().is_some());
    }
}
