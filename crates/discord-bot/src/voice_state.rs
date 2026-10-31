//! Pair Discord's two out-of-order voice events.

use std::collections::HashMap;

use rustlink_protocol::VoiceState;

use crate::{BotError, BotEvent};

#[derive(Default)]
struct Pending {
    session_id: Option<String>,
    channel_id: Option<String>,
    token: Option<String>,
    endpoint: Option<String>,
    emitted: Option<(String, String, String, String)>,
}

/// Correlates `VOICE_STATE_UPDATE` and `VOICE_SERVER_UPDATE` for one bot user.
pub struct VoiceStatePairer {
    bot_user_id: String,
    guild_filter: Option<String>,
    pending: HashMap<String, Pending>,
}

impl VoiceStatePairer {
    pub fn new(bot_user_id: impl Into<String>) -> Result<Self, BotError> {
        let bot_user_id = crate::validate_snowflake(&bot_user_id.into(), "bot user ID")?;
        Ok(Self { bot_user_id, guild_filter: None, pending: HashMap::new() })
    }

    /// Creates a pairer that can only produce voice state for one guild.
    ///
    /// A bot may receive voice events for every guild it is connected to. A
    /// bound pairer makes it impossible for an unrelated guild's token and
    /// endpoint to be applied to a fixed player by accident.
    pub fn for_guild(
        bot_user_id: impl Into<String>,
        guild_id: impl Into<String>,
    ) -> Result<Self, BotError> {
        let bot_user_id = crate::validate_snowflake(&bot_user_id.into(), "bot user ID")?;
        let guild_filter = crate::validate_snowflake(&guild_id.into(), "guild ID")?;
        Ok(Self { bot_user_id, guild_filter: Some(guild_filter), pending: HashMap::new() })
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Returns a new complete voice state once, regardless of event order.
    pub fn observe(&mut self, event: &BotEvent) -> Result<Option<VoiceState>, BotError> {
        match event {
            BotEvent::VoiceStateUpdate(update) => {
                if update.user_id != self.bot_user_id {
                    return Ok(None);
                }
                let Some(guild_id) = update.guild_id.as_deref() else {
                    return Ok(None);
                };
                let guild_id = crate::validate_snowflake(guild_id, "guild ID")?;
                if self.guild_filter.as_deref().is_some_and(|expected| expected != guild_id) {
                    return Ok(None);
                }
                if update.session_id.trim().is_empty() {
                    return Err(BotError::Protocol("voice state has no session ID".into()));
                }
                let pending = self.pending.entry(guild_id.clone()).or_default();
                // Discord may send a new state before the matching server
                // event. Never combine that new session/channel with the
                // previous voice token or endpoint.
                let state_changed = pending.session_id.as_deref()
                    != Some(update.session_id.as_str())
                    || pending.channel_id.as_ref() != update.channel_id.as_ref();
                if (pending.session_id.is_some() || pending.channel_id.is_some()) && state_changed {
                    pending.token = None;
                    pending.endpoint = None;
                    pending.emitted = None;
                }
                pending.session_id = Some(update.session_id.clone());
                pending.channel_id = update.channel_id.clone();
                if pending.channel_id.is_none() {
                    self.pending.remove(&guild_id);
                    return Ok(None);
                }
                self.ready(&guild_id)
            }
            BotEvent::VoiceServerUpdate(update) => {
                let guild_id = crate::validate_snowflake(&update.guild_id, "guild ID")?;
                if self.guild_filter.as_deref().is_some_and(|expected| expected != guild_id) {
                    return Ok(None);
                }
                let pending = self.pending.entry(guild_id.clone()).or_default();
                // A server update can arrive before the matching state update
                // when Discord reallocates voice. Never combine the new
                // server token with the old session/channel pair. Identical
                // duplicate server events are harmless and remain deduped.
                let changed_server = pending.emitted.is_some()
                    && (pending.token.as_deref() != Some(update.token.as_str())
                        || pending.endpoint.as_ref() != update.endpoint.as_ref());
                if changed_server {
                    pending.session_id = None;
                    pending.channel_id = None;
                    pending.emitted = None;
                }
                pending.token = Some(update.token.clone());
                pending.endpoint = update.endpoint.clone();
                if changed_server {
                    return Ok(None);
                }
                self.ready(&guild_id)
            }
            _ => Ok(None),
        }
    }

    pub fn clear(&mut self, guild_id: &str) -> Result<(), BotError> {
        let guild_id = crate::validate_snowflake(guild_id, "guild ID")?;
        self.pending.remove(&guild_id);
        Ok(())
    }

    fn ready(&mut self, guild_id: &str) -> Result<Option<VoiceState>, BotError> {
        let pending = self
            .pending
            .get_mut(guild_id)
            .ok_or_else(|| BotError::Protocol("voice pairing state disappeared".into()))?;
        let (Some(session_id), Some(channel_id), Some(token), Some(endpoint)) = (
            pending.session_id.clone(),
            pending.channel_id.clone(),
            pending.token.clone(),
            pending.endpoint.clone(),
        ) else {
            return Ok(None);
        };
        let session_id = crate::validate_text(&session_id, "voice session ID")?;
        if session_id.len() > 256 {
            return Err(BotError::Invalid("voice session ID is too long".into()));
        }
        let channel_id = crate::validate_snowflake(&channel_id, "channel ID")?;
        let token = crate::validate_text(&token, "voice token")?;
        if token.len() > 512 {
            return Err(BotError::Invalid("voice token is too long".into()));
        }
        let endpoint = crate::validate_text(&endpoint, "voice endpoint")?;
        if endpoint.len() > 512 {
            return Err(BotError::Invalid("voice endpoint is too long".into()));
        }
        let signature = (session_id.clone(), channel_id.clone(), token.clone(), endpoint.clone());
        if pending.emitted.as_ref() == Some(&signature) {
            return Ok(None);
        }
        pending.emitted = Some(signature);
        Ok(Some(VoiceState { token, endpoint, session_id, channel_id: Some(channel_id) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{VoiceServerUpdate, VoiceStateUpdate};

    fn state() -> BotEvent {
        BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("300".into()),
            session_id: "session".into(),
        })
    }

    fn server() -> BotEvent {
        BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "token".into(),
            endpoint: Some("voice.discord.media:443".into()),
        })
    }

    #[test]
    fn events_pair_in_either_order_and_only_emit_once() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&server()).unwrap().is_none());
        let voice = pairer.observe(&state()).unwrap().unwrap();
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert!(pairer.observe(&state()).unwrap().is_none());
    }

    #[test]
    fn unrelated_users_and_guildless_updates_are_ignored() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        let other = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "999".into(),
            channel_id: Some("300".into()),
            session_id: "session".into(),
        });
        assert!(pairer.observe(&other).unwrap().is_none());
        let guildless = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: None,
            user_id: "100".into(),
            channel_id: Some("300".into()),
            session_id: "session".into(),
        });
        assert!(pairer.observe(&guildless).unwrap().is_none());
    }

    #[test]
    fn leaving_a_channel_clears_stale_pairing() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&server()).unwrap().is_none());
        assert!(pairer.observe(&state()).unwrap().is_some());
        let leave = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: None,
            session_id: "session-2".into(),
        });
        assert!(pairer.observe(&leave).unwrap().is_none());
        assert!(pairer.observe(&state()).unwrap().is_none());
    }

    #[test]
    fn a_new_session_never_reuses_the_previous_server_token() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());
        let moved = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("301".into()),
            session_id: "session-2".into(),
        });
        assert!(pairer.observe(&moved).unwrap().is_none());
        let fresh_server = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "fresh-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        let voice = pairer.observe(&fresh_server).unwrap().unwrap();
        assert_eq!(voice.session_id, "session-2");
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.token, "fresh-token");
    }

    #[test]
    fn a_guild_bound_pairer_ignores_other_guild_voice_events() {
        let mut pairer = VoiceStatePairer::for_guild("100", "200").unwrap();
        let other_state = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("201".into()),
            user_id: "100".into(),
            channel_id: Some("301".into()),
            session_id: "other-session".into(),
        });
        let other_server = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "201".into(),
            token: "other-token".into(),
            endpoint: Some("voice.other.discord.media:443".into()),
        });
        assert!(pairer.observe(&other_state).unwrap().is_none());
        assert!(pairer.observe(&other_server).unwrap().is_none());
        assert!(pairer.pending.is_empty());
    }

    #[test]
    fn a_new_server_event_never_pairs_with_the_previous_voice_session() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        let refreshed = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "new-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        assert!(pairer.observe(&refreshed).unwrap().is_none());

        let refreshed_state = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("301".into()),
            session_id: "new-session".into(),
        });
        let voice = pairer.observe(&refreshed_state).unwrap().unwrap();
        assert_eq!(voice.session_id, "new-session");
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.token, "new-token");
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
    }
}
