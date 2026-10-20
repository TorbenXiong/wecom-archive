use crate::{
    ApiError, AppState, OpenDirectoryResponse, OpenMediaQuery, authorize, is_valid_content_hash,
    require_super_admin, sha256_file,
};
use archive_store::ArchiveStore;
use axum::Json;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::Response;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) async fn get_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(content_hash): AxumPath<String>,
) -> Result<Response, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if !is_valid_content_hash(&content_hash) {
        return Err(ApiError::invalid_query());
    }
    let media_path = state.data_root.join("media").join(&content_hash);
    let bytes = tokio::fs::read(&media_path)
        .await
        .map_err(|_| ApiError::not_found())?;
    let archive_path = Arc::clone(&state.archive_path);
    let mime = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.media_mime(&content_hash))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??
    .unwrap_or_else(|| "application/octet-stream".into());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::store())
}

pub(crate) async fn open_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(content_hash): AxumPath<String>,
    Query(query): Query<OpenMediaQuery>,
) -> Result<Json<OpenDirectoryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if !is_valid_content_hash(&content_hash) {
        return Err(ApiError::invalid_query());
    }
    let data_root = Arc::clone(&state.data_root);
    let opened_path = tokio::task::spawn_blocking(move || {
        prepare_media_copy(&data_root, &content_hash, &query.name)
    })
    .await
    .map_err(|_| ApiError::store())??;
    open_file_with_default_application(&opened_path)?;
    Ok(Json(OpenDirectoryResponse { opened: true }))
}

pub(crate) fn prepare_media_copy(
    data_root: &Path,
    content_hash: &str,
    name: &str,
) -> Result<PathBuf, ApiError> {
    if !is_valid_content_hash(content_hash)
        || name.is_empty()
        || name.chars().count() > 240
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || name.ends_with(['.', ' '])
    {
        return Err(ApiError::invalid_query());
    }
    let source = data_root.join("media").join(content_hash);
    if !source.is_file() {
        return Err(ApiError::not_found());
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 10
                && value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .map(str::to_ascii_lowercase);
    let Some(_) = extension else {
        return Err(ApiError::invalid_query());
    };
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
    {
        return Err(ApiError::invalid_query());
    }
    let opened_root = data_root.join("opened-media");
    std::fs::create_dir_all(&opened_root).map_err(|_| ApiError::store())?;
    let run_root = opened_root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&run_root).map_err(|_| ApiError::store())?;
    let partial = run_root.join("content.partial");
    let opened_path = run_root.join(name);
    let result = (|| {
        // A hard link would let external editors modify the archived original.
        std::fs::copy(&source, &partial).map_err(|_| ApiError::store())?;
        if sha256_file(&partial).map_err(|_| ApiError::store())?
            != content_hash.to_ascii_lowercase()
        {
            return Err(ApiError::media_open_failed(
                "附件校验失败，请重新采集附件内容。",
            ));
        }
        std::fs::rename(&partial, &opened_path).map_err(|_| ApiError::store())?;
        Ok(opened_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_dir(&run_root);
    }
    result
}

#[cfg(windows)]
pub(crate) fn open_file_with_default_application(path: &Path) -> Result<(), ApiError> {
    source_windows::desktop::open_file(path).map_err(|_| {
        ApiError::media_open_failed("Windows 无法打开此附件。请先保存附件，再选择合适的应用打开。")
    })
}

#[cfg(not(windows))]
pub(crate) fn open_file_with_default_application(_: &Path) -> Result<(), ApiError> {
    Err(ApiError::invalid_query())
}
