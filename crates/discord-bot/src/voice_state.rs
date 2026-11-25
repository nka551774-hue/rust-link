//! Pair Discord's two out-of-order voice events.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rustlink_protocol::VoiceState;

use crate::{BotError, BotEvent};

const LATE_SERVER_WINDOW: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Pending {
    session_id: Option<String>,
    channel_id: Option<String>,
    token: Option<zeroize::Zeroizing<String>>,
    endpoint: Option<String>,
    emitted: bool,
    /// When a newer server arrived after a pairing was emitted. A state change
    /// arriving shortly afterwards pairs with that server rather than clearing
    /// it, while an old server migration never leaks into a much later move.
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
        let bot_user_id = crate::validate_snowflake(&raw, "bot user ID")?.to_owned();
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
        let raw_user = bot_user_id.into();
        let raw_guild = guild_id.into();
        let bot_user_id = crate::validate_snowflake(&raw_user, "bot user ID")?.to_owned();
        let guild_filter = crate::validate_snowflake(&raw_guild, "guild ID")?.to_owned();
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
                if update.channel_id.is_none() {
                    self.pending.remove(guild_id);
                    return Ok(None);
                }
                let pending = match self.pending.get_mut(guild_id) {
                    Some(existing) => existing,
                    None => self.pending.entry(guild_id.to_owned()).or_default(),
                };
                // Discord may send a new state before the matching server
                // event. Never combine that new session/channel with the
                // previous voice token or endpoint.
                let state_changed = pending.session_id.as_deref()
                    != Some(update.session_id.as_str())
                    || pending.channel_id.as_ref() != update.channel_id.as_ref();
                if (pending.session_id.is_some() || pending.channel_id.is_some()) && state_changed {
                    // The token on hand belongs to the previous channel, unless
                    // a newer server arrived moments ago and is the one this
                    // state was waiting for.
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
                let guild_id = crate::validate_snowflake(&update.guild_id, "guild ID")?;
                if self.guild_filter.as_deref().is_some_and(|expected| expected != guild_id) {
                    return Ok(None);
                }
                let endpoint = update
                    .endpoint
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                let pending = match self.pending.get_mut(guild_id) {
                    Some(existing) => existing,
                    None => self.pending.entry(guild_id.to_owned()).or_default(),
                };
                // Discord reports an unavailable server with a null or empty
                // endpoint while it reallocates one. Preserve the current voice
                // state across that unavailable event and the first usable
                // endpoint after it, because failover does not guarantee another
                // state event.
                let reallocating = endpoint.is_none() || pending.endpoint.is_none();
                let server_changed = pending.token.is_some()
                    && (pending.token.as_deref().map(String::as_str)
                        != Some(update.token.as_str())
                        || pending.endpoint.as_deref() != endpoint.as_deref());
                if endpoint.is_some() && pending.session_id.is_some() && server_changed {
                    if pending.emitted && !reallocating {
                        pending.emitted = false;
                    }
                    pending.late_server_at = Some(Instant::now());
                }
                if endpoint.is_none() {
                    // An unavailable server invalidates the active connection,
                    // even when its replacement reuses the exact same fields.
                    pending.emitted = false;
                }
                pending.token = Some(zeroize::Zeroizing::new(update.token.clone()));
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
        let guild_id = crate::validate_snowflake(guild_id, "guild ID")?;
        self.pending.remove(guild_id);
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
        let session_id = crate::validate_text(session_id, "voice session ID")?;
        if session_id.len() > 256 {
            return Err(BotError::Invalid("voice session ID is too long".into()));
        }
        let channel_id = crate::validate_snowflake(channel_id, "channel ID")?;
        let token = crate::validate_text(token, "voice token")?;
        if token.len() > 512 {
            return Err(BotError::Invalid("voice token is too long".into()));
        }
        let endpoint = crate::validate_text(endpoint, "voice endpoint")?;
        if endpoint.len() > 512 {
            return Err(BotError::Invalid("voice endpoint is too long".into()));
        }
        let out = VoiceState {
            token: token.to_owned(),
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            channel_id: Some(channel_id.to_owned()),
        };
        pending.emitted = true;
        Ok(Some(out))
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
    fn a_server_migration_keeps_the_voice_session_and_channel() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        // Changing the region sends a new server and no state update at all.
        let migrated = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "new-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        let voice = pairer.observe(&migrated).unwrap().expect("the connection is rebuilt");
        assert_eq!(voice.session_id, "session");
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert_eq!(voice.token, "new-token");
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
        assert!(pairer.observe(&migrated).unwrap().is_none(), "a repeat is not a second rebuild");
    }

    #[test]
    fn a_state_that_follows_a_new_server_pairs_with_it_not_with_the_old_one() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        // A move can send the server first and the state second.
        let refreshed = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "new-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        assert!(pairer.observe(&refreshed).unwrap().is_some());

        let refreshed_state = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("301".into()),
            session_id: "new-session".into(),
        });
        let voice = pairer.observe(&refreshed_state).unwrap().unwrap();
        assert_eq!(voice.session_id, "new-session");
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.token, "new-token", "the server that arrived first is the one kept");
        assert_eq!(voice.endpoint, "voice2.discord.media:443");

        // A later move whose server has not arrived yet must not borrow that token.
        let moved_again = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("302".into()),
            session_id: "newer-session".into(),
        });
        assert!(pairer.observe(&moved_again).unwrap().is_none());
    }

    #[test]
    fn a_server_failover_reuses_the_current_voice_state() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        let unavailable = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "token".into(),
            endpoint: None,
        });
        assert!(pairer.observe(&unavailable).unwrap().is_none());

        let reallocated = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "new-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        let voice = pairer.observe(&reallocated).unwrap().unwrap();
        assert_eq!(voice.session_id, "session");
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert_eq!(voice.token, "new-token");
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
    }

    #[test]
    fn a_server_reallocated_with_the_same_fields_is_emitted_after_unavailability() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        let unavailable = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "token".into(),
            endpoint: None,
        });
        assert!(pairer.observe(&unavailable).unwrap().is_none());

        let voice = pairer.observe(&server()).unwrap().unwrap();
        assert_eq!(voice.session_id, "session");
        assert_eq!(voice.channel_id.as_deref(), Some("300"));
        assert_eq!(voice.token, "token");
        assert_eq!(voice.endpoint, "voice.discord.media:443");
    }

    #[test]
    fn empty_or_whitespace_endpoint_is_treated_as_unavailable_and_recovers() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        let empty = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "token".into(),
            endpoint: Some("   ".into()),
        });
        assert!(pairer.observe(&empty).unwrap().is_none());
        let voice = pairer.observe(&server()).unwrap().unwrap();
        assert_eq!(voice.endpoint, "voice.discord.media:443");
    }

    #[test]
    fn rapid_move_none_to_some_server_first_transition_pairs_with_following_state() {
        let mut pairer = VoiceStatePairer::new("100").unwrap();
        assert!(pairer.observe(&state()).unwrap().is_none());
        assert!(pairer.observe(&server()).unwrap().is_some());

        // Discord sends an empty/null endpoint with the new token first, then
        // resolves the usable endpoint with that same token, and only then
        // delivers the moved channel's VOICE_STATE_UPDATE.
        let reallocating = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "move-token".into(),
            endpoint: Some(String::new()),
        });
        assert!(pairer.observe(&reallocating).unwrap().is_none());

        let ready_server = BotEvent::VoiceServerUpdate(VoiceServerUpdate {
            guild_id: "200".into(),
            token: "move-token".into(),
            endpoint: Some("voice2.discord.media:443".into()),
        });
        assert!(pairer.observe(&ready_server).unwrap().is_some());

        let moved_state = BotEvent::VoiceStateUpdate(VoiceStateUpdate {
            guild_id: Some("200".into()),
            user_id: "100".into(),
            channel_id: Some("301".into()),
            session_id: "session-2".into(),
        });
        let voice = pairer.observe(&moved_state).unwrap().unwrap();
        assert_eq!(voice.session_id, "session-2");
        assert_eq!(voice.channel_id.as_deref(), Some("301"));
        assert_eq!(voice.token, "move-token");
        assert_eq!(voice.endpoint, "voice2.discord.media:443");
    }
}
