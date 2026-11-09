use crate::task_history::{HistoryRecord, Verification};
use crate::{ApiError, AppState, authorize};
use archive_store::{ArchiveHealth, ArchiveStore};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;
use std::sync::{Arc, atomic::Ordering};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthSummary {
    #[serde(flatten)]
    pub archive: ArchiveHealth,
    pub checked_at: String,
    pub latest_verification: Option<HistoryRecord>,
    pub available_bytes: Option<u64>,
    pub database_bytes: Option<u64>,
}

pub(crate) async fn summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<HealthSummary>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let latest_verification = crate::task_history::latest_verification(&state.task_history).await?;
    let available_bytes = source_windows::storage::available_bytes(&state.data_root).ok();
    let database_bytes = std::fs::metadata(&*state.archive_path)
        .ok()
        .map(|meta| meta.len());
    let archive = tokio::task::spawn_blocking(move || {
        let store =
            ArchiveStore::open_read_only(&state.archive_path).map_err(ApiError::from_store)?;
        let _snapshot = store.read_snapshot().map_err(ApiError::from_store)?;
        store.archive_health().map_err(ApiError::from_store)
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(HealthSummary {
        archive,
        checked_at: chrono::Utc::now().to_rfc3339(),
        latest_verification,
        available_bytes,
        database_bytes,
    }))
}

pub(crate) async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<HistoryRecord>), ApiError> {
    authorize(&headers, &state.token_sha256)?;
    if state
        .health_check_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            body: crate::ErrorResponse {
                code: "HEALTH_CHECK_RUNNING",
                message: "归档校验正在执行，请在任务中心查看进度。".into(),
                recoverable: true,
            },
        });
    }
    let guard = crate::LocalCollectionRunGuard::new(Arc::clone(&state.health_check_running));
    let history = state
        .task_history
        .begin(uuid::Uuid::now_v7().to_string(), "health_check")?;
    let record = state
        .task_history
        .get(&history.id)?
        .ok_or_else(ApiError::store)?;
    tokio::spawn(async move {
        let _guard = guard;
        history.progress("running", 0);
        let result = tokio::task::spawn_blocking(move || verify(&state))
            .await
            .unwrap_or_else(|_| Err(ApiError::store()));
        history.finish_verification(&result);
    });
    Ok((StatusCode::ACCEPTED, Json(record)))
}

fn verify(state: &AppState) -> Result<Verification, ApiError> {
    let store = ArchiveStore::open_read_only(&state.archive_path).map_err(ApiError::from_store)?;
    let _snapshot = store.read_snapshot().map_err(ApiError::from_store)?;
    let revision = store.summary().map_err(ApiError::from_store)?.revision;
    let database_ok = store.verify_integrity().is_ok();
    let mut result = Verification {
        revision,
        database_ok,
        checked_media: 0,
        missing_files: 0,
        damaged_files: 0,
        unreadable_files: 0,
    };
    if !database_ok {
        return Ok(result);
    }
    let media_root = state.data_root.join("media");
    let safe_root = std::fs::symlink_metadata(&media_root)
        .ok()
        .is_none_or(|meta| !source_windows::atomic_file::is_link(&meta) && meta.is_dir());
    store
        .visit_verified_media(|hash, size| {
            result.checked_media += 1;
            if !safe_root || !crate::is_valid_content_hash(hash) {
                result.unreadable_files += 1;
                return Ok(());
            }
            let path = media_root.join(hash);
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    result.missing_files += 1
                }
                Err(_) => result.unreadable_files += 1,
                Ok(meta) if source_windows::atomic_file::is_link(&meta) || !meta.is_file() => {
                    result.unreadable_files += 1
                }
                Ok(meta) if size.is_some_and(|size| size != meta.len()) => {
                    result.damaged_files += 1
                }
                Ok(_) => match crate::sha256_file(&path) {
                    Ok(actual) if actual.eq_ignore_ascii_case(hash) => {}
                    Ok(_) => result.damaged_files += 1,
                    Err(_) => result.unreadable_files += 1,
                },
            }
            Ok(())
        })
        .map_err(ApiError::from_store)?;
    Ok(result)
}
