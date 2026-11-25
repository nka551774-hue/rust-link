//! Delivering node events to an external endpoint.
//!
//! A Discord webhook URL receives an embed, which is what a Discord channel
//! expects; any other endpoint receives a scrubbed event as JSON. Delivery runs
//! on one background thread so a slow endpoint never blocks the player that
//! produced the event.
//!
//! The webhook is a side channel for an operator, so it must never be able to
//! take playback down with it. An endpoint that refuses or rate-limits a post
//! (Discord answers 429 under load) gets the event retried with a back-off, and
//! an event that still cannot be delivered, or that finds the queue full, is
//! dropped with a warning and counted.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rustlink_config::Webhook as Settings;
use serde_json::{json, Map, Value};

/// How many events may wait for delivery. Past this the newest are dropped: an
/// endpoint that has not kept up with what is already queued will not be helped
/// by waiting for more.
const QUEUE: usize = 128;

/// How long one delivery attempt may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times one event is posted before it is given up on.
const MAX_ATTEMPTS: u32 = 4;

/// The longest the worker waits before retrying, whatever the endpoint asks for.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Periodic node-wide report, rendered as a dedicated Discord embed.
pub const PLAYER_STATE_TYPE: &str = "node.players";

/// How many fields Discord renders in one embed.
const MAX_EMBED_FIELDS: usize = 25;
/// Discord also caps the combined text of all embeds in one message.
const MAX_EMBED_TEXT: usize = 6000;

/// Posts events to a single URL. A disabled configuration yields no client at
/// all, so a caller never has to check whether one is configured.
pub struct Client {
    url: String,
    queue: SyncSender<Value>,
    /// Events given up on, whether the queue was full or the endpoint refused.
    dropped: Arc<AtomicU64>,
    stop: Arc<(Mutex<bool>, Condvar)>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

impl Client {
    /// Builds a client for the configuration with private-network targets
    /// refused unless explicitly permitted via [`Client::with_network_policy`].
    pub fn new(settings: &Settings) -> Option<Client> {
        Self::with_network_policy(settings, false)
    }

    /// Builds a client for the configuration using `allow_private_networks` to
    /// enforce the node's SSRF network policy on webhook DNS resolution.
    pub fn with_network_policy(
        settings: &Settings,
        allow_private_networks: bool,
    ) -> Option<Client> {
        let target = settings.url.trim().to_string();
        if !settings.enable || target.is_empty() {
            return None;
        }
        if rustlink_source::netguard::validate_url(&target).is_err() {
            return None;
        }
        let discord = is_discord(&target);
        let agent = rustlink_source::netguard::agent(allow_private_networks);
        let (queue, inbox) = sync_channel(QUEUE);
        let worker = target.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("webhook".into())
            .spawn(move || {
                let mut payload = Vec::with_capacity(1024);
                while let Ok(event) = inbox.recv() {
                    if *worker_stop.0.lock().unwrap() {
                        break;
                    }
                    payload.clear();
                    let ok = if discord {
                        serde_json::to_writer(&mut payload, &discord_payload(&event)).is_ok()
                    } else {
                        serde_json::to_writer(&mut payload, &event).is_ok()
                    };
                    if !ok {
                        continue;
                    }
                    let outcome = deliver(
                        || {
                            if *worker_stop.0.lock().unwrap() {
                                return Err(Failure {
                                    message: "delivery worker stopped".into(),
                                    retryable: false,
                                    retry_after: None,
                                });
                            }
                            post(&agent, &worker, &payload)
                        },
                        |wait| {
                            let (lock, cvar) = &*worker_stop;
                            let guard = lock.lock().unwrap();
                            let _ = cvar.wait_timeout_while(guard, wait, |stopped| !*stopped);
                        },
                    );
                    if let Err((attempts, error)) = outcome {
                        if *worker_stop.0.lock().unwrap() {
                            break;
                        }
                        let count = worker_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                        if count == 1 || count.is_multiple_of(100) {
                            log::warn!(
                                "webhook event dropped after {attempts} attempt(s): {error} dropped={count}"
                            );
                        }
                    }
                }
            })
            .ok()?;
        Some(Client { url: target, queue, dropped, stop })
    }

