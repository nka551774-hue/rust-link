//! Per-guild audio player: state machine, voice transport lifecycle, and 20 ms Opus frame pump.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use rustlink_audio::{AudioError, FilterSet, Frames};
use rustlink_protocol::{
    decode_track, decode_track_owned, Int64, Object, Patch, Player as PlayerBody, PlayerState,
    PlayerUpdate, Track, VoiceState,
};
use rustlink_source::LoadResult;
use rustlink_voice::{Closed, Connection, Factory, VoiceError};
use serde_json::{json, Value};

use crate::playback::{Engine, Loader};
use crate::{now_millis, Counters, Emit};

const FRAME_MILLIS: f64 = 20.0;
const LATE: Duration = Duration::from_millis(100);
const MIN_CATCHUP_GAP: Duration = Duration::from_millis(5);
const STUCK_MILLIS: i64 = 10_000;
const MAX_USER_DATA_BYTES: usize = 16 << 10;

type PendingEvents = Vec<(&'static str, Value)>;

#[cfg(windows)]
mod high_res_timer {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(uPeriod: u32) -> u32;
        fn timeEndPeriod(uPeriod: u32) -> u32;
    }

    static ACTIVE_PUMPS: AtomicUsize = AtomicUsize::new(0);

    pub(super) struct Guard;

    impl Guard {
        pub(super) fn acquire() -> Self {
            if ACTIVE_PUMPS.fetch_add(1, Ordering::AcqRel) == 0 {
                // SAFETY: `timeBeginPeriod(1)` is thread-safe and requests 1 ms OS timer ticks.
                unsafe {
                    let _ = timeBeginPeriod(1);
                }
            }
            Self
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if ACTIVE_PUMPS.fetch_sub(1, Ordering::AcqRel) == 1 {
                // SAFETY: Balances `timeBeginPeriod(1)` when the last active pump exits.
                unsafe {
                    let _ = timeEndPeriod(1);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod high_res_timer {
    pub(super) struct Guard;

    impl Guard {
        pub(super) fn acquire() -> Self {
            Self
        }
    }
}

/// Sleeps in the OS kernel for the bulk of the interval and spin-parks across
/// the final sub-millisecond tail so 20 ms RTP frames fire with microsecond accuracy.
#[inline]
fn sleep_until_precise(deadline: Instant) {
    const KERNEL_SLEEP_MARGIN: Duration = Duration::from_micros(1_200);
    const YIELD_MARGIN: Duration = Duration::from_micros(150);
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        if remaining > KERNEL_SLEEP_MARGIN {
            std::thread::sleep(remaining - KERNEL_SLEEP_MARGIN);
        } else if remaining > YIELD_MARGIN {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

struct VoiceOutput<'a> {
    connection: &'a Arc<dyn Connection>,
    speaking: bool,
}

impl VoiceOutput<'_> {
    fn park_disconnected(&mut self, state: &mut State) {
        state.reset_pump();
        self.speaking = false;
    }

    fn quiet(&mut self) {
        if self.speaking {
            let _ = self.connection.speaking(false);
            self.speaking = false;
        }
    }
}

enum PumpGate<'a> {
    Ready(MutexGuard<'a, State>),
    Paused,
    Exit,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Source(#[from] rustlink_source::SourceError),
    #[error("player is destroyed")]
    Destroyed,
    #[error("identifier must resolve to one track")]
    NotATrack,
}

pub struct Deps {
    pub loader: Arc<dyn Loader>,
    pub engine: Arc<dyn Engine>,
    pub voice: Arc<dyn Factory>,
}

struct State {
    body: PlayerBody,
    end_time: Option<i64>,
    started: bool,
    closed: bool,
    running: bool,
    opening: bool,
    live_filters: bool,
    filter_set: FilterSet,
    pending_filters: Option<FilterSet>,
    generation: u64,
    connection: Option<Arc<dyn Connection>>,
    voice_generation: u64,
    voice_up: bool,
    connecting: Option<tokio::task::AbortHandle>,
}

impl State {
    fn reset_pump(&mut self) {
        self.running = false;
        self.opening = false;
        self.live_filters = false;
        self.pending_filters = None;
    }

    fn take_track(&mut self) -> Option<Track> {
        let mut track = self.body.track.take()?;
        track.metadata.position = self.body.state.position;
        self.body.state.position = 0;
        self.end_time = None;
        self.started = false;
        Some(track)
    }

    fn detach_voice(
        &mut self,
        channel: Option<&str>,
        reason: &str,
        events: &mut PendingEvents,
    ) -> Option<Arc<dyn Connection>> {
        let connection = self.connection.take();
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        if self.voice_up {
            self.voice_up = false;
            events.push(("voice.disconnected", voice_fields(channel, reason)));
        }
        connection
    }
}

pub struct Player {
    user: String,
    guild: String,
    deps: Arc<Deps>,
    emit: Emit,
    counters: Arc<Counters>,
    load_timeout: Duration,
    load_gate: Arc<tokio::sync::Semaphore>,
    state: Mutex<State>,
    wake: Condvar,
    updates: tokio::sync::Mutex<()>,
}

impl Player {
    pub fn new(
        user: String,
        guild: String,
        deps: Arc<Deps>,
        emit: Emit,
        counters: Arc<Counters>,
    ) -> Arc<Player> {
        Self::with_load_timeout(user, guild, deps, emit, counters, Duration::from_secs(30))
    }

    pub fn with_load_timeout(
        user: String,
        guild: String,
        deps: Arc<Deps>,
        emit: Emit,
        counters: Arc<Counters>,
        load_timeout: Duration,
    ) -> Arc<Player> {
        Self::with_limits(
            user,
            guild,
            deps,
            emit,
            counters,
            load_timeout,
            Arc::new(tokio::sync::Semaphore::new(8)),
        )
    }

    pub(crate) fn with_limits(
        user: String,
        guild: String,
        deps: Arc<Deps>,
        emit: Emit,
        counters: Arc<Counters>,
        load_timeout: Duration,
        load_gate: Arc<tokio::sync::Semaphore>,
    ) -> Arc<Player> {
        Arc::new(Player {
            user,
            guild: guild.clone(),
            deps,
            emit,
            counters,
            load_timeout,
            load_gate,
            state: Mutex::new(State {
                body: PlayerBody::new(guild),
                end_time: None,
                started: false,
                closed: false,
                running: false,
                opening: false,
                live_filters: false,
                filter_set: FilterSet::default(),
                pending_filters: None,
                generation: 0,
                connection: None,
                voice_generation: 0,
                voice_up: false,
                connecting: None,
            }),
            wake: Condvar::new(),
            updates: tokio::sync::Mutex::new(()),
        })
    }

    pub fn guild(&self) -> &str {
        &self.guild
    }

    pub fn snapshot(&self) -> PlayerBody {
        let state = self.state.lock().unwrap();
        self.snapshot_locked(&state)
    }

    pub fn state_snapshot(&self) -> (&str, PlayerState) {
        let state = self.state.lock().unwrap();
        (&self.guild, Self::live_state(&state))
    }

    pub fn track(&self) -> Option<Track> {
        let state = self.state.lock().unwrap();
        let mut track = state.body.track.clone()?;
        track.metadata.position = state.body.state.position;
        Some(track)
    }

    pub(crate) fn report_state(&self) -> (bool, Value) {
        let state = self.state.lock().unwrap();
        let live = Self::live_state(&state);
        let playing = state.body.track.is_some() && !state.body.paused && live.connected;
        let (title, author, source, duration_ms) = match state.body.track.as_ref() {
            Some(t) => (
                t.metadata.title.as_str(),
                t.metadata.author.as_str(),
                t.metadata.source_name.as_str(),
                t.metadata.length,
            ),
            None => ("", "", "", 0),
        };
        (
            playing,
            json!({
                "guild_id": self.guild,
                "title": title,
                "author": author,
                "source": source,
                "duration_ms": duration_ms,
                "position_ms": live.position,
                "endpoint": state.body.voice.endpoint,
                "volume": state.body.volume,
                "paused": state.body.paused,
                "connected": live.connected,
                "ping_ms": live.ping,
            }),
        )
    }

    pub fn playing(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.body.track.is_some()
            && !state.body.paused
            && state.connection.as_ref().is_some_and(|c| c.connected())
    }

    fn live_state(state: &State) -> PlayerState {
        let (connected, ping) = match state.connection.as_ref().filter(|c| c.connected()) {
            Some(conn) => (true, conn.ping()),
            None => (false, -1),
        };
        PlayerState { time: now_millis(), position: state.body.state.position, connected, ping }
    }

    fn snapshot_locked(&self, state: &State) -> PlayerBody {
        let mut body = state.body.clone();
        if let Some(track) = body.track.as_mut() {
            track.metadata.position = body.state.position;
        }
        body.state = Self::live_state(state);
        body
    }

    pub async fn update(
        self: &Arc<Self>,
        update: PlayerUpdate,
        no_replace: bool,
    ) -> Result<PlayerBody, PlayerError> {
        let _serialized = self.updates.lock().await;
        self.apply_locked(prepare(update)?, no_replace).await
    }

    pub(crate) async fn apply(
        self: &Arc<Self>,
        prepared: PreparedUpdate,
        no_replace: bool,
    ) -> Result<PlayerBody, PlayerError> {
        let _serialized = self.updates.lock().await;
        self.apply_locked(prepared, no_replace).await
    }

    async fn apply_locked(
        self: &Arc<Self>,
        mut prepared: PreparedUpdate,
        no_replace: bool,
    ) -> Result<PlayerBody, PlayerError> {
        let changes_track = prepared.changes_track;
        let skip_query = if changes_track && prepared.query.is_some() {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(PlayerError::Destroyed);
            }
            no_replace && state.body.track.is_some()
        } else {
            false
        };
        let resolved_track = match prepared.query.take().filter(|_| changes_track && !skip_query) {
            Some(identifier) => Some(self.resolve(identifier).await?),
            None => None,
        };
        let mut connect = None;
        let mut to_close: Option<Arc<dyn Connection>> = None;
        let mut events = PendingEvents::new();
        let result = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(PlayerError::Destroyed);
            }
            let ignore = skip_query || (no_replace && state.body.track.is_some());
            let mut track = if changes_track && !ignore {
                resolved_track.or_else(|| prepared.decoded_track.take())
            } else {
                None
            };
            let mut need_stop = false;
            let next_volume = prepared.volume.map_or(state.body.volume, Int64::get);
            let next_filters = prepared.filters.as_ref().unwrap_or(&state.body.filters);
            let settings_changed = prepared.volume.is_some_and(|v| v.get() != state.body.volume)
                || prepared.filters.as_ref().is_some_and(|f| *f != state.body.filters);
            let mut live_filter_update = None;
            if settings_changed {
                if let Ok(new_set) = rustlink_audio::parse_filters(next_filters, next_volume) {
                    if state.running {
                        if (state.live_filters || state.opening)
                            && state.filter_set.effective_time() == new_set.effective_time()
                        {
                            live_filter_update = Some(new_set.clone());
                        } else {
                            need_stop = true;
                        }
                    }
                    state.filter_set = new_set;
                } else if state.running {
                    need_stop = true;
                }
            }
            if changes_track && !ignore {
                need_stop = true;
                live_filter_update = None;
                if let Some(previous) = state.take_track() {
                    let reason = if track.is_some() { "replaced" } else { "stopped" };
                    events.push(("track.ended", json!({ "track": previous, "reason": reason })));
                }
                if let Some(next) = track.as_mut() {
                    next.metadata.position =
                        clamp_position(next.metadata.position, next.metadata.length);
                }
                state.body.state.position = track.as_ref().map_or(0, |next| next.metadata.position);
                state.body.track = track;
                state.started = false;
                state.end_time = None;
            }
            if let Some(user_data) = prepared.user_data.take().filter(|_| !ignore) {
                if let Some(current) = state.body.track.as_mut() {
                    current.user_data = user_data;
                }
            }
            if let Some(position) = prepared.position {
                let PlayerBody { track, state: player_state, .. } = &mut state.body;
                if let Some(current) = track.as_mut().filter(|t| t.metadata.is_seekable) {
                    let target = clamp_position(position.get(), current.metadata.length);
                    if target != player_state.position {
                        need_stop = true;
                        live_filter_update = None;
                    }
                    player_state.position = target;
                    current.metadata.position = target;
                }
            }
            match prepared.end_time {
                Patch::Missing => {}
                Patch::Null => state.end_time = None,
                Patch::Value(end_time) => state.end_time = Some(end_time.get()),
            }
            if let Some(volume) = prepared.volume {
                state.body.volume = volume.get();
            }
            if changes_track && !ignore {
                state.body.paused = prepared.paused.unwrap_or(false);
            } else if let Some(paused) = prepared.paused {
                state.body.paused = paused;
            }
            if let Some(filters) = prepared.filters {
                state.body.filters = filters;
            }
            if let Some(voice) = prepared.voice {
                let left = state.body.voice.channel_id.take();
                state.body.voice = voice.clone();
                need_stop = true;
                live_filter_update = None;
                to_close = state.detach_voice(left.as_deref(), "replaced", &mut events);
                state.voice_generation += 1;
                connect = Some((state.voice_generation, voice));
            }
            if need_stop {
                self.stop(&mut state);
            } else if let Some(set) = live_filter_update {
                state.pending_filters = Some(set);
            }
            self.start(&mut state, &mut events);
            self.wake.notify_all();
            self.snapshot_locked(&state)
        };
        if let Some(connection) = to_close {
            connection.close();
        }
        self.emit_all(events);
        if let Some((generation, voice)) = connect {
            self.open_voice(generation, voice);
        }
        Ok(result)
    }

    async fn resolve(&self, identifier: String) -> Result<Track, PlayerError> {
        let deadline = tokio::time::Instant::now() + self.load_timeout;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.load_gate).acquire_owned())
            .await
            .map_err(|_| PlayerError::Invalid("the source did not answer in time".into()))?
            .map_err(|_| PlayerError::Destroyed)?;
        let loader = self.deps.loader.clone();
        let trimmed = identifier.trim().to_owned();
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            loader.load_shared(&trimmed)
        });
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .map_err(|_| PlayerError::Invalid("the source did not answer in time".into()))?
            .map_err(|e| PlayerError::Invalid(e.to_string()))??;
        match result.as_ref() {
            LoadResult::Track(track) => Ok(track.as_ref().clone()),
            LoadResult::Error(error) => Err(PlayerError::Invalid(error.message.clone())),
            _ => Err(PlayerError::NotATrack),
        }
    }

    fn open_voice(self: &Arc<Self>, generation: u64, voice: VoiceState) {
        let weak: Weak<Player> = Arc::downgrade(self);
        let handler = Box::new(move |closed: Closed| {
            if let Some(player) = weak.upgrade() {
                player.connection_closed(generation, closed);
            }
        });
        let future = self.deps.voice.connect(self.user.clone(), self.guild.clone(), voice, handler);
        let player = Arc::clone(self);
        let task = tokio::spawn(async move {
            player.connected(generation, future.await);
        });
        let mut state = self.state.lock().unwrap();
        if state.closed || state.voice_generation != generation {
            task.abort();
        } else {
            state.connecting = Some(task.abort_handle());
        }
    }

    fn connected(
        self: &Arc<Self>,
        generation: u64,
        result: Result<Arc<dyn Connection>, VoiceError>,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.voice_generation != generation {
            drop(state);
            if let Ok(connection) = result {
                connection.close();
            }
            return;
        }
        state.connecting = None;
        let mut events = PendingEvents::new();
        match result {
            Ok(connection) => {
                if connection.connected() {
                    state.voice_up = true;
                    let channel = state.body.voice.channel_id.clone();
                    events.push(("voice.connected", voice_fields(channel.as_deref(), "")));
                }
                state.connection = Some(connection);
                self.start(&mut state, &mut events);
            }
            Err(error) => events.push((
                "voice.closed",
                json!({ "code": 4000, "reason": error.to_string(), "by_remote": false }),
            )),
        }
        drop(state);
        self.emit_all(events);
    }

    fn connection_closed(&self, generation: u64, closed: Closed) {
        let mut events = PendingEvents::new();
        {
            let mut state = self.state.lock().unwrap();
            if state.closed || state.voice_generation != generation {
                return;
            }
            self.stop(&mut state);
            if state.voice_up {
                state.voice_up = false;
                let channel = state.body.voice.channel_id.clone();
                let mut fields = voice_fields(channel.as_deref(), "closed");
                fields["code"] = json!(closed.code);
                fields["by_remote"] = json!(closed.by_remote);
                events.push(("voice.disconnected", fields));
            }
            events.push((
                "voice.closed",
                json!({ "code": closed.code, "reason": closed.reason, "by_remote": closed.by_remote }),
            ));
        }
        self.emit_all(events);
    }

    fn stop(&self, state: &mut State) {
        state.generation += 1;
        state.reset_pump();
        self.wake.notify_all();
        if let Some(connection) = state.connection.as_ref().filter(|c| c.connected()) {
            let _ = connection.speaking(false);
        }
    }

    fn start(self: &Arc<Self>, state: &mut State, events: &mut PendingEvents) {
        if state.closed || state.running || state.body.track.is_none() {
            return;
        }
        let Some(connection) = state.connection.as_ref().filter(|c| c.connected()).cloned() else {
            return;
        };
        let Some(track) = state.body.track.clone() else {
            return;
        };
        state.generation += 1;
        state.running = true;
        state.opening = true;
        state.live_filters = false;
        state.pending_filters = None;
        let generation = state.generation;
        let position = state.body.state.position;
        let volume = state.body.volume;
        let filters = state.body.filters.clone();
        let tempo = state.filter_set.tempo();
        let player = Arc::clone(self);
        let spawned =
            thread::Builder::new().name(format!("player-{}", self.guild)).spawn(move || {
                player.run(generation, track, position, volume, filters, tempo, connection)
            });
        if let Err(error) = spawned {
            state.reset_pump();
            if let Some(failed_track) = state.take_track() {
                self.push_track_failure(events, &failed_track, false, error.to_string());
            }
        }
    }

    fn push_track_failure(
        &self,
        events: &mut PendingEvents,
        track: &Track,
        stalled: bool,
        message: String,
    ) {
        let track_json = serde_json::to_value(track).unwrap_or(Value::Null);
        self.counters.errors.fetch_add(1, Ordering::Relaxed);
        if stalled {
            events.push((
                "track.stalled",
                json!({ "track": track_json.clone(), "threshold_ms": STUCK_MILLIS }),
            ));
        } else {
            events.push((
                "track.failed",
                json!({
                    "track": track_json.clone(),
                    "exception": {
                        "message": message,
                        "severity": "fault",
                        "cause": "rustlink.audio",
                    },
                }),
            ));
        }
        events.push(("track.ended", json!({ "track": track_json, "reason": "load_failed" })));
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        self: Arc<Self>,
        generation: u64,
        track: Track,
        position: i64,
        mut volume: i64,
        mut filters: Object,
        tempo: f64,
        connection: Arc<dyn Connection>,
    ) {
        let mut frames = loop {
            let mut opened = match self.deps.engine.start(&track, position, volume, &filters) {
                Ok(frames) => frames,
                Err(error) => {
                    self.finish(generation, Some(error));
                    return;
                }
            };
            let mut state = self.state.lock().unwrap();
            if state.generation != generation || state.closed {
                return;
            }
            let supports_live = opened.supports_live_filters();
            if let Some(pending) = state.pending_filters.take() {
                if supports_live {
                    let _ = opened.set_filters(&pending);
                } else {
                    volume = state.body.volume;
                    filters = state.body.filters.clone();
                    continue;
                }
            }
            state.opening = false;
            state.live_filters = supports_live;
            break opened;
        };
        let mut output = VoiceOutput { connection: &connection, speaking: false };
        self.pump(generation, &mut frames, position, tempo, &mut output);
        if output.speaking {
            let state = self.state.lock().unwrap();
            if state.generation == generation && output.connection.connected() {
                let _ = output.connection.speaking(false);
            }
        }
    }

    fn poll_pump(
        &self,
        generation: u64,
        output: &mut VoiceOutput<'_>,
        timer_guard: &mut Option<high_res_timer::Guard>,
        next: &mut Instant,
        last_send: &mut Option<Instant>,
    ) -> PumpGate<'_> {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation || state.closed {
            return PumpGate::Exit;
        }
        if !output.connection.connected() {
            output.park_disconnected(&mut state);
            return PumpGate::Exit;
        }
        if state.end_time.is_some_and(|end| state.body.state.position >= end) {
            drop(state);
            self.finish(generation, None);
            return PumpGate::Exit;
        }
        if state.body.paused {
            output.quiet();
            drop(state);
            drop(timer_guard.take());
            self.wait(generation);
            *timer_guard = Some(high_res_timer::Guard::acquire());
            *next = Instant::now();
            *last_send = None;
            return PumpGate::Paused;
        }
        PumpGate::Ready(state)
    }

    fn handle_voice_err(
        &self,
        generation: u64,
        mut state: MutexGuard<'_, State>,
        output: &mut VoiceOutput<'_>,
        error: VoiceError,
    ) {
        if matches!(error, VoiceError::NotReady) || !output.connection.connected() {
            output.park_disconnected(&mut state);
        } else {
            drop(state);
            self.finish(generation, Some(AudioError::Transport(error.to_string())));
        }
    }

    fn pump(
        &self,
        generation: u64,
        frames: &mut Box<dyn Frames>,
        position: i64,
        tempo: f64,
        output: &mut VoiceOutput<'_>,
    ) {
        let mut timer_guard = Some(high_res_timer::Guard::acquire());
        let mut position = position as f64;
        let mut next = Instant::now();
        let mut last_send: Option<Instant> = None;
        let mut frame = Vec::with_capacity(1500);
        loop {
            let pending_update = match self.poll_pump(
                generation,
                output,
                &mut timer_guard,
                &mut next,
                &mut last_send,
            ) {
                PumpGate::Ready(mut state) => state.pending_filters.take(),
                PumpGate::Paused => continue,
                PumpGate::Exit => return,
            };
            if let Some(set) = pending_update {
                let _ = frames.set_filters(&set);
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
                let target = last_send.map_or(next, |prev| next.max(prev + MIN_CATCHUP_GAP));
                sleep_until_precise(target);
                let started_track = {
                    let mut state = match self.poll_pump(
                        generation,
                        output,
                        &mut timer_guard,
                        &mut next,
                        &mut last_send,
                    ) {
                        PumpGate::Ready(state) => state,
                        PumpGate::Paused => continue,
                        PumpGate::Exit => return,
                    };
                    if !output.speaking {
                        if let Err(error) = output.connection.speaking(true) {
                            self.handle_voice_err(generation, state, output, error);
                            return;
                        }
                        output.speaking = true;
                        next = Instant::now();
                    }
                    if let Err(error) = output.connection.write(&frame) {
                        self.handle_voice_err(generation, state, output, error);
                        return;
                    }
                    last_send = Some(Instant::now());
                    self.counters.sent.fetch_add(1, Ordering::Relaxed);
                    position += FRAME_MILLIS * tempo;
                    state.body.state.position = position as i64;
                    (!state.started)
                        .then(|| {
                            state.started = true;
                            state.body.track.clone()
                        })
                        .flatten()
                };
                if let Some(track) = started_track {
                    self.event("track.started", json!({ "track": track }));
                }
                let _ = &timer_guard;
                break;
            }
            let frame_duration = Duration::from_millis(FRAME_MILLIS as u64);
            next += frame_duration;
            let now = Instant::now();
            let late = now.saturating_duration_since(next);
            if late > LATE {
                self.counters
                    .deficit
                    .fetch_add((late.as_millis() / FRAME_MILLIS as u128) as u64, Ordering::Relaxed);
                next = now + frame_duration;
            }
        }
    }

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

    fn finish(&self, generation: u64, error: Option<AudioError>) {
        let mut events = PendingEvents::with_capacity(2);
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation || state.closed {
                return;
            }
            let Some(track) = state.take_track() else {
                return;
            };
            self.stop(&mut state);
            if let Some(error) = error {
                let stalled = matches!(error, AudioError::Stuck);
                self.push_track_failure(&mut events, &track, stalled, error.to_string());
            } else {
                events.push(("track.ended", json!({ "track": track, "reason": "finished" })));
            }
        }
        self.emit_all(events);
    }

    pub fn destroy(&self) {
        let mut events = PendingEvents::new();
        let connection = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.closed = true;
            self.stop(&mut state);
            if let Some(track) = state.take_track() {
                events.push(("track.ended", json!({ "track": track, "reason": "cleanup" })));
            }
            let channel = state.body.voice.channel_id.clone();
            state.detach_voice(channel.as_deref(), "destroyed", &mut events)
        };
        if let Some(connection) = connection {
            connection.close();
        }
        self.emit_all(events);
    }

    fn emit_all(&self, events: PendingEvents) {
        for (kind, payload) in events {
            self.event(kind, payload);
        }
    }

    fn event(&self, kind: &str, fields: Value) {
        let mut event = match fields {
            Value::Object(object) => object,
            _ => Object::new(),
        };
        event.insert("timestamp_ms".into(), json!(now_millis()));
        event.insert("type".into(), json!(kind));
        event.insert("guild_id".into(), json!(self.guild));
        (self.emit)(event);
    }
}

