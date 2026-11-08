use crate::collection::{
    CollectionAuthorization, LOCAL_NOTICE_VERSION, MediaCollectionOptions,
    collect_candidate_with_options,
};
use crate::collector_config::{
    CollectionSchedule, EnterpriseCollectorConfig, ensure_collector_config_fresh,
    load_enterprise_collector_config,
};
use crate::collector_control::sync_collector_config_with_status;
use crate::collector_schedule::set_next_run_from_completion;
use crate::collector_transfer::{
    create_enterprise_package, has_upload_changes, post_enterprise_package, record_upload_success,
};
use crate::diagnostics;
use crate::paths::{
    OFFLINE_EXPORT_FILE_LOCK, collector_data_root, collector_work_root, discover_local_sources,
    next_offline_export_file,
};
use crate::source_keys::resolve_collection_key;
use crate::state::{
    AppState, CollectionRunGuard, CollectionSummary, CollectorScheduleStatus, CommandError,
    begin_collector_attempt, internal_error,
};
use archive_domain::{SourceCandidate, SourceCapability};
use chrono::Utc;
use serde::Serialize;
use std::fs;
use std::io::Write;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tauri::State;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BootstrapResponse {
    pub(super) organization_name: Option<String>,
    pub(super) display_name: Option<String>,
    pub(super) offline_export_enabled: bool,
    pub(super) hidden_mode_enabled: bool,
    pub(super) collector_schedule: CollectionSchedule,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SafeSourceCandidate {
    pub(super) source_id: String,
    pub(super) display_path: String,
    pub(super) client_version: Option<String>,
    pub(super) capability: SourceCapability,
    pub(super) databases: Vec<SafeSourceDatabase>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SafeSourceDatabase {
    pub(super) kind: String,
    pub(super) encrypted: bool,
    pub(super) page_size_hint: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadResult {
    pub(super) message_count: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OfflineExportResult {
    pub(super) file_name: String,
    pub(super) directory: String,
    pub(super) message_count: u64,
}

#[tauri::command]
pub(super) async fn bootstrap() -> Result<BootstrapResponse, CommandError> {
    diagnostics::write("bootstrap_started", "");
    // The heartbeat can wait for the server for up to 30 seconds. Running it
    // on the window thread prevents WebView2 from painting the initial page.
    let config = tauri::async_runtime::spawn_blocking(|| {
        let _ = sync_collector_config_with_status(None, None);
        load_enterprise_collector_config()
    })
    .await
    .map_err(|_| internal_error())??;
    diagnostics::write(
        "bootstrap_ready",
        &format!(
            "hidden_mode={};schedule={:?}",
            config.hidden_mode_enabled, config.collector_schedule.mode
        ),
    );
    Ok(BootstrapResponse {
        organization_name: Some(config.organization_name),
        display_name: (!config.display_name.is_empty()).then_some(config.display_name),
        offline_export_enabled: config.offline_export_enabled,
        hidden_mode_enabled: config.hidden_mode_enabled,
        collector_schedule: config.collector_schedule,
    })
}

#[tauri::command]
pub(super) fn get_collector_schedule_status(
    state: State<'_, AppState>,
) -> Result<CollectorScheduleStatus, CommandError> {
    state
        .schedule_status
        .lock()
        .map(|status| status.clone())
        .map_err(|_| internal_error())
}

#[tauri::command]
pub(super) fn hide_collector_window(window: tauri::WebviewWindow) -> Result<(), CommandError> {
    // Only an explicitly hidden collector may hide itself through the WebView.
    let hidden_mode_enabled = load_enterprise_collector_config()?.hidden_mode_enabled;
    diagnostics::write(
        "hide_command",
        &format!("hidden_mode={hidden_mode_enabled}"),
    );
    if !hidden_mode_enabled {
        diagnostics::write("hide_command_ignored", "normal_mode");
        return Ok(());
    }
    window.set_skip_taskbar(true).map_err(|_| CommandError {
        code: "WINDOW_HIDE_FAILED",
        message: "无法隐藏采集端窗口。".into(),
        recoverable: true,
    })?;
    window.hide().map_err(|_| CommandError {
        code: "WINDOW_HIDE_FAILED",
        message: "无法隐藏采集端窗口。".into(),
        recoverable: true,
    })
}

#[tauri::command]
pub(super) async fn discover_sources(
    state: State<'_, AppState>,
) -> Result<Vec<SafeSourceCandidate>, CommandError> {
    let candidates = tauri::async_runtime::spawn_blocking(discover_local_sources)
        .await
        .map_err(|_| internal_error())?;
    store_candidates(&state, candidates)
}

#[tauri::command]
pub(super) async fn collect_source(
    state: State<'_, AppState>,
    source_id: String,
) -> Result<CollectionSummary, CommandError> {
    if state
        .collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(CommandError {
            code: "COLLECTION_RUNNING",
            message: "已有采集任务正在运行，请稍后重试。".into(),
            recoverable: true,
        });
    }
    let run_guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(&state);
    let authorization = CollectionAuthorization {
        notice_version: LOCAL_NOTICE_VERSION.into(),
        notice_displayed_at: Utc::now(),
    };
    // 密钥探针、快照复制和数据库读取都可能耗时较长，必须全部放到阻塞线程，
    // 否则 Tauri 主线程会被拖住，窗口表现为“未响应”或只显示空白页。
    let collection_state = state.inner().clone();
    crate::state::run_owned_collector_attempt(
        collection_state,
        run_guard,
        move |collection_state| {
            let candidate = collection_state
                .discovered_sources
                .lock()
                .map_err(|_| internal_error())?
                .get(&source_id)
                .cloned()
                .ok_or(CommandError {
                    code: "SOURCE_HANDLE_EXPIRED",
                    message: "数据源授权已失效，请重新发现。".into(),
                    recoverable: true,
                })?;
            let config = load_enterprise_collector_config()?;
            ensure_collector_config_fresh(&config)?;
            let media_options = MediaCollectionOptions {
                include_files: config.include_files,
                include_images: config.include_images,
            };
            let encrypted = candidate
                .databases
                .iter()
                .any(|database| database.encrypted);
            let key = resolve_collection_key(&candidate, encrypted, &collector_data_root()?)?;
            let root = collector_work_root("manual")?;
            let result = collect_candidate_with_options(
                candidate,
                root.join("work"),
                key.as_ref(),
                authorization,
                media_options,
                None,
                None,
            );
            let _ = fs::remove_dir_all(root);
            let export = result?;
            let summary = CollectionSummary::from(&export);
            *collection_state
                .latest_export
                .lock()
                .map_err(|_| internal_error())? = Some(Arc::new(export));
            Ok(summary)
        },
    )
    .await
}

#[tauri::command]
pub(super) async fn upload_latest_enterprise(
    state: State<'_, AppState>,
) -> Result<UploadResult, CommandError> {
    let state = state.inner().clone();
    if state
        .collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(CommandError {
            code: "COLLECTION_RUNNING",
            message: "已有采集任务正在运行，请稍后重试。".into(),
            recoverable: true,
        });
    }
    let run_guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(&state);
    crate::state::run_owned_collector_attempt(state, run_guard, move |upload_state| {
        let config = load_enterprise_collector_config()?;
        ensure_collector_config_fresh(&config)?;
        upload_latest_for_config(upload_state, &config)
    })
    .await
}

pub(super) fn upload_latest_for_config(
    upload_state: &AppState,
    config: &EnterpriseCollectorConfig,
) -> Result<UploadResult, CommandError> {
    if let Some(confirmed) = crate::collector_upload::prepare_upload(
        upload_state,
        &config.upload_url,
        config.upload_token.as_bytes(),
    )? {
        // Confirmation may belong to a scheduled upload whose snapshot was
        // never published as latest_export. Do not read/re-upload an old one.
        if let Ok(mut status) = upload_state.schedule_status.lock() {
            status.last_success_at = Some(confirmed.completed_at);
            status.last_message_count = Some(confirmed.message_count);
            status.last_media_count = Some(confirmed.media_count);
        }
        set_next_run_from_completion(upload_state, &config.collector_schedule);
        return Ok(UploadResult {
            message_count: confirmed.message_count,
        });
    }
    let export = upload_state
        .latest_export
        .lock()
        .map_err(|_| internal_error())?
        .clone()
        .ok_or(CommandError {
            code: "COLLECTION_RESULT_MISSING",
            message: "没有可上传的采集结果，请先完成采集。".into(),
            recoverable: true,
        })?;
    if !has_upload_changes(upload_state, &export) {
        return Ok(UploadResult { message_count: 0 });
    }
    let package = create_enterprise_package(&export, config)?;
    let completed_at = post_enterprise_package(
        upload_state,
        &config.upload_url,
        config.upload_token.as_bytes(),
        &package,
    )?;
    record_upload_success(upload_state, &export);
    if let Ok(mut status) = upload_state.schedule_status.lock() {
        status.last_success_at = Some(completed_at);
        status.last_message_count = Some(export.message_count);
        status.last_media_count = Some(export.media_count);
    }
    set_next_run_from_completion(upload_state, &config.collector_schedule);
    Ok(UploadResult {
        message_count: export.message_count,
    })
}

#[tauri::command]
pub(super) fn export_latest_enterprise(
    state: State<'_, AppState>,
) -> Result<OfflineExportResult, CommandError> {
    let config = load_enterprise_collector_config()?;
    if !config.offline_export_enabled {
        return Err(CommandError {
            code: "OFFLINE_EXPORT_DISABLED",
            message: "管理员未启用采集端离线导出。".into(),
            recoverable: true,
        });
    }
    let export = state
        .latest_export
        .lock()
        .map_err(|_| internal_error())?
        .clone()
        .ok_or(CommandError {
            code: "COLLECTION_RESULT_MISSING",
            message: "没有可导出的采集结果，请先完成采集。".into(),
            recoverable: true,
        })?;
    let package = create_enterprise_package(&export, &config)?;
    let directory = collector_data_root()?;
    fs::create_dir_all(&directory).map_err(|_| CommandError {
        code: "OFFLINE_EXPORT_FAILED",
        message: "无法创建 collectorData 目录，请确认采集端所在目录可写。".into(),
        recoverable: true,
    })?;
    let _export_file_lock = OFFLINE_EXPORT_FILE_LOCK
        .lock()
        .map_err(|_| internal_error())?;
    let (file_name, target) = next_offline_export_file(&directory);
    let partial = directory.join(format!(".{file_name}.partial"));
    let result = persist_offline_package(&package, &partial, &target);
    result.map_err(|_| CommandError {
        code: "OFFLINE_EXPORT_FAILED",
        message: "加密文件导出失败，请确认采集端所在目录可写。".into(),
        recoverable: true,
    })?;
    Ok(OfflineExportResult {
        file_name,
        directory: directory.display().to_string(),
        message_count: export.message_count,
    })
}

pub(super) fn persist_offline_package(
    package: &[u8],
    partial: &Path,
    target: &Path,
) -> Result<(), std::io::Error> {
    // Only a successful exclusive create gives this export ownership of partial.
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(partial)?;
    let result = (|| -> Result<(), std::io::Error> {
        output.write_all(package)?;
        output.sync_all()?;
        drop(output);
        std::fs::hard_link(partial, target)
    })();
    let _ = fs::remove_file(partial);
    result
}

#[tauri::command]
#[cfg(windows)]
pub(super) fn open_offline_export_directory() -> Result<(), CommandError> {
    let directory = collector_data_root()?;
    fs::create_dir_all(&directory).map_err(|_| CommandError {
        code: "OFFLINE_EXPORT_DIRECTORY_OPEN_FAILED",
        message: "无法创建 collectorData 目录。".into(),
        recoverable: true,
    })?;
    Command::new("explorer.exe")
        .arg(directory)
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|_| CommandError {
            code: "OFFLINE_EXPORT_DIRECTORY_OPEN_FAILED",
            message: "无法打开加密文件所在目录。".into(),
            recoverable: true,
        })?;
    Ok(())
}

#[tauri::command]
#[cfg(not(windows))]
pub(super) fn open_offline_export_directory() -> Result<(), CommandError> {
    Err(CommandError {
        code: "PLATFORM_UNSUPPORTED",
        message: "当前平台暂不支持打开导出目录。".into(),
        recoverable: false,
    })
}

#[tauri::command]
pub(super) async fn sync_collector_config(state: State<'_, AppState>) -> Result<(), CommandError> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        sync_collector_config_with_status(Some(&state.schedule_status), Some(&state))
    })
    .await
    .map_err(|_| internal_error())?
}

pub(super) fn store_candidates(
    state: &State<'_, AppState>,
    candidates: Vec<SourceCandidate>,
) -> Result<Vec<SafeSourceCandidate>, CommandError> {
    let safe = candidates.iter().map(SafeSourceCandidate::from).collect();
    let mut stored = state
        .discovered_sources
        .lock()
        .map_err(|_| internal_error())?;
    for candidate in candidates {
        stored.insert(candidate.source_id.clone(), candidate);
    }
    Ok(safe)
}

impl From<&SourceCandidate> for SafeSourceCandidate {
    fn from(candidate: &SourceCandidate) -> Self {
        Self {
            source_id: candidate.source_id.clone(),
            display_path: candidate.display_path.clone(),
            client_version: candidate.client_version.clone(),
            capability: candidate.capability.clone(),
            databases: candidate
                .databases
                .iter()
                .map(|database| SafeSourceDatabase {
                    kind: database.kind.clone(),
                    encrypted: database.encrypted,
                    page_size_hint: database.page_size_hint,
                })
                .collect(),
        }
    }
}
