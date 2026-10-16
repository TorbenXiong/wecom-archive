use crate::{
    ApiError, AppState, ArchiveExportOptions, ArchiveExportSource, EXPORT_FILE_LOCK, ExportRequest,
    ExportResponse, ExportStartResponse, ExportTask, ExportTaskProgress, LocalCollectionMediaSink,
    LocalCollectionProgressReporter, LocalExportRequest, OpenDirectoryResponse, authorize,
    local_collection_error, parse_export_date, parse_message_type, persist_local_media_file,
    require_super_admin, sha256_file,
};
use archive_export::ExportFormat;
use archive_export::ExportMedia;
use archive_export::ExportPackage;
use archive_export::ExportParticipant;
use archive_export::ExportResult;
use archive_export::ExportScope;
use archive_export::export_package;
use archive_store::ArchiveStore;
use archive_store::MessageQuery;
use archive_transfer::sort_export_for_output;
use archive_transfer::write_importable_json_formatted;
use axum::Json;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::Local;
use chrono::TimeDelta;
use chrono::Utc;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use uuid::Uuid;

pub(crate) async fn create_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExportRequest>,
) -> Result<Json<ExportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let task = Arc::new(ExportTask::new());
    execute_archive_export(state, request, task, Uuid::now_v7().to_string())
        .await
        .map(Json)
}

async fn execute_archive_export(
    state: AppState,
    request: ExportRequest,
    task: Arc<ExportTask>,
    task_id: String,
) -> Result<ExportResponse, ApiError> {
    let message_type = request
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let conversation_ids = match request.scope {
        ExportScope::CurrentConversation | ExportScope::CurrentFilter => vec![
            request
                .conversation_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(ApiError::invalid_query)?,
        ],
        ExportScope::SelectedConversations => {
            let ids = request
                .conversation_ids
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Err(ApiError::invalid_query());
            }
            ids
        }
        ExportScope::DateRange | ExportScope::EntireArchive => Vec::new(),
    };
    let starts_at = parse_export_date(request.start_date.as_deref(), false)?;
    let ends_at = parse_export_date(request.end_date.as_deref(), true)?;
    if request.scope == ExportScope::DateRange && starts_at.is_none() && ends_at.is_none() {
        return Err(ApiError::invalid_query());
    }
    if let (Some(start), Some(end)) = (starts_at, ends_at)
        && start >= end
    {
        return Err(ApiError::invalid_query());
    }
    let base_query = MessageQuery {
        conversation_id: None,
        participant_id: if request.scope == ExportScope::CurrentFilter {
            request.participant_id
        } else {
            None
        },
        text: if request.scope == ExportScope::CurrentFilter {
            request.text
        } else {
            None
        },
        starts_at,
        ends_at: ends_at.map(|value| value - TimeDelta::nanoseconds(1)),
        message_type: if request.scope == ExportScope::CurrentFilter {
            message_type
        } else {
            None
        },
        media_only: request.scope == ExportScope::CurrentFilter
            && request.media_only.unwrap_or(false),
        limit: 50_000,
        offset: 0,
        sort_ascending: request.message_order.is_ascending(),
    };
    let archive_path = Arc::clone(&state.archive_path);
    let data_root = Arc::clone(&state.data_root);
    let export_directory = data_root.join("exports").display().to_string();
    let format = request.format;
    let scope = request.scope;
    let task_for_run = Arc::clone(&task);
    let result = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(ApiError::from_store)?;
        let _export_file_lock = loop {
            if task_for_run
                .cancelled
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(ApiError::from_export(
                    archive_export::ExportError::Cancelled,
                ));
            }
            match EXPORT_FILE_LOCK.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => return Err(ApiError::store()),
            }
        };
        task_for_run.update(5, "正在整理所选记录");
        let source = ArchiveExportSource::new_cancellable(
            &store,
            ArchiveExportOptions {
                query: base_query,
                conversation_ids,
                simplify: request.simplify,
                pretty: request.pretty,
                data_redaction: request.data_redaction,
                conversation_order_ascending: request.conversation_order.is_ascending(),
            },
            &task_for_run.cancelled,
        )
        .map_err(ApiError::from_store)?;
        task_for_run.update(25, "准备导出内容");
        let export_root = data_root.join("exports");
        std::fs::create_dir_all(&export_root).map_err(|_| ApiError::store())?;
        let extension = match format {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Html => "html",
            ExportFormat::Md => "md",
        };
        let package = ExportPackage {
            archive_id: "default".into(),
            scope,
            format,
            messages: Vec::new(),
            participants: source.participants().to_vec(),
            conversation_names: source.conversation_names().clone(),
            simplify: request.simplify,
            pretty: request.pretty,
            media: Vec::new(),
            generated_at: Utc::now(),
            conversation_order_ascending: request.conversation_order.is_ascending(),
            message_order_ascending: request.message_order.is_ascending(),
        };
        let (file_name, target) = next_export_file(&export_root, extension);
        task_for_run.update(35, "写入导出文件");
        let result = archive_export::export_streamed_with_progress(
            &package,
            &source,
            &target,
            &task_for_run.cancelled,
            &mut |done, total, stage| {
                let percent = if stage == "writing" {
                    35 + (done.saturating_mul(60) / total.max(1)).min(60) as u8
                } else {
                    97
                };
                task_for_run.update(
                    percent,
                    if stage == "writing" {
                        "正在写入导出文件"
                    } else {
                        "正在校验并完成文件"
                    },
                );
                if let Ok(mut progress) = task_for_run.progress.lock() {
                    progress.processed_messages = done;
                    progress.total_messages = Some(total);
                }
            },
        )
        .map_err(ApiError::from_export)?;
        Ok::<_, ApiError>((file_name, result))
    })
    .await
    .map_err(|_| ApiError::store())??;
    let response = ExportResponse {
        task_id: Some(task_id),
        status: Some("completed".into()),
        export_id: result.1.export_id.to_string(),
        file_name: result.0,
        directory: export_directory,
        message_count: result.1.message_count,
        media_count: result.1.media_count,
        missing_media_count: result.1.missing_media_count,
        manifest_sha256: result.1.manifest_sha256,
    };
    if let Ok(mut progress) = task.progress.lock() {
        progress.status = "completed".into();
        progress.stage = "已完成".into();
        progress.percent = 100;
        progress.result = Some(response.clone());
    }
    Ok(response)
}

