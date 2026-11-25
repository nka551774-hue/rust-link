//! Run with RUSTLINK_URL, RUSTLINK_PASSWORD, DISCORD_BOT_ID, DISCORD_GUILD_ID,
//! DISCORD_CHANNEL_ID, and the three DISCORD_VOICE_* values from Discord gateway events.
use std::env;

use rustlink_client::{Client, LoadResult, VoiceState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = env::var("RUSTLINK_URL");
    let client = Client::new(
        url.as_deref().unwrap_or("http://127.0.0.1:2333"),
        env::var("RUSTLINK_PASSWORD")?,
    )?;
    let bot = env::var("DISCORD_BOT_ID")?;
    let guild = env::var("DISCORD_GUILD_ID")?;
    let voice = VoiceState {
        token: env::var("DISCORD_VOICE_TOKEN")?,
        endpoint: env::var("DISCORD_VOICE_ENDPOINT")?,
        session_id: env::var("DISCORD_VOICE_SESSION_ID")?,
        channel_id: Some(env::var("DISCORD_CHANNEL_ID")?),
    };
    let query = env::args().nth(1).unwrap_or_else(|| "ytsearch:lofi".into());
    let session = client.create_session(&bot, 60).await?;
    let result = async {
        let mut events = session.events().await?;
        println!("{}", events.next().await?.ok_or("missing session.ready")?);
        let mut event_task = tokio::spawn(async move {
            while let Some(event) = events.next().await? {
                println!("{event}");
            }
            Ok::<_, rustlink_client::Error>(())
        });
        let playback = async {
            session.set_voice(&guild, voice).await?;
            let track = match client.resolve(&query).await? {
                LoadResult::Track(track) => *track,
                LoadResult::Search(tracks) => tracks.into_iter().next().ok_or("no results")?,
                LoadResult::Playlist(p) => p.tracks.into_iter().next().ok_or("empty playlist")?,
                LoadResult::Empty => return Err("no playable track".into()),
                LoadResult::Error(err) => {
                    return Err(format!("resolve failed: {}", err.message).into())
                }
            };
            session.play(&guild, &track.token).await?;
            tokio::select! {
                res = tokio::signal::ctrl_c() => res?,
                ended = &mut event_task => {
                    ended??;
                    return Err("Rustlink event stream closed".into());
                }
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        event_task.abort();
        playback
    }
    .await;
    let cleanup = session.destroy().await;
    result?;
    cleanup?;
    Ok(())
}
