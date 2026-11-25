//! Configuration validation before the node binds or opens network connections.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{
    Config, ConfigError, FILTER_NAMES, LOG_COLORS, LOG_FORMATS, LOG_LEVELS, LYRICS_SOURCES,
};

const MAX_LIMIT: u32 = 100_000;
const MAX_EVENT_BUFFER: u32 = 16_384;

fn invalid(message: impl Into<String>) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid(message.into()))
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.password.is_empty() {
        return invalid("set RUSTLINK_PASSWORD or password in config");
    }
    if config.password.trim() != config.password {
        return invalid("password must not have leading or trailing whitespace");
    }
    if !is_printable_ascii(&config.password) {
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
    if let Some(problem) = check_webhook_url(webhook_url, config.allow_private_networks) {
        return invalid(problem);
    }
    if let Some(problem) = check_proxy_url(config.youtube_proxy.trim()) {
        return invalid(problem);
    }
    if config.youtube_native {
        require_endpoints(
            &[
                ("youtubeEndpoint", &config.youtube_endpoint),
                ("youtubeMusicEndpoint", &config.youtube_music_endpoint),
            ],
            "youtubeEndpoint and youtubeMusicEndpoint are required while youtubeNative is on",
        )?;
    }
    if config.soundcloud_native {
        require_endpoints(
            &[
                ("soundcloudApiEndpoint", &config.soundcloud_api_endpoint),
                ("soundcloudWebEndpoint", &config.soundcloud_web_endpoint),
            ],
            "soundcloudApiEndpoint and soundcloudWebEndpoint are required while soundcloudNative is on",
        )?;
    }
    require_endpoints(
        &[("deezerApiEndpoint", &config.deezer_api_endpoint)],
        "deezerApiEndpoint is required",
    )?;

    for (name, value, min, max) in [
        ("webhook.stateLatencySeconds", config.webhook.state_latency_seconds, 0, MAX_LIMIT),
        ("logRetentionDays", config.log_retention_days, 0, MAX_LIMIT),
        ("maxPlayers", config.max_players, 1, MAX_LIMIT),
        ("maxSessions", config.max_sessions, 1, MAX_LIMIT),
        ("maxConcurrentLoads", config.max_concurrent_loads, 1, MAX_LIMIT),
        ("maxPlaylistTracks", config.max_playlist_tracks, 1, MAX_LIMIT),
        ("searchResults", config.search_results, 1, MAX_LIMIT),
        ("cacheEntries", config.cache_entries, 1, MAX_LIMIT),
        ("cacheTtlSeconds", config.cache_ttl_seconds, 1, MAX_LIMIT),
        ("loadTimeoutSeconds", config.load_timeout_seconds, 1, MAX_LIMIT),
        ("playerUpdateSeconds", config.player_update_seconds, 1, MAX_LIMIT),
        ("statsSeconds", config.stats_seconds, 1, MAX_LIMIT),
        ("soundcloudClientIdTtlSeconds", config.soundcloud_client_id_ttl_seconds, 1, MAX_LIMIT),
        ("eventBuffer", config.event_buffer, 1, MAX_EVENT_BUFFER),
    ] {
        if !(min..=max).contains(&value) {
            return invalid(format!("{name} must be between {min} and {max}"));
        }
    }
    if !(16_000..=256_000).contains(&config.opus_bitrate) {
        return invalid("opusBitrate must be 16000..256000");
    }
    if config.opus_complexity > 10 {
        return invalid("opusComplexity must be 0..10");
    }
    for (name, value) in [
        ("youtubePoToken", config.youtube_po_token.as_str()),
        ("youtubeVisitorData", config.youtube_visitor_data.as_str()),
        ("soundcloudClientId", config.soundcloud_client_id.as_str()),
        ("spotifyClientId", config.spotify_client_id.as_str()),
        ("spotifyClientSecret", config.spotify_client_secret.as_str()),
        ("spotifyRefreshToken", config.spotify_refresh_token.as_str()),
    ] {
        if has_control_bytes(value) {
            return invalid(format!("{name} must not contain control characters"));
        }
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
    for (name, value) in [
        ("youtubeClientVersion", config.youtube_client_version.as_str()),
        ("youtubeVisitorData", config.youtube_visitor_data.as_str()),
    ] {
        if !is_printable_ascii(value) {
            return invalid(format!("{name} must contain only printable ASCII characters"));
        }
    }
    Ok(())
}

fn require_endpoints(endpoints: &[(&str, &str)], missing_msg: &str) -> Result<(), ConfigError> {
    if endpoints.iter().any(|(_, url)| url.trim().is_empty()) {
        return invalid(missing_msg);
    }
    for &(name, url) in endpoints {
        if let Some(problem) = check_http_endpoint(name, url.trim()) {
            return invalid(problem);
        }
    }
    Ok(())
}

fn has_control_bytes(value: &str) -> bool {
    value.bytes().any(|b| b <= 0x1f || b == 0x7f) || value.chars().any(char::is_control)
}

fn valid_port_digits(port: &str, allow_zero: bool) -> bool {
    !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| allow_zero || p > 0)
}

