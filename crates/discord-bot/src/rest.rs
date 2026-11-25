//! Token-safe Discord REST access for the native Rust bot.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::Zeroizing;

use crate::BotError;

const DEFAULT_BASE: &str = "https://discord.com/api/v10/";
const USER_AGENT_VALUE: &str =
    concat!("DiscordBot (https://github.com/rustlink, ", env!("CARGO_PKG_VERSION"), ")");
const MAX_CONTENT_CHARS: usize = 2_000;
const MAX_RESPONSE_BYTES: usize = 2 << 20;

/// The subset of Discord's message object needed after a send.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SentMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Serialize)]
struct ContentBody<'a> {
    content: &'a str,
}

#[derive(Serialize)]
struct DeferBody {
    #[serde(rename = "type")]
    kind: u8,
}

/// Authenticated Discord REST access for a bot token.
pub struct DiscordRest {
    client: reqwest::Client,
    base: Url,
    token: Zeroizing<String>,
    auth: HeaderValue,
}

impl fmt::Debug for DiscordRest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordRest")
            .field("base", &redact(self.base.as_str(), &self.token))
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl DiscordRest {
    /// Creates a client against Discord's public API v10 endpoint.
    pub fn new(token: impl Into<String>) -> Result<Self, BotError> {
        Self::with_base(token, DEFAULT_BASE)
    }

    /// Creates a client against an explicit API base. Production hosts must be
    /// `discord.com` (or a subdomain); plain HTTP is accepted only on loopback
    /// for local protocol tests, so a bot token cannot be sent to an arbitrary
    /// proxy by accident.
    pub fn with_base(token: impl Into<String>, base: &str) -> Result<Self, BotError> {
        let token = Zeroizing::new(token.into());
        validate_token(&token)?;
        let auth_text = Zeroizing::new(format!("Bot {}", token.as_str()));
        let mut auth = HeaderValue::from_str(&auth_text).map_err(|_| {
            BotError::Invalid("bot token contains invalid header characters".into())
        })?;
        auth.set_sensitive(true);
        let base = parse_base(base)?;
        let client = reqwest::Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()?;
        Ok(Self { client, base, token, auth })
    }

    /// Sends a plain text message to a channel.
    pub async fn send_text(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<SentMessage, BotError> {
        let channel_id = crate::validate_snowflake(channel_id, "channel_id")?;
        validate_content(content)?;
        let body = self
            .call(
                Method::POST,
                &format!("channels/{channel_id}/messages"),
                &ContentBody { content },
                true,
                MAX_RATE_LIMIT_WAIT,
                Duration::from_secs(30),
            )
            .await?;
        serde_json::from_slice(&body).map_err(BotError::from)
    }

    /// Replaces this application's slash commands in one guild. Guild commands
    /// appear immediately, unlike global ones.
    pub async fn set_guild_commands(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: &serde_json::Value,
    ) -> Result<(), BotError> {
        let application_id = crate::validate_snowflake(application_id, "application_id")?;
        let guild_id = crate::validate_snowflake(guild_id, "guild_id")?;
        let path = format!("applications/{application_id}/guilds/{guild_id}/commands");
        self.call(Method::PUT, &path, commands, true, MAX_RATE_LIMIT_WAIT, Duration::from_secs(30))
            .await
            .map(drop)
    }

    /// Acknowledges a slash command within Discord's 3-second window and shows
    /// "thinking…" until the response is edited.
    pub async fn defer_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), BotError> {
        let interaction_id = crate::validate_snowflake(interaction_id, "interaction_id")?;
        validate_interaction_token(interaction_token)?;
        let path = format!("interactions/{interaction_id}/{interaction_token}/callback");
        self.call(
            Method::POST,
            &path,
            &DeferBody { kind: 5 },
            false,
            Duration::from_millis(1000),
            Duration::from_millis(2500),
        )
        .await
        .map(drop)
    }

    /// Replaces the text of a deferred interaction response.
    pub async fn edit_interaction_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), BotError> {
        let application_id = crate::validate_snowflake(application_id, "application_id")?;
        validate_interaction_token(interaction_token)?;
        validate_content(content)?;
        let path = format!("webhooks/{application_id}/{interaction_token}/messages/@original");
        self.call(
            Method::PATCH,
            &path,
            &ContentBody { content },
            false,
            MAX_RATE_LIMIT_WAIT,
            Duration::from_secs(30),
        )
        .await
        .map(drop)
    }

    /// Sends one JSON request and returns the success body. Interaction
    /// callback/webhook routes authenticate with `interaction_token` in the
    /// path, so the bot token is omitted there.
    async fn call<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        use_bot_auth: bool,
        max_retry_wait: Duration,
        request_timeout: Duration,
    ) -> Result<Vec<u8>, BotError> {
        let endpoint = self
            .base
            .join(path)
            .map_err(|error| BotError::Invalid(format!("invalid Discord endpoint: {error}")))?;
        let payload = serde_json::to_vec(body)?;
        let mut limited = 0;
        loop {
            let mut req = self
                .client
                .request(method.clone(), endpoint.clone())
                .timeout(request_timeout)
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, USER_AGENT_VALUE)
                .body(payload.clone());
            if use_bot_auth {
                req = req.header(AUTHORIZATION, self.auth.clone());
            }
            let response = req.send().await.map_err(BotError::from)?;
            let status = response.status();
            let retry_header = if status.as_u16() == 429 {
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            } else {
                None
            };
            let response_body = crate::read_limited_body(
                response,
                status,
                MAX_RESPONSE_BYTES,
                "Discord response exceeded the configured limit",
            )
            .await?;
            // Discord rate-limits per route and says how long to wait. A short
            // wait is worth taking, because the request is otherwise lost for no
            // fault of its own; a long one is reported, since a caller cannot
            // sit on it (an interaction token expires within minutes).
            if status.as_u16() == 429 && limited < MAX_RATE_LIMIT_RETRIES {
                if let Some(wait) = retry_wait(&response_body, retry_header.as_deref())
                    .filter(|wait| *wait <= max_retry_wait)
                {
                    limited += 1;
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
            if !status.is_success() {
                return Err(BotError::HttpStatus {
                    status: status.as_u16(),
                    message: redact(&discord_error_message(&response_body), self.token.as_str()),
                });
            }
            return Ok(response_body);
        }
    }
}

