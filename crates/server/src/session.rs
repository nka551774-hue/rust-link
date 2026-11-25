//! Client session state: owned players, per-guild update gates, and buffered events.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use rustlink_protocol::{Object, Player as PlayerBody};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::player::Player;
use crate::Server;

const DEFAULT_TIMEOUT: u32 = 60;

struct State {
    players: HashMap<String, Arc<Player>>,
    updates: HashMap<String, Weak<tokio::sync::Mutex<()>>>,
    resuming: bool,
    timeout: u32,
    connected: bool,
    has_connected: bool,
    closing: bool,
    closed: bool,
    disconnected: Option<Instant>,
}

impl State {
    fn apply_config(&mut self, resuming: Option<bool>, timeout: Option<u32>) {
        if let Some(resuming) = resuming {
            self.resuming = resuming;
        }
        if let Some(timeout) = timeout {
            self.timeout = timeout;
        }
    }

    fn expired(&self) -> bool {
        !self.connected
            && (!self.resuming
                || self
                    .disconnected
                    .is_none_or(|at| at.elapsed().as_secs() >= u64::from(self.timeout)))
    }

    fn is_usable(&self) -> bool {
        !self.closing && !self.closed && !self.expired()
    }
}

pub struct Session {
    pub id: String,
    pub user: String,
    server: Weak<Server>,
    state: Mutex<State>,
    events: mpsc::Sender<Value>,
    inbox: Mutex<Option<mpsc::Receiver<Value>>>,
    unsent: Mutex<Option<Value>>,
    buffer: usize,
    overflowed: AtomicBool,
    stop: Notify,
}

/// Serializes creation, update, and rollback per guild while sweeping idle gates on drop.
pub(crate) struct PlayerUpdateGate<'a> {
    session: &'a Session,
    guild: String,
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl PlayerUpdateGate<'_> {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.as_ref().expect("gate is live until dropped").lock().await
    }
}

impl Drop for PlayerUpdateGate<'_> {
    fn drop(&mut self) {
        let mut state = self.session.state.lock().unwrap();
        if self.lock.take().is_some_and(|lock| Arc::strong_count(&lock) == 1) {
            state.updates.remove(&self.guild);
        }
    }
}

impl Session {
    pub fn new(id: String, user: String, server: Weak<Server>, buffer: usize) -> Arc<Session> {
        let (events, inbox) = mpsc::channel(buffer.max(1));
        Arc::new(Session {
            id,
            user,
            server,
            state: Mutex::new(State {
                players: HashMap::new(),
                updates: HashMap::new(),
                resuming: true,
                timeout: DEFAULT_TIMEOUT,
                connected: false,
                has_connected: false,
                closing: false,
                closed: false,
                disconnected: Some(Instant::now()),
            }),
            events,
            inbox: Mutex::new(Some(inbox)),
            unsent: Mutex::new(None),
            buffer,
            overflowed: AtomicBool::new(false),
            stop: Notify::new(),
        })
    }

    pub fn take_inbox(&self) -> Option<mpsc::Receiver<Value>> {
        self.inbox.lock().unwrap().take()
    }

    pub fn restore_inbox(&self, inbox: mpsc::Receiver<Value>) {
        *self.inbox.lock().unwrap() = Some(inbox);
    }

    pub fn take_unsent(&self) -> Option<Value> {
        self.unsent.lock().unwrap().take()
    }

    pub fn restore_unsent(&self, event: Value) {
        *self.unsent.lock().unwrap() = Some(event);
    }

    pub fn emitter(self: &Arc<Self>) -> crate::Emit {
        let session = Arc::downgrade(self);
        Arc::new(move |event| {
            if let Some(session) = session.upgrade() {
                session.emit(event);
            }
        })
    }

    pub fn emit(&self, mut event: Object) {
        let (closing, closed) = {
            let state = self.state.lock().unwrap();
            (state.closing, state.closed)
        };
        if closed {
            return;
        }
        event.insert("session_id".into(), json!(self.id));
        log::debug!(
            target: "rustlink::event",
            "event dispatched {} session={} guild={}",
            event.get("type").and_then(Value::as_str).unwrap_or(""),
            self.id,
            event.get("guild_id").and_then(Value::as_str).unwrap_or("")
        );
        let value = Value::Object(event);
        let server = self.server.upgrade();
        if let Some(server) = &server {
            server.publish(&value);
        }
        if self.events.try_send(value).is_err()
            && !closing
            && !self.overflowed.swap(true, Ordering::SeqCst)
        {
            log::warn!(
                "event buffer overflow; closing session because the client is not reading session={} buffer={}",
                self.id,
                self.buffer
            );
            if let Some(owner) = server {
                let id = self.id.clone();
                let spawner = Arc::clone(&owner);
                spawner.spawn(async move { owner.remove_session(&id) });
            }
        }
    }

    pub fn snapshots(&self) -> Vec<PlayerBody> {
        self.players().iter().map(|player| player.snapshot()).collect()
    }

    pub fn player_count(&self) -> usize {
        self.state.lock().unwrap().players.len()
    }

    pub fn playing_player_count(&self) -> usize {
        self.usable_and_playing().unwrap_or(0)
    }