pub(crate) async fn start_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExportRequest>,
) -> Result<Json<ExportStartResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let task_id = Uuid::now_v7().to_string();
    let task = Arc::new(ExportTask::new());
    {
        let mut tasks = state.export_tasks.lock().map_err(|_| ApiError::store())?;
        if tasks.len() >= 64 {
            tasks.retain(|_, task| {
                task.progress
                    .lock()
                    .is_ok_and(|p| matches!(p.status.as_str(), "queued" | "running" | "cancelling"))
            });
        }
        if tasks.len() >= 64 {
            return Err(ApiError::invalid_query());
        }
        tasks.insert(task_id.clone(), Arc::clone(&task));
    }
    let run_id = task_id.clone();
    tokio::spawn(async move {
        let result = execute_archive_export(state, request, Arc::clone(&task), run_id).await;
        if let Err(error) = result
            && let Ok(mut progress) = task.progress.lock()
        {
            if task.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                progress.status = "cancelled".into();
                progress.stage = "已取消，未完成文件已清理".into();
            } else {
                progress.status = "failed".into();
                progress.stage = "导出失败".into();
                progress.error = Some(error.body.message);
                progress.error_code = Some(error.body.code.into());
            }
        }
    });
    Ok(Json(ExportStartResponse {
        task_id,
        status: "queued".into(),
    }))
}

pub(crate) async fn export_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(export_id): AxumPath<String>,
) -> Result<Json<ExportTaskProgress>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let task = state
        .export_tasks
        .lock()
        .map_err(|_| ApiError::store())?
        .get(&export_id)
        .cloned()
        .ok_or_else(ApiError::invalid_query)?;
    let progress = task.progress.lock().map_err(|_| ApiError::store())?.clone();
    Ok(Json(progress))
}

pub(crate) async fn cancel_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(export_id): AxumPath<String>,
) -> Result<Json<ExportTaskProgress>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let task = state
        .export_tasks
        .lock()
        .map_err(|_| ApiError::store())?
        .get(&export_id)
        .cloned()
        .ok_or_else(ApiError::invalid_query)?;
    task.cancel();
    let progress = task.progress.lock().map_err(|_| ApiError::store())?.clone();
    Ok(Json(progress))
}

