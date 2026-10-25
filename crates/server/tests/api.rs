//! Exercise the public API with in-memory media and voice, never real services.
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use rustlink_audio::{AudioError, Frames};
use rustlink_config::Config;
use rustlink_protocol::{encode_track, LoadResult, Object, Track, TrackInfo, VoiceState};
use rustlink_server::{Dependencies, Engine, Loader, Server};
use rustlink_source::SourceError;
use rustlink_voice::{CloseHandler, Connection, Factory, VoiceError};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tower::ServiceExt;

fn track() -> Track {
    encode_track(&TrackInfo {
        identifier: "test-track".into(),
        title: "測試 🎶".into(),
        source_name: "test".into(),
        is_seekable: true,
        length: 60_000,
        ..Default::default()
    })
    .unwrap()
}

struct Media;
impl Loader for Media {
    fn load(&self, query: &str) -> Result<LoadResult, SourceError> {
        Ok(match query {
            "empty" => LoadResult::Empty,
            "failure" => LoadResult::from_error("provider unavailable"),
            "search" => LoadResult::Search(vec![track()]),
            _ => LoadResult::Track(Box::new(track())),
        })
    }
    fn open(&self, _: &Track) -> Result<Box<dyn std::io::Read + Send>, SourceError> {
        Ok(Box::new(std::io::empty()))
    }
}
impl Engine for Media {
    fn start(&self, _: &Track, _: i64, _: i64, _: &Object) -> Result<Box<dyn Frames>, AudioError> {
        Ok(Box::new(Media))
    }
}
impl Frames for Media {
    fn next_frame(&mut self, _: &mut Vec<u8>) -> Result<bool, AudioError> {
        Ok(false)
    }
}
impl Factory for Media {
    fn connect(
        &self,
        _: String,
        _: String,
        _: VoiceState,
        _: CloseHandler,
    ) -> futures_util::future::BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>> {
        Box::pin(async { Ok(Arc::new(Media) as Arc<dyn Connection>) })
    }
}
impl Connection for Media {
    fn write(&self, _: &[u8]) -> Result<(), VoiceError> {
        Ok(())
    }
    fn speaking(&self, _: bool) -> Result<(), VoiceError> {
        Ok(())
    }
    fn connected(&self) -> bool {
        true
    }
    fn ping(&self) -> i64 {
        0
    }
    fn close(&self) {}
}

