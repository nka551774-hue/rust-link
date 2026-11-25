//! A Discord music bot for Rustlink.
//!
//! It logs in to Discord, answers slash commands, keeps a queue per server, and
//! hands the sound to a Rustlink node, which resolves the tracks, decodes them
//! and sends the audio to the voice channel. Nothing here touches audio.
//!
//! Configuration lives in a `.env` file (copy `.env.example`), or in the
//! environment, which wins over the file:
//!
//! ```text
//! cargo run -p rustlink-discord-bot --example music            # reads ./.env
//! cargo run -p rustlink-discord-bot --example music -- my.env  # another file
//! ```
//!
//! Commands: `/play`, `/skip`, `/pause`, `/resume`, `/stop`, `/queue`,
//! `/nowplaying` and `/volume`.

mod config;
mod state;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustlink_client::{
    Client, LoadResult, Object, Patch, PlayerUpdate, Session, Track, TrackUpdate, VoiceState,
};
use rustlink_discord_bot::{
    extract_message_query, truncate_chars, BotEvent, DiscordRest, Gateway, Interaction,
    MessageCreate, VoiceStatePairer, VoiceStateUpdate,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use config::Config;
use state::{format_length, normalize_query, parse_volume, Guild};

type Failure = Box<dyn std::error::Error>;

/// How many waiting tracks `/queue` lists.
const QUEUE_PAGE: usize = 10;
/// Re-send `OP 4` if a pending voice join has not completed after this long.
const JOIN_RETRY_AFTER: Duration = Duration::from_secs(5);
/// Expire an unanswered voice join (e.g. missing `CONNECT` permission or full channel).
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Failure> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".env".into());
    let config = Config::load(Path::new(&path))?;
    let rustlink = Client::new(&config.rustlink_url, config.password.clone())?;
    // Fail now, with a clear message, rather than at the first command.
    rustlink
        .info()
        .await
        .map_err(|error| format!("cannot reach Rustlink at {}: {error}", config.rustlink_url))?;

    let rest = Arc::new(DiscordRest::new(config.token.clone())?);
    let (gateway, ready, message_content) =
        Gateway::connect_with_messages(config.token.clone()).await?;
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    println!("music bot ready as {} (Rustlink at {})", ready.user_id, config.rustlink_url);
    if !message_content {
        println!(
            "note: Message Content intent is off; /play and @bot <link> work, enable Message Content Intent in Discord Developer Portal to auto-play bare links in chat"
        );
    }

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut stream = session.events().await?;
    let (sender, mut events) = mpsc::channel::<Value>(256);
    let reader = tokio::spawn(async move {
        while let Ok(Some(event)) = stream.next().await {
            if sender.send(event).await.is_err() {
                break;
            }
        }
    });

    let mut bot = Bot {
        pairer: VoiceStatePairer::new(&ready.user_id)?,
        application_id: ready.application_id,
        user_id: ready.user_id,
        commands: Arc::new(commands()),
        config,
        rustlink,
        session,
        rest,
        gateway,
        guilds: HashMap::new(),
        channels: HashMap::new(),
        voiced: HashSet::new(),
        joining: HashMap::new(),
        rejoining: HashSet::new(),
        registered_guilds: HashSet::new(),
        idle_since: HashMap::new(),
    };

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let outcome: Result<(), Failure> = loop {
        let step = tokio::select! {
            result = &mut shutdown => match result {
                Ok(()) => Step::Shutdown,
                Err(error) => break Err(error.into()),
            },
            event = bot.gateway.next_event() => Step::Gateway(event),
            event = events.recv() => Step::Rustlink(event),
            _ = tick.tick() => Step::Tick,
        };
        match step {
            Step::Shutdown => break Ok(()),
            Step::Gateway(Ok(Some(event))) => {
                if let Err(error) = bot.on_gateway(event).await {
                    break Err(error);
                }
            }
            Step::Gateway(Ok(None)) => {
                if let Err(error) = bot.gateway.reconnect(None).await {
                    break Err(error.into());
                }
            }
            Step::Gateway(Err(error)) => {
                if let Err(fatal) = bot.gateway.reconnect(Some(&error)).await {
                    break Err(fatal.into());
                }
            }
            Step::Rustlink(None) => break Err("the Rustlink event stream ended".into()),
            Step::Rustlink(Some(event)) => bot.on_rustlink(event).await,
            Step::Tick => bot.leave_when_idle().await,
        }
    };

    bot.shutdown().await;
    reader.abort();
    outcome
}

