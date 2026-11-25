//! Node configuration: strict YAML/JSON loading, environment overrides, and validation.

mod env;
mod validate;

pub use env::apply_env;
pub use validate::validate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

macro_rules! define_config {
    ($(
        $(#[$doc:meta])*
        [$kind:ident $(, $dfn:ident = $dstr:tt)?] $name:ident : $ty:ty = $init:expr;
    )*) => {
        $(define_config!(@dfn ($ty) ($init) $($dfn)?);)*

        /// Node configuration. `Debug` redacts credentials and `Drop` wipes secret buffers.
        #[derive(Clone, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct Config {
            $(
                $(#[$doc])*
                #[serde(default $(= $dstr)?)]
                pub $name: $ty,
            )*
        }

        impl Default for Config {
            fn default() -> Self {
                Self { $($name: $init,)* }
            }
        }

        impl Drop for Config {
            fn drop(&mut self) {
                $(define_config!(@drop $kind self.$name);)*
            }
        }

        impl std::fmt::Debug for Config {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("Config")
                    $(.field(stringify!($name), define_config!(@debug $kind self.$name)))*
                    .finish()
            }
        }
    };
    (@dfn ($ty:ty) ($init:expr)) => {};
    (@dfn ($ty:ty) ($init:expr) $dfn:ident) => {
        fn $dfn() -> $ty { $init }
    };
    (@drop plain $field:expr) => {};
    (@drop wipe $field:expr) => { wipe_string(&mut $field); };
    (@drop secret $field:expr) => { wipe_string(&mut $field); };
    (@debug plain $field:expr) => { &$field };
    (@debug wipe $field:expr) => { &$field };
    (@debug secret $field:expr) => { &redacted(&$field) };
}

define_config! {
    [plain, d_address = "d_address"] address: String = "0.0.0.0:2333".into();
    [secret] password: String = String::new();
    [plain, d_ffmpeg = "d_ffmpeg"] ffmpeg: String = "ffmpeg".into();
    [plain, d_tools = "d_tools"] tools_directory: String = ".tools".into();
    [plain] auto_download: bool = false;
    [plain, d_in_proc = "d_in_proc"] in_process_audio: bool = true;
    [plain, d_lyrics = "d_lyrics"] lyrics_source: String = "lrclib".into();
    [plain, d_log_level = "d_log_level"] log_level: String = "info".into();
    [plain, d_log_format = "d_log_format"] log_format: String = "text".into();
    [plain, d_log_color = "d_log_color"] log_color: String = "auto".into();
    [plain, d_log_dir = "d_log_dir"] log_directory: String = "logs".into();
    [plain, d_log_ret = "d_log_ret"] log_retention_days: u32 = 14;
    [plain, d_yt_native = "d_yt_native"] youtube_native: bool = true;
    [plain, d_yt_ep = "d_yt_ep"] youtube_endpoint: String = "https://youtubei.googleapis.com".into();
    [plain, d_ytm_ep = "d_ytm_ep"] youtube_music_endpoint: String = "https://music.youtube.com".into();
    [plain] youtube_client_version: String = String::new();
    [secret] youtube_po_token: String = String::new();
    [secret] youtube_visitor_data: String = String::new();
    [secret] youtube_proxy: String = String::new();
    [plain, d_sc_native = "d_sc_native"] soundcloud_native: bool = true;
    [plain, d_sc_api = "d_sc_api"] soundcloud_api_endpoint: String = "https://api-v2.soundcloud.com".into();
    [plain, d_sc_web = "d_sc_web"] soundcloud_web_endpoint: String = "https://soundcloud.com".into();
    [secret] soundcloud_client_id: String = String::new();
    [plain, d_sc_ttl = "d_sc_ttl"] soundcloud_client_id_ttl_seconds: u32 = 21_600;
    [wipe] spotify_client_id: String = String::new();
    [secret] spotify_client_secret: String = String::new();
    [secret] spotify_refresh_token: String = String::new();
    [plain, d_sp_market = "d_sp_market"] spotify_market: String = "US".into();
    [plain, d_dz_api = "d_dz_api"] deezer_api_endpoint: String = "https://api.deezer.com".into();
    [plain] filters: BTreeMap<String, bool> = BTreeMap::new();
    [plain] allow_private_networks: bool = false;
    [plain, d_max_players = "d_max_players"] max_players: u32 = 100;
    [plain, d_max_sessions = "d_max_sessions"] max_sessions: u32 = 100;
    [plain, d_max_loads = "d_max_loads"] max_concurrent_loads: u32 = 8;
    [plain, d_max_playlist = "d_max_playlist"] max_playlist_tracks: u32 = 100;
    [plain, d_search = "d_search"] search_results: u32 = 5;
    [plain, d_cache = "d_cache"] cache_entries: u32 = 1024;
    [plain, d_cache_ttl = "d_cache_ttl"] cache_ttl_seconds: u32 = 300;
    [plain, d_load_to = "d_load_to"] load_timeout_seconds: u32 = 30;
    [plain, d_update_s = "d_update_s"] player_update_seconds: u32 = 5;
    [plain, d_stats_s = "d_stats_s"] stats_seconds: u32 = 60;
    [plain, d_events = "d_events"] event_buffer: u32 = 256;
    [plain, d_bitrate = "d_bitrate"] opus_bitrate: u32 = 192_000;
    [plain, d_complexity = "d_complexity"] opus_complexity: u32 = 10;
    [plain] ratelimits: RateLimit = RateLimit::default();
    [plain] webhook: Webhook = Webhook::default();
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct RateLimit {
    pub enable: bool,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self { enable: true }
    }
}