/// How many times a request is repeated after Discord answers 429.
const MAX_RATE_LIMIT_RETRIES: usize = 3;

/// The longest a rate limit is waited out.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(10);

/// How long a 429 asks to wait: the `retry_after` of its JSON body, which has
/// sub-second precision, or else the `Retry-After` header. `None` when it
/// names no usable wait or one longer than [`MAX_RATE_LIMIT_WAIT`].
fn retry_wait(body: &[u8], header: Option<&str>) -> Option<Duration> {
    let seconds = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("retry_after").and_then(serde_json::Value::as_f64))
        .or_else(|| header.and_then(|text| text.trim().parse::<f64>().ok()))
        .unwrap_or(1.0);
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let wait = Duration::from_secs_f64(seconds.min(MAX_RATE_LIMIT_WAIT.as_secs_f64() * 2.0));
    (wait <= MAX_RATE_LIMIT_WAIT).then_some(wait.max(Duration::from_millis(50)))
}

/// Interaction tokens are spliced into URL paths, so only URL-safe characters pass.
/// Dot-segments (`.` and `..`) are explicitly refused so `Url::join` cannot traverse.
fn validate_interaction_token(token: &str) -> Result<(), BotError> {
    if token.is_empty()
        || token.len() > 512
        || token == "."
        || token == ".."
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BotError::Invalid("interaction token is malformed".into()));
    }
    Ok(())
}

fn parse_base(value: &str) -> Result<Url, BotError> {
    let mut base = Url::parse(value)
        .map_err(|error| BotError::Invalid(format!("invalid Discord API base: {error}")))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(BotError::Invalid(
            "Discord API base must be an HTTP(S) origin without credentials, query, or fragment"
                .into(),
        ));
    }
    let host = base.host_str().unwrap_or_default();
    let loopback =
        host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1" | "[::1]");
    let discord = host.eq_ignore_ascii_case("discord.com")
        || host
            .as_bytes()
            .get(host.len().wrapping_sub(".discord.com".len())..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(b".discord.com"));
    if !loopback && !discord {
        return Err(BotError::Invalid(
            "Discord API base must use discord.com or a loopback test host".into(),
        ));
    }
    if base.scheme() == "http" && !loopback {
        return Err(BotError::Invalid(
            "Discord API base must use HTTPS outside loopback tests".into(),
        ));
    }
    if !base.path().ends_with('/') {
        let mut path = base.path().to_owned();
        path.push('/');
        base.set_path(&path);
    }
    Ok(base)
}

fn validate_token(token: &str) -> Result<(), BotError> {
    if token.is_empty() || token.len() > 512 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(BotError::Invalid(
            "bot token is empty, too long, or contains invalid characters".into(),
        ));
    }
    Ok(())
}

