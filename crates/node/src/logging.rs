//! The console log, the log files and the startup wordmark.
//!
//! The console output is meant to be read by a person: the time is dimmed, the
//! level is coloured and padded to five columns, and the message follows.
//! Colour is written only when the output can show it, and never into a log
//! file, where an escape sequence is noise for every tool that reads one.
//!
//! The records themselves arrive through the `log` facade, which every other
//! crate writes to. A record carries its values inside its message, written as
//! `key=value` pairs for readable text and structured JSON output.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Days, Local, NaiveDate, SecondsFormat};
use log::{Level, LevelFilter, Log, Metadata, Record};

/// The label of the record written just before the process stops. It sits above
/// error so an operator can tell a failure the node recovered from at a glance.
const FATAL: &str = "FATAL";

/// The console timestamp, which carries no date: the date is in the log file
/// name, and a console line should stay short.
const CONSOLE_TIME: &str = "%H:%M:%S%.3f";

/// The stem of a daily log file. Only names in this shape are ever removed, so
/// a directory that holds anything else keeps it.
const FILE_PREFIX: &str = "rustlink-";

/// The process logger, once one has been installed.
static INSTALLED: OnceLock<&'static Logger> = OnceLock::new();

/// ANSI escapes, kept in one place so the palette is easy to adjust.
mod paint {
    pub const RESET: &str = "\x1b[0m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    /// The "LINK" half of the wordmark: the project red, #D93A49. None of the
    /// basic ANSI colours is that shade, so it is written as truecolor.
    pub const CRIMSON: &str = "\x1b[38;2;217;58;73m";
    /// The "RUST" half of the wordmark: the project teal, #39C5BB, truecolor
    /// for the same reason.
    pub const TEAL: &str = "\x1b[38;2;57;197;187m";
}

/// Where colour is allowed to go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    /// Reads the configuration value. The configuration is checked when it is
    /// loaded, so anything unexpected is treated as auto.
    pub fn parse(value: &str) -> Color {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" => Color::Always,
            "never" => Color::Never,
            _ => Color::Auto,
        }
    }

    /// Whether colour may be written to the console.
    pub fn allowed(self) -> bool {
        match self {
            Color::Always => true,
            Color::Never => false,
            // Auto colours a terminal and leaves a redirected stream alone,
            // which is what keeps a piped log clean.
            Color::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        }
    }
}

/// What the console and the files are written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Text,
    Json,
}

impl Format {
    /// Reads the configuration value, defaulting to text the way the
    /// configuration validator does.
    pub fn parse(value: &str) -> Format {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Format::Json,
            _ => Format::Text,
        }
    }
}

/// Everything the process logger needs to be built.
pub struct Options {
    pub level: LevelFilter,
    pub format: Format,
    pub color: Color,
    /// Where the daily files are kept. Empty leaves the console as the only
    /// output.
    pub directory: String,
    /// How many days of files to keep. Zero keeps every file, which is what a
    /// node whose logs are rotated by something else asks for.
    pub retention: u32,
}

/// Builds the process logger and makes it the one every crate writes to.
///
/// The log file is opened here rather than on the first record, so a directory
/// that cannot be written to is a startup failure instead of a surprise later.
pub fn install(options: &Options) -> Result<&'static Logger, String> {
    let files = if options.directory.trim().is_empty() {
        None
    } else {
        Some(Mutex::new(Files::open(options.directory.trim(), options.retention)?))
    };
    let logger: &'static Logger = Box::leak(Box::new(Logger {
        level: options.level,
        format: options.format,
        color: options.color.allowed(),
        files,
    }));
    log::set_logger(logger).map_err(|error| format!("logger: {error}"))?;
    log::set_max_level(options.level);
    let _ = INSTALLED.set(logger);
    Ok(logger)
}

/// Writes the record that says why the node is stopping, then ends the process.
pub fn fatal(message: &str, error: &str) -> ! {
    let line = format!("{message} error={}", quote_if_needed(error));
    match INSTALLED.get() {
        Some(logger) => logger.write(FATAL, paint::RED, &line),
        None => eprintln!("{FATAL} {line}"),
    }
    hold_console();
    std::process::exit(1);
}

