use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LOG_ROOT: OnceLock<PathBuf> = OnceLock::new();
static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();
static CONFIGURED_LEVEL: OnceLock<Mutex<LogLevel>> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Off,
    Normal,
    Verbose,
}

pub fn init(data_root: &Path) {
    let _ = LOG_LEVEL.set(log_level());
    let _ = CONFIGURED_LEVEL.set(Mutex::new(log_level()));
    let root = data_root.join("logs");
    let _ = std::fs::create_dir_all(root.join("server"));
    let _ = std::fs::create_dir_all(root.join("collector"));
    let _ = LOG_ROOT.set(root);
    write("diagnostics_initialized", "");
}

pub fn set_level(value: &str) {
    if let Some(level) = CONFIGURED_LEVEL.get() {
        if let Ok(mut level) = level.lock() {
            *level = parse_log_level(value);
        }
    }
}

pub fn write(event: &str, details: &str) {
    write_at("server", LogLevel::Normal, event, details);
}

pub fn verbose(event: &str, details: &str) {
    write_at("server", LogLevel::Verbose, event, details);
}

/// Append an event received from a collector to that collector's own log.
/// `collector_id` is the server-side stable identity used by collector management,
/// so logs remain separated even when display names or IP addresses change.
pub fn collector(collector_id: &str, event: &str, details: &str) {
    let directory = safe_component(collector_id);
    write_at(
        &format!("collector/{directory}"),
        LogLevel::Normal,
        event,
        details,
    );
}

fn write_at(directory: &str, required: LogLevel, event: &str, details: &str) {
    if configured_level() == LogLevel::Off
        || (required == LogLevel::Verbose && configured_level() != LogLevel::Verbose)
    {
        return;
    }
    let Some(root) = LOG_ROOT.get() else { return };
    let Ok(_guard) = LOG_LOCK.lock() else { return };
    let directory = root.join(directory);
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let date = Local::now().format("%Y-%m-%d").to_string();
    let path = directory.join(format!("{date}.log"));
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let event = sanitize_text(event, 128);
    let details = sanitize_text(details, 2048);
    let _ = writeln!(file, "{timestamp}\t{event}\t{details}");
    let _ = file.flush();
}

fn current_level() -> LogLevel {
    LOG_LEVEL.get().copied().unwrap_or_else(log_level)
}

fn configured_level() -> LogLevel {
    CONFIGURED_LEVEL
        .get()
        .and_then(|level| level.lock().ok().map(|level| *level))
        .unwrap_or_else(current_level)
}

fn log_level() -> LogLevel {
    parse_log_level(&std::env::var("WECOM_ARCHIVE_SERVER_LOG").unwrap_or_default())
}

fn parse_log_level(value: &str) -> LogLevel {
    match value.to_ascii_lowercase().as_str() {
        "off" | "0" | "false" => LogLevel::Off,
        "verbose" | "trace" | "2" => LogLevel::Verbose,
        _ => LogLevel::Normal,
    }
}

fn safe_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        "unknown-collector".into()
    } else {
        cleaned.into()
    }
}

fn sanitize_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '\r' | '\n' | '\t'))
        .take(max_chars)
        .collect()
}
