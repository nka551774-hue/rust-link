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

fn default_address() -> String {
    "0.0.0.0:2333".into()
}
fn default_ffmpeg() -> String {
    "ffmpeg".into()
}
fn default_tools_directory() -> String {
    ".tools".into()
}
const fn default_true() -> bool {
    true
}
fn default_lyrics_source() -> String {
    "lrclib".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_log_format() -> String {
    "text".into()
}
fn default_log_color() -> String {
    "auto".into()
}
fn default_log_directory() -> String {
    "logs".into()
}
const fn default_log_retention_days() -> u32 {
    14
}
fn default_youtube_endpoint() -> String {
    "https://youtubei.googleapis.com".into()
}
fn default_youtube_music_endpoint() -> String {
    "https://music.youtube.com".into()
}
fn default_soundcloud_api_endpoint() -> String {
    "https://api-v2.soundcloud.com".into()
}
fn default_soundcloud_web_endpoint() -> String {
    "https://soundcloud.com".into()
}
const fn default_soundcloud_client_id_ttl_seconds() -> u32 {
    21_600
}
fn default_spotify_market() -> String {
    "US".into()
}
fn default_deezer_api_endpoint() -> String {
    "https://api.deezer.com".into()
}
const fn default_max_players() -> u32 {
    100
}
const fn default_max_sessions() -> u32 {
    100
}
const fn default_max_concurrent_loads() -> u32 {
    8
}
const fn default_max_playlist_tracks() -> u32 {
    100
}
const fn default_search_results() -> u32 {
    5
}
const fn default_cache_entries() -> u32 {
    1024
}
const fn default_cache_ttl_seconds() -> u32 {
    300
}
const fn default_load_timeout_seconds() -> u32 {
    30
}
const fn default_player_update_seconds() -> u32 {
    5
}
const fn default_stats_seconds() -> u32 {
    60
}
const fn default_event_buffer() -> u32 {
    256
}
const fn default_opus_bitrate() -> u32 {
    192_000
}
const fn default_opus_complexity() -> u32 {
    10
}
const fn default_webhook_state_latency_seconds() -> u32 {
    5
}

/// Everything tunable about a node. `Debug` redacts credentials.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// Where the node listens. Every interface by default, because the client
    /// is usually a bot on another machine; a node behind a proxy that
    /// terminates TLS wants the loopback address instead.
    #[serde(default = "default_address")]
    pub address: String,
    #[serde(default)]
    pub password: String,

    #[serde(default = "default_ffmpeg")]
    pub ffmpeg: String,
    /// Where media helpers downloaded at startup are kept, relative to the
    /// working directory.
    #[serde(default = "default_tools_directory")]
    pub tools_directory: String,
    /// Install FFmpeg when missing from `tools_directory`. Explicit paths are
    /// used as configured; bare names are not searched on `PATH`.
    #[serde(default)]
    pub auto_download: bool,
    /// Play the formats that have a decoder built in without starting FFmpeg.
    /// Turning it off only removes an optimisation: FFmpeg covers everything
    /// the built-in decoders do.
    #[serde(default = "default_true")]
    pub in_process_audio: bool,
    /// Where lyrics come from. `none` switches lyrics off and stops the node
    /// calling anything for them.
    #[serde(default = "default_lyrics_source")]
    pub lyrics_source: String,

    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_log_format")]
    pub log_format: String,
    #[serde(default = "default_log_color")]
    pub log_color: String,
    /// Mirror the log into daily files inside this directory, relative to the
    /// working directory. Empty keeps the console as the only output.
    #[serde(default = "default_log_directory")]
    pub log_directory: String,
    /// How many days of daily log files to keep. Zero keeps every file, which
    /// is what a node whose logs are collected and rotated elsewhere wants.
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,

    #[serde(default = "default_true")]
    pub youtube_native: bool,
    #[serde(default = "default_youtube_endpoint")]
    pub youtube_endpoint: String,
    #[serde(default = "default_youtube_music_endpoint")]
    pub youtube_music_endpoint: String,
    #[serde(default)]
    pub youtube_client_version: String,
    #[serde(default)]
    pub youtube_po_token: String,
    #[serde(default)]
    pub youtube_visitor_data: String,
    /// Sends every YouTube request through this proxy (`http://`, `socks5://`
    /// or `socks5h://`), for a node whose own address YouTube asks to sign in.
    /// Empty connects directly. It may carry credentials, so `Debug` hides it.
    #[serde(default)]
    pub youtube_proxy: String,

    #[serde(default = "default_true")]
    pub soundcloud_native: bool,
    #[serde(default = "default_soundcloud_api_endpoint")]
    pub soundcloud_api_endpoint: String,
    #[serde(default = "default_soundcloud_web_endpoint")]
    pub soundcloud_web_endpoint: String,
    #[serde(default)]
    pub soundcloud_client_id: String,
    #[serde(default = "default_soundcloud_client_id_ttl_seconds")]
    pub soundcloud_client_id_ttl_seconds: u32,

    #[serde(default)]
    pub spotify_client_id: String,
    #[serde(default)]
    pub spotify_client_secret: String,
    #[serde(default)]
    pub spotify_refresh_token: String,
    #[serde(default = "default_spotify_market")]
    pub spotify_market: String,

    #[serde(default = "default_deezer_api_endpoint")]
    pub deezer_api_endpoint: String,

    /// Which filters clients may use. A filter that is absent stays enabled;
    /// listing it as `false` makes a player update refuse it.
    #[serde(default)]
    pub filters: BTreeMap<String, bool>,

    #[serde(default)]
    pub allow_private_networks: bool,

    #[serde(default = "default_max_players")]
    pub max_players: u32,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    #[serde(default = "default_max_concurrent_loads")]
    pub max_concurrent_loads: u32,
    #[serde(default = "default_max_playlist_tracks")]
    pub max_playlist_tracks: u32,
    #[serde(default = "default_search_results")]
    pub search_results: u32,

    #[serde(default = "default_cache_entries")]
    pub cache_entries: u32,
    #[serde(default = "default_cache_ttl_seconds")]
    pub cache_ttl_seconds: u32,
    #[serde(default = "default_load_timeout_seconds")]
    pub load_timeout_seconds: u32,
    #[serde(default = "default_player_update_seconds")]
    pub player_update_seconds: u32,
    #[serde(default = "default_stats_seconds")]
    pub stats_seconds: u32,
    #[serde(default = "default_event_buffer")]
    pub event_buffer: u32,
    /// Opus bitrate in bits per second.
    #[serde(default = "default_opus_bitrate")]
    pub opus_bitrate: u32,
    /// How hard the Opus encoder works, on its own scale of zero to ten. A node
    /// with more players than cores spends less here.
    #[serde(default = "default_opus_complexity")]
    pub opus_complexity: u32,

    #[serde(default)]
    pub ratelimits: RateLimit,
    #[serde(default)]
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
/// other endpoint is sent the event itself. `Debug` redacts the URL, because a
/// Discord webhook URL carries its token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Webhook {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub url: String,
    /// How long the node waits between whole-node player state reports. Zero
    /// turns the report off, leaving the endpoint carrying only events.
    #[serde(default = "default_webhook_state_latency_seconds")]
    pub state_latency_seconds: u32,
}

