//! Environment overrides.
//!
//! The environment always wins over the file, so a container can be reconfigured
//! without rebuilding the image. RUSTLINK_* settings also expose capacity limits.

use crate::Config;

/// A field the environment fills with text, which is how most settings arrive.
type StringField = fn(&mut Config) -> &mut String;

/// A field the environment fills from a truthy value.
type FlagField = fn(&mut Config) -> &mut bool;

/// A numeric field, which is only written when the value parses.
type NumberField = fn(&mut Config) -> &mut u32;

/// Applies the process environment on top of a configuration.
pub fn apply_env(config: &mut Config) {
    apply_env_with(config, |name| std::env::var(name).ok());
}

/// Applies overrides read from `lookup`, which is separated from the process
/// environment so the rules can be tested without touching global state.
pub fn apply_env_with(config: &mut Config, lookup: impl Fn(&str) -> Option<String>) {
    let strings: [(&str, StringField); 23] = [
        ("RUSTLINK_ADDRESS", |c| &mut c.address),
        ("RUSTLINK_PASSWORD", |c| &mut c.password),
        ("RUSTLINK_FFMPEG", |c| &mut c.ffmpeg),
        ("RUSTLINK_TOOLS_DIRECTORY", |c| &mut c.tools_directory),
        ("RUSTLINK_LOG_LEVEL", |c| &mut c.log_level),
        ("RUSTLINK_LOG_FORMAT", |c| &mut c.log_format),
        ("RUSTLINK_LOG_COLOR", |c| &mut c.log_color),
        ("RUSTLINK_LOG_DIRECTORY", |c| &mut c.log_directory),
        ("RUSTLINK_YOUTUBE_ENDPOINT", |c| &mut c.youtube_endpoint),
        ("RUSTLINK_YOUTUBE_MUSIC_ENDPOINT", |c| &mut c.youtube_music_endpoint),
        ("RUSTLINK_YOUTUBE_CLIENT_VERSION", |c| &mut c.youtube_client_version),
        ("RUSTLINK_YOUTUBE_PO_TOKEN", |c| &mut c.youtube_po_token),
        ("RUSTLINK_YOUTUBE_VISITOR_DATA", |c| &mut c.youtube_visitor_data),
        ("RUSTLINK_SOUNDCLOUD_API_ENDPOINT", |c| &mut c.soundcloud_api_endpoint),
        ("RUSTLINK_SOUNDCLOUD_WEB_ENDPOINT", |c| &mut c.soundcloud_web_endpoint),
        ("RUSTLINK_SOUNDCLOUD_CLIENT_ID", |c| &mut c.soundcloud_client_id),
        ("RUSTLINK_DEEZER_API_ENDPOINT", |c| &mut c.deezer_api_endpoint),
        ("RUSTLINK_LYRICS_SOURCE", |c| &mut c.lyrics_source),
        ("RUSTLINK_WEBHOOK_URL", |c| &mut c.webhook.url),
        // Spotify keeps its own variable names, because they are the ones the
        // Spotify dashboard hands out.
        ("SPOTIFY_CLIENT_ID", |c| &mut c.spotify_client_id),
        ("SPOTIFY_CLIENT_SECRET", |c| &mut c.spotify_client_secret),
        ("SPOTIFY_REFRESH_TOKEN", |c| &mut c.spotify_refresh_token),
        ("SPOTIFY_MARKET", |c| &mut c.spotify_market),
    ];
    for (name, field) in strings {
        if let Some(value) = lookup(name) {
            *field(config) = value;
        }
    }

    let flags: [(&str, FlagField); 7] = [
        ("RUSTLINK_YOUTUBE_NATIVE", |c| &mut c.youtube_native),
        ("RUSTLINK_SOUNDCLOUD", |c| &mut c.soundcloud_native),
        ("RUSTLINK_AUTO_DOWNLOAD", |c| &mut c.auto_download),
        ("RUSTLINK_IN_PROCESS_AUDIO", |c| &mut c.in_process_audio),
        ("RUSTLINK_PRIVATE_NETWORKS", |c| &mut c.allow_private_networks),
        ("RUSTLINK_RATELIMITS_ENABLE", |c| &mut c.ratelimits.enable),
        ("RUSTLINK_WEBHOOK_ENABLED", |c| &mut c.webhook.enable),
    ];
    for (name, field) in flags {
        if let Some(value) = lookup(name) {
            *field(config) = truthy(&value);
        }
    }

    let numbers: [(&str, NumberField); 15] = [
        ("RUSTLINK_MAX_PLAYERS", |c| &mut c.max_players),
        ("RUSTLINK_MAX_SESSIONS", |c| &mut c.max_sessions),
        ("RUSTLINK_MAX_CONCURRENT_LOADS", |c| &mut c.max_concurrent_loads),
        ("RUSTLINK_MAX_PLAYLIST_TRACKS", |c| &mut c.max_playlist_tracks),
        ("RUSTLINK_SEARCH_RESULTS", |c| &mut c.search_results),
        ("RUSTLINK_CACHE_ENTRIES", |c| &mut c.cache_entries),
        ("RUSTLINK_CACHE_TTL_SECONDS", |c| &mut c.cache_ttl_seconds),
        ("RUSTLINK_LOAD_TIMEOUT_SECONDS", |c| &mut c.load_timeout_seconds),
        ("RUSTLINK_PLAYER_UPDATE_SECONDS", |c| &mut c.player_update_seconds),
        ("RUSTLINK_STATS_SECONDS", |c| &mut c.stats_seconds),
        ("RUSTLINK_EVENT_BUFFER", |c| &mut c.event_buffer),
        ("RUSTLINK_OPUS_BITRATE", |c| &mut c.opus_bitrate),
        ("RUSTLINK_OPUS_COMPLEXITY", |c| &mut c.opus_complexity),
        ("RUSTLINK_LOG_RETENTION_DAYS", |c| &mut c.log_retention_days),
        ("RUSTLINK_WEBHOOK_STATE_LATENCY_SECONDS", |c| &mut c.webhook.state_latency_seconds),
    ];
    for (name, field) in numbers {
        if let Some(value) = lookup(name) {
            // A number that cannot be read is left alone rather than turning the
            // whole variable into a startup failure; validation still catches a
            // value that is out of range.
            if let Ok(parsed) = value.trim().parse::<u32>() {
                *field(config) = parsed;
            }
        }
    }
}

