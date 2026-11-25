//! Console log, rotating daily log files, and startup wordmark.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Days, Local, NaiveDate, SecondsFormat};
use log::{Level, LevelFilter, Log, Metadata, Record};

const FATAL: &str = "FATAL";
const CONSOLE_TIME: &str = "%H:%M:%S%.3f";
const FILE_PREFIX: &str = "rustlink-";
const ROTATE_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

static INSTALLED: OnceLock<&'static Logger> = OnceLock::new();

mod paint {
    pub const RESET: &str = "\x1b[0m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const CRIMSON: &str = "\x1b[38;2;217;58;73m";
    pub const TEAL: &str = "\x1b[38;2;57;197;187m";
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn parse(value: &str) -> Color {
        let v = value.trim();
        if v.eq_ignore_ascii_case("always") {
            Color::Always
        } else if v.eq_ignore_ascii_case("never") {
            Color::Never
        } else {
            Color::Auto
        }
    }

    pub fn allowed(self) -> bool {
        match self {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Text,
    Json,
}

impl Format {
    pub fn parse(value: &str) -> Format {
        if value.trim().eq_ignore_ascii_case("json") {
            Format::Json
        } else {
            Format::Text
        }
    }
}

pub struct Options {
    pub level: LevelFilter,
    pub format: Format,
    pub color: Color,
    pub directory: String,
    pub retention: u32,
}

pub fn install(options: &Options) -> Result<&'static Logger, String> {
    let dir = options.directory.trim();
    let files = (!dir.is_empty())
        .then(|| Files::open(dir, options.retention).map(Mutex::new))
        .transpose()?;
    let logger: &'static Logger = Box::leak(Box::new(Logger {
        level: options.level,
        format: options.format,
        color: options.color.allowed(),
        files,
    }));
    log::set_logger(logger).map_err(|e| format!("logger: {e}"))?;
    log::set_max_level(options.level);
    let _ = INSTALLED.set(logger);
    Ok(logger)
}

pub fn fatal(message: &str, error: &str) -> ! {
    let line = format!("{message} error={}", quote_if_needed(error));
    match INSTALLED.get() {
        Some(logger) => logger.write(FATAL, paint::RED, &line),
        None => eprintln!("{FATAL} {line}"),
    }
    hold_console();
    std::process::exit(1);
}

fn hold_console() {
    if std::env::args_os().nth(1).is_some()
        || std::env::var_os("RUSTLINK_NO_PAUSE").is_some()
        || !std::io::stdin().is_terminal()
        || !owns_ephemeral_console()
    {
        return;
    }
    println!("\npress Enter to close");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

#[cfg(windows)]
fn owns_ephemeral_console() -> bool {
    extern "system" {
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
    }
    let mut pids = [0u32; 2];
    // SAFETY: `pids` points to two writable `u32` slots matching `dwProcessCount = 2`.
    unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) == 1 }
}

#[cfg(not(windows))]
fn owns_ephemeral_console() -> bool {
    false
}

pub struct Logger {
    level: LevelFilter,
    format: Format,
    color: bool,
    files: Option<Mutex<Files>>,
}

impl Logger {
    fn lock_files(&self) -> Option<MutexGuard<'_, Files>> {
        self.files.as_ref().map(|f| f.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn path(&self) -> Option<String> {
        self.lock_files().map(|f| f.path())
    }

    fn write(&self, label: &str, color: &str, message: &str) {
        let at = Local::now();
        let console = match self.format {
            Format::Json => json_line(label, message, at),
            Format::Text => console_line(label, color, message, at, self.color),
        };
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(console.as_bytes());
            let _ = out.flush();
        }
        if let Some(mut files) = self.lock_files() {
            let file_line = match self.format {
                Format::Json => console,
                Format::Text => text_line(label, message, at),
            };
            let prune_job = files.write_line(&file_line, at.date_naive());
            drop(files);
            if let Some((directory, retention, today)) = prune_job {
                spawn_prune(directory, retention, today);
            }
        }
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let (label, color) = style(record.level());
        match record.args().as_str() {
            Some(literal) => self.write(label, color, literal),
            None => self.write(label, color, &record.args().to_string()),
        }
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
        if let Some(mut files) = self.lock_files() {
            let _ = files.handle.flush();
        }
    }
}

