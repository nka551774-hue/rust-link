//! Configuration validation.
//!
//! Everything a node needs to be self-consistent is checked here, before it
//! listens, so a mistake is a startup error naming the option rather than a
//! failure later that only shows up as a request that does not work.

use crate::{Config, ConfigError, LOG_COLORS, LOG_FORMATS, LOG_LEVELS, LYRICS_SOURCES};

const MAX_LIMIT: u32 = 100_000;

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    let invalid = |message: String| Err(ConfigError::Invalid(message));

    if config.password.is_empty() {
        return invalid("set RUSTLINK_PASSWORD or password in config".into());
    }
    if config.address.is_empty() {
        return invalid("address is required".into());
    }
    if config.auto_download && config.tools_directory.is_empty() {
        return invalid("toolsDirectory is required while autoDownload is on".into());
    }
    if !one_of(&config.lyrics_source, &LYRICS_SOURCES) {
        return invalid(format!("lyricsSource must be one of {}", LYRICS_SOURCES.join(", ")));
    }
    if !one_of(&config.log_level, &LOG_LEVELS) {
        return invalid(format!("logLevel must be one of {}", LOG_LEVELS.join(", ")));
    }
    if !one_of(&config.log_format, &LOG_FORMATS) {
        return invalid(format!("logFormat must be one of {}", LOG_FORMATS.join(", ")));
    }
    if !one_of(&config.log_color, &LOG_COLORS) {
        return invalid(format!("logColor must be one of {}", LOG_COLORS.join(", ")));
    }
    if config.webhook.enable && config.webhook.url.trim().is_empty() {
        return invalid("webhook.url is required while webhook is enabled".into());
    }
    if config.webhook.state_latency_seconds > MAX_LIMIT {
        return invalid("webhook.stateLatencySeconds must be between 0 and 100000".into());
    }
    // Zero is meaningful here: it keeps every daily file rather than a window of
    // them, which is what a node whose logs are rotated elsewhere asks for.
    if config.log_retention_days > MAX_LIMIT {
        return invalid("logRetentionDays must be between 0 and 100000".into());
    }
    if let Some(problem) = check_webhook_url(config.webhook.url.trim()) {
        return invalid(problem);
    }
    if config.youtube_native
        && (config.youtube_endpoint.is_empty() || config.youtube_music_endpoint.is_empty())
    {
        return invalid(
            "youtubeEndpoint and youtubeMusicEndpoint are required while youtubeNative is on"
                .into(),
        );
    }
    if config.soundcloud_native
        && (config.soundcloud_api_endpoint.is_empty() || config.soundcloud_web_endpoint.is_empty())
    {
        return invalid(
            "soundcloudApiEndpoint and soundcloudWebEndpoint are required while soundcloudNative is on"
                .into(),
        );
    }

    let limits: [(&str, u32); 12] = [
        ("maxPlayers", config.max_players),
        ("maxSessions", config.max_sessions),
        ("maxConcurrentLoads", config.max_concurrent_loads),
        ("maxPlaylistTracks", config.max_playlist_tracks),
        ("searchResults", config.search_results),
        ("cacheEntries", config.cache_entries),
        ("cacheTtlSeconds", config.cache_ttl_seconds),
        ("loadTimeoutSeconds", config.load_timeout_seconds),
        ("playerUpdateSeconds", config.player_update_seconds),
        ("statsSeconds", config.stats_seconds),
        ("eventBuffer", config.event_buffer),
        ("soundcloudClientIdTtlSeconds", config.soundcloud_client_id_ttl_seconds),
    ];
    for (name, value) in limits {
        if !(1..=MAX_LIMIT).contains(&value) {
            return invalid(format!("{name} must be between 1 and 100000"));
        }
    }
    if !(16_000..=256_000).contains(&config.opus_bitrate) {
        return invalid("opusBitrate must be 16000..256000".into());
    }
    if config.opus_complexity > 10 {
        return invalid("opusComplexity must be 0..10".into());
    }
    if config.spotify_client_id.is_empty() != config.spotify_client_secret.is_empty() {
        return invalid("Spotify client ID and secret must both be set".into());
    }
    if !config.spotify_refresh_token.is_empty() && config.spotify_client_id.is_empty() {
        return invalid(
            "Spotify refresh token requires the application client ID and secret".into(),
        );
    }
    if !config.spotify_client_id.is_empty() && config.spotify_market.is_empty() {
        return invalid("spotifyMarket is required when Spotify credentials are set".into());
    }
    Ok(())
}

/// Checks the shape of a webhook URL, which is the one place the node is told to
/// talk to an address rather than to a fixed service.
fn check_webhook_url(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let rest = match url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        Some(rest) => rest,
        None => return Some("webhook.url must be an http or https URL".into()),
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Some("webhook.url must be an http or https URL".into());
    }
    None
}

fn one_of(value: &str, allowed: &[&str]) -> bool {
    let value = value.trim();
    allowed.iter().any(|candidate| candidate.eq_ignore_ascii_case(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        Config { password: "secret".into(), ..Default::default() }
    }

    #[test]
    fn a_password_is_required() {
        let mut config = valid();
        config.password = String::new();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn defaults_are_valid() {
        validate(&valid()).unwrap();
    }

    #[test]
    fn limits_are_bounded() {
        let mut config = valid();
        config.opus_bitrate = 8_000;
        assert!(validate(&config).is_err());
        config.opus_bitrate = 128_000;
        config.opus_complexity = 11;
        assert!(validate(&config).is_err());
        config.opus_complexity = 4;
        validate(&config).unwrap();
        config.max_players = 0;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn a_webhook_needs_a_url_when_it_is_enabled() {
        let mut config = valid();
        config.webhook.enable = true;
        assert!(validate(&config).is_err());
        config.webhook.url = "not-a-url".into();
        assert!(validate(&config).is_err());
        config.webhook.url = "https://discord.com/api/webhooks/1/abc".into();
        validate(&config).unwrap();
    }

    #[test]
    fn spotify_credentials_come_in_pairs() {
        let mut config = valid();
        config.spotify_client_id = "id".into();
        assert!(validate(&config).is_err());
        config.spotify_client_secret = "secret".into();
        validate(&config).unwrap();
        config.spotify_refresh_token = "token".into();
        validate(&config).unwrap();
        config.spotify_client_id = String::new();
        config.spotify_client_secret = String::new();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn enum_like_options_are_checked_case_insensitively() {
        let mut config = valid();
        config.log_level = "DEBUG".into();
        validate(&config).unwrap();
        config.log_level = "verbose".into();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn a_native_client_needs_its_endpoints() {
        let mut config = valid();
        config.youtube_music_endpoint = String::new();
        assert!(validate(&config).is_err());
        config.youtube_native = false;
        validate(&config).unwrap();
    }

    #[test]
    fn a_log_retention_of_zero_is_allowed_and_an_absurd_one_is_not() {
        let mut config = valid();
        config.log_retention_days = 0;
        validate(&config).unwrap();
        config.log_retention_days = MAX_LIMIT + 1;
        assert!(validate(&config).is_err());
    }
}
