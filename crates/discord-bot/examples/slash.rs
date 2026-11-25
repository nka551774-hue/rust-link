//! `/play` and direct-link Discord bot backed by Rustlink.
//!
//! Required environment: DISCORD_BOT_TOKEN, RUSTLINK_PASSWORD.
//! Optional: RUSTLINK_URL (default http://127.0.0.1:2333).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustlink_client::{Client, LoadResult, Patch, PlayerUpdate, Session, Track, TrackUpdate};
use rustlink_discord_bot::{
    extract_message_query, extract_url_timestamp_ms, normalize_query, truncate_chars, BotEvent,
    DiscordRest, Gateway, Interaction, VoiceStatePairer,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

enum PendingPlay {
    Slash(Interaction),
    Message { channel_id: String, query: String },
}

struct WaitingJoin {
    request: PendingPlay,
    channel_id: String,
    started_at: Instant,
}

struct SlashBot {
    rustlink: Client,
    session: Session,
    rest: Arc<DiscordRest>,
    gateway: Gateway,
    pairer: VoiceStatePairer,
    user_id: String,
    application_id: String,
    commands: Arc<Value>,
    channels: HashMap<String, HashMap<String, String>>,
    voiced: HashSet<String>,
    rejoining: HashSet<String>,
    registered_guilds: HashSet<String>,
    waiting: HashMap<String, WaitingJoin>,
}

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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_BOT_TOKEN")?;
    let rustlink = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let rest = Arc::new(DiscordRest::new(token.clone())?);
    let (gateway, ready, message_content) = Gateway::connect_with_messages(token).await?;
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    println!("Discord bot READY as {}", ready.user_id);
    if !message_content {
        println!(
            "note: Message Content intent is off; /play and @bot <link> work, enable Message Content Intent in Discord Developer Portal to auto-play bare links"
        );
    }

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut rustlink_events = session.events().await?;
    let (event_tx, mut rustlink_rx) = mpsc::channel::<Value>(256);
    let event_task = tokio::spawn(async move {
        while let Ok(Some(event)) = rustlink_events.next().await {
            println!("rustlink event: {event}");
            if event_tx.send(event).await.is_err() {
                break;
            }
        }
    });

    let mut bot = SlashBot {
        pairer: VoiceStatePairer::new(&ready.user_id)?,
        user_id: ready.user_id,
        application_id: ready.application_id,
        commands: Arc::new(json!([{
            "name": "play",
            "description": "播放連結或搜尋關鍵字",
            "options": [{
                "type": 3,
                "name": "url",
                "description": "YouTube／SoundCloud／Spotify 連結或關鍵字",
                "required": true
            }]
        }])),
        rustlink,
        session,
        rest,
        gateway,
        channels: HashMap::new(),
        voiced: HashSet::new(),
        rejoining: HashSet::new(),
        registered_guilds: HashSet::new(),
        waiting: HashMap::new(),
    };

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let outcome: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            res = &mut shutdown => break res.map_err(Into::into),
            maybe_event = rustlink_rx.recv() => {
                let Some(ev) = maybe_event else { break Err("the Rustlink event stream ended".into()); };
                if ev["type"].as_str() == Some("voice.closed") {
                    if let Some(guild) = ev["guild_id"].as_str() {
                        bot.voiced.remove(guild);
                        if matches!(ev["code"].as_u64(), Some(4014 | 4022)) {
                            let _ = bot.pairer.clear(guild);
                            let _ = bot.session.destroy_player(guild).await;
                        }
                    }
                }
            }
            _ = tick.tick() => bot.expire_joins().await,
            event = bot.gateway.next_event() => match event {
                Ok(Some(ev)) => bot.on_event(ev).await,
                Ok(None) => if let Err(e) = bot.gateway.reconnect(None).await { break Err(e.into()); },
                Err(e) => if let Err(fatal) = bot.gateway.reconnect(Some(&e)).await { break Err(fatal.into()); },
            },
        }
    };

    let active: HashSet<String> = bot.voiced.into_iter().chain(bot.waiting.into_keys()).collect();
    for guild in &active {
        let _ = bot.gateway.set_voice_state(guild, None).await;
    }
    let _ = bot.session.destroy().await;
    event_task.abort();
    outcome
}

impl SlashBot {
    fn member_channel(&self, guild: &str, user: &str) -> Option<&str> {
        self.channels.get(guild).and_then(|m| m.get(user)).map(String::as_str)
    }

