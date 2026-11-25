//! Configuration validation.
//!
//! Everything a node needs to be self-consistent is checked here, before it
//! listens, so a mistake is a startup error naming the option rather than a
//! failure later that only shows up as a request that does not work.

use std::net::Ipv6Addr;

use crate::{
    Config, ConfigError, FILTER_NAMES, LOG_COLORS, LOG_FORMATS, LOG_LEVELS, LYRICS_SOURCES,
};

const MAX_LIMIT: u32 = 100_000;

fn invalid(message: impl Into<String>) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid(message.into()))
}

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.password.is_empty() {
        return invalid("set RUSTLINK_PASSWORD or password in config");
    }
    if config.password.trim() != config.password {
        return invalid("password must not have leading or trailing whitespace");
    }
    if !config.password.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return invalid(
            "password must contain only printable ASCII characters (HTTP Authorization header limit)",
        );
    }
    if let Some(problem) = check_address(&config.address) {
        return invalid(problem);
    }
    if config.auto_download && config.tools_directory.trim().is_empty() {
        return invalid("toolsDirectory is required while autoDownload is on");
    }
    let ffmpeg = config.ffmpeg.trim();
    if !config.in_process_audio && (ffmpeg.is_empty() || ffmpeg.eq_ignore_ascii_case("none")) {
        return invalid("ffmpeg is required when inProcessAudio is false");
    }
    for name in config.filters.keys() {
        if !FILTER_NAMES.contains(&name.as_str()) {
            return invalid(format!(
                "unknown filter {name:?} in filters; expected one of {}",
                FILTER_NAMES.join(", ")
            ));
        }
    }
    for (name, value, allowed) in [
        ("lyricsSource", config.lyrics_source.as_str(), LYRICS_SOURCES.as_slice()),
        ("logLevel", config.log_level.as_str(), LOG_LEVELS.as_slice()),
        ("logFormat", config.log_format.as_str(), LOG_FORMATS.as_slice()),
        ("logColor", config.log_color.as_str(), LOG_COLORS.as_slice()),
    ] {
        if !one_of(value, allowed) {
            return invalid(format!("{name} must be one of {}", allowed.join(", ")));
        }
    }
    let webhook_url = config.webhook.url.trim();
    if config.webhook.enable && webhook_url.is_empty() {
        return invalid("webhook.url is required while webhook is enabled");
    }
    if config.webhook.state_latency_seconds > MAX_LIMIT {
        return invalid("webhook.stateLatencySeconds must be between 0 and 100000");
    }
    // Zero is meaningful here: it keeps every daily file rather than a window of
    // them, which is what a node whose logs are rotated elsewhere asks for.
    if config.log_retention_days > MAX_LIMIT {
        return invalid("logRetentionDays must be between 0 and 100000");
    }
    if let Some(problem) = check_webhook_url(webhook_url) {
        return invalid(problem);
    }
    if let Some(problem) = check_proxy_url(config.youtube_proxy.trim()) {
        return invalid(problem);
    }
    if config.youtube_native {
        let yt_api = config.youtube_endpoint.trim();
        let yt_music = config.youtube_music_endpoint.trim();
        if yt_api.is_empty() || yt_music.is_empty() {
            return invalid(
                "youtubeEndpoint and youtubeMusicEndpoint are required while youtubeNative is on",
            );
        }
        if let Some(problem) = check_http_endpoint("youtubeEndpoint", yt_api) {
            return invalid(problem);
        }
        if let Some(problem) = check_http_endpoint("youtubeMusicEndpoint", yt_music) {
            return invalid(problem);
        }
    }
    if config.soundcloud_native {
        let sc_api = config.soundcloud_api_endpoint.trim();
        let sc_web = config.soundcloud_web_endpoint.trim();
        if sc_api.is_empty() || sc_web.is_empty() {
            return invalid(
                "soundcloudApiEndpoint and soundcloudWebEndpoint are required while soundcloudNative is on",
            );
        }
        if let Some(problem) = check_http_endpoint("soundcloudApiEndpoint", sc_api) {
            return invalid(problem);
        }
        if let Some(problem) = check_http_endpoint("soundcloudWebEndpoint", sc_web) {
            return invalid(problem);
        }
    }
    let deezer_api = config.deezer_api_endpoint.trim();
    if deezer_api.is_empty() {
        return invalid("deezerApiEndpoint is required");
    }
    if let Some(problem) = check_http_endpoint("deezerApiEndpoint", deezer_api) {
        return invalid(problem);
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
        return invalid("opusBitrate must be 16000..256000");
    }
    if config.opus_complexity > 10 {
        return invalid("opusComplexity must be 0..10");
    }
    let has_spotify_id = !config.spotify_client_id.trim().is_empty();
    let has_spotify_secret = !config.spotify_client_secret.trim().is_empty();
    if has_spotify_id != has_spotify_secret {
        return invalid("Spotify client ID and secret must both be set");
    }
    if !config.spotify_refresh_token.trim().is_empty() && !has_spotify_id {
        return invalid("Spotify refresh token requires the application client ID and secret");
    }
    if has_spotify_id {
        let market = config.spotify_market.trim();
        if market.is_empty() {
            return invalid("spotifyMarket is required when Spotify credentials are set");
        }
        if market.len() != 2 || !market.bytes().all(|b| b.is_ascii_alphabetic()) {
            return invalid("spotifyMarket must be a two-letter ISO country code such as US or TW");
        }
    }
    if !config.youtube_client_version.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return invalid("youtubeClientVersion must contain only printable ASCII characters");
    }
    if !config.youtube_visitor_data.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return invalid("youtubeVisitorData must contain only printable ASCII characters");
    }
    Ok(())
}

