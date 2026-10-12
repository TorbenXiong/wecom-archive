use crate::{
    ApiError, AppState, ImportResponse, MAX_CLIENT_EXPORT_BYTES, authorize,
    authorize_enterprise_upload, decode_hex, encode_hex, is_valid_content_hash,
    persist_enterprise_config,
};
use archive_store::ArchiveStore;
use archive_store::IngestSummary;
use archive_transfer::read_json;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::Utc;
use futures_util::StreamExt;
use sha2::Digest;
use sha2::Sha256;
use source_windows::enterprise_crypto;
use source_windows::secrets as dpapi_protect;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) async fn import_enterprise(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ImportResponse>, ApiError> {
    let collector_id =
        authorize_enterprise_upload(&headers, &state.token_sha256, &state.enterprise_config)?;
    let import_path = receive_package(&state.data_root, body).await?;
    let bytes = tokio::fs::read(&import_path)
        .await
        .map_err(|_| ApiError::invalid_export())?;
    let _ = tokio::fs::remove_file(&import_path).await;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    let package = archive_transfer::parse_enterprise_package_for_organization(
        &bytes,
        &config.organization_id,
    )
    .map_err(|_| ApiError::invalid_export())?;
    let protected_private_key_hex = config
        .collectors
        .iter()
        .find(|record| {
            record.key_id == package.key_id && !record.private_key_protected_hex.is_empty()
        })
        .map(|record| record.private_key_protected_hex.clone())
        .or_else(|| {
            if package.key_id == config.key_id {
                Some(config.private_key_protected_hex.clone())
            } else {
                config
                    .key_history
                    .iter()
                    .find(|record| record.key_id == package.key_id)
                    .map(|record| record.private_key_protected_hex.clone())
            }
        })
        .ok_or_else(ApiError::invalid_export)?;
    let protected_private_key =
        decode_hex(&protected_private_key_hex).map_err(|_| ApiError::store())?;
    let private_key = Zeroizing::new(
        dpapi_protect::unprotect(&protected_private_key).map_err(|_| ApiError::store())?,
    );
    let wrapped_dek =
        decode_hex(&package.wrapped_dek_hex).map_err(|_| ApiError::invalid_export())?;
    let nonce = decode_hex(&package.nonce_hex).map_err(|_| ApiError::invalid_export())?;
    let ciphertext = decode_hex(&package.ciphertext_hex).map_err(|_| ApiError::invalid_export())?;
    let tag =
        decode_hex(&package.authentication_tag_hex).map_err(|_| ApiError::invalid_export())?;
    let plaintext = Zeroizing::new(
        enterprise_crypto::decrypt(&private_key, &wrapped_dek, &nonce, &ciphertext, &tag)
            .map_err(|_| ApiError::invalid_export())?,
    );
    let export = archive_transfer::validate_enterprise_plaintext(&package, &plaintext)
        .map_err(|_| ApiError::invalid_export())?;
    let result = ingest_export(&state, export).await;
    if let Ok(response) = &result
        && let Some(collector_id) = collector_id
        && let Ok(mut config) = state.enterprise_config.lock()
        && let Some(collector) = config
            .collectors
            .iter_mut()
            .find(|collector| collector.collector_id == collector_id)
    {
        collector.last_upload_at = Some(response.0.completed_at.clone());
        let _ = persist_enterprise_config(&state.data_root, &config);
    }
    result
}

pub(crate) async fn import_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ImportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let import_path = receive_json(&state.data_root, body).await?;
    let parse_path = import_path.clone();
    let parsed = tokio::task::spawn_blocking(move || read_json(&parse_path))
        .await
        .map_err(|_| ApiError::invalid_export())?;
    let _ = std::fs::remove_file(&import_path);
    let export = parsed.map_err(|_| ApiError::invalid_export())?;
    ingest_export(&state, export).await
}

pub(crate) async fn ingest_export(
    state: &AppState,
    export: archive_domain::ClientExportV1,
) -> Result<Json<ImportResponse>, ApiError> {
    let export_id = export.export_id.to_string();
    let batch_count = export.batches.len();
    let media_count = export.media_count;
    let missing_media_count = export.missing_media_count;
    let archive_path = Arc::clone(&state.archive_path);
    let data_root = Arc::clone(&state.data_root);
    let conversations = export.conversations;
    let participants = export.participants;
    let batches = export.batches;
    let media_blobs = export.media_blobs;
    let (summary, revision) = tokio::task::spawn_blocking(move || {
        persist_media_blobs(&data_root, &media_blobs)?;
        let mut store = ArchiveStore::open(&archive_path).map_err(ApiError::from_store)?;
        let mut total = IngestSummary {
            inserted: 0,
            unchanged: 0,
            revised: 0,
        };
        for batch in &batches {
            let current = store.ingest_batch(batch).map_err(ApiError::from_store)?;
            total.inserted += current.inserted;
            total.unchanged += current.unchanged;
            total.revised += current.revised;
        }
        store
            .upsert_directory(&conversations, &participants)
            .map_err(ApiError::from_store)?;
        let revision = store.summary().map_err(ApiError::from_store)?.revision;
        Ok::<_, ApiError>((total, revision))
    })
    .await
    .map_err(|_| ApiError::store())??;
    // Keep the latest revision even when the workspace page is not currently
    // listening. `send` drops the value when there are no receivers, which
    // made the first collector upload invisible until a later upload.
    state.archive_update.send_replace(revision);
    let completed_at = Utc::now().to_rfc3339();
    Ok(Json(ImportResponse {
        export_id,
        batch_count,
        inserted: summary.inserted,
        unchanged: summary.unchanged,
        revised: summary.revised,
        media_count,
        missing_media_count,
        completed_at,
    }))
}

