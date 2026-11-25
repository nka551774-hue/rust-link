//! `/play` and direct-link Discord bot. Join a voice channel, paste a YouTube /
//! SoundCloud / Spotify / Deezer link directly in chat (or `@Bot <link / query>`,
//! or `/play url:<link>`), and the bot joins that channel and plays through
//! Rustlink. The link is resolved by Rustlink's native Rust source layer;
//! nothing runs yt-dlp.
//!
//! Required environment: DISCORD_BOT_TOKEN, RUSTLINK_PASSWORD. Optional:
//! RUSTLINK_URL (default http://127.0.0.1:2333). The command is registered in
//! every guild the bot is in, so it appears without the global-command delay.

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_BOT_TOKEN")?;
    let rustlink = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let rest = Arc::new(DiscordRest::new(token.clone())?);
    let (mut gateway, ready, message_content) = Gateway::connect_with_messages(token).await?;
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    let mut user_id = ready.user_id;
    let mut application_id = ready.application_id;
    println!("Discord bot READY as {user_id}");
    if !message_content {
        println!(
            "note: Message Content intent is off; /play and @bot <link> work, enable Message Content Intent in Discord Developer Portal to auto-play bare links in chat"
        );
    }

    let session = rustlink.create_session(&user_id, 60).await?;
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

    let commands: Arc<Value> = Arc::new(json!([{
        "name": "play",
        "description": "播放連結或搜尋關鍵字",
        "options": [{
            "type": 3,
            "name": "url",
            "description": "YouTube／SoundCloud／Spotify 連結或關鍵字",
            "required": true
        }]
    }]));
    let mut pairer = VoiceStatePairer::new(&user_id)?;
    // guild -> (user -> voice channel), for every member Discord reports.
    let mut channels: HashMap<String, HashMap<String, String>> = HashMap::new();
    // Guilds whose voice state Rustlink currently holds.
    let mut voiced: HashSet<String> = HashSet::new();
    // Guilds cycling `None -> Some(channel)` so the intermediate `None` is not treated as a kick.
    let mut rejoining: HashSet<String> = HashSet::new();
    // Guilds whose `/play` command has already been registered.
    let mut registered_guilds: HashSet<String> = HashSet::new();
    // A request waiting for the bot to finish joining: guild -> pending request.
    let mut waiting: HashMap<String, WaitingJoin> = HashMap::new();

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let outcome: Result<(), Box<dyn std::error::Error>> = loop {
        let event = tokio::select! {
            result = &mut shutdown => {
                if let Err(error) = result {
                    break Err(error.into());
                }
                break Ok(());
            }
            maybe_event = rustlink_rx.recv() => {
                let Some(rl_event) = maybe_event else {
                    break Err("the Rustlink event stream ended".into());
                };
                if rl_event["type"].as_str() == Some("voice.closed") {
                    if let Some(guild) = rl_event["guild_id"].as_str() {
                        voiced.remove(guild);
                        let code = rl_event["code"].as_u64().unwrap_or(0);
                        if matches!(code, 4014 | 4022) {
                            let _ = pairer.clear(guild);
                            let _ = session.destroy_player(guild).await;
                        }
                    }
                }
                continue;
            }
            _ = tick.tick() => {
                let expired: Vec<String> = waiting
                    .iter()
                    .filter(|(_, item)| item.started_at.elapsed() >= JOIN_TIMEOUT)
                    .map(|(guild, _)| guild.clone())
                    .collect();
                for guild in expired {
                    rejoining.remove(&guild);
                    let _ = pairer.clear(&guild);
                    let _ = gateway.set_voice_state(&guild, None).await;
                    if let Some(timed_out) = waiting.remove(&guild) {
                        reply(
                            &rest,
                            &application_id,
                            &timed_out.request,
                            "加入語音頻道逾時（請確認機器人具備連線權限且頻道未滿）。",
                        )
                        .await;
                    }
                }
                continue;
            }
            event = gateway.next_event() => match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    if let Err(error) = gateway.reconnect(None).await {
                        break Err(error.into());
                    }
                    continue;
                }
                Err(error) => {
                    if let Err(fatal) = gateway.reconnect(Some(&error)).await {
                        break Err(fatal.into());
                    }
                    continue;
                }
            },
        };
        match pairer.observe(&event) {
            Ok(Some(voice)) => {
                let guild = event.guild_id().unwrap_or_default().to_owned();
                rejoining.remove(&guild);
                match session.set_voice(&guild, voice).await {
                    Ok(_) => {
                        voiced.insert(guild.clone());
                        if let Some(pending) = waiting.remove(&guild) {
                            play(
                                &rustlink,
                                &session,
                                &rest,
                                &application_id,
                                &guild,
                                &pending.request,
                            )
                            .await;
                        }
                    }
                    Err(error) => {
                        eprintln!("could not set voice state in guild {guild}: {error}");
                        voiced.remove(&guild);
                        let _ = gateway.set_voice_state(&guild, None).await;
                        if let Some(pending) = waiting.remove(&guild) {
                            reply(
                                &rest,
                                &application_id,
                                &pending.request,
                                &format!("語音連線失敗：{error}"),
                            )
                            .await;
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("ignoring malformed voice event: {error}");
            }
        }
        match event {
            BotEvent::Ready(new_ready) => {
                user_id = new_ready.user_id;
                if !new_ready.application_id.is_empty() {
                    application_id = new_ready.application_id;
                }
                voiced.clear();
                rejoining.clear();
                let now = Instant::now();
                for (guild, item) in &mut waiting {
                    item.started_at = now;
                    if let Err(error) = gateway.set_voice_state(guild, Some(&item.channel_id)).await
                    {
                        eprintln!(
                            "could not re-request voice in guild {guild} after READY: {error}"
                        );
                    }
                }
            }
            BotEvent::Other { name, data } if name == "GUILD_CREATE" => {
                let Some(guild) = data["id"].as_str().map(str::to_owned) else { continue };
                if registered_guilds.insert(guild.clone()) {
                    let rest_clone = Arc::clone(&rest);
                    let app_id = application_id.clone();
                    let cmds = Arc::clone(&commands);
                    let guild_for_cmds = guild.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            rest_clone.set_guild_commands(&app_id, &guild_for_cmds, &cmds).await
                        {
                            eprintln!(
                                "could not register /play in guild {guild_for_cmds}: {error}"
                            );
                        }
                    });
                }
                let guild_channels = channels.entry(guild).or_default();
                guild_channels.clear();
                for state in data["voice_states"].as_array().into_iter().flatten() {
                    if let (Some(user), Some(channel)) =
                        (state["user_id"].as_str(), state["channel_id"].as_str())
                    {
                        guild_channels.insert(user.into(), channel.into());
                    }
                }
            }
            BotEvent::Other { name, data } if name == "GUILD_DELETE" => {
                if let Some(guild) = data["id"].as_str() {
                    if !data["unavailable"].as_bool().unwrap_or(false) {
                        registered_guilds.remove(guild);
                    }
                    channels.remove(guild);
                    voiced.remove(guild);
                    rejoining.remove(guild);
                    waiting.remove(guild);
                    let _ = pairer.clear(guild);
                    let _ = session.destroy_player(guild).await;
                }
            }
            BotEvent::VoiceStateUpdate(update) => {
                let Some(guild) = update.guild_id else { continue };
                match update.channel_id {
                    Some(channel) => {
                        if update.user_id == user_id {
                            rejoining.remove(&guild);
                        }
                        channels.entry(guild).or_default().insert(update.user_id, channel);
                    }
                    None => {
                        if let Some(guild_channels) = channels.get_mut(&guild) {
                            guild_channels.remove(&update.user_id);
                        }
                        if update.user_id == user_id && !rejoining.remove(&guild) {
                            voiced.remove(&guild);
                            let _ = pairer.clear(&guild);
                            let _ = session.destroy_player(&guild).await;
                            if let Some(pending) = waiting.remove(&guild) {
                                reply(
                                    &rest,
                                    &application_id,
                                    &pending.request,
                                    "已從語音頻道中斷連線。",
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            BotEvent::Interaction(request) if request.command == "play" => {
                if let Err(error) = rest.defer_interaction(&request.id, &request.token).await {
                    eprintln!("could not acknowledge /play: {error}");
                    continue;
                }
                let pending = PendingPlay::Slash(request);
                let PendingPlay::Slash(ref req) = pending else { unreachable!() };
                let Some(guild) = req.guild_id.clone() else {
                    reply(&rest, &application_id, &pending, "請在伺服器裡使用 /play。").await;
                    continue;
                };
                let Some(channel) = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&req.user_id))
                    .cloned()
                else {
                    reply(&rest, &application_id, &pending, "請先加入一個語音頻道。").await;
                    continue;
                };
                let bot_channel = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&user_id))
                    .cloned();
                if voiced.contains(&guild) && bot_channel.as_deref() == Some(channel.as_str()) {
                    play(&rustlink, &session, &rest, &application_id, &guild, &pending).await;
                } else {
                    if !voiced.contains(&guild) && bot_channel.as_deref() == Some(channel.as_str())
                    {
                        let _ = pairer.clear(&guild);
                        rejoining.insert(guild.clone());
                        if let Err(error) = gateway.set_voice_state(&guild, None).await {
                            rejoining.remove(&guild);
                            reply(
                                &rest,
                                &application_id,
                                &pending,
                                &format!("無法加入語音頻道：{error}"),
                            )
                            .await;
                            continue;
                        }
                    }
                    if let Some(previous) = waiting.insert(
                        guild.clone(),
                        WaitingJoin {
                            request: pending,
                            channel_id: channel.clone(),
                            started_at: Instant::now(),
                        },
                    ) {
                        reply(&rest, &application_id, &previous.request, "已被新的點播請求取代。")
                            .await;
                    }
                    if let Err(error) = gateway.set_voice_state(&guild, Some(&channel)).await {
                        rejoining.remove(&guild);
                        if let Some(failed) = waiting.remove(&guild) {
                            reply(
                                &rest,
                                &application_id,
                                &failed.request,
                                &format!("無法加入語音頻道：{error}"),
                            )
                            .await;
                        }
                    }
                }
            }
            BotEvent::MessageCreate(message) => {
                if message.author_id == user_id {
                    continue;
                }
                let Some(guild) = message.guild_id else { continue };
                let Some((query, mentioned)) = extract_message_query(&message.content, &user_id)
                else {
                    continue;
                };
                let Some(channel) = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&message.author_id))
                    .cloned()
                else {
                    if mentioned {
                        send_text(&rest, &message.channel_id, "請先加入一個語音頻道。").await;
                    }
                    continue;
                };
                let pending = PendingPlay::Message {
                    channel_id: message.channel_id,
                    query: query.into_owned(),
                };
                let bot_channel = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&user_id))
                    .cloned();
                if voiced.contains(&guild) && bot_channel.as_deref() == Some(channel.as_str()) {
                    play(&rustlink, &session, &rest, &application_id, &guild, &pending).await;
                } else {
                    if !voiced.contains(&guild) && bot_channel.as_deref() == Some(channel.as_str())
                    {
                        let _ = pairer.clear(&guild);
                        rejoining.insert(guild.clone());
                        if let Err(error) = gateway.set_voice_state(&guild, None).await {
                            rejoining.remove(&guild);
                            reply(
                                &rest,
                                &application_id,
                                &pending,
                                &format!("無法加入語音頻道：{error}"),
                            )
                            .await;
                            continue;
                        }
                    }
                    if let Some(previous) = waiting.insert(
                        guild.clone(),
                        WaitingJoin {
                            request: pending,
                            channel_id: channel.clone(),
                            started_at: Instant::now(),
                        },
                    ) {
                        reply(&rest, &application_id, &previous.request, "已被新的點播請求取代。")
                            .await;
                    }
                    if let Err(error) = gateway.set_voice_state(&guild, Some(&channel)).await {
                        rejoining.remove(&guild);
                        if let Some(failed) = waiting.remove(&guild) {
                            reply(
                                &rest,
                                &application_id,
                                &failed.request,
                                &format!("無法加入語音頻道：{error}"),
                            )
                            .await;
                        }
                    }
                }
            }
            _ => {}
        }
    };
    let active_guilds: HashSet<String> = voiced.into_iter().chain(waiting.into_keys()).collect();
    for guild in &active_guilds {
        let _ = gateway.set_voice_state(guild, None).await;
    }
    let _ = session.destroy().await;
    event_task.abort();
    outcome
}