    async fn expire_joins(&mut self) {
        let expired: Vec<String> = self
            .waiting
            .iter()
            .filter(|(_, item)| item.started_at.elapsed() >= JOIN_TIMEOUT)
            .map(|(g, _)| g.clone())
            .collect();
        for guild in expired {
            self.rejoining.remove(&guild);
            let _ = self.pairer.clear(&guild);
            let _ = self.gateway.set_voice_state(&guild, None).await;
            if let Some(timed_out) = self.waiting.remove(&guild) {
                self.reply(
                    &timed_out.request,
                    "加入語音頻道逾時（請確認機器人具備連線權限且頻道未滿）。",
                )
                .await;
            }
        }
    }

    async fn request_play(&mut self, guild: String, channel: String, pending: PendingPlay) {
        let bot_ch = self.member_channel(&guild, &self.user_id).map(str::to_owned);
        if self.voiced.contains(&guild) && bot_ch.as_deref() == Some(channel.as_str()) {
            self.play(&guild, &pending).await;
            return;
        }
        if !self.voiced.contains(&guild) && bot_ch.as_deref() == Some(channel.as_str()) {
            let _ = self.pairer.clear(&guild);
            self.rejoining.insert(guild.clone());
            if let Err(e) = self.gateway.set_voice_state(&guild, None).await {
                self.rejoining.remove(&guild);
                self.reply(&pending, &format!("無法加入語音頻道：{e}")).await;
                return;
            }
        }
        if let Some(prev) = self.waiting.insert(
            guild.clone(),
            WaitingJoin {
                request: pending,
                channel_id: channel.clone(),
                started_at: Instant::now(),
            },
        ) {
            self.reply(&prev.request, "已被新的點播請求取代。").await;
        }
        if let Err(e) = self.gateway.set_voice_state(&guild, Some(&channel)).await {
            self.rejoining.remove(&guild);
            if let Some(failed) = self.waiting.remove(&guild) {
                self.reply(&failed.request, &format!("無法加入語音頻道：{e}")).await;
            }
        }
    }