enum Step {
    Shutdown,
    Gateway(Result<Option<BotEvent>, rustlink_discord_bot::BotError>),
    Rustlink(Option<Value>),
    Tick,
}

struct Bot {
    config: Config,
    rustlink: Client,
    session: Session,
    rest: Arc<DiscordRest>,
    gateway: Gateway,
    pairer: VoiceStatePairer,
    application_id: String,
    user_id: String,
    commands: Arc<Value>,
    guilds: HashMap<String, Guild>,
    /// `guild` -> (`user` -> voice channel the member is in).
    channels: HashMap<String, HashMap<String, String>>,
    /// Guilds whose voice state Rustlink currently holds.
    voiced: HashSet<String>,
    /// Guilds the bot has been asked to join (`guild` -> `(target_channel, requested_at)`).
    joining: HashMap<String, (String, Instant)>,
    /// Guilds cycling `None -> Some(channel)` to force fresh voice credentials without wiping the queue.
    rejoining: HashSet<String>,
    /// Guilds whose slash commands have already been registered in this process.
    registered_guilds: HashSet<String>,
    /// When each guild last ran out of music.
    idle_since: HashMap<String, Instant>,
}

impl Bot {
    fn member_channel(&self, guild: &str, user: &str) -> Option<&str> {
        self.channels
            .get(guild)
            .and_then(|guild_channels| guild_channels.get(user))
            .map(String::as_str)
    }

    fn joining_channel(&self, guild: &str) -> Option<&str> {
        self.joining.get(guild).map(|(channel, _)| channel.as_str())
    }

    async fn on_gateway(&mut self, event: BotEvent) -> Result<(), Failure> {
        if let Some(voice) = self.pairer.observe(&event)? {
            let guild = event.guild_id().unwrap_or_default().to_owned();
            self.voice_ready(&guild, voice).await;
        }
        match event {
            BotEvent::Ready(ready) => self.on_ready(ready).await,
            BotEvent::Other { name, data } if name == "GUILD_CREATE" => {
                self.guild_create(data).await
            }
            BotEvent::Other { name, data } if name == "GUILD_DELETE" => {
                if let Some(guild) = data["id"].as_str() {
                    if !data["unavailable"].as_bool().unwrap_or(false) {
                        self.registered_guilds.remove(guild);
                    }
                    self.channels.remove(guild);
                    let _ = self.pairer.clear(guild);
                    self.forget(guild).await;
                }
            }
            BotEvent::VoiceStateUpdate(update) => self.voice_state(update).await,
            BotEvent::Interaction(request) => self.command(request).await,
            BotEvent::MessageCreate(message) => self.message(message).await,
            _ => {}
        }
        Ok(())
    }

    async fn on_ready(&mut self, ready: rustlink_discord_bot::Ready) {
        self.user_id = ready.user_id;
        if !ready.application_id.is_empty() {
            self.application_id = ready.application_id;
        }
        let mut rejoin_targets: HashMap<String, String> = HashMap::new();
        for (guild, (channel, _)) in &self.joining {
            rejoin_targets.insert(guild.clone(), channel.clone());
        }
        for (guild, state) in &self.guilds {
            if state.busy() && !rejoin_targets.contains_key(guild) {
                if let Some(channel) = self.member_channel(guild, &self.user_id) {
                    rejoin_targets.insert(guild.clone(), channel.to_owned());
                }
            }
        }
        self.voiced.clear();
        self.rejoining.clear();
        let now = Instant::now();
        for (guild, channel) in rejoin_targets {
            self.joining.insert(guild.clone(), (channel.clone(), now));
            if let Err(error) = self.gateway.set_voice_state(&guild, Some(&channel)).await {
                eprintln!("could not re-request voice state in guild {guild} after READY: {error}");
            }
        }
    }

