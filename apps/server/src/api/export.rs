use crate::{
    ApiError, AppState, ArchiveExportOptions, ArchiveExportSource, EXPORT_FILE_LOCK, ExportRequest,
    ExportResponse, LocalCollectionMediaSink, LocalCollectionProgressReporter, LocalExportRequest,
    OpenDirectoryResponse, authorize, local_collection_error, parse_export_date,
    parse_message_type, persist_local_media_file, require_super_admin, sha256_file,
};
use archive_export::ExportFormat;
use archive_export::ExportMedia;
use archive_export::ExportPackage;
use archive_export::ExportParticipant;
use archive_export::ExportResult;
use archive_export::ExportScope;
use archive_export::export_package;
use archive_export::export_streamed;
use archive_store::ArchiveStore;
use archive_store::MessageQuery;
use archive_transfer::sort_export_for_output;
use archive_transfer::write_importable_json_formatted;
use axum::Json;
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

pub(crate) async fn create_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExportRequest>,
) -> Result<Json<ExportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
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
    if let (Some(start), Some(end)) = (starts_at, ends_at) {
        if start >= end {
            return Err(ApiError::invalid_query());
        }
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
    let result = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(ApiError::from_store)?;
        let _export_file_lock = EXPORT_FILE_LOCK.lock().map_err(|_| ApiError::store())?;
        let source = ArchiveExportSource::new(
            &store,
            ArchiveExportOptions {
                query: base_query,
                conversation_ids,
                simplify: request.simplify,
                pretty: request.pretty,
                data_redaction: request.data_redaction,
                conversation_order_ascending: request.conversation_order.is_ascending(),
            },
        )
        .map_err(ApiError::from_store)?;
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
        let result = export_streamed(&package, &source, &target, &AtomicBool::new(false))
            .map_err(ApiError::from_export)?;
        Ok::<_, ApiError>((file_name, result))
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(ExportResponse {
        export_id: result.1.export_id.to_string(),
        file_name: result.0,
        directory: export_directory,
        message_count: result.1.message_count,
        media_count: result.1.media_count,
        missing_media_count: result.1.missing_media_count,
        manifest_sha256: result.1.manifest_sha256,
    }))
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
