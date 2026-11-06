use archive_domain::{ClientExportV1, SourceCandidate};
use chrono::Utc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommandError {
    pub(super) code: &'static str,
    pub(super) message: String,
    pub(super) recoverable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectionSummary {
    pub(super) export_id: String,
    pub(super) generated_at: String,
    pub(super) message_count: u64,
    pub(super) media_count: u64,
    pub(super) missing_media_count: u64,
    pub(super) content_sha256_prefix: String,
}

impl From<&ClientExportV1> for CollectionSummary {
    fn from(export: &ClientExportV1) -> Self {
        Self {
            export_id: export.export_id.to_string(),
            generated_at: export.generated_at.to_rfc3339(),
            message_count: export.message_count,
            media_count: export.media_count,
            missing_media_count: export.missing_media_count,
            content_sha256_prefix: export.content_sha256.chars().take(12).collect(),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct AppState {
    pub(super) pending_update:
        Arc<Mutex<Option<archive_transfer::software_update::SignedSoftwareUpdate>>>,
    pub(super) update_attempt: Arc<Mutex<Option<String>>>,
    pub(super) software_update_error: Arc<Mutex<Option<String>>>,
    pub(super) discovered_sources: Arc<Mutex<BTreeMap<String, SourceCandidate>>>,
    pub(super) latest_export: Arc<Mutex<Option<Arc<ClientExportV1>>>>,
    pub(super) collection_running: Arc<AtomicBool>,
    pub(super) scheduler_shutdown: Arc<AtomicBool>,
    pub(super) schedule_status: Arc<Mutex<CollectorScheduleStatus>>,
    pub(super) uploaded_messages: Arc<Mutex<BTreeMap<String, String>>>,
    pub(super) tray_app: Arc<Mutex<Option<tauri::AppHandle>>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectorScheduleStatus {
    pub(super) running: bool,
    pub(super) last_attempt_at: Option<String>,
    pub(super) last_success_at: Option<String>,
    pub(super) last_error: Option<String>,
    pub(super) last_message_count: Option<u64>,
    pub(super) last_media_count: Option<u64>,
    pub(super) uploaded_count: u64,
    pub(super) next_run_at: Option<String>,
    #[serde(skip)]
    pub(super) manual_collection_attempted_id: Option<String>,
    #[serde(skip)]
    pub(super) server_authorization_denied: bool,
    pub(super) last_manual_collection_request_id: Option<String>,
}

pub(super) fn begin_collector_attempt(state: &AppState) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.running = true;
        status.last_attempt_at = Some(Utc::now().to_rfc3339());
        status.last_error = None;
    }
}

pub(super) fn finish_collector_attempt<T>(state: &AppState, result: &Result<T, CommandError>) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.running = false;
        if let Err(error) = result {
            status.last_error = Some(error.message.clone());
        }
    }
}

pub(super) struct CollectionRunGuard(pub(super) Arc<AtomicBool>);

impl Drop for CollectionRunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) fn internal_error() -> CommandError {
    CommandError {
        code: "INTERNAL_ERROR",
        message: "本地操作失败，诊断信息已脱敏。".into(),
        recoverable: false,
    }
}

pub(super) fn sanitize_error(value: &str) -> String {
    value
        .split_whitespace()
        .map(|part| {
            if part.contains(':') || part.contains('\\') || part.contains('/') {
                "[已脱敏]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
