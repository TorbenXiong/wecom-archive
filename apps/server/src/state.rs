use crate::EnterpriseConfig;
use archive_domain::ClientExportV1;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore, watch};

#[derive(Clone)]
pub(super) struct AppState {
    pub(super) _data_lock: Option<Arc<std::fs::File>>,
    pub(super) data_root: Arc<PathBuf>,
    pub(super) archive_path: Arc<PathBuf>,
    pub(super) token_sha256: Arc<Mutex<[u8; 32]>>,
    pub(super) access_token_path: Arc<PathBuf>,
    pub(super) enterprise_config: Arc<Mutex<EnterpriseConfig>>,
    pub(super) collector_template: Option<Arc<PathBuf>>,
    pub(super) local_collector: Option<LocalCollector>,
    pub(super) local_collection_progress: Arc<Mutex<LocalCollectionProgress>>,
    pub(super) local_collection_running: Arc<AtomicBool>,
    pub(super) archive_update: watch::Sender<u64>,
    pub(super) network_mode: &'static str,
    pub(super) listen_address: String,
    pub(super) export_tasks: Arc<Mutex<HashMap<String, Arc<ExportTask>>>>,
    pub(super) export_slots: Arc<Semaphore>,
    pub(super) import_slots: Arc<Semaphore>,
    pub(super) tasks: Arc<Mutex<crate::tasks::TaskRegistry>>,
    pub(super) collector_uploads: Arc<Mutex<crate::collector_uploads::UploadRegistry>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportTaskProgress {
    pub status: String,
    pub percent: u8,
    pub stage: String,
    pub result: Option<crate::ExportResponse>,
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub processed_messages: u64,
    pub total_messages: Option<u64>,
    #[serde(skip)]
    pub(super) finished_at: Option<Instant>,
}

pub(crate) struct ExportTask {
    pub cancelled: AtomicBool,
    pub(super) cancellation: Notify,
    pub progress: Mutex<ExportTaskProgress>,
}

impl ExportTask {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            cancellation: Notify::new(),
            progress: Mutex::new(ExportTaskProgress {
                status: "queued".into(),
                percent: 0,
                stage: "排队中".into(),
                result: None,
                error: None,
                error_code: None,
                processed_messages: 0,
                total_messages: None,
                finished_at: None,
            }),
        }
    }

    pub(crate) fn expired(&self) -> bool {
        // A missing completion time or poisoned state has an unknown outcome.
        // Capacity pressure must not turn either into an eviction decision.
        self.progress.lock().is_ok_and(|progress| {
            matches!(
                progress.status.as_str(),
                "completed" | "failed" | "cancelled"
            ) && progress
                .finished_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(60 * 60))
        })
    }

    pub(crate) fn finish(&self, result: &Result<crate::ExportResponse, crate::ApiError>) {
        if let Ok(mut progress) = self.progress.lock()
            && progress.finished_at.is_none()
        {
            match result {
                Ok(response) => {
                    progress.status = "completed".into();
                    progress.stage = "已完成".into();
                    progress.percent = 100;
                    progress.result = Some(response.clone());
                }
                Err(_) if self.cancelled.load(Ordering::Acquire) => {
                    progress.status = "cancelled".into();
                    progress.stage = "已取消，未完成文件已清理".into();
                }
                Err(error) => {
                    progress.status = "failed".into();
                    progress.stage = "导出失败".into();
                    progress.error = Some(error.body.message.clone());
                    progress.error_code = Some(error.body.code.into());
                }
            }
            progress.finished_at = Some(Instant::now());
        }
    }

    pub fn cancel(&self) {
        if let Ok(mut progress) = self.progress.lock()
            && progress.finished_at.is_none()
            && matches!(
                progress.status.as_str(),
                "queued" | "running" | "cancelling"
            )
        {
            self.cancelled.store(true, Ordering::Release);
            progress.status = "cancelling".into();
            progress.stage = "正在取消并清理临时文件".into();
            self.cancellation.notify_one();
        }
    }

    pub fn update(&self, percent: u8, stage: &str) {
        if let Ok(mut progress) = self.progress.lock() {
            if self.cancelled.load(Ordering::Acquire)
                || progress.finished_at.is_some()
                || matches!(
                    progress.status.as_str(),
                    "completed" | "failed" | "cancelled"
                )
            {
                return;
            }
            progress.status = "running".into();
            progress.percent = percent;
            progress.stage = stage.into();
        }
    }
}

pub type LocalCollectionProgressReporter = Arc<dyn Fn(u8, &str, &str) + Send + Sync>;
pub type LocalCollectionMediaSink =
    Arc<dyn Fn(&Path, &str, u64) -> Result<(), String> + Send + Sync>;
pub(super) type LocalCollector = Arc<
    dyn Fn(
            bool,
            bool,
            Option<i64>,
            Option<String>,
            LocalCollectionProgressReporter,
            LocalCollectionMediaSink,
        ) -> Result<ClientExportV1, String>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCollectionProgress {
    pub(super) running: bool,
    pub(super) percent: u8,
    pub(super) stage: String,
    pub(super) detail: String,
}

impl Default for LocalCollectionProgress {
    fn default() -> Self {
        Self {
            running: false,
            percent: 0,
            stage: "等待开始".into(),
            detail: "选择采集范围后开始处理。".into(),
        }
    }
}