/// Checks the bind address syntax (`host:port` or `[ipv6]:port`) before startup.
fn check_address(address: &str) -> Option<String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Some("address is required".into());
    }
    if trimmed != address || trimmed.chars().any(char::is_whitespace) {
        return Some("address must not contain whitespace".into());
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, port)) = rest.rsplit_once("]:") else {
            return Some("IPv6 address must use [host]:port syntax".into());
        };
        if host.parse::<Ipv6Addr>().is_err() {
            return Some(format!("address has an invalid IPv6 host: [{host}]"));
        }
        if port.parse::<u16>().is_err() {
            return Some(format!("address has an invalid port: {port}"));
        }
        return None;
    }
    let Some((host, port)) = trimmed.rsplit_once(':') else {
        return Some("address must be host:port (or [ipv6]:port)".into());
    };
    if host.contains(':') {
        return Some(format!("IPv6 address must wrap the host in brackets, e.g. [{host}]:{port}"));
    }
    if host.is_empty() || host.contains(['/', '?', '#', '@', '\\']) {
        return Some("address must have a valid host before :port".into());
    }
    if port.parse::<u16>().is_err() {
        return Some(format!("address has an invalid port: {port}"));
    }
    None
}

fn strip_scheme_ascii_case<'a>(url: &'a str, schemes: &[&str]) -> Option<&'a str> {
    schemes.iter().find_map(|scheme| {
        let (prefix, rest) = url.split_at_checked(scheme.len())?;
        prefix.eq_ignore_ascii_case(scheme).then_some(rest)
    })
}

fn valid_authority_host_port(host_port: &str) -> bool {
    if host_port.is_empty()
        || host_port.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return false;
    }
    if let Some(rest) = host_port.strip_prefix('[') {
        let Some((ipv6, tail)) = rest.split_once(']') else {
            return false;
        };
        if ipv6.parse::<Ipv6Addr>().is_err() {
            return false;
        }
        return match tail.strip_prefix(':') {
            None => tail.is_empty(),
            Some(port) => !port.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0),
        };
    }
    match host_port.split_once(':') {
        None => true,
        Some((host, port)) => {
            !host.is_empty()
                && !port.is_empty()
                && !port.contains(':')
                && port.parse::<u16>().is_ok_and(|p| p > 0)
        }
    }
}