fn valid_non_ipv6_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.starts_with('.') {
        return false;
    }
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() {
        return false;
    }
    let mut last = "";
    for label in trimmed.split('.') {
        if !(1..=63).contains(&label.len())
            || label.starts_with('-')
            || label.ends_with('-')
            || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return false;
        }
        last = label;
    }
    if last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x") || last.starts_with("0X")
    {
        return host == trimmed && host.parse::<Ipv4Addr>().is_ok();
    }
    true
}

fn check_address(address: &str) -> Option<String> {
    if has_control_bytes(address) {
        return Some("address must not contain control characters".into());
    }
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Some("address is required".into());
    }
    if trimmed != address || trimmed.chars().any(char::is_whitespace) {
        return Some("address must not contain whitespace".into());
    }
    let port = if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, port)) = rest.rsplit_once("]:") else {
            return Some("IPv6 address must use [host]:port syntax".into());
        };
        if host.contains(['[', ']']) || host.parse::<Ipv6Addr>().is_err() {
            return Some(format!("address has an invalid IPv6 host: [{host}]"));
        }
        port
    } else {
        let Some((host, port)) = trimmed.rsplit_once(':') else {
            return Some("address must be host:port (or [ipv6]:port)".into());
        };
        if host.contains(':') {
            return Some(format!(
                "IPv6 address must wrap the host in brackets, e.g. [{host}]:{port}"
            ));
        }
        if host.contains(['[', ']']) || !valid_non_ipv6_host(host) {
            return Some("address must have a valid host before :port".into());
        }
        port
    };
    (!valid_port_digits(port, true)).then(|| format!("address has an invalid port: {port}"))
}

fn strip_scheme_ascii_case<'a>(url: &'a str, schemes: &[&str]) -> Option<&'a str> {
    schemes.iter().find_map(|scheme| {
        let (prefix, rest) = url.split_at_checked(scheme.len())?;
        prefix.eq_ignore_ascii_case(scheme).then_some(rest)
    })
}

fn is_rfc3986_unreserved_or_subdelim(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

fn valid_percent_encoding(text: &str) -> bool {
    let mut rest = text.as_bytes();
    while let Some(pos) = rest.iter().position(|&b| b == b'%') {
        let Some(&[h1, h2, ref tail @ ..]) = rest.get(pos + 1..) else {
            return false;
        };
        if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
            return false;
        }
        rest = tail;
    }
    true
}

fn valid_userinfo(userinfo: &str) -> bool {
    !userinfo.is_empty()
        && valid_percent_encoding(userinfo)
        && userinfo.bytes().all(|b| is_rfc3986_unreserved_or_subdelim(b) || b == b':' || b == b'%')
}

fn parse_authority_host(host_port: &str) -> Option<&str> {
    if host_port.is_empty() {
        return None;
    }
    if let Some(rest) = host_port.strip_prefix('[') {
        let (ipv6, tail) = rest.split_once(']')?;
        if ipv6.contains(['[', ']']) || ipv6.parse::<Ipv6Addr>().is_err() {
            return None;
        }
        return match tail.strip_prefix(':') {
            None if tail.is_empty() => Some(ipv6),
            Some(port) if valid_port_digits(port, false) => Some(ipv6),
            _ => None,
        };
    }
    if host_port.contains(['[', ']']) {
        return None;
    }
    match host_port.split_once(':') {
        None => valid_non_ipv6_host(host_port).then_some(host_port),
        Some((host, port)) => {
            (!port.contains(':') && valid_non_ipv6_host(host) && valid_port_digits(port, false))
                .then_some(host)
        }
    }
}

struct ParsedUrl<'a> {
    has_userinfo: bool,
    host: &'a str,
    tail: &'a str,
}

