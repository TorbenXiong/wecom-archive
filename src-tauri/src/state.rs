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
    pub(super) pending_upload: Arc<Mutex<Option<crate::collector_upload::PendingUpload>>>,
    pub(super) confirmed_upload: Arc<Mutex<Option<crate::collector_upload::UploadReceipt>>>,
    pub(super) tray_app: Arc<Mutex<Option<tauri::AppHandle>>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectorScheduleStatus {
    pub(super) running: bool,
    pub(super) last_attempt_at: Option<String>,
    pub(super) last_success_at: Option<String>,
    pub(super) last_upload_message_count: Option<u64>,
    pub(super) last_error: Option<String>,
    pub(super) upload_stage: Option<String>,
    pub(super) upload_result_unknown: bool,
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
        status.upload_stage = None;
    }
}

pub(super) fn finish_collector_attempt<T>(state: &AppState, result: &Result<T, CommandError>) {
    // A different local operation or configuration error must not hide an
    // unresolved upload. Match the pending-before-status lock order below.
    let unresolved = state
        .pending_upload
        .lock()
        .map_or(true, |pending| pending.is_some());
    if let Ok(mut status) = state.schedule_status.lock() {
        status.running = false;
        status.upload_stage = None;
        status.upload_result_unknown = unresolved
            || result
                .as_ref()
                .err()
                .is_some_and(|error| error.code == "ENTERPRISE_UPLOAD_RESULT_UNKNOWN");
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

/// Own the lock and completion bookkeeping inside the worker, so cancelling a
/// WebView command cannot release them while curl or archive preparation runs.
pub(super) async fn run_owned_collector_attempt<T: Send + 'static>(
    state: AppState,
    guard: CollectionRunGuard,
    work: impl FnOnce(&AppState) -> Result<T, CommandError> + Send + 'static,
) -> Result<T, CommandError> {
    let result_state = state.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _run_guard = guard;
        struct Completion<'a>(&'a AppState);
        impl Drop for Completion<'_> {
            fn drop(&mut self) {
                let unresolved = self
                    .0
                    .pending_upload
                    .lock()
                    .map_or(true, |pending| pending.is_some());
                if let Ok(mut status) = self.0.schedule_status.lock()
                    && status.running
                {
                    status.running = false;
                    status.upload_stage = None;
                    status.upload_result_unknown = unresolved;
                    status.last_error = Some("本地任务异常结束，结果需要重新确认。".into());
                }
            }
        }
        let _completion = Completion(&state);
        let result = work(&state);
        finish_collector_attempt(&state, &result);
        result
    })
    .await
    .unwrap_or_else(|_| {
        if result_state
            .pending_upload
            .lock()
            .map_or(true, |pending| pending.is_some())
        {
            Err(CommandError {
                code: "ENTERPRISE_UPLOAD_RESULT_UNKNOWN",
                message: "上传任务异常结束，结果暂未确认，请继续确认原上传。".into(),
                recoverable: true,
            })
        } else {
            Err(internal_error())
        }
    })
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

#[cfg(test)]
mod owned_attempt_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn uncertain_pending_state_is_not_hidden_by_another_local_result() {
        let state = AppState::default();
        let pending = Arc::clone(&state.pending_upload);
        assert!(
            std::thread::spawn(move || {
                let _held = pending.lock().unwrap();
                panic!("SYNTHETIC_PENDING_STATE_POISON");
            })
            .join()
            .is_err()
        );
        begin_collector_attempt(&state);
        finish_collector_attempt(&state, &Ok::<_, CommandError>(()));
        assert!(state.schedule_status.lock().unwrap().upload_result_unknown);
        assert!(!state.schedule_status.lock().unwrap().running);
        begin_collector_attempt(&state);
        finish_collector_attempt::<()>(&state, &Err(internal_error()));
        assert!(state.schedule_status.lock().unwrap().upload_result_unknown);
        assert!(!state.scheduler_shutdown.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_command_keeps_the_worker_lock_and_completion_bookkeeping() {
        let state = AppState::default();
        state.collection_running.store(true, Ordering::Release);
        begin_collector_attempt(&state);
        let guard = CollectionRunGuard(Arc::clone(&state.collection_running));
        let (ready, started) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let request = tauri::async_runtime::spawn(async move {
            run_owned_collector_attempt(worker_state, guard, move |state| {
                ready.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| internal_error())?;
                state.schedule_status.lock().unwrap().last_success_at =
                    Some("2026-01-02T03:04:05Z".into());
                Ok::<_, CommandError>(())
            })
            .await
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        request.abort();
        assert!(tauri::async_runtime::block_on(request).is_err());
        assert!(state.collection_running.load(Ordering::Acquire));
        assert!(state.schedule_status.lock().unwrap().running);
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.collection_running.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "OWNED_ATTEMPT_TIMEOUT"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let final_status = state.schedule_status.lock().unwrap();
        assert!(!final_status.running);
        assert!(final_status.last_error.is_none());
        assert_eq!(
            final_status.last_success_at.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
    }
}