    async fn guild_create(&mut self, data: Value) {
        let Some(guild) = data["id"].as_str().map(str::to_owned) else { return };
        if self.config.serves(&guild) && self.registered_guilds.insert(guild.clone()) {
            let rest = Arc::clone(&self.rest);
            let app_id = self.application_id.clone();
            let cmds = Arc::clone(&self.commands);
            let guild_for_cmds = guild.clone();
            tokio::spawn(async move {
                if let Err(error) = rest.set_guild_commands(&app_id, &guild_for_cmds, &cmds).await {
                    eprintln!("could not register the commands in guild {guild_for_cmds}: {error}");
                }
            });
        }
        let active_voice = self.voiced.contains(&guild) || self.joining.contains_key(&guild);
        let guild_channels = self.channels.entry(guild.clone()).or_default();
        guild_channels.clear();
        let mut stale = false;
        for state in data["voice_states"].as_array().into_iter().flatten() {
            if let (Some(user), Some(channel)) =
                (state["user_id"].as_str(), state["channel_id"].as_str())
            {
                if user == self.user_id && !active_voice {
                    stale = true;
                } else {
                    guild_channels.insert(user.into(), channel.into());
                }
            }
        }
        if stale {
            // A previous run left the bot in a channel Rustlink knows nothing about.
            let _ = self.gateway.set_voice_state(&guild, None).await;
        }
    }

    async fn voice_state(&mut self, update: VoiceStateUpdate) {
        let Some(guild) = update.guild_id else { return };
        match update.channel_id {
            Some(channel) => {
                if update.user_id == self.user_id {
                    self.rejoining.remove(&guild);
                }
                self.channels.entry(guild).or_default().insert(update.user_id, channel);
            }
            None => {
                if let Some(guild_channels) = self.channels.get_mut(&guild) {
                    guild_channels.remove(&update.user_id);
                }
                if update.user_id == self.user_id && !self.rejoining.remove(&guild) {
                    // Left, or removed by someone: what was queued has no channel to play in.
                    self.forget(&guild).await;
                }
            }
        }
    }

    /// Rustlink gets the voice details, and the queue starts when the bot was
    /// waiting to join.
    async fn voice_ready(&mut self, guild: &str, voice: VoiceState) {
        self.rejoining.remove(guild);
        let volume = self.guilds.get(guild).map_or(self.config.volume, |state| state.volume);
        let update = PlayerUpdate {
            voice: Some(voice),
            volume: Some(rustlink_protocol::Int64(volume)),
            ..Default::default()
        };
        if let Err(error) = self.session.update_player(guild, &update, false).await {
            eprintln!("Rustlink refused the voice state for guild {guild}: {error}");
            let _ = self.gateway.set_voice_state(guild, None).await;
            self.forget(guild).await;
            return;
        }
        self.voiced.insert(guild.to_string());
        if self.joining.remove(guild).is_some()
            && !self.guilds.get(guild).is_some_and(Guild::playing)
        {
            self.start_next(guild).await;
        } else if !self.guilds.get(guild).is_some_and(Guild::busy) {
            self.idle_since.entry(guild.to_string()).or_insert_with(Instant::now);
        }
    }

    /// Plays the next waiting track, skipping any Rustlink cannot start.
    async fn start_next(&mut self, guild: &str) {
        loop {
            let Some(state) = self.guilds.get_mut(guild) else { return };
            let Some(track) = state.advance() else {
                self.idle_since.insert(guild.to_string(), Instant::now());
                return;
            };
            let play_id = state.current_play_id().unwrap_or(0);
            let mut user_data = Object::new();
            user_data.insert("play_id".into(), json!(play_id));
            let update = PlayerUpdate {
                track: Patch::Value(TrackUpdate {
                    token: Patch::Value(track.token.clone()),
                    user_data: Some(user_data),
                    ..Default::default()
                }),
                ..Default::default()
            };
            match self.session.update_player(guild, &update, true).await {
                Ok(_) => {
                    self.idle_since.remove(guild);
                    return;
                }
                Err(error) => {
                    eprintln!("could not start {:?}: {error}", track.metadata.title);
                    state.finish();
                    if state.record(false) {
                        eprintln!(
                            "stopped the queue in guild {guild}: {} tracks in a row could not be started",
                            state::MAX_FAILURES_IN_A_ROW
                        );
                        self.idle_since.insert(guild.to_string(), Instant::now());
                        return;
                    }
                }
            }
        }
    }

