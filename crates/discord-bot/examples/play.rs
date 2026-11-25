//! Minimal native Rust bot-to-Rustlink playback bridge.
//!
//! Required environment: DISCORD_BOT_TOKEN, DISCORD_GUILD_ID, DISCORD_CHANNEL_ID,
//! RUSTLINK_PASSWORD (and optional RUSTLINK_URL, DISCORD_TEXT_CHANNEL_ID).

use std::time::Duration;

use rustlink_client::{Client, LoadResult};
use rustlink_discord_bot::{
    normalize_query, BotEvent, DiscordRest, Gateway, VoiceStatePairer, GUILDS, GUILD_VOICE_STATES,
};

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
    let guild = std::env::var("DISCORD_GUILD_ID")?;
    let channel = std::env::var("DISCORD_CHANNEL_ID")?;
    let rustlink = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let rest = DiscordRest::new(token.clone())?;
    let mut gateway = Gateway::connect(token, GUILDS | GUILD_VOICE_STATES).await?;
    let ready = gateway.next_ready().await?;
    println!("Discord bot READY as {}", ready.user_id);

    let session = rustlink.create_session(&ready.user_id, 60).await?;
    let mut events = session.events().await?;
    let mut event_task = tokio::spawn(async move {
        while let Some(event) = events.next().await? {
            println!("rustlink event: {event}");
        }
        Ok::<_, rustlink_client::Error>(())
    });

    gateway.set_voice_state(&guild, Some(&channel)).await?;
    let mut pairer = VoiceStatePairer::for_guild(&ready.user_id, &guild)?;
    let raw_query = std::env::args().nth(1).unwrap_or_else(|| "ytsearch:lofi".into());
    let query = normalize_query(&raw_query);
    let mut started = false;
    let join_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let outcome: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            res = &mut shutdown => break res.map_err(Into::into),
            ended = &mut event_task => break ended?.map_err(Into::into),
            _ = tokio::time::sleep_until(join_deadline), if !started => {
                break Err(format!("timed out waiting to join {channel} in guild {guild}").into());
            }
            event = gateway.next_event() => {
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => { gateway.reconnect(None).await?; continue; }
                    Err(err) => { gateway.reconnect(Some(&err)).await?; continue; }
                };
                if let Some(voice) = pairer.observe(&event)? {
                    session.set_voice(&guild, voice).await?;
                    if !started {
                        let track = match rustlink.resolve(&query).await? {
                            LoadResult::Track(t) => *t,
                            LoadResult::Search(t) => t.into_iter().next().ok_or("no results")?,
                            LoadResult::Playlist(p) => p.tracks.into_iter().next().ok_or("empty playlist")?,
                            _ => break Err("query did not resolve to a playable track".into()),
                        };
                        session.play(&guild, &track.token).await?;
                        if let Ok(text_ch) = std::env::var("DISCORD_TEXT_CHANNEL_ID") {
                            let _ = rest.send_text(&text_ch, "Rustlink playback started.").await;
                        }
                        started = true;
                    }
                }
                if matches!(&event, BotEvent::Ready(r) if r.user_id == ready.user_id) {
                    gateway.set_voice_state(&guild, Some(&channel)).await?;
                }
            }
        }
    };
    let _ = gateway.set_voice_state(&guild, None).await;
    let _ = session.destroy().await;
    event_task.abort();
    outcome
}
