//! Node entrypoint: CLI options, config discovery, helper resolution, and runtime lifecycle.

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
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    linux::tune_memory();
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => logging::fatal("Rustlink stopped", &error),
    }
}

#[derive(Debug, Default)]
struct Arguments {
    config: Option<PathBuf>,
    version: bool,
    check: bool,
    help: bool,
}

fn strip_ascii_os_prefix<'a>(value: &'a OsStr, prefix: &str) -> Option<&'a OsStr> {
    debug_assert!(prefix.is_ascii());
    let rest = value.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
    // SAFETY: `prefix` is pure ASCII, preserving valid OsStr code-unit boundaries on all platforms.
    Some(unsafe { OsStr::from_encoded_bytes_unchecked(rest) })
}

impl Arguments {
    fn parse<I, T>(arguments: I) -> Result<Arguments, String>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let mut arguments = arguments.into_iter().map(Into::into);
        let mut parsed = Arguments::default();
        while let Some(argument) = arguments.next() {
            let raw = argument.as_os_str();
            if raw == "--config" || raw == "-c" {
                let value = arguments
                    .next()
                    .filter(|v| !v.is_empty() && !v.as_encoded_bytes().starts_with(b"-"))
                    .ok_or("--config needs a configuration file")?;
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
            } else if raw == "--check" {
                parsed.check = true;
            } else if raw == "--help" || raw == "-h" {
                parsed.help = true;
            } else {
                let shown = raw.to_string_lossy();
                let name = shown.split('=').next().unwrap_or(&shown);
                return match name {
                    "--version" | "-V" => Err("--version does not take a value".into()),
                    "--check" => Err("--check does not take a value".into()),
                    "--help" | "-h" => Err("--help does not take a value".into()),
                    _ => Err(format!("unknown option {name}; use --help")),
                };
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
    let mut starter = None;
    if !arguments.check
        && path.is_none()
        && arguments.config.is_none()
        && !password_from_environment()
    {
        starter = write_starter();
        path = starter.as_ref().map(|written| written.path.clone());
    }
    let mut config = rustlink_config::load(path.as_deref()).map_err(|e| e.to_string())?;
    let format = logging::Format::parse(&config.log_format);
    let color = logging::Color::parse(&config.log_color);
    if !arguments.check && format != logging::Format::Json {
        print!(
            "{}",
            logging::banner(color.allowed(), rustlink_server::VERSION, rustlink_server::APIVERSION)
        );
        if let Some(starter) = &starter {
            println!(
                "no configuration file was found, so one was written:\n  {}\nthe password for clients is {}\nchange it if this node is reachable from anywhere else\n",
                starter.path.display(),
                starter.password
            );
        }
    }
    let logger = logging::install(&logging::Options {
        level: level(&config.log_level),
        format,
        color,
        directory: if arguments.check { String::new() } else { config.log_directory.clone() },
        retention: config.log_retention_days,
    })?;
    if let Some(file) = logger.path() {
        log::info!("logging to file path={file} directory={}", config.log_directory);
    }
    let explicit_ffmpeg_path = config.ffmpeg.contains('/') || config.ffmpeg.contains('\\');
    let resolver = tools::Resolver::new(&config.tools_directory, config.auto_download);
    match resolve_ffmpeg(&resolver, &config) {
        Ok(Some(ffmpeg)) => {
            config.ffmpeg = ffmpeg;
            log::debug!("media helper ready ffmpeg={}", config.ffmpeg);
        }
        Ok(None) => {
            config.ffmpeg.clear();
            log::info!("ffmpeg disabled; tracks in the formats the in-process pipeline does not take will fail to load");
        }
        Err(error) if !config.in_process_audio || explicit_ffmpeg_path => {
            resolver.close();
            return Err(error);
        }
        Err(error) => {
            config.ffmpeg.clear();
            log::warn!("ffmpeg unavailable; tracks in the formats the in-process pipeline does not take will fail to load error={error}");
        }
    }
    if arguments.check {
        rustlink_config::validate(&config)
            .map_err(|e| format!("configuration is not valid: {e}"))?;
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
    ensure_potoken_starter(&config);
    let sources = Arc::new(rustlink_source::Service::new(config.clone()));
    let engine = rustlink_audio::Engine {
        ffmpeg: (!config.ffmpeg.trim().is_empty()).then(|| config.ffmpeg.clone()),
        in_process: config.in_process_audio,
        bitrate: config.opus_bitrate as i32,
        complexity: config.opus_complexity as i32,
    };
    let playback: Arc<dyn Engine> =
        Arc::new(Playback { engine, source: Arc::clone(&sources) as Arc<dyn Loader> });
    let lyrics = rustlink_lyrics::Manager::from_config_with_youtube_auth(
        &config,
        sources.agent().clone(),
        sources.youtube_agent().cloned(),
        Some(sources.youtube_visitor_bridge()),
        Some(sources.youtube_po_token_reader()),
    )
    .map(Arc::new);
    let webhook = rustlink_server::webhook::Client::with_network_policy(
        &config.webhook,
        config.allow_private_networks,
    );
    if config.webhook.enable && webhook.is_none() {
        log::warn!("webhook is enabled in config, but worker could not be started");
    }
    let deps = Dependencies {
        loader: sources,
        engine: playback,
        voice: Arc::new(rustlink_voice::Discord),
        lyrics,
        webhook,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("rustlink")
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let address = config.address.clone();
    let result = runtime.block_on(async {
        let server = Server::new(config, deps).map_err(|e| e.to_string())?;
        server
            .clone()
            .listen(&address, interrupt())
            .await
            .map_err(|e| format!("could not listen on {address}: {e}"))?;
        server.close();
        Ok(())
    });
    resolver.close();
    log::info!("Rustlink stopped");
    result
}

async fn interrupt() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                res = tokio::signal::ctrl_c() => {
                    if res.is_err() {
                        let _ = terminate.recv().await;
                    }
                }
                _ = terminate.recv() => {}
            }
        } else if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
    log::info!("shutdown signal received");
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is an initialized mutable byte.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

fn wipe_string(secret: &mut String) {
    // SAFETY: overwriting initialized bytes with ASCII NUL preserves UTF-8 validity before `clear()`.
    unsafe { wipe_bytes(secret.as_bytes_mut()) };
    secret.clear();
}

struct Starter {
    path: PathBuf,
    password: String,
}

impl Drop for Starter {
    fn drop(&mut self) {
        wipe_string(&mut self.password);
    }
}

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
        let res = std::io::Write::write_all(&mut file, contents.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if res.is_err() {
            let _ = std::fs::remove_file(target);
        }
        res
    };
    if write_file(temp).is_err() {
        return write_file(path);
    }
    let result = match std::fs::hard_link(temp, path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(err),
        Err(_) if path.exists() => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "configuration file already exists",
        )),
        Err(_) => std::fs::rename(temp, path).or_else(|_| {
            let _ = std::fs::remove_file(temp);
            write_file(path)
        }),
    };
    let _ = std::fs::remove_file(temp);
    result
}

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
            let shown = std::env::current_dir().map(|dir| dir.join(&path)).unwrap_or(path);
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

