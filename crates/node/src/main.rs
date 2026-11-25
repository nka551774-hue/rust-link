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

use std::ffi::{OsStr, OsString};
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

fn strip_ascii_os_prefix<'a>(value: &'a OsStr, prefix: &str) -> Option<&'a OsStr> {
    debug_assert!(prefix.is_ascii());
    let rest = value.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
    // SAFETY: `prefix` is pure ASCII, which is a valid code-unit boundary in
    // every platform `OsStr` encoding (UTF-8, WTF-8, and raw byte sequences).
    Some(unsafe { OsStr::from_encoded_bytes_unchecked(rest) })
}

impl Arguments {
    fn parse<I, T>(arguments: I) -> Result<Arguments, String>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let mut arguments = arguments.into_iter().map(Into::into);
        let mut parsed = Arguments { config: None, version: false, check: false, help: false };
        while let Some(argument) = arguments.next() {
            let raw = argument.as_os_str();
            if raw == "--config" || raw == "-c" {
                let value = arguments.next().ok_or("--config needs a configuration file")?;
                if value.is_empty() || value.as_encoded_bytes().starts_with(b"-") {
                    return Err("--config needs a configuration file".into());
                }
                parsed.config = Some(PathBuf::from(value));
            } else if let Some(inline) = strip_ascii_os_prefix(raw, "--config=")
                .or_else(|| strip_ascii_os_prefix(raw, "-c="))
            {
                if inline.is_empty() {
                    return Err("--config needs a configuration file".into());
                }
                parsed.config = Some(PathBuf::from(inline));
            } else if raw == "--version" || raw == "-V" {
                parsed.version = true;
            } else if strip_ascii_os_prefix(raw, "--version=").is_some()
                || strip_ascii_os_prefix(raw, "-V=").is_some()
            {
                return Err("--version does not take a value".into());
            } else if raw == "--check" {
                parsed.check = true;
            } else if strip_ascii_os_prefix(raw, "--check=").is_some() {
                return Err("--check does not take a value".into());
            } else if raw == "--help" || raw == "-h" {
                parsed.help = true;
            } else if strip_ascii_os_prefix(raw, "--help=").is_some()
                || strip_ascii_os_prefix(raw, "-h=").is_some()
            {
                return Err("--help does not take a value".into());
            } else {
                let shown = raw.to_string_lossy();
                let name = shown.split('=').next().unwrap_or(&shown);
                return Err(format!("unknown option {name}; use --help"));
            }
        }
        Ok(parsed)
    }
}