fn voice_fields(channel: Option<&str>, reason: &str) -> Value {
    let mut fields = Object::new();
    if let Some(channel) = channel.filter(|c| !c.is_empty()) {
        fields.insert("channel_id".into(), json!(channel));
    }
    if !reason.is_empty() {
        fields.insert("reason".into(), json!(reason));
    }
    Value::Object(fields)
}

fn clamp_position(position: i64, length: i64) -> i64 {
    let clamped = position.max(0);
    if length > 0 {
        clamped.min(length)
    } else {
        clamped
    }
}

pub(crate) struct PreparedUpdate {
    changes_track: bool,
    decoded_track: Option<Track>,
    query: Option<String>,
    user_data: Option<Object>,
    position: Option<Int64>,
    end_time: Patch<Int64>,
    volume: Option<Int64>,
    paused: Option<bool>,
    pub(crate) filters: Option<Object>,
    voice: Option<VoiceState>,
}

struct BoundedCounter {
    bytes: usize,
    limit: usize,
}

impl std::io::Write for BoundedCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        if self.bytes > self.limit {
            return Err(std::io::Error::other("user_data exceeds 16 KiB"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_user_data(user_data: &Object) -> Result<(), PlayerError> {
    let mut counter = BoundedCounter { bytes: 0, limit: MAX_USER_DATA_BYTES };
    serde_json::to_writer(&mut counter, user_data)
        .map_err(|_| PlayerError::Invalid("user_data must not exceed 16 KiB".into()))
}

fn validate_scalar_fields(
    volume: Option<Int64>,
    position: Option<Int64>,
    end_time: Patch<Int64>,
    filters: Option<&Object>,
    voice: Option<&VoiceState>,
) -> Result<(), PlayerError> {
    if let Some(volume) = volume {
        if !(0..=1000).contains(&volume.get()) {
            return Err(PlayerError::Invalid("volume must be 0..1000".into()));
        }
    }
    if let Some(position) = position {
        if position.get() < 0 {
            return Err(PlayerError::Invalid("position must be nonnegative".into()));
        }
    }
    if let Patch::Value(end_time) = end_time {
        if end_time.get() <= 0 {
            return Err(PlayerError::Invalid("end_ms must be positive or null".into()));
        }
        if position.is_some_and(|pos| end_time.get() <= pos.get()) {
            return Err(PlayerError::Invalid("end_ms must be greater than position_ms".into()));
        }
    }
    if let Some(filters) = filters {
        rustlink_audio::parse_filters(filters, 100)
            .map_err(|e| PlayerError::Invalid(e.to_string()))?;
    }
    if let Some(voice) = voice {
        rustlink_voice::validate(voice).map_err(|e| PlayerError::Invalid(e.to_string()))?;
    }
    Ok(())
}

fn validate_track_fields(
    has_token: bool,
    query: Option<&str>,
    user_data: Option<&Object>,
) -> Result<(), PlayerError> {
    if let Some(data) = user_data {
        validate_user_data(data)?;
    }
    if has_token && query.is_some() {
        return Err(PlayerError::Invalid("token and query are mutually exclusive".into()));
    }
    if let Some(identifier) = query {
        if identifier.trim().is_empty() {
            return Err(PlayerError::Invalid("query must not be empty".into()));
        }
        if identifier.len() > 8192 {
            return Err(PlayerError::Invalid("query must contain 1..8192 bytes".into()));
        }
    }
    Ok(())
}

pub(crate) fn prepare(update: PlayerUpdate) -> Result<PreparedUpdate, PlayerError> {
    validate_scalar_fields(
        update.volume,
        update.position,
        update.end_time,
        update.filters.as_ref(),
        update.voice.as_ref(),
    )?;
    let (token, query, user_data) = match update.track {
        Patch::Value(spec) => (spec.token, spec.query, spec.user_data),
        Patch::Null => (Patch::Null, None, None),
        Patch::Missing => (Patch::Missing, None, None),
    };
    validate_track_fields(!token.is_missing(), query.as_deref(), user_data.as_ref())?;
    let changes_track = !token.is_missing() || query.is_some();
    let decoded_track = match token {
        Patch::Value(encoded) => {
            Some(decode_track_owned(encoded).map_err(|e| PlayerError::Invalid(e.to_string()))?)
        }
        Patch::Null | Patch::Missing => None,
    };
    Ok(PreparedUpdate {
        changes_track,
        decoded_track,
        query,
        user_data,
        position: update.position,
        end_time: update.end_time,
        volume: update.volume,
        paused: update.paused,
        filters: update.filters,
        voice: update.voice,
    })
}

pub fn validate(update: &PlayerUpdate) -> Result<(), PlayerError> {
    validate_scalar_fields(
        update.volume,
        update.position,
        update.end_time,
        update.filters.as_ref(),
        update.voice.as_ref(),
    )?;
    let (token, query, user_data) = match &update.track {
        Patch::Value(spec) => (&spec.token, spec.query.as_deref(), spec.user_data.as_ref()),
        Patch::Null => (&Patch::Null, None, None),
        Patch::Missing => (&Patch::Missing, None, None),
    };
    validate_track_fields(!token.is_missing(), query, user_data)?;
    if let Some(encoded) = token.value() {
        decode_track(encoded).map_err(|e| PlayerError::Invalid(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc::{channel, Receiver, Sender};

    use rustlink_protocol::{encode_track, TrackInfo};

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

    #[derive(Default)]
    struct FakeConnection {
        frames: AtomicUsize,
        closed: AtomicUsize,
        speaking: AtomicBool,
        speaking_off: AtomicUsize,
        pause_hook: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    }

    impl Connection for FakeConnection {
        fn write(&self, _payload: &[u8]) -> Result<(), VoiceError> {
            self.frames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn speaking(&self, on: bool) -> Result<(), VoiceError> {
            if !on {
                self.speaking_off.fetch_add(1, Ordering::SeqCst);
                if let Some((entered, release)) = self.pause_hook.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(3)).unwrap();
                }
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

    #[derive(Default)]
    struct TimedConnection(Mutex<Vec<Instant>>);

    impl Connection for TimedConnection {
        fn write(&self, _: &[u8]) -> Result<(), VoiceError> {
            self.0.lock().unwrap().push(Instant::now());
            Ok(())
        }
        fn speaking(&self, _: bool) -> Result<(), VoiceError> {
            Ok(())
        }
        fn connected(&self) -> bool {
            true
        }
        fn ping(&self) -> i64 {
            1
        }
        fn close(&self) {}
    }

    struct StaticVoice<C>(Arc<C>);

    impl<C: Connection + 'static> Factory for StaticVoice<C> {
        fn connect(
            &self,
            _user: String,
            _guild: String,
            _state: VoiceState,
            _on_close: rustlink_voice::CloseHandler,
        ) -> futures_util::future::BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>>
        {
            let conn = Arc::clone(&self.0);
            Box::pin(async move { Ok(conn as Arc<dyn Connection>) })
        }
    }

    fn sample() -> Track {
        encode_track(&TrackInfo {
            identifier: "abc".into(),
            title: "A Song".into(),
            length: 200_000,
            is_seekable: true,
            source_name: "test".into(),
            ..Default::default()
        })
        .unwrap()
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

    fn build_player(
        loader: Arc<dyn Loader>,
        engine: Arc<dyn Engine>,
        voice: Arc<dyn Factory>,
        counters: Arc<Counters>,
    ) -> (Arc<Player>, Receiver<Value>) {
        let (sender, receiver) = channel();
        let emit: Emit = Arc::new(move |event| {
            let _ = sender.send(Value::Object(event));
        });
        let player = Player::new(
            "1".into(),
            "10".into(),
            Arc::new(Deps { loader, engine, voice }),
            emit,
            counters,
        );
        (player, receiver)
    }

    fn player(frames: usize) -> (Arc<Player>, Receiver<Value>, Arc<FakeConnection>) {
        let conn = Arc::new(FakeConnection::default());
        let (player, rx) = build_player(
            Arc::new(FakeLoader),
            Arc::new(FakeEngine { frames }),
            Arc::new(StaticVoice(Arc::clone(&conn))),
            Arc::new(Counters::default()),
        );
        (player, rx, conn)
    }

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

    fn arrives(events: &Receiver<Value>, kind: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(event) if event["type"] == json!(kind) => return true,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        false
    }

    #[test]
    fn validation_enforces_volume_end_time_filters_tokens_queries_voice_and_user_data() {
        assert_eq!(
            validate(&update(json!({ "volume": 1001 }))).unwrap_err().to_string(),
            "volume must be 0..1000"
        );
        assert!(validate(&update(json!({ "volume": 0 }))).is_ok());

        assert!(validate(&update(json!({ "end_ms": 0 }))).is_err());
        assert!(validate(&update(json!({ "end_ms": null }))).is_ok());
        assert!(validate(&update(json!({ "end_ms": 5000 }))).is_ok());
        assert!(validate(&update(json!({ "position_ms": 5000, "end_ms": 5000 }))).is_err());
        assert!(validate(&update(json!({ "position_ms": 5000, "end_ms": 4999 }))).is_err());
        assert!(validate(&update(json!({ "position_ms": 5000, "end_ms": 5001 }))).is_ok());

        assert!(validate(&update(json!({ "filters": { "reverb": {} } })))
            .unwrap_err()
            .to_string()
            .contains("unsupported filter"));

        assert_eq!(
            validate(&update(json!({ "track": { "token": sample().token, "query": "abc" } })))
                .unwrap_err()
                .to_string(),
            "token and query are mutually exclusive"
        );
        assert_eq!(
            validate(&update(json!({ "track": { "query": "x".repeat(8193) } })))
                .unwrap_err()
                .to_string(),
            "query must contain 1..8192 bytes"
        );
        assert!(validate(&update(json!({ "track": { "query": "x".repeat(8192) } }))).is_ok());

        let mut bad_voice = voice();
        bad_voice.endpoint = "example.com:443".into();
        assert!(validate(&update(json!({ "voice": bad_voice })))
            .unwrap_err()
            .to_string()
            .contains("must be a Discord voice hostname"));

        let small = update(json!({ "track": { "user_data": { "k": "v" } } }));
        assert!(validate(&small).is_ok() && prepare(small).is_ok());
        let huge = update(json!({ "track": { "user_data": { "blob": "x".repeat(20_000) } } }));
        assert!(validate(&huge).is_err() && prepare(huge).is_err());
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
        assert_eq!(wait_for(&events, "track.ended")["reason"], "finished");
        assert!(connection.frames.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn playing_status_follows_the_live_connection_and_pause_state() {
        let (player, events, connection) = player(1000);
        player
            .update(update(json!({ "track": { "token": sample().token } })), false)
            .await
            .unwrap();
        assert!(!player.playing());

        player.update(update(json!({ "voice": voice() })), false).await.unwrap();
        wait_for(&events, "track.started");
        assert!(player.snapshot().state.connected && player.playing());

        player.update(update(json!({ "paused": true })), false).await.unwrap();
        assert!(!player.playing());
        player.update(update(json!({ "paused": false })), false).await.unwrap();
        assert!(player.playing());

        connection.close();
        assert!(!player.snapshot().state.connected && !player.playing());
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
        let (player, _) = build_player(
            Arc::new(FakeLoader),
            Arc::new(RecordingEngine(starts)),
            Arc::new(StaticVoice(Arc::new(FakeConnection::default()))),
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
    async fn a_paused_player_holds_its_position_resumes_and_releases_its_thread() {
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
        assert!(!arrives(&events, "track.started", Duration::from_millis(120)));
        let snapshot = player.snapshot();
        assert!(snapshot.paused && snapshot.state.position == 0);

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
        assert_eq!(weak.strong_count(), 0);
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
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&player) > 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let speaking = connection.speaking.load(Ordering::SeqCst);
        player.destroy();
        assert!(speaking, "an obsolete pause muted the replacement track");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn destroying_a_player_aborts_its_pending_voice_handshake() {
        struct Dropped(Sender<()>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        struct HangingVoice(Mutex<Option<Sender<()>>>);
        impl Factory for HangingVoice {
            fn connect(
                &self,
                _: String,
                _: String,
                _: VoiceState,
                _: rustlink_voice::CloseHandler,
            ) -> futures_util::future::BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>>
            {
                let guard = Dropped(self.0.lock().unwrap().take().unwrap());
                Box::pin(async move {
                    let _guard = guard;
                    std::future::pending().await
                })
            }
        }
        let (dropped, aborted) = channel();
        let (player, _) = build_player(
            Arc::new(FakeLoader),
            Arc::new(FakeEngine { frames: 1 }),
            Arc::new(HangingVoice(Mutex::new(Some(dropped)))),
            Arc::new(Counters::default()),
        );
        player.update(update(json!({ "voice": voice() })), false).await.unwrap();
        assert!(aborted.try_recv().is_err());
        player.destroy();
        let deadline = Instant::now() + Duration::from_secs(2);
        while aborted.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the voice handshake outlived its player");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn a_resume_or_stop_before_wait_does_not_lose_the_wakeup() {
        let (player, _, _) = player(1);
        player.wait(0);
        {
            let mut state = player.state.lock().unwrap();
            state.body.paused = true;
            player.stop(&mut state);
        }
        player.wait(0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn track_replacement_no_replace_and_destruction_behave_cleanly() {
        let (player, events, connection) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");

        let preserved = player
            .update(update(json!({ "track": { "token": sample().token } })), true)
            .await
            .unwrap();
        assert_eq!(preserved.track.unwrap().metadata.title, "A Song");
        assert!(!arrives(&events, "track.ended", Duration::from_millis(120)));

        player
            .update(update(json!({ "track": { "token": sample().token } })), false)
            .await
            .unwrap();
        assert_eq!(wait_for(&events, "track.ended")["reason"], "replaced");
        wait_for(&events, "track.started");

        player.destroy();
        assert_eq!(connection.closed.load(Ordering::Relaxed), 1);
        assert!(player.snapshot().track.is_none());
        assert_eq!(wait_for(&events, "track.ended")["reason"], "cleanup");
        assert_eq!(
            player.update(update(json!({ "volume": 50 })), false).await.unwrap_err().to_string(),
            "player is destroyed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_identifier_that_is_not_a_track_is_refused() {
        struct AlbumLoader;
        impl Loader for AlbumLoader {
            fn load(&self, _: &str) -> Result<LoadResult, rustlink_source::SourceError> {
                Ok(LoadResult::Search(Vec::new()))
            }
            fn open(
                &self,
                _: &Track,
            ) -> Result<Box<dyn std::io::Read + Send>, rustlink_source::SourceError> {
                Ok(Box::new(std::io::empty()))
            }
        }
        let (player, _) = build_player(
            Arc::new(AlbumLoader),
            Arc::new(FakeEngine { frames: 1 }),
            Arc::new(StaticVoice(Arc::new(FakeConnection::default()))),
            Arc::new(Counters::default()),
        );
        let error = player
            .update(update(json!({ "track": { "query": "ytsearch:a song" } })), false)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "identifier must resolve to one track");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transport_disconnect_preserves_loaded_track_without_emitting_failure() {
        let (player, events, connection) = player(1000);
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.started");

        connection.close();
        assert!(!arrives(&events, "track.failed", Duration::from_millis(120)));
        assert!(!arrives(&events, "track.ended", Duration::from_millis(120)));
        assert_eq!(
            player.snapshot().track.as_ref().map(|t| t.metadata.title.as_str()),
            Some("A Song")
        );
        player.destroy();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_volume_and_filter_updates_do_not_bump_generation_or_send_speaking_false() {
        let (player, events, conn) = player(1);
        player.update(update(json!({ "voice": voice() })), false).await.unwrap();
        wait_for(&events, "voice.connected");
        let gen_before = player.state.lock().unwrap().generation;
        let off_before = conn.speaking_off.load(Ordering::SeqCst);

        player
            .update(
                update(json!({ "volume": 80, "filters": { "tremolo": { "depth": 0.5 } } })),
                false,
            )
            .await
            .unwrap();
        assert_eq!(player.state.lock().unwrap().generation, gen_before);
        assert_eq!(conn.speaking_off.load(Ordering::SeqCst), off_before);
        player.destroy();
    }

    struct DelayedFrames {
        index: usize,
        total: usize,
        delay_on_second: Duration,
    }

    impl Frames for DelayedFrames {
        fn next_frame(&mut self, packet: &mut Vec<u8>) -> Result<bool, AudioError> {
            if self.index >= self.total {
                return Ok(false);
            }
            if self.index == 1 {
                std::thread::sleep(self.delay_on_second);
            }
            self.index += 1;
            packet.clear();
            packet.resize(8, self.index as u8);
            Ok(true)
        }
    }

    struct DelayedEngine {
        total: usize,
        delay: Duration,
    }

    impl Engine for DelayedEngine {
        fn start(
            &self,
            _: &Track,
            _: i64,
            _: i64,
            _: &Object,
        ) -> Result<Box<dyn Frames>, AudioError> {
            Ok(Box::new(DelayedFrames { index: 0, total: self.total, delay_on_second: self.delay }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_stall_pacing_waits_one_frame_interval_before_next_write() {
        let writes = Arc::new(TimedConnection::default());
        let counters = Arc::new(Counters::default());
        let (player, events) = build_player(
            Arc::new(FakeLoader),
            Arc::new(DelayedEngine { total: 3, delay: Duration::from_millis(165) }),
            Arc::new(StaticVoice(Arc::clone(&writes))),
            Arc::clone(&counters),
        );
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.ended");
        let stamps = writes.0.lock().unwrap().clone();
        assert_eq!(stamps.len(), 3);
        assert!(counters.deficit.load(Ordering::Relaxed) >= 5);
        let gap = stamps[2].saturating_duration_since(stamps[1]);
        assert!(gap >= Duration::from_millis(14), "frame after stall burst too early: {gap:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sub_threshold_hiccup_recovers_smoothly_without_zero_microsecond_packet_bursts() {
        let writes = Arc::new(TimedConnection::default());
        let (player, events) = build_player(
            Arc::new(FakeLoader),
            Arc::new(DelayedEngine { total: 4, delay: Duration::from_millis(55) }),
            Arc::new(StaticVoice(Arc::clone(&writes))),
            Arc::new(Counters::default()),
        );
        player
            .update(
                update(json!({ "track": { "token": sample().token }, "voice": voice() })),
                false,
            )
            .await
            .unwrap();
        wait_for(&events, "track.ended");
        let stamps = writes.0.lock().unwrap().clone();
        assert_eq!(stamps.len(), 4);
        let catchup_gap = stamps[2].saturating_duration_since(stamps[1]);
        assert!(
            catchup_gap >= Duration::from_millis(4),
            "catch-up frame burst in {catchup_gap:?} instead of respecting MIN_CATCHUP_GAP"
        );
    }
}