/// Keeps the last line readable when it was written into a window that closes
/// with the process, which is the window a double-clicked binary gets.
///
/// A node that was given a flag was typed at a prompt, and one without a
/// terminal is a service or a container: neither has a window to hold open.
fn hold_console() {
    if std::env::args().len() > 1 || !std::io::stdin().is_terminal() {
        return;
    }
    println!();
    println!("press Enter to close");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// The logger the crates write to.
pub struct Logger {
    level: LevelFilter,
    format: Format,
    color: bool,
    files: Option<Mutex<Files>>,
}

impl Logger {
    /// The file being written to, or `None` when file logging is off.
    pub fn path(&self) -> Option<String> {
        self.files.as_ref().map(|files| files.lock().unwrap().path())
    }

    /// Writes one record to the console and mirrors it into the file.
    fn write(&self, label: &str, color: &str, message: &str) {
        let at = Local::now();
        let console = match self.format {
            Format::Json => json_line(label, message, at),
            Format::Text => console_line(label, color, message, at, self.color),
        };
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(console.as_bytes());
        let _ = out.flush();
        drop(out);
        if let Some(files) = &self.files {
            files.lock().unwrap().write(self.format, label, message, at);
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
        self.write(label, color, &record.args().to_string());
    }

    fn flush(&self) {}
}
/// The daily file the log is mirrored into. Records are never coloured, even
/// when the console is: an escape sequence in a file is noise for every tool
/// that reads one.
struct Files {
    directory: PathBuf,
    day: String,
    handle: File,
    /// How many days of files are kept.
    retention: u32,
}

impl Files {
    fn open(directory: &str, retention: u32) -> Result<Files, String> {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("log directory {directory}: {error}"))?;
        let day = Local::now().format("%Y-%m-%d").to_string();
        let handle = open_day(Path::new(directory), &day)?;
        let files = Files { directory: PathBuf::from(directory), day, handle, retention };
        files.prune();
        Ok(files)
    }

    /// The file currently being written to.
    fn path(&self) -> String {
        self.directory.join(format!("{FILE_PREFIX}{}.log", self.day)).display().to_string()
    }

    /// Appends one record, reopening the file when the date changes so a node
    /// that stays up for weeks does not grow one file forever.
    fn write(&mut self, format: Format, label: &str, message: &str, at: DateTime<Local>) {
        let day = at.format("%Y-%m-%d").to_string();
        if day != self.day {
            // Keeping the previous file when the new day cannot be opened is
            // the lesser loss: a misnamed log beats no log.
            if let Ok(handle) = open_day(&self.directory, &day) {
                self.handle = handle;
                self.day = day;
                self.prune();
            }
        }
        let line = match format {
            Format::Json => json_line(label, message, at),
            Format::Text => text_line(label, message, at),
        };
        let _ = self.handle.write_all(line.as_bytes());
    }

    /// Removes the files that have aged out of the retention window.
    fn prune(&self) {
        prune(&self.directory, self.retention);
    }
}

fn open_day(directory: &Path, day: &str) -> Result<File, String> {
    let path = directory.join(format!("{FILE_PREFIX}{day}.log"));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("log file {}: {error}", path.display()))
}

/// Deletes the daily files older than the retention window.
///
/// Only names this node writes are considered, so a directory that holds other
/// files keeps them. Zero keeps every file. A deletion that fails is passed
/// over: a log file that could not be removed is not worth stopping a node for,
/// and the next start tries again.
fn prune(directory: &Path, retention: u32) {
    if retention == 0 {
        return;
    }
    let Some(cutoff) = Local::now().date_naive().checked_sub_days(Days::new(u64::from(retention)))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(logged_day) else {
            continue;
        };
        if day < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Reads the date out of a file name, or nothing when the name is not one of
/// this node's daily logs.
fn logged_day(name: &str) -> Option<NaiveDate> {
    let day = name.strip_prefix(FILE_PREFIX)?.strip_suffix(".log")?;
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
}

/// One console line: the time, the level and the message.
fn console_line(
    label: &str,
    color: &str,
    message: &str,
    at: DateTime<Local>,
    colored: bool,
) -> String {
    let time = at.format(CONSOLE_TIME).to_string();
    if !colored {
        return format!("{time} {label:<5} {message}\n");
    }
    let padded = format!("{label:<5}");
    format!(
        "{dim}{time}{reset} {color}{padded}{reset} {bold}{message}{reset}\n",
        dim = paint::DIM,
        reset = paint::RESET,
        color = color,
        bold = paint::BOLD,
        padded = padded,
    )
}

/// One JSON record containing time, level and message.
fn json_line(label: &str, message: &str, at: DateTime<Local>) -> String {
    let body = serde_json::json!({
        "time": at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "level": label,
        "msg": message,
    });
    format!("{body}\n")
}

/// One text record with quoted values for the log file.
fn text_line(label: &str, message: &str, at: DateTime<Local>) -> String {
    format!(
        "time={} level={} msg={}\n",
        at.to_rfc3339_opts(SecondsFormat::Millis, true),
        label,
        quote_if_needed(message)
    )
}

/// Quotes a value that would otherwise be ambiguous. Printable non-ASCII text
/// is kept as it is, which matters for a title written in CJK.
fn quote_if_needed(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }
    if value.chars().any(|character| {
        character < ' '
            || character == '\x7f'
            || character == ' '
            || character == '"'
            || character == '='
    }) {
        return format!("{value:?}");
    }
    value.to_string()
}