    async fn on_rustlink(&mut self, event: Value) {
        let guild = event["guild_id"].as_str().unwrap_or_default().to_string();
        match event["type"].as_str() {
            Some("track.started") => {
                if let Some(state) = self.guilds.get_mut(&guild) {
                    state.reset_failures();
                }
            }
            Some("track.ended") => {
                // A track that was replaced or stopped on purpose is not the end of the queue.
                if !matches!(event["reason"].as_str(), Some("finished" | "load_failed")) {
                    return;
                }
                let ended_play_id =
                    event.pointer("/track/user_data/play_id").and_then(Value::as_u64);
                let Some(state) = self.guilds.get_mut(&guild) else { return };
                if state.current_play_id() != ended_play_id || ended_play_id.is_none() {
                    return;
                }
                state.finish();
                if state.record(event["reason"] == "finished") {
                    eprintln!(
                        "stopped the queue in guild {guild}: {} tracks in a row could not be loaded (see the error above)",
                        state::MAX_FAILURES_IN_A_ROW
                    );
                    self.idle_since.insert(guild.clone(), Instant::now());
                    return;
                }
                self.start_next(&guild).await;
            }
            Some("track.failed") => {
                eprintln!("track failed in guild {guild}: {}", event["exception"]["message"]);
            }
            Some("voice.closed") => {
                let code = event["code"].as_u64().unwrap_or(0);
                println!("voice closed in guild {guild}: code {code}");
                if matches!(code, 4014 | 4022) {
                    self.forget(&guild).await;
                } else {
                    self.voiced.remove(&guild);
                    self.idle_since.entry(guild).or_insert_with(Instant::now);
                }
            }
            _ => {}
        }
    }

    async fn command(&mut self, request: Interaction) {
        if let Err(error) = self.rest.defer_interaction(&request.id, &request.token).await {
            eprintln!("could not acknowledge /{}: {error}", request.command);
            return;
        }
        let text = self.run(&request).await;
        // Discord caps a message at 2000 characters.
        let bounded = truncate_chars(&text, 1900);
        if let Err(error) =
            self.rest.edit_interaction_response(&self.application_id, &request.token, bounded).await
        {
            eprintln!("could not answer /{}: {error}", request.command);
        }
    }

    async fn message(&mut self, message: MessageCreate) {
        if message.author_id == self.user_id {
            return;
        }
        let Some(guild) = message.guild_id.as_deref() else { return };
        if !self.config.serves(guild) {
            return;
        }
        let Some((query, mentioned)) = extract_message_query(&message.content, &self.user_id)
        else {
            return;
        };
        let Some(channel) = self.member_channel(guild, &message.author_id).map(str::to_owned)
        else {
            if mentioned {
                self.send_channel_reply(&message.channel_id, "請先加入一個語音頻道。").await;
            }
            return;
        };
        let bot_channel = self.member_channel(guild, &self.user_id);
        let active_channel = bot_channel.or_else(|| self.joining_channel(guild));
        if !mentioned
            && self.guilds.get(guild).is_some_and(Guild::busy)
            && active_channel.is_some_and(|current| current != channel)
        {
            return;
        }
        let text = self.play(guild, &channel, &query).await;
        self.send_channel_reply(&message.channel_id, &text).await;
    }

    async fn send_channel_reply(&self, channel_id: &str, text: &str) {
        let bounded = truncate_chars(text, 1900);
        if let Err(error) = self.rest.send_text(channel_id, bounded).await {
            eprintln!("could not send reply in channel {channel_id}: {error}");
        }
    }