struct Files {
    directory: PathBuf,
    date: NaiveDate,
    handle: File,
    retention: u32,
    retry_after: Option<Instant>,
}

impl Files {
    fn open(directory: &str, retention: u32) -> Result<Files, String> {
        let date = Local::now().date_naive();
        let directory = PathBuf::from(directory);
        let handle = open_day(&directory, date)?;
        spawn_prune(directory.clone(), retention, date);
        Ok(Files { directory, date, handle, retention, retry_after: None })
    }

    fn path(&self) -> String {
        self.directory.join(format!("{FILE_PREFIX}{}.log", self.date)).display().to_string()
    }

    fn write_line(&mut self, line: &str, date: NaiveDate) -> Option<(PathBuf, u32, NaiveDate)> {
        self.write_line_at(line, date, Instant::now())
    }

    fn write_line_at(
        &mut self,
        line: &str,
        date: NaiveDate,
        now: Instant,
    ) -> Option<(PathBuf, u32, NaiveDate)> {
        let mut rotated = None;
        if date != self.date && self.retry_after.is_none_or(|until| now >= until) {
            match open_day(&self.directory, date) {
                Ok(handle) => {
                    self.handle = handle;
                    self.date = date;
                    self.retry_after = None;
                    rotated = Some((self.directory.clone(), self.retention, date));
                }
                Err(_) => self.retry_after = Some(now + ROTATE_RETRY_COOLDOWN),
            }
        }
        let _ = self.handle.write_all(line.as_bytes());
        rotated
    }
}

fn spawn_prune(directory: PathBuf, retention: u32, today: NaiveDate) {
    if retention == 0 {
        return;
    }
    let fallback = directory.clone();
    if std::thread::Builder::new()
        .name("log-prune".into())
        .spawn(move || prune(&directory, retention, today))
        .is_err()
    {
        prune(&fallback, retention, today);
    }
}

fn open_day(directory: &Path, date: NaiveDate) -> Result<File, String> {
    std::fs::create_dir_all(directory)
        .map_err(|e| format!("log directory {}: {e}", directory.display()))?;
    let path = directory.join(format!("{FILE_PREFIX}{date}.log"));
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    options.open(&path).map_err(|e| format!("log file {}: {e}", path.display()))
}

fn prune(directory: &Path, retention: u32, today: NaiveDate) {
    if retention == 0 {
        return;
    }
    let Some(cutoff) = today.checked_sub_days(Days::new(u64::from(retention - 1))) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str().and_then(logged_day).is_some_and(|day| day < cutoff) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn logged_day(name: &str) -> Option<NaiveDate> {
    let day = name.strip_prefix(FILE_PREFIX)?.strip_suffix(".log")?;
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
}

fn is_unsafe_log_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn has_unsafe_log_char(text: &str) -> bool {
    text.bytes().any(|b| b < b' ' || b == 0x7f || b == 0xc2 || b == 0xe2)
        && text.chars().any(is_unsafe_log_char)
}

fn console_line(
    label: &str,
    color: &str,
    message: &str,
    at: DateTime<Local>,
    colored: bool,
) -> String {
    let clean: Cow<'_, str> = if has_unsafe_log_char(message) {
        Cow::Owned(message.chars().map(|c| if is_unsafe_log_char(c) { ' ' } else { c }).collect())
    } else {
        Cow::Borrowed(message)
    };
    let time = at.format(CONSOLE_TIME);
    if !colored {
        return format!("{time} {label:<5} {clean}\n");
    }
    format!(
        "{dim}{time}{reset} {color}{label:<5}{reset} {bold}{clean}{reset}\n",
        dim = paint::DIM,
        reset = paint::RESET,
        bold = paint::BOLD,
    )
}

fn json_line(label: &str, message: &str, at: DateTime<Local>) -> String {
    let time = at.to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut out = Vec::with_capacity(36 + time.len() + label.len() + message.len());
    out.extend_from_slice(b"{\"time\":\"");
    out.extend_from_slice(time.as_bytes());
    out.extend_from_slice(b"\",\"level\":\"");
    out.extend_from_slice(label.as_bytes());
    out.extend_from_slice(b"\",\"msg\":");
    let _ = serde_json::to_writer(&mut out, message);
    out.extend_from_slice(b"}\n");
    String::from_utf8(out).unwrap_or_default()
}

