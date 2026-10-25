//! Delivering node events to an external endpoint.
//!
//! A Discord webhook URL receives an embed, which is what a Discord channel
//! expects; any other endpoint receives the event itself as JSON. Delivery runs
//! on one background thread so a slow endpoint never blocks the player that
//! produced the event, and a delivery that cannot be completed stops the node:
//! a configured endpoint is part of the deployment, not an optional extra.

use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustlink_config::Webhook as Settings;
use serde_json::{json, Map, Value};

/// How many events may wait for delivery. An endpoint that cannot keep up is a
/// failure like any other, so a full queue is fatal rather than a silent drop.
const QUEUE: usize = 128;

/// How long one delivery attempt may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Periodic node-wide report, rendered as a dedicated Discord embed.
pub const PLAYER_STATE_TYPE: &str = "node.players";

/// How many fields Discord renders in one embed.
const MAX_EMBED_FIELDS: usize = 25;
/// Discord also caps the combined text of all embeds in one message.
const MAX_EMBED_TEXT: usize = 6000;

/// Reports a delivery the node cannot recover from. The endpoint is
/// configuration, so a node that cannot reach it was told to talk to something
/// that is not listening.
pub type Fatal = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Posts events to a single URL. A disabled configuration yields no client at
/// all, so a caller never has to check whether one is configured.
pub struct Client {
    url: String,
    queue: SyncSender<Value>,
    fatal: Fatal,
}

impl Client {
    /// Builds a client for the configuration, or none when the webhook is off.
    pub fn new(settings: &Settings, fatal: Fatal) -> Option<Client> {
        let target = settings.url.trim().to_string();
        if !settings.enable || target.is_empty() {
            return None;
        }
        let discord = is_discord(&target);
        let (queue, inbox) = sync_channel(QUEUE);
        let worker = target.clone();
        let worker_fatal = Arc::clone(&fatal);
        thread::Builder::new()
            .name("webhook".into())
            .spawn(move || {
                while let Ok(event) = inbox.recv() {
                    if let Err(error) = post(&worker, discord, &event) {
                        worker_fatal("webhook delivery failed", &error);
                        return;
                    }
                }
            })
            .ok()?;
        Some(Client { url: target, queue, fatal })
    }

    /// The URL events are posted to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Queues an event for delivery. It returns immediately, so the caller is
    /// never held up by the endpoint.
    pub fn send(&self, event: Value) {
        match self.queue.try_send(event) {
            Ok(()) => {}
            // A full queue means the endpoint has not kept up with what the
            // node has already produced, which no later event can repair.
            Err(TrySendError::Full(_)) => {
                (self.fatal)("webhook delivery failed", "event queue is full")
            }
            Err(TrySendError::Disconnected(_)) => {
                (self.fatal)("webhook delivery failed", "delivery worker stopped")
            }
        }
    }
}

/// Delivers one event. A Discord endpoint gets an embed built from the event;
/// every other endpoint gets the event as it was published.
fn post(url: &str, discord: bool, event: &Value) -> Result<(), String> {
    let body = if discord { discord_payload(event) } else { event.clone() };
    let response = ureq::post(url)
        .header("Accept-Encoding", "identity")
        .header("Content-Type", "application/json")
        .header("User-Agent", "Rustlink")
        .config()
        .timeout_global(Some(SEND_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .send_json(&body)
        .map_err(|error| {
            let detail = error.to_string().replace(url, "<redacted webhook URL>");
            format!("webhook endpoint could not be reached: {detail}")
        })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("webhook answered {status}"));
    }
    Ok(())
}

/// Whether a URL points at a Discord webhook, which is served an embed instead
/// of the raw event.
fn is_discord(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "discord.com" || host == "discordapp.com" || host.ends_with(".discord.com") {
        return true;
    }
    url.path().contains("/api/webhooks/")
}