    async fn run(&mut self, request: &Interaction) -> String {
        let Some(guild) = request.guild_id.as_deref() else {
            return "請在伺服器裡使用這個指令。".into();
        };
        match request.command.as_str() {
            "queue" => return self.queue_text(guild),
            "nowplaying" => return self.now_playing(guild).await,
            "play" | "skip" | "pause" | "resume" | "stop" | "volume" => {}
            _ => return "不認得這個指令。".into(),
        }
        let Some(channel) = self.member_channel(guild, &request.user_id).map(str::to_owned) else {
            return "請先加入一個語音頻道。".into();
        };
        if request.command == "play" {
            return self.play(guild, &channel, request.option("query").unwrap_or_default()).await;
        }
        let same_channel = self.member_channel(guild, &self.user_id) == Some(channel.as_str())
            || (request.command == "stop" && self.joining_channel(guild) == Some(channel.as_str()));
        if !same_channel {
            return "請先加入機器人所在的語音頻道。".into();
        }
        match request.command.as_str() {
            "skip" => self.skip(guild).await,
            "pause" => self.pause(guild, true).await,
            "resume" => self.pause(guild, false).await,
            "stop" => self.stop(guild).await,
            _ => self.volume(guild, request.option("level").unwrap_or_default()).await,
        }
    }

    async fn play(&mut self, guild: &str, channel: &str, query: &str) -> String {
        let query = normalize_query(query);
        if query.is_empty() {
            return "請提供連結或關鍵字。".into();
        }
        let bot_channel = self.member_channel(guild, &self.user_id).map(str::to_owned);
        let active_channel = bot_channel.as_deref().or_else(|| self.joining_channel(guild));
        let busy = self.guilds.get(guild).is_some_and(Guild::busy);
        if busy && active_channel.is_some_and(|current| current != channel) {
            return "機器人正在別的語音頻道播放。".into();
        }
        let tracks = match self.resolve(&query).await {
            Ok(tracks) => tracks,
            Err(error) => return format!("找不到可播放的內容：{error}"),
        };
        let need_join = !(self.voiced.contains(guild) && bot_channel.as_deref() == Some(channel));
        let should_send_join = need_join
            && self.joining.get(guild).is_none_or(|(target, since)| {
                target != channel || since.elapsed() >= JOIN_RETRY_AFTER
            });
        if should_send_join {
            if !self.voiced.contains(guild) && bot_channel.as_deref() == Some(channel) {
                let _ = self.pairer.clear(guild);
                self.rejoining.insert(guild.to_string());
                if let Err(error) = self.gateway.set_voice_state(guild, None).await {
                    self.rejoining.remove(guild);
                    return format!("無法加入語音頻道：{error}");
                }
            }
            self.joining.insert(guild.to_string(), (channel.to_string(), Instant::now()));
            if let Err(error) = self.gateway.set_voice_state(guild, Some(channel)).await {
                self.joining.remove(guild);
                self.rejoining.remove(guild);
                return format!("無法加入語音頻道：{error}");
            }
        }
        let total = tracks.len();
        let first = tracks[0].metadata.title.clone();
        let state =
            self.guilds.entry(guild.to_string()).or_insert_with(|| Guild::new(self.config.volume));
        let was_playing = state.playing();
        let was_busy = state.busy();
        let added = state.enqueue(tracks, self.config.max_queue);
        if added == 0 {
            return format!("佇列已滿（上限 {} 首）。", self.config.max_queue);
        }
        let waiting = state.waiting();
        let message = describe_added(&first, total, added, was_busy, waiting);
        if !need_join && !was_playing {
            self.start_next(guild).await;
        }
        message
    }

    async fn resolve(&self, query: &str) -> Result<Vec<Track>, String> {
        let tracks = match self.rustlink.resolve(query).await.map_err(|error| error.to_string())? {
            LoadResult::Track(track) => vec![*track],
            // A search adds the best match, as a person who typed words expects.
            LoadResult::Search(mut tracks) => {
                tracks.truncate(1);
                tracks
            }
            LoadResult::Playlist(playlist) => playlist.tracks,
            LoadResult::Error(error) => return Err(error.message),
            LoadResult::Empty => Vec::new(),
        };
        if tracks.is_empty() {
            Err("沒有相符的結果".into())
        } else {
            Ok(tracks)
        }
    }

