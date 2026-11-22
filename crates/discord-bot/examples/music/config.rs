//! Settings for the music bot, read from a `.env` file and the environment.
//!
//! A real environment variable wins over the file, so a container or a shell can
//! override one value without editing the file.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Everything the bot needs to start. `Debug` hides the credentials.
#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub password: String,
    pub rustlink_url: String,
    /// Guilds that get the commands; `None` means every guild the bot is in.
    pub guilds: Option<HashSet<String>>,
    pub volume: i64,
    pub max_queue: usize,
    /// How long an idle bot stays in a voice channel; zero never leaves.
    pub idle_leave: Duration,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &"<redacted>")
            .field("password", &"<redacted>")
            .field("rustlink_url", &self.rustlink_url)
            .field("guilds", &self.guilds)
            .field("volume", &self.volume)
            .field("max_queue", &self.max_queue)
            .field("idle_leave", &self.idle_leave)
            .finish()
    }
}

impl Config {
    /// Reads `path` (a missing file is fine when the environment has the values)
    /// and the process environment.
    pub fn load(path: &Path) -> Result<Config, String> {
        let file = match std::fs::read_to_string(path) {
            Ok(text) => parse_env(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        Config::from_lookup(|name| std::env::var(name).ok().or_else(|| file.get(name).cloned()))
            .map_err(|problem| {
                format!("{problem}\nCopy .env.example to {} and fill it in.", path.display())
            })
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let get = |name: &str| {
            lookup(name).map(|value| value.trim().to_string()).filter(|v| !v.is_empty())
        };
        let required = |name: &str| get(name).ok_or_else(|| format!("{name} is required"));
        let number = |name: &str, default: i64, low: i64, high: i64| -> Result<i64, String> {
            match get(name) {
                None => Ok(default),
                Some(text) => text
                    .parse::<i64>()
                    .ok()
                    .filter(|value| (low..=high).contains(value))
                    .ok_or_else(|| format!("{name} must be a whole number from {low} to {high}")),
            }
        };
        let guilds = match get("DISCORD_GUILD_IDS") {
            None => None,
            Some(list) => {
                let mut ids = HashSet::new();
                for id in list.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                    if id.len() > 20 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(format!("DISCORD_GUILD_IDS has an invalid guild ID: {id:?}"));
                    }
                    ids.insert(id.to_string());
                }
                (!ids.is_empty()).then_some(ids)
            }
        };
        Ok(Config {
            token: required("DISCORD_BOT_TOKEN")?,
            password: required("RUSTLINK_PASSWORD")?,
            rustlink_url: get("RUSTLINK_URL").unwrap_or_else(|| "http://127.0.0.1:2333".into()),
            guilds,
            volume: number("BOT_DEFAULT_VOLUME", 100, 0, 200)?,
            max_queue: number("BOT_MAX_QUEUE", 100, 1, 1000)? as usize,
            idle_leave: Duration::from_secs(
                number("BOT_IDLE_LEAVE_SECONDS", 120, 0, 86_400)? as u64
            ),
        })
    }

    /// Whether the commands are registered in this guild.
    pub fn serves(&self, guild: &str) -> bool {
        self.guilds.as_ref().is_none_or(|guilds| guilds.contains(guild))
    }
}

/// Parses `KEY=VALUE` lines: `#` comments, an optional `export ` prefix, and
/// single or double quotes around a value. Anything else is ignored.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            continue;
        }
        values.insert(key.to_string(), unquote(value.trim()));
    }
    values
}

fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            if let Some(end) = rest.find(quote) {
                return rest[..end].to_string();
            }
        }
    }
    // An unquoted value ends at a space followed by `#`.
    match value.find(" #") {
        Some(end) => value[..end].trim_end().to_string(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }

    #[test]
    fn a_env_file_is_read_line_by_line() {
        let text = "\u{feff}# bot\r\nDISCORD_BOT_TOKEN=abc.def\r\nexport RUSTLINK_PASSWORD = \"pa ss#word\"\n\
                    RUSTLINK_URL='http://10.0.0.2:2333'\nBOT_MAX_QUEUE=50 # songs\nnot a setting\n=empty\nBAD KEY=1\n";
        let values = parse_env(text);
        assert_eq!(values["DISCORD_BOT_TOKEN"], "abc.def");
        assert_eq!(values["RUSTLINK_PASSWORD"], "pa ss#word", "quotes keep spaces and #");
        assert_eq!(values["RUSTLINK_URL"], "http://10.0.0.2:2333");
        assert_eq!(values["BOT_MAX_QUEUE"], "50", "an unquoted value stops at ` #`");
        assert_eq!(values.len(), 4, "malformed lines are skipped: {values:?}");
    }

    #[test]
    fn the_two_credentials_are_required_and_the_rest_have_defaults() {
        let error = Config::from_lookup(lookup(&[])).unwrap_err();
        assert!(error.contains("DISCORD_BOT_TOKEN"), "{error}");
        let error = Config::from_lookup(lookup(&[("DISCORD_BOT_TOKEN", "t")])).unwrap_err();
        assert!(error.contains("RUSTLINK_PASSWORD"), "{error}");

        let config =
            Config::from_lookup(lookup(&[("DISCORD_BOT_TOKEN", "t"), ("RUSTLINK_PASSWORD", "p")]))
                .unwrap();
        assert_eq!(config.rustlink_url, "http://127.0.0.1:2333");
        assert_eq!((config.volume, config.max_queue), (100, 100));
        assert_eq!(config.idle_leave, Duration::from_secs(120));
        assert!(config.serves("1") && config.serves("2"), "every guild by default");
    }

    #[test]
    fn numbers_and_guild_lists_are_validated() {
        let base = [("DISCORD_BOT_TOKEN", "t"), ("RUSTLINK_PASSWORD", "p")];
        let with = |extra: (&str, &str)| {
            let mut pairs = base.to_vec();
            pairs.push(extra);
            Config::from_lookup(lookup(&pairs))
        };
        assert!(with(("BOT_DEFAULT_VOLUME", "201")).is_err());
        assert!(with(("BOT_DEFAULT_VOLUME", "loud")).is_err());
        assert!(with(("BOT_MAX_QUEUE", "0")).is_err());
        assert_eq!(with(("BOT_IDLE_LEAVE_SECONDS", "0")).unwrap().idle_leave, Duration::ZERO);
        assert!(with(("DISCORD_GUILD_IDS", "12, x9")).is_err());

        let config = with(("DISCORD_GUILD_IDS", " 111 ,222,")).unwrap();
        assert!(config.serves("111") && config.serves("222") && !config.serves("333"));
    }

    #[test]
    fn debug_output_hides_the_credentials() {
        let config = Config::from_lookup(lookup(&[
            ("DISCORD_BOT_TOKEN", "token-secret"),
            ("RUSTLINK_PASSWORD", "password-secret"),
        ]))
        .unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("token-secret") && !shown.contains("password-secret"), "{shown}");
    }
}
