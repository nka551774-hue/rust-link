//! Strict validation for untrusted client-supplied Discord `VoiceState` updates.

use rustlink_protocol::VoiceState;

use crate::VoiceError;

/// Whether `raw` is a canonical non-zero 64-bit decimal Discord snowflake.
pub fn valid_id(raw: &str) -> bool {
    (1..=20).contains(&raw.len())
        && !raw.starts_with('0')
        && raw.bytes().all(|b| b.is_ascii_digit())
        && raw.parse::<u64>().is_ok()
}

fn has_ascii_suffix(text: &str, suffix: &str) -> bool {
    text.as_bytes()
        .get(text.len().wrapping_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix.as_bytes()))
}

fn has_ascii_whitespace_or_control(raw: &str) -> bool {
    raw.bytes().any(|b| b <= b' ' || b == 0x7f)
}

/// Validates a `VoiceState` before opening a Discord voice WebSocket.
pub fn validate(state: &VoiceState) -> Result<(), VoiceError> {
    let channel = state.channel_id.as_deref().unwrap_or_default();
    if state.token.is_empty()
        || state.session_id.is_empty()
        || state.endpoint.is_empty()
        || !valid_id(channel)
    {
        return Err(VoiceError::Invalid(
            "voice requires token, session_id, endpoint and channel_id".into(),
        ));
    }
    if state.token.len() > 512 || state.session_id.len() > 256 || state.endpoint.len() > 512 {
        return Err(VoiceError::Invalid("voice credentials or endpoint are too long".into()));
    }
    if [state.token.as_str(), state.session_id.as_str(), state.endpoint.as_str()]
        .into_iter()
        .any(has_ascii_whitespace_or_control)
    {
        return Err(VoiceError::Invalid(
            "voice credentials or endpoint contain whitespace or control characters".into(),
        ));
    }
    let endpoint = &state.endpoint;
    let Some(host) = voice_host(endpoint) else {
        return Err(VoiceError::Invalid(format!("invalid Discord voice endpoint {endpoint:?}")));
    };
    if !has_ascii_suffix(host, ".discord.media") && !has_ascii_suffix(host, ".discord.gg") {
        return Err(VoiceError::Invalid(format!(
            "voice endpoint {endpoint:?} must be a Discord voice hostname"
        )));
    }
    Ok(())
}

/// Extracts `host` from `host[:port]`, rejecting paths, userinfo, IPv6 literals, and malformed ports/labels.
fn voice_host(endpoint: &str) -> Option<&str> {
    if endpoint.is_empty()
        || endpoint.len() > 259
        || has_ascii_whitespace_or_control(endpoint)
        || endpoint.contains(['/', '?', '#', '@'])
    {
        return None;
    }
    let host = match endpoint.rsplit_once(':') {
        Some((host, port)) => {
            if port.starts_with('0')
                || !port.bytes().all(|b| b.is_ascii_digit())
                || port.parse::<u16>().is_err()
            {
                return None;
            }
            host
        }
        None => endpoint,
    };
    (!host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }))
    .then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(endpoint: &str, channel: Option<&str>) -> VoiceState {
        VoiceState {
            token: "token".into(),
            session_id: "session".into(),
            endpoint: endpoint.into(),
            channel_id: channel.map(str::to_string),
        }
    }

    #[test]
    fn a_zero_or_invented_id_is_not_an_id() {
        assert!(valid_id("123456789012345678"));
        for bad in ["0", "", "not a number", "12 34"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn snowflakes_cannot_differ_from_the_identify_payload_by_whitespace() {
        for id in [" 123", "123 ", "\t123", "123\n", "\u{2003}123", "18446744073709551616"] {
            assert!(!valid_id(id), "{id:?}");
        }
        assert!(valid_id("18446744073709551615"));
    }

    #[test]
    fn a_discord_voice_host_with_a_port_is_accepted() {
        assert!(validate(&state("c-iad21-1a2b3c.discord.media:443", Some("123"))).is_ok());
    }

    #[test]
    fn a_host_that_is_not_discords_is_refused() {
        let error = validate(&state("evil.example.com", Some("123"))).unwrap_err();
        assert!(error.to_string().contains("must be a Discord voice hostname"));
    }

    #[test]
    fn a_discord_host_with_a_path_or_userinfo_is_refused() {
        for bad in ["c-iad21-1a2b3c.discord.media/../x", "user@c-iad21-1a2b3c.discord.media"] {
            assert!(validate(&state(bad, Some("123"))).is_err(), "{bad}");
        }
    }

    #[test]
    fn malformed_voice_hosts_are_refused_before_connecting() {
        for endpoint in [
            "other.example\\a.discord.media",
            "other.example\\a.discord.gg:443",
            "a\r.discord.media",
            "a\n.discord.media",
            "a%2e.discord.media",
            ".discord.media",
            "a..discord.media",
            "-a.discord.media",
            "a-.discord.media",
            "a.discord.media:0",
        ] {
            assert!(validate(&state(endpoint, Some("123"))).is_err(), "{endpoint:?}");
        }
    }

    #[test]
    fn a_missing_channel_or_token_is_refused() {
        assert!(validate(&state("a.discord.media", None)).is_err());
        let mut missing = state("a.discord.media", Some("123"));
        missing.token.clear();
        assert!(validate(&missing).is_err());
    }

    #[test]
    fn oversized_voice_fields_are_refused_before_connecting() {
        for mutate in [
            |s: &mut VoiceState| s.token = "t".repeat(513),
            |s: &mut VoiceState| s.session_id = "s".repeat(257),
            |s: &mut VoiceState| s.endpoint = format!("{}.discord.media", "a".repeat(512)),
        ] {
            let mut s = state("a.discord.media", Some("123"));
            mutate(&mut s);
            assert!(validate(&s).is_err());
        }
    }

    #[test]
    fn a_voice_host_is_recognised_with_or_without_a_port() {
        assert_eq!(voice_host("a.discord.media"), Some("a.discord.media"));
        assert_eq!(voice_host("a.discord.media:443"), Some("a.discord.media"));
        for bad in ["a.discord.media:not-a-port", "a.discord.media:0443", "a.discord.media:00"] {
            assert_eq!(voice_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn empty_endpoint_whitespace_controls_and_leading_zero_ports_are_refused() {
        let err = validate(&state("", Some("123"))).unwrap_err();
        assert!(err.to_string().contains("requires token, session_id, endpoint and channel_id"));
        for ep in [
            "a.discord.media:0443",
            " a.discord.media:443",
            "a.discord.media:443 ",
            "a.discord.media\x7f",
        ] {
            assert!(validate(&state(ep, Some("123"))).is_err(), "{ep:?}");
        }
        for bad in ["tok en", " token", "token\n", "token\0", "token\x1f", "token\x7f"] {
            let mut s = state("a.discord.media:443", Some("123"));
            s.token = bad.into();
            assert!(validate(&s).is_err(), "bad token: {bad:?}");
            let mut s = state("a.discord.media:443", Some("123"));
            s.session_id = bad.into();
            assert!(validate(&s).is_err(), "bad session_id: {bad:?}");
        }
    }
}