    /// The URL events are posted to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// How many events were given up on so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues an event for delivery. It returns immediately, so the caller is
    /// never held up by the endpoint.
    pub fn send(&self, event: Value) {
        self.enqueue(scrub_event(event));
    }

    /// Queues an event by reference without cloning fields that will be scrubbed.
    pub(crate) fn send_ref(&self, event: &Value) {
        self.enqueue(scrub_ref(event));
    }

    fn enqueue(&self, event: Value) {
        let reason = match self.queue.try_send(event) {
            Ok(()) => return,
            // A full queue means the endpoint has not kept up with what the
            // node has already produced; the newest event is the one dropped.
            Err(TrySendError::Full(_)) => "event queue is full",
            Err(TrySendError::Disconnected(_)) => "delivery worker stopped",
        };
        let count = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // One line for the first drop and then one per hundred, so a stalled
        // endpoint is reported without filling the log.
        if count == 1 || count.is_multiple_of(100) {
            log::warn!("webhook event dropped: {reason} dropped={count}");
        }
    }
}

/// Why one post did not go through, and whether posting again could help.
struct Failure {
    message: String,
    retryable: bool,
    /// How long the endpoint asked to wait, from a 429's `Retry-After`.
    retry_after: Option<Duration>,
}

/// How long to wait before attempt `attempt + 1`: what the endpoint asked for,
/// or 1 s, 2 s, 4 s when it did not, never more than a minute and never zero.
fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(6)))
        .clamp(Duration::from_millis(250), MAX_RETRY_WAIT)
}

/// Runs `attempt` until it succeeds, fails for good, or [`MAX_ATTEMPTS`] is
/// used up. `sleep` is a parameter so the schedule can be tested without waiting.
fn deliver(
    mut attempt: impl FnMut() -> Result<(), Failure>,
    mut sleep: impl FnMut(Duration),
) -> Result<(), (u32, String)> {
    let mut last = String::new();
    let mut attempts = 0;
    for tries in 0..MAX_ATTEMPTS {
        attempts = tries + 1;
        match attempt() {
            Ok(()) => return Ok(()),
            Err(failure) => {
                last = failure.message;
                if !failure.retryable {
                    break;
                }
                if tries + 1 < MAX_ATTEMPTS {
                    sleep(backoff(tries, failure.retry_after));
                }
            }
        }
    }
    Err((attempts, last))
}

const SCRUBBED_KEYS: [&str; 6] =
    ["token", "uri", "artwork_url", "identifier", "extra", "user_data"];

/// Removes fields that must not cross the webhook trust boundary.
///
/// Events delivered through a session remain untouched; this function is only
/// called by [`Client::send`] on the copy queued for an external endpoint. The
/// walk is recursive so a future event shape cannot accidentally re-introduce
/// a nested token or signed URL under a different wrapper.
fn scrub_event(mut event: Value) -> Value {
    scrub_value(&mut event);
    event
}

fn scrub_ref(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut out = Map::with_capacity(object.len());
            for (key, child) in object {
                if !SCRUBBED_KEYS.contains(&key.as_str()) {
                    out.insert(key.clone(), scrub_ref(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(values.iter().map(scrub_ref).collect()),
        Value::String(text) => {
            let mut copy = text.clone();
            redact_urls_in_place(&mut copy, false, "<redacted-url>");
            Value::String(copy)
        }
        other => other.clone(),
    }
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            // `identifier` is normally a source ID, but the native HTTP source
            // also uses the caller's URL as its identifier. Removing it avoids
            // leaking a signed query through a field that is not named `uri`.
            for key in SCRUBBED_KEYS {
                object.remove(key);
            }
            for child in object.values_mut() {
                scrub_value(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                scrub_value(child);
            }
        }
        // Source/media/voice failures can include the URL they were trying to
        // open in diagnostic text (`message`, `cause`, `reason`, etc.). Keep
        // the diagnostic text, but replace URL tokens carrying credentials.
        Value::String(text) => redact_urls_in_place(text, false, "<redacted-url>"),
        _ => {}
    }
}

/// Redacts URLs embedded in diagnostic text while retaining surrounding
/// punctuation and error text. Allocates nothing when no URL matches.
fn redact_urls_in_place(text: &mut String, redact_all: bool, placeholder: &str) {
    let mut output: Option<String> = None;
    let mut cursor = 0;
    while let Some(relative) = find_http_scheme(&text[cursor..]) {
        let start = cursor + relative;
        let end = text[start..]
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '>' | '<' | ',')
            })
            .map_or(text.len(), |offset| start + offset);
        let candidate = &text[start..end];
        if redact_all || candidate.contains(['?', '#', '@']) {
            let buf = output.get_or_insert_with(|| {
                let mut s = String::with_capacity(text.len());
                s.push_str(&text[..cursor]);
                s
            });
            buf.push_str(&text[cursor..start]);
            buf.push_str(placeholder);
        } else if let Some(buf) = output.as_mut() {
            buf.push_str(&text[cursor..end]);
        }
        cursor = end;
    }
    if let Some(mut buf) = output {
        buf.push_str(&text[cursor..]);
        *text = buf;
    }
}