/// The label and colour of a level. The label is padded where it is written, so
/// every line starts its message in the same column.
fn style(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::Trace | Level::Debug => ("DEBUG", paint::CYAN),
        Level::Info => ("INFO", paint::GREEN),
        Level::Warn => ("WARN", paint::YELLOW),
        Level::Error => ("ERROR", paint::RED),
    }
}

/// The name drawn at startup.
const WORDMARK: &str = "RUSTLINK";

/// How many letters take the first colour of the wordmark; the rest take the
/// second.
const SPLIT: usize = 4;

/// Draws the wordmark and the version line, which a person reads at startup. A
/// node that writes JSON logs leaves it out so the stream stays parseable.
pub fn banner(color: bool, version: &str, api_version: &str) -> String {
    let mut out = String::new();
    for row in 0..6 {
        for (index, letter) in WORDMARK.chars().enumerate() {
            let glyph = block(letter)[row];
            if !color {
                out.push_str(glyph);
            } else if index < SPLIT {
                out.push_str(paint::TEAL);
                out.push_str(glyph);
                out.push_str(paint::RESET);
            } else {
                out.push_str(paint::CRIMSON);
                out.push_str(glyph);
                out.push_str(paint::RESET);
            }
        }
        out.push('\n');
    }
    let subtitle = format!("v{version}  -  Rustlink API {api_version}");
    if color {
        out.push_str(&format!("\n  {}{subtitle}{}\n\n", paint::DIM, paint::RESET));
    } else {
        out.push_str(&format!("\n  {subtitle}\n\n"));
    }
    out
}

