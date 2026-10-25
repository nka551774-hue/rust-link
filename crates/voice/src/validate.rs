//! Discord voice state validation.
//!
//! A voice update arrives from a client, which means every field in it is
//! attacker-controlled. The checks here are what keep a client from pointing the
//! node's voice socket at a host of its choosing.

use rustlink_protocol::VoiceState;

use crate::VoiceError;

/// Whether a string is a Discord snowflake that names something.
///
/// Zero is not a valid id, and neither is anything that is not a decimal number,
/// so an empty or invented value is refused rather than turned into a plausible
/// one.
pub fn valid_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 20 {
        return false;
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match trimmed.parse::<u64>() {
        Ok(value) => value != 0,
        Err(_) => false,
    }
}

/// Checks a voice state before the node acts on it.
pub fn validate(state: &VoiceState) -> Result<(), VoiceError> {
    let channel = state.channel_id.as_deref().unwrap_or_default();
    if state.token.is_empty() || state.session_id.is_empty() || !valid_id(channel) {
        return Err(VoiceError::Invalid(
            "voice requires token, session_id, endpoint and channel_id".into(),
        ));
    }
    // Discord sends the voice host with an optional port, for example
    // "c-iad21-1a2b3c.discord.media:443". A port is valid; userinfo, a path, a
    // query and a fragment are not. The hostname rule below is what keeps
    // non-Discord hosts out.
    let endpoint = state.endpoint.trim();
    let Some(host) = voice_host(endpoint) else {
        return Err(VoiceError::Invalid(format!("invalid Discord voice endpoint {endpoint:?}")));
    };
    let host = host.to_ascii_lowercase();
    if !host.ends_with(".discord.media") && !host.ends_with(".discord.gg") {
        return Err(VoiceError::Invalid(format!(
            "voice endpoint {endpoint:?} must be a Discord voice hostname"
        )));
    }
    Ok(())
}

/// Returns the hostname of a voice endpoint, or `None` when the value is not a
/// bare `host[:port]`.
fn voice_host(endpoint: &str) -> Option<&str> {
    if endpoint.is_empty() || endpoint.len() > 253 + 6 {
        return None;
    }
    if endpoint.contains(['/', '?', '#', '@', ' ', '\t']) {
        return None;
    }
    let (host, port) = match endpoint.rsplit_once(':') {
        // A bare IPv6 literal is not a Discord hostname, and a host with a port
        // has to keep the port numeric.
        Some((host, port)) => {
            if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            (host, Some(port))
        }
        None => (endpoint, None),
    };
    if host.is_empty() {
        return None;
    }
    if let Some(port) = port {
        if port.parse::<u32>().map(|value| value > 65_535).unwrap_or(true) {
            return None;
        }
    }
    Some(host)
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
        assert!(!valid_id("0"));
        assert!(!valid_id(""));
        assert!(!valid_id("not a number"));
        assert!(!valid_id("12 34"));
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
    fn a_discord_host_with_a_path_is_refused() {
        assert!(validate(&state("c-iad21-1a2b3c.discord.media/../x", Some("123"))).is_err());
    }

    #[test]
    fn a_discord_host_with_userinfo_is_refused() {
        assert!(validate(&state("user@c-iad21-1a2b3c.discord.media", Some("123"))).is_err());
    }

    #[test]
    fn a_missing_channel_or_token_is_refused() {
        assert!(validate(&state("a.discord.media", None)).is_err());
        let mut missing = state("a.discord.media", Some("123"));
        missing.token.clear();
        assert!(validate(&missing).is_err());
    }

    #[test]
    fn a_voice_host_is_recognised_with_or_without_a_port() {
        assert_eq!(voice_host("a.discord.media"), Some("a.discord.media"));
        assert_eq!(voice_host("a.discord.media:443"), Some("a.discord.media"));
        assert_eq!(voice_host("a.discord.media:not-a-port"), None);
    }
}