pub(crate) fn persist_media_blobs(
    data_root: &Path,
    media_blobs: &[archive_domain::MediaBlobV1],
) -> Result<(), ApiError> {
    if media_blobs.is_empty() {
        return Ok(());
    }
    let media_root = data_root.join("media");
    std::fs::create_dir_all(&media_root).map_err(|_| ApiError::store())?;
    for blob in media_blobs {
        if !is_valid_content_hash(&blob.content_hash) {
            return Err(ApiError::invalid_export());
        }
        let bytes = decode_hex(&blob.content_hex).map_err(|_| ApiError::invalid_export())?;
        if bytes.len() as u64 != blob.size_bytes
            || encode_hex(&Sha256::digest(&bytes)) != blob.content_hash
        {
            return Err(ApiError::invalid_export());
        }
        let target = media_root.join(&blob.content_hash);
        if target.is_file() {
            continue;
        }
        let partial = media_root.join(format!(
            ".{}.{}.partial",
            blob.content_hash,
            Uuid::new_v4().simple()
        ));
        let write_result = (|| -> Result<(), std::io::Error> {
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&partial)?;
            std::io::Write::write_all(&mut output, &bytes)?;
            output.sync_all()?;
            match std::fs::hard_link(&partial, &target) {
                Ok(()) => Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && target.is_file() =>
                {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })();
        let _ = std::fs::remove_file(&partial);
        write_result.map_err(|_| ApiError::store())?;
    }
    Ok(())
}

pub(crate) fn persist_local_media_file(
    data_root: &Path,
    source: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), ApiError> {
    if !is_valid_content_hash(expected_hash) || !source.is_file() {
        return Err(ApiError::invalid_export());
    }
    let media_root = data_root.join("media");
    std::fs::create_dir_all(&media_root).map_err(|_| ApiError::store())?;
    let target = media_root.join(expected_hash);
    if target.is_file() {
        return (target.metadata().map_err(|_| ApiError::store())?.len() == expected_size)
            .then_some(())
            .ok_or_else(ApiError::invalid_export);
    }
    let partial = media_root.join(format!(
        ".{}.{}.partial",
        expected_hash,
        Uuid::new_v4().simple()
    ));
    let copy_result = (|| -> Result<(), std::io::Error> {
        let mut input = std::fs::File::open(source)?;
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = std::io::Read::read(&mut input, &mut buffer)?;
            if read == 0 {
                break;
            }
            std::io::Write::write_all(&mut output, &buffer[..read])?;
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        output.sync_all()?;
        if size != expected_size || encode_hex(&hasher.finalize()) != expected_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "media changed while copying",
            ));
        }
        match std::fs::hard_link(&partial, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && target.is_file() => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(&partial);
    copy_result.map_err(|_| ApiError::store())
}

pub(crate) async fn receive_json(root: &Path, body: Body) -> Result<PathBuf, ApiError> {
    receive_json_with_limit(root, body, MAX_CLIENT_EXPORT_BYTES).await
}

pub(crate) async fn receive_json_with_limit(
    root: &Path,
    body: Body,
    limit: u64,
) -> Result<PathBuf, ApiError> {
    let import_root = root.join("imports");
    tokio::fs::create_dir_all(&import_root)
        .await
        .map_err(|_| ApiError::store())?;
    let partial = import_root.join(format!("{}.json.partial", Uuid::new_v4()));
    let complete = partial.with_extension("json");
    let mut output = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .await
        .map_err(|_| ApiError::store())?;
    let mut received = 0_u64;
    let mut stream = body.into_data_stream();
    while let Some(next) = stream.next().await {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(ApiError::invalid_export());
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > limit {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::too_large());
        }
        if output.write_all(&chunk).await.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::store());
        }
    }
    if received == 0 || output.sync_all().await.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(ApiError::invalid_export());
    }
    drop(output);
    tokio::fs::rename(&partial, &complete)
        .await
        .map_err(|_| ApiError::store())?;
    Ok(complete)
}

pub(crate) async fn receive_package(root: &Path, body: Body) -> Result<PathBuf, ApiError> {
    let import_root = root.join("imports");
    tokio::fs::create_dir_all(&import_root)
        .await
        .map_err(|_| ApiError::store())?;
    let partial = import_root.join(format!("{}.wca.partial", Uuid::new_v4()));
    let complete = partial.with_extension("wca");
    let mut output = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .await
        .map_err(|_| ApiError::store())?;
    let mut received = 0_u64;
    let mut stream = body.into_data_stream();
    while let Some(next) = stream.next().await {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(ApiError::invalid_export());
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > MAX_CLIENT_EXPORT_BYTES {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::too_large());
        }
        if output.write_all(&chunk).await.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::store());
        }
    }
    if received == 0 || output.sync_all().await.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(ApiError::invalid_export());
    }
    drop(output);
    tokio::fs::rename(&partial, &complete)
        .await
        .map_err(|_| ApiError::store())?;
    Ok(complete)
}
