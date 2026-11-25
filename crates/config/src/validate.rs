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

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    let invalid = |message: String| Err(ConfigError::Invalid(message));

    if config.password.is_empty() {
        return invalid("set RUSTLINK_PASSWORD or password in config".into());
    }
    if config.password.trim() != config.password {
        return invalid("password must not have leading or trailing whitespace".into());
    }
    if !config.password.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return invalid(
            "password must contain only printable ASCII characters (HTTP Authorization header limit)"
                .into(),
        );
    }
    if let Some(problem) = check_address(&config.address) {
        return invalid(problem);
    }
    if config.auto_download && config.tools_directory.trim().is_empty() {
        return invalid("toolsDirectory is required while autoDownload is on".into());
    }
    if !config.in_process_audio
        && (config.ffmpeg.trim().is_empty() || config.ffmpeg.trim().eq_ignore_ascii_case("none"))
    {
        return invalid("ffmpeg is required when inProcessAudio is false".into());
    }
    for name in config.filters.keys() {
        if !FILTER_NAMES.contains(&name.as_str()) {
            return invalid(format!(
                "unknown filter {name:?} in filters; expected one of {}",
                FILTER_NAMES.join(", ")
            ));
        }
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
    if let Some(problem) = check_proxy_url(config.youtube_proxy.trim()) {
        return invalid(problem);
    }
    if config.youtube_native {
        if config.youtube_endpoint.trim().is_empty()
            || config.youtube_music_endpoint.trim().is_empty()
        {
            return invalid(
                "youtubeEndpoint and youtubeMusicEndpoint are required while youtubeNative is on"
                    .into(),
            );
        }
        if let Some(problem) =
            check_http_endpoint("youtubeEndpoint", config.youtube_endpoint.trim())
        {
            return invalid(problem);
        }
        if let Some(problem) =
            check_http_endpoint("youtubeMusicEndpoint", config.youtube_music_endpoint.trim())
        {
            return invalid(problem);
        }
    }
    if config.soundcloud_native {
        if config.soundcloud_api_endpoint.trim().is_empty()
            || config.soundcloud_web_endpoint.trim().is_empty()
        {
            return invalid(
                "soundcloudApiEndpoint and soundcloudWebEndpoint are required while soundcloudNative is on"
                    .into(),
            );
        }
        if let Some(problem) =
            check_http_endpoint("soundcloudApiEndpoint", config.soundcloud_api_endpoint.trim())
        {
            return invalid(problem);
        }
        if let Some(problem) =
            check_http_endpoint("soundcloudWebEndpoint", config.soundcloud_web_endpoint.trim())
        {
            return invalid(problem);
        }
    }
    if config.deezer_api_endpoint.trim().is_empty() {
        return invalid("deezerApiEndpoint is required".into());
    }
    if let Some(problem) =
        check_http_endpoint("deezerApiEndpoint", config.deezer_api_endpoint.trim())
    {
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
        return invalid("opusBitrate must be 16000..256000".into());
    }
    if config.opus_complexity > 10 {
        return invalid("opusComplexity must be 0..10".into());
    }
    if config.spotify_client_id.trim().is_empty() != config.spotify_client_secret.trim().is_empty()
    {
        return invalid("Spotify client ID and secret must both be set".into());
    }
    if !config.spotify_refresh_token.trim().is_empty() && config.spotify_client_id.trim().is_empty()
    {
        return invalid(
            "Spotify refresh token requires the application client ID and secret".into(),
        );
    }
    if !config.spotify_client_id.trim().is_empty() {
        let market = config.spotify_market.trim();
        if market.is_empty() {
            return invalid("spotifyMarket is required when Spotify credentials are set".into());
        }
        if market.len() != 2 || !market.bytes().all(|b| b.is_ascii_alphabetic()) {
            return invalid(
                "spotifyMarket must be a two-letter ISO country code such as US or TW".into(),
            );
        }
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
        if url.len() >= scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme) {
            Some(&url[scheme.len()..])
        } else {
            None
        }
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
            Some(port) => !port.is_empty() && port.parse::<u16>().is_ok(),
        };
    }
    match host_port.split_once(':') {
        None => true,
        Some((host, port)) => {
            !host.is_empty()
                && !port.is_empty()
                && !port.contains(':')
                && port.parse::<u16>().is_ok()
        }
    }
}

fn check_http_endpoint(name: &str, url: &str) -> Option<String> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Some(format!("{name} must be an http or https URL"));
    }
    let Some(rest) = strip_scheme_ascii_case(url, &["https://", "http://"]) else {
        return Some(format!("{name} must be an http or https URL"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    if !valid_authority_host_port(host_port) {
        return Some(format!("{name} must be an http or https URL"));
    }
    None
}

fn check_webhook_url(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    check_http_endpoint("webhook.url", url)
}

fn check_proxy_url(url: &str) -> Option<String> {
    const SCHEMES: [&str; 4] = ["http://", "socks4://", "socks5://", "socks5h://"];
    let problem = || {
        Some(
            "youtubeProxy must be an http://, socks4://, socks5:// or socks5h:// URL with a host"
                .into(),
        )
    };
    if url.is_empty() {
        return None;
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return problem();
    }
    let Some(rest) = strip_scheme_ascii_case(url, &SCHEMES) else {
        return problem();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    if !valid_authority_host_port(host_port) {
        return problem();
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
        for bad in ["127.0.0.1:40000", "ftp://proxy.example", "socks5://", "http://user@:8080"] {
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