/// Embed colours, chosen so a track that started reads calm and a failure that
/// stopped playback reads as a fault.
const COLOR_START: u32 = 0x57f287;
const COLOR_END: u32 = 0x95a5a6;
const COLOR_FAULT: u32 = 0xed4245;
const COLOR_DEFAULT: u32 = 0x5865f2;
const COLOR_STATE: u32 = 0x5865f2;

/// Renders an event as the `{"embeds": [...]}` body a Discord webhook accepts.
/// The track is the headline, and the remaining fields carry what an operator
/// needs to tell one occurrence from another.
fn discord_payload(event: &Value) -> Value {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == PLAYER_STATE_TYPE {
        return player_state_payload(event);
    }
    let mut embed = Map::new();
    embed.insert("title".into(), json!(event_title(kind)));
    embed.insert("color".into(), json!(event_color(kind)));
    embed.insert("timestamp".into(), json!(timestamp()));
    embed.insert("footer".into(), json!({ "text": "Rustlink" }));
    let mut fields: Vec<Value> = Vec::new();
    let mut add = |name: &str, value: &Value| {
        let text = embed_value(value);
        if text.is_empty() {
            return;
        }
        fields.push(json!({ "name": name, "value": truncate(&text, 1024), "inline": true }));
    };
    match event.get("track").and_then(|track| track.get("metadata")) {
        Some(info) => {
            let title = embed_value(info.get("title").unwrap_or(&Value::Null));
            if !title.is_empty() {
                let mut description = format!("**{title}**");
                let author = embed_value(info.get("author").unwrap_or(&Value::Null));
                if !author.is_empty() {
                    description.push_str(" - ");
                    description.push_str(&author);
                }
                embed.insert("description".into(), json!(truncate(&description, 4096)));
            }
            add("Source", info.get("source").unwrap_or(&Value::Null));
            add("Guild", event.get("guild_id").unwrap_or(&Value::Null));
            add("URI", info.get("uri").unwrap_or(&Value::Null));
            if let Some(length) = number(event.get("duration_ms").unwrap_or(&Value::Null))
                .or_else(|| number(info.get("duration_ms").unwrap_or(&Value::Null)))
            {
                if length > 0.0 {
                    add("Duration", &json!(format_duration(length as i64)));
                }
            }
        }
        None => {
            // An event with no track is about the player itself: which guild it
            // serves and, for the voice events, which channel the bot was on.
            add("Guild", event.get("guild_id").unwrap_or(&Value::Null));
            add("Channel", event.get("channel_id").unwrap_or(&Value::Null));
        }
    }
    add("Reason", event.get("reason").unwrap_or(&Value::Null));
    add("Code", event.get("code").unwrap_or(&Value::Null));
    add("By remote", event.get("by_remote").unwrap_or(&Value::Null));
    if let Some(threshold) = number(event.get("threshold_ms").unwrap_or(&Value::Null)) {
        if threshold > 0.0 {
            add("Threshold", &json!(format!("{threshold:.0} ms")));
        }
    }
    if let Some(exception) = event.get("exception") {
        add("Exception", exception.get("message").unwrap_or(&Value::Null));
        add("Cause", exception.get("cause").unwrap_or(&Value::Null));
    }
    if !fields.is_empty() {
        embed.insert("fields".into(), Value::Array(fields));
    }
    bounded_payload(embed)
}

/// Renders the node-wide player report as an embed. The headline says how busy
/// the node is and each player becomes a field, so a channel shows what the node
/// is playing without opening a client.
fn player_state_payload(report: &Value) -> Value {
    let mut embed = Map::new();
    embed.insert("title".into(), json!("Player State"));
    embed.insert("color".into(), json!(COLOR_STATE));
    embed.insert("timestamp".into(), json!(timestamp()));
    embed.insert("footer".into(), json!({ "text": "Rustlink" }));
    let players = embed_value(report.get("players").unwrap_or(&Value::Null));
    let playing = embed_value(report.get("playing_players").unwrap_or(&Value::Null));
    let sessions = embed_value(report.get("sessions").unwrap_or(&Value::Null));
    embed.insert(
        "description".into(),
        json!(format!(
            "{} · {} playing · {}",
            plural(&players, "player"),
            playing,
            plural(&sessions, "session")
        )),
    );
    if let Some(states) = report.get("states").and_then(Value::as_array) {
        let fields = player_state_fields(states);
        if !fields.is_empty() {
            embed.insert("fields".into(), Value::Array(fields));
        }
    }
    bounded_payload(embed)
}