fn run() -> Result<(), String> {
    let arguments = Arguments::parse(std::env::args_os().skip(1))?;
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
    if !arguments.check
        && path.is_none()
        && arguments.config.is_none()
        && !password_from_environment()
    {
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
    let log_directory = if arguments.check { String::new() } else { config.log_directory.clone() };
    let logger = logging::install(&logging::Options {
        level: level(&config.log_level),
        format,
        color,
        directory: log_directory,
        retention: config.log_retention_days,
    })?;
    if let Some(file) = logger.path() {
        log::info!("logging to file path={file} directory={}", config.log_directory);
    }
    // Helpers are resolved before anything starts: a copy in the tools
    // directory is used as it is, and a missing one is installed there, so a
    // fresh checkout needs no preparation step.
    let explicit_ffmpeg_path = config.ffmpeg.contains('/') || config.ffmpeg.contains('\\');
    let resolver = tools::Resolver::new(&config.tools_directory, config.auto_download);
    // FFmpeg is the fall back for the formats the in-process pipeline does not
    // take, so a node without it still plays everything else unless either
    // inProcessAudio is off or an explicit FFmpeg path was configured.
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
        Err(error) if !config.in_process_audio || explicit_ffmpeg_path => {
            resolver.close();
            return Err(error);
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
            path.as_deref().unwrap_or_else(|| Path::new("defaults")).display(),
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
        ffmpeg: (!config.ffmpeg.trim().is_empty()).then(|| config.ffmpeg.clone()),
        in_process: config.in_process_audio,
        bitrate: config.opus_bitrate as i32,
        complexity: config.opus_complexity as i32,
    };
    let playback: Arc<dyn Engine> =
        Arc::new(Playback { engine, source: Arc::clone(&sources) as Arc<dyn Loader> });
    // Lyrics come from a public service, so the node only calls one when the
    // configuration names it. YouTube Music requests share the YouTube source
    // agent (including youtubeProxy when configured) and warmed visitorData.
    let lyrics = rustlink_lyrics::Manager::from_config_with_youtube_visitor(
        &config,
        sources.agent().clone(),
        sources.youtube_agent().cloned(),
        Some(sources.youtube_visitor_bridge()),
    )
    .map(Arc::new);
    // A configured webhook gets its worker before the server starts, so it is
    // ready for the first event.
    let webhook = rustlink_server::webhook::Client::with_network_policy(
        &config.webhook,
        config.allow_private_networks,
    );
    if config.webhook.enable && webhook.is_none() {
        log::warn!("webhook is enabled in config, but worker could not be started");
    }
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
    let address = config.address.clone();
    let result = runtime.block_on(async {
        let server = Server::new(config, deps).map_err(|error| error.to_string())?;
        server
            .clone()
            .listen(&address, interrupt())
            .await
            .map_err(|error| format!("could not listen on {address}: {error}"))?;
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
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
                log::info!("shutdown signal received");
                return;
            }
        };
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                if res.is_err() {
                    let _ = terminate.recv().await;
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
    log::info!("shutdown signal received");
}

fn wipe_string(secret: &mut String) {
    // SAFETY: `write_volatile` cannot unwind, and overwriting initialized bytes
    // with `0` (ASCII NUL) preserves UTF-8 validity before `clear()`.
    unsafe {
        for byte in secret.as_bytes_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    secret.clear();
}

/// The configuration a node writes for itself when it was started without one.
struct Starter {
    path: PathBuf,
    password: String,
}

impl Drop for Starter {
    fn drop(&mut self) {
        wipe_string(&mut self.password);
    }
}

/// Whether a password was handed to the node through the environment, which is
/// how a container is configured and why one never needs a file.
fn password_from_environment() -> bool {
    std::env::var("RUSTLINK_PASSWORD").is_ok_and(|mut value| {
        let present = !value.trim().is_empty();
        wipe_string(&mut value);
        present
    })
}

fn write_starter_at(path: &Path, temp: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_file = |target: &Path| -> std::io::Result<()> {
        let mut file = options.open(target)?;
        let result = {
            use std::io::Write;
            file.write_all(contents.as_bytes()).and_then(|()| file.sync_all())
        };
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(target);
        }
        result
    };
    match write_file(temp) {
        Ok(()) => match std::fs::hard_link(temp, path) {
            Ok(()) => {
                let _ = std::fs::remove_file(temp);
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(temp);
                Err(err)
            }
            Err(_) => {
                if path.exists() {
                    let _ = std::fs::remove_file(temp);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "configuration file already exists",
                    ))
                } else if std::fs::rename(temp, path).is_ok() {
                    Ok(())
                } else {
                    let _ = std::fs::remove_file(temp);
                    write_file(path)
                }
            }
        },
        Err(_) => write_file(path),
    }
}