/// Reads a switch. Anything other than `0` or `false` counts as on.
fn truthy(value: &str) -> bool {
    !(value == "0" || value.eq_ignore_ascii_case("false"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)], name: &str) -> Option<String> {
        pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn environment_overrides_the_file() {
        let mut config = Config { address: "127.0.0.1:2333".into(), ..Default::default() };
        let pairs = [("RUSTLINK_ADDRESS", "0.0.0.0:2333"), ("RUSTLINK_PASSWORD", "hunter2")];
        apply_env_with(&mut config, |name| lookup(&pairs, name));
        assert_eq!(config.address, "0.0.0.0:2333");
        assert_eq!(config.password, "hunter2");
    }

    #[test]
    fn switches_treat_zero_and_false_as_off() {
        for value in ["0", "false", "FALSE"] {
            let mut config = Config::default();
            let pairs = [("RUSTLINK_YOUTUBE_NATIVE", value)];
            apply_env_with(&mut config, |name| lookup(&pairs, name));
            assert!(!config.youtube_native, "{value} should switch the flag off");
        }
        let mut config = Config::default();
        let pairs = [("RUSTLINK_YOUTUBE_NATIVE", "1")];
        apply_env_with(&mut config, |name| lookup(&pairs, name));
        assert!(config.youtube_native);
    }

    #[test]
    fn numbers_are_read_and_garbage_is_ignored() {
        let mut config = Config::default();
        let pairs = [
            ("RUSTLINK_OPUS_BITRATE", "96000"),
            ("RUSTLINK_OPUS_COMPLEXITY", "4"),
            ("RUSTLINK_MAX_PLAYERS", "nonsense"),
            ("RUSTLINK_LOG_RETENTION_DAYS", "0"),
        ];
        apply_env_with(&mut config, |name| lookup(&pairs, name));
        assert_eq!(config.opus_bitrate, 96_000);
        assert_eq!(config.opus_complexity, 4);
        assert_eq!(config.max_players, 100);
        assert_eq!(config.log_retention_days, 0);
    }

    #[test]
    fn spotify_variables_use_their_own_names() {
        let mut config = Config::default();
        let pairs = [("SPOTIFY_CLIENT_ID", "id"), ("SPOTIFY_MARKET", "TW")];
        apply_env_with(&mut config, |name| lookup(&pairs, name));
        assert_eq!(config.spotify_client_id, "id");
        assert_eq!(config.spotify_market, "TW");
        assert!(config.spotify_enabled());
    }
}
