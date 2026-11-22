//! The node process: its parameters, the helpers it runs beside, the subsystems
//! they configure, and the runtime around all of it.
//!
//! Nothing here decides how the node behaves; that lives in the crates. This is
//! the file that reads a configuration, puts the parts together and keeps the
//! process alive until the operator stops it.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod linux;
mod logging;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::LevelFilter;
use rustlink_config::Config;
use rustlink_server::{Dependencies, Engine, Loader, Playback, Server};

fn main() -> std::process::ExitCode {
    // The arena cap has to be in place while the process is still single
    // threaded, so it is the first thing that happens, before the runtime, the
    // helper threads or anything else allocates from a second thread.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    linux::tune_memory();
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => logging::fatal("Rustlink stopped", &error),
    }
}

/// Rustlink's command-line options.
#[derive(Debug)]
struct Arguments {
    config: Option<PathBuf>,
    version: bool,
    check: bool,
    help: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Arguments, String> {
        let mut parsed = Arguments { config: None, version: false, check: false, help: false };
        let mut arguments = arguments;
        while let Some(argument) = arguments.next() {
            let (name, inline) = match argument.split_once('=') {
                Some((name, value)) => (name.to_string(), Some(value.to_string())),
                None => (argument, None),
            };
            match name.as_str() {
                "--config" | "-c" => {
                    let value = match inline {
                        Some(value) => value,
                        None => arguments
                            .next()
                            .ok_or_else(|| "--config needs a configuration file".to_string())?,
                    };
                    if value.is_empty() || value.starts_with('-') {
                        return Err("--config needs a configuration file".into());
                    }
                    parsed.config = Some(PathBuf::from(value));
                }
                "--version" | "-V" if inline.is_none() => parsed.version = true,
                "--check" if inline.is_none() => parsed.check = true,
                "--help" | "-h" if inline.is_none() => parsed.help = true,
                other => return Err(format!("unknown option {other}; use --help")),
            }
        }
        Ok(parsed)
    }
}

