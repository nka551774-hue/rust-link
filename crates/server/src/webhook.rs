//! Asynchronous delivery of scrubbed node events and Discord embeds to an external webhook.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustlink_config::Webhook as Settings;
use serde_json::{json, Map, Value};

const QUEUE: usize = 128;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Periodic node-wide report, rendered as a dedicated Discord embed.
pub const PLAYER_STATE_TYPE: &str = "node.players";

const MAX_EMBED_FIELDS: usize = 25;
const MAX_EMBED_TEXT: usize = 6000;

const COLOR_START: u32 = 0x57f287;
const COLOR_END: u32 = 0x95a5a6;
const COLOR_FAULT: u32 = 0xed4245;
const COLOR_DEFAULT: u32 = 0x5865f2;
const COLOR_STATE: u32 = 0x5865f2;

const SCRUBBED_KEYS: [&str; 6] =
    ["token", "uri", "artwork_url", "identifier", "extra", "user_data"];

pub struct Client {
    url: String,
    queue: SyncSender<Value>,
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

fn record_drop(counter: &AtomicU64, detail: impl std::fmt::Display) {
    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(100) {
        log::warn!("webhook event dropped{detail} dropped={count}");
    }
}

impl Client {
    pub fn new(settings: &Settings) -> Option<Client> {
        Self::with_network_policy(settings, false)
    }

    pub fn with_network_policy(
        settings: &Settings,
        allow_private_networks: bool,
    ) -> Option<Client> {
        let target = settings.url.trim().to_string();
        if !settings.enable
            || target.is_empty()
            || rustlink_source::netguard::validate_url(&target).is_err()
        {
            return None;
        }
        let parsed = url::Url::parse(&target).ok()?;
        if !allow_private_networks && is_disallowed_webhook_host(&parsed) {
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
                        record_drop(
                            &worker_dropped,
                            format_args!(" after {attempts} attempt(s): {error}"),
                        );
                    }
                }
            })
            .ok()?;
        Some(Client { url: target, queue, dropped, stop })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn send(&self, event: Value) {
        self.enqueue(scrub_event(event));
    }

    pub(crate) fn send_ref(&self, event: &Value) {
        self.enqueue(scrub_ref(event));
    }

    fn enqueue(&self, event: Value) {
        let reason = match self.queue.try_send(event) {
            Ok(()) => return,
            Err(TrySendError::Full(_)) => "event queue is full",
            Err(TrySendError::Disconnected(_)) => "delivery worker stopped",
        };
        record_drop(&self.dropped, format_args!(": {reason}"));
    }
}

struct Failure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(6)))
        .clamp(Duration::from_millis(250), MAX_RETRY_WAIT)
}

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

fn has_ascii_suffix(s: &str, suffix: &str) -> bool {
    s.as_bytes()
        .get(s.len().wrapping_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix.as_bytes()))
}

fn is_disallowed_webhook_host(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Ipv4(v4)) => {
            !rustlink_source::netguard::is_public(std::net::IpAddr::V4(v4))
        }
        Some(url::Host::Ipv6(v6)) => {
            !rustlink_source::netguard::is_public(std::net::IpAddr::V6(v6))
        }
        Some(url::Host::Domain(domain)) => {
            let bare = domain.trim_end_matches('.');
            bare.eq_ignore_ascii_case("localhost") || has_ascii_suffix(bare, ".localhost")
        }
        None => true,
    }
}

fn scrub_event(mut event: Value) -> Value {
    scrub_value(&mut event, false);
    event
}

fn scrub_ref(value: &Value) -> Value {
    scrub_ref_inner(value, false)
}

