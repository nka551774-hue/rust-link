//! Pair Discord's two out-of-order voice events.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rustlink_protocol::VoiceState;
use zeroize::Zeroizing;

use crate::{validate_snowflake, validate_text, BotError, BotEvent};

const LATE_SERVER_WINDOW: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Pending {
    session_id: Option<String>,
    channel_id: Option<String>,
    token: Option<Zeroizing<String>>,
    endpoint: Option<String>,
    emitted: bool,
    late_server_at: Option<Instant>,
}

/// Correlates `VOICE_STATE_UPDATE` and `VOICE_SERVER_UPDATE` for one bot user.
pub struct VoiceStatePairer {
    bot_user_id: String,
    guild_filter: Option<String>,
    pending: HashMap<String, Pending>,
}

impl VoiceStatePairer {
    pub fn new(bot_user_id: impl Into<String>) -> Result<Self, BotError> {
        let raw = bot_user_id.into();
        Ok(Self {
            bot_user_id: validate_snowflake(&raw, "bot user ID")?.to_owned(),
            guild_filter: None,
            pending: HashMap::new(),
        })
    }

    /// Creates a pairer restricted to one guild.
    pub fn for_guild(
        bot_user_id: impl Into<String>,
        guild_id: impl Into<String>,
    ) -> Result<Self, BotError> {
        let mut pairer = Self::new(bot_user_id)?;
        let raw_guild = guild_id.into();
        pairer.guild_filter = Some(validate_snowflake(&raw_guild, "guild ID")?.to_owned());
        Ok(pairer)
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    fn accept_guild<'a>(&self, raw: &'a str) -> Result<Option<&'a str>, BotError> {
        let guild_id = validate_snowflake(raw, "guild ID")?;
        Ok(self.guild_filter.as_deref().is_none_or(|g| g == guild_id).then_some(guild_id))
    }

    fn pending_mut(&mut self, guild_id: &str) -> &mut Pending {
        if !self.pending.contains_key(guild_id) {
            self.pending.insert(guild_id.to_owned(), Pending::default());
        }
        self.pending.get_mut(guild_id).expect("inserted")
    }

    /// Returns a new complete voice state once, regardless of event order.
    pub fn observe(&mut self, event: &BotEvent) -> Result<Option<VoiceState>, BotError> {
        match event {
            BotEvent::VoiceStateUpdate(update) => {
                if update.user_id != self.bot_user_id {
                    return Ok(None);
                }
                let Some(raw_guild) = update.guild_id.as_deref() else { return Ok(None) };
                let Some(guild_id) = self.accept_guild(raw_guild)? else { return Ok(None) };
                if update.session_id.trim().is_empty() {
                    return Err(BotError::Protocol("voice state has no session ID".into()));
                }
                if update.channel_id.is_none() {
                    self.pending.remove(guild_id);
                    return Ok(None);
                }
                let pending = self.pending_mut(guild_id);
                let state_changed = pending.session_id.as_deref() != Some(&update.session_id)
                    || pending.channel_id != update.channel_id;
                if (pending.session_id.is_some() || pending.channel_id.is_some()) && state_changed {
                    let fresh_server = pending
                        .late_server_at
                        .take()
                        .is_some_and(|at| at.elapsed() <= LATE_SERVER_WINDOW);
                    if !fresh_server {
                        pending.token = None;
                        pending.endpoint = None;
                    }
                    pending.emitted = false;
                }
                pending.session_id = Some(update.session_id.clone());
                pending.channel_id = update.channel_id.clone();
                self.ready(guild_id)
            }
            BotEvent::VoiceServerUpdate(update) => {
                let Some(guild_id) = self.accept_guild(&update.guild_id)? else {
                    return Ok(None);
                };
                let endpoint = update
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let pending = self.pending_mut(guild_id);
                let reallocating = endpoint.is_none() || pending.endpoint.is_none();
                let server_changed = pending.token.is_some()
                    && (pending.token.as_deref().map(String::as_str) != Some(&update.token)
                        || pending.endpoint != endpoint);
                if endpoint.is_some() && pending.session_id.is_some() && server_changed {
                    if pending.emitted && !reallocating {
                        pending.emitted = false;
                    }
                    pending.late_server_at = Some(Instant::now());
                }
                if endpoint.is_none() {
                    pending.emitted = false;
                }
                pending.token = Some(Zeroizing::new(update.token.clone()));
                pending.endpoint = endpoint;
                self.ready(guild_id)
            }
            BotEvent::Ready(ready) if ready.user_id == self.bot_user_id => {
                self.pending.clear();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn clear(&mut self, guild_id: &str) -> Result<(), BotError> {
        self.pending.remove(validate_snowflake(guild_id, "guild ID")?);
        Ok(())
    }

    fn ready(&mut self, guild_id: &str) -> Result<Option<VoiceState>, BotError> {
        let pending = self
            .pending
            .get_mut(guild_id)
            .ok_or_else(|| BotError::Protocol("voice pairing state disappeared".into()))?;
        if pending.emitted {
            return Ok(None);
        }
        let (Some(session_id), Some(channel_id), Some(token), Some(endpoint)) = (
            pending.session_id.as_deref(),
            pending.channel_id.as_deref(),
            pending.token.as_deref(),
            pending.endpoint.as_deref(),
        ) else {
            return Ok(None);
        };
        let out = VoiceState {
            token: validate_text(token, "voice token", 512)?.to_owned(),
            endpoint: validate_text(endpoint, "voice endpoint", 512)?.to_owned(),
            session_id: validate_text(session_id, "voice session ID", 256)?.to_owned(),
            channel_id: Some(validate_snowflake(channel_id, "channel ID")?.to_owned()),
        };
        pending.emitted = true;
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{VoiceServerUpdate, VoiceStateUpdate};

    fn vsu(guild: Option<&str>, user: &str, ch: Option<&str>, session: &str) -> BotEvent {
        BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: guild.map(Into::into),
            user_id: user.into(),
            channel_id: ch.map(Into::into),
            session_id: session.into(),
        })
    }

    fn vserv(guild: &str, token: &str, endpoint: Option<&str>) -> BotEvent {
        BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: guild.into(),
            token: token.into(),
            endpoint: endpoint.map(Into::into),
        })
    }

    fn state() -> BotEvent {
        vsu(Some("200"), "100", Some("300"), "session")
    }

    fn server() -> BotEvent {
        vserv("200", "token", Some("voice.discord.media:443"))
    }

    #[test]
    fn events_pair_in_either_order_and_only_emit_once() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&server()).unwrap().is_none());
        assert_eq!(pairer.observe(&state()).unwrap().unwrap().channel_id.as_deref(), Some("300"));
        assert!(pairer.observe(&state()).unwrap().is_none());
    }

    #[test]
    fn unrelated_users_and_guildless_updates_are_ignored() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer
            .observe(&vsu(Some("200"), "999", Some("300"), "session"))
            .unwrap()
            .is_none());
        assert!(pairer.observe(&vsu(None, "100", Some("300"), "session")).unwrap().is_none());
    }

    #[test]
    fn leaving_a_channel_clears_stale_pairing() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&server()).unwrap().is_none());
        assert!(pairer.observe(&state()).unwrap().is_some());
        assert!(pairer.observe(&vsu(Some("200"), "100", None, "session-2")).unwrap().is_none());
        assert!(pairer.observe(&state()).unwrap().is_none());
    }

    #[test]
    fn a_new_session_never_reuses_the_previous_server_token() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        assert!(pairer
            .observe(&vsu(Some("200"), "100", Some("301"), "session-2"))
            .unwrap()
            .is_none());
        let voice = pairer
            .observe(&vserv("200", "fresh-token", Some("voice2.discord.media:443")))
            .unwrap()
            .unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("session-2", "fresh-token"));
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
    }

    #[test]
    fn a_guild_bound_pairer_ignores_other_guild_voice_events() {
        let mut pairer = VoiceStatePairer::for_guild("100", "200").unwrap();
        assert!(pairer.observe(&vsu(Some("201"), "100", Some("301"), "other")).unwrap().is_none());
        assert!(pairer
            .observe(&vserv("201", "other", Some("voice.discord.media:443")))
            .unwrap()
            .is_none());
        assert!(pairer.pending.is_empty());
    }

    #[test]
    fn a_server_migration_keeps_the_voice_session_and_channel() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        let migrated = vserv("200", "new-token", Some("voice2.discord.media:443"));
        let voice = pairer.observe(&migrated).unwrap().unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("session", "new-token"));
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
        assert!(pairer.observe(&migrated).unwrap().is_none());
    }

    #[test]
    fn a_state_that_follows_a_new_server_pairs_with_it_not_with_the_old_one() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        assert!(pairer
            .observe(&vserv("200", "new-token", Some("voice2.discord.media:443")))
            .unwrap()
            .is_some());

        let voice =
            pairer.observe(&vsu(Some("200"), "100", Some("301"), "new-session")).unwrap().unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("new-session", "new-token"));
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
        assert!(pairer
            .observe(&vsu(Some("200"), "100", Some("302"), "newer-session"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_server_failover_reuses_the_current_voice_state() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        assert!(pairer.observe(&vserv("200", "token", None)).unwrap().is_none());

        let voice = pairer
            .observe(&vserv("200", "new-token", Some("voice2.discord.media:443")))
            .unwrap()
            .unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("session", "new-token"));
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
    }

    #[test]
    fn a_server_reallocated_with_the_same_fields_is_emitted_after_unavailability() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        assert!(pairer.observe(&vserv("200", "token", None)).unwrap().is_none());

        let voice = pairer.observe(&server()).unwrap().unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("session", "token"));
        assert_eq!(voice.endpoint, "voice.discord.media:443");
    }

    #[test]
    fn empty_or_whitespace_endpoint_is_treated_as_unavailable_and_recovers() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&vserv("200", "token", Some("   "))).unwrap().is_none());
        assert_eq!(pairer.observe(&server()).unwrap().unwrap().endpoint, "voice.discord.media:443");
    }

    #[test]
    fn rapid_move_none_to_some_server_first_transition_pairs_with_following_state() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        assert!(pairer.observe(&vserv("200", "move-token", Some(""))).unwrap().is_none());
        assert!(pairer
            .observe(&vserv("200", "move-token", Some("voice2.discord.media:443")))
            .unwrap()
            .is_some());

        let voice =
            pairer.observe(&vsu(Some("200"), "100", Some("301"), "session-2")).unwrap().unwrap();
        assert_eq!((voice.session_id.as_str(), voice.token.as_str()), ("session-2", "move-token"));
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
    }
}