/// Writes the configuration a first start runs with.
///
/// A file that cannot be written is not fatal on its own: the node falls back
/// to its defaults and the environment, and the validation that follows names
/// the password it is missing.
fn write_starter() -> Option<Starter> {
    let Some(mut password) = random_password() else {
        println!("no configuration file was found, and no password could be generated");
        return None;
    };
    let path = PathBuf::from(rustlink_config::STARTER_FILE);
    let mut contents = rustlink_config::starter(&password);
    let mut nonce = [0u8; 8];
    let _ = getrandom::fill(&mut nonce);
    let temp = PathBuf::from(format!(
        ".{}.tmp-{}",
        rustlink_config::STARTER_FILE,
        tools::hex_encode(&nonce)
    ));
    let written = write_starter_at(&path, &temp, &contents);
    wipe_string(&mut contents);
    match written {
        Ok(()) => {
            // The node is normally double-clicked, so the message names the file
            // in full rather than relative to a directory nobody can see.
            let shown =
                std::env::current_dir().map(|directory| directory.join(&path)).unwrap_or(path);
            Some(Starter { path: shown, password })
        }
        Err(error) => {
            wipe_string(&mut password);
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
    let password = tools::hex_encode(&bytes);
    for byte in &mut bytes {
        // SAFETY: `byte` is an initialized stack byte.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
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
        match path {
            Some(path) => format!("config={}", path.display()),
            None => "config=defaults".into(),
        },
        format!("sources={}", source_summary(config)),
        format!("inProcessAudio={}", config.in_process_audio),
        format!(
            "ffmpeg={}",
            if config.ffmpeg.is_empty() { "disabled" } else { config.ffmpeg.as_str() }
        ),
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
    let mut active = Vec::with_capacity(6);
    if config.youtube_native {
        active.push("youtube(native)");
    }
    if config.soundcloud_native {
        active.push("soundcloud(native)");
    }
    if config.spotify_enabled() {
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
    let name = name.trim();
    if name.eq_ignore_ascii_case("debug") {
        LevelFilter::Debug
    } else if name.eq_ignore_ascii_case("warn") {
        LevelFilter::Warn
    } else if name.eq_ignore_ascii_case("error") {
        LevelFilter::Error
    } else {
        LevelFilter::Info
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
        assert_eq!(fields.len(), 19);
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
        let mut config = Config::default();
        config.tools_directory = directory.display().to_string();
        config.auto_download = false;
        config.ffmpeg = "ffmpeg".into();
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        let ffmpeg =
            resolve_ffmpeg(&resolver, &config).unwrap().expect("ffmpeg is in the tools directory");
        assert!(ffmpeg.ends_with(&format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)), "{ffmpeg}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_helper_that_is_switched_off_is_not_looked_for() {
        let mut config = Config::default();
        config.ffmpeg = "none".into();
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        assert!(resolve_ffmpeg(&resolver, &config).unwrap().is_none());
    }

    #[test]
    fn a_helper_that_is_missing_is_reported() {
        let directory =
            std::env::temp_dir().join(format!("rustlink-no-tools-{}", std::process::id()));
        let mut config = Config::default();
        config.tools_directory = directory.display().to_string();
        config.auto_download = false;
        config.ffmpeg = "ffmpeg".into();
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        assert!(resolve_ffmpeg(&resolver, &config).is_err());
    }

    #[test]
    fn inline_config_supports_dash_prefixed_files_and_check_value_reports_clear_error() {
        assert_eq!(
            arguments("--config=-file.yaml").unwrap().config,
            Some(PathBuf::from("-file.yaml"))
        );
        assert_eq!(arguments("-c=-file.yaml").unwrap().config, Some(PathBuf::from("-file.yaml")));
        let err = arguments("--check=true").unwrap_err();
        assert_eq!(err, "--check does not take a value");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(vec![b'-', b'c', 0xff, b'.', b'y', b'm', b'l']);
            let parsed = Arguments::parse([OsString::from("--config"), raw.clone()])
                .expect("non-UTF-8 path");
            assert_eq!(parsed.config, Some(PathBuf::from(raw)));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let unpaired = OsString::from_wide(&[0xd800, u16::from(b'.'), u16::from(b'y')]);
            let parsed = Arguments::parse([OsString::from("--config"), unpaired.clone()])
                .expect("unpaired UTF-16 path");
            assert_eq!(parsed.config, Some(PathBuf::from(unpaired)));
        }
    }

    #[test]
    fn starter_writes_atomically_preserves_existing_file_and_wipes_password_on_drop() {
        let directory =
            std::env::temp_dir().join(format!("rustlink-starter-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("config.yaml");
        let temp = directory.join(".config.yaml.tmp-1");
        write_starter_at(&target, &temp, "password: \"abc\"\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "password: \"abc\"\n");
        assert!(!temp.exists());
        assert!(write_starter_at(&target, &temp, "password: \"overwrite\"\n").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "password: \"abc\"\n");
        let _ = std::fs::remove_dir_all(&directory);

        let mut text = String::from("super-secret-starter-password");
        wipe_string(&mut text);
        assert!(text.is_empty());
    }
}
