//! One client connection: the players it owns and the events it is told about.
//!
//! REST creates the session. Its event socket may disconnect for the configured
//! grace period; players keep running and queued events survive reconnection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use rustlink_protocol::{Object, Player as PlayerBody};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::player::Player;
use crate::Server;

/// How long a session waits to be resumed when the client does not say. The
/// client can change it with a `PATCH` on the session.
const DEFAULT_TIMEOUT: u32 = 60;

struct State {
    players: HashMap<String, Arc<Player>>,
    /// Whether the client asked for its session to survive a disconnect.
    resuming: bool,
    /// How long the session is kept after a disconnect, in seconds.
    timeout: u32,
    connected: bool,
    has_connected: bool,
    closed: bool,
    disconnected: Option<Instant>,
}

pub struct Session {
    pub id: String,
    pub user: String,
    server: Weak<Server>,
    state: Mutex<State>,
    /// Events waiting for the socket. A client that stops reading fills this
    /// and is then closed, because a node that drops events silently leaves a
    /// client showing something that is not happening.
    events: mpsc::Sender<Value>,
    /// The receiving half, which belongs to the socket draining it. It is
    /// parked here while a session has no socket, so the events that arrive in
    /// the meantime are waiting for the client when it resumes.
    inbox: Mutex<Option<mpsc::Receiver<Value>>>,
    /// How many events may wait, which the overflow warning names.
    buffer: usize,
    /// Set when the buffer overflowed, so the warning and the close happen
    /// once.
    overflowed: AtomicBool,
    /// Wakes the writer when the session is destroyed.
    stop: Notify,
}

impl Session {
    /// Builds a session around the queue its socket will drain. The receiving
    /// half stays inside the session, so a client that resumes can pick up the
    /// events that arrived while it was away.
    pub fn new(id: String, user: String, server: Weak<Server>, buffer: usize) -> Arc<Session> {
        let (events, inbox) = mpsc::channel(buffer.max(1));
        Arc::new(Session {
            id,
            user,
            server,
            state: Mutex::new(State {
                players: HashMap::new(),
                resuming: true,
                timeout: DEFAULT_TIMEOUT,
                connected: false,
                has_connected: false,
                closed: false,
                disconnected: Some(Instant::now()),
            }),
            events,
            inbox: Mutex::new(Some(inbox)),
            buffer,
            overflowed: AtomicBool::new(false),
            stop: Notify::new(),
        })
    }

    /// Hands the queue to the socket that is about to drain it. A session has
    /// one socket at a time, so a second caller gets nothing.
    pub fn take_inbox(&self) -> Option<mpsc::Receiver<Value>> {
        self.inbox.lock().unwrap().take()
    }

    /// Puts the queue back once the socket is gone, so a client that resumes
    /// finds the events that were sent while it was disconnected.
    pub fn restore_inbox(&self, inbox: mpsc::Receiver<Value>) {
        *self.inbox.lock().unwrap() = Some(inbox);
    }

    /// The callback a player delivers its events through. It holds the session
    /// weakly, so a player that outlives its session cannot keep it alive.
    pub fn emitter(self: &Arc<Self>) -> crate::Emit {
        let session = Arc::downgrade(self);
        Arc::new(move |event| {
            if let Some(session) = session.upgrade() {
                session.emit(event);
            }
        })
    }

    /// Sends one event to the client, and to the webhook if one is configured.
    pub fn emit(&self, mut event: Object) {
        event.insert("session_id".into(), json!(self.id));
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        log::debug!(target: "rustlink::event", "event dispatched {kind} session={} guild={}", self.id, event.get("guild_id").and_then(Value::as_str).unwrap_or(""));
        let value = Value::Object(event);
        if let Some(server) = self.server.upgrade() {
            server.publish(&value);
        }
        if self.state.lock().unwrap().closed {
            return;
        }
        if self.events.try_send(value).is_err() && !self.overflowed.swap(true, Ordering::SeqCst) {
            log::warn!(
                "event buffer overflow; closing session because the client is not reading session={} buffer={}",
                self.id,
                self.buffer
            );
            if let Some(server) = self.server.upgrade() {
                let id = self.id.clone();
                // The node is moved into the task rather than borrowed from, so
                // the task can outlive this call without holding the session.
                let owner = Arc::clone(&server);
                server.spawn(async move { owner.remove_session(&id) });
            }
        }
    }

