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
use std::path::Path;
use std::time::{Duration, Instant};

use rustlink_client::{Client, LoadResult, Session, Track, VoiceState};
use rustlink_discord_bot::{
    BotEvent, DiscordRest, Gateway, Interaction, VoiceStatePairer, VoiceStateUpdate, GUILDS,
    GUILD_VOICE_STATES,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use config::Config;
use state::{format_length, normalize_query, parse_volume, Guild};

type Failure = Box<dyn std::error::Error>;

/// How many waiting tracks `/queue` lists.
const QUEUE_PAGE: usize = 10;

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

    let rest = DiscordRest::new(config.token.clone())?;
    let mut gateway = Gateway::connect(config.token.clone(), GUILDS | GUILD_VOICE_STATES).await?;
    let ready = loop {
        if let BotEvent::Ready(ready) =
            gateway.next_event().await?.ok_or("the Discord gateway closed before READY")?
        {
            break ready;
        }
    };
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    println!("music bot ready as {} (Rustlink at {})", ready.user_id, config.rustlink_url);

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
        config,
        rustlink,
        session,
        rest,
        gateway,
        guilds: HashMap::new(),
        channels: HashMap::new(),
        voiced: HashSet::new(),
        joining: HashSet::new(),
        idle_since: HashMap::new(),
    };

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let outcome: Result<(), Failure> = loop {
        let step = tokio::select! {
            result = &mut shutdown => match result {
                Ok(()) => Step::Shutdown,
                Err(error) => break Err(error.into()),
            },
            event = bot.gateway.next_event() => match event {
                Ok(event) => Step::Gateway(event),
                Err(error) => break Err(error.into()),
            },
            event = events.recv() => Step::Rustlink(event),
            _ = tick.tick() => Step::Tick,
        };
        match step {
            Step::Shutdown => break Ok(()),
            Step::Gateway(None) => break Err("the Discord gateway closed".into()),
            Step::Gateway(Some(event)) => {
                if let Err(error) = bot.on_gateway(event).await {
                    break Err(error);
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
    Gateway(Option<BotEvent>),
    Rustlink(Option<Value>),
    Tick,
}

struct Bot {
    config: Config,
    rustlink: Client,
    session: Session,
    rest: DiscordRest,
    gateway: Gateway,
    pairer: VoiceStatePairer,
    application_id: String,
    user_id: String,
    guilds: HashMap<String, Guild>,
    /// `(guild, user)` to the voice channel the member is in.
    channels: HashMap<(String, String), String>,
    /// Guilds whose voice state Rustlink currently holds.
    voiced: HashSet<String>,
    /// Guilds the bot has been asked to join, and should play in once it lands.
    joining: HashSet<String>,
    /// When each guild last ran out of music.
    idle_since: HashMap<String, Instant>,
}

impl Bot {
    async fn on_gateway(&mut self, event: BotEvent) -> Result<(), Failure> {
        if let Some(voice) = self.pairer.observe(&event)? {
            let guild = match &event {
                BotEvent::VoiceStateUpdate(update) => update.guild_id.clone().unwrap_or_default(),
                BotEvent::VoiceServerUpdate(update) => update.guild_id.clone(),
                _ => String::new(),
            };
            self.voice_ready(&guild, voice).await;
        }
        match event {
            BotEvent::Other { name, data } if name == "GUILD_CREATE" => {
                self.guild_create(data).await
            }
            BotEvent::VoiceStateUpdate(update) => self.voice_state(update).await,
            BotEvent::Interaction(request) => self.command(request).await,
            _ => {}
        }
        Ok(())
    }

    async fn guild_create(&mut self, data: Value) {
        let Some(guild) = data["id"].as_str().map(str::to_owned) else { return };
        if self.config.serves(&guild) {
            if let Err(error) =
                self.rest.set_guild_commands(&self.application_id, &guild, &commands()).await
            {
                eprintln!("could not register the commands in guild {guild}: {error}");
            }
        }
        let mut stale = false;
        for state in data["voice_states"].as_array().into_iter().flatten() {
            if let (Some(user), Some(channel)) =
                (state["user_id"].as_str(), state["channel_id"].as_str())
            {
                if user == self.user_id {
                    stale = true;
                } else {
                    self.channels.insert((guild.clone(), user.into()), channel.into());
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
        let key = (guild.clone(), update.user_id.clone());
        match update.channel_id {
            Some(channel) => {
                self.channels.insert(key, channel);
            }
            None => {
                self.channels.remove(&key);
                if update.user_id == self.user_id {
                    // Left, or removed by someone: what was queued has no channel to play in.
                    self.forget(&guild).await;
                }
            }
        }
    }

    /// Rustlink gets the voice details, and the queue starts when the bot was
    /// waiting to join.
    async fn voice_ready(&mut self, guild: &str, voice: VoiceState) {
        if let Err(error) = self.session.set_voice(guild, voice).await {
            eprintln!("Rustlink refused the voice state for guild {guild}: {error}");
            return;
        }
        self.voiced.insert(guild.to_string());
        let volume = self.guilds.get(guild).map_or(self.config.volume, |state| state.volume);
        if volume != 100 {
            let _ = self.session.set_volume(guild, volume).await;
        }
        if self.joining.remove(guild) && !self.guilds.get(guild).is_some_and(Guild::playing) {
            self.start_next(guild).await;
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
            match self.session.play(guild, &track.token).await {
                Ok(_) => {
                    self.idle_since.remove(guild);
                    return;
                }
                Err(error) => {
                    eprintln!("could not start {:?}: {error}", track.metadata.title);
                    state.finish();
                }
            }
        }
    }

    async fn on_rustlink(&mut self, event: Value) {
        let guild = event["guild_id"].as_str().unwrap_or_default().to_string();
        match event["type"].as_str() {
            Some("track.ended") => {
                // A track that was replaced or stopped on purpose is not the end of the queue.
                if !matches!(event["reason"].as_str(), Some("finished" | "load_failed")) {
                    return;
                }
                let ended = event.pointer("/track/metadata/identifier").and_then(Value::as_str);
                let Some(state) = self.guilds.get_mut(&guild) else { return };
                if state.current().map(|track| track.metadata.identifier.as_str()) != ended {
                    return;
                }
                state.finish();
                self.start_next(&guild).await;
            }
            Some("track.failed") => {
                eprintln!("track failed in guild {guild}: {}", event["exception"]["message"]);
            }
            Some("voice.closed") => {
                println!("voice closed in guild {guild}: code {}", event["code"]);
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
        let text: String = text.chars().take(1900).collect();
        if let Err(error) =
            self.rest.edit_interaction_response(&self.application_id, &request.token, &text).await
        {
            eprintln!("could not answer /{}: {error}", request.command);
        }
    }

    async fn run(&mut self, request: &Interaction) -> String {
        let Some(guild) = request.guild_id.clone() else {
            return "請在伺服器裡使用這個指令。".into();
        };
        match request.command.as_str() {
            "queue" => return self.queue_text(&guild),
            "nowplaying" => return self.now_playing(&guild).await,
            "play" | "skip" | "pause" | "resume" | "stop" | "volume" => {}
            _ => return "不認得這個指令。".into(),
        }
        let Some(channel) = self.channels.get(&(guild.clone(), request.user_id.clone())).cloned()
        else {
            return "請先加入一個語音頻道。".into();
        };
        if request.command == "play" {
            return self.play(&guild, &channel, request.option("query").unwrap_or_default()).await;
        }
        if self.channels.get(&(guild.clone(), self.user_id.clone())) != Some(&channel) {
            return "請先加入機器人所在的語音頻道。".into();
        }
        match request.command.as_str() {
            "skip" => self.skip(&guild).await,
            "pause" => self.pause(&guild, true).await,
            "resume" => self.pause(&guild, false).await,
            "stop" => self.stop(&guild).await,
            _ => self.volume(&guild, request.option("level").unwrap_or_default()).await,
        }
    }

    async fn play(&mut self, guild: &str, channel: &str, query: &str) -> String {
        let query = normalize_query(query);
        if query.is_empty() {
            return "請提供連結或關鍵字。".into();
        }
        let bot_channel = self.channels.get(&(guild.to_string(), self.user_id.clone())).cloned();
        let busy = self.guilds.get(guild).is_some_and(Guild::busy);
        if busy && bot_channel.as_deref().is_some_and(|current| current != channel) {
            return "機器人正在別的語音頻道播放。".into();
        }
        let tracks = match self.resolve(&query).await {
            Ok(tracks) => tracks,
            Err(error) => return format!("找不到可播放的內容：{error}"),
        };
        let total = tracks.len();
        let first = tracks[0].metadata.title.clone();
        let state =
            self.guilds.entry(guild.to_string()).or_insert_with(|| Guild::new(self.config.volume));
        let was_playing = state.playing();
        let added = state.enqueue(tracks, self.config.max_queue);
        if added == 0 {
            return format!("佇列已滿（上限 {} 首）。", self.config.max_queue);
        }
        let waiting = state.waiting();
        let message = describe_added(&first, total, added, was_playing, waiting);

        if self.voiced.contains(guild) && bot_channel.as_deref() == Some(channel) {
            if !was_playing {
                self.start_next(guild).await;
            }
        } else {
            self.joining.insert(guild.to_string());
            if let Err(error) = self.gateway.set_voice_state(guild, Some(channel)).await {
                self.joining.remove(guild);
                return format!("無法加入語音頻道：{error}");
            }
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
            format!("⏭️ 已跳過「{skipped}」，佇列已播完。")
        } else {
            self.start_next(guild).await;
            format!("⏭️ 已跳過「{skipped}」。")
        }
    }

    async fn pause(&mut self, guild: &str, paused: bool) -> String {
        if !self.guilds.get(guild).is_some_and(Guild::playing) {
            return "目前沒有播放。".into();
        }
        match self.session.pause(guild, paused).await {
            Ok(_) if paused => "⏸️ 已暫停。".into(),
            Ok(_) => "▶️ 繼續播放。".into(),
            Err(error) => format!("操作失敗：{error}"),
        }
    }

    async fn stop(&mut self, guild: &str) -> String {
        let _ = self.session.stop(guild).await;
        let _ = self.gateway.set_voice_state(guild, None).await;
        self.forget(guild).await;
        "⏹️ 已停止並離開語音頻道。".into()
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
        format!("🔊 音量 {volume}%")
    }

    fn queue_text(&self, guild: &str) -> String {
        let Some(state) = self.guilds.get(guild).filter(|state| state.busy()) else {
            return "佇列是空的。".into();
        };
        let mut lines = Vec::new();
        if let Some(track) = state.current() {
            lines.push(format!("🎵 正在播放：{}", describe(track)));
        }
        if state.waiting() > 0 {
            lines.push(format!("📜 接下來（共 {} 首）：", state.waiting()));
            for (number, track) in state.upcoming(QUEUE_PAGE).enumerate() {
                lines.push(format!("{}. {}", number + 1, describe(track)));
            }
            if state.waiting() > QUEUE_PAGE {
                lines.push(format!("…還有 {} 首", state.waiting() - QUEUE_PAGE));
            }
        }
        lines.join("\n")
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
        let mut text = format!("🎵 {}\n⏱️ {position} / {length}", describe(track));
        if let Some(link) = &track.metadata.uri {
            text.push_str(&format!("\n{link}"));
        }
        text
    }

    /// Drops what the bot knew about a guild and releases its Rustlink player.
    async fn forget(&mut self, guild: &str) {
        self.voiced.remove(guild);
        self.joining.remove(guild);
        self.idle_since.remove(guild);
        if let Some(state) = self.guilds.get_mut(guild) {
            state.clear();
        }
        let _ = self.session.destroy_player(guild).await;
    }

    /// Leaves a voice channel that has been quiet for `BOT_IDLE_LEAVE_SECONDS`.
    async fn leave_when_idle(&mut self) {
        let limit = self.config.idle_leave;
        if limit.is_zero() {
            return;
        }
        let quiet: Vec<String> = self
            .idle_since
            .iter()
            .filter(|(guild, since)| {
                since.elapsed() >= limit && !self.guilds.get(*guild).is_some_and(Guild::busy)
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
        let guilds: Vec<String> = self.voiced.iter().cloned().collect();
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
    was_playing: bool,
    waiting: usize,
) -> String {
    let mut text = match (total, was_playing) {
        (1, false) => format!("▶️ 播放：{first}"),
        (1, true) => format!("➕ 已加入佇列 #{waiting}：{first}"),
        (_, false) => format!("▶️ 已加入 {added} 首，從「{first}」開始播放"),
        (_, true) => format!("➕ 已加入 {added} 首到佇列"),
    };
    if added < total {
        text.push_str(&format!("（佇列已滿，略過 {} 首）", total - added));
    }
    text
}

/// `title — author (3:45)`
fn describe(track: &Track) -> String {
    let info = &track.metadata;
    format!("{} — {}（{}）", info.title, info.author, format_length(info.length, info.is_stream))
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
        assert_eq!(describe_added("A", 1, 1, false, 0), "▶️ 播放：A");
        assert_eq!(describe_added("A", 1, 1, true, 3), "➕ 已加入佇列 #3：A");
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
