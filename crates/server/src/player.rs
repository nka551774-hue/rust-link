//! One player: the track it is on, the voice connection it sends to, and the
//! events it reports.
//!
//! A player is a small state machine behind one mutex plus a thread that paces
//! frames out of the decoder. Everything a client can change goes through the
//! update path, which is serialized, so two updates never interleave half-way
//! through a track change.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustlink_audio::{AudioError, Frames};
use rustlink_protocol::{
    decode_track, Object, Patch, Player as PlayerBody, PlayerUpdate, Track, VoiceState,
};
use rustlink_source::LoadResult;
use rustlink_voice::{Closed, Connection, Factory};
use serde_json::json;

use crate::playback::{Engine, Loader};
use crate::{Counters, Emit};

/// The length of one Opus frame, which is what paces playback.
const FRAME_MILLIS: f64 = 20.0;

/// How late a frame may be before the deficit counter notices.
const LATE: Duration = Duration::from_millis(100);

/// The stall threshold the node reports in a `track.stalled`. The engine
/// applies the same limit, and a client is told what it was.
const STUCK_MILLIS: i64 = 10_000;

/// The connection frames are written to, and whether that connection has been
/// told the node is speaking.
///
/// Discord drops audio that arrives before a speaking announcement, so the flag
/// has to travel with the connection it belongs to.
struct VoiceOutput<'a> {
    connection: &'a Arc<dyn Connection>,
    speaking: bool,
}

/// Why an update could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    /// The request itself is not something a player can be asked to do.
    #[error("{0}")]
    Invalid(String),
    /// The identifier could not be resolved.
    #[error("{0}")]
    Source(#[from] rustlink_source::SourceError),
    /// The player was destroyed while the update was in flight.
    #[error("player is destroyed")]
    Destroyed,
    /// An identifier resolved to something other than one track.
    #[error("identifier must resolve to one track")]
    NotATrack,
}

/// The dependencies a player is built with, shared by every player on the node.
pub struct Deps {
    pub loader: Arc<dyn Loader>,
    pub engine: Arc<dyn Engine>,
    pub voice: Arc<dyn Factory>,
}

/// Everything the player owns, behind one lock.
struct State {
    /// The body a client reads back and a `player.updated` reports.
    body: PlayerBody,
    /// The position playback stops at, when the client set one.
    end_time: Option<i64>,
    /// Whether the current track has been announced as started.
    started: bool,
    /// Whether the player has been destroyed.
    closed: bool,
    /// Whether a thread is currently pacing frames.
    running: bool,
    /// Bumped whenever playback is stopped, so a thread that is still unwinding
    /// knows it is no longer the current one.
    generation: u64,
    /// The voice connection audio is written to.
    connection: Option<Arc<dyn Connection>>,
    /// Bumped whenever a connection is opened, so a handshake that finishes
    /// late is discarded rather than adopted.
    voice_generation: u64,
    /// Whether a join has been announced and no leave has been announced for
    /// it yet, so a dead connection is reported once.
    voice_up: bool,
}

pub struct Player {
    user: String,
    guild: String,
    deps: Arc<Deps>,
    emit: Emit,
    counters: Arc<Counters>,
    state: Mutex<State>,
    /// Wakes the playback thread when the state it waits on changes.
    wake: Condvar,
    /// Serializes updates, so a slow source lookup cannot be overtaken by the
    /// next request for the same player.
    updates: tokio::sync::Mutex<()>,
}

impl Player {
    /// Builds a player for a guild. It has no track and no voice connection
    /// until a client asks for one.
    pub fn new(
        user: String,
        guild: String,
        deps: Arc<Deps>,
        emit: Emit,
        counters: Arc<Counters>,
    ) -> Arc<Player> {
        Arc::new(Player {
            user,
            guild: guild.clone(),
            deps,
            emit,
            counters,
            state: Mutex::new(State {
                body: PlayerBody::new(guild),
                end_time: None,
                started: false,
                closed: false,
                running: false,
                generation: 0,
                connection: None,
                voice_generation: 0,
                voice_up: false,
            }),
            wake: Condvar::new(),
            updates: tokio::sync::Mutex::new(()),
        })
    }

    /// The guild the player serves.
    pub fn guild(&self) -> &str {
        &self.guild
    }

    /// What the client is told about the player.
    pub fn snapshot(&self) -> PlayerBody {
        let state = self.state.lock().unwrap();
        self.snapshot_locked(&state)
    }