fn validate_content(content: &str) -> Result<(), BotError> {
    if content.is_empty()
        || content.contains('\0')
        || content.chars().nth(MAX_CONTENT_CHARS).is_some()
    {
        return Err(BotError::Invalid(format!(
            "message content must contain 1..{MAX_CONTENT_CHARS} characters and no NUL"
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
struct DiscordErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

fn discord_error_message(body: &[u8]) -> String {
    let Ok(error) = serde_json::from_slice::<DiscordErrorBody>(body) else {
        return "Discord returned an error response".into();
    };
    let message = error.message.unwrap_or_else(|| "Discord request failed".into());
    match error.code {
        Some(code) if !code.is_null() => format!("{message} (code {code})"),
        _ => message,
    }
}

fn redact(value: &str, token: &str) -> String {
    if token.is_empty() || !value.contains(token) {
        value.to_owned()
    } else {
        value.replace(token, "<redacted-token>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_error_text_never_include_the_token() {
        let token = "super.secret.token";
        let rest =
            DiscordRest::with_base(token, "http://127.0.0.1:12345/super.secret.token/api/v10/")
                .unwrap();
        assert!(!format!("{rest:?}").contains(token));
        assert_eq!(redact(&format!("bad token={token}"), token), "bad token=<redacted-token>");
    }

    #[test]
    fn base_rejects_credentials_and_query_strings() {
        assert!(DiscordRest::with_base("token", "https://user:pass@example.test/").is_err());
        assert!(DiscordRest::with_base("token", "https://discord.com/?token=secret").is_err());
        assert!(DiscordRest::with_base("token", "https://example.test/api/v10").is_err());
        assert!(DiscordRest::with_base("token", "https://discord.com/api/v10").is_ok());
        assert!(DiscordRest::with_base("token", "http://example.test/api/v10").is_err());
    }

    #[test]
    fn message_and_channel_limits_are_checked_before_network_io() {
        let rest = DiscordRest::with_base("token", "http://127.0.0.1:1/").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(rest.send_text("not-a-snowflake", "hello")).unwrap_err();
        assert!(matches!(error, BotError::Invalid(_)));
        let error = runtime
            .block_on(rest.send_text("123", &"x".repeat(MAX_CONTENT_CHARS + 1)))
            .unwrap_err();
        assert!(matches!(error, BotError::Invalid(_)));
    }

    #[test]
    fn interaction_tokens_cannot_escape_their_url_path() {
        assert!(validate_interaction_token("aW50ZXJhY3Rpb24.abc-DEF_123").is_ok());
        for bad in ["", ".", "..", "a/../b", "a?b", "a b", "a%2Fb"] {
            assert!(validate_interaction_token(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_rate_limit_wait_comes_from_the_body_then_the_header_and_is_bounded() {
        let body = br#"{"message":"You are being rate limited.","retry_after":1.5,"global":false}"#;
        assert_eq!(retry_wait(body, None), Some(Duration::from_millis(1500)));
        assert_eq!(retry_wait(body, Some("9")), Some(Duration::from_millis(1500)), "the body wins");
        assert_eq!(retry_wait(b"", Some("2")), Some(Duration::from_secs(2)));
        assert_eq!(
            retry_wait(b"not json", None),
            Some(Duration::from_secs(1)),
            "no hint: one second"
        );
        assert_eq!(retry_wait(br#"{"retry_after":0}"#, None), Some(Duration::from_millis(50)));
        assert_eq!(retry_wait(br#"{"retry_after":60}"#, None), None, "too long to sit on");
        assert_eq!(retry_wait(br#"{"retry_after":-1}"#, None), None);
        assert_eq!(
            retry_wait(b"", Some("nan")),
            None,
            "a wait that is not a number is not trusted"
        );
    }

    /// A 429 that asks to wait `retry_after` seconds.
    fn limited(retry_after: &str) -> String {
        let body = format!(r#"{{"message":"limited","retry_after":{retry_after},"global":false}}"#);
        format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn ok() -> String {
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string()
    }

    /// Answers each request with the next of `responses`, and reports how many
    /// requests were made.
    async fn scripted(responses: Vec<String>) -> (String, tokio::task::JoinHandle<usize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut served = 0;
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0u8; 4096];
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                served += 1;
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            served
        });
        (base, server)
    }

    #[tokio::test]
    async fn a_short_rate_limit_is_waited_out_and_the_request_goes_through() {
        let (base, server) = scripted(vec![limited("0.05"), limited("0.05"), ok()]).await;
        let rest = DiscordRest::with_base("token", &base).unwrap();
        rest.edit_interaction_response("123", "abc", "hello").await.expect("retried to success");
        assert_eq!(server.await.unwrap(), 3, "two limited answers, then the real one");
    }

    #[tokio::test]
    async fn a_rate_limit_that_will_not_lift_is_reported_without_waiting_it_out() {
        let (base, server) = scripted(vec![limited("600.0")]).await;
        let rest = DiscordRest::with_base("token", &base).unwrap();
        let started = std::time::Instant::now();
        let error = rest.edit_interaction_response("123", "abc", "hello").await.unwrap_err();
        assert!(matches!(error, BotError::HttpStatus { status: 429, .. }), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "it waited out ten minutes");
        assert_eq!(server.await.unwrap(), 1, "and did not ask again");
    }

    #[tokio::test]
    async fn retries_stop_after_the_limit_when_the_rate_limit_keeps_coming() {
        // The first request and three retries.
        let (base, server) = scripted(vec![limited("0.05"); 1 + MAX_RATE_LIMIT_RETRIES]).await;
        let rest = DiscordRest::with_base("token", &base).unwrap();
        let error = rest.edit_interaction_response("123", "abc", "hello").await.unwrap_err();
        assert!(matches!(error, BotError::HttpStatus { status: 429, .. }), "{error}");
        assert_eq!(server.await.unwrap(), 1 + MAX_RATE_LIMIT_RETRIES);
    }

    #[test]
    fn error_body_parser_does_not_return_raw_json() {
        let body = br#"{"message":"bad request","code": 50035,"errors":{"content":{}}}"#;
        let message = discord_error_message(body);
        assert_eq!(message, "bad request (code 50035)");
        assert!(!message.contains("errors"));
    }
}
