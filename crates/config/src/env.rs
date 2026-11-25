//! Environment overrides (`RUSTLINK_*` and `SPOTIFY_*`).

use crate::Config;

pub fn apply_env(config: &mut Config) {
    apply_env_with(config, |name| std::env::var(name).ok());
}

pub fn apply_env_with(config: &mut Config, lookup: impl Fn(&str) -> Option<String>) {
    macro_rules! env_map {
        (str: $($var:literal => $field:expr),* $(,)?) => {
            $(if let Some(v) = lookup($var) { $field = v; })*
        };
        (non_empty: $($var:literal => $field:expr),* $(,)?) => {
            $(if let Some(v) = lookup($var).filter(|v| !v.trim().is_empty()) { $field = v; })*
        };
        (flag: $($var:literal => $field:expr),* $(,)?) => {
            $(if let Some(v) = lookup($var).as_deref().and_then(switch) { $field = v; })*
        };
        (num: $($var:literal => $field:expr),* $(,)?) => {
            $(if let Some(v) = lookup($var).and_then(|v| v.trim().parse().ok()) { $field = v; })*
        };
    }

    env_map!(str:
        "RUSTLINK_ADDRESS" => config.address,
        "RUSTLINK_PASSWORD" => config.password,
        "RUSTLINK_FFMPEG" => config.ffmpeg,
        "RUSTLINK_TOOLS_DIRECTORY" => config.tools_directory,
        "RUSTLINK_LOG_LEVEL" => config.log_level,
        "RUSTLINK_LOG_FORMAT" => config.log_format,
        "RUSTLINK_LOG_COLOR" => config.log_color,
        "RUSTLINK_LOG_DIRECTORY" => config.log_directory,
        "RUSTLINK_YOUTUBE_ENDPOINT" => config.youtube_endpoint,
        "RUSTLINK_YOUTUBE_MUSIC_ENDPOINT" => config.youtube_music_endpoint,
        "RUSTLINK_YOUTUBE_CLIENT_VERSION" => config.youtube_client_version,
        "RUSTLINK_YOUTUBE_PO_TOKEN_FILE" => config.youtube_po_token_file,
        "RUSTLINK_YOUTUBE_PO_TOKEN" => config.youtube_po_token,
        "RUSTLINK_YOUTUBE_VISITOR_DATA" => config.youtube_visitor_data,
        "RUSTLINK_YOUTUBE_PROXY" => config.youtube_proxy,
        "RUSTLINK_SOUNDCLOUD_API_ENDPOINT" => config.soundcloud_api_endpoint,
        "RUSTLINK_SOUNDCLOUD_WEB_ENDPOINT" => config.soundcloud_web_endpoint,
        "RUSTLINK_SOUNDCLOUD_CLIENT_ID" => config.soundcloud_client_id,
        "RUSTLINK_DEEZER_API_ENDPOINT" => config.deezer_api_endpoint,
        "RUSTLINK_LYRICS_SOURCE" => config.lyrics_source,
        "RUSTLINK_WEBHOOK_URL" => config.webhook.url,
    );

    env_map!(non_empty:
        "SPOTIFY_CLIENT_ID" => config.spotify_client_id,
        "RUSTLINK_SPOTIFY_CLIENT_ID" => config.spotify_client_id,
        "SPOTIFY_CLIENT_SECRET" => config.spotify_client_secret,
        "RUSTLINK_SPOTIFY_CLIENT_SECRET" => config.spotify_client_secret,
        "SPOTIFY_REFRESH_TOKEN" => config.spotify_refresh_token,
        "RUSTLINK_SPOTIFY_REFRESH_TOKEN" => config.spotify_refresh_token,
        "SPOTIFY_MARKET" => config.spotify_market,
        "RUSTLINK_SPOTIFY_MARKET" => config.spotify_market,
    );

    env_map!(flag:
        "RUSTLINK_YOUTUBE_NATIVE" => config.youtube_native,
        "RUSTLINK_YOUTUBE_REQUIRE_PO_TOKEN" => config.youtube_require_po_token,
        "RUSTLINK_SOUNDCLOUD" => config.soundcloud_native,
        "RUSTLINK_SOUNDCLOUD_NATIVE" => config.soundcloud_native,
        "RUSTLINK_AUTO_DOWNLOAD" => config.auto_download,
        "RUSTLINK_IN_PROCESS_AUDIO" => config.in_process_audio,
        "RUSTLINK_PRIVATE_NETWORKS" => config.allow_private_networks,
        "RUSTLINK_ALLOW_PRIVATE_NETWORKS" => config.allow_private_networks,
        "RUSTLINK_RATELIMITS_ENABLE" => config.ratelimits.enable,
        "RUSTLINK_WEBHOOK_ENABLED" => config.webhook.enable,
        "RUSTLINK_WEBHOOK_ENABLE" => config.webhook.enable,
    );

    env_map!(num:
        "RUSTLINK_MAX_PLAYERS" => config.max_players,
        "RUSTLINK_MAX_SESSIONS" => config.max_sessions,
        "RUSTLINK_MAX_CONCURRENT_LOADS" => config.max_concurrent_loads,
        "RUSTLINK_MAX_PLAYLIST_TRACKS" => config.max_playlist_tracks,
        "RUSTLINK_SEARCH_RESULTS" => config.search_results,
        "RUSTLINK_CACHE_ENTRIES" => config.cache_entries,
        "RUSTLINK_CACHE_TTL_SECONDS" => config.cache_ttl_seconds,
        "RUSTLINK_LOAD_TIMEOUT_SECONDS" => config.load_timeout_seconds,
        "RUSTLINK_PLAYER_UPDATE_SECONDS" => config.player_update_seconds,
        "RUSTLINK_STATS_SECONDS" => config.stats_seconds,
        "RUSTLINK_EVENT_BUFFER" => config.event_buffer,
        "RUSTLINK_OPUS_BITRATE" => config.opus_bitrate,
        "RUSTLINK_OPUS_COMPLEXITY" => config.opus_complexity,
        "RUSTLINK_LOG_RETENTION_DAYS" => config.log_retention_days,
        "RUSTLINK_SOUNDCLOUD_CLIENT_ID_TTL_SECONDS" => config.soundcloud_client_id_ttl_seconds,
        "RUSTLINK_WEBHOOK_STATE_LATENCY_SECONDS" => config.webhook.state_latency_seconds,
    );
}