    async fn skip(&mut self, guild: &str) -> String {
        let Some(state) = self.guilds.get_mut(guild) else { return "目前沒有播放。".into() };
        let Some(skipped) = state.current().map(|track| track.metadata.title.clone()) else {
            return "目前沒有播放。".into();
        };
        state.finish();
        if state.waiting() == 0 {
            let _ = self.session.stop(guild).await;
            self.idle_since.insert(guild.to_string(), Instant::now());
            format!("已跳過「{skipped}」，佇列已播完。")
        } else {
            self.start_next(guild).await;
            format!("已跳過「{skipped}」。")
        }
    }

    async fn pause(&mut self, guild: &str, paused: bool) -> String {
        if !self.guilds.get(guild).is_some_and(Guild::playing) {
            return "目前沒有播放。".into();
        }
        match self.session.pause(guild, paused).await {
            Ok(_) if paused => "已暫停。".into(),
            Ok(_) => "繼續播放。".into(),
            Err(error) => format!("操作失敗：{error}"),
        }
    }

    async fn stop(&mut self, guild: &str) -> String {
        let _ = self.session.stop(guild).await;
        let _ = self.gateway.set_voice_state(guild, None).await;
        self.forget(guild).await;
        "已停止並離開語音頻道。".into()
    }

    async fn volume(&mut self, guild: &str, level: &str) -> String {
        let Some(volume) = parse_volume(level) else {
            return "音量請輸入 0 到 200 的整數。".into();
        };
        self.guilds
            .entry(guild.to_string())
            .or_insert_with(|| Guild::new(self.config.volume))
            .volume = volume;
        if self.voiced.contains(guild) {
            if let Err(error) = self.session.set_volume(guild, volume).await {
                return format!("設定失敗：{error}");
            }
        }
        format!("音量 {volume}%")
    }

    fn queue_text(&self, guild: &str) -> String {
        let Some(state) = self.guilds.get(guild).filter(|state| state.busy()) else {
            return "佇列是空的。".into();
        };
        let mut out = String::with_capacity(256);
        if let Some(track) = state.current() {
            out.push_str("正在播放：");
            write_describe(&mut out, track);
        }
        if state.waiting() > 0 {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = write!(out, "接下來（共 {} 首）：", state.waiting());
            for (number, track) in state.upcoming(QUEUE_PAGE).enumerate() {
                let _ = write!(out, "\n{}. ", number + 1);
                write_describe(&mut out, track);
            }
            if state.waiting() > QUEUE_PAGE {
                let _ = write!(out, "\n…還有 {} 首", state.waiting() - QUEUE_PAGE);
            }
        }
        out
    }

    async fn now_playing(&self, guild: &str) -> String {
        let Some(track) = self.guilds.get(guild).and_then(Guild::current) else {
            return "目前沒有播放。".into();
        };
        let position = match self.session.player(guild).await {
            Ok(player) => format_length(player.state.position, false),
            Err(_) => "?".into(),
        };
        let length = format_length(track.metadata.length, track.metadata.is_stream);
        let mut text = String::with_capacity(128);
        write_describe(&mut text, track);
        let _ = write!(text, "\n進度：{position} / {length}");
        if let Some(link) = &track.metadata.uri {
            text.push('\n');
            text.push_str(link);
        }
        text
    }

    /// Drops what the bot knew about a guild and releases its Rustlink player.
    async fn forget(&mut self, guild: &str) {
        self.voiced.remove(guild);
        self.joining.remove(guild);
        self.rejoining.remove(guild);
        self.idle_since.remove(guild);
        let _ = self.pairer.clear(guild);
        if let Some(state) = self.guilds.get_mut(guild) {
            state.clear();
        }
        let _ = self.session.destroy_player(guild).await;
    }

