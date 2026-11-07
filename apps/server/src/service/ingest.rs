use crate::tasks::{Task, run_detached};
use crate::{
    ApiError, AppState, ImportResponse, MAX_CLIENT_EXPORT_BYTES, authorize,
    authorize_enterprise_upload, decode_hex, encode_hex, is_valid_content_hash,
    persist_enterprise_config,
};
use archive_store::ArchiveStore;
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
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;
use zeroize::Zeroizing;

const IMPORT_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
const IMPORT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const IMPORT_RECEIVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) async fn acquire_import_slot(
    state: &AppState,
    wait: Duration,
) -> Result<OwnedSemaphorePermit, ApiError> {
    timeout(wait, Arc::clone(&state.import_slots).acquire_owned())
        .await
        .map_err(|_| ApiError::import_busy())?
        .map_err(|_| ApiError::store())
}

pub(crate) async fn import_enterprise(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ImportResponse>, ApiError> {
    let collector_id =
        authorize_enterprise_upload(&headers, &state.token_sha256, &state.enterprise_config)?;
    if collector_id.is_some() && headers.contains_key("x-archive-task-id") {
        // Only workspace credentials can query the task result. Do not let
        // collector identities consume the workspace tracking record budget.
        return Err(ApiError::unauthorized());
    }
    let task = Task::register(&state, &headers, "import")?;
    let work = import_enterprise_work(state, body, collector_id, task.clone());
    if task.is_some() {
        run_detached(task, work).await
    } else {
        work.await
    }
}

async fn import_enterprise_work(
    state: AppState,
    body: Body,
    collector_id: Option<String>,
    task: Option<Arc<Task>>,
) -> Result<Json<ImportResponse>, ApiError> {
    let permit = acquire_import_slot(&state, IMPORT_QUEUE_TIMEOUT).await?;
    if let Some(task) = &task {
        task.update("receiving", "接收导入文件", Some(0));
    }
    let file = ImportFile(if let Some(task) = &task {
        receive_file_with_progress(
            &state.data_root,
            body,
            "wca",
            MAX_CLIENT_EXPORT_BYTES,
            IMPORT_IDLE_TIMEOUT,
            IMPORT_RECEIVE_TIMEOUT,
            Some(task),
        )
        .await?
    } else {
        receive_package(&state.data_root, body).await?
    });
    let import_state = state.clone();
    tokio::task::spawn_blocking(move || {
        // A disconnected caller cannot release the slot while this worker is
        // still retaining the decrypted package or writing the archive.
        let _permit = permit;
        if let Some(task) = &task {
            task.update("running", "解密并校验导入文件", None);
        }
        let export = decode_enterprise_export(&import_state, &file.0)?;
        drop(file);
        let result = ingest_export_with_progress(&import_state, export, task.as_deref());
        if let Ok(response) = &result
            && let Some(collector_id) = collector_id
            && let Ok(mut current) = import_state.enterprise_config.lock()
        {
            let mut config = current.clone();
            if let Some(collector) = config
                .collectors
                .iter_mut()
                .find(|collector| collector.collector_id == collector_id)
            {
                collector.last_upload_at = Some(response.0.completed_at.clone());
                match persist_enterprise_config(&import_state.data_root, &config) {
                    Ok(()) => *current = config,
                    Err(_) => crate::diagnostics::write(
                        "collector_upload_state_persist_failed",
                        "code=ARCHIVE_STORE_FAILED",
                    ),
                }
            }
        }
        result
    })
    .await
    .map_err(|_| ApiError::store())?
}

struct ImportFile(PathBuf);

impl Drop for ImportFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn decode_enterprise_export(
    state: &AppState,
    path: &Path,
) -> Result<archive_domain::ClientExportV1, ApiError> {
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    let mut package = archive_transfer::read_enterprise_package(path, &config.organization_id)
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
    let ciphertext = std::mem::take(&mut package.ciphertext);
    let plaintext = Zeroizing::new(
        enterprise_crypto::decrypt(
            &private_key,
            &package.wrapped_dek,
            &package.nonce,
            &ciphertext,
            &package.authentication_tag,
        )
        .map_err(|_| ApiError::invalid_export())?,
    );
    drop(ciphertext);
    archive_transfer::validate_decoded_enterprise_plaintext(&package, &plaintext)
        .map_err(|_| ApiError::invalid_export())
}

pub(crate) async fn import_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ImportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let task = Task::register(&state, &headers, "import")?;
    let work = import_json_work(state, body, task.clone());
    if task.is_some() {
        run_detached(task, work).await
    } else {
        work.await
    }
}

