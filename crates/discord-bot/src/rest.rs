//! Token-safe Discord REST access for the native Rust bot.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::redirect::Policy;
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use url::Url;
use zeroize::Zeroizing;

use crate::BotError;

const DEFAULT_BASE: &str = "https://discord.com/api/v10/";
const MAX_CONTENT_CHARS: usize = 2_000;
const MAX_RESPONSE_BYTES: u64 = 2 << 20;

/// The subset of Discord's message object needed after a send.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SentMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub content: String,
}

/// Authenticated Discord REST access for a bot token.
pub struct DiscordRest {
    client: reqwest::Client,
    base: Url,
    token: Zeroizing<String>,
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
        let token = token.into();
        validate_token(&token)?;
        let base = parse_base(base)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()?;
        Ok(Self { client, base, token: Zeroizing::new(token) })
    }

    /// Sends a plain text message to a channel.
    pub async fn send_text(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<SentMessage, BotError> {
        validate_snowflake(channel_id, "channel_id")?;
        validate_content(content)?;
        let body = self
            .call(
                Method::POST,
                &format!("channels/{channel_id}/messages"),
                &json!({ "content": content }),
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
        validate_snowflake(application_id, "application_id")?;
        validate_snowflake(guild_id, "guild_id")?;
        let path = format!("applications/{application_id}/guilds/{guild_id}/commands");
        self.call(Method::PUT, &path, commands).await.map(drop)
    }

    /// Acknowledges a slash command within Discord's 3-second window and shows
    /// "thinking…" until the response is edited.
    pub async fn defer_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), BotError> {
        validate_snowflake(interaction_id, "interaction_id")?;
        validate_interaction_token(interaction_token)?;
        let path = format!("interactions/{interaction_id}/{interaction_token}/callback");
        self.call(Method::POST, &path, &json!({ "type": 5 })).await.map(drop)
    }

    /// Replaces the text of a deferred interaction response.
    pub async fn edit_interaction_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), BotError> {
        validate_snowflake(application_id, "application_id")?;
        validate_interaction_token(interaction_token)?;
        validate_content(content)?;
        let path = format!("webhooks/{application_id}/{interaction_token}/messages/@original");
        self.call(Method::PATCH, &path, &json!({ "content": content })).await.map(drop)
    }

    /// Sends one authenticated JSON request and returns the success body.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Vec<u8>, BotError> {
        let endpoint = self
            .base
            .join(path)
            .map_err(|error| BotError::Invalid(format!("invalid Discord endpoint: {error}")))?;
        let authorization = HeaderValue::from_str(&format!("Bot {}", self.token.as_str()))
            .map_err(|_| {
                BotError::Invalid("bot token contains invalid header characters".into())
            })?;
        let response = self
            .client
            .request(method, endpoint)
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, "rustlink-discord-bot/0.1")
            .json(body)
            .send()
            .await
            .map_err(BotError::from)?;
        let status = response.status();
        if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BYTES) {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                message: "Discord response exceeded the configured limit".into(),
            });
        }
        let body = read_response_body(response, status).await?;
        if !status.is_success() {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                message: redact(&discord_error_message(&body), self.token.as_str()),
            });
        }
        Ok(body)
    }
}

/// Interaction tokens are spliced into URL paths, so only URL-safe characters pass.
fn validate_interaction_token(token: &str) -> Result<(), BotError> {
    if token.is_empty()
        || token.len() > 512
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BotError::Invalid("interaction token is malformed".into()));
    }
    Ok(())
}

async fn read_response_body(
    mut response: reqwest::Response,
    status: reqwest::StatusCode,
) -> Result<Vec<u8>, BotError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BotError::from)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES as usize {
            return Err(BotError::HttpStatus {
                status: status.as_u16(),
                message: "Discord response exceeded the configured limit".into(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
    let host = base.host_str().unwrap_or_default().to_ascii_lowercase();
    let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]");
    let discord = host == "discord.com" || host.ends_with(".discord.com");
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
        base.set_path(&format!("{}/", base.path()));
    }
    Ok(base)
}

fn validate_token(token: &str) -> Result<(), BotError> {
    if token.is_empty()
        || token.len() > 512
        || token.bytes().any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        return Err(BotError::Invalid(
            "bot token is empty, too long, or contains whitespace".into(),
        ));
    }
    if token.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err(BotError::Invalid("bot token contains invalid header characters".into()));
    }
    Ok(())
}

fn validate_snowflake(value: &str, name: &str) -> Result<(), BotError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.parse::<u64>().map_or(true, |id| id == 0)
    {
        return Err(BotError::Invalid(format!("{name} must be a nonzero Discord snowflake")));
    }
    Ok(())
}

fn validate_content(content: &str) -> Result<(), BotError> {
    let length = content.chars().count();
    if content.is_empty() || length > MAX_CONTENT_CHARS || content.contains('\0') {
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
        Some(code) if !code.is_null() => format!("{} (code {})", message, code),
        _ => message,
    }
}

fn redact(value: &str, token: &str) -> String {
    if token.is_empty() {
        value.to_string()
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
        for bad in ["", "a/../b", "a?b", "a b", "a%2Fb"] {
            assert!(validate_interaction_token(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn error_body_parser_does_not_return_raw_json() {
        let body = br#"{"message":"bad request","code": 50035,"errors":{"content":{}}}"#;
        let message = discord_error_message(body);
        assert_eq!(message, "bad request (code 50035)");
        assert!(!message.contains("errors"));
    }
}
