//! Optional request identity for recovering long-running import/collection results.
//! Records contain no payloads, file names, source paths or credentials.
use crate::{ApiError, AppState, ErrorResponse, ImportResponse, authorize};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::Utc;
use serde::Serialize;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const RETENTION: Duration = Duration::from_secs(60 * 60);
const MAX_TASKS: usize = 128;

#[derive(Default)]
pub(crate) struct TaskRegistry(HashMap<String, Arc<Task>>);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProgress {
    pub task_id: String,
    pub kind: &'static str,
    pub status: &'static str,
    pub stage: String,
    pub received_bytes: u64,
    pub started_at: String,
    pub updated_at: String,
    pub result: Option<ImportResponse>,
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub error_status: Option<u16>,
    #[serde(skip)]
    finished_at: Option<Instant>,
}

pub(crate) struct Task(
    Mutex<TaskProgress>,
    Option<crate::task_history::HistoryHandle>,
);

fn error(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError {
        status,
        body: ErrorResponse {
            code,
            message: message.into(),
            recoverable: true,
        },
    }
}

pub(crate) fn request_id(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    headers
        .get("x-archive-task-id")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
                .filter(|id| !id.is_nil())
                .map(|id| id.to_string())
                .ok_or_else(|| error(StatusCode::BAD_REQUEST, "INVALID_TASK_ID", "任务标识无效。"))
        })
        .transpose()
}

pub(crate) fn duplicate_id() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "TASK_ALREADY_EXISTS",
        "该任务已登记，请查询原任务状态，不要重复提交。",
    )
}

impl TaskRegistry {
    fn prune(&mut self) {
        // A poisoned record has an unknown outcome; never evict it as if it
        // were a confirmed, expired terminal task.
        self.0.retain(|_, task| {
            task.0.lock().map_or(true, |p| {
                p.finished_at.is_none_or(|at| at.elapsed() < RETENTION)
            })
        });
    }
}

impl Task {
    pub fn register(
        state: &AppState,
        headers: &HeaderMap,
        kind: &'static str,
    ) -> Result<Option<Arc<Self>>, ApiError> {
        let Some(id) = request_id(headers)? else {
            return Ok(None);
        };
        let mut tasks = state.tasks.lock().map_err(|_| ApiError::store())?;
        tasks.prune();
        if tasks.0.contains_key(&id) {
            return Err(duplicate_id());
        }
        if tasks.0.len() >= MAX_TASKS {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "TASK_CAPACITY_REACHED",
                "任务记录已满，请稍后重试。",
            ));
        }
        let task = Self::create_tracked(state, id.clone(), kind)?;
        tasks.0.insert(id, Arc::clone(&task));
        Ok(Some(task))
    }

    pub(crate) fn create(id: String, kind: &'static str) -> Arc<Self> {
        let now = Utc::now().to_rfc3339();
        Arc::new(Self(
            Mutex::new(TaskProgress {
                task_id: id,
                kind,
                status: "queued",
                stage: "等待执行".into(),
                received_bytes: 0,
                started_at: now.clone(),
                updated_at: now,
                result: None,
                error: None,
                error_code: None,
                error_status: None,
                finished_at: None,
            }),
            None,
        ))
    }

    pub(crate) fn create_tracked(
        state: &AppState,
        id: String,
        kind: &'static str,
    ) -> Result<Arc<Self>, ApiError> {
        let history = state.task_history.begin(id.clone(), kind)?;
        Ok(Self::with_history(id, kind, history))
    }

    pub(crate) fn with_history(
        id: String,
        kind: &'static str,
        history: crate::task_history::HistoryHandle,
    ) -> Arc<Self> {
        let mut task = Self::create(id, kind);
        Arc::get_mut(&mut task).expect("new task is exclusive").1 = Some(history);
        task
    }

    pub(crate) fn snapshot(&self) -> Result<TaskProgress, ApiError> {
        Ok(self.0.lock().map_err(|_| ApiError::store())?.clone())
    }

    pub(crate) fn expired(&self) -> bool {
        self.0
            .lock()
            .is_ok_and(|p| p.finished_at.is_some_and(|at| at.elapsed() >= RETENTION))
    }

    pub fn update(&self, status: &'static str, stage: &str, received: Option<u64>) {
        if let Ok(mut progress) = self.0.lock()
            && progress.finished_at.is_none()
        {
            progress.status = status;
            progress.stage = stage.into();
            if let Some(bytes) = received {
                progress.received_bytes = bytes;
            }
            progress.updated_at = Utc::now().to_rfc3339();
            if let Some(history) = &self.1 {
                history.stage(status, stage, progress.received_bytes);
            }
        }
    }

    fn finish(&self, result: &Result<Json<ImportResponse>, ApiError>) {
        if let Ok(mut progress) = self.0.lock()
            && progress.finished_at.is_none()
        {
            match result {
                Ok(result) => {
                    progress.status = "completed";
                    progress.stage = "已完成".into();
                    progress.result = Some(result.0.clone());
                }
                Err(error) => {
                    progress.status = "failed";
                    progress.stage = "任务未完成".into();
                    progress.error = Some(error.body.message.clone());
                    progress.error_code = Some(error.body.code.into());
                    progress.error_status = Some(error.status.as_u16());
                }
            }
            progress.updated_at = Utc::now().to_rfc3339();
            progress.finished_at = Some(Instant::now());
            if let Some(history) = &self.1 {
                history.finish_import(result);
            }
        }
    }
}