fn random_password() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let password = tools::hex_encode(&bytes);
    wipe_bytes(&mut bytes);
    Some(password)
}

fn is_none(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("none")
}

fn resolve_ffmpeg(resolver: &tools::Resolver, config: &Config) -> Result<Option<String>, String> {
    if is_none(&config.ffmpeg) {
        Ok(None)
    } else {
        resolver.resolve("ffmpeg", &config.ffmpeg).map(Some)
    }
}

fn log_startup(config: &Config, path: Option<&Path>) {
    log::info!("Rustlink starting");
    for field in startup_fields(config, path) {
        log::info!("  {field}");
    }
}

fn startup_fields(config: &Config, path: Option<&Path>) -> Vec<String> {
    vec![
        format!("version={}", rustlink_server::VERSION),
        format!("apiVersion={}", rustlink_server::APIVERSION),
        match path {
            Some(p) => format!("config={}", p.display()),
            None => "config=defaults".into(),
        },
        format!("sources={}", source_summary(config)),
        format!("inProcessAudio={}", config.in_process_audio),
        format!("ffmpeg={}", if config.ffmpeg.is_empty() { "disabled" } else { &config.ffmpeg }),
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

fn ensure_potoken_starter(config: &Config) {
    let raw = config.youtube_po_token_file.trim();
    if raw.is_empty() {
        return;
    }
    let path = Path::new(raw);
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut nonce = [0u8; 8];
    let _ = getrandom::fill(&mut nonce);
    let temp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(OsStr::to_str).unwrap_or("potoken.yaml"),
        tools::hex_encode(&nonce)
    ));
    if write_starter_at(path, &temp, rustlink_config::starter_potoken()).is_ok() {
        log::info!("created starter YouTube poToken file path={}", path.display());
    }
}