fn switch(value: &str) -> Option<bool> {
    let v = value.trim();
    if ["1", "true", "yes", "on"].iter().any(|s| v.eq_ignore_ascii_case(s)) {
        Some(true)
    } else if ["0", "false", "no", "off"].iter().any(|s| v.eq_ignore_ascii_case(s)) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(config: &mut Config, pairs: &[(&str, &str)]) {
        apply_env_with(config, |name| {
            pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_string())
        });
    }

    fn from_env(pairs: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        apply(&mut config, pairs);
        config
    }

    #[test]
    fn environment_overrides_the_file() {
        let mut config = Config::default();
        config.address = "127.0.0.1:2333".into();
        apply(
            &mut config,
            &[("RUSTLINK_ADDRESS", "0.0.0.0:2333"), ("RUSTLINK_PASSWORD", "hunter2")],
        );
        assert_eq!(
            (config.address.as_str(), config.password.as_str()),
            ("0.0.0.0:2333", "hunter2")
        );
    }

    #[test]
    fn switches_treat_zero_and_false_as_off() {
        for value in ["0", "false", "FALSE", "no", "off", " false "] {
            assert!(!from_env(&[("RUSTLINK_YOUTUBE_NATIVE", value)]).youtube_native, "{value}");
        }
        assert!(from_env(&[("RUSTLINK_YOUTUBE_NATIVE", "1")]).youtube_native);
        assert!(!from_env(&[("RUSTLINK_PRIVATE_NETWORKS", "")]).allow_private_networks);
    }

    #[test]
    fn numbers_are_read_and_garbage_is_ignored() {
        let config = from_env(&[
            ("RUSTLINK_OPUS_BITRATE", "96000"),
            ("RUSTLINK_OPUS_COMPLEXITY", "4"),
            ("RUSTLINK_MAX_PLAYERS", "nonsense"),
            ("RUSTLINK_LOG_RETENTION_DAYS", "0"),
        ]);
        assert_eq!(
            (
                config.opus_bitrate,
                config.opus_complexity,
                config.max_players,
                config.log_retention_days
            ),
            (96_000, 4, 100, 0)
        );
    }

    #[test]
    fn spotify_variables_use_their_own_names() {
        let mut config = from_env(&[("SPOTIFY_CLIENT_ID", "id"), ("SPOTIFY_MARKET", "TW")]);
        assert_eq!(
            (config.spotify_client_id.as_str(), config.spotify_market.as_str()),
            ("id", "TW")
        );
        assert!(!config.spotify_enabled());

        apply(
            &mut config,
            &[
                ("SPOTIFY_CLIENT_ID", "legacy-id"),
                ("RUSTLINK_SPOTIFY_CLIENT_ID", "id"),
                ("SPOTIFY_CLIENT_SECRET", "secret"),
                ("SPOTIFY_MARKET", "TW"),
            ],
        );
        assert!(config.spotify_enabled());
        assert_eq!(
            (
                config.spotify_client_id.as_str(),
                config.spotify_client_secret.as_str(),
                config.spotify_market.as_str()
            ),
            ("id", "secret", "TW")
        );

        apply(
            &mut config,
            &[
                ("SPOTIFY_CLIENT_ID", ""),
                ("SPOTIFY_CLIENT_SECRET", "   "),
                ("SPOTIFY_MARKET", "   "),
            ],
        );
        assert_eq!(
            (
                config.spotify_client_id.as_str(),
                config.spotify_client_secret.as_str(),
                config.spotify_market.as_str()
            ),
            ("id", "secret", "TW")
        );
    }

    #[test]
    fn canonical_flag_aliases_and_soundcloud_ttl_are_supported() {
        let config = from_env(&[
            ("RUSTLINK_SOUNDCLOUD_NATIVE", "false"),
            ("RUSTLINK_ALLOW_PRIVATE_NETWORKS", "true"),
            ("RUSTLINK_WEBHOOK_ENABLE", "true"),
            ("RUSTLINK_SOUNDCLOUD_CLIENT_ID_TTL_SECONDS", "3600"),
        ]);
        assert!(
            !config.soundcloud_native && config.allow_private_networks && config.webhook.enable
        );
        assert_eq!(config.soundcloud_client_id_ttl_seconds, 3_600);
    }
}