/// The observer owns the worker handle even when the HTTP handler disappears.
/// It also converts worker panics into a terminal, queryable result.
pub(crate) async fn run_detached(
    task: Option<Arc<Task>>,
    work: impl Future<Output = Result<Json<ImportResponse>, ApiError>> + Send + 'static,
) -> Result<Json<ImportResponse>, ApiError> {
    let worker = tokio::spawn(work);
    tokio::spawn(async move {
        let result = worker.await.unwrap_or_else(|_| Err(ApiError::store()));
        if let Some(task) = task {
            task.finish(&result);
        }
        result
    })
    .await
    .map_err(|_| ApiError::store())?
}

/// Legacy imports retain their original cancellation behavior while recording
/// interruption as an unknown outcome, since a blocking worker may still commit.
pub(crate) async fn run_inline(
    task: Arc<Task>,
    work: impl Future<Output = Result<Json<ImportResponse>, ApiError>>,
) -> Result<Json<ImportResponse>, ApiError> {
    struct Interrupted(Arc<Task>);
    impl Drop for Interrupted {
        fn drop(&mut self) {
            if let Some(history) = &self.0.1 {
                history.interrupted();
            }
        }
    }
    let _guard = Interrupted(Arc::clone(&task));
    let result = work.await;
    task.finish(&result);
    result
}

pub(crate) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<TaskProgress>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let id = Uuid::parse_str(&id)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "INVALID_TASK_ID", "任务标识无效。"))?
        .to_string();
    let task = {
        let mut tasks = state.tasks.lock().map_err(|_| ApiError::store())?;
        tasks.prune();
        tasks.0.get(&id).cloned()
    };
    if let Some(task) = task {
        return Ok(Json(task.snapshot()?));
    }
    if let Some(record) = state.task_history.get(&id)?
        && matches!(record.kind.as_str(), "import" | "collection")
    {
        let status = match record.status.as_str() {
            "completed" => "completed",
            "failed" => "failed",
            _ => "unknown",
        };
        return Ok(Json(TaskProgress {
            task_id: record.task_id,
            kind: if record.kind == "import" {
                "import"
            } else {
                "collection"
            },
            status,
            stage: if status == "unknown" {
                "任务结果待核对，请检查归档"
            } else {
                "历史任务"
            }
            .into(),
            received_bytes: record.received_bytes,
            started_at: record.started_at,
            updated_at: record.updated_at,
            result: record.import_result,
            error: (status == "failed").then(|| "任务未完成，请核对归档后再操作。".into()),
            error_code: record.error_code,
            error_status: record.error_status,
            finished_at: None,
        }));
    }
    Err({
        error(
            StatusCode::NOT_FOUND,
            "TASK_NOT_FOUND",
            "无法查询此任务；服务端可能已重启或记录已过期，请先核对归档结果。",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(finished_at: Option<Instant>) -> Arc<Task> {
        Arc::new(Task(
            Mutex::new(TaskProgress {
                task_id: Uuid::new_v4().to_string(),
                kind: "import",
                status: if finished_at.is_some() {
                    "completed"
                } else {
                    "running"
                },
                stage: "synthetic".into(),
                received_bytes: 0,
                started_at: String::new(),
                updated_at: String::new(),
                result: None,
                error: None,
                error_code: None,
                error_status: None,
                finished_at,
            }),
            None,
        ))
    }

    #[test]
    fn pruning_retains_active_recent_and_unknown_records() {
        let active = record(None);
        let poisoned = record(None);
        let poison = Arc::clone(&poisoned);
        assert!(
            std::thread::spawn(move || {
                let _held = poison.0.lock().unwrap();
                panic!("SYNTHETIC_RECORD_POISON");
            })
            .join()
            .is_err()
        );
        let mut registry = TaskRegistry(HashMap::from([
            ("active".into(), active),
            ("recent".into(), record(Some(Instant::now()))),
            (
                "expired".into(),
                record(Some(Instant::now() - RETENTION - Duration::from_secs(1))),
            ),
            ("unknown".into(), poisoned),
        ]));
        registry.prune();
        assert_eq!(registry.0.len(), 3);
        assert!(registry.0.contains_key("active"));
        assert!(registry.0.contains_key("recent"));
        assert!(registry.0.contains_key("unknown"));
        assert!(!registry.0.contains_key("expired"));
    }
}