/// Resolves the query natively in Rustlink, plays the first playable track,
/// and sends or edits the reply with the outcome.
async fn play(
    rustlink: &Client,
    session: &Session,
    rest: &DiscordRest,
    application_id: &str,
    guild: &str,
    request: &PendingPlay,
) {
    let query = match request {
        PendingPlay::Slash(interaction) => interaction.option("url").unwrap_or_default(),
        PendingPlay::Message { query, .. } => query.as_str(),
    };
    let requested_start = extract_url_timestamp_ms(query);
    let message = match resolve(rustlink, query).await {
        Ok(track) => {
            let start_ms = requested_start.filter(|&ms| {
                track.metadata.is_seekable
                    && (track.metadata.length <= 0 || ms < track.metadata.length)
            });
            let started = match start_ms {
                Some(pos) => {
                    session
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
                None => session.play(guild, &track.token).await,
            };
            match started {
                Ok(_) => format!("正在播放：{}（{}）", track.metadata.title, track.metadata.author),
                Err(error) => format!("播放失敗：{error}"),
            }
        }
        Err(error) => format!("找不到可播放的內容：{error}"),
    };
    reply(rest, application_id, request, &message).await;
}

async fn resolve(rustlink: &Client, query: &str) -> Result<Track, Box<dyn std::error::Error>> {
    let query = normalize_query(query);
    if query.is_empty() {
        return Err("請提供連結或關鍵字".into());
    }
    let track = match rustlink.resolve(&query).await? {
        LoadResult::Track(track) => Some(*track),
        LoadResult::Search(tracks) => tracks.into_iter().next(),
        LoadResult::Playlist(playlist) => playlist.tracks.into_iter().next(),
        LoadResult::Error(error) => return Err(error.message.into()),
        LoadResult::Empty => None,
    };
    track.ok_or_else(|| "連結沒有解析出曲目".into())
}

async fn reply(rest: &DiscordRest, application_id: &str, request: &PendingPlay, text: &str) {
    // Discord caps a message at 2000 characters; a long error must not fail the reply.
    let bounded = truncate_chars(text, 1900);
    match request {
        PendingPlay::Slash(interaction) => {
            if let Err(error) =
                rest.edit_interaction_response(application_id, &interaction.token, bounded).await
            {
                eprintln!("could not answer /play: {error}");
            }
        }
        PendingPlay::Message { channel_id, .. } => send_text(rest, channel_id, bounded).await,
    }
}

async fn send_text(rest: &DiscordRest, channel_id: &str, text: &str) {
    let bounded = truncate_chars(text, 1900);
    if let Err(error) = rest.send_text(channel_id, bounded).await {
        eprintln!("could not send reply in channel {channel_id}: {error}");
    }
}