    /// Whether the player is playing, which is what the node's counters ask of
    /// every player it has.
    ///
    /// It reads the three fields the answer needs rather than building the body
    /// a client is sent, because a stats request asks this of every player on
    /// the node.
    pub fn playing(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.body.track.is_some()
            && !state.body.paused
            && state.connection.as_ref().is_some_and(|connection| connection.connected())
    }

    fn snapshot_locked(&self, state: &State) -> PlayerBody {
        let mut body = state.body.clone();
        if let Some(track) = body.track.as_mut() {
            track.metadata.position = body.state.position;
        }
        body.state.time = now_millis();
        body.state.connected =
            state.connection.as_ref().map(|connection| connection.connected()).unwrap_or(false);
        body.state.ping = if body.state.connected {
            state.connection.as_ref().map(|connection| connection.ping()).unwrap_or(-1)
        } else {
            -1
        };
        body
    }
    /// Applies a player update.
    ///
    /// The update is serialized against every other update for this player, so
    /// the result a client sees is the result of its own request and nothing
    /// that arrived in between.
    pub async fn update(
        self: &Arc<Self>,
        update: PlayerUpdate,
        no_replace: bool,
    ) -> Result<PlayerBody, PlayerError> {
        let _serialized = self.updates.lock().await;
        validate(&update)?;
        let change = update.track_update();
        let changes_track = !change.token.is_missing() || change.query.is_some();
        let ignore = {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(PlayerError::Destroyed);
            }
            no_replace && state.body.track.is_some()
        };
        let mut track: Option<Track> = None;
        if changes_track && !ignore {
            if let Some(identifier) = change.query.clone() {
                track = Some(self.resolve(identifier).await?);
            } else if let Some(encoded) = change.token.value() {
                track = Some(
                    decode_track(encoded)
                        .map_err(|error| PlayerError::Invalid(error.to_string()))?,
                );
            }
        }
        let mut connect = None;
        let result = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(PlayerError::Destroyed);
            }
            let mut restart = update.volume.is_some_and(|volume| volume.get() != state.body.volume)
                || update.filters.as_ref().is_some_and(|filters| *filters != state.body.filters);
            if changes_track && !ignore {
                self.stop(&mut state);
                if let Some(previous) = state.body.track.take() {
                    let reason = if track.is_some() { "replaced" } else { "stopped" };
                    self.event("track.ended", json!({ "track": previous, "reason": reason }));
                }
                state.body.state.position = match &track {
                    Some(next) => next.metadata.position,
                    None => 0,
                };
                state.body.track = track;
                state.started = false;
                state.end_time = None;
                restart = true;
            }
            if let Some(user_data) = &change.user_data {
                if !ignore {
                    if let Some(current) = state.body.track.as_mut() {
                        current.user_data = user_data.clone();
                    }
                }
            }
            if let Some(position) = update.position {
                if let Some(current) = state.body.track.as_ref() {
                    if current.metadata.is_seekable {
                        let mut target = position.get();
                        if current.metadata.length > 0 && target > current.metadata.length {
                            target = current.metadata.length;
                        }
                        restart |= target != state.body.state.position;
                        state.body.state.position = target;
                    }
                }
            }
            match &update.end_time {
                Patch::Missing => {}
                Patch::Null => state.end_time = None,
                Patch::Value(end_time) => state.end_time = Some(end_time.get()),
            }
            if let Some(volume) = update.volume {
                state.body.volume = volume.get();
            }
            if changes_track && !ignore {
                // A new track starts unless the same request explicitly pauses it.
                state.body.paused = update.paused.unwrap_or(false);
            } else if let Some(paused) = update.paused {
                state.body.paused = paused;
            }
            if let Some(filters) = &update.filters {
                state.body.filters = filters.clone();
            }
            if restart {
                self.stop(&mut state);
            }
            if let Some(voice) = &update.voice {
                // The channel being left is the one the previous voice state
                // named, which the update has not overwritten yet.
                let left = state.body.voice.channel_id.clone();
                state.body.voice = voice.clone();
                self.stop(&mut state);
                if let Some(connection) = state.connection.take() {
                    connection.close();
                }
                if state.voice_up {
                    state.voice_up = false;
                    self.event("voice.disconnected", voice_fields(left.as_deref(), "replaced"));
                }
                state.voice_generation += 1;
                connect = Some((state.voice_generation, voice.clone()));
            }
            self.start(&mut state);
            self.wake.notify_all();
            self.snapshot_locked(&state)
        };
        if let Some((generation, voice)) = connect {
            self.open_voice(generation, voice);
        }
        Ok(result)
    }

    /// Resolves an identifier on a thread of its own, because it is network
    /// work and the runtime must stay free to answer other clients meanwhile.
    async fn resolve(&self, identifier: String) -> Result<Track, PlayerError> {
        let loader = self.deps.loader.clone();
        let result = tokio::task::spawn_blocking(move || loader.load_shared(&identifier))
            .await
            .map_err(|error| PlayerError::Invalid(error.to_string()))??;
        match result.as_ref() {
            LoadResult::Track(track) => Ok(track.as_ref().clone()),
            _ => Err(PlayerError::NotATrack),
        }
    }

    /// Opens a voice connection for a guild, and adopts it if it is still the
    /// one the player is waiting for.
    fn open_voice(self: &Arc<Self>, generation: u64, voice: VoiceState) {
        let weak: Weak<Player> = Arc::downgrade(self);
        let handler = Box::new(move |closed: Closed| {
            if let Some(player) = weak.upgrade() {
                player.connection_closed(generation, closed);
            }
        });
        let future = self.deps.voice.connect(self.user.clone(), self.guild.clone(), voice, handler);
        let player = Arc::clone(self);
        tokio::spawn(async move {
            let result = future.await;
            player.connected(generation, result);
        });
    }

    fn connected(
        self: &Arc<Self>,
        generation: u64,
        result: Result<Arc<dyn Connection>, rustlink_voice::VoiceError>,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.voice_generation != generation {
            drop(state);
            if let Ok(connection) = result {
                connection.close();
            }
            return;
        }
        match result {
            Ok(connection) => {
                // A connection that reports itself ready is the moment the bot
                // is actually in the channel, which is what a listener wants to
                // know; one that died while it was being opened never gets
                // announced.
                if connection.connected() {
                    state.voice_up = true;
                    let channel = state.body.voice.channel_id.clone();
                    self.event("voice.connected", voice_fields(channel.as_deref(), ""));
                }
                state.connection = Some(connection);
                self.start(&mut state);
            }
            Err(error) => {
                self.event(
                    "voice.closed",
                    json!({ "code": 4000, "reason": error.to_string(), "by_remote": false }),
                );
            }
        }
    }
    /// Reports a voice connection that ended, whichever side ended it.
    fn connection_closed(&self, generation: u64, closed: Closed) {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.voice_generation != generation {
            return;
        }
        self.stop(&mut state);
        // The bot leaving is reported before the protocol event that explains
        // the close, so a channel reads as a departure followed by its cause.
        if state.voice_up {
            state.voice_up = false;
            let channel = state.body.voice.channel_id.clone();
            let mut fields = voice_fields(channel.as_deref(), "closed");
            fields["code"] = json!(closed.code);
            fields["by_remote"] = json!(closed.by_remote);
            self.event("voice.disconnected", fields);
        }
        self.event(
            "voice.closed",
            json!({ "code": closed.code, "reason": closed.reason, "by_remote": closed.by_remote }),
        );
    }

    /// Stops pacing frames. The thread notices at its next frame, which is at
    /// most one frame away.
    fn stop(&self, state: &mut State) {
        state.generation += 1;
        state.running = false;
        self.wake.notify_all();
        if let Some(connection) = state.connection.as_ref() {
            if connection.connected() {
                let _ = connection.speaking(false);
            }
        }
    }

    /// Starts pacing frames, if there is a track, a live connection and no
    /// thread already doing it.
    fn start(self: &Arc<Self>, state: &mut State) {
        let Some(track) = state.body.track.clone() else {
            return;
        };
        let Some(connection) = state.connection.clone() else {
            return;
        };
        if state.closed || state.running || !connection.connected() {
            return;
        }
        state.generation += 1;
        state.running = true;
        let generation = state.generation;
        let position = state.body.state.position;
        let volume = state.body.volume;
        let filters = state.body.filters.clone();
        let player = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name(format!("player-{}", self.guild))
            .spawn(move || player.run(generation, track, position, volume, filters, connection));
        if spawned.is_err() {
            state.running = false;
        }
    }

    /// Paces frames out of the decoder until the track ends or the player is
    /// stopped.
    fn run(
        self: Arc<Self>,
        generation: u64,
        track: Track,
        position: i64,
        volume: i64,
        filters: Object,
        connection: Arc<dyn Connection>,
    ) {
        let frames = match self.deps.engine.start(&track, position, volume, &filters) {
            Ok(frames) => frames,
            Err(error) => {
                self.finish(generation, Some(error));
                return;
            }
        };
        let mut frames = frames;
        let mut output = VoiceOutput { connection: &connection, speaking: false };
        self.pump(generation, &mut frames, position, volume, &filters, &mut output);
        let state = self.state.lock().unwrap();
        if state.generation == generation && output.speaking {
            let _ = output.connection.speaking(false);
        }
    }

    fn pump(
        &self,
        generation: u64,
        frames: &mut Box<dyn Frames>,
        position: i64,
        volume: i64,
        filters: &Object,
        output: &mut VoiceOutput,
    ) {
        let tempo =
            rustlink_audio::parse_filters(filters, volume).map(|set| set.tempo()).unwrap_or(1.0);
        let mut position = position as f64;
        let mut next = Instant::now();
        // One packet buffer for the life of the pump: a player runs for hours,
        // and one frame is written here every twenty milliseconds.
        let mut frame = Vec::new();
        loop {
            // Pause consumes neither source data nor playback time.
            {
                let state = self.state.lock().unwrap();
                if state.generation != generation || state.closed {
                    return;
                }
                if let Some(end) = state.end_time {
                    if state.body.state.position >= end {
                        drop(state);
                        self.finish(generation, None);
                        return;
                    }
                }
                if state.body.paused {
                    if output.speaking {
                        let _ = output.connection.speaking(false);
                        output.speaking = false;
                    }
                    drop(state);
                    self.wait(generation);
                    next = Instant::now();
                    continue;
                }
            }
            match frames.next_frame(&mut frame) {
                Ok(true) => {}
                Ok(false) => {
                    self.finish(generation, None);
                    return;
                }
                Err(error) => {
                    self.finish(generation, Some(error));
                    return;
                }
            }
            loop {
                let wait = next.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
                let mut state = self.state.lock().unwrap();
                if state.generation != generation || state.closed {
                    return;
                }
                if state.end_time.is_some_and(|end| state.body.state.position >= end) {
                    drop(state);
                    self.finish(generation, None);
                    return;
                }
                if state.body.paused {
                    if output.speaking {
                        let _ = output.connection.speaking(false);
                        output.speaking = false;
                    }
                    drop(state);
                    self.wait(generation);
                    next = Instant::now();
                    continue;
                }
                if !output.speaking {
                    if let Err(error) = output.connection.speaking(true) {
                        drop(state);
                        self.finish(generation, Some(AudioError::Transport(error.to_string())));
                        return;
                    }
                    output.speaking = true;
                }
                if let Err(error) = output.connection.write(&frame) {
                    drop(state);
                    self.finish(generation, Some(AudioError::Transport(error.to_string())));
                    return;
                }
                self.counters.sent.fetch_add(1, Ordering::Relaxed);
                position += FRAME_MILLIS * tempo;
                state.body.state.position = position as i64;
                if !state.started {
                    state.started = true;
                    let track = state.body.track.clone();
                    self.event("track.started", json!({ "track": track }));
                }
                break;
            }
            next += Duration::from_millis(FRAME_MILLIS as u64);
            let late = Instant::now().saturating_duration_since(next);
            if late > LATE {
                self.counters
                    .deficit
                    .fetch_add((late.as_millis() / FRAME_MILLIS as u128) as u64, Ordering::Relaxed);
                next = Instant::now();
            }
        }
    }

    /// Check the predicate while holding the same mutex as updates, so an
    /// update between the pause check and this wait cannot lose its signal.
    /// Paused players need no periodic wake-up or CPU polling.
    fn wait(&self, generation: u64) {
        let state = self.state.lock().unwrap();
        drop(
            self.wake
                .wait_while(state, |state| {
                    state.generation == generation
                        && !state.closed
                        && state.body.paused
                        && state.end_time.is_none_or(|end| state.body.state.position < end)
                })
                .unwrap(),
        );
    }

    /// Reports the end of a track, and stops the player from pacing more of it.
    fn finish(&self, generation: u64, error: Option<AudioError>) {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation || state.closed || state.body.track.is_none() {
            return;
        }
        let mut reason = "finished";
        if let Some(error) = error {
            reason = "load_failed";
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
            let track = state.body.track.clone();
            if matches!(error, AudioError::Stuck) {
                self.event(
                    "track.stalled",
                    json!({ "track": track, "threshold_ms": STUCK_MILLIS }),
                );
            } else {
                self.event(
                    "track.failed",
                    json!({
                        "track": track,
                        "exception": {
                            "message": error.to_string(),
                            "severity": "fault",
                            "cause": "rustlink.audio",
                        },
                    }),
                );
            }
        }
        let track = state.body.track.clone();
        self.event("track.ended", json!({ "track": track, "reason": reason }));
        state.body.track = None;
        state.running = false;
        state.started = false;
    }

    /// Tears the player down: the track ends, the connection closes and the
    /// playback thread stops.
    pub fn destroy(&self) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.closed = true;
        self.stop(&mut state);
        if let Some(track) = state.body.track.take() {
            self.event("track.ended", json!({ "track": track, "reason": "cleanup" }));
        }
        let connection = state.connection.take();
        if state.voice_up {
            state.voice_up = false;
            let channel = state.body.voice.channel_id.clone();
            self.event("voice.disconnected", voice_fields(channel.as_deref(), "destroyed"));
        }
        drop(state);
        if let Some(connection) = connection {
            connection.close();
        }
    }

    /// Sends one event to the session that owns this player.
    fn event(&self, kind: &str, fields: serde_json::Value) {
        let mut event = match fields {
            serde_json::Value::Object(object) => object,
            _ => Object::new(),
        };
        event.insert("timestamp_ms".into(), json!(now_millis()));
        event.insert("type".into(), json!(kind));
        event.insert("guild_id".into(), json!(self.guild));
        (self.emit)(event);
    }
}