    pub(crate) fn usable_and_playing(&self) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.is_usable().then(|| state.players.values().filter(|p| p.playing()).count())
    }

    pub(crate) fn collect_reports(&self, out: &mut Vec<Value>) -> usize {
        let mut playing = 0;
        for player in self.players() {
            let (is_playing, entry) = player.report_state();
            playing += usize::from(is_playing);
            out.push(entry);
        }
        playing
    }

    pub fn players(&self) -> Vec<Arc<Player>> {
        self.state.lock().unwrap().players.values().cloned().collect()
    }

    pub fn player(&self, guild: &str) -> Option<Arc<Player>> {
        self.state.lock().unwrap().players.get(guild).cloned()
    }

    pub(crate) fn update_gate(&self, guild: &str) -> PlayerUpdateGate<'_> {
        let mut state = self.state.lock().unwrap();
        let lock = state.updates.get(guild).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            state.updates.insert(guild.to_owned(), Arc::downgrade(&lock));
            lock
        });
        PlayerUpdateGate { session: self, guild: guild.to_owned(), lock: Some(lock) }
    }

    pub fn get_or_insert<E>(
        &self,
        guild: &str,
        create: impl FnOnce() -> Result<Arc<Player>, E>,
    ) -> Result<Option<(Arc<Player>, bool)>, E> {
        let mut state = self.state.lock().unwrap();
        if state.closing || state.closed {
            return Ok(None);
        }
        if let Some(player) = state.players.get(guild) {
            return Ok(Some((Arc::clone(player), false)));
        }
        let player = create()?;
        state.players.insert(guild.to_owned(), Arc::clone(&player));
        Ok(Some((player, true)))
    }

    pub fn remove_if_same(&self, guild: &str, expected: &Arc<Player>) -> Option<Arc<Player>> {
        let mut state = self.state.lock().unwrap();
        Arc::ptr_eq(state.players.get(guild)?, expected).then(|| state.players.remove(guild))?
    }

    pub fn remove(&self, guild: &str) -> Option<Arc<Player>> {
        self.state.lock().unwrap().players.remove(guild)
    }

    pub fn has_connected(&self) -> bool {
        self.state.lock().unwrap().has_connected
    }

    pub fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({
            "session_id": self.id,
            "bot_id": self.user,
            "connected": state.connected,
            "resume": state.resuming,
            "resume_timeout_secs": state.timeout,
            "event_url": format!("/api/v1/sessions/{}/events", self.id),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    pub fn is_closed(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.closing || state.closed
    }

    pub fn resuming(&self) -> bool {
        self.state.lock().unwrap().resuming
    }

    pub fn timeout(&self) -> u32 {
        self.state.lock().unwrap().timeout
    }

    pub(crate) fn configure(&self, resuming: Option<bool>, timeout: Option<u32>) {
        self.state.lock().unwrap().apply_config(resuming, timeout);
    }

    pub fn update(&self, resuming: Option<bool>, timeout: Option<u32>) -> Value {
        let mut state = self.state.lock().unwrap();
        state.apply_config(resuming, timeout);
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

    pub(crate) fn rollback_connect(&self, had_connected: bool) {
        let mut state = self.state.lock().unwrap();
        state.connected = false;
        state.has_connected = had_connected;
        state.disconnected = Some(Instant::now());
    }

    pub fn disconnected_for(&self) -> Option<Duration> {
        self.state.lock().unwrap().disconnected.map(|at| at.elapsed())
    }

    pub fn expired(&self) -> bool {
        self.state.lock().unwrap().expired()
    }

    pub(crate) fn is_usable(&self) -> bool {
        self.state.lock().unwrap().is_usable()
    }

    pub(crate) fn claim_subscriber(&self) -> Result<Option<bool>, ()> {
        let mut state = self.state.lock().unwrap();
        if !state.is_usable() {
            return Ok(None);
        }
        if state.connected {
            return Err(());
        }
        let had_connected = state.has_connected;
        state.connected = true;
        state.has_connected = true;
        state.disconnected = None;
        Ok(Some(had_connected))
    }

    pub(crate) fn detach_info(&self) -> (bool, usize, bool, u32) {
        let mut state = self.state.lock().unwrap();
        state.connected = false;
        state.disconnected = Some(Instant::now());
        (state.closing || state.closed, state.players.len(), state.resuming, state.timeout)
    }

    pub fn destroy(&self) {
        let players = {
            let mut state = self.state.lock().unwrap();
            if state.closing || state.closed {
                return;
            }
            state.closing = true;
            std::mem::take(&mut state.players)
        };
        let server = self.server.upgrade();
        for (_, player) in players {
            player.destroy();
            if let Some(server) = &server {
                server.player_destroyed();
            }
        }
        self.state.lock().unwrap().closed = true;
        self.stop.notify_one();
    }

    pub fn wake(&self) {
        self.stop.notify_one();
    }

    pub async fn stopped(&self) {
        self.stop.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guild_update_gates_serialize_waiters_without_blocking_other_guilds() {
        let session = Session::new("session".into(), "1".into(), Weak::new(), 4);
        let first = session.update_gate("10");
        let first_lock = first.lock().await;
        let waiting = session.update_gate("10");
        assert!(waiting.lock.as_ref().unwrap().try_lock().is_err());
        let other = session.update_gate("20");
        assert!(other.lock.as_ref().unwrap().try_lock().is_ok());
        drop(other);
        drop(first_lock);
        drop(first);
        assert_eq!(session.state.lock().unwrap().updates.len(), 1);
        let next_lock = waiting.lock().await;
        drop(next_lock);
        drop(waiting);
        assert!(session.state.lock().unwrap().updates.is_empty());
    }
}