fn scrub_ref_inner(value: &Value, in_exception: bool) -> Value {
    match value {
        Value::Object(object) => {
            let mut out = Map::with_capacity(object.len());
            for (key, child) in object {
                if !SCRUBBED_KEYS.contains(&key.as_str()) {
                    out.insert(
                        key.clone(),
                        scrub_ref_inner(child, in_exception || key == "exception"),
                    );
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => {
            Value::Array(values.iter().map(|child| scrub_ref_inner(child, in_exception)).collect())
        }
        Value::String(text) => {
            Value::String(redact_urls(text, in_exception, "<redacted-url>").into_owned())
        }
        other => other.clone(),
    }
}

fn scrub_value(value: &mut Value, in_exception: bool) {
    match value {
        Value::Object(object) => {
            for key in SCRUBBED_KEYS {
                object.remove(key);
            }
            for (key, child) in object.iter_mut() {
                scrub_value(child, in_exception || key == "exception");
            }
        }
        Value::Array(values) => {
            for child in values {
                scrub_value(child, in_exception);
            }
        }
        Value::String(text) => redact_urls_in_place(text, in_exception, "<redacted-url>"),
        _ => {}
    }
}

fn redact_urls<'a>(text: &'a str, redact_all: bool, placeholder: &str) -> Cow<'a, str> {
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
    match output {
        Some(mut buf) => {
            buf.push_str(&text[cursor..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(text),
    }
}

fn redact_urls_in_place(text: &mut String, redact_all: bool, placeholder: &str) {
    if let Cow::Owned(redacted) = redact_urls(text, redact_all, placeholder) {
        *text = redacted;
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
            let retryable = !matches!(error, ureq::Error::BadUri(_) | ureq::Error::Http(_));
            let mut detail = error.to_string().replace(url, "<redacted webhook URL>");
            redact_urls_in_place(&mut detail, true, "<redacted webhook URL>");
            Failure {
                message: format!("webhook endpoint could not be reached: {detail}"),
                retryable,
                retry_after: None,
            }
        })?;
    let status = response.status().as_u16();
    let header_retry = (status == 429)
        .then(|| {
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|t| t.trim().parse::<f64>().ok())
        })
        .flatten();
    let mut response_bytes = Vec::new();
    let _ = response.into_body().into_reader().take(4096).read_to_end(&mut response_bytes);
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body_retry = (status == 429)
        .then(|| {
            serde_json::from_slice::<Value>(&response_bytes)
                .ok()
                .and_then(|json| json.get("retry_after").and_then(Value::as_f64))
        })
        .flatten();
    let retry_after = body_retry
        .or(header_retry)
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| Duration::from_secs_f64(s.min(MAX_RETRY_WAIT.as_secs_f64())));
    Err(Failure {
        message: format!("webhook answered {status}"),
        retryable: status == 429 || status >= 500,
        retry_after,
    })
}

fn is_discord(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    host.eq_ignore_ascii_case("discord.com")
        || host.eq_ignore_ascii_case("discordapp.com")
        || has_ascii_suffix(host, ".discord.com")
        || has_ascii_suffix(host, ".discordapp.com")
        || url.path().contains("/api/webhooks/")
}

fn base_embed(title: &str, color: u32) -> Map<String, Value> {
    let mut embed = Map::new();
    embed.insert("title".into(), json!(title));
    embed.insert("color".into(), json!(color));
    embed.insert("timestamp".into(), json!(timestamp()));
    embed.insert("footer".into(), json!({ "text": "Rustlink" }));
    embed
}

fn str_field<'a>(obj: &'a Value, key: &str) -> Cow<'a, str> {
    obj.get(key).map_or(Cow::Borrowed(""), embed_value)
}

fn num_field(obj: &Value, key: &str) -> Option<f64> {
    obj.get(key).and_then(Value::as_f64)
}

fn discord_payload(event: &Value) -> Value {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == PLAYER_STATE_TYPE {
        return player_state_payload(event);
    }
    let mut embed = base_embed(event_title(kind), event_color(kind));
    let mut fields: Vec<Value> = Vec::new();
    let mut push_field = |name: &str, text: &str| {
        if !text.is_empty() {
            fields.push(json!({ "name": name, "value": truncate(text, 1024), "inline": true }));
        }
    };
    match event.get("track").and_then(|track| track.get("metadata")) {
        Some(info) => {
            let title = str_field(info, "title");
            if !title.is_empty() {
                let author = str_field(info, "author");
                let description = if author.is_empty() {
                    format!("**{title}**")
                } else {
                    format!("**{title}** - {author}")
                };
                embed.insert("description".into(), json!(truncate(&description, 4096)));
            }
            push_field("Source", &str_field(info, "source"));
            push_field("Guild", &str_field(event, "guild_id"));
            push_field("URI", &str_field(info, "uri"));
            if let Some(length) = num_field(event, "duration_ms")
                .or_else(|| num_field(info, "duration_ms"))
                .filter(|&l| l > 0.0)
            {
                push_field("Duration", &format_duration(length as i64));
            }
        }
        None => {
            push_field("Guild", &str_field(event, "guild_id"));
            push_field("Channel", &str_field(event, "channel_id"));
        }
    }
    push_field("Reason", &str_field(event, "reason"));
    push_field("Code", &str_field(event, "code"));
    push_field("By remote", &str_field(event, "by_remote"));
    if let Some(threshold) = num_field(event, "threshold_ms").filter(|&t| t > 0.0) {
        push_field("Threshold", &format!("{threshold:.0} ms"));
    }
    if let Some(exception) = event.get("exception") {
        push_field("Exception", &str_field(exception, "message"));
        push_field("Cause", &str_field(exception, "cause"));
    }
    if !fields.is_empty() {
        embed.insert("fields".into(), Value::Array(fields));
    }
    bounded_payload(embed)
}