/// Per-field limits alone do not keep long track metadata or a busy player
/// report below Discord's message-wide budget. Retain the headline and then
/// include fields in their existing order while space remains.
fn bounded_payload(mut embed: Map<String, Value>) -> Value {
    let mut remaining = MAX_EMBED_TEXT;
    for (key, limit) in [("title", 256), ("description", 4096)] {
        if let Some(text) = embed.get(key).and_then(Value::as_str) {
            let text = truncate(text, limit.min(remaining));
            remaining -= discord_len(&text);
            embed.insert(key.into(), json!(text));
        }
    }
    remaining = remaining.saturating_sub(
        embed.get("footer").and_then(|footer| footer["text"].as_str()).map_or(0, discord_len),
    );
    if let Some(Value::Array(fields)) = embed.remove("fields") {
        let mut bounded = Vec::new();
        for mut field in fields.into_iter().take(MAX_EMBED_FIELDS) {
            let name = truncate(field["name"].as_str().unwrap_or(""), 256);
            let name_length = discord_len(&name);
            if remaining <= name_length {
                break;
            }
            let value =
                truncate(field["value"].as_str().unwrap_or(""), 1024.min(remaining - name_length));
            if name.is_empty() || value.is_empty() {
                continue;
            }
            remaining -= name_length + discord_len(&value);
            field["name"] = json!(name);
            field["value"] = json!(value);
            bounded.push(field);
        }
        if !bounded.is_empty() {
            embed.insert("fields".into(), Value::Array(bounded));
        }
    }
    json!({ "embeds": [Value::Object(embed)] })
}

/// Renders one field per player. Discord caps an embed at [`MAX_EMBED_FIELDS`],
/// so a busier node gets a final field counting the rest instead of a body
/// Discord would reject.
fn player_state_fields(states: &[Value]) -> Vec<Value> {
    let mut fields = Vec::new();
    for (index, state) in states.iter().enumerate() {
        if fields.len() >= MAX_EMBED_FIELDS - 1 {
            fields.push(json!({
                "name": "…",
                "value": format!("{} more players", states.len() - index),
                "inline": false,
            }));
            break;
        }
        fields.push(json!({
            "name": truncate(&player_state_name(state), 256),
            "value": truncate(&player_state_value(state), 1024),
            "inline": false,
        }));
    }
    fields
}

/// Heads a player field with the guild it serves and what it is playing, which
/// is what an operator scans a channel for.
fn player_state_name(state: &Value) -> String {
    let mut name = embed_value(state.get("guild_id").unwrap_or(&Value::Null));
    let title = embed_value(state.get("title").unwrap_or(&Value::Null));
    if !title.is_empty() {
        name.push_str(" · ");
        name.push_str(&title);
    }
    if name.trim().is_empty() {
        return "Player".into();
    }
    name
}

