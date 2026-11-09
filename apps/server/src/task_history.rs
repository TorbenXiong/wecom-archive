//! Durable, payload-free receipts. An interrupted receipt is never a failure receipt.
use crate::{ApiError, AppState, ExportResponse, ImportResponse, authorize};
use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryRecord {
    pub task_id: String,
    pub kind: String,
    pub status: String,
    pub started_at: String,
    pub updated_at: String,
    pub received_bytes: u64,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_status: Option<u16>,
    pub import_result: Option<ImportResponse>,
    pub output: Option<OutputReceipt>,
    pub verification: Option<Verification>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OutputReceipt {
    pub file_name: String,
    pub export_id: String,
    pub message_count: u64,
    pub media_count: u64,
    pub missing_media_count: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Verification {
    pub revision: u64,
    pub database_ok: bool,
    pub checked_media: u64,
    pub missing_files: u64,
    pub damaged_files: u64,
    pub unreadable_files: u64,
}

pub(crate) struct TaskHistory {
    root: PathBuf,
    inner: Mutex<HistoryState>,
}

#[derive(Default)]
struct HistoryState {
    records: HashMap<String, HistoryRecord>,
    persistence_error: bool,
}

#[derive(Clone)]
pub(crate) struct HistoryHandle {
    pub id: String,
    history: Arc<TaskHistory>,
}

fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "unknown")
}

impl HistoryRecord {
    fn validate(&self) -> Result<(), ApiError> {
        let known_kind = matches!(
            self.kind.as_str(),
            "import"
                | "collection"
                | "collector_upload"
                | "export"
                | "local_export"
                | "health_check"
        );
        let known_status = matches!(
            self.status.as_str(),
            "queued"
                | "receiving"
                | "running"
                | "cancelling"
                | "completed"
                | "failed"
                | "cancelled"
                | "unknown"
        );
        let valid_results = if self.status == "completed" {
            match self.kind.as_str() {
                "import" | "collection" | "collector_upload" => {
                    self.import_result.is_some()
                        && self.output.is_none()
                        && self.verification.is_none()
                }
                "export" | "local_export" => {
                    self.output.is_some()
                        && self.import_result.is_none()
                        && self.verification.is_none()
                }
                "health_check" => {
                    self.verification.is_some()
                        && self.import_result.is_none()
                        && self.output.is_none()
                }
                _ => false,
            }
        } else {
            self.import_result.is_none() && self.output.is_none() && self.verification.is_none()
        };
        // A parseable status alone is not evidence of successful completion.
        // Reject inconsistent receipts before changing recovery state on disk.
        if !known_kind
            || !known_status
            || !valid_results
            || chrono::DateTime::parse_from_rfc3339(&self.started_at).is_err()
            || chrono::DateTime::parse_from_rfc3339(&self.updated_at).is_err()
        {
            return Err(ApiError::store());
        }
        Ok(())
    }
}

impl TaskHistory {
    pub fn open(root: &Path) -> Result<Arc<Self>, ApiError> {
        let root = root.join("task-history");
        std::fs::create_dir_all(&root).map_err(|_| ApiError::store())?;
        if source_windows::atomic_file::is_link(
            &std::fs::symlink_metadata(&root).map_err(|_| ApiError::store())?,
        ) {
            return Err(ApiError::store());
        }
        let mut records = HashMap::new();
        for entry in std::fs::read_dir(&root).map_err(|_| ApiError::store())? {
            let entry = entry.map_err(|_| ApiError::store())?;
            if entry.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path()).map_err(|_| ApiError::store())?;
            if !meta.is_file() || source_windows::atomic_file::is_link(&meta) {
                return Err(ApiError::store());
            }
            let mut record: HistoryRecord = serde_json::from_slice(
                &std::fs::read(entry.path()).map_err(|_| ApiError::store())?,
            )
            .map_err(|_| ApiError::store())?;
            let id = uuid::Uuid::parse_str(&record.task_id).map_err(|_| ApiError::store())?;
            if id.is_nil()
                || record.task_id != id.to_string()
                || entry.file_name().to_str() != Some(&format!("{id}.json"))
            {
                return Err(ApiError::store());
            }
            record.validate()?;
            if let Some(output) = &record.output
                && (Path::new(&output.file_name)
                    .file_name()
                    .and_then(|v| v.to_str())
                    != Some(&output.file_name)
                    || output.file_name.contains(['/', '\\', ':'])
                    || !crate::is_valid_content_hash(&output.sha256))
            {
                return Err(ApiError::store());
            }
            if !terminal(&record.status) {
                record.status = "unknown".into();
                record.updated_at = Utc::now().to_rfc3339();
                source_windows::atomic_file::write(
                    &entry.path(),
                    &serde_json::to_vec(&record).map_err(|_| ApiError::store())?,
                )
                .map_err(|_| ApiError::store())?;
            }
            records.insert(record.task_id.clone(), record);
        }
        Ok(Arc::new(Self {
            root,
            inner: Mutex::new(HistoryState {
                records,
                persistence_error: false,
            }),
        }))
    }

    pub fn get(&self, id: &str) -> Result<Option<HistoryRecord>, ApiError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| ApiError::store())?
            .records
            .get(id)
            .cloned())
    }

    pub fn begin(self: &Arc<Self>, id: String, kind: &str) -> Result<HistoryHandle, ApiError> {
        let parsed = uuid::Uuid::parse_str(&id).map_err(|_| ApiError::invalid_query())?;
        if parsed.is_nil() || parsed.to_string() != id {
            return Err(ApiError::invalid_query());
        }
        let mut inner = self.inner.lock().map_err(|_| ApiError::store())?;
        if inner.records.contains_key(&id) {
            return Err(crate::tasks::duplicate_id());
        }
        let now = Utc::now().to_rfc3339();
        let record = HistoryRecord {
            task_id: id.clone(),
            kind: kind.into(),
            status: "queued".into(),
            started_at: now.clone(),
            updated_at: now,
            received_bytes: 0,
            phase: "等待执行".into(),
            error_code: None,
            error_status: None,
            import_result: None,
            output: None,
            verification: None,
        };
        self.save(&record).inspect_err(|_| {
            inner.persistence_error = true;
        })?;
        inner.records.insert(id.clone(), record);
        Ok(HistoryHandle {
            id,
            history: Arc::clone(self),
        })
    }

    fn save(&self, record: &HistoryRecord) -> Result<(), ApiError> {
        let bytes = serde_json::to_vec(record).map_err(|_| ApiError::store())?;
        source_windows::atomic_file::write(
            &self.root.join(format!("{}.json", record.task_id)),
            &bytes,
        )
        .map_err(|_| ApiError::store())
    }
}

