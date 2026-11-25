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

use rustlink_client::{Client, LoadResult, Session, Track};
use rustlink_discord_bot::{
    extract_message_query, normalize_query, truncate_chars, BotEvent, DiscordRest, Gateway,
    Interaction, VoiceStatePairer,
};
use serde_json::{json, Value};

enum PendingPlay {
    Slash(Interaction),
    Message { channel_id: String, query: String },
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
    println!("Discord bot READY as {}", ready.user_id);
    if !message_content {
        println!(
            "note: Message Content intent is off; /play and @bot <link> work, enable Message Content Intent in Discord Developer Portal to auto-play bare links in chat"
        );
    }

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut rustlink_events = session.events().await?;
    let mut event_task = tokio::spawn(async move {
        while let Some(event) = rustlink_events.next().await? {
            println!("rustlink event: {event}");
        }
        Ok::<_, rustlink_client::Error>(())
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
    let mut pairer = VoiceStatePairer::new(&ready.user_id)?;
    // guild -> (user -> voice channel), for every member Discord reports.
    let mut channels: HashMap<String, HashMap<String, String>> = HashMap::new();
    // Guilds whose voice state Rustlink currently holds.
    let mut voiced: HashSet<String> = HashSet::new();
    // A request waiting for the bot to finish joining: guild -> pending request.
    let mut waiting: HashMap<String, PendingPlay> = HashMap::new();

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        let event = tokio::select! {
            result = &mut shutdown => { result?; break; }
            ended = &mut event_task => { ended??; break; }
            event = gateway.next_event() => match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    gateway.reconnect(None).await?;
                    continue;
                }
                Err(error) => {
                    gateway.reconnect(Some(&error)).await?;
                    continue;
                }
            },
        };
        if let Some(voice) = pairer.observe(&event)? {
            let guild = event.guild_id().unwrap_or_default().to_owned();
            session.set_voice(&guild, voice).await?;
            voiced.insert(guild.clone());
            if let Some(request) = waiting.remove(&guild) {
                play(&rustlink, &session, &rest, &ready.application_id, &guild, &request).await;
            }
        }
        match event {
            BotEvent::Other { name, data } if name == "GUILD_CREATE" => {
                let Some(guild) = data["id"].as_str().map(str::to_owned) else { continue };
                let rest_clone = Arc::clone(&rest);
                let app_id = ready.application_id.clone();
                let cmds = Arc::clone(&commands);
                let guild_for_cmds = guild.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        rest_clone.set_guild_commands(&app_id, &guild_for_cmds, &cmds).await
                    {
                        eprintln!("could not register /play in guild {guild_for_cmds}: {error}");
                    }
                });
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
                    channels.remove(guild);
                    voiced.remove(guild);
                    waiting.remove(guild);
                    let _ = pairer.clear(guild);
                    let _ = session.destroy_player(guild).await;
                }
            }
            BotEvent::VoiceStateUpdate(update) => {
                let Some(guild) = update.guild_id else { continue };
                match update.channel_id {
                    Some(channel) => {
                        channels.entry(guild).or_default().insert(update.user_id, channel);
                    }
                    None => {
                        if let Some(guild_channels) = channels.get_mut(&guild) {
                            guild_channels.remove(&update.user_id);
                        }
                        if update.user_id == ready.user_id {
                            voiced.remove(&guild);
                            let _ = pairer.clear(&guild);
                            let _ = session.destroy_player(&guild).await;
                            if let Some(pending_request) = waiting.remove(&guild) {
                                reply(
                                    &rest,
                                    &ready.application_id,
                                    &pending_request,
                                    "已從語音頻道中斷連線。",
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            BotEvent::Interaction(request) if request.command == "play" => {
                rest.defer_interaction(&request.id, &request.token).await?;
                let pending = PendingPlay::Slash(request);
                let PendingPlay::Slash(ref req) = pending else { unreachable!() };
                let Some(guild) = req.guild_id.clone() else {
                    reply(&rest, &ready.application_id, &pending, "請在伺服器裡使用 /play。").await;
                    continue;
                };
                let Some(channel) = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&req.user_id))
                    .cloned()
                else {
                    reply(&rest, &ready.application_id, &pending, "請先加入一個語音頻道。").await;
                    continue;
                };
                let bot_channel = channels
                    .get(&guild)
                    .and_then(|guild_channels| guild_channels.get(&ready.user_id));
                if voiced.contains(&guild) && bot_channel == Some(&channel) {
                    play(&rustlink, &session, &rest, &ready.application_id, &guild, &pending).await;
                } else {
                    if let Some(previous) = waiting.insert(guild.clone(), pending) {
                        reply(&rest, &ready.application_id, &previous, "已被新的點播請求取代。")
                            .await;
                    }
                    gateway.set_voice_state(&guild, Some(&channel)).await?;
                }
            }
            BotEvent::MessageCreate(message) => {
                if message.author_id == ready.user_id {
                    continue;
                }
                let Some(guild) = message.guild_id else { continue };
                let Some((query, mentioned)) =
                    extract_message_query(&message.content, &ready.user_id)
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
                    .and_then(|guild_channels| guild_channels.get(&ready.user_id));
                if voiced.contains(&guild) && bot_channel == Some(&channel) {
                    play(&rustlink, &session, &rest, &ready.application_id, &guild, &pending).await;
                } else {
                    if let Some(previous) = waiting.insert(guild.clone(), pending) {
                        reply(&rest, &ready.application_id, &previous, "已被新的點播請求取代。")
                            .await;
                    }
                    gateway.set_voice_state(&guild, Some(&channel)).await?;
                }
            }
            _ => {}
        }
    }
    for guild in &voiced {
        let _ = gateway.set_voice_state(guild, None).await;
    }
    let _ = session.destroy().await;
    event_task.abort();
    Ok(())
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
    let message = match resolve(rustlink, query).await {
        Ok(track) => match session.play(guild, &track.token).await {
            Ok(_) => format!("正在播放：{}（{}）", track.metadata.title, track.metadata.author),
            Err(error) => format!("播放失敗：{error}"),
        },
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
