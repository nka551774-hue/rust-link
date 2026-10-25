//! Node configuration.
//!
//! A configuration file is YAML, which means JSON works too because it is a
//! subset of YAML. Field names are matched strictly: a misspelled option stops
//! startup and names the field, because a typo that is silently ignored is
//! harder to find than a startup error.
//!
//! Environment variables are applied on top of the file, so a container can be
//! configured without mounting one.

mod env;
mod validate;

pub use env::apply_env;
pub use validate::validate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Everything tunable about a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// Where the node listens. Every interface by default, because the client
    /// is usually a bot on another machine; a node behind a proxy that
    /// terminates TLS wants the loopback address instead.
    pub address: String,
    pub password: String,

    pub ffmpeg: String,
    /// Where media helpers downloaded at startup are kept, relative to the
    /// working directory.
    pub tools_directory: String,
    /// Install FFmpeg when missing from `tools_directory`. Explicit paths are
    /// used as configured; bare names are not searched on `PATH`.
    pub auto_download: bool,
    /// Play the formats that have a decoder built in without starting FFmpeg.
    /// Turning it off only removes an optimisation: FFmpeg covers everything
    /// the built-in decoders do.
    pub in_process_audio: bool,
    /// Where lyrics come from. `none` switches lyrics off and stops the node
    /// calling anything for them.
    pub lyrics_source: String,

    pub log_level: String,
    pub log_format: String,
    pub log_color: String,
    /// Mirror the log into daily files inside this directory, relative to the
    /// working directory. Empty keeps the console as the only output.
    pub log_directory: String,
    /// How many days of daily log files to keep. Zero keeps every file, which
    /// is what a node whose logs are collected and rotated elsewhere wants.
    pub log_retention_days: u32,

    pub youtube_native: bool,
    pub youtube_endpoint: String,
    pub youtube_music_endpoint: String,
    pub youtube_client_version: String,
    pub youtube_po_token: String,
    pub youtube_visitor_data: String,

    pub soundcloud_native: bool,
    pub soundcloud_api_endpoint: String,
    pub soundcloud_web_endpoint: String,
    pub soundcloud_client_id: String,
    pub soundcloud_client_id_ttl_seconds: u32,

    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub spotify_refresh_token: String,
    pub spotify_market: String,

    pub deezer_api_endpoint: String,

    /// Which filters clients may use. A filter that is absent stays enabled;
    /// listing it as `false` makes a player update refuse it.
    pub filters: BTreeMap<String, bool>,

    pub allow_private_networks: bool,

    pub max_players: u32,
    pub max_sessions: u32,
    pub max_concurrent_loads: u32,
    pub max_playlist_tracks: u32,
    pub search_results: u32,

    pub cache_entries: u32,
    pub cache_ttl_seconds: u32,
    pub load_timeout_seconds: u32,
    pub player_update_seconds: u32,
    pub stats_seconds: u32,
    pub event_buffer: u32,
    /// Opus bitrate in bits per second.
    pub opus_bitrate: u32,
    /// How hard the Opus encoder works, on its own scale of zero to ten. A node
    /// with more players than cores spends less here.
    pub opus_complexity: u32,

    pub ratelimits: RateLimit,
    pub webhook: Webhook,
}

/// Request throttling switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct RateLimit {
    pub enable: bool,
}

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit { enable: true }
    }
}

/// Where node events are posted. A Discord webhook URL is sent an embed; any
/// other endpoint is sent the event itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Webhook {
    pub enable: bool,
    pub url: String,
    /// How long the node waits between whole-node player state reports. Zero
    /// turns the report off, leaving the endpoint carrying only events.
    pub state_latency_seconds: u32,
}

impl Default for Webhook {
    fn default() -> Self {
        Webhook { enable: false, url: String::new(), state_latency_seconds: 5 }
    }
}

/// The filter names the protocol defines, in the order the reference
/// implementation reports them.
pub const FILTER_NAMES: [&str; 10] = [
    "volume",
    "equalizer",
    "karaoke",
    "timescale",
    "tremolo",
    "vibrato",
    "rotation",
    "distortion",
    "channelMix",
    "lowPass",
];

pub const LOG_LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];
pub const LOG_FORMATS: [&str; 2] = ["text", "json"];
pub const LOG_COLORS: [&str; 3] = ["auto", "always", "never"];
pub const LYRICS_SOURCES: [&str; 2] = ["lrclib", "none"];

/// The file names discovery looks for, in order.
pub const CANDIDATES: [&str; 3] = ["config.yaml", "config.yml", "config.json"];