/// Export the current local WeCom snapshot directly. This keeps the fast
/// standalone export flow separate from archive ingestion: it avoids writing
/// the snapshot to SQLite and then reading the same messages back for export.
pub(crate) async fn create_local_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LocalExportRequest>,
) -> Result<Json<ExportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let collector = state
        .local_collector
        .clone()
        .ok_or_else(ApiError::local_collection_unavailable)?;
    let include_media = {
        let config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        config.include_files || config.include_images
    };
    let data_root = Arc::clone(&state.data_root);
    let export_directory = data_root.join("exports").display().to_string();
    let format = request.format;
    let data_redaction = request.data_redaction;
    let source_root = request.source_root;
    let conversation_order_ascending = request.conversation_order.is_ascending();
    let message_order_ascending = request.message_order.is_ascending();
    let result = tokio::task::spawn_blocking(move || {
        let reporter: LocalCollectionProgressReporter = Arc::new(|_, _, _| {});
        let media_root = Arc::clone(&data_root);
        let media_sink: LocalCollectionMediaSink =
            Arc::new(move |source, expected_hash, expected_size| {
                persist_local_media_file(&media_root, source, expected_hash, expected_size)
                    .map_err(|_| "媒体文件写入归档失败。".to_owned())
            });
        let mut export = collector(
            include_media,
            data_redaction,
            None,
            source_root,
            reporter,
            media_sink,
        )
        .map_err(local_collection_error)?;
        let export_id = export.export_id;
        let export_root = data_root.join("exports");
        std::fs::create_dir_all(&export_root).map_err(|_| ApiError::store())?;
        let extension = match format {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Html => "html",
            ExportFormat::Md => "md",
        };
        let _export_file_lock = EXPORT_FILE_LOCK.lock().map_err(|_| ApiError::store())?;
        let (file_name, target) = next_export_file(&export_root, extension);
        if format == ExportFormat::Json && !request.simplify {
            sort_export_for_output(
                &mut export,
                conversation_order_ascending,
                message_order_ascending,
            )
            .map_err(|_| ApiError::invalid_export())?;
            write_importable_json_formatted(&export, &target, request.pretty)
                .map_err(|_| ApiError::invalid_export())?;
            let result = ExportResult {
                export_id,
                target: target.clone(),
                message_count: export.message_count,
                media_count: export.media_count,
                missing_media_count: export.missing_media_count,
                manifest_sha256: sha256_file(&target).map_err(|_| ApiError::store())?,
            };
            return Ok::<_, ApiError>((file_name, result));
        }
        let messages = export
            .batches
            .iter()
            .flat_map(|batch| batch.messages.iter())
            .cloned()
            .collect::<Vec<_>>();
        let conversation_names = export
            .conversations
            .into_iter()
            .filter_map(|conversation| {
                conversation
                    .display_name
                    .map(|name| (conversation.conversation_id, name))
            })
            .collect();
        let participants = export
            .participants
            .into_iter()
            .map(|participant| ExportParticipant {
                participant_id: participant.participant_id,
                display_name: participant.display_name,
                participant_kind: participant.participant_kind,
            })
            .collect::<Vec<_>>();
        let media = messages
            .iter()
            .flat_map(|message| &message.media)
            .map(|item| ExportMedia {
                content_sha256: item.content_hash.clone(),
                original_name: item.original_name.clone(),
                mime_type: item.mime_type.clone(),
                size_bytes: item.size_bytes,
                source_path: None,
                integrity: item.integrity.clone(),
                missing_reason: item.missing_reason.clone(),
            })
            .collect::<Vec<_>>();
        let package = ExportPackage {
            archive_id: "local".into(),
            scope: ExportScope::EntireArchive,
            format,
            messages,
            participants,
            conversation_names,
            simplify: request.simplify,
            pretty: request.pretty,
            media,
            generated_at: Utc::now(),
            conversation_order_ascending,
            message_order_ascending,
        };
        let result = export_package(&package, &target, &AtomicBool::new(false))
            .map_err(ApiError::from_export)?;
        Ok::<_, ApiError>((file_name, result))
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(ExportResponse {
        task_id: None,
        status: None,
        export_id: result.1.export_id.to_string(),
        file_name: result.0,
        directory: export_directory,
        message_count: result.1.message_count,
        media_count: result.1.media_count,
        missing_media_count: result.1.missing_media_count,
        manifest_sha256: result.1.manifest_sha256,
    }))
}

pub(crate) async fn open_export_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OpenDirectoryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let directory = state.data_root.join("exports");
    std::fs::create_dir_all(&directory).map_err(|_| ApiError::store())?;
    #[cfg(windows)]
    {
        source_windows::desktop::open_directory(&directory).map_err(|_| ApiError::store())?;
        Ok(Json(OpenDirectoryResponse { opened: true }))
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(ApiError::invalid_query())
    }
}

pub(crate) fn next_export_file(export_root: &Path, extension: &str) -> (String, PathBuf) {
    loop {
        let file_name = format!(
            "wecom-{}.{}",
            Local::now().format("%Y%m%d-%H%M%S"),
            extension
        );
        let target = export_root.join(&file_name);
        if !target.exists() {
            return (file_name, target);
        }
        // Keep the timestamp truthful while avoiding a same-second overwrite.
        std::thread::sleep(Duration::from_millis(100));
    }
}
