//! Settings for the music bot, read from a `.env` file and the environment.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Everything the bot needs to start. `Debug` hides credentials.
#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub password: String,
    pub rustlink_url: String,
    pub youtube_po_token: String,
    pub youtube_visitor_data: String,
    pub guilds: Option<HashSet<String>>,
    pub volume: i64,
    pub max_queue: usize,
    pub idle_leave: Duration,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &"<redacted>")
            .field("password", &"<redacted>")
            .field("rustlink_url", &self.rustlink_url)
            .field(
                "youtube_po_token",
                &if self.youtube_po_token.is_empty() { "" } else { "<redacted>" },
            )
            .field(
                "youtube_visitor_data",
                &if self.youtube_visitor_data.is_empty() { "" } else { "<redacted>" },
            )
            .field("guilds", &self.guilds)
            .field("volume", &self.volume)
            .field("max_queue", &self.max_queue)
            .field("idle_leave", &self.idle_leave)
            .finish()
    }
}

impl Config {
    /// Reads `path` (missing file is ignored) overridden by process environment.
    pub fn load(path: &Path) -> Result<Config, String> {
        let file = match std::fs::read_to_string(path) {
            Ok(text) => parse_env(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Config::from_lookup(|name| std::env::var(name).ok().or_else(|| file.get(name).cloned()))
            .map_err(|p| format!("{p}\nCopy .env.example to {} and fill it in.", path.display()))
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let get = |k: &str| lookup(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let required = |k: &str| get(k).ok_or_else(|| format!("{k} is required"));
        let number = |k: &str, def: i64, low: i64, high: i64| -> Result<i64, String> {
            match get(k) {
                None => Ok(def),
                Some(s) => s
                    .parse::<i64>()
                    .ok()
                    .filter(|v| (low..=high).contains(v))
                    .ok_or_else(|| format!("{k} must be a whole number from {low} to {high}")),
            }
        };
        let guilds = match get("DISCORD_GUILD_IDS") {
            None => None,
            Some(list) => {
                let mut ids = HashSet::new();
                for id in list.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                    if id.len() > 20
                        || id.starts_with('0')
                        || !id.bytes().all(|b| b.is_ascii_digit())
                        || !id.parse::<u64>().is_ok_and(|v| v > 0)
                    {
                        return Err(format!("DISCORD_GUILD_IDS has an invalid guild ID: {id:?}"));
                    }
                    ids.insert(id.to_owned());
                }
                (!ids.is_empty()).then_some(ids)
            }
        };
        Ok(Config {
            token: required("DISCORD_BOT_TOKEN")?,
            password: required("RUSTLINK_PASSWORD")?,
            rustlink_url: get("RUSTLINK_URL").unwrap_or_else(|| "http://127.0.0.1:2333".into()),
            youtube_po_token: get("RUSTLINK_YOUTUBE_PO_TOKEN")
                .or_else(|| get("YOUTUBE_PO_TOKEN"))
                .unwrap_or_default(),
            youtube_visitor_data: get("RUSTLINK_YOUTUBE_VISITOR_DATA")
                .or_else(|| get("YOUTUBE_VISITOR_DATA"))
                .unwrap_or_default(),
            guilds,
            volume: number("BOT_DEFAULT_VOLUME", 100, 0, 200)?,
            max_queue: number("BOT_MAX_QUEUE", 100, 1, 1000)? as usize,
            idle_leave: Duration::from_secs(
                number("BOT_IDLE_LEAVE_SECONDS", 120, 0, 86_400)? as u64
            ),
        })
    }

    pub fn serves(&self, guild: &str) -> bool {
        self.guilds.as_ref().is_none_or(|g| g.contains(guild))
    }
}

/// Parses `KEY=VALUE` lines with `#` comments, `export ` prefix, and quotes.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.strip_prefix("export ").unwrap_or(line).split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            values.insert(key.to_owned(), unquote(val.trim()));
        }
    }
    values
}

fn unquote(value: &str) -> String {
    for q in ['"', '\''] {
        if let Some(end) = value.strip_prefix(q).and_then(|rest| rest.find(q)) {
            return value[1..1 + end].to_owned();
        }
    }
    value.find(" #").map_or_else(|| value.to_owned(), |end| value[..end].trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn a_env_file_is_read_line_by_line() {
        let text = "\u{feff}# bot\r\nDISCORD_BOT_TOKEN=abc.def\r\nexport RUSTLINK_PASSWORD = \"pa ss#word\"\n\
                    RUSTLINK_URL='http://10.0.0.2:2333'\nBOT_MAX_QUEUE=50 # songs\nnot a setting\n=empty\nBAD KEY=1\n";
        let values = parse_env(text);
        assert_eq!(values["DISCORD_BOT_TOKEN"], "abc.def");
        assert_eq!(values["RUSTLINK_PASSWORD"], "pa ss#word");
        assert_eq!(values["RUSTLINK_URL"], "http://10.0.0.2:2333");
        assert_eq!(values["BOT_MAX_QUEUE"], "50");
        assert_eq!(values.len(), 4, "{values:?}");
    }

    #[test]
    fn the_two_credentials_are_required_and_the_rest_have_defaults() {
        assert!(Config::from_lookup(lookup(&[])).unwrap_err().contains("DISCORD_BOT_TOKEN"));
        assert!(Config::from_lookup(lookup(&[("DISCORD_BOT_TOKEN", "t")]))
            .unwrap_err()
            .contains("RUSTLINK_PASSWORD"));

        let cfg =
            Config::from_lookup(lookup(&[("DISCORD_BOT_TOKEN", "t"), ("RUSTLINK_PASSWORD", "p")]))
                .unwrap();
        assert_eq!(cfg.rustlink_url, "http://127.0.0.1:2333");
        assert_eq!((cfg.volume, cfg.max_queue), (100, 100));
        assert_eq!(cfg.idle_leave, Duration::from_secs(120));
        assert!(cfg.serves("1") && cfg.serves("2"));
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

        let cfg = with(("DISCORD_GUILD_IDS", " 111 ,222,")).unwrap();
        assert!(cfg.serves("111") && cfg.serves("222") && !cfg.serves("333"));
    }

    #[test]
    fn debug_output_hides_the_credentials() {
        let cfg = Config::from_lookup(lookup(&[
            ("DISCORD_BOT_TOKEN", "token-secret"),
            ("RUSTLINK_PASSWORD", "password-secret"),
        ]))
        .unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("token-secret") && !shown.contains("password-secret"), "{shown}");
    }
}