fn run() -> Result<(), String> {
    let arguments = Arguments::parse(std::env::args().skip(1))?;
    if arguments.help {
        println!("Rustlink — Rust Discord audio service\n\nUsage: rustlink [OPTIONS]\n\n  -c, --config PATH  Read this configuration file\n      --check        Validate configuration and media helpers, then exit\n  -V, --version      Print server and API versions\n  -h, --help         Show this help\n\nWithout a configuration, the first start writes config.yaml with a random password.\nVoice: Discord gateway v8, DAVE v1, Opus.");
        return Ok(());
    }
    if arguments.version {
        println!(
            "Rustlink {} (Rustlink API {})",
            rustlink_server::VERSION,
            rustlink_server::APIVERSION
        );
        return Ok(());
    }
    let mut path = rustlink_config::discover(arguments.config.as_deref());
    // A node that was started with nothing beside it and no password in the
    // environment is the double-click case. It is given a configuration rather
    // than refused, because a node with no password cannot start and the window
    // it would say so in closes with the process.
    let mut starter = None;
    if path.is_none() && arguments.config.is_none() && !password_from_environment() {
        starter = write_starter();
        path = starter.as_ref().map(|written| written.path.clone());
    }
    let mut config = rustlink_config::load(path.as_deref()).map_err(|error| error.to_string())?;
    let format = logging::Format::parse(&config.log_format);
    let color = logging::Color::parse(&config.log_color);
    // The wordmark is for a person watching a terminal, so it stays out of a
    // JSON stream that something else is parsing.
    if !arguments.check && format != logging::Format::Json {
        print!(
            "{}",
            logging::banner(color.allowed(), rustlink_server::VERSION, rustlink_server::APIVERSION)
        );
    }
    if let Some(starter) = &starter {
        if format != logging::Format::Json {
            println!("no configuration file was found, so one was written:");
            println!("  {}", starter.path.display());
            println!("the password for clients is {}", starter.password);
            println!("change it if this node is reachable from anywhere else");
            println!();
        }
    }
    let logger = logging::install(&logging::Options {
        level: level(&config.log_level),
        format,
        color,
        directory: config.log_directory.clone(),
        retention: config.log_retention_days,
    })?;
    if let Some(file) = logger.path() {
        log::info!("logging to file path={file} directory={}", config.log_directory);
    }
    // Helpers are resolved before anything starts: a copy in the tools
    // directory is used as it is, and a missing one is installed there, so a
    // fresh checkout needs no preparation step.
    let resolver = tools::Resolver::new(&config.tools_directory, config.auto_download);
    // FFmpeg is the fall back for the formats the in-process pipeline does not
    // take, so a node without it still plays everything else. Losing it costs
    // those formats rather than the node, which is why this is a warning.
    match resolve_ffmpeg(&resolver, &config) {
        Ok(Some(ffmpeg)) => {
            config.ffmpeg = ffmpeg;
            log::debug!("media helper ready ffmpeg={}", config.ffmpeg);
        }
        Ok(None) => {
            // Not wanted at all, which is a choice rather than a failure.
            config.ffmpeg = String::new();
            log::info!("ffmpeg disabled; tracks in the formats the in-process pipeline does not take will fail to load");
        }
        Err(error) => {
            config.ffmpeg = String::new();
            log::warn!("ffmpeg unavailable; tracks in the formats the in-process pipeline does not take will fail to load error={error}");
        }
    }
    if arguments.check {
        // Resolving the helpers above can leave the configuration in a state the
        // server would refuse, so it is checked again here rather than reported
        // as valid on the strength of the check that ran when it was loaded.
        rustlink_config::validate(&config)
            .map_err(|error| format!("configuration is not valid: {error}"))?;
        log::info!(
            "configuration valid config={} sources={} logLevel={} logFormat={} logColor={} logDirectory={}",
            path.map(|path| path.display().to_string()).unwrap_or_else(|| "defaults".into()),
            source_summary(&config),
            config.log_level,
            config.log_format,
            config.log_color,
            config.log_directory,
        );
        resolver.close();
        return Ok(());
    }
    log_startup(&config, path.as_deref());
    let sources = Arc::new(rustlink_source::Service::new(config.clone()));
    let engine = rustlink_audio::Engine {
        ffmpeg: Some(config.ffmpeg.clone()).filter(|program| !program.trim().is_empty()),
        in_process: config.in_process_audio,
        bitrate: config.opus_bitrate as i32,
        complexity: config.opus_complexity as i32,
    };
    let playback: Arc<dyn Engine> =
        Arc::new(Playback { engine, source: Arc::clone(&sources) as Arc<dyn Loader> });
    // Lyrics come from a public service, so the node only calls one when the
    // configuration names it. The catalogue is a YouTube client, so it goes
    // through the same guarded agent the sources use.
    let lyrics =
        rustlink_lyrics::Manager::from_config(&config, sources.agent().clone()).map(Arc::new);
    // A configured webhook gets its worker before the server starts, so it is
    // ready for the first event.
    let webhook = rustlink_server::webhook::Client::new(
        &config.webhook,
        Arc::new(|message: &str, error: &str| logging::fatal(message, error)),
    );
    let deps = Dependencies {
        loader: Arc::clone(&sources) as Arc<dyn Loader>,
        engine: playback,
        voice: Arc::new(rustlink_voice::Discord),
        lyrics,
        webhook,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // The name is what an operator reading `top -H` or `ps -L` on a busy
        // server has to tell the node's threads from everything else's.
        .thread_name("rustlink")
        .enable_all()
        .build()
        .map_err(|error| format!("runtime: {error}"))?;
    let result = runtime.block_on(async {
        let server = Server::new(config.clone(), deps).map_err(|error| error.to_string())?;
        server
            .clone()
            .listen(&config.address, interrupt())
            .await
            .map_err(|error| format!("could not listen on {}: {error}", config.address))?;
        // Sessions and players end before the process does, so a client that is
        // watching sees the tracks stop rather than the socket vanish.
        server.close();
        Ok(())
    });
    resolver.close();
    log::info!("Rustlink stopped");
    result
}

/// Resolves when the operator stops the node, which is a keyboard interrupt
/// everywhere and a termination signal on the platforms that have one.
async fn interrupt() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                log::info!("shutdown signal received");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    log::info!("shutdown signal received");
}

/// The configuration a node writes for itself when it was started without one.
struct Starter {
    path: PathBuf,
    password: String,
}

/// Whether a password was handed to the node through the environment, which is
/// how a container is configured and why one never needs a file.
fn password_from_environment() -> bool {
    std::env::var("RUSTLINK_PASSWORD").map(|value| !value.trim().is_empty()).unwrap_or(false)
}