    /// What every player on the session reports.
    pub fn snapshots(&self) -> Vec<PlayerBody> {
        self.players().iter().map(|player| player.snapshot()).collect()
    }

    /// The players the session owns.
    pub fn players(&self) -> Vec<Arc<Player>> {
        self.state.lock().unwrap().players.values().cloned().collect()
    }

    /// One player by the guild it serves.
    pub fn player(&self, guild: &str) -> Option<Arc<Player>> {
        self.state.lock().unwrap().players.get(guild).cloned()
    }

    /// Serializes creation against other requests and session destruction.
    /// `None` means the session was destroyed while the request was in flight.
    pub fn get_or_insert<E>(
        &self,
        guild: &str,
        create: impl FnOnce() -> Result<Arc<Player>, E>,
    ) -> Result<Option<Arc<Player>>, E> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Ok(None);
        }
        if let Some(player) = state.players.get(guild) {
            return Ok(Some(Arc::clone(player)));
        }
        let player = create()?;
        state.players.insert(guild.to_owned(), Arc::clone(&player));
        Ok(Some(player))
    }

    /// Removes a player and reports whether there was one.
    pub fn remove(&self, guild: &str) -> Option<Arc<Player>> {
        self.state.lock().unwrap().players.remove(guild)
    }

    pub fn has_connected(&self) -> bool {
        self.state.lock().unwrap().has_connected
    }

    pub fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({ "session_id": self.id, "bot_id": self.user, "connected": state.connected,
            "resume": state.resuming, "resume_timeout_secs": state.timeout,
            "event_url": format!("/api/v1/sessions/{}/events", self.id) })
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub fn resuming(&self) -> bool {
        self.state.lock().unwrap().resuming
    }

    pub fn timeout(&self) -> u32 {
        self.state.lock().unwrap().timeout
    }

    /// The settings a `PATCH` on the session changes, and the answer it gets.
    pub fn update(&self, resuming: Option<bool>, timeout: Option<u32>) -> Value {
        let mut state = self.state.lock().unwrap();
        if let Some(resuming) = resuming {
            state.resuming = resuming;
        }
        if let Some(timeout) = timeout {
            state.timeout = timeout;
        }
        json!({ "resume": state.resuming, "resume_timeout_secs": state.timeout })
    }

    pub fn connect(&self) {
        let mut state = self.state.lock().unwrap();
        state.connected = true;
        state.has_connected = true;
        state.disconnected = None;
    }

    pub fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        state.connected = false;
        state.disconnected = Some(Instant::now());
    }

    /// How long the session has been waiting to be resumed.
    pub fn disconnected_for(&self) -> Option<std::time::Duration> {
        self.state.lock().unwrap().disconnected.map(|at| at.elapsed())
    }

    /// Whether the session has been away for longer than it was told to wait.
    pub fn expired(&self) -> bool {
        let state = self.state.lock().unwrap();
        if state.connected {
            return false;
        }
        if !state.resuming {
            return true;
        }
        match state.disconnected {
            Some(at) => at.elapsed().as_secs() >= state.timeout as u64,
            None => true,
        }
    }

    /// Tears the session down, ending every track it owns.
    pub fn destroy(&self) {
        let players = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.players)
        };
        // Notifying rather than notifying waiters leaves a permit behind, so a
        // writer that starts after the session was destroyed sees it at once.
        self.stop.notify_one();
        for (_, player) in players {
            player.destroy();
            if let Some(server) = self.server.upgrade() {
                server.player_destroyed();
            }
        }
    }

    /// Wakes the writer loop, which then sees the session is closed and stops.
    pub fn wake(&self) {
        self.stop.notify_one();
    }

    /// Waits until the session is destroyed.
    pub async fn stopped(&self) {
        self.stop.notified().await;
    }
}