fn parse_rfc3986_url<'a>(url: &'a str, schemes: &[&str]) -> Option<ParsedUrl<'a>> {
    if !url.bytes().all(|b| {
        (0x21..=0x7e).contains(&b)
            && !matches!(b, b'\\' | b'<' | b'>' | b'|' | b'^' | b'{' | b'}' | b'`' | b'"')
    }) || !valid_percent_encoding(url)
    {
        return None;
    }
    let rest = strip_scheme_ascii_case(url, schemes)?;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    if tail.contains(['[', ']']) {
        return None;
    }
    let (has_userinfo, host_port) = match authority.split_once('@') {
        Some((userinfo, rest_host)) => {
            if rest_host.contains('@') || !valid_userinfo(userinfo) {
                return None;
            }
            (true, rest_host)
        }
        None => (false, authority),
    };
    Some(ParsedUrl { has_userinfo, host: parse_authority_host(host_port)?, tail })
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || matches!(
            address.octets(),
            [0 | 224.., ..]
                | [100, 64..=127, ..]
                | [198, 18 | 19, ..]
                | [192, 88, 99, _]
                | [192, 0, 0, _]
        ))
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    let s = address.segments();
    (s[0] & 0xe000 == 0x2000)
        && !matches!(
            (s[0], s[1]),
            (0x2001, 0x0db8) | (0x2002, _) | (0x2001, ..0x0200) | (0x3fff, ..0x1000)
        )
}

fn is_disallowed_private_host(host: &str) -> bool {
    let bare = host.trim_end_matches('.');
    if bare.eq_ignore_ascii_case("localhost")
        || bare
            .as_bytes()
            .get(bare.len().wrapping_sub(10)..)
            .is_some_and(|s| s.eq_ignore_ascii_case(b".localhost"))
    {
        return true;
    }
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => !is_public_v4(v4),
        Ok(IpAddr::V6(v6)) => !is_public_v6(v6),
        Err(_) => false,
    }
}

fn check_http_endpoint(name: &str, url: &str) -> Option<String> {
    let valid = parse_rfc3986_url(url, &["https://", "http://"])
        .is_some_and(|p| !p.has_userinfo && !p.tail.contains(['@', '?', '#']));
    (!valid).then(|| {
        format!("{name} must be an http or https base URL without credentials, query, or fragment")
    })
}

fn check_webhook_url(url: &str, allow_private_networks: bool) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let Some(parsed) = parse_rfc3986_url(url, &["https://", "http://"]) else {
        return Some("webhook.url must be an http or https URL without credentials".into());
    };
    if parsed.has_userinfo || parsed.tail.contains('#') {
        return Some("webhook.url must be an http or https URL without credentials".into());
    }
    (!allow_private_networks && is_disallowed_private_host(parsed.host)).then(|| {
        "webhook.url cannot target localhost or a non-public IP address while allowPrivateNetworks is false"
            .into()
    })
}

fn check_proxy_url(url: &str) -> Option<String> {
    const SCHEMES: [&str; 6] =
        ["http://", "https://", "socks4://", "socks4a://", "socks5://", "socks5h://"];
    let valid =
        url.is_empty() || parse_rfc3986_url(url, &SCHEMES).is_some_and(|p| p.tail.is_empty());
    (!valid).then(|| {
        "youtubeProxy must be an http://, https://, socks4://, socks4a://, socks5:// or socks5h:// URL with a host and no path"
            .into()
    })
}