    async fn on_event(&mut self, event: BotEvent) {
        match self.pairer.observe(&event) {
            Ok(Some(voice)) => {
                let guild = event.guild_id().unwrap_or_default().to_owned();
                self.rejoining.remove(&guild);
                match self.session.set_voice(&guild, voice).await {
                    Ok(_) => {
                        self.voiced.insert(guild.clone());
                        if let Some(pending) = self.waiting.remove(&guild) {
                            self.play(&guild, &pending.request).await;
                        }
                    }
                    Err(e) => {
                        eprintln!("could not set voice state in guild {guild}: {e}");
                        self.voiced.remove(&guild);
                        let _ = self.gateway.set_voice_state(&guild, None).await;
                        if let Some(pending) = self.waiting.remove(&guild) {
                            self.reply(&pending.request, &format!("語音連線失敗：{e}")).await;
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("ignoring malformed voice event: {e}"),
        }

        match event {
            BotEvent::Ready(new_ready) => {
                self.user_id = new_ready.user_id;
                if !new_ready.application_id.is_empty() {
                    self.application_id = new_ready.application_id;
                }
                self.voiced.clear();
                self.rejoining.clear();
                let now = Instant::now();
                for (guild, item) in &mut self.waiting {
                    item.started_at = now;
                    if let Err(e) =
                        self.gateway.set_voice_state(guild, Some(&item.channel_id)).await
                    {
                        eprintln!("could not re-request voice in guild {guild} after READY: {e}");
                    }
                }
            }
            BotEvent::Other { name, data } if name == "GUILD_CREATE" => {
                let Some(guild) = data["id"].as_str().map(str::to_owned) else { return };
                if self.registered_guilds.insert(guild.clone()) {
                    let (rest, app_id, cmds, g) = (
                        Arc::clone(&self.rest),
                        self.application_id.clone(),
                        Arc::clone(&self.commands),
                        guild.clone(),
                    );
                    tokio::spawn(async move {
                        if let Err(e) = rest.set_guild_commands(&app_id, &g, &cmds).await {
                            eprintln!("could not register /play in guild {g}: {e}");
                        }
                    });
                }
                let guild_channels = self.channels.entry(guild).or_default();
                guild_channels.clear();
                for st in data["voice_states"].as_array().into_iter().flatten() {
                    if let (Some(u), Some(ch)) = (st["user_id"].as_str(), st["channel_id"].as_str())
                    {
                        guild_channels.insert(u.into(), ch.into());
                    }
                }
            }
            BotEvent::Other { name, data } if name == "GUILD_DELETE" => {
                if let Some(guild) = data["id"].as_str() {
                    if !data["unavailable"].as_bool().unwrap_or(false) {
                        self.registered_guilds.remove(guild);
                    }
                    self.channels.remove(guild);
                    self.voiced.remove(guild);
                    self.rejoining.remove(guild);
                    self.waiting.remove(guild);
                    let _ = self.pairer.clear(guild);
                    let _ = self.session.destroy_player(guild).await;
                }
            }
            BotEvent::VoiceStateUpdate(update) => {
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
                            self.voiced.remove(&guild);
                            let _ = self.pairer.clear(&guild);
                            let _ = self.session.destroy_player(&guild).await;
                            if let Some(pending) = self.waiting.remove(&guild) {
                                self.reply(&pending.request, "已從語音頻道中斷連線。").await;
                            }
                        }
                    }
                }
            }
            BotEvent::Interaction(req) if req.command == "play" => {
                if let Err(e) = self.rest.defer_interaction(&req.id, &req.token).await {
                    eprintln!("could not acknowledge /play: {e}");
                    return;
                }
                let guild = req.guild_id.clone();
                let user_ch = guild
                    .as_deref()
                    .and_then(|g| self.member_channel(g, &req.user_id))
                    .map(str::to_owned);
                let pending = PendingPlay::Slash(req);
                let Some(guild) = guild else {
                    self.reply(&pending, "請在伺服器裡使用 /play。").await;
                    return;
                };
                let Some(channel) = user_ch else {
                    self.reply(&pending, "請先加入一個語音頻道。").await;
                    return;
                };
                self.request_play(guild, channel, pending).await;
            }
            BotEvent::MessageCreate(msg) if msg.author_id != self.user_id => {
                let Some(guild) = msg.guild_id else { return };
                let Some((query, mentioned)) = extract_message_query(&msg.content, &self.user_id)
                else {
                    return;
                };
                let Some(channel) = self.member_channel(&guild, &msg.author_id).map(str::to_owned)
                else {
                    if mentioned {
                        send_text(&self.rest, &msg.channel_id, "請先加入一個語音頻道。").await;
                    }
                    return;
                };
                let pending =
                    PendingPlay::Message { channel_id: msg.channel_id, query: query.into_owned() };
                self.request_play(guild, channel, pending).await;
            }
            _ => {}
        }
    }

    async fn play(&self, guild: &str, request: &PendingPlay) {
        let query = match request {
            PendingPlay::Slash(ix) => ix.option("url").unwrap_or_default(),
            PendingPlay::Message { query, .. } => query.as_str(),
        };
        let requested_start = extract_url_timestamp_ms(query);
        let msg = match resolve(&self.rustlink, query).await {
            Ok(track) => {
                let start_ms = requested_start.filter(|&ms| {
                    track.metadata.is_seekable
                        && (track.metadata.length <= 0 || ms < track.metadata.length)
                });
                let res = match start_ms {
                    Some(pos) => {
                        self.session
                            .update_player(
                                guild,
                                &PlayerUpdate {
                                    track: Patch::Value(TrackUpdate {
                                        token: Patch::Value(track.token.clone()),
                                        ..Default::default()
                                    }),
                                    position: Some(pos.into()),
                                    ..Default::default()
                                },
                                true,
                            )
                            .await
                    }
                    None => self.session.play(guild, &track.token).await,
                };
                match res {
                    Ok(_) => {
                        format!("正在播放：{}（{}）", track.metadata.title, track.metadata.author)
                    }
                    Err(e) => format!("播放失敗：{e}"),
                }
            }
            Err(e) => format!("找不到可播放的內容：{e}"),
        };
        self.reply(request, &msg).await;
    }

    async fn reply(&self, request: &PendingPlay, text: &str) {
        let bounded = truncate_chars(text, 1900);
        match request {
            PendingPlay::Slash(ix) => {
                if let Err(e) = self
                    .rest
                    .edit_interaction_response(&self.application_id, &ix.token, bounded)
                    .await
                {
                    eprintln!("could not answer /play: {e}");
                }
            }
            PendingPlay::Message { channel_id, .. } => {
                send_text(&self.rest, channel_id, bounded).await;
            }
        }
    }
}

async fn resolve(rustlink: &Client, query: &str) -> Result<Track, Box<dyn std::error::Error>> {
    let query = normalize_query(query);
    if query.is_empty() {
        return Err("請提供連結或關鍵字".into());
    }
    let track = match rustlink.resolve(&query).await? {
        LoadResult::Track(t) => Some(*t),
        LoadResult::Search(t) => t.into_iter().next(),
        LoadResult::Playlist(p) => p.tracks.into_iter().next(),
        LoadResult::Error(e) => return Err(e.message.into()),
        LoadResult::Empty => None,
    };
    track.ok_or_else(|| "連結沒有解析出曲目".into())
}

async fn send_text(rest: &DiscordRest, channel_id: &str, text: &str) {
    if let Err(e) = rest.send_text(channel_id, truncate_chars(text, 1900)).await {
        eprintln!("could not send reply in channel {channel_id}: {e}");
    }
}
