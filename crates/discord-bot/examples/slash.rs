//! `/play` slash-command bot. Join a voice channel, type `/play url:<YouTube
//! link>`, and the bot joins that channel and plays through Rustlink. The link
//! is resolved by Rustlink's native Rust source layer; nothing runs yt-dlp.
//!
//! Required environment: DISCORD_BOT_TOKEN, RUSTLINK_PASSWORD. Optional:
//! RUSTLINK_URL (default http://127.0.0.1:2333). The command is registered in
//! every guild the bot is in, so it appears without the global-command delay.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rustlink_client::{Client, LoadResult, Session, Track};
use rustlink_discord_bot::{
    BotEvent, DiscordRest, Gateway, Interaction, VoiceStatePairer, GUILDS, GUILD_VOICE_STATES,
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_BOT_TOKEN")?;
    let rustlink = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let rest = Arc::new(DiscordRest::new(token.clone())?);
    let mut gateway = Gateway::connect(token, GUILDS | GUILD_VOICE_STATES).await?;
    let ready = loop {
        if let BotEvent::Ready(ready) =
            gateway.next_event().await?.ok_or("gateway closed before READY")?
        {
            break ready;
        }
    };
    if ready.application_id.is_empty() {
        return Err("READY carried no application ID".into());
    }
    println!("Discord bot READY as {}", ready.user_id);

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut rustlink_events = session.events().await?;
    let mut event_task = tokio::spawn(async move {
        while let Some(event) = rustlink_events.next().await? {
            println!("rustlink event: {event}");
        }
        Ok::<_, rustlink_client::Error>(())
    });

    let commands = json!([{
        "name": "play",
        "description": "播放連結或搜尋關鍵字",
        "options": [{
            "type": 3,
            "name": "url",
            "description": "YouTube／SoundCloud／Spotify 連結或關鍵字",
            "required": true
        }]
    }]);
    let mut pairer = VoiceStatePairer::new(&ready.user_id)?;
    // (guild, user) -> voice channel, for every member Discord reports.
    let mut channels: HashMap<(String, String), String> = HashMap::new();
    // Guilds whose voice state Rustlink currently holds.
    let mut voiced: HashSet<String> = HashSet::new();
    // A request waiting for the bot to finish joining: guild -> interaction.
    let mut waiting: HashMap<String, Interaction> = HashMap::new();

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        let event = tokio::select! {
            result = &mut shutdown => { result?; break; }
            ended = &mut event_task => { ended??; break; }
            event = gateway.next_event() => match event? { Some(event) => event, None => break },
        };
        if let Some(voice) = pairer.observe(&event)? {
            let guild = match &event {
                BotEvent::VoiceStateUpdate(update) => update.guild_id.clone().unwrap_or_default(),
                BotEvent::VoiceServerUpdate(update) => update.guild_id.clone(),
                _ => String::new(),
            };
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
                let cmds = commands.clone();
                let guild_for_cmds = guild.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        rest_clone.set_guild_commands(&app_id, &guild_for_cmds, &cmds).await
                    {
                        eprintln!("could not register /play in guild {guild_for_cmds}: {error}");
                    }
                });
                channels.retain(|(g, _), _| g != &guild);
                for state in data["voice_states"].as_array().into_iter().flatten() {
                    if let (Some(user), Some(channel)) =
                        (state["user_id"].as_str(), state["channel_id"].as_str())
                    {
                        channels.insert((guild.clone(), user.into()), channel.into());
                    }
                }
            }
            BotEvent::Other { name, data } if name == "GUILD_DELETE" => {
                if let Some(guild) = data["id"].as_str() {
                    channels.retain(|(g, _), _| g != guild);
                    voiced.remove(guild);
                    waiting.remove(guild);
                    let _ = pairer.clear(guild);
                    let _ = session.destroy_player(guild).await;
                }
            }
            BotEvent::VoiceStateUpdate(update) => {
                let Some(guild) = update.guild_id else { continue };
                let key = (guild.clone(), update.user_id.clone());
                match update.channel_id {
                    Some(channel) => {
                        channels.insert(key, channel);
                    }
                    None => {
                        channels.remove(&key);
                        if update.user_id == ready.user_id {
                            voiced.remove(&guild);
                            waiting.remove(&guild);
                        }
                    }
                }
            }
            BotEvent::Interaction(request) if request.command == "play" => {
                rest.defer_interaction(&request.id, &request.token).await?;
                let Some(guild) = request.guild_id.clone() else {
                    reply(&rest, &ready.application_id, &request, "請在伺服器裡使用 /play。").await;
                    continue;
                };
                let Some(channel) =
                    channels.get(&(guild.clone(), request.user_id.clone())).cloned()
                else {
                    reply(&rest, &ready.application_id, &request, "請先加入一個語音頻道。").await;
                    continue;
                };
                let bot_channel = channels.get(&(guild.clone(), ready.user_id.clone()));
                if voiced.contains(&guild) && bot_channel == Some(&channel) {
                    play(&rustlink, &session, &rest, &ready.application_id, &guild, &request).await;
                } else {
                    // Joining (or moving) produces a fresh voice pair; play once it lands.
                    waiting.insert(guild.clone(), request);
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
/// and edits the deferred reply with the outcome.
async fn play(
    rustlink: &Client,
    session: &Session,
    rest: &DiscordRest,
    application_id: &str,
    guild: &str,
    request: &Interaction,
) {
    let query = request.option("url").unwrap_or_default();
    let message = match resolve(rustlink, query).await {
        Ok(track) => match session.play(guild, &track.token).await {
            Ok(_) => format!("▶️ 正在播放：{}（{}）", track.metadata.title, track.metadata.author),
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

fn normalize_query(text: &str) -> String {
    let text = text.trim();
    const PREFIXES: [&str; 10] = [
        "ytsearch:",
        "ytmsearch:",
        "scsearch:",
        "spsearch:",
        "dzsearch:",
        "dzisrc:",
        "spotify:",
        "deezer:",
        "soundcloud:",
        "youtube:playlist:",
    ];
    let is_video_id = text.len() == 11
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if text.is_empty()
        || text.contains("://")
        || text.starts_with("<http")
        || is_video_id
        || PREFIXES.iter().any(|prefix| text.starts_with(prefix))
    {
        text.to_string()
    } else {
        format!("ytsearch:{text}")
    }
}

async fn reply(rest: &DiscordRest, application_id: &str, request: &Interaction, text: &str) {
    // Discord caps a message at 2000 characters; a long error must not fail the edit.
    let text: String = text.chars().take(1900).collect();
    if let Err(error) = rest.edit_interaction_response(application_id, &request.token, &text).await
    {
        eprintln!("could not answer /play: {error}");
    }
}
