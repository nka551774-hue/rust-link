//! YouTube sign-in for the TV client, through Google's device authorization
//! flow: the operator opens a short URL on any device and types a code, and
//! the node keeps the resulting refresh token in a local file.
//!
//! Only the refresh token is stored. Access tokens live in memory and are
//! renewed before they expire.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The YouTube TV application's OAuth client. It ships inside every TV app and
/// identifies that app, not an operator, so it is not a credential to protect.
const CLIENT_ID: &str = "861556708454-d6dlm3lh05idd8npek18k6be8ba3oc68.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "SboVhoG9s0rNafixCSGGKXAT";
const SCOPE: &str = "http://gdata.youtube.com https://www.googleapis.com/auth/youtube";
const DEVICE_URL: &str = "https://www.youtube.com/o/oauth2/device/code";
const TOKEN_URL: &str = "https://www.youtube.com/o/oauth2/token";
const DEVICE_GRANT: &str = "http://oauth.net/grant_type/device/1.0";
/// Renew an access token this long before Google says it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// What the operator must do to sign in.
pub struct Prompt {
    pub url: String,
    pub code: String,
    pub expires_in: Duration,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    refresh_token: String,
}

struct Session {
    agent: ureq::Agent,
    refresh: Mutex<Option<String>>,
    access: Mutex<Option<(String, Instant)>>,
}

static SESSION: OnceLock<Session> = OnceLock::new();

/// The outcome of asking Google for an access token.
#[derive(Debug, PartialEq, Eq)]
enum Grant {
    Token {
        access: String,
        expires_in: Duration,
        refresh: Option<String>,
    },
    /// Google refused the grant: the token was revoked or expired, so the
    /// operator has to sign in again.
    Rejected,
    /// The device code is still waiting for the operator.
    Pending {
        slower: bool,
    },
    /// Google could not be asked; keep what is stored and try later.
    Unavailable(String),
}

/// Checks the stored sign-in and, when there is none or Google rejects it,
/// shows a new code through `prompt` and waits for the operator on a thread of
/// its own. The node keeps serving meanwhile.
pub fn start(agent: ureq::Agent, path: PathBuf, prompt: impl Fn(&Prompt) + Send + 'static) {
    let session = SESSION.get_or_init(|| Session {
        agent,
        refresh: Mutex::new(None),
        access: Mutex::new(None),
    });
    let spawned = std::thread::Builder::new()
        .name("rustlink-youtube-oauth".into())
        .spawn(move || sign_in(session, &path, &prompt));
    if let Err(error) = spawned {
        log::warn!("youtube sign-in could not start error={error}");
    }
}

fn sign_in(session: &Session, path: &Path, prompt: &dyn Fn(&Prompt)) {
    if let Some(refresh) = load(path) {
        match refresh_grant(&session.agent, &refresh) {
            Grant::Token { access, expires_in, .. } => {
                session.store(refresh, access, expires_in);
                log::info!("youtube sign-in is valid file={}", path.display());
                return;
            }
            Grant::Unavailable(error) => {
                // A network failure says nothing about the token, so keep it
                // and let the next request renew it.
                *session.refresh.lock().unwrap() = Some(refresh);
                log::warn!(
                    "youtube sign-in could not be checked; keeping the stored token error={error}"
                );
                return;
            }
            Grant::Rejected | Grant::Pending { .. } => {
                log::warn!("youtube sign-in was rejected; a new sign-in is needed");
            }
        }
    }
    loop {
        let device = match device_code(&session.agent) {
            Ok(device) => device,
            Err(error) => {
                log::warn!("youtube sign-in could not be requested error={error}");
                return;
            }
        };
        prompt(&Prompt {
            url: device.verification_url.clone(),
            code: device.user_code.clone(),
            expires_in: Duration::from_secs(device.expires_in),
        });
        match wait_for_operator(&session.agent, &device) {
            Some(Grant::Token { access, expires_in, refresh: Some(refresh) }) => {
                if let Err(error) = save(path, &refresh) {
                    log::warn!("youtube sign-in could not be saved error={error}");
                }
                session.store(refresh, access, expires_in);
                log::info!("youtube sign-in complete file={}", path.display());
                return;
            }
            // The code expired unused: show a fresh one.
            None => continue,
            Some(_) => {
                log::warn!("youtube sign-in was declined; restart the node to try again");
                return;
            }
        }
    }
}

impl Session {
    fn store(&self, refresh: String, access: String, expires_in: Duration) {
        *self.refresh.lock().unwrap() = Some(refresh);
        *self.access.lock().unwrap() = Some((access, Instant::now() + expires_in));
    }
}

/// Whether a sign-in is held, without touching the network.
pub fn signed_in() -> bool {
    SESSION.get().is_some_and(|session| session.refresh.lock().unwrap().is_some())
}

