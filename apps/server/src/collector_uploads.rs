//! Collector results have separate ownership and quotas from workspace tasks.
use crate::tasks::{Task, run_detached};
use crate::{ApiError, AppState, ErrorResponse, ImportResponse, authorize_enterprise_upload};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

const MAX_RECORDS: usize = 4096;
const MAX_DEVICE_RECORDS: usize = 64;
const MAX_DEVICE_ACTIVE: usize = 2;

#[derive(Clone)]
pub(crate) struct UploadIdentity {
    pub export_id: Uuid,
    pub content_sha256: String,
}

#[derive(Clone)]
struct UploadRecord {
    identity: UploadIdentity,
    task: Arc<Task>,
}

#[derive(Default)]
pub(crate) struct UploadRegistry(HashMap<(String, String), UploadRecord>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadStatus {
    schema: &'static str,
    upload_id: String,
    export_id: String,
    content_sha256: String,
    status: &'static str,
    stage: String,
    received_bytes: u64,
    result: Option<ImportResponse>,
    error: Option<String>,
    error_code: Option<&'static str>,
    error_status: Option<u16>,
}

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

fn device(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    authorize_enterprise_upload(headers, &state.token_sha256, &state.enterprise_config)?.ok_or_else(
        || {
            error(
                StatusCode::FORBIDDEN,
                "COLLECTOR_IDENTITY_REQUIRED",
                "此接口仅供已绑定采集端确认上传结果。",
            )
        },
    )
}

fn uuid(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(ApiError::invalid_query)
}

fn identity(headers: &HeaderMap) -> Result<UploadIdentity, ApiError> {
    let export_id = headers
        .get("x-collector-export-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(ApiError::invalid_query)?;
    let content_sha256 = headers
        .get("x-collector-content-sha256")
        .and_then(|v| v.to_str().ok())
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
        .ok_or_else(ApiError::invalid_query)?;
    Ok(UploadIdentity {
        export_id: uuid(export_id)?,
        content_sha256: content_sha256.into(),
    })
}

impl UploadRecord {
    fn status(&self) -> Result<UploadStatus, ApiError> {
        let progress = self.task.snapshot()?;
        Ok(UploadStatus {
            schema: "collector-upload.v1",
            upload_id: progress.task_id,
            export_id: self.identity.export_id.to_string(),
            content_sha256: self.identity.content_sha256.clone(),
            status: progress.status,
            stage: progress.stage,
            received_bytes: progress.received_bytes,
            result: progress.result,
            error: progress.error,
            error_code: progress.error_code,
            error_status: progress.error_status,
        })
    }
}

pub(crate) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<UploadStatus>, ApiError> {
    let owner = device(&state, &headers)?;
    let id = uuid(&id)?.to_string();
    let record = {
        let mut registry = state
            .collector_uploads
            .lock()
            .map_err(|_| ApiError::store())?;
        let key = (owner, id);
        if registry
            .0
            .get(&key)
            .is_some_and(|record| record.task.expired())
        {
            registry.0.remove(&key);
        }
        registry.0.get(&key).cloned().ok_or_else(|| {
            error(
                StatusCode::NOT_FOUND,
                "COLLECTOR_UPLOAD_NOT_FOUND",
                "未找到此设备的上传记录。",
            )
        })?
    };
    Ok(Json(record.status()?))
}

pub(crate) async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Body,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let owner = device(&state, &headers)?;
    let id = uuid(&id)?.to_string();
    let identity = identity(&headers)?;
    let record = UploadRecord {
        identity: identity.clone(),
        task: Task::create(id.clone(), "collector_upload"),
    };
    {
        let mut registry = state
            .collector_uploads
            .lock()
            .map_err(|_| ApiError::store())?;
        registry.0.retain(|_, record| !record.task.expired());
        if registry.0.contains_key(&(owner.clone(), id.clone())) {
            return Err(error(
                StatusCode::CONFLICT,
                "COLLECTOR_UPLOAD_EXISTS",
                "此上传已登记，请查询原上传结果。",
            ));
        }
        let records = registry
            .0
            .iter()
            .filter(|((device, _), _)| device == &owner)
            .collect::<Vec<_>>();
        let active = records
            .iter()
            .filter(|(_, record)| {
                record.task.snapshot().map_or(true, |p| {
                    matches!(p.status, "queued" | "receiving" | "running")
                })
            })
            .count();
        if registry.0.len() >= MAX_RECORDS
            || records.len() >= MAX_DEVICE_RECORDS
            || active >= MAX_DEVICE_ACTIVE
        {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "COLLECTOR_UPLOAD_BUSY",
                "此设备的上传记录或并发任务已达上限，请稍后查询原任务再重试。",
            ));
        }
        registry
            .0
            .insert((owner.clone(), id.clone()), record.clone());
    }
    let (accepted, received) = tokio::sync::oneshot::channel();
    let task = Arc::clone(&record.task);
    // Do not acknowledge until the file is received: an early response could
    // make the HTTP client stop sending the rest of the encrypted package.
    tokio::spawn(async move {
        let work = crate::service::receive_collector_upload(
            state,
            body,
            owner,
            Arc::clone(&task),
            identity,
            accepted,
        );
        let _ = run_detached(Some(task), work).await;
    });
    received.await.map_err(|_| ApiError::store())??;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"schema":"collector-upload.v1","uploadId":id,"status":"accepted"})),
    ))
}