/// Describes a bot voice event: the channel the connection was on and, when it
/// ended, why. The voice token, session and endpoint are left out, because an
/// event is readable by anyone the webhook is shared with.
fn voice_fields(channel: Option<&str>, reason: &str) -> serde_json::Value {
    let mut fields = Object::new();
    if let Some(channel) = channel {
        if !channel.is_empty() {
            fields.insert("channel_id".into(), json!(channel));
        }
    }
    if !reason.is_empty() {
        fields.insert("reason".into(), json!(reason));
    }
    serde_json::Value::Object(fields)
}
/// Checks a request before any of it is applied, so a request that cannot be
/// honoured leaves the player exactly as it was.
pub fn validate(update: &PlayerUpdate) -> Result<(), PlayerError> {
    if let Some(volume) = update.volume {
        if !(0..=1000).contains(&volume.get()) {
            return Err(PlayerError::Invalid("volume must be 0..1000".into()));
        }
    }
    if let Some(position) = update.position {
        if position.get() < 0 {
            return Err(PlayerError::Invalid("position must be nonnegative".into()));
        }
    }
    if let Patch::Value(end_time) = &update.end_time {
        if end_time.get() <= 0 {
            return Err(PlayerError::Invalid("end_ms must be positive or null".into()));
        }
    }
    if let Some(filters) = &update.filters {
        rustlink_audio::parse_filters(filters, 100)
            .map_err(|error| PlayerError::Invalid(error.to_string()))?;
    }
    if let Some(voice) = &update.voice {
        rustlink_voice::validate(voice).map_err(|error| PlayerError::Invalid(error.to_string()))?;
    }
    let change = update.track_update();
    if !change.token.is_missing() && change.query.is_some() {
        return Err(PlayerError::Invalid("token and query are mutually exclusive".into()));
    }
    if let Some(identifier) = &change.query {
        if identifier.trim().is_empty() {
            return Err(PlayerError::Invalid("query must not be empty".into()));
        }
        if identifier.len() > 8192 {
            return Err(PlayerError::Invalid("query must contain 1..8192 bytes".into()));
        }
    }
    if let Some(encoded) = change.token.value() {
        decode_track(encoded).map_err(|error| PlayerError::Invalid(error.to_string()))?;
    }
    Ok(())
}