/// A bearer token for a TV client request, renewed when it is about to
/// expire. `None` while the node is not signed in.
pub fn bearer() -> Option<String> {
    let session = SESSION.get()?;
    if let Some((token, until)) = session.access.lock().unwrap().as_ref() {
        if Instant::now() + EXPIRY_MARGIN < *until {
            return Some(token.clone());
        }
    }
    let refresh = session.refresh.lock().unwrap().clone()?;
    match refresh_grant(&session.agent, &refresh) {
        Grant::Token { access, expires_in, .. } => {
            *session.access.lock().unwrap() = Some((access.clone(), Instant::now() + expires_in));
            Some(access)
        }
        Grant::Rejected => {
            // Stop offering a dead token; a restart prompts for a new sign-in.
            *session.refresh.lock().unwrap() = None;
            log::warn!("youtube sign-in was revoked; restart the node to sign in again");
            None
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_url: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

fn device_code(agent: &ureq::Agent) -> Result<DeviceCode, String> {
    // A per-start identifier; Google uses it only to label the signed-in device.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let body = json!({
        "client_id": CLIENT_ID,
        "scope": SCOPE,
        "device_id": format!("rustlink-{nonce:x}"),
        "device_model": "ytlr::",
    });
    let (status, answer) = post(agent, DEVICE_URL, &body)?;
    if !(200..300).contains(&status) {
        return Err(format!("google answered {status}"));
    }
    serde_json::from_value(answer).map_err(|error| format!("unreadable device code: {error}"))
}

/// Polls until the operator signs in. `None` means the code expired unused.
fn wait_for_operator(agent: &ureq::Agent, device: &DeviceCode) -> Option<Grant> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));
    while Instant::now() < deadline {
        std::thread::sleep(interval);
        let body = json!({
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "code": device.device_code,
            "grant_type": DEVICE_GRANT,
        });
        match grant(agent, &body) {
            Grant::Pending { slower } => {
                if slower {
                    interval += Duration::from_secs(5);
                }
            }
            // A dropped connection while waiting is retried on the next tick.
            Grant::Unavailable(_) => {}
            Grant::Rejected => return None,
            other => return Some(other),
        }
    }
    None
}

fn refresh_grant(agent: &ureq::Agent, refresh: &str) -> Grant {
    grant(
        agent,
        &json!({
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "refresh_token": refresh,
            "grant_type": "refresh_token",
        }),
    )
}

fn grant(agent: &ureq::Agent, body: &Value) -> Grant {
    match post(agent, TOKEN_URL, body) {
        Ok((status, answer)) => classify(status, &answer),
        Err(error) => Grant::Unavailable(error),
    }
}

/// Reads a token endpoint answer. Kept apart from the network so the
/// decisions it makes can be tested.
fn classify(status: u16, answer: &Value) -> Grant {
    if let Some(access) = answer.get("access_token").and_then(Value::as_str) {
        return Grant::Token {
            access: access.to_owned(),
            expires_in: Duration::from_secs(
                answer.get("expires_in").and_then(Value::as_u64).unwrap_or(3600),
            ),
            refresh: answer.get("refresh_token").and_then(Value::as_str).map(str::to_owned),
        };
    }
    match answer.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Grant::Pending { slower: false },
        Some("slow_down") => Grant::Pending { slower: true },
        // A refused or revoked grant needs the operator either way.
        Some("invalid_grant" | "access_denied" | "expired_token" | "unauthorized_client") => {
            Grant::Rejected
        }
        Some(other) => Grant::Unavailable(format!("google answered {status}: {other}")),
        None => Grant::Unavailable(format!("google answered {status}")),
    }
}

fn post(agent: &ureq::Agent, url: &str, body: &Value) -> Result<(u16, Value), String> {
    let response = agent
        .post(url)
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .send_json(body)
        .map_err(|error| format!("google could not be reached: {error}"))?;
    let status = response.status().as_u16();
    let answer = response
        .into_body()
        .with_config()
        .limit(64 << 10)
        .read_json::<Value>()
        .map_err(|error| format!("google sent an unreadable answer: {error}"))?;
    Ok((status, answer))
}

fn load(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let stored: Stored = serde_json::from_str(&text).ok()?;
    Some(stored.refresh_token).filter(|token| !token.trim().is_empty())
}

fn save(path: &Path, refresh: &str) -> std::io::Result<()> {
    if let Some(directory) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(directory)?;
    }
    let text = serde_json::to_string(&Stored { refresh_token: refresh.to_owned() })?;
    std::fs::write(path, text)?;
    // The refresh token signs in as the operator's account; keep it private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_answers_decide_between_sign_in_retry_and_prompt() {
        assert_eq!(
            classify(200, &json!({"access_token":"a","expires_in":60,"refresh_token":"r"})),
            Grant::Token {
                access: "a".into(),
                expires_in: Duration::from_secs(60),
                refresh: Some("r".into())
            }
        );
        assert_eq!(
            classify(428, &json!({"error":"authorization_pending"})),
            Grant::Pending { slower: false }
        );
        assert_eq!(classify(403, &json!({"error":"slow_down"})), Grant::Pending { slower: true });
        // Only a refusal from Google asks the operator to sign in again.
        assert_eq!(classify(400, &json!({"error":"invalid_grant"})), Grant::Rejected);
        assert!(matches!(classify(503, &json!({})), Grant::Unavailable(_)));
    }

    #[test]
    fn the_stored_file_holds_only_the_refresh_token() {
        let path = std::env::temp_dir()
            .join(format!("rustlink-oauth-{}", std::process::id()))
            .join("youtube-oauth.json");
        assert_eq!(load(&path), None, "a missing file means not signed in");
        save(&path, "refresh-value").unwrap();
        assert_eq!(load(&path).as_deref(), Some("refresh-value"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, r#"{"refresh_token":"refresh-value"}"#);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), None, "a damaged file prompts for a new sign-in");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
