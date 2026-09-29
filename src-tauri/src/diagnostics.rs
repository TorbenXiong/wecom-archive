use chrono::Local;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<Mutex<Option<LogFile>>> = OnceLock::new();
static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();
static EVENTS: OnceLock<Mutex<VecDeque<DiagnosticEvent>>> = OnceLock::new();
static CONFIGURED_LEVEL: OnceLock<Mutex<LogLevel>> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEvent {
    pub timestamp: u64,
    pub event: String,
    pub details: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Off,
    Normal,
    Verbose,
}

pub fn init() {
    let _ = LOG_LEVEL.set(log_level());
    let _ = CONFIGURED_LEVEL.set(Mutex::new(log_level()));
    let _ = EVENTS.set(Mutex::new(VecDeque::new()));
    let _ = LOG_FILE.set(Mutex::new(None));
}

pub fn set_level(value: &str) {
    if let Some(level) = CONFIGURED_LEVEL.get() {
        if let Ok(mut level) = level.lock() {
            *level = parse_log_level(value);
        }
    }
}

pub fn write(event: &str, details: &str) {
    write_at(LogLevel::Normal, event, details);
}

pub fn verbose(event: &str, details: &str) {
    write_at(LogLevel::Verbose, event, details);
}

fn write_at(required: LogLevel, event: &str, details: &str) {
    if configured_level() == LogLevel::Off
        || (required == LogLevel::Verbose && configured_level() != LogLevel::Verbose)
    {
        return;
    }
    let Some(file) = ensure_log_file() else {
        return;
    };
    let Ok(mut state) = file.lock() else { return };
    let date = Local::now().format("%Y-%m-%d").to_string();
    if state.as_ref().is_none_or(|current| current.date != date) {
        let Some(path) = log_path(&date) else { return };
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        *state = Some(LogFile { date, file });
    }
    let Some(file) = state.as_mut().map(|current| &mut current.file) else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    let details = details.replace(['\r', '\n'], " ");
    if let Some(events) = EVENTS.get() {
        if let Ok(mut events) = events.lock() {
            events.push_back(DiagnosticEvent {
                timestamp,
                event: event.to_owned(),
                details: details.clone(),
            });
            while events.len() > 100 {
                events.pop_front();
            }
        }
    }
    let log_timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let _ = writeln!(file, "{log_timestamp}\t{event}\t{details}");
    let _ = file.flush();
}

pub fn take_events() -> Vec<DiagnosticEvent> {
    EVENTS
        .get()
        .and_then(|events| {
            events
                .lock()
                .ok()
                .map(|mut events| events.drain(..).collect())
        })
        .unwrap_or_default()
}

pub fn restore_events(mut restored: Vec<DiagnosticEvent>) {
    if let Some(events) = EVENTS.get() {
        if let Ok(mut events) = events.lock() {
            restored.append(&mut events.drain(..).collect());
            restored.truncate(100);
            *events = restored.into_iter().collect();
        }
    }
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
    parse_log_level(
        &std::env::var("WECOM_ARCHIVE_COLLECTOR_LOG")
            .ok()
            .unwrap_or_default(),
    )
}

fn parse_log_level(value: &str) -> LogLevel {
    match value.to_ascii_lowercase().as_str() {
        "normal" | "1" | "true" => LogLevel::Normal,
        "verbose" | "trace" | "2" => LogLevel::Verbose,
        _ => LogLevel::Off,
    }
}

fn log_path(date: &str) -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|path| {
        path.parent().map(|parent| {
            parent
                .join("collectorData")
                .join("logs")
                .join(format!("{date}.log"))
        })
    })
}

fn ensure_log_file() -> Option<&'static Mutex<Option<LogFile>>> {
    let _ = LOG_FILE.set(Mutex::new(None));
    LOG_FILE.get()
}

struct LogFile {
    date: String,
    file: File,
}

#[cfg(test)]
mod tests {
    use super::{LogLevel, parse_log_level};

    #[test]
    fn collector_logging_is_off_without_an_explicit_enable_value() {
        assert!(parse_log_level("") == LogLevel::Off);
        assert!(parse_log_level("invalid") == LogLevel::Off);
        assert!(parse_log_level("off") == LogLevel::Off);
        assert!(parse_log_level("normal") == LogLevel::Normal);
    }
}