fn player_state_payload(report: &Value) -> Value {
    let mut embed = base_embed("Player State", COLOR_STATE);
    let players = str_field(report, "players");
    let playing = str_field(report, "playing_players");
    let sessions = str_field(report, "sessions");
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
        embed.get("footer").and_then(|f| f["text"].as_str()).map_or(0, discord_len),
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

fn player_state_name(state: &Value) -> String {
    let guild = str_field(state, "guild_id");
    let title = str_field(state, "title");
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

fn player_state_value(state: &Value) -> String {
    let mut out = String::with_capacity(48);
    let sep = |out: &mut String| {
        if !out.is_empty() {
            out.push_str(" · ");
        }
    };
    if str_field(state, "title").is_empty() {
        out.push_str("idle");
    } else {
        if let Some(position) = num_field(state, "position_ms") {
            write_duration(&mut out, position as i64);
        }
        if let Some(length) = num_field(state, "duration_ms").filter(|&l| l > 0.0) {
            out.push('/');
            write_duration(&mut out, length as i64);
        }
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
    let volume = str_field(state, "volume");
    if !volume.is_empty() {
        sep(&mut out);
        out.push_str("vol ");
        out.push_str(&volume);
    }
    if state.get("connected").and_then(Value::as_bool) == Some(false) {
        sep(&mut out);
        out.push_str("disconnected");
    }
    if let Some(ping) = num_field(state, "ping_ms").filter(|&p| p >= 0.0) {
        sep(&mut out);
        let _ = write!(out, "ping {}", ping as i64);
    }
    if out.is_empty() {
        "—".into()
    } else {
        out
    }
}

fn plural(count: &str, noun: &str) -> String {
    if count == "1" {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

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

fn event_color(kind: &str) -> u32 {
    match kind {
        "track.started" | "voice.connected" => COLOR_START,
        "track.ended" | "voice.disconnected" => COLOR_END,
        "track.failed" | "track.stalled" => COLOR_FAULT,
        _ => COLOR_DEFAULT,
    }
}

fn embed_value(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::String(text) => Cow::Borrowed(text.trim()),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::Number(num) => match num.as_i64() {
            Some(integer) => Cow::Owned(integer.to_string()),
            None => Cow::Owned(num.to_string()),
        },
        other => Cow::Owned(other.to_string().trim().to_owned()),
    }
}

fn discord_len(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }
}

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

fn format_duration(millis: i64) -> String {
    let mut out = String::with_capacity(10);
    write_duration(&mut out, millis);
    out
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (year, month, day) = civil_from_days(now.div_euclid(86_400));
    let seconds = now.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

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

    fn test_client(cap: usize) -> (Client, std::sync::mpsc::Receiver<Value>) {
        let (queue, receiver) = sync_channel(cap);
        let client = Client {
            url: "https://example.invalid".into(),
            queue,
            dropped: Arc::new(AtomicU64::new(0)),
            stop: Arc::new((Mutex::new(false), Condvar::new())),
        };
        (client, receiver)
    }

    #[test]
    fn failed_delivery_does_not_put_the_webhook_token_in_logs() {
        let agent = rustlink_source::netguard::agent(false);
        let target = "http://bad host/api/webhooks/123/private-webhook-token?key=private-key";
        let error = post(&agent, target, br#"{"type":"test"}"#).unwrap_err().message;
        assert!(error.contains("could not be reached"));
        assert!(!error.contains("private-webhook-token") && !error.contains("private-key"));
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
    fn track_voice_and_player_state_events_render_embeds_cleanly() {
        let track_payload = discord_payload(&json!({
            "op": "event",
            "type": "track.started",
            "guild_id": "1",
            "track": { "metadata": { "title": "A Song", "author": "Someone", "duration_ms": 200000, "source": "youtube" } },
        }));
        let embed = &track_payload["embeds"][0];
        assert_eq!(embed["title"], "Track Started");
        assert_eq!(embed["color"], COLOR_START);
        assert_eq!(embed["description"], "**A Song** - Someone");
        let fields = embed["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f["name"] == "Duration" && f["value"] == "3:20"));
        assert!(fields.iter().any(|f| f["name"] == "Source" && f["value"] == "youtube"));

        let voice_payload = discord_payload(&json!({
            "type": "voice.disconnected",
            "guild_id": "1",
            "channel_id": "2",
            "reason": "destroyed",
        }));
        let v_embed = &voice_payload["embeds"][0];
        assert_eq!(v_embed["title"], "Bot Left Voice");
        let v_fields = v_embed["fields"].as_array().unwrap();
        assert!(v_fields.iter().any(|f| f["name"] == "Channel" && f["value"] == "2"));
        assert!(v_fields.iter().any(|f| f["name"] == "Reason" && f["value"] == "destroyed"));

        let report_payload = discord_payload(&json!({
            "type": PLAYER_STATE_TYPE,
            "players": 2,
            "playing_players": 1,
            "sessions": 1,
            "states": [
                { "guild_id": "1", "title": "A Song", "position_ms": 30000, "duration_ms": 200000, "paused": false, "volume": 100, "connected": true, "ping_ms": 42 },
                { "guild_id": "2", "title": "", "volume": 50, "connected": false },
            ],
        }));
        let r_embed = &report_payload["embeds"][0];
        assert_eq!(r_embed["title"], "Player State");
        assert_eq!(r_embed["description"], "2 players · 1 playing · 1 session");
        let r_fields = r_embed["fields"].as_array().unwrap();
        assert_eq!(r_fields[0]["name"], "1 · A Song");
        assert_eq!(r_fields[0]["value"], "0:30/3:20 · playing · vol 100 · ping 42");
        assert_eq!(r_fields[1]["value"], "idle · vol 50 · disconnected");
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
        assert_eq!(scrubbed["track"]["metadata"]["title"], "A Song");
        for key in ["token", "user_data", "extra"] {
            assert!(scrubbed["track"].get(key).is_none());
        }
        for key in ["identifier", "uri", "artwork_url"] {
            assert!(scrubbed["track"]["metadata"].get(key).is_none());
        }
        assert_eq!(original["track"]["token"], "rlt1.secret-track-token");
        assert_eq!(original["track"]["metadata"]["uri"], "https://cdn.example/audio?sig=secret");
    }

    #[test]
    fn client_queues_scrubbed_copies_and_drops_overflow_without_blocking() {
        let (client, receiver) = test_client(1);
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

        let (full_client, full_rx) = test_client(1);
        full_client.send(json!({ "type": "track.started" }));
        full_client.send(json!({ "type": "track.ended" }));
        full_client.send(json!({ "type": "track.ended" }));
        assert_eq!(full_client.dropped(), 2);
        assert_eq!(full_rx.try_recv().unwrap()["type"], "track.started");
        drop(full_rx);
        full_client.send(json!({ "type": "track.started" }));
        assert_eq!(full_client.dropped(), 3);
    }

    fn refused(retryable: bool, retry_after: Option<Duration>) -> Failure {
        Failure { message: "webhook answered 429".into(), retryable, retry_after }
    }

    #[test]
    fn delivery_retries_rate_limits_backs_off_and_stops_on_permanent_refusal() {
        let mut calls = 0;
        let mut waits = Vec::new();
        assert!(deliver(
            || {
                calls += 1;
                if calls < 3 {
                    Err(refused(true, Some(Duration::from_secs(7))))
                } else {
                    Ok(())
                }
            },
            |wait| waits.push(wait),
        )
        .is_ok());
        assert_eq!(calls, 3);
        assert_eq!(waits, [Duration::from_secs(7), Duration::from_secs(7)]);

        let mut calls = 0;
        let mut waits = Vec::new();
        assert_eq!(
            deliver(
                || {
                    calls += 1;
                    Err(refused(true, None))
                },
                |wait| waits.push(wait),
            )
            .unwrap_err(),
            (MAX_ATTEMPTS, "webhook answered 429".into())
        );
        assert_eq!(calls, MAX_ATTEMPTS);
        assert_eq!(waits, [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)]);

        let mut calls = 0;
        assert!(deliver(
            || {
                calls += 1;
                Err(refused(false, None))
            },
            |_| panic!("a permanent refusal must not wait"),
        )
        .is_err());
        assert_eq!(calls, 1);
        assert_eq!(backoff(0, Some(Duration::from_secs(3600))), MAX_RETRY_WAIT);
        assert_eq!(backoff(0, Some(Duration::ZERO)), Duration::from_millis(250));
        assert_eq!(backoff(40, None), MAX_RETRY_WAIT);
    }

    #[test]
    fn truncation_and_duration_formatting_handle_cjk_utf16_and_extremes() {
        let cjk = "曲".repeat(300);
        let cut = truncate(&cjk, 256);
        assert_eq!(cut.chars().count(), 256);
        assert!(cut.ends_with('…'));

        let emoji_src = "\u{1f600}".repeat(300);
        let astral = truncate(&emoji_src, 256);
        assert!(discord_len(&astral) <= 256 && astral.ends_with('…'));
        assert!(astral[..astral.len() - '…'.len_utf8()].chars().all(|c| c == '\u{1f600}'));

        assert_eq!(format_duration(200_000), "3:20");
        assert_eq!(format_duration(3_600_000), "1:00:00");
        assert_eq!(format_duration(500), "0:01");
        assert_eq!(format_duration(i64::MAX), "2562047788015:12:56");
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
                        assert!(!text.is_empty() && discord_len(text) <= limit);
                        total += discord_len(text);
                    }
                }
            }
        }
        assert!(total <= 6000, "Discord rejects {total} embed characters");
    }

    #[test]
    fn oversized_metadata_many_players_and_astral_chars_stay_within_discord_budget() {
        let long_meta = discord_payload(&json!({
            "type": "track.failed", "guild_id": "1",
            "track": {"metadata": {
                "title": "長曲名".repeat(2000), "author": "歌手".repeat(1000),
                "source": "youtube", "uri": format!("https://example.com/{}", "x".repeat(3000)),
            }},
            "reason": "失敗".repeat(1000),
            "exception": {"message": "原因".repeat(1000), "cause": "rustlink.audio"},
        }));
        assert_discord_embed_limits(&long_meta);
        assert_eq!(long_meta["embeds"][0]["title"], "Track Exception");
        assert!(long_meta["embeds"][0]["description"].as_str().unwrap().starts_with("**長曲名"));

        let states: Vec<Value> = (0..50)
            .map(|i| {
                json!({
                    "guild_id": i.to_string(), "title": "曲".repeat(400),
                    "position_ms": 30000, "duration_ms": 200000,
                    "paused": false, "volume": 100, "connected": true, "ping_ms": 42,
                })
            })
            .collect();
        let many = discord_payload(&json!({
            "type": PLAYER_STATE_TYPE, "players": 50,
            "playing_players": 50, "sessions": 1, "states": states,
        }));
        assert_discord_embed_limits(&many);
        assert_eq!(many["embeds"][0]["description"], "50 players · 50 playing · 1 session");

        let astral = discord_payload(&json!({
            "type": "track.failed", "guild_id": "1",
            "track": {"metadata": {
                "title": "\u{1f600}".repeat(3000), "author": "\u{1f3b5}".repeat(1500),
                "uri": format!("https://example.com/{}", "\u{1f680}".repeat(1500)),
            }},
            "reason": "\u{1f4a5}".repeat(1000),
            "exception": {"message": "\u{26a0}\u{fe0f}".repeat(1000)},
        }));
        assert_discord_embed_limits(&astral);
    }

    #[test]
    fn exception_scrubs_urls_and_network_policy_refuses_private_or_invalid_targets() {
        let raw_event = json!({
            "type": "track.failed",
            "exception": {
                "message": "stream https://cdn.example/sig/secret-path-token/ip/203.0.113.5/audio.opus reset",
                "cause": "https://user:password@example.invalid/audio#fragment",
                "cause_stack_trace": "plain decoder context"
            }
        });
        for scrubbed in [scrub_event(raw_event.clone()), scrub_ref(&raw_event)] {
            assert_eq!(scrubbed["exception"]["message"], "stream <redacted-url> reset");
            assert_eq!(scrubbed["exception"]["cause"], "<redacted-url>");
            assert_eq!(scrubbed["exception"]["cause_stack_trace"], "plain decoder context");
            let text = scrubbed.to_string();
            assert!(!text.contains("secret-path-token") && !text.contains("password"));
        }

        for blocked in [
            "http://127.0.0.1:8080/hook",
            "http://localhost:8080/hook",
            "http://sub.localhost./hook",
            "http://[::1]:8080/hook",
        ] {
            let settings = Settings { enable: true, url: blocked.into(), state_latency_seconds: 5 };
            assert!(Client::with_network_policy(&settings, false).is_none(), "{blocked}");
        }
        let local = Settings {
            enable: true,
            url: "http://127.0.0.1:8080/hook".into(),
            state_latency_seconds: 5,
        };
        assert!(Client::with_network_policy(&local, true).is_some());

        let agent = rustlink_source::netguard::agent(false);
        assert!(!post(&agent, "not-a-valid-uri", b"{}").err().unwrap().retryable);
    }
}