fn find_http_scheme(text: &str) -> Option<usize> {
    const SCHEMES: [&[u8]; 8] =
        [b"socks5h", b"socks5", b"socks4a", b"socks4", b"https", b"http", b"wss", b"ws"];
    let bytes = text.as_bytes();
    let mut offset = 0usize;
    while let Some(rel) = bytes[offset..].windows(3).position(|w| w == b"://") {
        let colon = offset + rel;
        for scheme in SCHEMES {
            if let Some(start) = colon.checked_sub(scheme.len()) {
                let at_boundary = start == 0
                    || !matches!(
                        bytes[start - 1],
                        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.'
                    );
                if at_boundary && bytes[start..colon].eq_ignore_ascii_case(scheme) {
                    return Some(start);
                }
            }
        }
        offset = colon + 3;
    }
    None
}

/// Delivers one pre-serialized JSON payload, draining bounded response bytes so
/// `ureq` can return TLS sockets to its keep-alive pool.
fn post(agent: &ureq::Agent, url: &str, body: &[u8]) -> Result<(), Failure> {
    let response = agent
        .post(url)
        .header("Accept-Encoding", "identity")
        .header("Content-Type", "application/json")
        .header("User-Agent", "Rustlink")
        .config()
        .timeout_global(Some(SEND_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .send(body)
        .map_err(|error| {
            let mut detail = error.to_string().replace(url, "<redacted webhook URL>");
            redact_urls_in_place(&mut detail, true, "<redacted webhook URL>");
            Failure {
                message: format!("webhook endpoint could not be reached: {detail}"),
                retryable: true,
                retry_after: None,
            }
        })?;
    let status = response.status().as_u16();
    let header_retry = if status == 429 {
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<f64>().ok())
    } else {
        None
    };
    let mut response_bytes = Vec::new();
    let _ = std::io::Read::take(response.into_body().into_reader(), 4096)
        .read_to_end(&mut response_bytes);
    if (200..300).contains(&status) {
        return Ok(());
    }
    // A rate limit and a server fault may pass; any other refusal (a deleted
    // webhook, a bad payload) will be refused again, so it is not retried.
    let body_retry = if status == 429 {
        serde_json::from_slice::<Value>(&response_bytes)
            .ok()
            .and_then(|json| json.get("retry_after").and_then(Value::as_f64))
    } else {
        None
    };
    let retry_after = body_retry
        .or(header_retry)
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| Duration::from_secs_f64(seconds.min(MAX_RETRY_WAIT.as_secs_f64())));
    Err(Failure {
        message: format!("webhook answered {status}"),
        retryable: status == 429 || status >= 500,
        retry_after,
    })
}