fn text_line(label: &str, message: &str, at: DateTime<Local>) -> String {
    format!(
        "time={} level={} msg={}\n",
        at.to_rfc3339_opts(SecondsFormat::Millis, true),
        label,
        quote_if_needed(message)
    )
}

fn quote_if_needed(value: &str) -> Cow<'_, str> {
    if value.is_empty() {
        return Cow::Borrowed("\"\"");
    }
    if value.bytes().any(|b| b <= b' ' || b == 0x7f || matches!(b, b'"' | b'\\' | b'='))
        || has_unsafe_log_char(value)
    {
        return Cow::Owned(format!("{value:?}"));
    }
    Cow::Borrowed(value)
}

const fn style(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::Trace | Level::Debug => ("DEBUG", paint::CYAN),
        Level::Info => ("INFO", paint::GREEN),
        Level::Warn => ("WARN", paint::YELLOW),
        Level::Error => ("ERROR", paint::RED),
    }
}

/// ANSI Shadow wordmark split into `("RUST", "LINK")` halves per row.
const WORDMARK: [(&str, &str); 6] = [
    ("██████╗ ██╗   ██╗███████╗████████╗", "██╗     ██╗███╗   ██╗██╗  ██╗"),
    ("██╔══██╗██║   ██║██╔════╝╚══██╔══╝", "██║     ██║████╗  ██║██║ ██╔╝"),
    ("██████╔╝██║   ██║███████╗   ██║   ", "██║     ██║██╔██╗ ██║█████╔╝ "),
    ("██╔══██╗██║   ██║╚════██║   ██║   ", "██║     ██║██║╚██╗██║██╔═██╗ "),
    ("██║  ██║╚██████╔╝███████║   ██║   ", "███████╗██║██║ ╚████║██║  ██╗"),
    ("╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ", "╚══════╝╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝"),
];

