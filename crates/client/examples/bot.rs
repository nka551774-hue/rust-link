//! Run with RUSTLINK_URL, RUSTLINK_PASSWORD, DISCORD_BOT_ID, DISCORD_GUILD_ID.
//! The three DISCORD_VOICE_* values come from your bot's Discord gateway events,
//! not from its bot token. This example does not log in to Discord itself.
use rustlink_client::{Client, LoadResult, VoiceState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(
        &std::env::var("RUSTLINK_URL").unwrap_or_else(|_| "http://127.0.0.1:2333".into()),
        std::env::var("RUSTLINK_PASSWORD")?,
    )?;
    let bot = std::env::var("DISCORD_BOT_ID")?;
    let guild = std::env::var("DISCORD_GUILD_ID")?;
    let voice = VoiceState {
        token: std::env::var("DISCORD_VOICE_TOKEN")?,
        endpoint: std::env::var("DISCORD_VOICE_ENDPOINT")?,
        session_id: std::env::var("DISCORD_VOICE_SESSION_ID")?,
        channel_id: Some(std::env::var("DISCORD_CHANNEL_ID")?),
    };
    let query = std::env::args().nth(1).unwrap_or_else(|| "ytsearch:lofi".into());
    let session = client.create_session(&bot, 60).await?;
    let result = async {
        let mut events = session.events().await?;
        println!("{}", events.next().await?.ok_or("missing session.ready")?);
        // Drain events throughout source resolution and playback.
        let event_task = tokio::spawn(async move {
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
                LoadResult::Playlist(playlist) => {
                    playlist.tracks.into_iter().next().ok_or("empty playlist")?
                }
                _ => return Err("no playable track".into()),
            };
            session.play(&guild, &track.token).await?;
            tokio::signal::ctrl_c().await?;
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
