use crate::{
    ApiError, AppState, EnterpriseCollectorRecord, EnterpriseConfig, authorize,
    collector_by_device_token, decode_hex, encode_hex, persist_enterprise_config,
    require_super_admin, sha256_file,
};
use archive_transfer::software_update::{
    MAX_EXECUTABLE_BYTES, SignedSoftwareUpdate, SoftwareUpdate, newer, signing_payload,
};
use axum::{
    Json,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, header},
    response::Response,
};
use chrono::{TimeDelta, Utc};
use source_windows::{enterprise_crypto, secrets};

pub(crate) async fn request_collector_update(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let collector = config
        .collectors
        .iter_mut()
        .find(|c| c.collector_id == collector_id && c.revoked_at.is_none())
        .ok_or_else(ApiError::not_found)?;
    if !collector.update_capable
        || !collector
            .client_version
            .as_deref()
            .is_some_and(|version| newer(version, env!("CARGO_PKG_VERSION")))
    {
        return Err(ApiError::invalid_query());
    }
    collector.software_update_request = Some(uuid::Uuid::new_v4().to_string());
    collector.software_update_version = Some(env!("CARGO_PKG_VERSION").into());
    collector.software_update_error = None;
    let response = serde_json::json!({"status":"queued","version":env!("CARGO_PKG_VERSION")});
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(response))
}

pub(crate) fn signed_software_update(
    config: &EnterpriseConfig,
    collector: &EnterpriseCollectorRecord,
) -> Result<Option<SignedSoftwareUpdate>, ApiError> {
    let (Some(request_id), Some(version)) = (
        &collector.software_update_request,
        &collector.software_update_version,
    ) else {
        return Ok(None);
    };
    if !collector.update_capable
        || version != env!("CARGO_PKG_VERSION")
        || !collector
            .client_version
            .as_deref()
            .is_some_and(|current| newer(current, version))
    {
        return Ok(None);
    }
    let executable = std::env::current_exe().map_err(|_| ApiError::store())?;
    let size = std::fs::metadata(&executable)
        .map_err(|_| ApiError::store())?
        .len();
    if size == 0 || size > MAX_EXECUTABLE_BYTES {
        return Err(ApiError::store());
    }
    let update = SoftwareUpdate {
        request_id: request_id.clone(),
        collector_id: collector.collector_id.clone(),
        version: version.clone(),
        sha256: sha256_file(&executable).map_err(|_| ApiError::store())?,
        size,
        expires_at: (Utc::now() + TimeDelta::minutes(15)).to_rfc3339(),
    };
    let private = secrets::unprotect(
        &decode_hex(&config.signing_private_key_protected_hex).map_err(|_| ApiError::store())?,
    )
    .map_err(|_| ApiError::store())?;
    let signature = enterprise_crypto::sign(
        &private,
        &signing_payload(&update).map_err(|_| ApiError::store())?,
    )
    .map_err(|_| ApiError::store())?;
    Ok(Some(SignedSoftwareUpdate {
        update,
        signature_hex: encode_hex(&signature),
    }))
}

pub(crate) async fn download_collector_update(
    State(state): State<AppState>,
    AxumPath((collector_id, request_id)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    {
        let mut config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        let collector = collector_by_device_token(&mut config, &collector_id, &headers)?;
        if !collector.update_capable
            || collector.software_update_request.as_deref() != Some(request_id.as_str())
            || collector.software_update_version.as_deref() != Some(env!("CARGO_PKG_VERSION"))
        {
            return Err(ApiError::not_found());
        }
    }
    let executable = std::env::current_exe().map_err(|_| ApiError::store())?;
    let file = tokio::fs::File::open(executable)
        .await
        .map_err(|_| ApiError::store())?;
    let size = file.metadata().await.map_err(|_| ApiError::store())?.len();
    if size == 0 || size > MAX_EXECUTABLE_BYTES {
        return Err(ApiError::store());
    }
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0; 64 * 1024];
        let size = file.read(&mut buffer).await?;
        if size == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(size);
        Ok(Some((buffer, file)))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::store())
}