/// Whether a URL points at a Discord webhook, which is served an embed instead
/// of the raw event.
fn is_discord(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    let has_suffix = |suffix: &[u8]| {
        host.as_bytes()
            .get(host.len().wrapping_sub(suffix.len())..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
    };
    let discord_host = host.eq_ignore_ascii_case("discord.com")
        || host.eq_ignore_ascii_case("discordapp.com")
        || has_suffix(b".discord.com")
        || has_suffix(b".discordapp.com");
    discord_host || url.path().contains("/api/webhooks/")
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
        if let Some(Value::String(slot)) = embed.get_mut(key) {
            let cut = truncate(slot, limit.min(remaining));
            remaining -= discord_len(&cut);
            if let Cow::Owned(shortened) = cut {
                *slot = shortened;
            }
        }
    }
    remaining = remaining.saturating_sub(
        embed.get("footer").and_then(|footer| footer["text"].as_str()).map_or(0, discord_len),
    );
    if let Some(Value::Array(fields)) = embed.remove("fields") {
        let mut bounded = Vec::new();
        for mut field in fields.into_iter().take(MAX_EMBED_FIELDS) {
            let name = truncate(field["name"].as_str().unwrap_or(""), 256);
            if name.is_empty() {
                continue;
            }
            let name_length = discord_len(&name);
            if remaining <= name_length {
                break;
            }
            let name_owned = match name {
                Cow::Borrowed(_) => None,
                Cow::Owned(shortened) => Some(shortened),
            };
            let value =
                truncate(field["value"].as_str().unwrap_or(""), 1024.min(remaining - name_length));
            if value.is_empty() {
                continue;
            }
            remaining -= name_length + discord_len(&value);
            if let Cow::Owned(shortened) = value {
                field["value"] = Value::String(shortened);
            }
            if let Some(shortened) = name_owned {
                field["name"] = Value::String(shortened);
            }
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
    let guild = embed_value(state.get("guild_id").unwrap_or(&Value::Null));
    let title = embed_value(state.get("title").unwrap_or(&Value::Null));
    let mut name = String::with_capacity(guild.len() + title.len() + 3);
    name.push_str(&guild);
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
    let mut out = String::with_capacity(48);
    let sep = |out: &mut String| {
        if !out.is_empty() {
            out.push_str(" · ");
        }
    };
    if embed_value(state.get("title").unwrap_or(&Value::Null)).is_empty() {
        out.push_str("idle");
    } else {
        let mut has_elapsed = false;
        if let Some(position) = number(state.get("position_ms").unwrap_or(&Value::Null)) {
            write_duration(&mut out, position as i64);
            has_elapsed = true;
        }
        if let Some(length) = number(state.get("duration_ms").unwrap_or(&Value::Null)) {
            if length > 0.0 {
                out.push('/');
                write_duration(&mut out, length as i64);
                has_elapsed = true;
            }
        }
        let _ = has_elapsed;
        match state.get("paused").and_then(Value::as_bool) {
            Some(true) => {
                sep(&mut out);
                out.push_str("paused");
            }
            Some(false) => {
                sep(&mut out);
                out.push_str("playing");
            }
            None => {}
        }
    }
    let volume = embed_value(state.get("volume").unwrap_or(&Value::Null));
    if !volume.is_empty() {
        sep(&mut out);
        out.push_str("vol ");
        out.push_str(&volume);
    }
    if state.get("connected").and_then(Value::as_bool) == Some(false) {
        sep(&mut out);
        out.push_str("disconnected");
    }
    if let Some(ping) = number(state.get("ping_ms").unwrap_or(&Value::Null)) {
        if ping >= 0.0 {
            sep(&mut out);
            let _ = write!(out, "ping {}", ping as i64);
        }
    }
    if out.is_empty() {
        return "—".into();
    }
    out
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
fn event_title(kind: &str) -> &str {
    match kind {
        "track.started" => "Track Started",
        "track.ended" => "Track Ended",
        "track.failed" => "Track Exception",
        "track.stalled" => "Track Stuck",
        "voice.closed" => "Voice WebSocket Closed",
        "voice.connected" => "Bot Joined Voice",
        "voice.disconnected" => "Bot Left Voice",
        "" => "Rustlink Event",
        other => other,
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
fn embed_value(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::String(text) => Cow::Borrowed(text.trim()),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => Cow::Owned(integer.to_string()),
            None => Cow::Owned(number.to_string()),
        },
        other => Cow::Owned(other.to_string().trim().to_owned()),
    }
}

/// Conservatively measures the same UTF-16 units JavaScript clients use. This
/// keeps astral characters such as emoji inside the documented field budgets
/// without ever cutting a Rust `char` or UTF-8 sequence in half.
fn discord_len(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

/// Keeps a value inside a Discord text budget and reserves one unit for an
/// ellipsis when truncation is necessary.
fn truncate(text: &str, limit: usize) -> Cow<'_, str> {
    if discord_len(text) <= limit {
        return Cow::Borrowed(text);
    }
    if limit == 0 {
        return Cow::Borrowed("");
    }
    let content_limit = limit - 1;
    let mut used = 0;
    let mut end_byte = 0;
    for (idx, character) in text.char_indices() {
        let width = character.len_utf16();
        if used + width > content_limit {
            break;
        }
        used += width;
        end_byte = idx + character.len_utf8();
    }
    let mut out = String::with_capacity(end_byte + '…'.len_utf8());
    out.push_str(&text[..end_byte]);
    out.push('…');
    Cow::Owned(out)
}

fn write_duration(out: &mut String, millis: i64) {
    let millis = millis.max(0);
    let total = millis / 1000 + i64::from(millis % 1000 >= 500);
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        let _ = write!(out, "{hours}:{minutes:02}:{seconds:02}");
    } else {
        let _ = write!(out, "{minutes}:{seconds:02}");
    }
}

/// Renders a track length as m:ss or h:mm:ss.
fn format_duration(millis: i64) -> String {
    let mut out = String::with_capacity(10);
    write_duration(&mut out, millis);
    out
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

    #[test]
    fn failed_delivery_does_not_put_the_webhook_token_in_logs() {
        // An invalid authority fails before any network request is attempted.
        let agent = rustlink_source::netguard::agent(false);
        let target = "http://bad host/api/webhooks/123/private-webhook-token?key=private-key";
        let error = post(&agent, target, br#"{"type":"test"}"#).unwrap_err().message;
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
    fn webhook_copy_scrubs_track_tokens_urls_and_private_metadata() {
        let event = json!({
            "type": "track.started",
            "track": {
                "token": "rlt1.secret-track-token",
                "user_data": { "requester": "private-user" },
                "extra": { "signed_url": "https://cdn.example/audio?sig=secret" },
                "metadata": {
                    "identifier": "https://cdn.example/audio?sig=secret",
                    "uri": "https://cdn.example/audio?sig=secret",
                    "artwork_url": "https://cdn.example/art?sig=secret",
                    "title": "A Song",
                    "source": "http"
                }
            }
        });
        let original = event.clone();
        let scrubbed = scrub_event(event);

        // The copy queued for a webhook carries useful presentation metadata,
        // but no replayable token, signed URL, or caller-owned bag.
        assert_eq!(scrubbed["track"]["metadata"]["title"], "A Song");
        assert!(scrubbed["track"].get("token").is_none());
        assert!(scrubbed["track"].get("user_data").is_none());
        assert!(scrubbed["track"].get("extra").is_none());
        assert!(scrubbed["track"]["metadata"].get("identifier").is_none());
        assert!(scrubbed["track"]["metadata"].get("uri").is_none());
        assert!(scrubbed["track"]["metadata"].get("artwork_url").is_none());

        // Session consumers still own the original, complete event value.
        assert_eq!(original["track"]["token"], "rlt1.secret-track-token");
        assert_eq!(original["track"]["metadata"]["uri"], "https://cdn.example/audio?sig=secret");
    }

    #[test]
    fn client_send_queues_the_scrubbed_copy() {
        let (queue, receiver) = sync_channel(1);
        let client = Client {
            url: "https://example.invalid".into(),
            queue,
            dropped: Arc::new(AtomicU64::new(0)),
            stop: Arc::new((Mutex::new(false), Condvar::new())),
        };
        client.send_ref(&json!({
            "type": "track.started",
            "track": {
                "token": "rlt1.secret",
                "metadata": { "uri": "https://cdn.example/audio?sig=secret", "title": "A Song" }
            }
        }));
        let queued = receiver.try_recv().expect("scrubbed event was queued");
        assert!(queued["track"].get("token").is_none());
        assert!(queued["track"]["metadata"].get("uri").is_none());
        assert_eq!(queued["track"]["metadata"]["title"], "A Song");
        assert_eq!(client.dropped(), 0);
    }

    #[test]
    fn a_full_queue_drops_the_newest_event_instead_of_stopping_the_node() {
        let (queue, receiver) = sync_channel(1);
        let client = Client {
            url: "https://example.invalid".into(),
            queue,
            dropped: Arc::new(AtomicU64::new(0)),
            stop: Arc::new((Mutex::new(false), Condvar::new())),
        };
        // Nothing reads the queue, as when the endpoint has stalled the worker.
        client.send(json!({ "type": "track.started" }));
        client.send(json!({ "type": "track.ended" }));
        client.send(json!({ "type": "track.ended" }));
        assert_eq!(client.dropped(), 2, "the two that did not fit are counted, not fatal");
        assert_eq!(receiver.try_recv().unwrap()["type"], "track.started", "the oldest is kept");
        drop(receiver);
        client.send(json!({ "type": "track.started" }));
        assert_eq!(client.dropped(), 3, "a stopped worker drops too");
    }

    fn refused(retryable: bool, retry_after: Option<Duration>) -> Failure {
        Failure { message: "webhook answered 429".into(), retryable, retry_after }
    }

    #[test]
    fn a_rate_limited_post_is_retried_after_the_wait_the_endpoint_asked_for() {
        let mut calls = 0;
        let mut waits = Vec::new();
        let result = deliver(
            || {
                calls += 1;
                if calls < 3 {
                    Err(refused(true, Some(Duration::from_secs(7))))
                } else {
                    Ok(())
                }
            },
            |wait| waits.push(wait),
        );
        assert!(result.is_ok());
        assert_eq!(calls, 3);
        assert_eq!(waits, [Duration::from_secs(7), Duration::from_secs(7)]);
    }

    #[test]
    fn retries_back_off_and_end_after_the_attempt_limit() {
        let mut calls = 0;
        let mut waits = Vec::new();
        let result = deliver(
            || {
                calls += 1;
                Err(refused(true, None))
            },
            |wait| waits.push(wait),
        );
        assert_eq!(result.unwrap_err(), (MAX_ATTEMPTS, "webhook answered 429".into()));
        assert_eq!(calls, MAX_ATTEMPTS);
        assert_eq!(
            waits,
            [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)],
            "no wait after the last attempt"
        );
    }

    #[test]
    fn a_refusal_that_will_repeat_is_not_retried_and_waits_are_bounded() {
        let mut calls = 0;
        let result = deliver(
            || {
                calls += 1;
                Err(refused(false, None))
            },
            |_| panic!("a permanent refusal must not wait"),
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert_eq!(backoff(0, Some(Duration::from_secs(3600))), MAX_RETRY_WAIT);
        assert_eq!(backoff(0, Some(Duration::ZERO)), Duration::from_millis(250));
        assert_eq!(backoff(40, None), MAX_RETRY_WAIT, "the shift cannot overflow");
    }

    #[test]
    fn webhook_exception_text_redacts_signed_urls_but_keeps_diagnostics() {
        let scrubbed = scrub_event(json!({
            "type": "track.failed",
            "exception": {
                "message": "media https://cdn.example/audio?sig=secret&expires=1 failed",
                "cause": "https://user:password@example.invalid/audio#fragment",
                "cause_stack_trace": "plain decoder context"
            }
        }));
        let exception = &scrubbed["exception"];
        assert_eq!(exception["message"], "media <redacted-url> failed");
        assert_eq!(exception["cause"], "<redacted-url>");
        assert_eq!(exception["cause_stack_trace"], "plain decoder context");
        let encoded = scrubbed.to_string();
        assert!(!encoded.contains("sig=secret"));
        assert!(!encoded.contains("password"));
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
        let emoji = "\u{1f600}".repeat(300);
        let cut = truncate(&emoji, 256);
        assert!(discord_len(&cut) <= 256);
        assert!(cut.ends_with('…'));
        assert!(cut[..cut.len() - '…'.len_utf8()]
            .chars()
            .all(|character| character == '\u{1f600}'));
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
                "title": "\u{1f600}".repeat(3000), "author": "\u{1f3b5}".repeat(1500),
                "uri": format!("https://example.com/{}", "\u{1f680}".repeat(1500)),
            }},
            "reason": "\u{1f4a5}".repeat(1000),
            "exception": {"message": "\u{26a0}\u{fe0f}".repeat(1000)},
        }));
        assert_discord_embed_limits(&payload);
    }

    #[test]
    fn extreme_duration_still_formats_as_a_nonnegative_time() {
        assert_eq!(format_duration(i64::MAX), "2562047788015:12:56");
    }
}
