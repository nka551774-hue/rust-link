//! Minimal native Rust bot-to-Rustlink playback bridge.
//!
//! Required environment: DISCORD_BOT_TOKEN, DISCORD_GUILD_ID,
//! DISCORD_CHANNEL_ID, RUSTLINK_URL, RUSTLINK_PASSWORD.  The optional first
//! argument is a Rustlink source query (the resolver itself is native Rust).

use rustlink_client::{Client, LoadResult};
use rustlink_discord_bot::{
    BotEvent, DiscordRest, Gateway, VoiceStatePairer, GUILDS, GUILD_VOICE_STATES,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_BOT_TOKEN")?;
    let guild = std::env::var("DISCORD_GUILD_ID")?;
    let channel = std::env::var("DISCORD_CHANNEL_ID")?;
    let rustlink = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let rest = DiscordRest::new(token.clone())?;
    let mut gateway = Gateway::connect(token, GUILDS | GUILD_VOICE_STATES).await?;
    let ready = loop {
        if let BotEvent::Ready(ready) =
            gateway.next_event().await?.ok_or("gateway closed before READY")?
        {
            break ready;
        }
    };
    println!("Discord bot READY as {}", ready.user_id);

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut rustlink_events = session.events().await?;
    let event_task = tokio::spawn(async move {
        while let Some(event) = rustlink_events.next().await? {
            println!("rustlink event: {event}");
        }
        Ok::<_, rustlink_client::Error>(())
    });

    gateway.set_voice_state(&guild, Some(&channel)).await?;
    let mut pairer = VoiceStatePairer::for_guild(&ready.user_id, &guild)?;
    let query = std::env::args().nth(1).unwrap_or_else(|| "ytsearch:lofi".into());
    let mut started = false;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                break;
            }
            event = gateway.next_event() => {
                let Some(event) = event? else { break; };
                if let Some(voice) = pairer.observe(&event)? {
                    session.set_voice(&guild, voice).await?;
                    if !started {
                        let track = match rustlink.resolve(&query).await? {
                            LoadResult::Track(track) => *track,
                            LoadResult::Search(tracks) => tracks.into_iter().next().ok_or("no results")?,
                            LoadResult::Playlist(playlist) => playlist.tracks.into_iter().next().ok_or("empty playlist")?,
                            _ => return Err("query did not resolve to a playable track".into()),
                        };
                        session.play(&guild, &track.token).await?;
                        if let Ok(text_channel) = std::env::var("DISCORD_TEXT_CHANNEL_ID") {
                            let _ = rest.send_text(&text_channel, "Rustlink playback started.").await;
                        }
                        started = true;
                    }
                }
            }
        }
    }
    let _ = gateway.set_voice_state(&guild, None).await;
    let _ = session.destroy().await;
    event_task.abort();
    Ok(())
}