/// Writes the configuration a first start runs with.
///
/// A file that cannot be written is not fatal on its own: the node falls back
/// to its defaults and the environment, and the validation that follows names
/// the password it is missing.
fn write_starter() -> Option<Starter> {
    let Some(password) = random_password() else {
        println!("no configuration file was found, and no password could be generated");
        return None;
    };
    let path = PathBuf::from(rustlink_config::STARTER_FILE);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&path).and_then(|mut file| {
        use std::io::Write;
        file.write_all(rustlink_config::starter(&password).as_bytes())
    });
    match written {
        Ok(()) => {
            // The node is normally double-clicked, so the message names the file
            // in full rather than relative to a directory nobody can see.
            let shown = std::env::current_dir()
                .map(|directory| directory.join(&path))
                .unwrap_or_else(|_| path.clone());
            Some(Starter { path: shown, password })
        }
        Err(error) => {
            println!(
                "no configuration file was found, and one could not be written to {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Sixteen random bytes as hexadecimal, which is the password a node that
/// configured itself hands out.
///
/// Nothing is returned when the system source cannot be read: a node whose
/// password can be guessed is worse than one that has to be configured by hand.
fn random_password() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut password = String::with_capacity(32);
    for byte in bytes {
        password.push_str(&format!("{byte:02x}"));
    }
    Some(password)
}

/// Reports a helper the configuration has switched off. Naming none is how a
/// setting says it wants no such helper, which is a different thing from naming
/// one that cannot be found, and the two are reported differently.
fn is_none(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("none")
}

/// Source resolution is native. Only the optional FFmpeg codec helper needs
/// locating or installing before the server starts.
fn resolve_ffmpeg(resolver: &tools::Resolver, config: &Config) -> Result<Option<String>, String> {
    if is_none(&config.ffmpeg) {
        Ok(None)
    } else {
        resolver.resolve("ffmpeg", &config.ffmpeg).map(Some)
    }
}

/// Writes the startup summary one setting per log record so text, JSON and
/// daily file output all remain valid one-line records.
fn log_startup(config: &Config, path: Option<&Path>) {
    log::info!("Rustlink starting");
    for field in startup_fields(config, path) {
        log::info!("  {field}");
    }
}

/// The startup settings in a stable order for operators and log collectors.
fn startup_fields(config: &Config, path: Option<&Path>) -> Vec<String> {
    vec![
        format!("version={}", rustlink_server::VERSION),
        format!("apiVersion={}", rustlink_server::APIVERSION),
        format!(
            "config={}",
            path.map(|path| path.display().to_string()).unwrap_or_else(|| "defaults".into())
        ),
        format!("sources={}", source_summary(config)),
        format!("toolsDirectory={}", config.tools_directory),
        format!("autoDownload={}", config.auto_download),
        format!("maxPlayers={}", config.max_players),
        format!("maxSessions={}", config.max_sessions),
        format!("maxConcurrentLoads={}", config.max_concurrent_loads),
        format!("cacheEntries={}", config.cache_entries),
        format!("loadTimeoutSeconds={}", config.load_timeout_seconds),
        format!("eventBuffer={}", config.event_buffer),
        format!("opusBitrate={}", config.opus_bitrate),
        format!("opusComplexity={}", config.opus_complexity),
        format!("allowPrivateNetworks={}", config.allow_private_networks),
        format!("ratelimits={}", config.ratelimits.enable),
        format!("webhook={}", config.webhook.enable),
    ]
}

/// Which source paths are active, for the startup summary.
fn source_summary(config: &Config) -> String {
    let mut active = Vec::new();
    if config.youtube_native {
        active.push("youtube(native)");
    }
    if config.soundcloud_native {
        active.push("soundcloud(native)");
    }
    if !config.spotify_client_id.is_empty() {
        active.push("spotify");
    }
    if !config.spotify_refresh_token.is_empty() {
        active.push("spotify(user-token)");
    }
    active.push("deezer");
    active.push("http");
    active.join(",")
}

/// Reads the configured level. The configuration is validated when it is
/// loaded, so anything unexpected falls back to info.
fn level(name: &str) -> LevelFilter {
    match name.trim().to_ascii_lowercase().as_str() {
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(line: &str) -> Result<Arguments, String> {
        Arguments::parse(line.split_whitespace().map(str::to_string))
    }

    #[test]
    fn a_flag_is_read_with_or_without_an_equals_sign() {
        assert_eq!(arguments("-c node.yaml").unwrap().config, Some(PathBuf::from("node.yaml")));
        assert_eq!(
            arguments("--config=node.yaml").unwrap().config,
            Some(PathBuf::from("node.yaml"))
        );
        assert_eq!(
            arguments("--config node.yaml").unwrap().config,
            Some(PathBuf::from("node.yaml"))
        );
    }

    #[test]
    fn the_switches_that_take_no_value_are_read_as_present() {
        let parsed = arguments("--version --check").unwrap();
        assert!(parsed.version);
        assert!(parsed.check);
        assert!(parsed.config.is_none());
    }

    #[test]
    fn a_flag_that_is_not_known_is_refused_rather_than_ignored() {
        assert!(arguments("--verbose").unwrap_err().contains("unknown option --verbose"));
        assert!(arguments("--config").unwrap_err().contains("--config needs"));
        assert!(arguments("--config --check").is_err());
        assert!(arguments("--check=false").is_err());
        assert!(arguments("config file.yaml").is_err());
        assert!(arguments("--help").unwrap().help);
    }

    #[test]
    fn a_helper_named_none_is_switched_off() {
        assert!(is_none("none"));
        assert!(is_none(" NONE "));
        assert!(!is_none("ffmpeg"));
    }

    #[test]
    fn a_generated_password_is_thirty_two_hexadecimal_characters() {
        let password = random_password().expect("the system source answers");
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|character| character.is_ascii_hexdigit()));
    }

    #[test]
    fn two_generated_passwords_are_not_the_same() {
        assert_ne!(random_password(), random_password());
    }

    #[test]
    fn the_startup_summary_names_the_sources_that_are_on() {
        let config = Config::default();
        let summary = source_summary(&config);
        assert!(summary.contains("youtube(native)"));
        assert!(summary.contains("soundcloud(native)"));
        assert!(summary.ends_with("deezer,http"));
    }

    #[test]
    fn startup_fields_are_one_setting_per_line_in_a_stable_order() {
        let config = Config::default();
        let fields = startup_fields(&config, Some(Path::new("config.yaml")));
        assert_eq!(fields.len(), 17);
        assert_eq!(fields[0], format!("version={}", rustlink_server::VERSION));
        assert_eq!(fields[1], format!("apiVersion={}", rustlink_server::APIVERSION));
        assert_eq!(fields[2], "config=config.yaml");
        assert_eq!(fields[3], format!("sources={}", source_summary(&config)));
        assert_eq!(fields.last().unwrap(), &format!("webhook={}", config.webhook.enable));
        assert!(fields
            .iter()
            .all(|field| !field.chars().any(|character| character == '\r' || character == '\n')));
    }

    #[test]
    fn the_level_names_the_validator_accepts_are_the_ones_read_here() {
        assert_eq!(level("debug"), LevelFilter::Debug);
        assert_eq!(level("WARN"), LevelFilter::Warn);
        assert_eq!(level("error"), LevelFilter::Error);
        assert_eq!(level(""), LevelFilter::Info);
    }

    /// A tools directory holding a stub for the helper, so the lookup answers
    /// without touching the network.
    fn stub_tools() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("rustlink-helpers-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(directory.join(file), b"stub").unwrap();
        directory
    }

    #[test]
    fn ffmpeg_is_resolved_from_the_tools_directory() {
        let directory = stub_tools();
        let config = Config {
            tools_directory: directory.display().to_string(),
            auto_download: false,
            ffmpeg: "ffmpeg".into(),
            ..Config::default()
        };
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        let ffmpeg =
            resolve_ffmpeg(&resolver, &config).unwrap().expect("ffmpeg is in the tools directory");
        assert!(ffmpeg.ends_with(&format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)), "{ffmpeg}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_helper_that_is_switched_off_is_not_looked_for() {
        let config = Config { ffmpeg: "none".into(), ..Config::default() };
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        assert!(resolve_ffmpeg(&resolver, &config).unwrap().is_none());
    }

    #[test]
    fn a_helper_that_is_missing_is_reported() {
        let directory =
            std::env::temp_dir().join(format!("rustlink-no-tools-{}", std::process::id()));
        let config = Config {
            tools_directory: directory.display().to_string(),
            auto_download: false,
            ffmpeg: "ffmpeg".into(),
            ..Config::default()
        };
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        assert!(resolve_ffmpeg(&resolver, &config).is_err());
    }
}