    /// Expires stuck voice joins and leaves channels that have been quiet for
    /// `BOT_IDLE_LEAVE_SECONDS`.
    async fn leave_when_idle(&mut self) {
        let expired_joins: Vec<String> = self
            .joining
            .iter()
            .filter(|(_, (_, since))| since.elapsed() >= JOIN_TIMEOUT)
            .map(|(guild, _)| guild.clone())
            .collect();
        for guild in expired_joins {
            eprintln!(
                "giving up on voice join in guild {guild}: no voice setup within {JOIN_TIMEOUT:?}"
            );
            let _ = self.gateway.set_voice_state(&guild, None).await;
            self.forget(&guild).await;
        }

        let limit = self.config.idle_leave;
        if limit.is_zero() {
            return;
        }
        let quiet: Vec<String> = self
            .idle_since
            .iter()
            .filter(|(guild, since)| {
                since.elapsed() >= limit
                    && (!self.guilds.get(*guild).is_some_and(Guild::busy)
                        || (!self.voiced.contains(*guild) && !self.joining.contains_key(*guild)))
            })
            .map(|(guild, _)| guild.clone())
            .collect();
        for guild in quiet {
            println!("leaving guild {guild}: nothing played for {limit:?}");
            let _ = self.gateway.set_voice_state(&guild, None).await;
            self.forget(&guild).await;
        }
    }

    async fn shutdown(&mut self) {
        let guilds: HashSet<String> =
            self.voiced.iter().chain(self.joining.keys()).cloned().collect();
        for guild in guilds {
            let _ = self.session.stop(&guild).await;
            let _ = self.gateway.set_voice_state(&guild, None).await;
        }
        let _ = self.session.destroy().await;
    }
}

/// The reply to `/play`.
fn describe_added(
    first: &str,
    total: usize,
    added: usize,
    was_busy: bool,
    waiting: usize,
) -> String {
    let mut text = match (total, was_busy) {
        (1, false) => format!("播放：{first}"),
        (1, true) => format!("已加入佇列 #{waiting}：{first}"),
        (_, false) => format!("已加入 {added} 首，從「{first}」開始播放"),
        (_, true) => format!("已加入 {added} 首到佇列"),
    };
    if added < total {
        let _ = write!(text, "（佇列已滿，略過 {} 首）", total - added);
    }
    text
}

/// Writes `title — author（3:45）` directly into `out`.
fn write_describe(out: &mut String, track: &Track) {
    let info = &track.metadata;
    let _ = write!(
        out,
        "{} — {}（{}）",
        info.title,
        info.author,
        format_length(info.length, info.is_stream)
    );
}

/// The slash commands registered in each guild.
fn commands() -> Value {
    json!([
        {"name": "play", "description": "播放連結或搜尋關鍵字，加入佇列", "options": [
            {"type": 3, "name": "query", "description": "YouTube／SoundCloud／Spotify 連結或關鍵字", "required": true}
        ]},
        {"name": "skip", "description": "跳過目前的歌曲"},
        {"name": "pause", "description": "暫停"},
        {"name": "resume", "description": "繼續播放"},
        {"name": "stop", "description": "停止、清空佇列並離開語音頻道"},
        {"name": "queue", "description": "顯示佇列"},
        {"name": "nowplaying", "description": "顯示目前播放的歌曲"},
        {"name": "volume", "description": "調整音量（0 到 200）", "options": [
            {"type": 3, "name": "level", "description": "0 到 200，100 是原音量", "required": true}
        ]}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_play_reply_says_what_happened() {
        assert_eq!(describe_added("A", 1, 1, false, 0), "播放：A");
        assert_eq!(describe_added("A", 1, 1, true, 3), "已加入佇列 #3：A");
        assert!(describe_added("A", 5, 5, false, 4).contains("已加入 5 首，從「A」開始播放"));
        assert!(describe_added("A", 5, 3, true, 9).ends_with("（佇列已滿，略過 2 首）"));
    }

    #[test]
    fn every_command_has_a_name_and_a_description_discord_accepts() {
        let commands = commands();
        let names: Vec<&str> = commands
            .as_array()
            .unwrap()
            .iter()
            .map(|command| {
                let description = command["description"].as_str().unwrap();
                assert!((1..=100).contains(&description.chars().count()), "{description}");
                command["name"].as_str().unwrap()
            })
            .collect();
        assert_eq!(
            names,
            ["play", "skip", "pause", "resume", "stop", "queue", "nowplaying", "volume"]
        );
    }
}