/// The wall clock a client reads out of `player.updated`.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc::{channel, Receiver, Sender};

    use rustlink_protocol::{encode_track, TrackInfo};
    use rustlink_voice::VoiceError;
    use serde_json::Value;

    use crate::playback::Engine;

    /// A source that resolves one fixed track and opens an empty stream.
    struct FakeLoader;

    impl Loader for FakeLoader {
        fn load(&self, _identifier: &str) -> Result<LoadResult, rustlink_source::SourceError> {
            Ok(LoadResult::Track(Box::new(sample())))
        }

        fn open(
            &self,
            _track: &Track,
        ) -> Result<Box<dyn std::io::Read + Send>, rustlink_source::SourceError> {
            Ok(Box::new(std::io::empty()))
        }
    }

    struct FakeFrames {
        left: usize,
    }

    impl Frames for FakeFrames {
        fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
            if self.left == 0 {
                return Ok(false);
            }
            self.left -= 1;
            packet.clear();
            packet.resize(16, 0);
            Ok(true)
        }
    }

    /// An engine that produces a fixed number of frames.
    struct FakeEngine {
        frames: usize,
    }

    impl Engine for FakeEngine {
        fn start(
            &self,
            _track: &Track,
            _position: i64,
            _volume: i64,
            _filters: &Object,
        ) -> Result<Box<dyn Frames>, AudioError> {
            Ok(Box::new(FakeFrames { left: self.frames }))
        }
    }

    /// A connection that counts what it was sent.
    #[derive(Default)]
    struct FakeConnection {
        frames: AtomicUsize,
        closed: AtomicUsize,
        speaking: AtomicBool,
        pause_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    }

    impl Connection for FakeConnection {
        fn write(&self, _payload: &[u8]) -> Result<(), VoiceError> {
            self.frames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn speaking(&self, on: bool) -> Result<(), VoiceError> {
            let hook = if on { None } else { self.pause_hook.lock().unwrap().take() };
            if let Some((entered, release)) = hook {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            self.speaking.store(on, Ordering::SeqCst);
            Ok(())
        }
        fn connected(&self) -> bool {
            self.closed.load(Ordering::Relaxed) == 0
        }
        fn ping(&self) -> i64 {
            7
        }
        fn close(&self) {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A voice factory that hands back one connection immediately.
    struct FakeVoice {
        connection: Arc<FakeConnection>,
    }

    impl Factory for FakeVoice {
        fn connect(
            &self,
            _user: String,
            _guild: String,
            _state: VoiceState,
            _on_close: rustlink_voice::CloseHandler,
        ) -> futures_util::future::BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>>
        {
            let connection = self.connection.clone();
            Box::pin(async move { Ok(connection as Arc<dyn Connection>) })
        }
    }

    fn sample() -> Track {
        let info = TrackInfo {
            identifier: "abc".into(),
            title: "A Song".into(),
            length: 200_000,
            is_seekable: true,
            source_name: "test".into(),
            ..Default::default()
        };
        encode_track(&info).unwrap()
    }

    fn voice() -> VoiceState {
        VoiceState {
            token: "token".into(),
            endpoint: "c-iad21-1a2b3c.discord.media:443".into(),
            session_id: "session".into(),
            channel_id: Some("2".into()),
        }
    }

    fn update(body: Value) -> PlayerUpdate {
        serde_json::from_value(body).unwrap()
    }

    fn player(frames: usize) -> (Arc<Player>, Receiver<Value>, Arc<FakeConnection>) {
        let connection = Arc::new(FakeConnection::default());
        let (sender, receiver): (Sender<Value>, Receiver<Value>) = channel();
        let emit: Emit = Arc::new(move |event| {
            let _ = sender.send(Value::Object(event));
        });
        let deps = Arc::new(Deps {
            loader: Arc::new(FakeLoader),
            engine: Arc::new(FakeEngine { frames }),
            voice: Arc::new(FakeVoice { connection: connection.clone() }),
        });
        let player =
            Player::new("1".into(), "10".into(), deps, emit, Arc::new(Counters::default()));
        (player, receiver, connection)
    }

    /// Waits for the first event of a kind, failing the test if it never comes.
    fn wait_for(events: &Receiver<Value>, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(event) = events.recv_timeout(Duration::from_millis(100)) {
                if event["type"] == json!(kind) {
                    return event;
                }
            }
        }
        panic!("no {kind} arrived");
    }

    /// Reports whether an event of a kind arrives within a window.
    fn arrives(events: &Receiver<Value>, kind: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while let Ok(left) = deadline.checked_duration_since(Instant::now()).ok_or(()) {
            match events.recv_timeout(left) {
                Ok(event) if event["type"] == json!(kind) => return true,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        false
    }

    #[test]
    fn a_volume_outside_the_protocol_range_is_refused() {
        let error = validate(&update(json!({ "volume": 1001 }))).unwrap_err();
        assert_eq!(error.to_string(), "volume must be 0..1000");
        assert!(validate(&update(json!({ "volume": 0 }))).is_ok());
    }

    #[test]
    fn an_end_time_that_is_not_positive_is_refused() {
        assert!(validate(&update(json!({ "end_ms": 0 }))).is_err());
        assert!(validate(&update(json!({ "end_ms": null }))).is_ok());
        assert!(validate(&update(json!({ "end_ms": 5000 }))).is_ok());
    }

    #[test]
    fn a_filter_the_protocol_does_not_define_is_refused() {
        let error = validate(&update(json!({ "filters": { "reverb": {} } }))).unwrap_err();
        assert!(error.to_string().contains("unsupported filter"));
    }

    #[test]
    fn an_encoded_track_and_an_identifier_are_mutually_exclusive() {
        let error = validate(&update(json!({
            "track": { "token": sample().token, "query": "abc" },
        })))
        .unwrap_err();
        assert_eq!(error.to_string(), "token and query are mutually exclusive");
    }

    #[test]
    fn an_identifier_over_the_resolve_limit_is_refused() {
        let error =
            validate(&update(json!({ "track": { "query": "x".repeat(8193) } }))).unwrap_err();
        assert_eq!(error.to_string(), "query must contain 1..8192 bytes");
        assert!(validate(&update(json!({ "track": { "query": "x".repeat(8192) } }))).is_ok());
    }

    #[test]
    fn a_voice_endpoint_that_is_not_discord_is_refused() {
        let mut state = voice();
        state.endpoint = "example.com:443".into();
        let error = validate(&update(json!({ "voice": state }))).unwrap_err();
        assert!(error.to_string().contains("must be a Discord voice hostname"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn playing_a_track_announces_it_and_writes_its_frames() {
        let (player, events, connection) = player(3);
        let result = player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        assert_eq!(result.track.unwrap().metadata.title, "A Song");
        let start = wait_for(&events, "track.started");
        assert_eq!(start["track"]["metadata"]["title"], "A Song");
        assert_eq!(start["guild_id"], "10");
        let end = wait_for(&events, "track.ended");
        assert_eq!(end["reason"], "finished");
        assert!(connection.frames.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn playing_status_follows_the_live_connection_and_pause_state() {
        let (player, events, connection) = player(1000);
        player
            .update(update(json!({ "track": { "token": sample().token } })), false)
            .await
            .unwrap();
        assert!(!player.playing(), "a loaded track has no voice connection yet");

        player.update(update(json!({ "voice": voice() })), false).await.unwrap();
        wait_for(&events, "track.started");
        assert!(player.snapshot().state.connected);
        assert!(player.playing(), "active playback must be counted in node stats");

        player.update(update(json!({ "paused": true })), false).await.unwrap();
        assert!(!player.playing());
        player.update(update(json!({ "paused": false })), false).await.unwrap();
        assert!(player.playing());

        connection.close();
        assert!(!player.snapshot().state.connected);
        assert!(!player.playing(), "a closed transport must stop counting immediately");
        player.destroy();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_settings_and_unseekable_positions_do_not_reopen_the_source() {
        struct RecordingEngine(Sender<i64>);
        impl Engine for RecordingEngine {
            fn start(
                &self,
                _: &Track,
                position: i64,
                _: i64,
                _: &Object,
            ) -> Result<Box<dyn Frames>, AudioError> {
                self.0.send(position).unwrap();
                Ok(Box::new(FakeFrames { left: 1000 }))
            }
        }
        let (starts, opened) = channel();
        let connection = Arc::new(FakeConnection::default());
        let player = Player::new(
            "1".into(),
            "10".into(),
            Arc::new(Deps {
                loader: Arc::new(FakeLoader),
                engine: Arc::new(RecordingEngine(starts)),
                voice: Arc::new(FakeVoice { connection }),
            }),
            Arc::new(|_| {}),
            Arc::new(Counters::default()),
        );
        let mut info = sample().metadata;
        info.is_seekable = false;
        let track = encode_track(&info).unwrap();
        player
            .update(
                update(
                    json!({ "track": { "token": track.token }, "voice": voice(), "paused": true }),
                ),
                false,
            )
            .await
            .unwrap();
        assert_eq!(opened.recv_timeout(Duration::from_secs(2)).unwrap(), 0);

        player
            .update(update(json!({ "position_ms": 5000, "volume": 100, "filters": {} })), false)
            .await
            .unwrap();
        assert_eq!(player.snapshot().state.position, 0);
        assert!(opened.recv_timeout(Duration::from_millis(200)).is_err());

        let settings = json!({ "volume": 75, "filters": { "timescale": { "speed": 1.1 } } });
        player.update(update(settings.clone()), false).await.unwrap();
        assert_eq!(opened.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        player.update(update(settings), false).await.unwrap();
        assert!(opened.recv_timeout(Duration::from_millis(200)).is_err());
        player.destroy();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_player_holds_its_position() {
        let (player, events, _) = player(50);
        player
            .update(
                update(json!({
                    "track": { "token": sample().token },
                    "voice": voice(),
                    "paused": true,
                })),
                false,
            )
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let snapshot = player.snapshot();
        assert!(snapshot.paused);
        assert_eq!(snapshot.state.position, 0);
        assert!(!arrives(&events, "track.started", Duration::from_millis(200)));
        player.destroy();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_stale_pause_cannot_mute_a_replacement_track() {
        let (player, events, connection) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");

        // Suspend the old pump as it sends the pause announcement. A new
        // track must never send its speaking announcement ahead of that one.
        let (entered, paused) = channel();
        let (release, resume) = channel();
        *connection.pause_hook.lock().unwrap() = Some((entered, resume));
        player.update(update(json!({ "paused": true })), false).await.unwrap();
        paused.recv_timeout(Duration::from_secs(2)).unwrap();
        let replacement = Arc::clone(&player);
        let mut change = tokio::spawn(async move {
            replacement
                .update(update(json!({ "track": { "token": sample().token } })), false)
                .await
                .unwrap();
        });
        let completed = tokio::time::timeout(Duration::from_millis(200), &mut change).await;
        if let Ok(result) = completed.as_ref() {
            result.as_ref().unwrap();
            wait_for(&events, "track.started");
        }
        release.send(()).unwrap();
        if completed.is_err() {
            change.await.unwrap();
            wait_for(&events, "track.started");
        }
        // The old pump must have observed its invalidated generation and
        // exited before checking the final transport announcement.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&player) > 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let speaking = connection.speaking.load(Ordering::SeqCst);
        player.destroy();
        assert!(speaking, "an obsolete pause muted the replacement track");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_player_resumes_and_a_paused_destroy_releases_its_thread() {
        let (player, events, connection) = player(1000);
        player
            .update(
                update(json!({
                    "track": { "token": sample().token },
                    "voice": voice(),
                    "paused": true,
                })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "voice.connected");
        player.update(update(json!({ "paused": false })), false).await.unwrap();
        wait_for(&events, "track.started");
        assert!(connection.frames.load(Ordering::Relaxed) > 0);
        player.update(update(json!({ "paused": true })), false).await.unwrap();
        let weak = Arc::downgrade(&player);
        player.destroy();
        drop(player);
        let deadline = Instant::now() + Duration::from_secs(2);
        while weak.strong_count() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(weak.strong_count(), 0, "the parked playback thread retained its player");
    }

    #[test]
    fn a_resume_or_stop_before_wait_does_not_lose_the_wakeup() {
        let (player, _, _) = player(1);
        // The predicate is already false. These calls must return without a
        // timer or a notification that would have happened before the wait.
        player.wait(0);
        {
            let mut state = player.state.lock().unwrap();
            state.body.paused = true;
            player.stop(&mut state);
        }
        player.wait(0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacing_a_track_ends_the_previous_one() {
        let (player, events, _) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");
        player
            .update(update(json!({ "track": { "token": sample().token } })), false)
            .await
            .unwrap();
        assert_eq!(wait_for(&events, "track.ended")["reason"], "replaced");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_replace_leaves_the_playing_track_alone() {
        let (player, events, _) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");
        let result = player
            .update(update(json!({ "track": { "token": sample().token } })), true)
            .await
            .unwrap();
        assert_eq!(result.track.unwrap().metadata.title, "A Song");
        assert!(!arrives(&events, "track.ended", Duration::from_millis(200)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn destroying_a_player_ends_its_track_and_closes_its_connection() {
        let (player, events, connection) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");
        player.destroy();
        assert_eq!(connection.closed.load(Ordering::Relaxed), 1);
        assert!(player.snapshot().track.is_none());
        assert_eq!(wait_for(&events, "track.ended")["reason"], "cleanup");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_update_after_destruction_is_refused() {
        let (player, _events, _) = player(1);
        player.destroy();
        let error = player.update(update(json!({ "volume": 50 })), false).await.unwrap_err();
        assert_eq!(error.to_string(), "player is destroyed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_identifier_that_is_not_a_track_is_refused() {
        struct AlbumLoader;

        impl Loader for AlbumLoader {
            fn load(&self, _identifier: &str) -> Result<LoadResult, rustlink_source::SourceError> {
                Ok(LoadResult::Search(Vec::new()))
            }
            fn open(
                &self,
                _track: &Track,
            ) -> Result<Box<dyn std::io::Read + Send>, rustlink_source::SourceError> {
                Ok(Box::new(std::io::empty()))
            }
        }

        let deps = Arc::new(Deps {
            loader: Arc::new(AlbumLoader),
            engine: Arc::new(FakeEngine { frames: 1 }),
            voice: Arc::new(FakeVoice { connection: Arc::new(FakeConnection::default()) }),
        });
        let player = Player::new(
            "1".into(),
            "10".into(),
            deps,
            Arc::new(|_event| {}),
            Arc::new(Counters::default()),
        );
        let error = player
            .update(update(json!({ "track": { "query": "ytsearch:a song" } })), false)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "identifier must resolve to one track");
    }
}