/// Describes one player: where its track is, whether it is playing, how loud it
/// is and how it reaches the voice channel. Who owns the player is deliberately
/// absent, because the report is readable by anyone the endpoint is shared with.
fn player_state_value(state: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if embed_value(state.get("title").unwrap_or(&Value::Null)).is_empty() {
        parts.push("idle".into());
    } else {
        let mut elapsed = String::new();
        if let Some(position) = number(state.get("position_ms").unwrap_or(&Value::Null)) {
            elapsed = format_duration(position as i64);
        }
        if let Some(length) = number(state.get("duration_ms").unwrap_or(&Value::Null)) {
            if length > 0.0 {
                elapsed.push('/');
                elapsed.push_str(&format_duration(length as i64));
            }
        }
        if !elapsed.is_empty() {
            parts.push(elapsed);
        }
        match state.get("paused").and_then(Value::as_bool) {
            Some(true) => parts.push("paused".into()),
            Some(false) => parts.push("playing".into()),
            None => {}
        }
    }
    let volume = embed_value(state.get("volume").unwrap_or(&Value::Null));
    if !volume.is_empty() {
        parts.push(format!("vol {volume}"));
    }
    if state.get("connected").and_then(Value::as_bool) == Some(false) {
        parts.push("disconnected".into());
    }
    if let Some(ping) = number(state.get("ping_ms").unwrap_or(&Value::Null)) {
        if ping >= 0.0 {
            parts.push(format!("ping {}", ping as i64));
        }
    }
    if parts.is_empty() {
        return "—".into();
    }
    parts.join(" · ")
}

/// Renders a count with its noun, keeping the singular when the count is one so
/// an embed does not read "1 players".
fn plural(count: &str, noun: &str) -> String {
    if count == "1" {
        return format!("{count} {noun}");
    }
    format!("{count} {noun}s")
}

/// Reads a value that JSON decoded as a number.
fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// Turns a Rustlink event name into the heading a Discord embed shows.
fn event_title(kind: &str) -> String {
    match kind {
        "track.started" => "Track Started".into(),
        "track.ended" => "Track Ended".into(),
        "track.failed" => "Track Exception".into(),
        "track.stalled" => "Track Stuck".into(),
        "voice.closed" => "Voice WebSocket Closed".into(),
        "voice.connected" => "Bot Joined Voice".into(),
        "voice.disconnected" => "Bot Left Voice".into(),
        "" => "Rustlink Event".into(),
        other => other.to_string(),
    }
}

/// Picks the embed accent for an event.
fn event_color(kind: &str) -> u32 {
    match kind {
        "track.started" | "voice.connected" => COLOR_START,
        "track.ended" | "voice.disconnected" => COLOR_END,
        "track.failed" | "track.stalled" => COLOR_FAULT,
        _ => COLOR_DEFAULT,
    }
}

/// Renders a value decoded from JSON as the string an embed field needs, and
/// returns the empty string for anything an embed cannot show.
fn embed_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.trim().to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.to_string(),
            None => number.to_string(),
        },
        other => other.to_string().trim().to_string(),
    }
}

/// Conservatively measures the same UTF-16 units JavaScript clients use. This
/// keeps astral characters such as emoji inside the documented field budgets
/// without ever cutting a Rust `char` or UTF-8 sequence in half.
fn discord_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Keeps a value inside a Discord text budget and reserves one unit for an
/// ellipsis when truncation is necessary.
fn truncate(text: &str, limit: usize) -> String {
    if discord_len(text) <= limit {
        return text.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    let content_limit = limit - 1;
    let mut used = 0;
    let mut out = String::new();
    for character in text.chars() {
        let width = character.len_utf16();
        if used + width > content_limit {
            break;
        }
        out.push(character);
        used += width;
    }
    out.push('…');
    out
}

/// Renders a track length as m:ss or h:mm:ss.
fn format_duration(millis: i64) -> String {
    let millis = millis.max(0);
    let total = millis / 1000 + i64::from(millis % 1000 >= 500);
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        return format!("{hours}:{minutes:02}:{seconds:02}");
    }
    format!("{minutes}:{seconds:02}")
}

/// The timestamp an embed carries, in the form Discord parses.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = now.div_euclid(86_400);
    let seconds = now.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// Converts a count of days since the epoch into a calendar date, which is what