/// The wordmark in the ANSI Shadow face: six rows per letter, ten columns wide.
fn block(letter: char) -> [&'static str; 6] {
    match letter {
        'G' => [
            " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} ",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} ",
            "\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}",
            " \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} ",
        ],
        'O' => [
            " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} ",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}",
            " \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} ",
        ],
        'R' => [
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} ",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}",
        ],
        'U' => [
            "\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}",
            " \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} ",
        ],
        'S' => [
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}",
            "\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}",
        ],
        'T' => [
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}",
            "\u{255a}\u{2550}\u{2550}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{255d}",
            "   \u{2588}\u{2588}\u{2551}   ",
            "   \u{2588}\u{2588}\u{2551}   ",
            "   \u{2588}\u{2588}\u{2551}   ",
            "   \u{255a}\u{2550}\u{255d}   ",
        ],
        'L' => [
            "\u{2588}\u{2588}\u{2557}     ",
            "\u{2588}\u{2588}\u{2551}     ",
            "\u{2588}\u{2588}\u{2551}     ",
            "\u{2588}\u{2588}\u{2551}     ",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}",
            "\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}",
        ],
        'A' => [
            " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} ",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}",
        ],
        'I' => [
            "\u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2550}\u{255d}",
        ],
        'N' => [
            "\u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551}\u{255a}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}",
            "\u{2588}\u{2588}\u{2551} \u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
            "\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{2550}\u{2550}\u{255d}",
        ],
        'K' => [
            "\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2557}",
            "\u{2588}\u{2588}\u{2551} \u{2588}\u{2588}\u{2554}\u{255d}",
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d} ",
            "\u{2588}\u{2588}\u{2554}\u{2550}\u{2588}\u{2588}\u{2557} ",
            "\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2557}",
            "\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}",
        ],
        _ => ["          "; 6],
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Local> {
        use chrono::TimeZone;
        Local.with_ymd_and_hms(2026, 10, 11, 9, 5, 3).unwrap()
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
    fn a_json_line_carries_the_time_the_level_and_the_message() {
        let line = json_line("INFO", "Rustlink listening", at());
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["msg"], "Rustlink listening");
        assert!(value["time"].as_str().unwrap().starts_with("2026-10-11T09:05:03"));
    }

    #[test]
    fn a_console_line_starts_with_the_time_and_the_padded_level() {
        let line = console_line("INFO", paint::GREEN, "Rustlink listening", at(), false);
        assert_eq!(line, "09:05:03.000 INFO  Rustlink listening\n");
    }

    #[test]
    fn a_coloured_console_line_wraps_the_level_and_the_message() {
        let line = console_line("WARN", paint::YELLOW, "ffmpeg unavailable", at(), true);
        assert!(line.contains(paint::YELLOW));
        assert!(line.contains(paint::BOLD));
        assert!(line.ends_with(&format!("{}\n", paint::RESET)));
        assert!(line.contains("ffmpeg unavailable"));
    }

    #[test]
    fn a_text_line_names_its_fields() {
        let line = text_line("ERROR", "webhook delivery failed", at());
        assert!(line.starts_with("time=2026-10-11T09:05:03"));
        assert!(line.contains(" level=ERROR "));
        assert!(line.contains("msg=\"webhook delivery failed\""));
    }

    #[test]
    fn the_wordmark_is_drawn_one_row_per_line() {
        let plain = banner(false, "0.2.0", "1");
        // Six rows of letters, then a blank line, then the version line.
        let rows: Vec<&str> = plain.lines().take_while(|line| !line.is_empty()).collect();
        assert_eq!(rows.len(), 6);
        // Every row of a letter has to be as wide as its siblings, or the
        // letters would not line up.
        for letter in WORDMARK.chars() {
            let glyph = block(letter);
            let width = glyph[0].chars().count();
            assert!(glyph.iter().all(|row| row.chars().count() == width), "{letter}");
        }
        assert!(rows[0].starts_with("██████╗ "));
        assert!(plain.contains("v0.2.0  -  Rustlink API 1"));
        assert!(!plain.contains('\x1b'));
        let colored = banner(true, "0.2.0", "1");
        // Each half carries a project colour, and both are truecolor escapes
        // because neither shade is one of the sixteen basic colours.
        assert!(colored.contains(paint::CRIMSON));
        assert!(colored.contains(paint::TEAL));
        assert!(colored.contains("\x1b[38;2;217;58;73m"));
        assert!(colored.contains("\x1b[38;2;57;197;187m"));
        // "RUST" comes first, so the teal half is the one that leads.
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
    fn the_file_name_of_the_day_is_the_one_the_startup_line_reports() {
        let directory =
            std::env::temp_dir().join(format!("rustlink-log-test-{}", std::process::id()));
        let files = Files::open(&directory.display().to_string(), 14).unwrap();
        let day = Local::now().format("%Y-%m-%d").to_string();
        assert!(files.path().ends_with(&format!("rustlink-{day}.log")));
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A directory holding one stale file, one file from today and one the node
    /// did not write.
    fn prepared_directory(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let directory =
            std::env::temp_dir().join(format!("rustlink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
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
    fn a_file_older_than_the_window_goes_and_the_others_stay() {
        let (directory, stale, today, foreign) = prepared_directory("prune");
        prune(&directory, 14);
        assert!(!stale.exists(), "a file from 2020 is outside a fourteen day window");
        assert!(today.exists(), "today's file is inside it");
        assert!(foreign.exists(), "a file this node did not write is left alone");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_name_that_only_looks_like_a_log_file_is_left_alone() {
        let directory = std::env::temp_dir().join(format!("rustlink-odd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let odd = directory.join("rustlink-not-a-date.log");
        std::fs::write(&odd, b"a line\n").unwrap();
        prune(&directory, 1);
        assert!(odd.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn zero_retention_keeps_every_file() {
        let (directory, stale, _, _) = prepared_directory("keep");
        prune(&directory, 0);
        assert!(stale.exists(), "zero days means every file is kept");
        let _ = std::fs::remove_dir_all(&directory);
    }
}
