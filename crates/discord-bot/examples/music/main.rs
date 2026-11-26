//! Production queue music bot for Rustlink.
//!
//! Reads `.env` (or a path passed as the first CLI argument), overridden by real
//! environment variables. Supports `/play`, `/skip`, `/pause`, `/resume`,
//! `/stop`, `/queue`, `/nowplaying`, `/volume`, `@Bot <query>`, and bare music links.

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
    extract_message_query, extract_url_timestamp_ms, truncate_chars, BotEvent, DiscordRest,
    Gateway, Interaction, MessageCreate, VoiceStatePairer, VoiceStateUpdate,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use config::Config;
use state::{format_length, normalize_query, parse_volume, Guild};

type Failure = Box<dyn std::error::Error>;

const QUEUE_PAGE: usize = 10;
const JOIN_RETRY_AFTER: Duration = Duration::from_secs(5);
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => res,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

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
    for attempt in 0..6 {
        match rustlink.info().await {
            Ok(_) => break,
            Err(e) if attempt < 5 && !e.to_string().contains("401") => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                return Err(format!("cannot reach Rustlink at {}: {e}", config.rustlink_url).into());
            }
        }
    }
    if !config.youtube_po_token.is_empty()
        || !config.youtube_gvs_po_token.is_empty()
        || !config.youtube_player_po_token.is_empty()
        || !config.youtube_visitor_data.is_empty()
    {
        rustlink
            .set_youtube_token_bundle(&rustlink_client::YoutubeTokens {
                po_token: &config.youtube_po_token,
                gvs_po_token: &config.youtube_gvs_po_token,
                player_po_token: &config.youtube_player_po_token,
                visitor_data: &config.youtube_visitor_data,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("failed to push YouTube PO token to Rustlink: {e}"))?;
    }

    let rest = Arc::new(DiscordRest::new(config.token.clone())?);
    let (gateway, ready, message_content) =
        Gateway::connect_with_messages(config.token.clone()).await?;
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    println!("music bot ready as {} (Rustlink at {})", ready.user_id, config.rustlink_url);
    if !message_content {
        println!(
            "note: Message Content intent is off; /play and @bot <link> work, enable Message Content Intent in Discord Developer Portal to auto-play bare links"
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

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let outcome: Result<(), Failure> = loop {
        tokio::select! {
            res = &mut shutdown => break res.map_err(Into::into),
            ev = bot.gateway.next_event() => match ev {
                Ok(Some(event)) => if let Err(e) = bot.on_gateway(event).await { break Err(e); },
                Ok(None) => if let Err(e) = bot.gateway.reconnect(None).await { break Err(e.into()); },
                Err(e) => if let Err(fatal) = bot.gateway.reconnect(Some(&e)).await { break Err(fatal.into()); },
            },
            ev = events.recv() => match ev {
                Some(event) => bot.on_rustlink(event).await,
                None => break Err("the Rustlink event stream ended".into()),
            },
            _ = tick.tick() => bot.leave_when_idle().await,
        }
    };

    bot.shutdown().await;
    reader.abort();
    outcome
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
    channels: HashMap<String, HashMap<String, String>>,
    voiced: HashSet<String>,
    joining: HashMap<String, (String, Instant)>,
    rejoining: HashSet<String>,
    registered_guilds: HashSet<String>,
    idle_since: HashMap<String, Instant>,
}

impl Bot {
    fn member_channel(&self, guild: &str, user: &str) -> Option<&str> {
        self.channels.get(guild).and_then(|m| m.get(user)).map(String::as_str)
    }

    fn joining_channel(&self, guild: &str) -> Option<&str> {
        self.joining.get(guild).map(|(ch, _)| ch.as_str())
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
        let mut targets: HashMap<String, String> =
            self.joining.iter().map(|(g, (ch, _))| (g.clone(), ch.clone())).collect();
        for (guild, state) in &self.guilds {
            if state.busy() && !targets.contains_key(guild) {
                if let Some(ch) = self.member_channel(guild, &self.user_id) {
                    targets.insert(guild.clone(), ch.to_owned());
                }
            }
        }
        self.voiced.clear();
        self.rejoining.clear();
        let now = Instant::now();
        for (guild, channel) in targets {
            self.joining.insert(guild.clone(), (channel.clone(), now));
            if let Err(e) = self.gateway.set_voice_state(&guild, Some(&channel)).await {
                eprintln!("could not re-request voice state in guild {guild} after READY: {e}");
            }
        }
    }

    async fn guild_create(&mut self, data: Value) {
        let Some(guild) = data["id"].as_str().map(str::to_owned) else { return };
        if self.config.serves(&guild) && self.registered_guilds.insert(guild.clone()) {
            let (rest, app_id, cmds, g) = (
                Arc::clone(&self.rest),
                self.application_id.clone(),
                Arc::clone(&self.commands),
                guild.clone(),
            );
            tokio::spawn(async move {
                if let Err(e) = rest.set_guild_commands(&app_id, &g, &cmds).await {
                    eprintln!("could not register commands in guild {g}: {e}");
                }
            });
        }
        let active_voice = self.voiced.contains(&guild) || self.joining.contains_key(&guild);
        let guild_channels = self.channels.entry(guild.clone()).or_default();
        guild_channels.clear();
        let mut stale = false;
        for st in data["voice_states"].as_array().into_iter().flatten() {
            if let (Some(u), Some(ch)) = (st["user_id"].as_str(), st["channel_id"].as_str()) {
                if u == self.user_id && !active_voice {
                    stale = true;
                } else {
                    guild_channels.insert(u.into(), ch.into());
                }
            }
        }
        if stale {
            let _ = self.gateway.set_voice_state(&guild, None).await;
        }
    }

    async fn voice_state(&mut self, update: VoiceStateUpdate) {
        let Some(guild) = update.guild_id else { return };
        match update.channel_id {
            Some(ch) => {
                if update.user_id == self.user_id {
                    self.rejoining.remove(&guild);
                }
                self.channels.entry(guild).or_default().insert(update.user_id, ch);
            }
            None => {
                if let Some(m) = self.channels.get_mut(&guild) {
                    m.remove(&update.user_id);
                }
                if update.user_id == self.user_id && !self.rejoining.remove(&guild) {
                    self.forget(&guild).await;
                }
            }
        }
    }

    async fn voice_ready(&mut self, guild: &str, voice: VoiceState) {
        self.rejoining.remove(guild);
        let volume = self.guilds.get(guild).map_or(self.config.volume, |s| s.volume);
        let update = PlayerUpdate {
            voice: Some(voice),
            volume: Some(rustlink_protocol::Int64(volume)),
            ..Default::default()
        };
        if let Err(e) = self.session.update_player(guild, &update, false).await {
            eprintln!("Rustlink refused voice state for guild {guild}: {e}");
            let _ = self.gateway.set_voice_state(guild, None).await;
            self.forget(guild).await;
            return;
        }
        self.voiced.insert(guild.to_owned());
        if self.joining.remove(guild).is_some()
            && !self.guilds.get(guild).is_some_and(Guild::playing)
        {
            self.start_next(guild).await;
        } else if !self.guilds.get(guild).is_some_and(Guild::busy) {
            self.idle_since.entry(guild.to_owned()).or_insert_with(Instant::now);
        }
    }

    async fn start_next(&mut self, guild: &str) {
        loop {
            let Some(state) = self.guilds.get_mut(guild) else { return };
            let Some((track, start_ms)) = state.advance_with_start() else {
                self.idle_since.insert(guild.to_owned(), Instant::now());
                return;
            };
            let mut user_data = Object::new();
            user_data.insert("play_id".into(), json!(state.current_play_id().unwrap_or(0)));
            let update = PlayerUpdate {
                track: Patch::Value(TrackUpdate {
                    token: Patch::Value(track.token.clone()),
                    user_data: Some(user_data),
                    ..Default::default()
                }),
                position: start_ms.map(Into::into),
                ..Default::default()
            };
            match self.session.update_player(guild, &update, true).await {
                Ok(_) => {
                    self.idle_since.remove(guild);
                    return;
                }
                Err(e) => {
                    eprintln!("could not start {:?}: {e}", track.metadata.title);
                    state.finish();
                    if state.record(false) {
                        eprintln!(
                            "stopped queue in guild {guild}: {} consecutive failures",
                            state::MAX_FAILURES_IN_A_ROW
                        );
                        self.idle_since.insert(guild.to_owned(), Instant::now());
                        return;
                    }
                }
            }
        }
    }

    async fn on_rustlink(&mut self, event: Value) {
        let guild = event["guild_id"].as_str().unwrap_or_default().to_owned();
        match event["type"].as_str() {
            Some("track.started") => {
                if let Some(state) = self.guilds.get_mut(&guild) {
                    state.reset_failures();
                }
            }
            Some("track.ended") => {
                if !matches!(event["reason"].as_str(), Some("finished" | "load_failed")) {
                    return;
                }
                let ended_id = event.pointer("/track/user_data/play_id").and_then(Value::as_u64);
                let Some(state) = self.guilds.get_mut(&guild) else { return };
                if ended_id.is_none() || state.current_play_id() != ended_id {
                    return;
                }
                let failed = (event["reason"] == "load_failed").then(|| state.current().cloned());
                state.finish();
                // YouTube refuses some hosts ("confirm you're not a bot"); play the same song
                // from SoundCloud instead of skipping it.
                if let Some(twin) = self.soundcloud_twin(failed.flatten()).await {
                    println!("playing {:?} from SoundCloud instead", twin.metadata.title);
                    if let Some(state) = self.guilds.get_mut(&guild) {
                        state.retry_with(twin);
                    }
                    self.start_next(&guild).await;
                    return;
                }
                let Some(state) = self.guilds.get_mut(&guild) else { return };
                if state.record(event["reason"] == "finished") {
                    eprintln!(
                        "stopped queue in guild {guild}: {} consecutive load failures",
                        state::MAX_FAILURES_IN_A_ROW
                    );
                    self.idle_since.insert(guild, Instant::now());
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
        if let Err(e) = self.rest.defer_interaction(&request.id, &request.token).await {
            eprintln!("could not acknowledge /{}: {e}", request.command);
            return;
        }
        let text = self.run(&request).await;
        if let Err(e) = self
            .rest
            .edit_interaction_response(
                &self.application_id,
                &request.token,
                truncate_chars(&text, 1900),
            )
            .await
        {
            eprintln!("could not answer /{}: {e}", request.command);
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
        let active_ch =
            self.member_channel(guild, &self.user_id).or_else(|| self.joining_channel(guild));
        if !mentioned
            && self.guilds.get(guild).is_some_and(Guild::busy)
            && active_ch.is_some_and(|cur| cur != channel)
        {
            return;
        }
        let text = self.play(guild, &channel, &query).await;
        self.send_channel_reply(&message.channel_id, &text).await;
    }

    async fn send_channel_reply(&self, channel_id: &str, text: &str) {
        if let Err(e) = self.rest.send_text(channel_id, truncate_chars(text, 1900)).await {
            eprintln!("could not send reply in channel {channel_id}: {e}");
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

    async fn play(&mut self, guild: &str, channel: &str, raw_query: &str) -> String {
        let requested_start = extract_url_timestamp_ms(raw_query);
        let query = normalize_query(raw_query);
        if query.is_empty() {
            return "請提供連結或關鍵字。".into();
        }
        let bot_ch = self.member_channel(guild, &self.user_id).map(str::to_owned);
        let active_ch = bot_ch.as_deref().or_else(|| self.joining_channel(guild));
        if self.guilds.get(guild).is_some_and(Guild::busy)
            && active_ch.is_some_and(|cur| cur != channel)
        {
            return "機器人正在別的語音頻道播放。".into();
        }
        let tracks = match self.resolve(&query).await {
            Ok(tracks) => tracks,
            Err(e) => return format!("找不到可播放的內容：{e}"),
        };
        let need_join = !(self.voiced.contains(guild) && bot_ch.as_deref() == Some(channel));
        let should_send_join = need_join
            && self
                .joining
                .get(guild)
                .is_none_or(|(t, since)| t != channel || since.elapsed() >= JOIN_RETRY_AFTER);
        if should_send_join {
            if !self.voiced.contains(guild) && bot_ch.as_deref() == Some(channel) {
                let _ = self.pairer.clear(guild);
                self.rejoining.insert(guild.to_owned());
                if let Err(e) = self.gateway.set_voice_state(guild, None).await {
                    self.rejoining.remove(guild);
                    return format!("無法加入語音頻道：{e}");
                }
            }
            self.joining.insert(guild.to_owned(), (channel.to_owned(), Instant::now()));
            if let Err(e) = self.gateway.set_voice_state(guild, Some(channel)).await {
                self.joining.remove(guild);
                self.rejoining.remove(guild);
                return format!("無法加入語音頻道：{e}");
            }
        }
        let total = tracks.len();
        let first_start = (total == 1).then_some(requested_start).flatten().filter(|&ms| {
            tracks[0].metadata.is_seekable
                && (tracks[0].metadata.length <= 0 || ms < tracks[0].metadata.length)
        });
        let first = tracks[0].metadata.title.clone();
        let volume = self.config.volume;
        let state = self.guilds.entry(guild.to_owned()).or_insert_with(|| Guild::new(volume));
        let (was_playing, was_busy) = (state.playing(), state.busy());
        let added = state.enqueue_with_start(tracks, first_start, self.config.max_queue);
        if added == 0 {
            return format!("佇列已滿（上限 {} 首）。", self.config.max_queue);
        }
        let message = describe_added(&first, total, added, was_busy, state.waiting());
        if !need_join && !was_playing {
            self.start_next(guild).await;
        }
        message
    }

    async fn resolve(&self, query: &str) -> Result<Vec<Track>, String> {
        let tracks = match self.rustlink.resolve(query).await.map_err(|e| e.to_string())? {
            LoadResult::Track(t) => vec![*t],
            LoadResult::Search(mut t) => {
                t.truncate(1);
                t
            }
            LoadResult::Playlist(p) => p.tracks,
            LoadResult::Error(e) => return Err(e.message),
            LoadResult::Empty => Vec::new(),
        };
        (!tracks.is_empty()).then_some(tracks).ok_or_else(|| "沒有相符的結果".into())
    }

    /// The closest SoundCloud match for a YouTube track that could not be loaded.
    async fn soundcloud_twin(&self, failed: Option<Track>) -> Option<Track> {
        let failed = failed.filter(|t| t.metadata.source_name == "youtube")?;
        let query = format!("scsearch:{} {}", failed.metadata.author, failed.metadata.title);
        match self.rustlink.resolve(query.trim()).await.ok()? {
            LoadResult::Search(tracks) => state::stand_in(&failed, tracks),
            _ => None,
        }
    }

    async fn skip(&mut self, guild: &str) -> String {
        let Some(state) = self.guilds.get_mut(guild) else { return "目前沒有播放。".into() };
        let Some(skipped) = state.current().map(|t| t.metadata.title.clone()) else {
            return "目前沒有播放。".into();
        };
        state.finish();
        if state.waiting() == 0 {
            let _ = self.session.stop(guild).await;
            self.idle_since.insert(guild.to_owned(), Instant::now());
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
            Err(e) => format!("操作失敗：{e}"),
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
        let default_vol = self.config.volume;
        self.guilds.entry(guild.to_owned()).or_insert_with(|| Guild::new(default_vol)).volume =
            volume;
        if self.voiced.contains(guild) {
            if let Err(e) = self.session.set_volume(guild, volume).await {
                return format!("設定失敗：{e}");
            }
        }
        format!("音量 {volume}%")
    }

    fn queue_text(&self, guild: &str) -> String {
        let Some(state) = self.guilds.get(guild).filter(|s| s.busy()) else {
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
            for (idx, track) in state.upcoming(QUEUE_PAGE).enumerate() {
                let _ = write!(out, "\n{}. ", idx + 1);
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
        let pos = match self.session.player(guild).await {
            Ok(p) => format_length(p.state.position, false),
            Err(_) => "?".into(),
        };
        let len = format_length(track.metadata.length, track.metadata.is_stream);
        let mut text = String::with_capacity(128);
        write_describe(&mut text, track);
        let _ = write!(text, "\n進度：{pos} / {len}");
        if let Some(link) = &track.metadata.uri {
            let _ = write!(text, "\n{link}");
        }
        text
    }

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

    async fn leave_when_idle(&mut self) {
        let expired: Vec<String> = self
            .joining
            .iter()
            .filter(|(_, (_, since))| since.elapsed() >= JOIN_TIMEOUT)
            .map(|(g, _)| g.clone())
            .collect();
        for guild in expired {
            eprintln!("giving up on voice join in guild {guild} after {JOIN_TIMEOUT:?}");
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
            .filter(|(g, since)| {
                since.elapsed() >= limit
                    && (!self.guilds.get(*g).is_some_and(Guild::busy)
                        || (!self.voiced.contains(*g) && !self.joining.contains_key(*g)))
            })
            .map(|(g, _)| g.clone())
            .collect();
        for guild in quiet {
            println!("leaving guild {guild}: idle for {limit:?}");
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
        let cmds = commands();
        let names: Vec<&str> = cmds
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let desc = c["description"].as_str().unwrap();
                assert!((1..=100).contains(&desc.chars().count()), "{desc}");
                c["name"].as_str().unwrap()
            })
            .collect();
        assert_eq!(
            names,
            ["play", "skip", "pause", "resume", "stop", "queue", "nowplaying", "volume"]
        );
    }
}