async fn import_json_work(
    state: AppState,
    body: Body,
    task: Option<Arc<Task>>,
) -> Result<Json<ImportResponse>, ApiError> {
    let permit = acquire_import_slot(&state, IMPORT_QUEUE_TIMEOUT).await?;
    if let Some(task) = &task {
        task.update("receiving", "接收导入文件", Some(0));
    }
    let file = ImportFile(if let Some(task) = &task {
        receive_file_with_progress(
            &state.data_root,
            body,
            "json",
            MAX_CLIENT_EXPORT_BYTES,
            IMPORT_IDLE_TIMEOUT,
            IMPORT_RECEIVE_TIMEOUT,
            Some(task),
        )
        .await?
    } else {
        receive_json(&state.data_root, body).await?
    });
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Some(task) = &task {
            task.update("running", "校验导入文件", None);
        }
        let export = read_json(&file.0).map_err(|_| ApiError::invalid_export())?;
        drop(file);
        ingest_export_with_progress(&state, export, task.as_deref())
    })
    .await
    .map_err(|_| ApiError::store())?
}

pub(crate) async fn ingest_export(
    state: &AppState,
    export: archive_domain::ClientExportV1,
) -> Result<Json<ImportResponse>, ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || ingest_export_blocking(&state, export))
        .await
        .map_err(|_| ApiError::store())?
}

fn ingest_export_blocking(
    state: &AppState,
    export: archive_domain::ClientExportV1,
) -> Result<Json<ImportResponse>, ApiError> {
    ingest_export_with_progress(state, export, None)
}

fn ingest_export_with_progress(
    state: &AppState,
    export: archive_domain::ClientExportV1,
    task: Option<&Task>,
) -> Result<Json<ImportResponse>, ApiError> {
    let export_id = export.export_id.to_string();
    let batch_count = export.batches.len();
    let media_count = export.media_count;
    let missing_media_count = export.missing_media_count;
    let conversations = export.conversations;
    let participants = export.participants;
    let batches = export.batches;
    let media_blobs = export.media_blobs;
    let (summary, revision) = {
        if let Some(task) = task {
            task.update("running", "校验并保存附件", None);
        }
        // Release each large hexadecimal attachment as soon as it is durable.
        for blob in media_blobs {
            persist_media_blobs(&state.data_root, std::slice::from_ref(&blob))?;
        }
        if let Some(task) = task {
            task.update("running", "写入归档并提交事务", None);
        }
        let mut store = ArchiveStore::open(&state.archive_path).map_err(ApiError::from_store)?;
        let total = store
            .ingest_batches_with_directory(batches, &conversations, &participants)
            .map_err(ApiError::from_store)?;
        let revision = store.summary().map_err(ApiError::from_store)?.revision;
        (total, revision)
    };
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
        if blob.size_bytes.checked_mul(2) != Some(blob.content_hex.len() as u64) {
            return Err(ApiError::invalid_export());
        }
        let target = media_root.join(&blob.content_hash);
        let partial = media_root.join(format!(
            ".{}.{}.partial",
            blob.content_hash,
            Uuid::new_v4().simple()
        ));
        let mut owns_partial = false;
        let write_result = (|| -> Result<(), ApiError> {
            let mut output = if target.is_file() {
                None
            } else {
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&partial)
                    .map_err(|_| ApiError::store())?;
                owns_partial = true;
                Some(file)
            };
            let mut hasher = Sha256::new();
            for chunk in blob.content_hex.as_bytes().chunks(128 * 1024) {
                let text = std::str::from_utf8(chunk).map_err(|_| ApiError::invalid_export())?;
                let decoded = decode_hex(text).map_err(|_| ApiError::invalid_export())?;
                hasher.update(&decoded);
                if let Some(output) = &mut output {
                    std::io::Write::write_all(output, &decoded).map_err(|_| ApiError::store())?;
                }
            }
            if encode_hex(&hasher.finalize()) != blob.content_hash {
                return Err(ApiError::invalid_export());
            }
            let Some(output) = output else {
                return verify_persisted_media(&target, &blob.content_hash, blob.size_bytes);
            };
            output.sync_all().map_err(|_| ApiError::store())?;
            drop(output);
            match std::fs::hard_link(&partial, &target) {
                Ok(()) => Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && target.is_file() =>
                {
                    verify_persisted_media(&target, &blob.content_hash, blob.size_bytes)
                }
                Err(_) => Err(ApiError::store()),
            }
        })();
        if owns_partial {
            let _ = std::fs::remove_file(&partial);
        }
        write_result?;
    }
    Ok(())
}