fn one_of(value: &str, allowed: &[&str]) -> bool {
    let v = value.trim();
    allowed.iter().any(|c| c.eq_ignore_ascii_case(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Config {
        let mut config = Config::default();
        config.password = "secret".into();
        config
    }

    fn with(mutate: impl FnOnce(&mut Config)) -> Result<(), ConfigError> {
        let mut config = valid();
        mutate(&mut config);
        validate(&config)
    }

    #[test]
    fn a_password_is_required() {
        assert!(with(|c| c.password.clear()).is_err());
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
            with(|c| c.youtube_proxy = good.into()).unwrap_or_else(|e| panic!("{good}: {e:?}"));
        }
        for bad in [
            "127.0.0.1:40000",
            "ftp://proxy.example",
            "socks5://",
            "http://user@:8080",
            "測試測試",
            "http://user@other@proxy.example:8080",
            "socks5://127.0.0.1:1080/trailing",
            "http://proxy.example:8080/",
            "http://999.0.0.1:8080",
            "http://proxy<bad>.example:8080",
        ] {
            assert!(with(|c| c.youtube_proxy = bad.into()).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn limits_are_bounded() {
        assert!(with(|c| c.opus_bitrate = 8_000).is_err());
        assert!(with(|c| c.opus_complexity = 11).is_err());
        with(|c| {
            c.opus_bitrate = 128_000;
            c.opus_complexity = 4;
        })
        .unwrap();
        assert!(with(|c| c.max_players = 0).is_err());
    }

    #[test]
    fn a_webhook_needs_a_url_when_it_is_enabled() {
        assert!(with(|c| c.webhook.enable = true).is_err());
        assert!(with(|c| {
            c.webhook.enable = true;
            c.webhook.url = "not-a-url".into();
        })
        .is_err());
        with(|c| {
            c.webhook.enable = true;
            c.webhook.url = "https://discord.com/api/webhooks/1/abc".into();
        })
        .unwrap();
    }

    #[test]
    fn spotify_credentials_come_in_pairs() {
        assert!(with(|c| c.spotify_client_id = "id".into()).is_err());
        with(|c| {
            c.spotify_client_id = "id".into();
            c.spotify_client_secret = "secret".into();
        })
        .unwrap();
        with(|c| {
            c.spotify_client_id = "id".into();
            c.spotify_client_secret = "secret".into();
            c.spotify_refresh_token = "token".into();
        })
        .unwrap();
        assert!(with(|c| c.spotify_refresh_token = "token".into()).is_err());
    }

    #[test]
    fn enum_like_options_are_checked_case_insensitively() {
        with(|c| c.log_level = "DEBUG".into()).unwrap();
        assert!(with(|c| c.log_level = "verbose".into()).is_err());
    }

    #[test]
    fn a_native_client_needs_its_endpoints() {
        assert!(with(|c| c.youtube_music_endpoint.clear()).is_err());
        with(|c| {
            c.youtube_music_endpoint.clear();
            c.youtube_native = false;
        })
        .unwrap();
    }

    #[test]
    fn a_log_retention_of_zero_is_allowed_and_an_absurd_one_is_not() {
        with(|c| c.log_retention_days = 0).unwrap();
        assert!(with(|c| c.log_retention_days = MAX_LIMIT + 1).is_err());
    }

    #[test]
    fn passwords_addresses_filters_and_markets_reject_unusable_values() {
        for bad_pw in ["   ", " secret", "secret ", "secret\n", "密碼"] {
            assert!(with(|c| c.password = bad_pw.into()).is_err(), "{bad_pw:?}");
        }
        for good_addr in ["0.0.0.0:2333", "127.0.0.1:0", "[::]:2333", "[::1]:2333", "localhost:80"]
        {
            with(|c| c.address = good_addr.into()).unwrap_or_else(|e| panic!("{good_addr}: {e}"));
        }
        for bad_addr in [
            "::1:2333",
            ":::2333",
            "0.0.0.0",
            "0.0.0.0:70000",
            "[not-ip]:2333",
            ":80",
            "127.0.0.256:2333",
            "0.0.0.0:+2333",
            "127.0.0.1:2333\x00",
            "host]bad:2333",
            "host|bad:2333",
        ] {
            assert!(with(|c| c.address = bad_addr.into()).is_err(), "{bad_addr:?}");
        }
        assert!(with(|c| {
            c.filters.insert("lowpass".into(), false);
        })
        .is_err());
        with(|c| {
            c.filters.insert("lowPass".into(), false);
        })
        .unwrap();
        assert!(with(|c| {
            c.in_process_audio = false;
            c.ffmpeg = "none".into();
        })
        .is_err());
        assert!(with(|c| {
            c.spotify_client_id = "id".into();
            c.spotify_client_secret = "secret".into();
            c.spotify_market = "US&limit=50".into();
        })
        .is_err());
    }

    #[test]
    fn control_bytes_event_buffer_cap_rfc3986_authorities_and_private_webhooks_are_enforced() {
        with(|c| c.event_buffer = 16_384).unwrap();
        assert!(with(|c| c.event_buffer = 16_385).is_err());

        for mutate in [
            |c: &mut Config| c.youtube_po_token = "tok\x07en".into(),
            |c: &mut Config| c.youtube_visitor_data = "vis\x7f".into(),
            |c: &mut Config| c.soundcloud_client_id = "sc\n".into(),
            |c: &mut Config| {
                c.spotify_client_id = "id\x1b".into();
                c.spotify_client_secret = "secret".into();
            },
            |c: &mut Config| {
                c.spotify_client_id = "id".into();
                c.spotify_client_secret = "sec\x00ret".into();
            },
            |c: &mut Config| {
                c.spotify_client_id = "id".into();
                c.spotify_client_secret = "secret".into();
                c.spotify_refresh_token = "ref\r".into();
            },
        ] {
            assert!(with(mutate).is_err());
        }

        for bad_url in [
            "https://bad^host.example/hook",
            "https://bad{host}.example/hook",
            "https://bad`host.example/hook",
            "https://bad\"host.example/hook",
            "https://bad|host.example/hook",
            "https://127.0.0.256/hook",
            "http://localhost:8080/hook",
            "http://sub.localhost./hook",
            "http://127.0.0.1:8080/hook",
            "http://10.0.0.5/hook",
            "http://[::1]:8080/hook",
            "http://[::ffff:127.0.0.1]/hook",
        ] {
            assert!(
                with(|c| {
                    c.webhook.enable = true;
                    c.webhook.url = bad_url.into();
                })
                .is_err(),
                "{bad_url}"
            );
        }

        with(|c| {
            c.allow_private_networks = true;
            c.webhook.enable = true;
            c.webhook.url = "http://127.0.0.1:8080/hook".into();
        })
        .unwrap();
    }
}