const fn d_webhook_latency() -> u32 {
    5
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Webhook {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default = "d_webhook_latency")]
    pub state_latency_seconds: u32,
}

impl Default for Webhook {
    fn default() -> Self {
        Self { enable: false, url: String::new(), state_latency_seconds: 5 }
    }
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

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` points to an initialized byte in `bytes`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

fn wipe_string(secret: &mut String) {
    // SAFETY: Overwriting UTF-8 bytes with ASCII NUL (0) preserves UTF-8 validity before `clear()`.
    unsafe { wipe_bytes(secret.as_bytes_mut()) };
    secret.clear();
}

#[inline]
const fn redacted(value: &str) -> &'static str {
    if value.is_empty() {
        ""
    } else {
        "<redacted>"
    }
}

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
pub const CANDIDATES: [&str; 3] = ["config.yaml", "config.yml", "config.json"];
pub const STARTER_FILE: &str = "config.yaml";

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

    #[inline]
    pub fn filter_enabled(&self, name: &str) -> bool {
        self.filters.get(name).copied().unwrap_or(true)
    }
}

pub fn discover(explicit: Option<&Path>) -> Option<PathBuf> {
    explicit.map(Path::to_path_buf).or_else(|| {
        CANDIDATES.into_iter().map(Path::new).find(|p| p.is_file()).map(Path::to_path_buf)
    })
}

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
    let read_err = |source| ConfigError::Read { path: display.clone(), source };
    let too_large =
        || ConfigError::Invalid(format!("configuration file {display} exceeds 1 MiB limit"));
    let file = std::fs::File::open(path).map_err(read_err)?;
    if file.metadata().is_ok_and(|m| m.len() > MAX_CONFIG_BYTES) {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes).map_err(read_err)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        wipe_bytes(&mut bytes);
        return Err(too_large());
    }
    String::from_utf8(bytes).map_err(|err| {
        let mut raw = err.into_bytes();
        wipe_bytes(&mut raw);
        ConfigError::Read {
            path: display,
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "configuration file is not valid UTF-8",
            ),
        }
    })
}

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
    serde_yaml::from_str::<Option<Config>>(text.strip_prefix('\u{feff}').unwrap_or(text))
        .map(Option::unwrap_or_default)
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
        assert_eq!((defaults.opus_bitrate, defaults.opus_complexity), (192_000, 10));
        let explicit =
            parse_configuration("password: secret\nopusBitrate: 128000\nopusComplexity: 4\n")
                .unwrap();
        assert_eq!((explicit.opus_bitrate, explicit.opus_complexity), (128_000, 4));
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
        let config: Config = serde_yaml::from_str(&starter("4f2a9c")).unwrap();
        assert_eq!(config.password, "4f2a9c");
        assert_eq!(config.address, "0.0.0.0:2333");
        assert!(config.auto_download);
    }

    #[test]
    fn a_password_of_only_digits_survives_the_round_trip() {
        for pw in ["12345678", "p\"a\\s#s:word"] {
            let config: Config = serde_yaml::from_str(&starter(pw)).unwrap();
            assert_eq!(config.password, pw);
        }
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
        let config: Config =
            serde_yaml::from_str("password: secret\nlogRetentionDays: 3\n").unwrap();
        assert_eq!(config.log_retention_days, 3);
        let config: Config = serde_yaml::from_str("password: secret\n").unwrap();
        assert_eq!(config.log_retention_days, 14);
    }

    #[test]
    fn empty_and_comment_only_configs_use_defaults() {
        for empty in ["", "   \n", "\u{feff}# comment only\n", "---\n"] {
            let config = parse_configuration(empty).unwrap();
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
        let err = load(Some(&path)).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(err.to_string().contains("exceeds 1 MiB"), "{err}");
    }
}