fn verify_persisted_media(path: &Path, hash: &str, size: u64) -> Result<(), ApiError> {
    if std::fs::metadata(path)
        .map_err(|_| ApiError::store())?
        .len()
        != size
        || crate::sha256_file(path).map_err(|_| ApiError::store())? != hash
    {
        return Err(ApiError::store());
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
        return verify_persisted_media(&target, expected_hash, expected_size);
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
                if std::fs::metadata(&target)?.len() == expected_size
                    && crate::sha256_file(&target)? == expected_hash
                {
                    Ok(())
                } else {
                    Err(std::io::Error::other("existing media integrity failed"))
                }
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
    receive_file(
        root,
        body,
        "json",
        limit,
        IMPORT_IDLE_TIMEOUT,
        IMPORT_RECEIVE_TIMEOUT,
    )
    .await
}

pub(crate) async fn receive_package(root: &Path, body: Body) -> Result<PathBuf, ApiError> {
    receive_file(
        root,
        body,
        "wca",
        MAX_CLIENT_EXPORT_BYTES,
        IMPORT_IDLE_TIMEOUT,
        IMPORT_RECEIVE_TIMEOUT,
    )
    .await
}

pub(crate) async fn receive_file(
    root: &Path,
    body: Body,
    extension: &str,
    limit: u64,
    idle_timeout: Duration,
    total_timeout: Duration,
) -> Result<PathBuf, ApiError> {
    receive_file_with_progress(
        root,
        body,
        extension,
        limit,
        idle_timeout,
        total_timeout,
        None,
    )
    .await
}

async fn receive_file_with_progress(
    root: &Path,
    body: Body,
    extension: &str,
    limit: u64,
    idle_timeout: Duration,
    total_timeout: Duration,
    task: Option<&Task>,
) -> Result<PathBuf, ApiError> {
    let import_root = root.join("imports");
    tokio::fs::create_dir_all(&import_root)
        .await
        .map_err(|_| ApiError::store())?;
    let complete = import_root.join(format!("{}.{extension}", Uuid::new_v4()));
    let partial = complete.with_extension(format!("{extension}.partial"));
    let mut output = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .await
        .map_err(|_| ApiError::store())?;
    // Register cleanup only after exclusive creation succeeds.
    let _partial_cleanup = ImportFile(partial.clone());
    let mut received = 0_u64;
    let mut stream = body.into_data_stream();
    let total_deadline = Instant::now() + total_timeout;
    let mut idle_deadline = Instant::now() + idle_timeout;
    loop {
        let deadline = idle_deadline.min(total_deadline);
        if Instant::now() >= deadline {
            return Err(ApiError::import_receive_timeout());
        }
        let Some(next) = timeout_at(deadline, stream.next())
            .await
            .map_err(|_| ApiError::import_receive_timeout())?
        else {
            break;
        };
        let chunk = next.map_err(|_| ApiError::invalid_export())?;
        // Empty frames do not count as upload progress.
        if chunk.is_empty() {
            continue;
        }
        received = received.saturating_add(chunk.len() as u64);
        if received > limit {
            return Err(ApiError::too_large());
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|_| ApiError::store())?;
        if let Some(task) = task {
            task.update("receiving", "接收导入文件", Some(received));
        }
        idle_deadline = Instant::now() + idle_timeout;
    }
    if received == 0 || output.sync_all().await.is_err() {
        return Err(ApiError::invalid_export());
    }
    drop(output);
    tokio::fs::rename(&partial, &complete)
        .await
        .map_err(|_| ApiError::store())?;
    Ok(complete)
}