pub fn banner(color: bool, version: &str, api_version: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(768);
    for (rust, link) in WORDMARK {
        if color {
            let _ = writeln!(
                out,
                "{}{rust}{}{}{link}{}",
                paint::TEAL,
                paint::RESET,
                paint::CRIMSON,
                paint::RESET
            );
        } else {
            let _ = writeln!(out, "{rust}{link}");
        }
    }
    if color {
        let _ = writeln!(
            out,
            "\n  {}v{version}  -  Rustlink API {api_version}{}\n",
            paint::DIM,
            paint::RESET
        );
    } else {
        let _ = writeln!(out, "\n  v{version}  -  Rustlink API {api_version}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Local> {
        use chrono::TimeZone;
        Local.with_ymd_and_hms(2026, 10, 11, 9, 5, 3).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustlink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_plain_value_is_not_quoted_but_an_ambiguous_one_is() {
        assert_eq!(quote_if_needed("ready"), "ready");
        assert_eq!(quote_if_needed("台北"), "台北");
        assert_eq!(quote_if_needed("two words"), "\"two words\"");
        assert_eq!(quote_if_needed("key=value"), "\"key=value\"");
        assert_eq!(quote_if_needed(""), "\"\"");
    }

    #[test]
    fn json_console_and_text_lines_format_cleanly() {
        let json = json_line("INFO", "Rustlink listening", at());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["msg"], "Rustlink listening");
        assert!(value["time"].as_str().unwrap().starts_with("2026-10-11T09:05:03"));

        assert_eq!(
            console_line("INFO", paint::GREEN, "Rustlink listening", at(), false),
            "09:05:03.000 INFO  Rustlink listening\n"
        );

        let colored = console_line("WARN", paint::YELLOW, "ffmpeg unavailable", at(), true);
        assert!(colored.contains(paint::YELLOW) && colored.contains(paint::BOLD));
        assert!(colored.ends_with(&format!("{}\n", paint::RESET)));

        let text = text_line("ERROR", "webhook delivery failed", at());
        assert!(text.starts_with("time=2026-10-11T09:05:03"));
        assert!(text.contains(" level=ERROR msg=\"webhook delivery failed\""));
    }

    #[test]
    fn the_wordmark_is_drawn_one_row_per_line() {
        let plain = banner(false, "0.2.0", "1");
        let rows: Vec<&str> = plain.lines().take_while(|line| !line.is_empty()).collect();
        assert_eq!(rows.len(), 6);
        let width = rows[0].chars().count();
        assert!(rows.iter().all(|row| row.chars().count() == width));
        assert!(rows[0].starts_with("██████╗ "));
        assert!(plain.contains("v0.2.0  -  Rustlink API 1"));
        assert!(!plain.contains('\x1b'));

        let colored = banner(true, "0.2.0", "1");
        assert!(colored.contains(paint::CRIMSON) && colored.contains(paint::TEAL));
        assert!(colored.find(paint::TEAL) < colored.find(paint::CRIMSON));
    }

    #[test]
    fn the_configured_values_are_read_the_way_the_validator_writes_them() {
        assert_eq!(Color::parse("Always"), Color::Always);
        assert_eq!(Color::parse("never"), Color::Never);
        assert_eq!(Color::parse(""), Color::Auto);
        assert_eq!(Format::parse("JSON"), Format::Json);
        assert_eq!(Format::parse("text"), Format::Text);
    }

    #[test]
    fn the_file_name_of_the_day_is_the_one_the_startup_summary_reports() {
        let directory = temp_dir("log-test");
        let files = Files::open(&directory.display().to_string(), 14).unwrap();
        let day = Local::now().format("%Y-%m-%d").to_string();
        assert!(files.path().ends_with(&format!("rustlink-{day}.log")));
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn prepared_directory(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let directory = temp_dir(name);
        std::fs::create_dir_all(&directory).unwrap();
        let stale = directory.join("rustlink-2020-01-01.log");
        let today = directory.join(format!("rustlink-{}.log", Local::now().format("%Y-%m-%d")));
        let foreign = directory.join("notes.txt");
        for file in [&stale, &today, &foreign] {
            std::fs::write(file, b"a line\n").unwrap();
        }
        (directory, stale, today, foreign)
    }

    #[test]
    fn pruning_respects_retention_window_and_ignores_foreign_files() {
        let (directory, stale, today, foreign) = prepared_directory("prune");
        let odd = directory.join("rustlink-not-a-date.log");
        std::fs::write(&odd, b"a line\n").unwrap();
        prune(&directory, 14, Local::now().date_naive());
        assert!(!stale.exists());
        assert!(today.exists() && foreign.exists() && odd.exists());
        let _ = std::fs::remove_dir_all(&directory);

        let (keep_dir, keep_stale, _, _) = prepared_directory("keep");
        prune(&keep_dir, 0, Local::now().date_naive());
        assert!(keep_stale.exists());
        let _ = std::fs::remove_dir_all(&keep_dir);
    }

    #[test]
    fn poisoned_file_mutex_recovers_without_panicking() {
        let directory = temp_dir("poison");
        let logger = std::sync::Arc::new(Logger {
            level: LevelFilter::Info,
            format: Format::Text,
            color: false,
            files: Some(Mutex::new(Files::open(&directory.display().to_string(), 0).unwrap())),
        });
        let poisoned = std::sync::Arc::clone(&logger);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.files.as_ref().unwrap().lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(logger.path().is_some());
        logger.write("INFO", paint::GREEN, "recovered after poison");
        logger.flush();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn rotation_recreates_missing_directory_and_throttles_failed_reopens() {
        let directory = temp_dir("rotate");
        let mut files = Files::open(&directory.display().to_string(), 0).unwrap();
        let start = Instant::now();
        let earlier = NaiveDate::from_ymd_opt(2025, 1, 2).unwrap();

        let _ = std::fs::remove_dir_all(&directory);
        assert!(files.write_line_at("first\n", earlier, start).is_some());
        assert_eq!(files.date, earlier);
        assert!(directory.join("rustlink-2025-01-02.log").is_file());

        let blocker = directory.join("blocker-file");
        std::fs::write(&blocker, b"x").unwrap();
        files.directory = blocker;
        let next_day = NaiveDate::from_ymd_opt(2025, 1, 3).unwrap();
        assert!(files.write_line_at("second\n", next_day, start).is_none());
        let cooldown_until = files.retry_after.expect("failed rotation sets a retry cooldown");
        assert_eq!(files.date, earlier);

        files.directory = directory.clone();
        assert!(files
            .write_line_at("during-cooldown\n", next_day, start + Duration::from_secs(1))
            .is_none());
        assert_eq!(files.date, earlier);
        assert!(files.write_line_at("after-cooldown\n", next_day, cooldown_until).is_some());
        assert_eq!(files.date, next_day);
        assert!(files.retry_after.is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
