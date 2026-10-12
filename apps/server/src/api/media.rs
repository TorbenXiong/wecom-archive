use crate::{
    ApiError, AppState, OpenDirectoryResponse, OpenMediaQuery, authorize, is_valid_content_hash,
    require_super_admin, sanitize_file_component,
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
use std::path::Path;
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
    let source = state.data_root.join("media").join(&content_hash);
    if !source.is_file() {
        return Err(ApiError::not_found());
    }
    let extension = Path::new(&query.name)
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
    let Some(extension) = extension else {
        return Err(ApiError::invalid_query());
    };
    let opened_root = state.data_root.join("opened-media");
    std::fs::create_dir_all(&opened_root).map_err(|_| ApiError::store())?;
    let opened_name = format!(
        "{}-{}.{}",
        content_hash,
        sanitize_file_component(
            Path::new(&query.name)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("file")
        ),
        extension
    );
    let opened_path = opened_root.join(opened_name);
    if !opened_path.is_file() {
        std::fs::hard_link(&source, &opened_path).map_err(|_| ApiError::store())?;
    }
    open_file_with_default_application(&opened_path)?;
    Ok(Json(OpenDirectoryResponse { opened: true }))
}

#[cfg(windows)]
pub(crate) fn open_file_with_default_application(path: &Path) -> Result<(), ApiError> {
    source_windows::desktop::open_file(path).map_err(|_| ApiError::store())
}

#[cfg(not(windows))]
pub(crate) fn open_file_with_default_application(_: &Path) -> Result<(), ApiError> {
    Err(ApiError::invalid_query())
}