struct Node(Arc<Server>);
impl Node {
    fn new(mut config: Config) -> Self {
        config.password = "test-secret".into();
        config.ratelimits.enable = false;
        Self(
            Server::new(
                config,
                Dependencies {
                    loader: Arc::new(Media),
                    engine: Arc::new(Media),
                    voice: Arc::new(Media),
                    lyrics: None,
                    webhook: None,
                },
            )
            .unwrap(),
        )
    }
    async fn raw(
        &self,
        method: &str,
        path: &str,
        auth: &str,
        body: String,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut request =
            Request::builder().method(method).uri(path).header("content-type", "application/json");
        if !auth.is_empty() {
            request = request.header("authorization", auth);
        }
        let response =
            self.0.router().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 2 << 20).await.unwrap();
        let value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        (status, headers, value)
    }
    async fn request(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let (status, _, value) =
            self.raw(method, path, "Bearer test-secret", body.to_string()).await;
        (status, value)
    }
    async fn session(&self) -> String {
        let (status, session) =
            self.request("POST", "/api/v1/sessions", json!({"bot_id":"123"})).await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        session["session_id"].as_str().unwrap().to_owned()
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[tokio::test]
async fn source_capabilities_follow_native_provider_switches() {
    let node =
        Node::new(Config { youtube_native: false, soundcloud_native: false, ..Config::default() });
    let (status, info) = node.request("GET", "/api/v1/info", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["sources"], json!(["http", "deezer"]));

    let node = Node::new(Config::default());
    let (status, info) = node.request("GET", "/api/v1/info", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["sources"], json!(["http", "youtube", "soundcloud", "deezer"]));
}

#[tokio::test]
async fn native_auth_headers_errors_and_voice_capabilities() {
    let node = Node::new(Config::default());
    for auth in ["", "test-secret", "Bearer wrong"] {
        let (status, headers, body) = node.raw("GET", "/api/v1/info", auth, String::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers["rustlink-api-version"], "1");
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    let (status, info) = node.request("GET", "/api/v1/info", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["api_version"], "1");
    assert_eq!(info["capabilities"]["discord_dave"], true);
    assert_eq!(info["voice"]["gateway_version"], 8);
    assert_eq!(info["voice"]["max_dave_protocol_version"], 1);
    for path in ["/api/v1/unknown", "/api/v2/info", "/unknown"] {
        let (status, error) = node.request("GET", path, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["path"], path);
    }
    let (status, _, health) = node.raw("GET", "/healthz", "", String::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");
    let (status, _, error) =
        node.raw("POST", "/api/v1/sessions", "Bearer test-secret", "x".repeat((1 << 20) + 1)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(error["error"]["code"].is_string());
    let (status, error) = node.request("GET", "/api/v1/tracks/resolve", Value::Null).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(error["error"]["code"], "method_not_allowed");
}

#[tokio::test]
async fn resolve_inspect_and_provider_errors() {
    let node = Node::new(Config::default());
    let (status, result) =
        node.request("POST", "/api/v1/tracks/resolve", json!({"query":"song"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["kind"], "track");
    let token = result["result"]["token"].clone();
    assert!(token.as_str().unwrap().starts_with("rlt1."));
    let (status, decoded) =
        node.request("POST", "/api/v1/tracks/inspect", json!({"token": token})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decoded, result["result"]);
    let (status, batch) =
        node.request("POST", "/api/v1/tracks/inspect-batch", json!({"tokens":[token]})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch["tracks"][0], decoded);
    assert_eq!(
        node.request("POST", "/api/v1/lyrics", json!({"token":token})).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        node.request("POST", "/api/v1/tracks/resolve", json!({"query":"empty"})).await.1,
        json!({"kind":"empty", "result":null})
    );
    assert_eq!(
        node.request("POST", "/api/v1/tracks/resolve", json!({"query":"failure"})).await.0,
        StatusCode::BAD_GATEWAY
    );
    for body in [json!({"identifier":"song"}), json!({"query":" "}), json!({"query":1})] {
        assert_eq!(
            node.request("POST", "/api/v1/tracks/resolve", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        node.request("POST", "/api/v1/tracks/inspect", json!({"token":"QAAAgwM="})).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn rest_session_player_lifecycle_and_patch_semantics() {
    let node = Node::new(Config::default());
    let id = node.session().await;
    let session = format!("/api/v1/sessions/{id}");
    let player = format!("{session}/players/456");
    let (status, body) = node
        .request(
            "PATCH",
            &player,
            json!({"track":{"token":track().token,"user_data":{"requester":"abc"}}, "paused":true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["track"]["user_data"]["requester"], "abc");
    assert_eq!(
        node.request("GET", &format!("{session}/players"), Value::Null).await.1["players"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, body) =
        node.request("PATCH", &player, json!({"volume": 75, "position_ms": 3000})).await;
    assert_eq!(body["volume"], 75);
    assert_eq!(body["state"]["position_ms"], 3000);
    assert_eq!(body["paused"], true);
    for invalid in [
        json!({"volume":"25"}),
        json!({"unknown_track_field": "invalid"}),
        json!({"end_ms":0}),
        json!({"voice":{"token":"x"}}),
    ] {
        assert_eq!(node.request("PATCH", &player, invalid).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(node.request("GET", &player, Value::Null).await.1["volume"], 75);
    }
    let (_, preserved) =
        node.request("PATCH", &format!("{player}?replace=false"), json!({"track": null})).await;
    assert!(preserved["track"].is_object());
    let (_, stopped) = node.request("PATCH", &player, json!({"track":null, "end_ms": null})).await;
    assert!(stopped["track"].is_null());
    let (_, settings) = node.request("PATCH", &session, json!({"resume_timeout_secs":120})).await;
    assert_eq!(settings["resume_timeout_secs"], 120);
    for body in [json!({"resume":"yes"}), json!({"timeout":10}), json!({"resume_timeout_secs":0})] {
        assert_eq!(node.request("PATCH", &session, body).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(node.request("DELETE", &player, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(node.request("GET", &player, Value::Null).await.0, StatusCode::NOT_FOUND);
    assert_eq!(node.request("DELETE", &session, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(node.request("GET", &session, Value::Null).await.0, StatusCode::NOT_FOUND);
    assert_eq!(node.request("GET", "/api/v1/stats", Value::Null).await.1["players"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_player_creation_obeys_capacity_and_reuses_the_player() {
    let node = Arc::new(Node::new(Config { max_players: 1, ..Config::default() }));
    let id = node.session().await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let node = Arc::clone(&node);
        let path = format!("/api/v1/sessions/{id}/players/456");
        tasks.push(tokio::spawn(async move {
            node.request("PATCH", &path, json!({"volume":42})).await.0
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(node.request("GET", "/api/v1/stats", Value::Null).await.1["players"], 1);
    assert_eq!(
        node.request("PATCH", &format!("/api/v1/sessions/{id}/players/789"), json!({})).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    node.request("DELETE", &format!("/api/v1/sessions/{id}"), Value::Null).await;
    assert_eq!(node.request("GET", "/api/v1/stats", Value::Null).await.1["players"], 0);
}

#[tokio::test]
async fn sessions_expire_and_capacity_is_released() {
    let node = Node::new(Config { max_sessions: 1, ..Config::default() });
    let (_, session) = node
        .request("POST", "/api/v1/sessions", json!({"bot_id":"123", "resume_timeout_secs":1}))
        .await;
    assert_eq!(
        node.request("POST", "/api/v1/sessions", json!({"bot_id":"123"})).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let path = format!("/api/v1/sessions/{}", session["session_id"].as_str().unwrap());
    assert_eq!(node.request("GET", &path, Value::Null).await.0, StatusCode::NOT_FOUND);
    node.session().await;
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn event(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                other => panic!("unexpected message {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_ready_queue_resume_conflict_and_shutdown() {
    let node = Node::new(Config::default());
    let id = node.session().await;
    let path = format!("/api/v1/sessions/{id}");
    // Queue an event before attaching, to verify ready is always sent first.
    node.request("PATCH", &format!("{path}/players/456"), json!({"track":{"token":track().token}}))
        .await;
    node.request("PATCH", &format!("{path}/players/456"), json!({"track":null})).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = node.0.router();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let request = || {
        let mut request = format!("ws://{address}{path}/events").into_client_request().unwrap();
        request.headers_mut().insert("authorization", "Bearer test-secret".parse().unwrap());
        request
    };
    let (mut socket, _) = tokio_tungstenite::connect_async(request()).await.unwrap();
    let ready = event(&mut socket).await;
    assert_eq!(ready["type"], "session.ready");
    assert_eq!(ready["resumed"], false);
    assert_eq!(ready["session_id"], id);
    let ended = event(&mut socket).await;
    assert_eq!(ended["type"], "track.ended");
    assert_eq!(ended["session_id"], id);
    assert!(ended.get("op").is_none());
    match tokio_tungstenite::connect_async(request()).await.unwrap_err() {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), StatusCode::CONFLICT)
        }
        error => panic!("{error}"),
    }
    socket.send(Message::Text("{}".into())).await.unwrap();
    assert_eq!(event(&mut socket).await["error"]["code"], "rest_required");
    socket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if node.request("GET", &path, Value::Null).await.1["connected"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        node.request("PATCH", &format!("{path}/players/456"), json!({"paused":true})).await.0,
        StatusCode::OK
    );
    let (mut resumed, _) = tokio_tungstenite::connect_async(request()).await.unwrap();
    let ready = event(&mut resumed).await;
    assert_eq!(ready["resumed"], true);
    assert_eq!(ready["players"][0]["paused"], true);
    node.request("DELETE", &path, Value::Null).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(message)) = resumed.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_client_drives_the_real_http_and_event_contract() {
    let node = Node::new(Config::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = node.0.router();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = rustlink_client::Client::new(&format!("http://{address}"), "test-secret").unwrap();
    let session = client.create_session("123", 60).await.unwrap();
    let mut events = session.events().await.unwrap();
    assert_eq!(events.next().await.unwrap().unwrap()["type"], "session.ready");
    let track = client.resolve("song").await.unwrap().track().unwrap();
    assert_eq!(client.inspect(&track.token).await.unwrap(), track);
    assert!(client.lyrics(&track.token).await.unwrap().is_none());
    session.play("456", &track.token).await.unwrap();
    assert!(session.pause("456", true).await.unwrap().paused);
    assert_eq!(session.seek("456", 5000).await.unwrap().state.position, 5000);
    assert_eq!(session.set_volume("456", 80).await.unwrap().volume, 80);
    assert_eq!(session.players().await.unwrap().len(), 1);
    assert!(session.stop("456").await.unwrap().track.is_none());
    let ended = tokio::time::timeout(Duration::from_secs(3), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ended["type"], "track.ended");
    assert!(client.session(session.id()).await.unwrap().info().await.unwrap().connected);
    session.destroy_player("456").await.unwrap();
    match session.player("456").await.unwrap_err() {
        rustlink_client::Error::Api { status, code, .. } => {
            assert_eq!(status, 404);
            assert_eq!(code, "not_found");
        }
        error => panic!("unexpected client error: {error}"),
    }
    events.close().await.unwrap();
    session.destroy().await.unwrap();
    task.abort();
}