impl Default for Config {
    fn default() -> Self {
        Config {
            address: "0.0.0.0:2333".into(),
            password: String::new(),
            ffmpeg: "ffmpeg".into(),
            tools_directory: ".tools".into(),
            auto_download: false,
            in_process_audio: true,
            lyrics_source: "lrclib".into(),
            log_level: "info".into(),
            log_format: "text".into(),
            log_color: "auto".into(),
            log_directory: "logs".into(),
            log_retention_days: 14,
            youtube_native: true,
            youtube_endpoint: "https://youtubei.googleapis.com".into(),
            youtube_music_endpoint: "https://music.youtube.com".into(),
            youtube_client_version: String::new(),
            youtube_po_token: String::new(),
            youtube_visitor_data: String::new(),
            soundcloud_native: true,
            soundcloud_api_endpoint: "https://api-v2.soundcloud.com".into(),
            soundcloud_web_endpoint: "https://soundcloud.com".into(),
            soundcloud_client_id: String::new(),
            soundcloud_client_id_ttl_seconds: 21_600,
            spotify_client_id: String::new(),
            spotify_client_secret: String::new(),
            spotify_refresh_token: String::new(),
            spotify_market: "US".into(),
            deezer_api_endpoint: "https://api.deezer.com".into(),
            filters: BTreeMap::new(),
            allow_private_networks: false,
            max_players: 100,
            max_sessions: 100,
            max_concurrent_loads: 8,
            max_playlist_tracks: 100,
            search_results: 5,
            cache_entries: 1024,
            cache_ttl_seconds: 300,
            load_timeout_seconds: 30,
            player_update_seconds: 5,
            stats_seconds: 60,
            event_buffer: 256,
            opus_bitrate: 128_000,
            opus_complexity: 10,
            ratelimits: RateLimit::default(),
            webhook: Webhook::default(),
        }
    }
}

impl Config {
    pub fn load_timeout(&self) -> Duration {
        Duration::from_secs(self.load_timeout_seconds as u64)
    }

    pub fn soundcloud_client_id_ttl(&self) -> Duration {
        Duration::from_secs(self.soundcloud_client_id_ttl_seconds as u64)
    }

    pub fn lyrics_enabled(&self) -> bool {
        !self.lyrics_source.eq_ignore_ascii_case("none")
    }

    pub fn spotify_enabled(&self) -> bool {
        !self.spotify_client_id.is_empty()
    }

    /// Whether the operator left a filter available to clients. A name that is
    /// absent from the map is enabled, which is what the reference
    /// implementation does.
    pub fn filter_enabled(&self, name: &str) -> bool {
        self.filters.get(name).copied().unwrap_or(true)
    }
}

/// Returns the configuration file to load.
///
/// An explicit path is used as given. Otherwise the first conventional name
/// present in the working directory is used, so an unpacked release starts with
/// no arguments at all.
pub fn discover(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    CANDIDATES.iter().map(PathBuf::from).find(|candidate| candidate.is_file())
}

/// The file a node writes for itself when it is started without one, which is
/// what double-clicking the binary looks like.
pub const STARTER_FILE: &str = "config.yaml";

/// Renders the configuration a node writes when it has none.
///
/// Every option has a default, so this only states what a fresh node cannot
/// guess: the password a client authenticates with, and where its files go. The
/// password is quoted because YAML would read an all-digit value as a number,
/// and it is generated rather than fixed because a node that ships with a
/// password ships with a hole in it.
pub fn starter(password: &str) -> String {
    format!(
        "# Written by Rustlink because no configuration file was found. Every\n\
         # setting has a default, so this only states what a fresh node needs,\n\
         # and the environment overrides it.\n\
         address: 0.0.0.0:2333\n\
         password: \"{password}\"\n\
         logLevel: info\n\
         logDirectory: logs\n\
         # Fetch FFmpeg on first start for media formats the built-in\n\
         # decoders do not support. Source resolution runs in Rust.\n\
         autoDownload: true\n"
    )
}

/// Loads the configuration: defaults, then the file, then the environment.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let mut config = Config::default();
    if let Some(path) = discover(explicit) {
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.display().to_string(), source })?;
        config = parse_configuration(&text)
            .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })?;
    }
    apply_env(&mut config);
    validate(&config)?;
    Ok(config)
}

fn parse_configuration(text: &str) -> Result<Config, serde_yaml::Error> {
    // Windows editors and PowerShell 5 may write a UTF-8 BOM before YAML/JSON.
    serde_yaml::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_utf8_bom_and_crlf_configuration_is_accepted() {
        for text in [
            "\u{feff}password: secret\r\nautoDownload: false\r\n",
            "\u{feff}{\"password\":\"secret\",\"autoDownload\":false}\r\n",
        ] {
            let config = parse_configuration(text).unwrap();
            assert_eq!(config.password, "secret");
            assert!(!config.auto_download);
        }
    }

    #[test]
    fn the_starter_configuration_loads_with_the_password_it_was_given() {
        let config: Config =
            serde_yaml::from_str(&starter("4f2a9c")).expect("the starter file parses");
        assert_eq!(config.password, "4f2a9c");
        assert_eq!(config.address, "0.0.0.0:2333");
        assert!(config.auto_download);
    }

    #[test]
    fn a_password_of_only_digits_survives_the_round_trip() {
        let config: Config =
            serde_yaml::from_str(&starter("12345678")).expect("the starter file parses");
        assert_eq!(config.password, "12345678");
    }

    #[test]
    fn the_starter_file_is_one_discovery_looks_for() {
        assert!(CANDIDATES.contains(&STARTER_FILE));
    }

    #[test]
    fn the_log_retention_window_is_read_from_the_file_or_defaulted() {
        let config: Config = serde_yaml::from_str("password: secret\nlogRetentionDays: 3\n")
            .expect("the file parses");
        assert_eq!(config.log_retention_days, 3);
        let config: Config = serde_yaml::from_str("password: secret\n").unwrap();
        assert_eq!(config.log_retention_days, 14, "an unset window is two weeks");
    }
}