impl Default for Webhook {
    fn default() -> Self {
        Webhook { enable: false, url: String::new(), state_latency_seconds: 5 }
    }
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` points to an initialized byte in `bytes`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

fn wipe_string(secret: &mut String) {
    // SAFETY: `write_volatile` cannot unwind, and overwriting initialized bytes
    // with `0` (ASCII NUL) preserves UTF-8 validity before `clear()`.
    unsafe { wipe_bytes(secret.as_bytes_mut()) };
    secret.clear();
}

impl Drop for Webhook {
    fn drop(&mut self) {
        wipe_string(&mut self.url);
    }
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("enable", &self.enable)
            .field("url", &redacted(&self.url))
            .field("state_latency_seconds", &self.state_latency_seconds)
            .finish()
    }
}

#[inline]
const fn redacted(value: &str) -> &'static str {
    if value.is_empty() {
        ""
    } else {
        "<redacted>"
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        wipe_string(&mut self.password);
        wipe_string(&mut self.youtube_po_token);
        wipe_string(&mut self.youtube_visitor_data);
        wipe_string(&mut self.youtube_proxy);
        wipe_string(&mut self.soundcloud_client_id);
        wipe_string(&mut self.spotify_client_id);
        wipe_string(&mut self.spotify_client_secret);
        wipe_string(&mut self.spotify_refresh_token);
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("address", &self.address)
            .field("password", &redacted(&self.password))
            .field("ffmpeg", &self.ffmpeg)
            .field("tools_directory", &self.tools_directory)
            .field("auto_download", &self.auto_download)
            .field("in_process_audio", &self.in_process_audio)
            .field("lyrics_source", &self.lyrics_source)
            .field("log_level", &self.log_level)
            .field("log_format", &self.log_format)
            .field("log_color", &self.log_color)
            .field("log_directory", &self.log_directory)
            .field("log_retention_days", &self.log_retention_days)
            .field("youtube_native", &self.youtube_native)
            .field("youtube_endpoint", &self.youtube_endpoint)
            .field("youtube_music_endpoint", &self.youtube_music_endpoint)
            .field("youtube_client_version", &self.youtube_client_version)
            .field("youtube_po_token", &redacted(&self.youtube_po_token))
            .field("youtube_visitor_data", &redacted(&self.youtube_visitor_data))
            .field("youtube_proxy", &redacted(&self.youtube_proxy))
            .field("soundcloud_native", &self.soundcloud_native)
            .field("soundcloud_api_endpoint", &self.soundcloud_api_endpoint)
            .field("soundcloud_web_endpoint", &self.soundcloud_web_endpoint)
            .field("soundcloud_client_id", &redacted(&self.soundcloud_client_id))
            .field("soundcloud_client_id_ttl_seconds", &self.soundcloud_client_id_ttl_seconds)
            .field("spotify_client_id", &self.spotify_client_id)
            .field("spotify_client_secret", &redacted(&self.spotify_client_secret))
            .field("spotify_refresh_token", &redacted(&self.spotify_refresh_token))
            .field("spotify_market", &self.spotify_market)
            .field("deezer_api_endpoint", &self.deezer_api_endpoint)
            .field("filters", &self.filters)
            .field("allow_private_networks", &self.allow_private_networks)
            .field("max_players", &self.max_players)
            .field("max_sessions", &self.max_sessions)
            .field("max_concurrent_loads", &self.max_concurrent_loads)
            .field("max_playlist_tracks", &self.max_playlist_tracks)
            .field("search_results", &self.search_results)
            .field("cache_entries", &self.cache_entries)
            .field("cache_ttl_seconds", &self.cache_ttl_seconds)
            .field("load_timeout_seconds", &self.load_timeout_seconds)
            .field("player_update_seconds", &self.player_update_seconds)
            .field("stats_seconds", &self.stats_seconds)
            .field("event_buffer", &self.event_buffer)
            .field("opus_bitrate", &self.opus_bitrate)
            .field("opus_complexity", &self.opus_complexity)
            .field("ratelimits", &self.ratelimits)
            .field("webhook", &self.webhook)
            .finish()
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
            youtube_proxy: String::new(),
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
            opus_bitrate: 192_000,
            opus_complexity: 10,
            ratelimits: RateLimit::default(),
            webhook: Webhook::default(),
        }
    }
}

impl Config {
    #[inline]
    pub const fn load_timeout(&self) -> Duration {
        Duration::from_secs(self.load_timeout_seconds as u64)
    }

    #[inline]
    pub const fn soundcloud_client_id_ttl(&self) -> Duration {
        Duration::from_secs(self.soundcloud_client_id_ttl_seconds as u64)
    }

    #[inline]
    pub fn lyrics_enabled(&self) -> bool {
        !self.lyrics_source.trim().eq_ignore_ascii_case("none")
    }

    #[inline]
    pub fn spotify_enabled(&self) -> bool {
        !self.spotify_client_id.trim().is_empty() && !self.spotify_client_secret.trim().is_empty()
    }

    /// Whether the operator left a filter available to clients. A name that is
    /// absent from the map is enabled, which is what the reference
    /// implementation does.
    #[inline]
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
    CANDIDATES
        .into_iter()
        .map(Path::new)
        .find(|candidate| candidate.is_file())
        .map(Path::to_path_buf)
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
    const PREFIX: &str = "# Written by Rustlink because no configuration file was found. Every\n\
        # setting has a default, so this only states what a fresh node needs,\n\
        # and the environment overrides it.\n\
        address: 0.0.0.0:2333\n\
        password: \"";
    const SUFFIX: &str = "\"\n\
        logLevel: info\n\
        logDirectory: logs\n\
        # Fetch FFmpeg on first start for media formats the built-in\n\
        # decoders do not support. Source resolution runs in Rust.\n\
        autoDownload: true\n";
    let mut out = String::with_capacity(PREFIX.len() + SUFFIX.len() + password.len());
    out.push_str(PREFIX);
    for ch in password.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            _ => out.push(ch),
        }
    }
    out.push_str(SUFFIX);
    out
}

const MAX_CONFIG_BYTES: u64 = 1 << 20;

fn read_bounded_config(path: &Path) -> Result<String, ConfigError> {
    use std::io::Read as _;
    let display = path.display().to_string();
    let file = std::fs::File::open(path)
        .map_err(|source| ConfigError::Read { path: display.clone(), source })?;
    if let Ok(meta) = file.metadata() {
        if meta.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::Invalid(format!(
                "configuration file {display} exceeds 1 MiB limit"
            )));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Read { path: display.clone(), source })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        wipe_bytes(&mut bytes);
        return Err(ConfigError::Invalid(format!(
            "configuration file {display} exceeds 1 MiB limit"
        )));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) => {
            let mut raw = error.into_bytes();
            wipe_bytes(&mut raw);
            Err(ConfigError::Read {
                path: display,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "configuration file is not valid UTF-8",
                ),
            })
        }
    }
}

/// Loads the configuration: defaults, then the file, then the environment.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let mut config = match discover(explicit) {
        Some(path) => {
            let mut text = read_bounded_config(&path)?;
            let parsed = parse_configuration(&text);
            wipe_string(&mut text);
            parsed
                .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })?
        }
        None => Config::default(),
    };
    apply_env(&mut config);
    validate(&config)?;
    Ok(config)
}

fn parse_configuration(text: &str) -> Result<Config, serde_yaml::Error> {
    // Windows editors and PowerShell 5 may write a UTF-8 BOM before YAML/JSON.
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    serde_yaml::from_str::<Option<Config>>(body).map(Option::unwrap_or_default)
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
    fn music_defaults_apply_only_when_encoding_settings_are_omitted() {
        let defaults = parse_configuration("password: secret\n").unwrap();
        assert_eq!(defaults.opus_bitrate, 192_000);
        assert_eq!(defaults.opus_complexity, 10);
        let explicit =
            parse_configuration("password: secret\nopusBitrate: 128000\nopusComplexity: 4\n")
                .unwrap();
        assert_eq!(explicit.opus_bitrate, 128_000);
        assert_eq!(explicit.opus_complexity, 4);
    }

    #[test]
    fn validated_lyrics_off_value_does_not_enable_network_providers() {
        let config = parse_configuration("password: secret\nlyricsSource: ' NoNe '\n").unwrap();
        validate(&config).unwrap();
        assert!(!config.lyrics_enabled());
    }

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
        let special = "p\"a\\s#s:word";
        let config: Config =
            serde_yaml::from_str(&starter(special)).expect("special characters parse safely");
        assert_eq!(config.password, special);
    }

    #[test]
    fn the_starter_file_is_one_discovery_looks_for() {
        assert!(CANDIDATES.contains(&STARTER_FILE));
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let mut config = Config::default();
        config.password = "hunter2-pass".into();
        config.spotify_client_secret = "spotify-secret".into();
        config.spotify_refresh_token = "refresh-secret".into();
        config.youtube_po_token = "po-secret".into();
        config.youtube_visitor_data = "visitor-secret".into();
        config.youtube_proxy = "socks5://user:proxy-secret@127.0.0.1:1080".into();
        config.soundcloud_client_id = "sc-secret".into();
        config.webhook.url = "https://discord.com/api/webhooks/1/hook-secret".into();
        for rendered in [format!("{config:?}"), format!("{:?}", config.webhook)] {
            for secret in [
                "hunter2",
                "spotify-secret",
                "refresh-secret",
                "po-secret",
                "visitor-secret",
                "proxy-secret",
                "sc-secret",
                "hook-secret",
            ] {
                assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
            }
        }
        assert!(format!("{config:?}").contains("0.0.0.0:2333"));
    }

    #[test]
    fn the_log_retention_window_is_read_from_the_file_or_defaulted() {
        let config: Config = serde_yaml::from_str("password: secret\nlogRetentionDays: 3\n")
            .expect("the file parses");
        assert_eq!(config.log_retention_days, 3);
        let config: Config = serde_yaml::from_str("password: secret\n").unwrap();
        assert_eq!(config.log_retention_days, 14, "an unset window is two weeks");
    }

    #[test]
    fn empty_and_comment_only_configs_use_defaults() {
        for empty in ["", "   \n", "\u{feff}# comment only\n", "---\n"] {
            let config = parse_configuration(empty).expect("empty/comment config parses");
            assert_eq!(config.address, "0.0.0.0:2333");
            assert!(config.password.is_empty());
        }
    }

    #[test]
    fn config_files_larger_than_one_mib_are_refused() {
        let path = std::env::temp_dir()
            .join(format!("rustlink-oversized-config-{}.yaml", std::process::id()));
        let mut oversized = String::from("password: secret\n# ");
        oversized.push_str(&"a".repeat((MAX_CONFIG_BYTES as usize) + 64));
        std::fs::write(&path, oversized).unwrap();
        let err = load(Some(&path)).expect_err("config > 1 MiB must fail");
        let _ = std::fs::remove_file(&path);
        assert!(err.to_string().contains("exceeds 1 MiB"), "{err}");
    }
}