fn source_summary(config: &Config) -> String {
    let mut active = Vec::with_capacity(6);
    if config.youtube_native {
        if config.youtube_require_po_token || !config.youtube_po_token_file.trim().is_empty() {
            active.push("youtube(native+potoken)");
        } else {
            active.push("youtube(native)");
        }
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
    active.extend(["deezer", "http"]);
    active.join(",")
}

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
        for raw in ["-c node.yaml", "--config=node.yaml", "--config node.yaml"] {
            assert_eq!(arguments(raw).unwrap().config, Some(PathBuf::from("node.yaml")));
        }
    }

    #[test]
    fn the_switches_that_take_no_value_are_read_as_present() {
        let parsed = arguments("--version --check").unwrap();
        assert!(parsed.version && parsed.check && parsed.config.is_none());
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
        assert!(is_none("none") && is_none(" NONE ") && !is_none("ffmpeg"));
    }

    #[test]
    fn a_generated_password_is_thirty_two_hexadecimal_characters() {
        let password = random_password().expect("the system source answers");
        assert_eq!(password.len(), 32);
        assert!(password.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(random_password(), random_password());
    }

    #[test]
    fn the_startup_summary_names_the_sources_that_are_on() {
        let summary = source_summary(&Config::default());
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
        assert!(fields.iter().all(|f| !f.contains(['\r', '\n'])));
    }

    #[test]
    fn the_level_names_the_validator_accepts_are_the_ones_read_here() {
        assert_eq!(level("debug"), LevelFilter::Debug);
        assert_eq!(level("WARN"), LevelFilter::Warn);
        assert_eq!(level("error"), LevelFilter::Error);
        assert_eq!(level(""), LevelFilter::Info);
    }

    #[test]
    fn ffmpeg_resolution_handles_cached_disabled_and_missing_helpers() {
        let directory =
            std::env::temp_dir().join(format!("rustlink-helpers-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let exe = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(directory.join(&exe), b"stub").unwrap();

        let mut config = Config::default();
        config.tools_directory = directory.display().to_string();
        config.auto_download = false;
        config.ffmpeg = "ffmpeg".into();
        let resolver = tools::Resolver::new(&config.tools_directory, false);
        assert!(resolve_ffmpeg(&resolver, &config).unwrap().unwrap().ends_with(&exe));
        let _ = std::fs::remove_dir_all(&directory);

        config.ffmpeg = "none".into();
        assert!(resolve_ffmpeg(&resolver, &config).unwrap().is_none());

        config.ffmpeg = "ffmpeg".into();
        assert!(resolve_ffmpeg(&resolver, &config).is_err());
    }

    #[test]
    fn inline_config_supports_dash_prefixed_files_and_check_value_reports_clear_error() {
        for flag in ["--config=-file.yaml", "-c=-file.yaml"] {
            assert_eq!(arguments(flag).unwrap().config, Some(PathBuf::from("-file.yaml")));
        }
        assert_eq!(arguments("--check=true").unwrap_err(), "--check does not take a value");
        assert_eq!(arguments("-V=1").unwrap_err(), "--version does not take a value");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(vec![b'-', b'c', 0xff, b'.', b'y', b'm', b'l']);
            let parsed = Arguments::parse([OsString::from("--config"), raw.clone()]).unwrap();
            assert_eq!(parsed.config, Some(PathBuf::from(raw)));
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let unpaired = OsString::from_wide(&[0xd800, u16::from(b'.'), u16::from(b'y')]);
            let parsed = Arguments::parse([OsString::from("--config"), unpaired.clone()]).unwrap();
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