/// the embed timestamp needs and what the standard library does not provide.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn failed_delivery_does_not_put_the_webhook_token_in_logs() {
        // An invalid authority fails before any network request is attempted.
        let target = "http://bad host/api/webhooks/123/private-webhook-token?key=private-key";
        let error = post(target, false, &json!({ "type": "test" })).unwrap_err();
        assert!(error.contains("could not be reached"));
        assert!(!error.contains("private-webhook-token"));
        assert!(!error.contains("private-key"));
        assert!(!error.contains(target));
    }

    #[test]
    fn a_discord_url_is_recognised_by_host_or_path() {
        assert!(is_discord("https://discord.com/api/webhooks/1/abc"));
        assert!(is_discord("https://canary.discord.com/api/webhooks/1/abc"));
        assert!(is_discord("https://example.com/api/webhooks/1/abc"));
        assert!(!is_discord("https://example.com/hooks/1/abc"));
    }

    #[test]
    fn a_track_event_becomes_an_embed_with_its_track_as_the_headline() {
        let event = json!({
            "op": "event",
            "type": "track.started",
            "guild_id": "1",
            "track": { "metadata": { "title": "A Song", "author": "Someone", "duration_ms": 200000, "source": "youtube" } },
        });
        let payload = discord_payload(&event);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Track Started");
        assert_eq!(embed["color"], COLOR_START);
        assert_eq!(embed["description"], "**A Song** - Someone");
        let fields = embed["fields"].as_array().unwrap();
        assert!(fields.iter().any(|field| field["name"] == "Duration" && field["value"] == "3:20"));
        assert!(fields
            .iter()
            .any(|field| field["name"] == "Source" && field["value"] == "youtube"));
    }

    #[test]
    fn a_voice_event_names_the_channel_and_the_reason() {
        let event = json!({
            "type": "voice.disconnected",
            "guild_id": "1",
            "channel_id": "2",
            "reason": "destroyed",
        });
        let payload = discord_payload(&event);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Bot Left Voice");
        let fields = embed["fields"].as_array().unwrap();
        assert!(fields.iter().any(|field| field["name"] == "Channel" && field["value"] == "2"));
        assert!(fields
            .iter()
            .any(|field| field["name"] == "Reason" && field["value"] == "destroyed"));
    }

    #[test]
    fn the_player_report_names_each_player_by_guild_and_track() {
        let report = json!({
            "type": PLAYER_STATE_TYPE,
            "players": 2,
            "playing_players": 1,
            "sessions": 1,
            "states": [
                { "guild_id": "1", "title": "A Song", "position_ms": 30000, "duration_ms": 200000, "paused": false, "volume": 100, "connected": true, "ping_ms": 42 },
                { "guild_id": "2", "title": "", "volume": 50, "connected": false },
            ],
        });
        let payload = discord_payload(&report);
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "Player State");
        assert_eq!(embed["description"], "2 players · 1 playing · 1 session");
        let fields = embed["fields"].as_array().unwrap();
        assert_eq!(fields[0]["name"], "1 · A Song");
        assert_eq!(fields[0]["value"], "0:30/3:20 · playing · vol 100 · ping 42");
        assert_eq!(fields[1]["value"], "idle · vol 50 · disconnected");
    }

    #[test]
    fn a_value_longer_than_an_embed_allows_is_cut_on_a_character_boundary() {
        let long = "曲".repeat(300);
        let cut = truncate(&long, 256);
        assert_eq!(cut.chars().count(), 256);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn emoji_are_bounded_by_utf16_units_without_splitting_utf8() {
        let cut = truncate(&"😀".repeat(300), 256);
        assert!(discord_len(&cut) <= 256);
        assert!(cut.ends_with('…'));
        assert!(cut[..cut.len() - '…'.len_utf8()].chars().all(|character| character == '😀'));
    }

    #[test]
    fn a_length_is_rendered_as_minutes_and_seconds() {
        assert_eq!(format_duration(200_000), "3:20");
        assert_eq!(format_duration(3_600_000), "1:00:00");
        assert_eq!(format_duration(500), "0:01");
    }

    fn assert_discord_embed_limits(payload: &Value) {
        let mut total = 0;
        for embed in payload["embeds"].as_array().unwrap() {
            for (value, limit) in [
                (&embed["title"], 256),
                (&embed["description"], 4096),
                (&embed["footer"]["text"], 2048),
                (&embed["author"]["name"], 256),
            ] {
                if let Some(text) = value.as_str() {
                    assert!(discord_len(text) <= limit);
                    total += discord_len(text);
                }
            }
            if let Some(fields) = embed["fields"].as_array() {
                assert!(fields.len() <= 25);
                for field in fields {
                    for (key, limit) in [("name", 256), ("value", 1024)] {
                        let text = field[key].as_str().unwrap();
                        assert!(!text.is_empty());
                        assert!(discord_len(text) <= limit);
                        total += discord_len(text);
                    }
                }
            }
        }
        assert!(total <= 6000, "Discord rejects {total} embed characters");
    }

    #[test]
    fn long_track_metadata_stays_within_discord_embed_total() {
        let payload = discord_payload(&json!({
            "type": "track.failed", "guild_id": "1",
            "track": {"metadata": {
                "title": "長曲名".repeat(2000), "author": "歌手".repeat(1000),
                "source": "youtube", "uri": format!("https://example.com/{}", "x".repeat(3000)),
            }},
            "reason": "失敗".repeat(1000),
            "exception": {"message": "原因".repeat(1000), "cause": "rustlink.audio"},
        }));
        assert_discord_embed_limits(&payload);
        assert_eq!(payload["embeds"][0]["title"], "Track Exception");
        assert!(payload["embeds"][0]["description"].as_str().unwrap().starts_with("**長曲名"));
    }

    #[test]
    fn many_players_stay_within_discord_embed_total() {
        let states: Vec<Value> = (0..50)
            .map(|index| {
                json!({
                    "guild_id": index.to_string(), "title": "曲".repeat(400),
                    "position_ms": 30000, "duration_ms": 200000,
                    "paused": false, "volume": 100, "connected": true, "ping_ms": 42,
                })
            })
            .collect();
        let payload = discord_payload(&json!({
            "type": PLAYER_STATE_TYPE, "players": 50,
            "playing_players": 50, "sessions": 1, "states": states,
        }));
        assert_discord_embed_limits(&payload);
        assert_eq!(payload["embeds"][0]["description"], "50 players · 50 playing · 1 session");
    }

    #[test]
    fn emoji_heavy_payloads_stay_inside_the_conservative_total_budget() {
        let payload = discord_payload(&json!({
            "type": "track.failed", "guild_id": "1",
            "track": {"metadata": {
                "title": "😀".repeat(3000), "author": "🎵".repeat(1500),
                "uri": format!("https://example.com/{}", "🚀".repeat(1500)),
            }},
            "reason": "💥".repeat(1000),
            "exception": {"message": "⚠️".repeat(1000)},
        }));
        assert_discord_embed_limits(&payload);
    }

    #[test]
    fn queue_backpressure_and_a_stopped_worker_use_the_fatal_contract() {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&failures);
        let fatal: Fatal = Arc::new(move |message, error| {
            recorded.lock().unwrap().push(format!("{message}: {error}"));
        });

        let (queue, receiver) = sync_channel(0);
        let full =
            Client { url: "https://example.invalid".into(), queue, fatal: Arc::clone(&fatal) };
        full.send(json!({"type":"test"}));
        drop(receiver);

        let (queue, receiver) = sync_channel(1);
        drop(receiver);
        let stopped = Client { url: "https://example.invalid".into(), queue, fatal };
        stopped.send(json!({"type":"test"}));

        let failures = failures.lock().unwrap();
        assert!(failures.iter().any(|failure| failure.contains("event queue is full")));
        assert!(failures.iter().any(|failure| failure.contains("delivery worker stopped")));
    }

    #[test]
    fn extreme_duration_still_formats_as_a_nonnegative_time() {
        assert_eq!(format_duration(i64::MAX), "2562047788015:12:56");
    }
}