fn has_valid_url_authority(url: &str, schemes: &[&str]) -> bool {
    if url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return false;
    }
    let Some(rest) = strip_scheme_ascii_case(url, schemes) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    valid_authority_host_port(host_port)
}

fn check_http_endpoint(name: &str, url: &str) -> Option<String> {
    let valid =
        has_valid_url_authority(url, &["https://", "http://"]) && !url.contains(['@', '?', '#']);
    (!valid).then(|| {
        format!("{name} must be an http or https base URL without credentials, query, or fragment")
    })
}

fn check_webhook_url(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let valid = has_valid_url_authority(url, &["https://", "http://"]) && !url.contains(['@', '#']);
    (!valid).then(|| "webhook.url must be an http or https URL without credentials".into())
}

fn check_proxy_url(url: &str) -> Option<String> {
    const SCHEMES: [&str; 6] =
        ["http://", "https://", "socks4://", "socks4a://", "socks5://", "socks5h://"];
    if url.is_empty() || (has_valid_url_authority(url, &SCHEMES) && !url.contains(['?', '#'])) {
        return None;
    }
    Some(
        "youtubeProxy must be an http://, https://, socks4://, socks4a://, socks5:// or socks5h:// URL with a host"
            .into(),
    )
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
    fn a_youtube_proxy_needs_a_supported_scheme_and_a_host() {
        for good in [
            "",
            "socks5://127.0.0.1:40000",
            "http://user:pass@proxy.example:8080",
            "socks5h://p.example",
        ] {
            let config = Config { youtube_proxy: good.into(), ..valid() };
            validate(&config).unwrap_or_else(|error| panic!("{good}: {error:?}"));
        }
        for bad in
            ["127.0.0.1:40000", "ftp://proxy.example", "socks5://", "http://user@:8080", "測試測試"]
        {
            let config = Config { youtube_proxy: bad.into(), ..valid() };
            assert!(validate(&config).is_err(), "{bad} was accepted");
        }
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

    #[test]
    fn passwords_addresses_filters_and_markets_reject_unusable_values() {
        for bad_pw in ["   ", " secret", "secret ", "secret\n", "密碼"] {
            let config = Config { password: bad_pw.into(), ..valid() };
            assert!(validate(&config).is_err(), "password {bad_pw:?} should fail");
        }
        for good_addr in ["0.0.0.0:2333", "127.0.0.1:0", "[::]:2333", "[::1]:2333", "localhost:80"]
        {
            let config = Config { address: good_addr.into(), ..valid() };
            validate(&config).unwrap_or_else(|e| panic!("{good_addr}: {e}"));
        }
        for bad_addr in ["::1:2333", ":::2333", "0.0.0.0", "0.0.0.0:70000", "[not-ip]:2333", ":80"]
        {
            let config = Config { address: bad_addr.into(), ..valid() };
            assert!(validate(&config).is_err(), "address {bad_addr:?} should fail");
        }
        let mut bad_filter = valid();
        bad_filter.filters.insert("lowpass".into(), false);
        assert!(validate(&bad_filter).is_err());
        bad_filter.filters.clear();
        bad_filter.filters.insert("lowPass".into(), false);
        validate(&bad_filter).unwrap();

        let no_audio = Config { in_process_audio: false, ffmpeg: "none".into(), ..valid() };
        assert!(validate(&no_audio).is_err());

        let bad_market = Config {
            spotify_client_id: "id".into(),
            spotify_client_secret: "secret".into(),
            spotify_market: "US&limit=50".into(),
            ..valid()
        };
        assert!(validate(&bad_market).is_err());
    }
}