impl HistoryHandle {
    pub fn interrupted(&self) {
        self.change(|record| record.status = "unknown".into());
    }
    fn change(&self, edit: impl FnOnce(&mut HistoryRecord)) {
        if let Ok(mut inner) = self.history.inner.lock() {
            let Some(record) = inner.records.get_mut(&self.id) else {
                return;
            };
            if terminal(&record.status) {
                return;
            }
            edit(record);
            record.updated_at = Utc::now().to_rfc3339();
            if self.history.save(record).is_err() {
                inner.persistence_error = true;
            }
        }
    }

    pub fn progress(&self, status: &str, received: u64) {
        self.stage(status, "", received);
    }

    pub fn stage(&self, status: &str, stage: &str, received: u64) {
        // Only fixed product labels can cross the persistence boundary. Source
        // adapters may report arbitrary details; those never enter the receipt.
        let phase = match stage {
            "接收导入文件"
            | "接收加密上传"
            | "解密并校验导入文件"
            | "校验导入文件"
            | "校验并保存附件"
            | "写入归档并提交事务"
            | "准备本机采集" => stage,
            _ => match status {
                "queued" => "等待执行",
                "receiving" => "接收数据",
                "cancelling" => "取消并清理",
                _ => "处理数据",
            },
        };
        // Reception reports every chunk. Persist phase changes, not every chunk.
        if let Ok(Some(record)) = self.history.get(&self.id)
            && record.status == status
            && record.phase == phase
        {
            return;
        }
        self.change(|record| {
            record.status = status.into();
            record.phase = phase.into();
            record.received_bytes = received;
        });
    }

    pub fn finish_import(&self, result: &Result<Json<ImportResponse>, ApiError>) {
        self.change(|record| match result {
            Ok(response) => {
                record.status = "completed".into();
                record.import_result = Some(response.0.clone());
            }
            Err(error) => {
                record.status = "failed".into();
                record.error_code = Some(error.body.code.into());
                record.error_status = Some(error.status.as_u16());
            }
        });
    }

    pub fn finish_export(&self, result: &Result<ExportResponse, ApiError>, cancelled: bool) {
        self.change(|record| match result {
            Ok(response) => {
                record.status = "completed".into();
                record.output = Some(OutputReceipt {
                    file_name: response.file_name.clone(),
                    export_id: response.export_id.clone(),
                    message_count: response.message_count,
                    media_count: response.media_count,
                    missing_media_count: response.missing_media_count,
                    sha256: response.manifest_sha256.clone(),
                });
            }
            Err(error) => {
                record.status = if cancelled { "cancelled" } else { "failed" }.into();
                record.error_code = Some(error.body.code.into());
                record.error_status = Some(error.status.as_u16());
            }
        });
    }

    pub fn finish_verification(&self, result: &Result<Verification, ApiError>) {
        self.change(|record| match result {
            Ok(verification) => {
                record.status = "completed".into();
                record.verification = Some(verification.clone());
            }
            Err(error) => {
                record.status = "failed".into();
                record.error_code = Some(error.body.code.into());
                record.error_status = Some(error.status.as_u16());
            }
        });
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct HistoryQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub kind: Option<String>,
    pub status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryPage {
    pub records: Vec<HistoryRecord>,
    pub total: usize,
    pub persistence_error: bool,
}

pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryPage>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let inner = state
        .task_history
        .inner
        .lock()
        .map_err(|_| ApiError::store())?;
    let mut rows: Vec<_> = inner
        .records
        .values()
        .filter(|r| {
            query.kind.as_ref().is_none_or(|kind| &r.kind == kind)
                && query
                    .status
                    .as_ref()
                    .is_none_or(|status| &r.status == status)
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.task_id.cmp(&a.task_id))
    });
    let total = rows.len();
    Ok(Json(HistoryPage {
        records: rows
            .into_iter()
            .skip(query.offset.unwrap_or(0))
            .take(query.limit.unwrap_or(50).clamp(1, 200))
            .collect(),
        total,
        persistence_error: inner.persistence_error,
    }))
}

pub(crate) async fn latest_verification(
    history: &TaskHistory,
) -> Result<Option<HistoryRecord>, ApiError> {
    let inner = history.inner.lock().map_err(|_| ApiError::store())?;
    Ok(inner
        .records
        .values()
        .filter(|r| r.kind == "health_check")
        .max_by(|a, b| a.started_at.cmp(&b.started_at))
        .cloned())
}
