use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archive_domain::{
    ArchiveBatchV1, ClientExportV1, CollectionScope, ConversationV1, EmployeeNoticeEvidence,
    ExternalContactConsent, MediaIntegrity, MessageType, ParticipantV1, RetentionDirective,
    SourceAdapter, SourceCandidate, SourceCapability,
};
use chrono::{Local, NaiveTime, TimeDelta, TimeZone, Utc};
use message_parser::{RawMessageRow, normalize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use source_windows::WindowsSourceAdapter;
use source_windows::roots::normalize_source_root;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State};
use zeroize::{Zeroize, Zeroizing};

mod collector_http;
mod collector_update;
mod diagnostics;
mod sqlite3mc;
mod update;
pub use update::UpdateInfo;

pub fn check_for_update() -> Result<Option<update::UpdateInfo>, String> {
    update::check()
}

pub fn install_update(version: &str) -> Result<(), String> {
    update::install(version)
}

pub use update::{acknowledge_startup, run_helper as run_update_helper};

const LOCAL_NOTICE_VERSION: &str = "client-notice.v1";
const LOCAL_NOTICE_TEXT: &str = "加密传输本机企业微信聊天记录到服务端";

static OFFLINE_EXPORT_FILE_LOCK: Mutex<()> = Mutex::new(());
static SOURCE_ROOTS_FILE_LOCK: Mutex<()> = Mutex::new(());

fn next_offline_export_file(directory: &Path) -> (String, PathBuf) {
    loop {
        let file_name = format!("wecom-{}.wca", Local::now().format("%Y%m%d-%H%M%S"));
        let target = directory.join(&file_name);
        if !target.exists() {
            return (file_name, target);
        }
        // A repeated export in the same second must never replace the first file.
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn collector_data_root() -> Result<PathBuf, CommandError> {
    std::env::current_exe()
        .map_err(|_| internal_error())?
        .parent()
        .map(|parent| parent.join("collectorData"))
        .ok_or_else(internal_error)
}

fn collector_runtime_config_path(collector_id: &str) -> Result<PathBuf, CommandError> {
    let collector_id = uuid::Uuid::parse_str(collector_id).map_err(|_| internal_error())?;
    Ok(collector_data_root()?
        .join("instances")
        .join(collector_id.to_string())
        .join("collector-config.dpapi"))
}

fn legacy_collector_runtime_config_path() -> Result<PathBuf, CommandError> {
    Ok(collector_data_root()?
        .parent()
        .ok_or_else(internal_error)?
        .join("collector-config.dpapi"))
}

fn collector_work_root(prefix: &str) -> Result<PathBuf, CommandError> {
    create_work_root(&collector_data_root()?, prefix)
}

fn local_data_root(executable_directory: &Path, configured: Option<PathBuf>) -> PathBuf {
    configured.unwrap_or_else(|| executable_directory.join("serverData"))
}

fn create_work_root(data_root: &Path, prefix: &str) -> Result<PathBuf, CommandError> {
    let root = data_root
        .join("work")
        .join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).map_err(|_| internal_error())?;
    Ok(root)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    organization_name: Option<String>,
    display_name: Option<String>,
    offline_export_enabled: bool,
    hidden_mode_enabled: bool,
    collector_schedule: CollectionSchedule,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorDiagnosticEvent {
    timestamp: u64,
    event: String,
    details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseCollectorConfig {
    schema_version: String,
    organization_id: String,
    organization_name: String,
    collection_notice: String,
    key_id: String,
    public_key_hex: String,
    signing_public_key_hex: String,
    signature_hex: String,
    upload_url: String,
    upload_token: String,
    #[serde(default)]
    collector_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    config_revision: u64,
    #[serde(default)]
    config_expires_at: Option<String>,
    #[serde(default = "default_collector_enabled")]
    enabled: bool,
    #[serde(default)]
    include_files: bool,
    #[serde(default)]
    include_images: bool,
    #[serde(default = "default_data_redaction")]
    data_redaction: bool,
    #[serde(default)]
    offline_export_enabled: bool,
    #[serde(default)]
    hidden_mode_enabled: bool,
    #[serde(default = "default_collector_log_level")]
    collector_log_level: String,
    #[serde(default)]
    manual_collection_request_id: Option<String>,
    #[serde(default)]
    collector_schedule: CollectionSchedule,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CollectionScheduleMode {
    #[default]
    Disabled,
    Interval,
    Daily,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CollectionSchedule {
    #[serde(default)]
    mode: CollectionScheduleMode,
    #[serde(default = "default_schedule_interval_minutes")]
    interval_minutes: u32,
    #[serde(default = "default_schedule_daily_time")]
    daily_time: String,
}

fn default_schedule_interval_minutes() -> u32 {
    60
}

fn default_data_redaction() -> bool {
    true
}

fn default_collector_log_level() -> String {
    "off".into()
}

fn default_schedule_daily_time() -> String {
    "02:00".into()
}

fn default_collector_enabled() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandError {
    code: &'static str,
    message: String,
    recoverable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafeSourceCandidate {
    source_id: String,
    display_path: String,
    client_version: Option<String>,
    capability: SourceCapability,
    databases: Vec<SafeSourceDatabase>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafeSourceDatabase {
    kind: String,
    encrypted: bool,
    page_size_hint: Option<u32>,
}

#[derive(Debug)]
struct CollectionAuthorization {
    notice_version: String,
    notice_displayed_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default)]
struct MediaCollectionOptions {
    include_files: bool,
    include_images: bool,
}

struct MediaResolver {
    roots: Vec<PathBuf>,
    files_by_key: BTreeMap<String, Vec<PathBuf>>,
}

impl MediaResolver {
    fn new(candidate: &SourceCandidate, options: MediaCollectionOptions) -> Self {
        let mut root_candidates = candidate.media_roots.clone();
        // The client may place downloaded previews beside the recognized media
        // folders. The source root is already authorized, so include it as a
        // bounded fallback without reading outside the copied account.
        root_candidates.push(candidate.root_path.clone());
        root_candidates.sort();
        root_candidates.dedup();
        let roots = root_candidates
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect::<Vec<_>>();
        let mut files_by_key = BTreeMap::new();
        if options.include_files || options.include_images {
            let account_root = candidate.root_path.canonicalize().ok();
            let index_account_root = candidate.media_roots.is_empty()
                || account_root.as_ref().is_some_and(|root| {
                    candidate
                        .media_roots
                        .iter()
                        .filter_map(|path| path.canonicalize().ok())
                        .any(|path| &path == root)
                });
            for root in &roots {
                if !index_account_root && account_root.as_ref().is_some_and(|path| root == path) {
                    continue;
                }
                index_media_directory(root, root, 0, &mut files_by_key);
            }
        }
        Self {
            roots,
            files_by_key,
        }
    }

    fn resolve(&self, candidate_root: &Path, locator: &str) -> Option<PathBuf> {
        let locator_path = PathBuf::from(locator);
        let direct_candidates = if locator_path.is_absolute() {
            vec![locator_path]
        } else {
            std::iter::once(candidate_root.join(&locator_path))
                .chain(self.roots.iter().map(|root| root.join(&locator_path)))
                .collect()
        };
        let mut direct_match = None;
        for candidate in direct_candidates {
            if let Some(path) = self.authorized_file(candidate) {
                if direct_match
                    .as_ref()
                    .is_some_and(|existing| existing != &path)
                {
                    return None;
                }
                direct_match = Some(path);
            }
        }
        if direct_match.is_some() {
            return direct_match;
        }
        media_lookup_keys(locator).into_iter().find_map(|key| {
            self.files_by_key
                .get(&key)
                .filter(|paths| paths.len() == 1)
                .and_then(|paths| self.authorized_file(paths[0].clone()))
        })
    }

    fn authorized_file(&self, candidate: PathBuf) -> Option<PathBuf> {
        let path = candidate.canonicalize().ok()?;
        (path.is_file() && self.roots.iter().any(|root| path.starts_with(root))).then_some(path)
    }
}

fn index_media_directory(
    root: &Path,
    directory: &Path,
    depth: usize,
    index: &mut BTreeMap<String, Vec<PathBuf>>,
) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            index_media_directory(root, &path, depth + 1, index);
        } else if file_type.is_file() {
            let mut keys = media_lookup_keys(&entry.file_name().to_string_lossy());
            if let Ok(relative) = path.strip_prefix(root) {
                keys.extend(media_lookup_keys(&relative.to_string_lossy()));
            }
            for key in keys {
                let paths = index.entry(key).or_default();
                if !paths.contains(&path) {
                    paths.push(path.clone());
                }
            }
        }
    }
}

fn media_lookup_keys(value: &str) -> Vec<String> {
    let normalized = value
        .trim()
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '<' | '>'))
        .replace('\\', "/");
    let mut keys = Vec::new();
    if !normalized.is_empty() {
        keys.push(normalized.to_ascii_lowercase());
    }
    if let Some(name) = normalized
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
    {
        keys.push(name.to_ascii_lowercase());
        if let Some((stem, _)) = name.rsplit_once('.') {
            keys.push(stem.to_ascii_lowercase());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKeyKind {
    Passphrase,
    DerivedAes128,
    DerivedAes256,
    RawWxSqlite3Key,
}

struct SourceKey {
    kind: SourceKeyKind,
    bytes: Zeroizing<Vec<u8>>,
    legacy: bool,
    legacy_page_size: u32,
}

impl SourceKey {
    fn passphrase(bytes: &[u8]) -> Self {
        Self {
            kind: SourceKeyKind::Passphrase,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy: false,
            legacy_page_size: 0,
        }
    }

    fn derived_aes128_with_config(
        bytes: &[u8],
        legacy: bool,
        legacy_page_size: u32,
    ) -> Result<Self, CommandError> {
        if bytes.len() != 16 {
            return Err(CommandError {
                code: "SOURCE_KEY_FORMAT_INVALID",
                message: "AES-128 派生密钥必须恰好为 16 字节。".into(),
                recoverable: true,
            });
        }
        Ok(Self {
            kind: SourceKeyKind::DerivedAes128,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy,
            legacy_page_size,
        })
    }

    fn derived_aes256_with_config(
        bytes: &[u8],
        legacy: bool,
        legacy_page_size: u32,
    ) -> Result<Self, CommandError> {
        if bytes.len() != 32 {
            return Err(CommandError {
                code: "SOURCE_KEY_FORMAT_INVALID",
                message: "AES-256 派生密钥必须恰好为 32 字节。".into(),
                recoverable: true,
            });
        }
        Ok(Self {
            kind: SourceKeyKind::DerivedAes256,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy,
            legacy_page_size,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionSummary {
    export_id: String,
    generated_at: String,
    message_count: u64,
    media_count: u64,
    missing_media_count: u64,
    content_sha256_prefix: String,
}

impl From<&ClientExportV1> for CollectionSummary {
    fn from(export: &ClientExportV1) -> Self {
        Self {
            export_id: export.export_id.to_string(),
            generated_at: export.generated_at.to_rfc3339(),
            message_count: export.message_count,
            media_count: export.media_count,
            missing_media_count: export.missing_media_count,
            content_sha256_prefix: export.content_sha256.chars().take(12).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadResult {
    message_count: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseUploadResponse {
    completed_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfflineExportResult {
    file_name: String,
    directory: String,
    message_count: u64,
}

#[derive(Clone, Default)]
struct AppState {
    pending_update: Arc<Mutex<Option<archive_transfer::software_update::SignedSoftwareUpdate>>>,
    update_attempt: Arc<Mutex<Option<String>>>,
    software_update_error: Arc<Mutex<Option<String>>>,
    discovered_sources: Arc<Mutex<BTreeMap<String, SourceCandidate>>>,
    latest_export: Arc<Mutex<Option<ClientExportV1>>>,
    collection_running: Arc<AtomicBool>,
    scheduler_shutdown: Arc<AtomicBool>,
    schedule_status: Arc<Mutex<CollectorScheduleStatus>>,
    uploaded_messages: Arc<Mutex<BTreeMap<String, String>>>,
    tray_app: Arc<Mutex<Option<tauri::AppHandle>>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorScheduleStatus {
    running: bool,
    last_attempt_at: Option<String>,
    last_success_at: Option<String>,
    last_error: Option<String>,
    last_message_count: Option<u64>,
    last_media_count: Option<u64>,
    uploaded_count: u64,
    next_run_at: Option<String>,
    #[serde(skip)]
    manual_collection_attempted_id: Option<String>,
    #[serde(skip)]
    server_authorization_denied: bool,
    last_manual_collection_request_id: Option<String>,
}

fn begin_collector_attempt(state: &AppState) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.running = true;
        status.last_attempt_at = Some(Utc::now().to_rfc3339());
        status.last_error = None;
    }
}

fn finish_collector_attempt<T>(state: &AppState, result: &Result<T, CommandError>) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.running = false;
        if let Err(error) = result {
            status.last_error = Some(error.message.clone());
        }
    }
}

fn message_fingerprints(export: &ClientExportV1) -> BTreeMap<String, String> {
    export
        .batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .filter_map(|message| {
            let mut stable = message.clone();
            stable.collection_batch_id = uuid::Uuid::nil();
            serde_json::to_vec(&stable).ok().map(|bytes| {
                (
                    message.stable_message_id.clone(),
                    hex::encode(Sha256::digest(bytes)),
                )
            })
        })
        .collect()
}

fn has_upload_changes(state: &AppState, export: &ClientExportV1) -> bool {
    if state
        .schedule_status
        .lock()
        .map_or(true, |status| status.last_success_at.is_none())
    {
        return true;
    }
    let current = message_fingerprints(export);
    state.uploaded_messages.lock().map_or(true, |uploaded| {
        current
            .iter()
            .any(|(id, fingerprint)| uploaded.get(id) != Some(fingerprint))
    })
}

fn record_upload_success(state: &AppState, export: &ClientExportV1) {
    let current = message_fingerprints(export);
    if let (Ok(mut uploaded), Ok(mut status)) =
        (state.uploaded_messages.lock(), state.schedule_status.lock())
    {
        uploaded.extend(current);
        status.uploaded_count = uploaded.len() as u64;
    }
}

#[tauri::command]
async fn bootstrap() -> Result<BootstrapResponse, CommandError> {
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
fn get_collector_schedule_status(
    state: State<'_, AppState>,
) -> Result<CollectorScheduleStatus, CommandError> {
    state
        .schedule_status
        .lock()
        .map(|status| status.clone())
        .map_err(|_| internal_error())
}

#[tauri::command]
fn hide_collector_window(window: tauri::WebviewWindow) -> Result<(), CommandError> {
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
async fn discover_sources(
    state: State<'_, AppState>,
) -> Result<Vec<SafeSourceCandidate>, CommandError> {
    let candidates = tauri::async_runtime::spawn_blocking(discover_local_sources)
        .await
        .map_err(|_| internal_error())?;
    store_candidates(&state, candidates)
}

#[tauri::command]
async fn collect_source(
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
    let _run_guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(&state);
    let authorization = CollectionAuthorization {
        notice_version: LOCAL_NOTICE_VERSION.into(),
        notice_displayed_at: Utc::now(),
    };
    // 密钥探针、快照复制和数据库读取都可能耗时较长，必须全部放到阻塞线程，
    // 否则 Tauri 主线程会被拖住，窗口表现为“未响应”或只显示空白页。
    let collection_state = state.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
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
        result
    })
    .await
    .unwrap_or_else(|_| Err(internal_error()));
    let result = result.and_then(|export| {
        let summary = CollectionSummary::from(&export);
        *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
        Ok(summary)
    });
    finish_collector_attempt(&state, &result);
    result
}

struct CollectionRunGuard(Arc<AtomicBool>);

impl Drop for CollectionRunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Collects the currently signed-in local WeCom profile for the embedded
/// enterprise server. The snapshot and key-probe files only live under the
/// server data directory and are removed before returning.
pub fn collect_local_export(
    include_media: bool,
    data_redaction: bool,
    source_root: Option<String>,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        None,
        source_root,
        std::sync::Arc::new(|_, _, _| {}),
        None,
    )
}

pub fn collect_local_export_with_progress(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    source_root: Option<String>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: wecom_archive_server::LocalCollectionMediaSink,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        since_unix_ms,
        source_root,
        progress,
        Some(&media_sink),
    )
}

fn collect_local_export_internal(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    source_root: Option<String>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
) -> Result<ClientExportV1, String> {
    progress(6, "发现数据源", "正在查找当前登录的企业微信数据。");
    let pasted_resolved_root = source_root
        .as_deref()
        .map(|raw| {
            normalize_source_root(raw)
                .as_deref()
                .and_then(source_windows::roots::resolve_source_root)
                .ok_or_else(|| "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。".to_owned())
        })
        .transpose()?;
    let roots = if let Some(root) = pasted_resolved_root.as_ref() {
        vec![root.clone()]
    } else {
        collection_roots(
            None,
            &load_remembered_source_roots(&source_roots_config_path()),
            &source_windows::default_roots(),
        )
    };
    let candidates = source_windows::discover_roots(&roots);
    if candidates.is_empty() {
        return Err(if source_root.is_some() {
            "这个文件夹里没找到企业微信数据库，请填企业微信实际存放聊天记录的那一层目录。"
                .to_owned()
        } else {
            "未发现可支持的本机企业微信数据，请确认客户端已登录。".to_owned()
        });
    }
    progress(15, "读取授权", "已发现数据源，正在准备只读访问。");
    let executable = std::env::current_exe().map_err(|_| internal_error().message)?;
    let data_root = local_data_root(
        executable
            .parent()
            .ok_or_else(|| internal_error().message)?,
        std::env::var_os("WECOM_ARCHIVE_SERVER_DATA").map(PathBuf::from),
    );
    let temporary_root = create_work_root(&data_root, "local").map_err(|error| error.message)?;
    let result = (|| {
        let (candidate, key) = choose_collectable_source(candidates, &mut |source| {
            resolve_collection_key(
                source,
                source.databases.iter().any(|database| database.encrypted),
                &data_root,
            )
        })?;
        progress(25, "创建快照", "正在只读复制数据库及一致性文件。");
        let export = collect_candidate_with_options_since(
            candidate,
            temporary_root.join("work"),
            key.as_ref(),
            CollectionAuthorization {
                notice_version: LOCAL_NOTICE_VERSION.into(),
                notice_displayed_at: Utc::now(),
            },
            MediaCollectionOptions {
                include_files: include_media,
                include_images: include_media,
            },
            media_sink,
            Some(&progress),
            since_unix_ms,
        )?;
        progress(88, "整理采集结果", "正在校验消息及媒体内容完整性。");
        Ok::<_, CommandError>(if data_redaction {
            redact_sensitive_export(&export)
        } else {
            export
        })
    })();
    let _ = fs::remove_dir_all(&temporary_root);
    let export = result.map_err(|error| error.message)?;
    if let Some(root) = pasted_resolved_root.as_ref()
        && remember_source_root(&source_roots_config_path(), root).is_err()
    {
        diagnostics::write("source_root_persist", "status=failed");
    }
    Ok(export)
}

#[tauri::command]
async fn upload_latest_enterprise(
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
    let _run_guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(&state);
    let upload_state = state.clone();
    let result: Result<UploadResult, CommandError> =
        tauri::async_runtime::spawn_blocking(move || {
            let config = load_enterprise_collector_config()?;
            ensure_collector_config_fresh(&config)?;
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
            if !has_upload_changes(&upload_state, &export) {
                return Ok(UploadResult { message_count: 0 });
            }
            let package = create_enterprise_package(&export, &config)?;
            let completed_at = post_enterprise_package(
                &config.upload_url,
                config.upload_token.as_bytes(),
                &package,
            )?;
            record_upload_success(&upload_state, &export);
            if let Ok(mut status) = upload_state.schedule_status.lock() {
                status.last_success_at = Some(completed_at);
                status.last_message_count = Some(export.message_count);
                status.last_media_count = Some(export.media_count);
            }
            set_next_run_from_completion(&upload_state, &config.collector_schedule);
            Ok(UploadResult {
                message_count: export.message_count,
            })
        })
        .await
        .unwrap_or_else(|_| Err(internal_error()));
    finish_collector_attempt(&state, &result);
    result
}

#[tauri::command]
fn export_latest_enterprise(
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
    let result = (|| -> Result<(), std::io::Error> {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        output.write_all(&package)?;
        output.sync_all()?;
        std::fs::hard_link(&partial, &target)
    })();
    let _ = fs::remove_file(&partial);
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

#[tauri::command]
#[cfg(windows)]
fn open_offline_export_directory() -> Result<(), CommandError> {
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
fn open_offline_export_directory() -> Result<(), CommandError> {
    Err(CommandError {
        code: "PLATFORM_UNSUPPORTED",
        message: "当前平台暂不支持打开导出目录。".into(),
        recoverable: false,
    })
}

fn post_enterprise_package(
    endpoint: &str,
    token: &[u8],
    package: &[u8],
) -> Result<String, CommandError> {
    let token = std::str::from_utf8(token).map_err(|_| internal_error())?;
    let response = collector_http::post_json(
        endpoint,
        token,
        package,
        "application/octet-stream",
        15,
        300,
    )
    .map_err(|_| CommandError {
        code: "ENTERPRISE_UPLOAD_FAILED",
        message: "企业加密包上传失败，请检查服务端连接后重试。".into(),
        recoverable: true,
    })?;
    if !response.is_success() {
        return Err(CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message: format!("服务端未接受企业加密包（HTTP {}）。", response.status),
            recoverable: true,
        });
    }
    serde_json::from_slice::<EnterpriseUploadResponse>(&response.body)
        .map(|response| response.completed_at)
        .map_err(|_| CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message: "服务端未返回有效的上传完成时间。".into(),
            recoverable: true,
        })
}

fn collector_control_payload(
    config: &EnterpriseCollectorConfig,
    expires_at: &str,
) -> Result<Vec<u8>, CommandError> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": config.config_revision,
        "expiresAt": expires_at,
        "config": {
            "collectorId": config.collector_id,
            "displayName": config.display_name,
            "enabled": config.enabled,
            "configRevision": config.config_revision,
            "organizationId": config.organization_id,
            "organizationName": config.organization_name,
            "collectionNotice": config.collection_notice,
            "uploadUrl": config.upload_url,
            "keyId": config.key_id,
            "publicKeyHex": config.public_key_hex,
            "includeMedia": config.include_files || config.include_images,
            "dataRedaction": config.data_redaction,
            "offlineExportEnabled": config.offline_export_enabled,
            "hiddenModeEnabled": config.hidden_mode_enabled,
            "collectorLogLevel": config.collector_log_level,
            "manualCollectionRequestId": config.manual_collection_request_id,
            "schedule": config.collector_schedule,
        },
    }))
    .map_err(|_| internal_error())
}

fn load_enterprise_collector_config() -> Result<EnterpriseCollectorConfig, CommandError> {
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let embedded_bytes =
        archive_transfer::read_enterprise_collector_config(&executable).map_err(|_| {
            CommandError {
                code: "ENTERPRISE_CONFIG_MISSING",
                message: "采集端内未找到企业配置。".into(),
                recoverable: false,
            }
        })?;
    let embedded: EnterpriseCollectorConfig =
        serde_json::from_slice(&embedded_bytes).map_err(|_| CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置无效。".into(),
            recoverable: false,
        })?;
    let runtime = collector_runtime_config_path(&embedded.collector_id)
        .ok()
        .and_then(|path| source_windows::dpapi::load_current_user(&path).ok())
        .or_else(|| {
            legacy_collector_runtime_config_path()
                .ok()
                .and_then(|path| source_windows::dpapi::load_current_user(&path).ok())
        })
        .and_then(|bytes| serde_json::from_slice::<EnterpriseCollectorConfig>(&bytes).ok())
        .filter(|config| config.collector_id == embedded.collector_id);
    let config = runtime.unwrap_or(embedded);
    let key = hex::decode(&config.public_key_hex).map_err(|_| CommandError {
        code: "ENTERPRISE_CONFIG_INVALID",
        message: "企业采集配置中的加密密钥无效。".into(),
        recoverable: false,
    })?;
    let signing_public_key =
        hex::decode(&config.signing_public_key_hex).map_err(|_| CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置中的签名公钥无效。".into(),
            recoverable: false,
        })?;
    let signature = hex::decode(&config.signature_hex).map_err(|_| CommandError {
        code: "ENTERPRISE_CONFIG_INVALID",
        message: "企业采集配置中的签名无效。".into(),
        recoverable: false,
    })?;
    let signature_valid = if let Some(expires_at) = config.config_expires_at.as_deref() {
        collector_control_payload(&config, expires_at)
            .ok()
            .is_some_and(|payload| {
                source_windows::enterprise_crypto::verify(&signing_public_key, &payload, &signature)
                    .is_ok()
            })
    } else {
        let signing_payload = collector_signing_payload(&config);
        source_windows::enterprise_crypto::verify(
            &signing_public_key,
            signing_payload.as_bytes(),
            &signature,
        )
        .is_ok()
            || verify_legacy_collector_signature(&config, &signing_public_key, &signature, true)
            || (!config.offline_export_enabled
                && verify_legacy_collector_signature(
                    &config,
                    &signing_public_key,
                    &signature,
                    false,
                ))
    };
    if !signature_valid {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_SIGNATURE_INVALID",
            message: "企业采集配置签名校验失败。".into(),
            recoverable: false,
        });
    }
    if config.schema_version != "enterprise-collector.v1"
        || config.organization_id.trim().is_empty()
        || config.organization_name.trim().is_empty()
        || config.collection_notice.trim().is_empty()
        || config.key_id.trim().is_empty()
        || config.upload_url.trim().is_empty()
        || config.upload_token.trim().is_empty()
        || key.len() < 64
    {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置不完整或不允许离线运行。".into(),
            recoverable: false,
        });
    }
    Ok(config)
}

fn ensure_collector_config_fresh(config: &EnterpriseCollectorConfig) -> Result<(), CommandError> {
    if config
        .config_expires_at
        .as_deref()
        .is_some_and(|expires_at| {
            chrono::DateTime::parse_from_rfc3339(expires_at)
                .ok()
                .is_none_or(|value| value.with_timezone(&Utc) <= Utc::now())
        })
    {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_EXPIRED",
            message: "企业采集配置已过期，请等待服务端同步。".into(),
            recoverable: true,
        });
    }
    Ok(())
}

const REMEMBERED_SOURCE_ROOTS_SCHEMA: u32 = 1;
const MAX_REMEMBERED_SOURCE_ROOTS: usize = 3;

#[derive(Debug, Deserialize, Serialize)]
struct RememberedSourceRoots {
    schema_version: u32,
    roots: Vec<PathBuf>,
}

fn source_roots_config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("source-roots.dpapi")
}

fn load_remembered_source_roots(path: &Path) -> Vec<PathBuf> {
    let Ok(plaintext) = source_windows::dpapi::load_current_user(path) else {
        return Vec::new();
    };
    let Ok(stored) = serde_json::from_slice::<RememberedSourceRoots>(&plaintext) else {
        return Vec::new();
    };
    if stored.schema_version != REMEMBERED_SOURCE_ROOTS_SCHEMA {
        return Vec::new();
    }
    stored
        .roots
        .into_iter()
        .filter_map(|root| source_windows::roots::resolve_source_root(&root))
        .take(MAX_REMEMBERED_SOURCE_ROOTS)
        .collect()
}

fn remember_source_root(path: &Path, root: &Path) -> Result<(), String> {
    // Local collection and quick export can finish concurrently. Serialize the
    // read-modify-write so they don't share a DPAPI partial file or lose roots.
    let _guard = SOURCE_ROOTS_FILE_LOCK
        .lock()
        .map_err(|_| "无法保存企业微信数据目录。".to_owned())?;
    let mut roots = vec![root.to_path_buf()];
    roots.extend(
        load_remembered_source_roots(path)
            .into_iter()
            .filter(|existing| existing != root),
    );
    roots.truncate(MAX_REMEMBERED_SOURCE_ROOTS);
    let payload = serde_json::to_vec(&RememberedSourceRoots {
        schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA,
        roots,
    })
    .map_err(|_| "无法保存企业微信数据目录。".to_owned())?;
    source_windows::dpapi::store_current_user(path, &payload)
        .map_err(|_| "无法保存企业微信数据目录。".to_owned())
}

fn collection_roots(
    pasted_root: Option<&str>,
    remembered: &[PathBuf],
    detected: &[PathBuf],
) -> Vec<PathBuf> {
    // An explicit retry must never silently collect a different account from
    // another root. Automatic discovery still combines saved and default roots.
    if let Some(raw) = pasted_root {
        return normalize_source_root(raw).into_iter().collect();
    }
    let mut roots = Vec::new();
    for root in remembered.iter().chain(detected) {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    roots
}

fn discover_local_sources() -> Vec<SourceCandidate> {
    source_windows::discover_roots(&collection_roots(
        None,
        &load_remembered_source_roots(&source_roots_config_path()),
        &source_windows::default_roots(),
    ))
}

#[tauri::command]
fn verify_collector_control_envelope(
    control: &serde_json::Value,
    signing_public_key_hex: &str,
    current_revision: u64,
) -> Result<(), CommandError> {
    if control
        .get("schemaVersion")
        .and_then(serde_json::Value::as_str)
        != Some("collector-control.v1")
    {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置版本无效。".into(),
            recoverable: true,
        });
    }
    let revision = control
        .get("configRevision")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置修订号无效。".into(),
            recoverable: true,
        })?;
    if revision < current_revision {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_ROLLBACK",
            message: "服务端返回了较旧的采集配置，已拒绝应用。".into(),
            recoverable: true,
        });
    }
    let expires_at = control
        .get("expiresAt")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置缺少有效期。".into(),
            recoverable: true,
        })?;
    if chrono::DateTime::parse_from_rfc3339(expires_at)
        .ok()
        .is_none_or(|value| value.with_timezone(&Utc) <= Utc::now())
    {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_EXPIRED",
            message: "服务端控制配置已过期。".into(),
            recoverable: true,
        });
    }
    let signature = control
        .get("signatureHex")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置缺少签名。".into(),
            recoverable: true,
        })?;
    let payload = serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": revision,
        "expiresAt": expires_at,
        "config": control.get("config").ok_or_else(internal_error)?,
    });
    let public_key = hex::decode(signing_public_key_hex).map_err(|_| CommandError {
        code: "COLLECTOR_CONFIG_INVALID",
        message: "服务端签名公钥无效。".into(),
        recoverable: true,
    })?;
    let signature = hex::decode(signature).map_err(|_| CommandError {
        code: "COLLECTOR_CONFIG_INVALID",
        message: "服务端控制配置签名无效。".into(),
        recoverable: true,
    })?;
    let bytes = serde_json::to_vec(&payload).map_err(|_| internal_error())?;
    source_windows::enterprise_crypto::verify(&public_key, &bytes, &signature).map_err(|_| {
        CommandError {
            code: "COLLECTOR_CONFIG_SIGNATURE_INVALID",
            message: "服务端控制配置签名校验失败。".into(),
            recoverable: true,
        }
    })
}

fn server_base_url(upload_url: &str) -> &str {
    upload_url
        .trim_end_matches('/')
        .strip_suffix("/api/v1/imports/enterprise")
        .unwrap_or(upload_url.trim_end_matches('/'))
}

fn collector_heartbeat_failure(
    response: &collector_http::Response,
    status_state: Option<&Arc<Mutex<CollectorScheduleStatus>>>,
    app_state: Option<&AppState>,
) -> CommandError {
    let unauthorized = response.status == 401
        && serde_json::from_slice::<serde_json::Value>(&response.body)
            .ok()
            .is_some_and(|value| {
                value.get("code").and_then(serde_json::Value::as_str) == Some("UNAUTHORIZED")
            });
    if unauthorized {
        if let Some(state) = status_state
            && let Ok(mut status) = state.lock()
        {
            status.server_authorization_denied = true;
            status.last_error = Some("采集端授权已失效，后台采集已停止。".into());
        }
        if let Some(state) = app_state {
            state.scheduler_shutdown.store(true, Ordering::Release);
            if let Ok(Some(app)) = state.tray_app.lock().map(|handle| handle.clone()) {
                app.exit(0);
            }
        }
    }
    CommandError {
        code: if unauthorized {
            "COLLECTOR_AUTHORIZATION_DENIED"
        } else {
            "COLLECTOR_HEARTBEAT_FAILED"
        },
        message: if unauthorized {
            "采集端授权已失效，后台采集已停止。"
        } else {
            "服务端暂时不可用，将稍后重试连接。"
        }
        .into(),
        recoverable: !unauthorized,
    }
}

fn sync_collector_config_with_status(
    status_state: Option<&Arc<Mutex<CollectorScheduleStatus>>>,
    app_state: Option<&AppState>,
) -> Result<(), CommandError> {
    let current = load_enterprise_collector_config()?;
    diagnostics::write(
        "heartbeat_started",
        &format!(
            "revision={};hidden_mode={}",
            current.config_revision, current.hidden_mode_enabled
        ),
    );
    if current.collector_id.is_empty() {
        return Ok(());
    }
    let endpoint = format!(
        "{}/api/v1/collectors/{}/heartbeat",
        server_base_url(&current.upload_url),
        current.collector_id
    );
    let status = status_state.and_then(|state| state.lock().ok().map(|value| value.clone()));
    let current_revision = if ensure_collector_config_fresh(&current).is_ok() {
        current.config_revision
    } else {
        0
    };
    let pending_diagnostics = diagnostics::take_events();
    let body = serde_json::to_vec(&serde_json::json!({ "updateCapable": true, "softwareUpdateError": app_state.and_then(|state| state.software_update_error.lock().ok().and_then(|value| value.clone())), "clientVersion": env!("CARGO_PKG_VERSION"), "currentConfigRevision": current_revision, "lastAppliedRevision": current.config_revision, "lastRunAt": status.as_ref().and_then(|value| value.last_attempt_at.clone()), "lastSuccessAt": status.as_ref().and_then(|value| value.last_success_at.clone()), "lastError": status.as_ref().and_then(|value| value.last_error.clone()), "lastManualCollectionRequestId": status.as_ref().and_then(|value| value.last_manual_collection_request_id.clone()), "diagnostics": pending_diagnostics.iter().map(|event| CollectorDiagnosticEvent { timestamp: event.timestamp, event: event.event.clone(), details: event.details.clone() }).collect::<Vec<_>>() })).map_err(|_| internal_error())?;
    let response = match collector_http::post_json(
        &endpoint,
        &current.upload_token,
        &body,
        "application/json",
        10,
        30,
    ) {
        Ok(response) => response,
        Err(_) => {
            diagnostics::restore_events(pending_diagnostics);
            diagnostics::write("heartbeat_failed", "transport_failed");
            return Err(CommandError {
                code: "COLLECTOR_HEARTBEAT_FAILED",
                message: "无法连接服务端，将稍后重试。".into(),
                recoverable: true,
            });
        }
    };
    if !response.is_success() {
        diagnostics::restore_events(pending_diagnostics);
        diagnostics::write("heartbeat_failed", "http_request_failed");
        return Err(collector_heartbeat_failure(
            &response,
            status_state,
            app_state,
        ));
    }
    if let Some(state) = status_state
        && let Ok(mut status) = state.lock()
    {
        status.server_authorization_denied = false;
    }
    let response: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| internal_error())?;
    if let (Some(value), Some(state)) = (response.get("softwareUpdate"), app_state) {
        collector_update::receive(value, &current, state);
    }
    let Some(control) = response.get("control").filter(|value| !value.is_null()) else {
        diagnostics::write("heartbeat_ok", "control=none");
        return Ok(());
    };
    let control_hidden_mode = control
        .get("config")
        .and_then(|value| value.get("hiddenModeEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    diagnostics::write(
        "heartbeat_control",
        &format!("hidden_mode={control_hidden_mode}"),
    );
    verify_collector_control_envelope(
        control,
        &current.signing_public_key_hex,
        current.config_revision,
    )?;
    let config = control.get("config").ok_or_else(internal_error)?;
    let mut artifact = serde_json::to_value(&current).map_err(|_| internal_error())?;
    let object = artifact.as_object_mut().ok_or_else(internal_error)?;
    for (from, to) in [
        ("displayName", "displayName"),
        ("organizationName", "organizationName"),
        ("collectionNotice", "collectionNotice"),
        ("uploadUrl", "uploadUrl"),
        ("keyId", "keyId"),
        ("publicKeyHex", "publicKeyHex"),
        ("dataRedaction", "dataRedaction"),
        ("offlineExportEnabled", "offlineExportEnabled"),
        ("hiddenModeEnabled", "hiddenModeEnabled"),
        ("collectorLogLevel", "collectorLogLevel"),
        ("manualCollectionRequestId", "manualCollectionRequestId"),
        ("schedule", "collectorSchedule"),
        ("configRevision", "configRevision"),
    ] {
        if let Some(value) = config.get(from) {
            object.insert(to.into(), value.clone());
        }
    }
    if let Some(include_media) = config.get("includeMedia") {
        object.insert("includeFiles".into(), include_media.clone());
        object.insert("includeImages".into(), include_media.clone());
    }
    if let Some(value) = control.get("expiresAt") {
        object.insert("configExpiresAt".into(), value.clone());
    }
    if let Some(value) = config.get("displayName") {
        object.insert("displayName".into(), value.clone());
    }
    if let Some(value) = control.get("signatureHex") {
        object.insert("signatureHex".into(), value.clone());
    }
    if let Some(enabled) = config.get("enabled").and_then(|value| value.as_bool()) {
        object.insert("enabled".into(), enabled.into());
    }
    let path = collector_runtime_config_path(&current.collector_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| internal_error())?;
    }
    source_windows::dpapi::store_current_user(
        &path,
        &serde_json::to_vec_pretty(&artifact).map_err(|_| internal_error())?,
    )
    .map_err(|_| internal_error())?;
    if let Some(app_state) = app_state
        && let Ok(Some(app)) = app_state.tray_app.lock().map(|handle| handle.clone())
    {
        let latest = load_enterprise_collector_config()?;
        diagnostics::set_level(&latest.collector_log_level);
        diagnostics::write(
            "apply_visibility",
            &format!("hidden_mode={}", latest.hidden_mode_enabled),
        );
        apply_collector_visibility(&app, &latest).map_err(|_| internal_error())?;
        let _ = app.emit("collector:schedule-updated", &latest.collector_schedule);
    }
    Ok(())
}

#[tauri::command]
async fn sync_collector_config(state: State<'_, AppState>) -> Result<(), CommandError> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        sync_collector_config_with_status(Some(&state.schedule_status), Some(&state))
    })
    .await
    .map_err(|_| internal_error())?
}

fn collector_signing_payload(config: &EnterpriseCollectorConfig) -> String {
    format!(
        "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        config.organization_id,
        config.organization_name,
        config.collection_notice,
        config.key_id,
        config.public_key_hex,
        "aes-256-gcm+rsa-oaep-sha256",
        config.upload_url,
        config.upload_token,
        config.include_files,
        config.include_images,
        config.data_redaction,
        config.offline_export_enabled,
        config.hidden_mode_enabled,
        match config.collector_schedule.mode {
            CollectionScheduleMode::Disabled => "disabled",
            CollectionScheduleMode::Interval => "interval",
            CollectionScheduleMode::Daily => "daily",
        },
        config.collector_schedule.interval_minutes,
        config.collector_schedule.daily_time,
    )
}

fn verify_legacy_collector_signature(
    config: &EnterpriseCollectorConfig,
    signing_public_key: &[u8],
    signature: &[u8],
    include_offline_export: bool,
) -> bool {
    let payload = if include_offline_export {
        format!(
            "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            config.organization_id,
            config.organization_name,
            config.collection_notice,
            config.key_id,
            config.public_key_hex,
            "aes-256-gcm+rsa-oaep-sha256",
            config.upload_url,
            config.upload_token,
            config.include_files,
            config.include_images,
            config.data_redaction,
            config.offline_export_enabled,
        )
    } else {
        format!(
            "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            config.organization_id,
            config.organization_name,
            config.collection_notice,
            config.key_id,
            config.public_key_hex,
            "aes-256-gcm+rsa-oaep-sha256",
            config.upload_url,
            config.upload_token,
            config.include_files,
            config.include_images,
            config.data_redaction,
        )
    };
    source_windows::enterprise_crypto::verify(signing_public_key, payload.as_bytes(), signature)
        .is_ok()
}

fn create_enterprise_package(
    export: &ClientExportV1,
    config: &EnterpriseCollectorConfig,
) -> Result<Vec<u8>, CommandError> {
    let public_key = hex::decode(&config.public_key_hex).map_err(|_| internal_error())?;
    let redacted_export = if config.data_redaction {
        redact_sensitive_export(export)
    } else {
        export.clone()
    };
    let plaintext =
        Zeroizing::new(serde_json::to_vec(&redacted_export).map_err(|_| internal_error())?);
    let encrypted =
        source_windows::enterprise_crypto::encrypt(&public_key, &plaintext).map_err(|_| {
            CommandError {
                code: "ENTERPRISE_ENCRYPTION_FAILED",
                message: "企业加密包生成失败；不会降级为明文导出。".into(),
                recoverable: true,
            }
        })?;
    archive_transfer::create_enterprise_package(
        &redacted_export,
        &config.organization_id,
        &config.key_id,
        &encrypted.wrapped_dek,
        &encrypted.nonce,
        &encrypted.ciphertext,
        &encrypted.tag,
    )
    .map_err(|_| CommandError {
        code: "ENTERPRISE_ENCRYPTION_FAILED",
        message: "企业加密包生成失败；不会降级为明文导出。".into(),
        recoverable: true,
    })
}

fn redact_sensitive_export(export: &ClientExportV1) -> ClientExportV1 {
    let mut redacted = export.clone();
    for batch in &mut redacted.batches {
        archive_transfer::redact_sensitive_messages(&mut batch.messages);
        batch.content_sha256 = hex::encode(Sha256::digest(
            serde_json::to_vec(&batch.messages).unwrap_or_default(),
        ));
    }
    let checksum_value = if redacted.media_transport == "embedded_hex_v1" {
        serde_json::to_vec(&(
            &redacted.conversations,
            &redacted.participants,
            &redacted.batches,
            &redacted.media_blobs,
        ))
    } else {
        serde_json::to_vec(&(
            &redacted.conversations,
            &redacted.participants,
            &redacted.batches,
        ))
    };
    redacted.content_sha256 = hex::encode(Sha256::digest(checksum_value.unwrap_or_default()));
    redacted
}

#[cfg(test)]
fn redact_sensitive_text(value: &str) -> String {
    archive_transfer::redact_sensitive_text(value)
}

#[cfg(test)]
fn redact_sensitive_json(value: &mut serde_json::Value) {
    archive_transfer::redact_sensitive_json(value);
}

fn store_candidates(
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

fn resolve_collection_key(
    candidate: &SourceCandidate,
    encrypted: bool,
    data_root: &Path,
) -> Result<Option<SourceKey>, CommandError> {
    if !encrypted {
        return Ok(None);
    }
    let candidates = run_isolated_key_probe(candidate, data_root)?;
    for key in candidates {
        if key_opens_candidate(candidate, &key) {
            return Ok(Some(key));
        }
    }
    Err(CommandError {
        code: "SOURCE_KEY_NOT_FOUND",
        message: "自动解析未找到可验证的数据库密钥；未读取或导出任何消息。".into(),
        recoverable: true,
    })
}

fn choose_collectable_source(
    candidates: Vec<SourceCandidate>,
    resolve_key: &mut dyn FnMut(&SourceCandidate) -> Result<Option<SourceKey>, CommandError>,
) -> Result<(SourceCandidate, Option<SourceKey>), CommandError> {
    // Only the logged-in account's key is present in memory, so an account whose key
    // cannot be resolved is not a failure — it is just not the account to collect.
    let mut last_error = None;
    for candidate in candidates {
        match resolve_key(&candidate) {
            Ok(key) => return Ok((candidate, key)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or(CommandError {
        code: "SOURCE_NOT_FOUND",
        message: "未发现可支持的本机企业微信数据，请确认客户端已登录。".into(),
        recoverable: true,
    }))
}

fn key_opens_candidate(candidate: &SourceCandidate, key: &SourceKey) -> bool {
    candidate
        .databases
        .iter()
        .find(|database| database.kind == "message")
        .is_some_and(|database| match key.kind {
            SourceKeyKind::RawWxSqlite3Key => {
                sqlite3mc::quick_verify_raw_wxsqlite3_key(&database.path, &key.bytes)
            }
            _ => open_source_database(&database.path, Some(key)).is_ok(),
        })
}

fn open_source_database(
    path: &Path,
    key: Option<&SourceKey>,
) -> Result<rusqlite::Connection, sqlite3mc::CipherDatabaseError> {
    match key {
        None => sqlite3mc::open_read_only(path, None),
        Some(SourceKey {
            kind: SourceKeyKind::Passphrase,
            bytes,
            ..
        }) => sqlite3mc::open_read_only(path, Some(bytes)),
        Some(SourceKey {
            kind: SourceKeyKind::DerivedAes128,
            bytes,
            legacy,
            legacy_page_size,
        }) => sqlite3mc::open_read_only_derived_aes128(path, bytes, *legacy, *legacy_page_size),
        Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) => sqlite3mc::open_read_only_derived_aes128(path, bytes, false, 0),
        Some(SourceKey {
            kind: SourceKeyKind::DerivedAes256,
            bytes,
            legacy,
            legacy_page_size,
        }) => sqlite3mc::open_read_only_derived_aes256(path, bytes, *legacy, *legacy_page_size),
    }
}

#[cfg(windows)]
fn run_isolated_key_probe(
    candidate: &SourceCandidate,
    data_root: &Path,
) -> Result<Vec<SourceKey>, CommandError> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let validation_page_prefix_hex = validation_page_prefix(candidate, data_root);
    let request = source_windows::probe::ProbeRequest {
        authorization_token: token.clone(),
        max_candidates: 640,
        validation_page_prefix_hex,
    };
    #[cfg(test)]
    let executable = std::env::var_os("WECOM_ARCHIVE_PROBE_EXE")
        .map(PathBuf::from)
        .ok_or_else(internal_error)?;
    #[cfg(not(test))]
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let mut child = Command::new(executable)
        .arg("--key-probe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| probe_error("KEY_PROBE_START_FAILED", "无法启动一次性只读密钥探针。"))?;
    let request_bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| internal_error())?);
    child
        .stdin
        .take()
        .ok_or_else(internal_error)?
        .write_all(&request_bytes)
        .map_err(|_| probe_error("KEY_PROBE_PROTOCOL_FAILED", "无法授权一次性只读密钥探针。"))?;
    let output = child
        .wait_with_output()
        .map_err(|_| probe_error("KEY_PROBE_FAILED", "一次性只读密钥探针未完成。"))?;
    let stdout = Zeroizing::new(output.stdout);
    let mut response: source_windows::probe::ProbeResponse = serde_json::from_slice(&stdout)
        .map_err(|_| probe_error("KEY_PROBE_PROTOCOL_FAILED", "密钥探针返回了无效响应。"))?;
    if !output.status.success()
        || !response.ok
        || response.authorization_token != token
        || response.code != "PROBE_COMPLETED"
    {
        response.authorization_token.zeroize();
        for candidate in &mut response.candidates {
            candidate.value_hex.zeroize();
        }
        return Err(probe_error(
            "KEY_PROBE_TRUST_FAILED",
            "未找到与已验证企业微信版本匹配的签名可信进程。",
        ));
    }
    response.authorization_token.zeroize();
    let mut candidates = Vec::with_capacity(response.candidates.len());
    for candidate in &mut response.candidates {
        if let Ok(decoded) = hex::decode(candidate.value_hex.as_bytes()) {
            let key = match candidate.kind {
                source_windows::probe::ProbeCandidateKind::DerivedAes128 => {
                    SourceKey::derived_aes128_with_config(
                        &decoded,
                        candidate.legacy.unwrap_or(false),
                        candidate.legacy_page_size.unwrap_or(0),
                    )
                    .ok()
                }
                source_windows::probe::ProbeCandidateKind::DerivedAes256 => {
                    SourceKey::derived_aes256_with_config(
                        &decoded,
                        candidate.legacy.unwrap_or(false),
                        candidate.legacy_page_size.unwrap_or(0),
                    )
                    .ok()
                }
                source_windows::probe::ProbeCandidateKind::RawWxSqlite3Key => {
                    if decoded.len() == 16 {
                        Some(SourceKey {
                            kind: SourceKeyKind::RawWxSqlite3Key,
                            bytes: Zeroizing::new(decoded),
                            legacy: false,
                            legacy_page_size: 0,
                        })
                    } else {
                        None
                    }
                }
                source_windows::probe::ProbeCandidateKind::Passphrase => {
                    Some(SourceKey::passphrase(&decoded))
                }
            };
            if let Some(key) = key {
                candidates.push(key);
            }
        }
        candidate.value_hex.zeroize();
    }
    Ok(candidates)
}

#[cfg(windows)]
fn validation_page_prefix(candidate: &SourceCandidate, data_root: &Path) -> Option<String> {
    let message_path = candidate
        .databases
        .iter()
        .find(|database| database.kind == "message")
        .map(|database| database.path.clone())?;
    let mut page_prefix = [0_u8; 32];
    if File::open(&message_path)
        .and_then(|mut file| file.read_exact(&mut page_prefix))
        .is_ok()
    {
        return Some(hex::encode(page_prefix));
    }
    let work_root = create_work_root(data_root, "probe").ok()?;
    let result = (|| {
        let receipt = WindowsSourceAdapter
            .snapshot(candidate, work_root.clone())
            .ok()?;
        let copied = receipt.copied_files.iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })?;
        File::open(copied)
            .and_then(|mut file| file.read_exact(&mut page_prefix))
            .ok()
            .map(|_| hex::encode(page_prefix))
    })();
    let _ = fs::remove_dir_all(&work_root);
    result
}

#[cfg(not(windows))]
fn run_isolated_key_probe(
    _candidate: &SourceCandidate,
    _data_root: &Path,
) -> Result<Vec<SourceKey>, CommandError> {
    Err(probe_error(
        "KEY_PROBE_UNAVAILABLE",
        "当前平台不支持 Windows 密钥探针。",
    ))
}

fn probe_error(code: &'static str, message: &str) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        recoverable: true,
    }
}

#[cfg(test)]
fn collect_candidate(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
) -> Result<ClientExportV1, CommandError> {
    collect_candidate_with_options(
        candidate,
        work_root,
        key,
        authorization,
        MediaCollectionOptions::default(),
        None,
        None,
    )
}

fn collect_candidate_with_options(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    media_options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
) -> Result<ClientExportV1, CommandError> {
    collect_candidate_with_options_since(
        candidate,
        work_root,
        key,
        authorization,
        media_options,
        media_sink,
        progress,
        None,
    )
}

fn collect_candidate_with_options_since(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    media_options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
    since_unix_ms: Option<i64>,
) -> Result<ClientExportV1, CommandError> {
    if candidate
        .databases
        .iter()
        .any(|database| database.encrypted)
        && key.is_none()
    {
        return Err(CommandError {
            code: "SOURCE_KEY_REQUIRED",
            message: "检测到加密数据库，需要手动密钥或已保存的本机密钥。".into(),
            recoverable: true,
        });
    }
    report_collection_progress(
        progress,
        30,
        "创建快照",
        "正在复制数据库、WAL 和 SHM 文件。",
    );
    let receipt = WindowsSourceAdapter
        .snapshot(&candidate, work_root.clone())
        .map_err(|error| CommandError {
            code: "SOURCE_SNAPSHOT_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?;
    report_collection_progress(
        progress,
        42,
        "打开快照",
        "数据库快照已完成，正在安全打开副本。",
    );
    let result = read_snapshot_export_with_options(
        &candidate,
        &receipt,
        key,
        authorization,
        media_options,
        media_sink,
        progress,
        since_unix_ms,
    );
    cleanup_snapshot(&work_root, &receipt.snapshot_root);
    result
}

fn read_snapshot_export_with_options(
    candidate: &SourceCandidate,
    receipt: &archive_domain::SnapshotReceipt,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
    media_options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
    since_unix_ms: Option<i64>,
) -> Result<ClientExportV1, CommandError> {
    report_collection_progress(
        progress,
        46,
        "解析数据库",
        "正在打开只读快照并识别消息结构。",
    );
    let message_database = receipt
        .copied_files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })
        .ok_or_else(unsupported_source_schema)?;
    let (database_path, database_key) =
        if let Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) = key
        {
            let plain = sqlite3mc::decrypt_raw_wxsqlite3_database(message_database, bytes)
                .map_err(|_| CommandError {
                    code: "SOURCE_KEY_INVALID",
                    message: "密钥无法解密数据库页；未读取或导出任何消息。".into(),
                    recoverable: true,
                })?;
            (plain, None)
        } else {
            (message_database.to_path_buf(), key)
        };
    let connection =
        open_source_database(&database_path, database_key).map_err(|error| match error {
            sqlite3mc::CipherDatabaseError::InvalidKey => CommandError {
                code: "SOURCE_KEY_INVALID",
                message: "密钥无法打开加密数据库；未读取或导出任何消息。".into(),
                recoverable: true,
            },
            _ => CommandError {
                code: "DECRYPTION_ENGINE_FAILED",
                message: "解密引擎无法安全打开数据库；未读取或导出任何消息。".into(),
                recoverable: true,
            },
        })?;
    let batch_id = uuid::Uuid::new_v4();
    let mut messages = Vec::new();
    let mut media_blobs = Vec::new();
    if media_options.include_files || media_options.include_images {
        report_collection_progress(
            progress,
            52,
            "扫描媒体目录",
            "正在建立图片和文件索引，不会修改源目录。",
        );
    }
    let media_resolver = MediaResolver::new(candidate, media_options);
    report_collection_progress(progress, 60, "读取消息", "正在解析聊天消息及附件引用。");
    let has_normalized = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='messages')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| unsupported_source_schema())?
        != 0;
    if has_normalized {
        let query = if since_unix_ms.is_some() {
            "SELECT source_message_id, conversation_id, sender_id, sent_at_unix_ms,
                    outgoing, raw_type, payload_json, recalled
             FROM messages WHERE sent_at_unix_ms >= ?1
             ORDER BY sent_at_unix_ms, source_message_id"
        } else {
            "SELECT source_message_id, conversation_id, sender_id, sent_at_unix_ms,
                    outgoing, raw_type, payload_json, recalled
             FROM messages ORDER BY sent_at_unix_ms, source_message_id"
        };
        let mut statement = connection
            .prepare(query)
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(since_unix_ms), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .map_err(|_| unsupported_source_schema())?;
        for row in rows {
            let (
                source_message_id,
                conversation_id,
                sender_id,
                sent_at_unix_ms,
                outgoing,
                raw_type,
                payload_json,
                recalled,
            ) = row.map_err(|_| unsupported_source_schema())?;
            let payload =
                serde_json::from_str(&payload_json).map_err(|_| unsupported_source_schema())?;
            let mut message = normalize(
                RawMessageRow {
                    source_instance_id: candidate.source_id.clone(),
                    source_message_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    outgoing: outgoing.map(|value| value != 0),
                    raw_type,
                    payload,
                    recalled: recalled != 0,
                },
                batch_id,
            )
            .map_err(|_| unsupported_source_schema())?;
            ensure_media_reference(&mut message);
            media_blobs.extend(resolve_media_metadata_with_local_paths(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
                &[],
            )?);
            messages.push(message);
        }
    } else {
        let has_wecom = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='message_table')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| unsupported_source_schema())?
            != 0;
        if !has_wecom {
            return Err(unsupported_source_schema());
        }
        let source_cutoff = since_unix_ms.and_then(|cutoff| {
            connection
                .query_row("SELECT MAX(send_time) FROM message_table", [], |row| {
                    Ok(sqlite_value_to_i64(row.get_ref(0)?))
                })
                .ok()
                .flatten()
                .map(|sample| source_timestamp_cutoff(cutoff, sample))
        });
        let local_media_column = if table_columns(&connection, "message_table")
            .iter()
            .any(|column| column == "local_extra_content")
        {
            "local_extra_content"
        } else {
            "NULL"
        };
        let predicate = if source_cutoff.is_some() {
            "WHERE send_time >= ?1"
        } else {
            ""
        };
        let query = format!(
            "SELECT rowid, send_time, conversation_id, sender_id, content_type, content, {local_media_column}
             FROM message_table {predicate} ORDER BY send_time, rowid"
        );
        let mut statement = connection
            .prepare(&query)
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(source_cutoff), |row| {
                let row_id = sqlite_value_to_string(row.get_ref(0)?);
                let send_time = sqlite_value_to_i64(row.get_ref(1)?).unwrap_or_default();
                let sent_at_unix_ms = normalize_source_timestamp(send_time);
                let conversation_id = sqlite_value_to_string(row.get_ref(2)?);
                let sender_id = match row.get_ref(3)? {
                    rusqlite::types::ValueRef::Null => None,
                    value => Some(sqlite_value_to_string(value)),
                };
                let raw_type = sqlite_value_to_string(row.get_ref(4)?);
                let content = sqlite_value_to_bytes(row.get_ref(5)?);
                let local_extra = sqlite_value_to_bytes(row.get_ref(6)?);
                Ok((
                    row_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    raw_type,
                    content,
                    local_extra,
                ))
            })
            .map_err(|_| unsupported_source_schema())?;
        for row in rows {
            let (
                source_message_id,
                conversation_id,
                sender_id,
                sent_at_unix_ms,
                raw_type,
                content,
                local_extra,
            ) = row.map_err(|_| unsupported_source_schema())?;
            if since_unix_ms.is_some_and(|cutoff| sent_at_unix_ms < cutoff) {
                continue;
            }
            let payload = readable_message_payload(&raw_type, &content);
            let local_paths = local_media_paths(&raw_type, &content, &local_extra);
            let mut message = normalize(
                RawMessageRow {
                    source_instance_id: candidate.source_id.clone(),
                    source_message_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    outgoing: None,
                    raw_type,
                    payload,
                    recalled: false,
                },
                batch_id,
            )
            .map_err(|_| unsupported_source_schema())?;
            ensure_media_reference(&mut message);
            media_blobs.extend(resolve_media_metadata_with_local_paths(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
                &local_paths,
            )?);
            messages.push(message);
        }
    }

    report_collection_progress(
        progress,
        76,
        "整理消息",
        "正在整理会话、发送人及群成员信息。",
    );

    let inferred_self_sender_id = infer_self_sender_id(&messages);
    if let Some(self_sender_id) = inferred_self_sender_id.as_deref() {
        for message in &mut messages {
            if message.direction == archive_domain::MessageDirection::Unknown {
                message.direction = if message.sender_id.as_deref() == Some(self_sender_id) {
                    archive_domain::MessageDirection::Outgoing
                } else if message.sender_id.is_some() {
                    archive_domain::MessageDirection::Incoming
                } else {
                    archive_domain::MessageDirection::System
                };
            }
        }
    }

    let mut display_names = DisplayNameIndex::default();
    let mut group_announcements = Vec::new();
    collect_display_names(&connection, &mut display_names);
    let mut metadata_paths = receipt
        .copied_files
        .iter()
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })
        .collect::<Vec<_>>();
    metadata_paths.sort_by_key(
        |path| match path.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.eq_ignore_ascii_case("user.db") => 0,
            Some(name) if name.eq_ignore_ascii_case("session.db") => 1,
            _ => 2,
        },
    );
    for metadata_path in metadata_paths {
        if let Some(metadata) = open_snapshot_metadata_database(metadata_path, key) {
            collect_display_names(&metadata, &mut display_names);
            collect_group_announcements(&metadata, &mut group_announcements);
        }
    }
    for (conversation_id, body, sent_at_unix_ms) in group_announcements {
        if messages.iter().any(|message| {
            message.conversation_id == conversation_id
                && message.body_text.as_deref() == Some(body.as_str())
        }) {
            continue;
        }
        let body_hash = hex::encode(Sha256::digest(body.as_bytes()));
        let source_message_id = format!(
            "group-announcement:{}:{}",
            conversation_id,
            &body_hash[..20]
        );
        let mut stable_hasher = Sha256::new();
        stable_hasher.update(b"message.v1\0");
        stable_hasher.update(candidate.source_id.as_bytes());
        stable_hasher.update(b"\0");
        stable_hasher.update(source_message_id.as_bytes());
        messages.push(archive_domain::MessageV1 {
            schema_version: archive_domain::MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "windows_local".into(),
            stable_message_id: hex::encode(stable_hasher.finalize()),
            source_instance_id: candidate.source_id.clone(),
            source_message_id,
            conversation_id,
            sender_id: None,
            sent_at: Utc
                .timestamp_millis_opt(sent_at_unix_ms)
                .single()
                .unwrap_or_else(Utc::now),
            direction: archive_domain::MessageDirection::System,
            message_type: MessageType::System,
            body_text: Some(body.clone()),
            quoted_message_id: None,
            lifecycle: archive_domain::LifecycleState::Active,
            media: Vec::new(),
            raw_type: "group_announcement".into(),
            raw_payload: serde_json::json!({ "kind": "group_announcement", "text": body }),
            parser_version: "windows-group-metadata/0.0.2".into(),
            collection_batch_id: batch_id,
        });
    }
    messages.sort_by(|left, right| {
        left.sent_at
            .cmp(&right.sent_at)
            .then_with(|| left.stable_message_id.cmp(&right.stable_message_id))
    });
    for message in &messages {
        if let (Some(sender_id), Some(name)) = (
            message.sender_id.as_ref(),
            payload_display_name(&message.raw_payload),
        ) {
            display_names.set_participant(sender_id, name, 30);
        }
    }

    let mut conversation_participants = BTreeMap::<String, Vec<String>>::new();
    let mut participant_ids = std::collections::BTreeSet::new();
    let mut outgoing_sender_counts = BTreeMap::<String, usize>::new();
    for message in &messages {
        let members = conversation_participants
            .entry(message.conversation_id.clone())
            .or_default();
        if let Some(sender_id) = &message.sender_id {
            if !members.contains(sender_id) {
                members.push(sender_id.clone());
            }
            participant_ids.insert(sender_id.clone());
            if message.direction == archive_domain::MessageDirection::Outgoing {
                *outgoing_sender_counts.entry(sender_id.clone()).or_default() += 1;
            }
        }
        if display_names
            .participants
            .contains_key(&message.conversation_id)
            && !members.contains(&message.conversation_id)
        {
            members.push(message.conversation_id.clone());
            participant_ids.insert(message.conversation_id.clone());
        }
    }
    for (conversation_id, scoped_members) in &display_names.conversation_members {
        let Some(members) = conversation_participants.get_mut(conversation_id) else {
            continue;
        };
        for member_id in scoped_members.keys() {
            if !members.iter().any(|member| member == member_id) {
                members.push(member_id.clone());
            }
            participant_ids.insert(member_id.clone());
        }
    }
    let self_sender_id = outgoing_sender_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(sender_id, _)| sender_id)
        .or(inferred_self_sender_id);
    for (conversation_id, members) in &mut conversation_participants {
        let Some(direct_members) = conversation_id.strip_prefix("S:") else {
            continue;
        };
        for member_id in direct_members.split('_') {
            if member_id.is_empty() {
                continue;
            }
            if !members.iter().any(|member| member == member_id) {
                members.push(member_id.to_owned());
            }
            participant_ids.insert(member_id.to_owned());
        }
    }
    let conversations = conversation_participants
        .into_iter()
        .map(|(conversation_id, participant_ids)| {
            let conversation_type = display_names
                .conversation_types
                .get(&conversation_id)
                .map(String::as_str)
                .unwrap_or_else(|| conversation_type(&conversation_id, participant_ids.len()));
            let metadata_name = || {
                display_names
                    .conversations
                    .get(&conversation_id)
                    .or_else(|| display_names.participants.get(&conversation_id))
                    .cloned()
            };
            let display_name = if conversation_type == "assistant"
                && display_names.conversations.contains_key(&conversation_id)
            {
                metadata_name().map(|name| {
                    self_sender_id
                        .as_ref()
                        .and_then(|id| display_names.participants.get(id))
                        .map(|employee| format!("{name}-{employee}"))
                        .unwrap_or(name)
                })
            } else if conversation_type == "direct" {
                direct_conversation_name(
                    &conversation_id,
                    &display_names.participants,
                    &display_names.conversation_members,
                )
                .or_else(metadata_name)
            } else if conversation_type == "system" {
                metadata_name().or_else(|| Some("系统通知".into()))
            } else {
                metadata_name()
            };
            ConversationV1 {
                conversation_id,
                display_name,
                conversation_type: Some(conversation_type.into()),
                participant_ids,
            }
        })
        .collect::<Vec<_>>();
    // Carry directory metadata once per conversation, including members who have
    // not sent a message. Keeping it in raw_payload preserves full JSON round trips.
    let members_by_conversation = conversations
        .iter()
        .map(|conversation| {
            (
                conversation.conversation_id.as_str(),
                &conversation.participant_ids,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut profiled_conversations = std::collections::BTreeSet::new();
    for message in &mut messages {
        if !profiled_conversations.insert(message.conversation_id.clone()) {
            continue;
        }
        let Some(members) = members_by_conversation.get(message.conversation_id.as_str()) else {
            continue;
        };
        let profiles = members
            .iter()
            .filter_map(|id| {
                display_names
                    .profiles
                    .get(id)
                    .map(|profile| (id.clone(), serde_json::Value::Object(profile.clone())))
            })
            .collect::<serde_json::Map<_, _>>();
        if !profiles.is_empty()
            && let Some(payload) = message.raw_payload.as_object_mut()
        {
            payload.insert(
                "participant_profiles".into(),
                serde_json::Value::Object(profiles),
            );
        }
    }
    let participants = participant_ids
        .into_iter()
        .map(|participant_id| ParticipantV1 {
            display_name: display_names
                .participants
                .get(&participant_id)
                .cloned()
                .or_else(|| {
                    if Some(participant_id.as_str()) == self_sender_id.as_deref() {
                        None
                    } else {
                        preferred_scoped_member_name(
                            &participant_id,
                            &display_names.conversation_members,
                        )
                    }
                }),
            participant_kind: display_names
                .participant_kinds
                .get(&participant_id)
                .cloned(),
            participant_id,
        })
        .collect::<Vec<_>>();
    let content_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&messages).map_err(|_| internal_error())?,
    ));
    report_collection_progress(
        progress,
        84,
        "校验媒体",
        "正在校验已采集图片和文件的完整性。",
    );
    media_blobs.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
    media_blobs.dedup_by(|left, right| left.content_hash == right.content_hash);
    let collected_at = Utc::now();
    let batch = ArchiveBatchV1 {
        schema_version: archive_domain::BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: source_windows::ADAPTER_ID.into(),
        source_instance_id: candidate.source_id.clone(),
        collected_at,
        employee_notice: EmployeeNoticeEvidence {
            evidence_hash: notice_evidence_hash(&authorization.notice_version),
            notice_version: authorization.notice_version,
            displayed_at: authorization.notice_displayed_at,
            acknowledged_at: None,
        },
        external_contact_consent: ExternalContactConsent::Unknown,
        collection_scope: CollectionScope {
            source_ids: vec![candidate.source_id.clone()],
            conversation_ids: conversations
                .iter()
                .map(|conversation| conversation.conversation_id.clone())
                .collect(),
            starts_at: None,
            ends_at: None,
            message_types: Vec::<MessageType>::new(),
        },
        cursor_before: None,
        cursor_after: Some(receipt.source_fingerprint.clone()),
        messages,
        content_sha256,
        retention: RetentionDirective {
            policy_id: None,
            expires_at: None,
            legal_hold: false,
        },
    };
    archive_transfer::create_export_with_media(
        candidate.client_version.as_deref().unwrap_or("unverified"),
        vec![batch],
        conversations,
        participants,
        media_blobs,
    )
    .map_err(|error| CommandError {
        code: "COLLECTION_EXPORT_INVALID",
        message: format!(
            "采集结果未通过本地完整性校验：{}",
            sanitize_error(&error.to_string())
        ),
        recoverable: false,
    })
}

fn report_collection_progress(
    progress: Option<&wecom_archive_server::LocalCollectionProgressReporter>,
    percent: u8,
    stage: &str,
    detail: &str,
) {
    if let Some(progress) = progress {
        progress(percent, stage, detail);
    }
}

#[derive(Default)]
struct DisplayNameIndex {
    participants: BTreeMap<String, String>,
    participant_priorities: BTreeMap<String, u8>,
    conversations: BTreeMap<String, String>,
    conversation_priorities: BTreeMap<String, u8>,
    conversation_members: BTreeMap<String, BTreeMap<String, String>>,
    conversation_types: BTreeMap<String, String>,
    participant_kinds: BTreeMap<String, String>,
    profiles: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    profile_priorities: BTreeMap<(String, String), u8>,
}

impl DisplayNameIndex {
    fn set_participant(&mut self, id: &str, name: String, priority: u8) {
        if self
            .participant_priorities
            .get(id)
            .is_none_or(|current| priority >= *current)
        {
            self.participants.insert(id.to_owned(), name);
            self.participant_priorities.insert(id.to_owned(), priority);
        }
    }

    fn set_profile(&mut self, id: &str, key: &str, value: String, priority: u8) {
        let id = id.trim();
        let value = value.trim();
        if id.is_empty() || value.is_empty() {
            return;
        }
        let profile_key = (id.to_owned(), key.to_owned());
        if self
            .profile_priorities
            .get(&profile_key)
            .is_none_or(|current| priority >= *current)
        {
            self.profiles
                .entry(id.to_owned())
                .or_default()
                .insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
            self.profile_priorities.insert(profile_key, priority);
        }
    }
}

fn infer_self_sender_id(messages: &[archive_domain::MessageV1]) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut seen = std::collections::BTreeSet::new();
    for message in messages {
        if !seen.insert(&message.conversation_id) {
            continue;
        }
        let Some(members) = message.conversation_id.strip_prefix("S:") else {
            continue;
        };
        for member in members.split('_').filter(|member| !member.is_empty()) {
            *counts.entry(member.to_owned()).or_default() += 1;
        }
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    match ranked.as_slice() {
        [(sender_id, count), rest @ ..]
            if rest
                .first()
                .is_none_or(|(_, next_count)| count > next_count) =>
        {
            Some(sender_id.clone())
        }
        _ => None,
    }
}

fn direct_conversation_name(
    conversation_id: &str,
    participants: &BTreeMap<String, String>,
    conversation_members: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    let scoped = conversation_members.get(conversation_id);
    let mut names = Vec::new();
    for participant_id in conversation_id.strip_prefix("S:")?.split('_') {
        let Some(name) = participants
            .get(participant_id)
            .or_else(|| scoped.and_then(|members| members.get(participant_id)))
        else {
            continue;
        };
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    (!names.is_empty()).then(|| names.join("、"))
}

fn conversation_type(conversation_id: &str, participant_count: usize) -> &'static str {
    if conversation_id.starts_with("S:") {
        "direct"
    } else if conversation_id.starts_with("M:") {
        "application"
    } else if conversation_id.starts_with("O:") {
        "assistant"
    } else if conversation_id.starts_with("T:") {
        "temporary"
    } else if conversation_id.starts_with("Y:") {
        "system"
    } else if conversation_id.starts_with("R:") || participant_count > 2 {
        "group"
    } else {
        "direct"
    }
}

fn preferred_scoped_member_name(
    participant_id: &str,
    conversation_members: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for members in conversation_members.values() {
        let Some(name) = members.get(participant_id) else {
            continue;
        };
        let Some(name) = clean_display_name(name) else {
            continue;
        };
        *counts.entry(name).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(name, _)| name)
}

fn open_snapshot_metadata_database(
    path: &Path,
    key: Option<&SourceKey>,
) -> Option<rusqlite::Connection> {
    if let Ok(connection) = open_source_database(path, None) {
        return Some(connection);
    }
    match key {
        Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) => sqlite3mc::decrypt_raw_wxsqlite3_database(path, bytes)
            .ok()
            .and_then(|plain| open_source_database(&plain, None).ok()),
        Some(key) => open_source_database(path, Some(key)).ok(),
        None => None,
    }
}

fn collect_display_names(connection: &rusqlite::Connection, index: &mut DisplayNameIndex) {
    collect_document_conversations(connection, index);
    collect_participant_profiles(connection, index);
    for (table, id_column, name_column, priority) in [
        ("user_table", "id", "name", 100),
        ("wechat_contactV1", "wxid", "name", 80),
        ("USER", "RID", "name", 90),
    ] {
        collect_name_rows_priority(
            connection,
            table,
            id_column,
            name_column,
            &mut index.participants,
            &mut index.participant_priorities,
            priority,
        );
    }
    collect_conversation_member_rows(
        connection,
        "conversation_user_table",
        "conversation_id",
        "user_id",
        "nick_name",
        &mut index.conversation_members,
    );
    collect_name_rows_priority(
        connection,
        "conversation_table",
        "id",
        "name",
        &mut index.conversations,
        &mut index.conversation_priorities,
        100,
    );
    // O:* 会话是面向当前员工的应用私聊。把应用本身也放进参与者目录，
    // 后续才能稳定显示“应用名-用户名”，而不是短 ID。
    let assistant_ids = index
        .conversations
        .iter()
        .filter(|(id, name)| id.starts_with("O:") && !name.trim().is_empty())
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in assistant_ids {
        let name = index.conversations.get(&id).cloned().unwrap_or_default();
        index.set_participant(&id, name, 105);
        index.participant_kinds.insert(id, "bot".into());
    }

    let Ok(mut statement) = connection.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    ) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return;
    };
    let tables = rows.filter_map(Result::ok).collect::<Vec<_>>();
    for table in tables {
        let normalized_table = normalized_identifier(&table);
        let columns = table_columns(connection, &table);
        let name_column = choose_column(
            &columns,
            &[
                "remark_name",
                "remark",
                "display_name",
                "displayname",
                "nickname",
                "nick_name",
                "real_name",
                "name",
                "alias",
            ],
        );
        let Some(name_column) = name_column else {
            continue;
        };
        if normalized_table.contains("conversation")
            && (normalized_table.contains("user") || normalized_table.contains("member"))
            && let (Some(conversation_column), Some(participant_column)) = (
                choose_column(
                    &columns,
                    &[
                        "conversation_id",
                        "conversationid",
                        "session_id",
                        "sessionid",
                    ],
                ),
                choose_column(
                    &columns,
                    &[
                        "participant_id",
                        "user_id",
                        "userid",
                        "member_id",
                        "account_id",
                        "rid",
                        "vid",
                        "wxid",
                        "username",
                        "id",
                    ],
                ),
            )
        {
            collect_conversation_member_rows(
                connection,
                &table,
                conversation_column,
                participant_column,
                name_column,
                &mut index.conversation_members,
            );
            continue;
        }
        if [
            "participant",
            "contact",
            "member",
            "userinfo",
            "user",
            "friend",
            "buddy",
        ]
        .iter()
        .any(|keyword| normalized_table.contains(keyword))
            && !normalized_table.contains("conversation")
        {
            for id_column in choose_identifier_columns(
                &columns,
                &[
                    "participant_id",
                    "user_id",
                    "userid",
                    "member_id",
                    "account_id",
                    "rid",
                    "vid",
                    "wxid",
                    "openid",
                    "open_id",
                    "username",
                    "id",
                ],
            ) {
                collect_name_rows_priority(
                    connection,
                    &table,
                    id_column,
                    name_column,
                    &mut index.participants,
                    &mut index.participant_priorities,
                    20,
                );
            }
        }
        if !["user", "member"]
            .iter()
            .any(|keyword| normalized_table.contains(keyword))
            && ["conversation", "session", "chat", "room", "group"]
                .iter()
                .any(|keyword| normalized_table.contains(keyword))
            && let Some(id_column) = choose_column(
                &columns,
                &[
                    "conversation_id",
                    "session_id",
                    "chat_id",
                    "room_id",
                    "group_id",
                    "id",
                ],
            )
        {
            collect_name_rows_priority(
                connection,
                &table,
                id_column,
                name_column,
                &mut index.conversations,
                &mut index.conversation_priorities,
                20,
            );
        }
    }
}

fn collect_participant_profiles(connection: &rusqlite::Connection, index: &mut DisplayNameIndex) {
    let fields: [(&str, &[&str]); 11] = [
        ("mobile", &["mobile", "mobile_phone", "mobilephone"]),
        ("phone", &["phone", "telephone", "tel"]),
        (
            "office_phone",
            &[
                "office_phone",
                "officephone",
                "work_phone",
                "workphone",
                "landline",
            ],
        ),
        ("email", &["email", "mail", "email_address"]),
        (
            "employee_no",
            &["employee_no", "employee_number", "staff_no", "number"],
        ),
        (
            "department",
            &["department", "department_name", "dept_name", "dept"],
        ),
        (
            "organization",
            &[
                "external_corp_name",
                "corp_name",
                "company_name",
                "company",
                "enterprise",
            ],
        ),
        (
            "position",
            &["position", "external_job", "job_title", "title", "job"],
        ),
        ("address", &["address", "work_address", "office_address"]),
        ("english_name", &["english_name", "englishname", "en_name"]),
        ("signature", &["signature", "personal_signature", "sign"]),
    ];
    for (table, id_candidates) in [
        ("user_table", &["id"][..]),
        ("wechat_contactV1", &["wxid"][..]),
        ("USER", &["RID"][..]),
    ] {
        let columns = table_columns(connection, table);
        let Some(id_column) = choose_column(&columns, id_candidates) else {
            continue;
        };
        let selected = fields
            .iter()
            .filter_map(|(key, candidates)| {
                let present = choose_identifier_columns(&columns, candidates);
                (!present.is_empty()).then_some((*key, present))
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            continue;
        }
        let mut projection = vec![quote_identifier(id_column)];
        projection.extend(selected.iter().map(|(_, columns)| {
            let alternatives = columns
                .iter()
                .map(|column| {
                    format!(
                        "nullif(trim(CAST({} AS TEXT)), '')",
                        quote_identifier(column)
                    )
                })
                .collect::<Vec<_>>();
            if alternatives.len() == 1 {
                alternatives[0].clone()
            } else {
                format!("coalesce({})", alternatives.join(", "))
            }
        }));
        let sql = format!(
            "SELECT {} FROM {}{} LIMIT 100000",
            projection.join(", "),
            quote_identifier(table),
            choose_column(
                &columns,
                &[
                    "updated_at",
                    "update_time",
                    "modify_time",
                    "last_update_time",
                    "updated"
                ]
            )
            .map(|column| format!(" ORDER BY {} DESC", quote_identifier(column)))
            .unwrap_or_default()
        );
        let Ok(mut statement) = connection.prepare(&sql) else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| {
            let id = sqlite_value_to_string(row.get_ref(0)?);
            let mut profile = serde_json::Map::new();
            for (index, (key, _)) in selected.iter().enumerate() {
                let value = sqlite_value_to_string(row.get_ref(index + 1)?);
                if !value.trim().is_empty() && value.len() <= 2048 {
                    profile.insert((*key).into(), serde_json::Value::String(value));
                }
            }
            Ok((id, profile))
        }) else {
            continue;
        };
        for (id, profile) in rows.filter_map(Result::ok) {
            let id = id.trim();
            if id.is_empty() || profile.is_empty() {
                continue;
            }
            let priority = match table {
                "user_table" => 100,
                "USER" => 90,
                _ => 80,
            };
            for (key, value) in profile {
                if let Some(value) = value.as_str() {
                    index.set_profile(id, &key, value.to_owned(), priority);
                }
            }
        }
    }
    collect_protobuf_participant_profiles(connection, index);
    collect_department_profiles(connection, index);
}

fn collect_protobuf_participant_profiles(
    connection: &rusqlite::Connection,
    index: &mut DisplayNameIndex,
) {
    let Ok(mut statement) = connection.prepare("SELECT id, pb_content FROM user_table") else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_bytes(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, raw) in rows.filter_map(Result::ok) {
        let profile = decode_participant_profile(&id, &raw);
        if profile.is_empty() {
            continue;
        }
        for (key, value) in profile {
            if let Some(value) = value.as_str() {
                index.set_profile(&id, &key, value.to_owned(), 110);
            }
        }
    }
}

fn decode_participant_profile(id: &str, raw: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    let mut profile = serde_json::Map::new();
    let Some(fields) = protobuf_fields(raw) else {
        return profile;
    };
    // Bind the payload to its directory row before projecting any private fields.
    if !fields.iter().any(|(field, value)| matches!((field, value), (1, ProtobufValue::Integer(value)) if value.to_string() == id)) { return profile; }
    for (field, value) in &fields {
        let ProtobufValue::Bytes(bytes) = value else {
            continue;
        };
        if let Some(text) = profile_text(bytes) {
            let key = match field {
                3 | 17 if text.contains('@') && !text.chars().any(char::is_whitespace) => {
                    Some("email")
                }
                5 => Some("mobile"),
                // Some client versions keep the landline/extension in the
                // compact profile envelope instead of the ordinary columns.
                6 if looks_like_phone(&text) => Some("phone"),
                22 => Some("english_name"),
                50 => Some("signature"),
                // The visible position column is empty on newer clients;
                // field 55 is the corresponding UTF-8 directory attribute.
                55 => Some("position"),
                _ => None,
            };
            if let Some(key) = key {
                profile
                    .entry(key)
                    .or_insert(serde_json::Value::String(text));
            }
        }
        // The directory stores configured attributes as labeled records at 33.1.
        // Parse only this observed envelope; do not treat arbitrary protobuf text
        // (login accounts, URLs, internal IDs) as contact information.
        if *field != 33 {
            continue;
        }
        for (entry_field, entry) in protobuf_fields(bytes).unwrap_or_default() {
            let (1, ProtobufValue::Bytes(entry)) = (entry_field, entry) else {
                continue;
            };
            let Some(attributes) = protobuf_fields(entry) else {
                continue;
            };
            let text_at = |wanted| {
                attributes.iter().find_map(|(field, value)| match value {
                    ProtobufValue::Bytes(raw) if *field == wanted => profile_text(raw),
                    _ => None,
                })
            };
            let (Some(label), Some(value)) = (text_at(1), text_at(2)) else {
                continue;
            };
            if label.len() > 128 || label.chars().any(char::is_control) {
                continue;
            }
            let key = match label.as_str() {
                "手机" | "手机号" | "手机号码" => "mobile".into(),
                "座机" | "办公电话" => "office_phone".into(),
                "电话" | "联系电话" => "phone".into(),
                "邮箱" | "电子邮箱" | "企业邮箱" => "email".into(),
                "工号" | "员工编号" | "员工工号" => "employee_no".into(),
                "部门" => "department".into(),
                "企业" | "公司" => "organization".into(),
                "职位" | "职务" => "position".into(),
                "地址" => "address".into(),
                "英文名" => "english_name".into(),
                _ => format!("custom:{label}"),
            };
            profile.insert(key, serde_json::Value::String(value));
        }
    }
    profile
}

fn looks_like_phone(value: &str) -> bool {
    let digits = value
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    (3..=20).contains(&digits)
        && value
            .chars()
            .all(|character| character.is_ascii_digit() || "+-()－— extEXT".contains(character))
}

fn profile_text(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    (!text.is_empty()
        && text.len() <= 2048
        && !text
            .chars()
            .any(|value| value.is_control() && value != '\n' && value != '\t'))
    .then(|| text.to_owned())
}

fn collect_department_profiles(connection: &rusqlite::Connection, index: &mut DisplayNameIndex) {
    let Ok(mut departments) =
        connection.prepare("SELECT id, name, parent_id FROM department_tableV2")
    else {
        return;
    };
    let Ok(rows) = departments.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    let departments = rows
        .filter_map(Result::ok)
        .map(|(id, name, parent)| (id, (name, parent)))
        .collect::<BTreeMap<_, _>>();
    let mut organization_roots = BTreeMap::<String, String>::new();
    for sql in [
        "SELECT u.id, c.root_department_id FROM user_table u JOIN company_group4 c ON c.id = u.corp_id",
        "SELECT u.id, roots.id FROM user_table u JOIN (
           SELECT min(d.id) AS id, d.corpany_id FROM department_tableV2 d
           WHERE NOT EXISTS (SELECT 1 FROM department_tableV2 p WHERE p.id = d.parent_id)
           GROUP BY d.corpany_id HAVING count(*) = 1
         ) roots ON roots.corpany_id = u.corp_id",
    ] {
        if let Ok(mut statement) = connection.prepare(sql)
            && let Ok(rows) = statement.query_map([], |row| {
                Ok((
                    sqlite_value_to_string(row.get_ref(0)?),
                    sqlite_value_to_string(row.get_ref(1)?),
                ))
            })
        {
            for (id, root) in rows.filter_map(Result::ok) {
                if let Some((name, _)) = departments.get(&root)
                    && !name.trim().is_empty()
                {
                    index.set_profile(&id, "organization", name.clone(), 115);
                    organization_roots.entry(id).or_insert(root);
                }
            }
        }
    }
    let Ok(mut statement) = connection.prepare("SELECT user_id, department_id, job FROM user_dept_tableV2 ORDER BY is_main_job DESC, sort, department_id") else { return; };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    let mut paths = BTreeMap::<String, Vec<String>>::new();
    for (id, mut department_id, job) in rows.filter_map(Result::ok) {
        if id.trim().is_empty() {
            continue;
        }
        let mut visited = std::collections::BTreeSet::new();
        let mut names = Vec::new();
        while visited.len() < 32 && visited.insert(department_id.clone()) {
            if organization_roots.get(&id) == Some(&department_id) {
                break;
            }
            let Some((name, parent)) = departments.get(&department_id) else {
                break;
            };
            if !name.trim().is_empty() {
                names.push(name.clone());
            }
            department_id = parent.clone();
        }
        names.reverse();
        let path = names.join("/");
        let user_paths = paths.entry(id.clone()).or_default();
        if !path.is_empty() && !user_paths.contains(&path) {
            user_paths.push(path);
        }
        if !job.trim().is_empty() {
            let has_explicit_position = index
                .profiles
                .get(&id)
                .and_then(|profile| profile.get("position"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if !has_explicit_position {
                index.set_profile(&id, "position", job, 120);
            }
        }
    }
    for (id, paths) in paths {
        if !paths.is_empty() {
            index.set_profile(&id, "department", paths.join("；"), 120);
        }
    }
}

fn collect_document_conversations(connection: &rusqlite::Connection, index: &mut DisplayNameIndex) {
    let Ok(mut statement) =
        connection.prepare("SELECT id, type_content FROM conversation_table WHERE id LIKE 'M:%'")
    else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            sqlite_value_to_bytes(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, content) in rows.filter_map(Result::ok) {
        let Some((title, is_report)) = document_conversation_metadata(&id, &content) else {
            continue;
        };
        index.conversations.insert(id.clone(), title);
        index.conversation_priorities.insert(id.clone(), 110);
        index
            .conversation_types
            .insert(id.clone(), "document".into());
        if let Some(entity_id) = id.strip_prefix("M:") {
            index.set_participant(
                entity_id,
                if is_report {
                    "汇报提醒"
                } else {
                    "文档通知"
                }
                .into(),
                110,
            );
            index
                .participant_kinds
                .insert(entity_id.into(), "bot".into());
        }
    }
}

fn document_conversation_metadata(id: &str, raw: &[u8]) -> Option<(String, bool)> {
    let entity_id = id.strip_prefix("M:")?.parse::<u64>().ok()?;
    let mut offset = 0;
    let mut entity_matches = false;
    let mut title = None;
    let mut document_url = None;
    while offset < raw.len() {
        let tag = read_varint(raw, &mut offset)?;
        if tag >> 3 == 0 {
            return None;
        }
        match tag & 7 {
            0 => {
                let value = read_varint(raw, &mut offset)?;
                if tag >> 3 == 1 {
                    entity_matches = value == entity_id;
                }
            }
            1 => offset = offset.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_varint(raw, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let bytes = raw.get(offset..end)?;
                offset = end;
                // Only the observed title/location fields are needed;
                // other fields may contain document access material.
                match tag >> 3 {
                    4 => document_url = std::str::from_utf8(bytes).ok().map(str::to_owned),
                    5 => title = std::str::from_utf8(bytes).ok().and_then(clean_display_name),
                    _ => {}
                }
            }
            5 => offset = offset.checked_add(4)?,
            _ => return None,
        }
        if offset > raw.len() {
            return None;
        }
    }
    let url = document_url?;
    (entity_matches && url.starts_with("https://doc.weixin.qq.com/"))
        .then(|| (title, url.starts_with("https://doc.weixin.qq.com/journal/")))
        .and_then(|(title, report)| title.map(|title| (title, report)))
}

fn collect_group_announcements(
    connection: &rusqlite::Connection,
    target: &mut Vec<(String, String, i64)>,
) {
    let columns = table_columns(connection, "conversation_table");
    if !columns.iter().any(|column| column == "id")
        || !columns.iter().any(|column| column == "notice_content")
    {
        return;
    }
    let time_expression = if columns.iter().any(|column| column == "notice_time") {
        "notice_time"
    } else if columns.iter().any(|column| column == "modify_time") {
        "modify_time"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT id, notice_content, {time_expression} FROM conversation_table WHERE notice_content IS NOT NULL"
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_bytes(row.get_ref(1)?),
            sqlite_value_to_i64(row.get_ref(2)?).unwrap_or_default(),
        ))
    }) else {
        return;
    };
    for (conversation_id, raw_body, raw_time) in rows.filter_map(Result::ok) {
        let Some(body) = decode_wecom_metadata_body(&raw_body) else {
            continue;
        };
        if conversation_id.trim().is_empty() || is_opaque_identifier(&body) {
            continue;
        }
        target.push((conversation_id, body, normalize_source_timestamp(raw_time)));
    }
}

fn payload_display_name(payload: &serde_json::Value) -> Option<String> {
    ["sender_name", "display_name", "nickname", "nick_name"]
        .into_iter()
        .filter_map(|key| payload.get(key).and_then(serde_json::Value::as_str))
        .find_map(clean_display_name)
}

fn clean_display_name(value: &str) -> Option<String> {
    if value
        .chars()
        .any(|character| character.is_control() || character == '\u{fffd}')
    {
        return None;
    }
    let cleaned = value.trim();
    (!cleaned.is_empty() && cleaned != "-").then(|| cleaned.chars().take(128).collect())
}

fn normalized_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn choose_column<'a>(columns: &'a [String], candidates: &[&str]) -> Option<&'a str> {
    candidates.iter().find_map(|candidate| {
        let expected = normalized_identifier(candidate);
        columns
            .iter()
            .find(|column| normalized_identifier(column) == expected)
            .map(String::as_str)
    })
}

fn table_columns(connection: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let Ok(mut statement) = connection.prepare(&sql) else {
        return vec![];
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
        return vec![];
    };
    rows.filter_map(Result::ok).collect()
}

fn collect_name_rows_priority(
    connection: &rusqlite::Connection,
    table: &str,
    id_column: &str,
    name_column: &str,
    target: &mut BTreeMap<String, String>,
    priorities: &mut BTreeMap<String, u8>,
    priority: u8,
) {
    let sql = format!(
        "SELECT {}, {} FROM {} WHERE {} IS NOT NULL AND {} IS NOT NULL LIMIT 100000",
        quote_identifier(id_column),
        quote_identifier(name_column),
        quote_identifier(table),
        quote_identifier(id_column),
        quote_identifier(name_column),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
        ))
    }) else {
        return;
    };
    for (id, raw_name) in rows.filter_map(Result::ok) {
        let id = id.trim();
        let Some(name) = clean_display_name(&raw_name) else {
            continue;
        };
        if !id.is_empty()
            && id != name
            && priorities
                .get(id)
                .is_none_or(|current| priority >= *current)
        {
            target.insert(id.to_owned(), name);
            priorities.insert(id.to_owned(), priority);
        }
    }
}

fn collect_conversation_member_rows(
    connection: &rusqlite::Connection,
    table: &str,
    conversation_column: &str,
    participant_column: &str,
    name_column: &str,
    target: &mut BTreeMap<String, BTreeMap<String, String>>,
) {
    let sql = format!(
        "SELECT {}, {}, {} FROM {} WHERE {} IS NOT NULL AND {} IS NOT NULL LIMIT 200000",
        quote_identifier(conversation_column),
        quote_identifier(participant_column),
        quote_identifier(name_column),
        quote_identifier(table),
        quote_identifier(conversation_column),
        quote_identifier(participant_column),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return;
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            sqlite_value_to_string(row.get_ref(0)?),
            sqlite_value_to_string(row.get_ref(1)?),
            sqlite_value_to_string(row.get_ref(2)?),
        ))
    }) else {
        return;
    };
    for (conversation_id, participant_id, raw_name) in rows.filter_map(Result::ok) {
        let conversation_id = conversation_id.trim();
        let participant_id = participant_id.trim();
        if conversation_id.is_empty() || participant_id.is_empty() {
            continue;
        }
        let name = clean_display_name(&raw_name).unwrap_or_default();
        let members = target.entry(conversation_id.to_owned()).or_default();
        if name.is_empty() {
            members.entry(participant_id.to_owned()).or_default();
        } else {
            members
                .entry(participant_id.to_owned())
                .and_modify(|current| {
                    if current.is_empty() {
                        *current = name.clone();
                    }
                })
                .or_insert(name);
        }
    }
}

fn choose_identifier_columns<'a>(columns: &'a [String], candidates: &[&str]) -> Vec<&'a str> {
    candidates
        .iter()
        .filter_map(|candidate| {
            let expected = normalized_identifier(candidate);
            columns
                .iter()
                .find(|column| normalized_identifier(column) == expected)
                .map(String::as_str)
        })
        .collect()
}

fn sqlite_value_to_string(value: rusqlite::types::ValueRef<'_>) -> String {
    match value {
        rusqlite::types::ValueRef::Null => String::new(),
        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
        rusqlite::types::ValueRef::Real(value) => value.to_string(),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            String::from_utf8_lossy(value).into_owned()
        }
    }
}

fn sqlite_value_to_bytes(value: rusqlite::types::ValueRef<'_>) -> Vec<u8> {
    match value {
        rusqlite::types::ValueRef::Null => vec![],
        rusqlite::types::ValueRef::Integer(value) => value.to_string().into_bytes(),
        rusqlite::types::ValueRef::Real(value) => value.to_string().into_bytes(),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            value.to_vec()
        }
    }
}

fn decode_wecom_body(raw: &[u8]) -> Option<String> {
    if let Some(segments) = decode_wecom_rich_segments(raw) {
        let text = segments
            .iter()
            .filter_map(|segment| segment.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>();
        return clean_message_text(&text);
    }
    let values = decode_wecom_values(raw);
    (!values.is_empty()).then(|| values.join("\n"))
}

fn decode_wecom_rich_segments(raw: &[u8]) -> Option<Vec<serde_json::Value>> {
    let mut segments = Vec::new();
    for (field, value) in protobuf_fields(raw)? {
        let ProtobufValue::Bytes(fragment) = value else {
            return None;
        };
        if field != 1 {
            return None;
        }
        let mut kind = 0;
        let mut content = None;
        for (field, value) in protobuf_fields(fragment)? {
            match (field, value) {
                (1, ProtobufValue::Integer(value)) => kind = value,
                (2, ProtobufValue::Bytes(value)) => content = Some(value),
                _ => {}
            }
        }
        let fields = protobuf_fields(content?)?;
        let value = fields
            .iter()
            .find_map(|(field, value)| match (field, value) {
                (1, ProtobufValue::Bytes(value)) => Some(*value),
                _ => None,
            })?;
        let text = std::str::from_utf8(value).ok()?;
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return None;
        }
        segments.push(serde_json::json!({
            "kind": if kind == 3 { "emoji" } else { "text" },
            "text": text,
        }));
    }
    (!segments.is_empty()).then_some(segments)
}

fn decode_wecom_metadata_body(raw: &[u8]) -> Option<String> {
    let mut values = decode_wecom_values(raw);
    let mut seen = std::collections::BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
    if values.iter().any(|value| !is_opaque_identifier(value)) {
        values.retain(|value| !is_opaque_identifier(value));
    }
    (!values.is_empty()).then(|| values.into_iter().take(12).collect::<Vec<_>>().join("\n"))
}

fn decode_wecom_values(raw: &[u8]) -> Vec<String> {
    if let Ok(text) = std::str::from_utf8(raw)
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
    {
        return clean_message_text(text).into_iter().collect();
    }
    let mut values = Vec::new();
    let _ = parse_protobuf_text(raw, 0, &mut values);
    values
}

fn is_opaque_identifier(value: &str) -> bool {
    let compact = value.trim();
    !compact.is_empty()
        && (compact.chars().all(|character| character.is_ascii_digit())
            || (compact.len() >= 24
                && compact
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-')))
}

fn readable_message_payload(raw_type: &str, raw: &[u8]) -> serde_json::Value {
    if matches!(raw_type.trim(), "14" | "29")
        && let Some(payload) = wecom_image_payload(raw_type.trim(), raw)
    {
        return payload;
    }
    if raw_type.trim() == "20"
        && let Some(payload) = wecom_file_payload(raw)
    {
        return payload;
    }
    if let Ok(serde_json::Value::Object(mut payload)) =
        serde_json::from_slice::<serde_json::Value>(raw)
    {
        let values = payload
            .values()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        enrich_media_payload(raw_type, &mut payload, &values);
        return serde_json::Value::Object(payload);
    }
    let values = decode_wecom_values(raw);
    let text = if raw_type.trim() == "2" {
        decode_wecom_body(raw)
    } else {
        decode_wecom_metadata_body(raw)
    };
    let mut payload = serde_json::Map::new();
    if let Some(text) = text {
        let key = match raw_type.trim() {
            "15" | "16" | "20" => "name",
            "13" => "title",
            _ => "text",
        };
        payload.insert(key.into(), serde_json::Value::String(text));
    }
    if raw_type.trim() == "2"
        && let Some(segments) = decode_wecom_rich_segments(raw)
    {
        payload.insert("rich_segments".into(), serde_json::Value::Array(segments));
    }
    enrich_media_payload(raw_type, &mut payload, &values);
    serde_json::Value::Object(payload)
}

fn wecom_image_payload(raw_type: &str, raw: &[u8]) -> Option<serde_json::Value> {
    let fields = protobuf_fields(raw)?;
    let mut payload = serde_json::Map::new();
    let mut hints = Vec::new();
    // Project only observed local lookup metadata. Field 1 contains remote
    // access material and must never enter the archive or diagnostics.
    for (field, value) in fields {
        if let ProtobufValue::Bytes(bytes) = value {
            let Some(text) = std::str::from_utf8(bytes).ok() else {
                continue;
            };
            if raw_type == "14" && field == 2 {
                let name = text.replace('\\', "/").rsplit('/').next()?.to_owned();
                if let Some(name) = clean_message_text(&name) {
                    payload.insert("name".into(), name.clone().into());
                    payload.insert("media_path".into(), name.into());
                }
            } else if ((raw_type == "14" && matches!(field, 8 | 10 | 30))
                || (raw_type == "29" && field == 6))
                && text.len() == 32
                && text.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !hints
                    .iter()
                    .any(|hint: &String| hint.eq_ignore_ascii_case(text))
            {
                hints.push(text.to_owned());
            }
        }
    }
    payload.insert("media_lookup_hints".into(), serde_json::json!(hints));
    Some(payload.into())
}

fn local_media_paths(raw_type: &str, content: &[u8], local_extra: &[u8]) -> Vec<PathBuf> {
    if !is_media_message_type(raw_type) {
        return Vec::new();
    }
    let primary_resource = protobuf_fields(content)
        .unwrap_or_default()
        .into_iter()
        .find_map(|(field, value)| {
            if let (1, ProtobufValue::Bytes(bytes)) = (field, value) {
                Some(bytes)
            } else {
                None
            }
        });
    let mut paths = Vec::new();
    for (field, value) in protobuf_fields(local_extra).unwrap_or_default() {
        let (8, ProtobufValue::Bytes(cache)) = (field, value) else {
            continue;
        };
        let mut previous = None;
        for (field, value) in protobuf_fields(cache).unwrap_or_default() {
            let (1, ProtobufValue::Bytes(bytes)) = (field, value) else {
                continue;
            };
            if let Ok(text) = std::str::from_utf8(bytes) {
                let path = PathBuf::from(text);
                let local_disk = matches!(path.components().next(),
                    Some(std::path::Component::Prefix(prefix))
                    if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)));
                if path.is_absolute()
                    && local_disk
                    && !path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    paths.push((
                        previous != primary_resource || primary_resource.is_none(),
                        path,
                    ));
                }
            }
            previous = Some(bytes);
        }
    }
    paths.sort_by_key(|(secondary, _)| *secondary);
    let mut seen = std::collections::BTreeSet::new();
    paths
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn wecom_file_payload(raw: &[u8]) -> Option<serde_json::Value> {
    let mut name = None;
    let mut size = None;
    let mut md5 = None;
    for (field, value) in protobuf_fields(raw)? {
        match (field, value) {
            (2, ProtobufValue::Bytes(bytes)) => {
                name = std::str::from_utf8(bytes).ok().and_then(clean_message_text)
            }
            (4, ProtobufValue::Integer(value)) => size = Some(value),
            (10, ProtobufValue::Bytes(bytes)) => {
                md5 = std::str::from_utf8(bytes)
                    .ok()
                    .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_owned)
            }
            _ => {}
        }
    }
    let name = name?;
    let mut payload = serde_json::json!({ "name": name, "media_path": name });
    if let Some(size) = size {
        payload["size"] = size.into();
    }
    if let Some(md5) = md5 {
        payload["media_md5"] = md5.into();
    }
    Some(payload)
}

fn enrich_media_payload(
    raw_type: &str,
    payload: &mut serde_json::Map<String, serde_json::Value>,
    values: &[String],
) {
    if is_media_message_type(raw_type)
        && !["media_path", "file_path", "local_path", "path"]
            .iter()
            .any(|key| {
                payload
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some()
            })
        && let Some(locator) = media_locator_candidate(raw_type, values)
    {
        let file_name = locator
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        payload.insert("media_path".into(), serde_json::Value::String(locator));
        if let Some(file_name) = file_name {
            payload.insert("name".into(), serde_json::Value::String(file_name));
        }
    }
    if raw_type.trim() == "49"
        && ["media_path", "file_path", "local_path", "path"]
            .iter()
            .any(|key| {
                payload
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .is_some()
            })
    {
        payload
            .entry("kind")
            .or_insert_with(|| serde_json::Value::String("file".into()));
    }
}

fn is_media_message_type(raw_type: &str) -> bool {
    matches!(
        raw_type.trim().to_ascii_lowercase().as_str(),
        "3" | "4"
            | "14"
            | "29"
            | "image"
            | "img"
            | "15"
            | "16"
            | "20"
            | "49"
            | "49:file"
            | "file"
            | "attachment"
    )
}

fn media_locator_candidate(raw_type: &str, values: &[String]) -> Option<String> {
    let allow_plain_name = matches!(
        raw_type.trim().to_ascii_lowercase().as_str(),
        "15" | "16" | "20" | "49" | "49:file" | "file" | "attachment"
    );
    let mut candidates = values
        .iter()
        .flat_map(|value| {
            std::iter::once(value.as_str()).chain(value.split(|character: char| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | '<' | '>' | '=' | ';' | ',')
            }))
        })
        .filter_map(|value| {
            let candidate = value.trim().trim_matches(|character: char| {
                matches!(character, '"' | '\'' | '[' | ']' | '(' | ')')
            });
            let score = media_locator_score(candidate, allow_plain_name);
            (score > 0).then(|| (score, candidate.to_owned()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    candidates.into_iter().next().map(|(_, value)| value)
}

fn media_locator_score(value: &str, allow_plain_name: bool) -> u8 {
    let value = value.trim();
    if value.is_empty() || value.len() > 1024 {
        return 0;
    }
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or_default();
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    let known_extension = extension.as_deref().is_some_and(|extension| {
        matches!(
            extension,
            "jpg"
                | "jpeg"
                | "png"
                | "gif"
                | "webp"
                | "bmp"
                | "heic"
                | "pdf"
                | "doc"
                | "docx"
                | "xls"
                | "xlsx"
                | "ppt"
                | "pptx"
                | "txt"
                | "csv"
                | "zip"
                | "rar"
                | "7z"
                | "mp3"
                | "wav"
                | "mp4"
                | "mov"
                | "dat"
                | "exe"
                | "msi"
                | "msix"
                | "bat"
                | "cmd"
                | "ps1"
        )
    });
    if normalized.contains('/') && known_extension {
        6
    } else if known_extension {
        5
    } else if normalized.contains('/') {
        4
    } else if matches!(value.len(), 32 | 40 | 64)
        && value.chars().all(|character| character.is_ascii_hexdigit())
    {
        3
    } else if allow_plain_name
        && value.len() <= 260
        && !value
            .chars()
            .any(|character| matches!(character, '\n' | '\r' | '\t'))
    {
        1
    } else {
        0
    }
}

fn ensure_media_reference(message: &mut archive_domain::MessageV1) {
    if !matches!(
        message.message_type,
        MessageType::Image | MessageType::File | MessageType::Audio | MessageType::Video
    ) || !message.media.is_empty()
    {
        return;
    }
    message.media.push(archive_domain::MediaRefV1 {
        content_hash: None,
        original_name: message
            .raw_payload
            .get("name")
            .or_else(|| message.raw_payload.get("filename"))
            .or_else(|| message.raw_payload.get("file_name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| message.body_text.clone()),
        mime_type: None,
        size_bytes: None,
        source_locator: message.source_message_id.clone(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("media_locator_not_present_in_message_payload".into()),
    });
}

enum ProtobufValue<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn protobuf_fields(raw: &[u8]) -> Option<Vec<(u64, ProtobufValue<'_>)>> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < raw.len() {
        let tag = read_varint(raw, &mut offset)?;
        let field = tag >> 3;
        if field == 0 {
            return None;
        }
        let value = match tag & 7 {
            0 => ProtobufValue::Integer(read_varint(raw, &mut offset)?),
            1 => {
                offset = offset.checked_add(8)?;
                ProtobufValue::Fixed
            }
            2 => {
                let length = usize::try_from(read_varint(raw, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let segment = raw.get(offset..end)?;
                offset = end;
                ProtobufValue::Bytes(segment)
            }
            5 => {
                offset = offset.checked_add(4)?;
                ProtobufValue::Fixed
            }
            _ => return None,
        };
        if offset > raw.len() {
            return None;
        }
        fields.push((field, value));
    }
    (!fields.is_empty()).then_some(fields)
}

fn parse_protobuf_text(raw: &[u8], depth: usize, values: &mut Vec<String>) -> Option<()> {
    if raw.is_empty() || depth > 4 {
        return None;
    }
    for (_, value) in protobuf_fields(raw)? {
        if let ProtobufValue::Bytes(segment) = value {
            let mut nested = Vec::new();
            if parse_protobuf_text(segment, depth + 1, &mut nested).is_some() && !nested.is_empty()
            {
                values.extend(nested);
            } else if let Some(text) = readable_protobuf_segment(segment) {
                values.push(text);
            }
        }
    }
    Some(())
}

fn read_varint(raw: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *raw.get(*offset)?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn readable_protobuf_segment(raw: &[u8]) -> Option<String> {
    if raw.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let text = clean_message_text(text)?;
    Some(text)
}

fn clean_message_text(value: &str) -> Option<String> {
    let cleaned = value
        .chars()
        .filter(|character| {
            *character != '\u{fffd}'
                && (!character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        })
        .collect::<String>();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

fn sqlite_value_to_i64(value: rusqlite::types::ValueRef<'_>) -> Option<i64> {
    match value {
        rusqlite::types::ValueRef::Integer(value) => Some(value),
        rusqlite::types::ValueRef::Real(value) => Some(value as i64),
        rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
            std::str::from_utf8(value).ok()?.parse().ok()
        }
        rusqlite::types::ValueRef::Null => None,
    }
}

fn normalize_source_timestamp(value: i64) -> i64 {
    match value.unsigned_abs() {
        0..=99_999_999_999 => value.saturating_mul(1_000),
        100_000_000_000..=99_999_999_999_999 => value,
        100_000_000_000_000..=99_999_999_999_999_999 => value / 1_000,
        _ => value / 1_000_000,
    }
}

fn source_timestamp_cutoff(cutoff_unix_ms: i64, sample: i64) -> i64 {
    match sample.unsigned_abs() {
        0..=99_999_999_999 => cutoff_unix_ms / 1_000,
        100_000_000_000..=99_999_999_999_999 => cutoff_unix_ms,
        100_000_000_000_000..=99_999_999_999_999_999 => cutoff_unix_ms.saturating_mul(1_000),
        _ => cutoff_unix_ms.saturating_mul(1_000_000),
    }
}

fn resolve_media_metadata_with_local_paths(
    candidate: &SourceCandidate,
    resolver: &MediaResolver,
    message: &mut archive_domain::MessageV1,
    options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
    local_paths: &[PathBuf],
) -> Result<Vec<archive_domain::MediaBlobV1>, CommandError> {
    let mut blobs = Vec::new();
    if message.media.is_empty()
        || (!options.include_files && !options.include_images)
        || matches!(
            message.message_type,
            archive_domain::MessageType::Audio | archive_domain::MessageType::Video
        )
    {
        return Ok(blobs);
    }
    for media in &mut message.media {
        let resolved = local_paths
            .iter()
            .find_map(|path| resolver.authorized_file(path.clone()))
            .or_else(|| resolver.resolve(&candidate.root_path, &media.source_locator))
            .or_else(|| {
                media
                    .original_name
                    .as_deref()
                    .and_then(|name| resolver.resolve(&candidate.root_path, name))
            })
            .or_else(|| {
                message
                    .raw_payload
                    .get("media_md5")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|md5| resolver.resolve(&candidate.root_path, md5))
            })
            .or_else(|| {
                message
                    .raw_payload
                    .get("media_lookup_hints")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .find_map(|hint| resolver.resolve(&candidate.root_path, hint))
            })
            .or_else(|| resolver.resolve(&candidate.root_path, &message.source_message_id));
        match resolved
            .as_deref()
            .and_then(|path| media_store::hash_source(path).ok())
        {
            Some((hash, size)) => {
                let should_embed = match message.message_type {
                    archive_domain::MessageType::Image => options.include_images,
                    archive_domain::MessageType::File => options.include_files,
                    _ => false,
                };
                if should_embed && let Some(source_path) = resolved.as_deref() {
                    if let Some(sink) = media_sink {
                        sink(source_path, &hash, size).map_err(|_| CommandError {
                            code: "MEDIA_STORE_FAILED",
                            message: "媒体文件写入归档失败。".into(),
                            recoverable: true,
                        })?;
                    } else if let Ok(bytes) = fs::read(source_path)
                        && bytes.len() as u64 == size
                    {
                        blobs.push(archive_domain::MediaBlobV1 {
                            content_hash: hash.clone(),
                            original_name: media.original_name.clone(),
                            mime_type: media.mime_type.clone(),
                            size_bytes: size,
                            content_hex: hex::encode(bytes),
                        });
                    }
                }
                media.content_hash = Some(hash.clone());
                media.size_bytes = Some(size);
                media.source_locator = format!("sha256:{hash}");
                media.integrity = MediaIntegrity::Verified;
                media.missing_reason = None;
            }
            None => {
                media.content_hash = None;
                media.source_locator = "unavailable".into();
                media.integrity = MediaIntegrity::Missing;
                media.missing_reason = Some("not_found_in_authorized_media_roots".into());
            }
        }
    }
    blobs.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
    blobs.dedup_by(|left, right| left.content_hash == right.content_hash);
    Ok(blobs)
}

fn cleanup_snapshot(work_root: &Path, snapshot_root: &Path) {
    let canonical_work_root = work_root.canonicalize().ok();
    let canonical_parent = snapshot_root
        .parent()
        .and_then(|parent| parent.canonicalize().ok());
    let safe = canonical_work_root.is_some() && canonical_work_root == canonical_parent;
    if safe {
        let _ = fs::remove_dir_all(snapshot_root);
    }
}

fn unsupported_source_schema() -> CommandError {
    CommandError {
        code: "SOURCE_SCHEMA_UNSUPPORTED",
        message: "数据结构与当前解析器不匹配；未生成不完整导出。".into(),
        recoverable: true,
    }
}

fn notice_evidence_hash(version: &str) -> String {
    hex::encode(Sha256::digest(format!("{version}\n{LOCAL_NOTICE_TEXT}")))
}

fn internal_error() -> CommandError {
    CommandError {
        code: "INTERNAL_ERROR",
        message: "本地操作失败，诊断信息已脱敏。".into(),
        recoverable: false,
    }
}

fn sanitize_error(value: &str) -> String {
    value
        .split_whitespace()
        .map(|part| {
            if part.contains(':') || part.contains('\\') || part.contains('/') {
                "[已脱敏]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run_key_probe() -> i32 {
    let mut input = Zeroizing::new(Vec::new());
    if std::io::stdin()
        .take(64 * 1024)
        .read_to_end(&mut input)
        .is_err()
    {
        return 20;
    }
    let request: source_windows::probe::ProbeRequest = match serde_json::from_slice(&input) {
        Ok(request) => request,
        Err(_) => return 20,
    };
    let response = match source_windows::probe::scan(&request) {
        Ok(response) => response,
        Err(error) => source_windows::probe::ProbeResponse {
            ok: false,
            code: match error {
                source_windows::probe::ProbeError::Authorization => "PROBE_AUTHORIZATION_REQUIRED",
                source_windows::probe::ProbeError::TrustedTargetNotFound => {
                    "PROBE_TRUSTED_TARGET_NOT_FOUND"
                }
                source_windows::probe::ProbeError::Scan => "PROBE_SCAN_FAILED",
            }
            .into(),
            authorization_token: request.authorization_token,
            candidates: Vec::new(),
            processes_scanned: 0,
            regions_scanned: 0,
        },
    };
    let ok = response.ok;
    if serde_json::to_writer(std::io::stdout().lock(), &response).is_err() {
        return 21;
    }
    if ok { 0 } else { 22 }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    diagnostics::init();
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            let collector_config = load_enterprise_collector_config().ok();
            if let Some(config) = collector_config.as_ref() {
                diagnostics::set_level(&config.collector_log_level);
            }
            let hidden_mode_enabled = collector_config
                .as_ref()
                .is_some_and(|config| config.hidden_mode_enabled);
            diagnostics::write(
                "window_setup",
                &format!("config_loaded={};hidden_mode={hidden_mode_enabled}", collector_config.is_some()),
            );
            // The workspace and generated collectors share one Tauri identifier, but
            // serve different pages. WebView2 must not reuse the workspace browser
            // process (or another collector's process) for the collector window.
            let collector_profile = collector_config
                .as_ref()
                .map(|config| hex::encode(Sha256::digest(config.collector_id.as_bytes())))
                .unwrap_or_else(|| "unconfigured".into());
            let webview_data_dir = collector_data_root()
                .map_err(|_| std::io::Error::other("collectorData path unavailable"))?
                .join("webview")
                .join(collector_profile);
            let _collector_window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .data_directory(webview_data_dir)
            .initialization_script(
                r#"window.addEventListener('DOMContentLoaded', () => {
                    window.setTimeout(() => {
                        const root = document.getElementById('root');
                        if (root && !root.hasChildNodes()) {
                            root.textContent = '采集端界面加载失败，请关闭后重新打开采集端。';
                            root.style.cssText = 'padding:24px;font:14px/1.6 sans-serif;color:#9b2c2c';
                        }
                    }, 6000);
                });"#,
            )
            .title("企业微信采集端")
            .inner_size(440.0, 360.0)
            .min_inner_size(420.0, 340.0)
            .resizable(true)
            .center()
            // Create hidden-mode collectors without ever presenting the native
            // window. The page-load callback below keeps the window hidden as
            // well, while tray actions can still show it when applicable.
            .visible(!hidden_mode_enabled)
            .skip_taskbar(hidden_mode_enabled)
            .on_page_load(move |window, payload| {
                diagnostics::verbose(
                    "page_load",
                    &format!("event={:?};hidden_mode={hidden_mode_enabled}", payload.event()),
                );
                if hidden_mode_enabled
                    && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                {
                    diagnostics::write("page_load_hide", "hidden_mode=true");
                    let _ = window.set_skip_taskbar(true);
                    let _ = window.hide();
                }
            })
            .icon(collector_tray_icon())?
            .build()?;
            let app_state = app.state::<AppState>().inner().clone();
            if let Ok(mut tray_app) = app_state.tray_app.lock() {
                *tray_app = Some(app.handle().clone());
            }
            if !hidden_mode_enabled {
                if let Some(config) = collector_config.as_ref() {
                    create_collector_tray(&app.handle(), config)?;
                }
            }
            let tray_app = app.handle().clone();
            let tray_state = app_state.clone();
            std::thread::spawn(move || {
                while !tray_state.scheduler_shutdown.load(Ordering::Acquire) {
                    let tip = tray_state.schedule_status.lock().ok().map(|status| {
                        let last = status
                            .last_success_at
                            .as_deref()
                            .map(format_status_time)
                            .unwrap_or_else(|| "尚未成功".into());
                        let next = status.next_run_at.as_deref().unwrap_or("计算中");
                        let state = if status.running {
                            "上传中"
                        } else if status.last_error.is_some() {
                            "最近失败"
                        } else {
                            "运行正常"
                        };
                        let schedule_tip = load_enterprise_collector_config()
                            .map(|config| describe_collector_schedule(&config.collector_schedule))
                            .unwrap_or_else(|_| "采集计划加载中".into());
                        format!("{}\n状态：{}\n最近成功：{}\n下次执行：{}\n已上传：{} 条", schedule_tip, state, last, next, status.uploaded_count)
                    });
                    if let (Some(tray), Some(tip)) = (tray_app.tray_by_id("collector-tray"), tip) {
                        let _ = tray.set_tooltip(Some(tip));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
            let scheduler_state = app.state::<AppState>().inner().clone();
            std::thread::Builder::new()
                .name("wecom-archive-collector-scheduler".into())
                .spawn(move || collector_scheduler(scheduler_state))
                .map_err(|_| "无法创建采集端后台任务。")?;
            update::acknowledge_startup();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            sync_collector_config,
            get_collector_schedule_status,
            hide_collector_window,
            discover_sources,
            collect_source,
            upload_latest_enterprise,
            export_latest_enterprise,
            open_offline_export_directory,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                diagnostics::write("window_close_requested", "hide_for_tray=true");
                api.prevent_close();
                let _ = window.hide();
            }
            if matches!(event, tauri::WindowEvent::Destroyed)
                && let Some(state) = window.try_state::<AppState>()
            {
                state.scheduler_shutdown.store(true, Ordering::Release);
            }
        })
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}

fn create_collector_tray(
    app: &tauri::AppHandle,
    config: &EnterpriseCollectorConfig,
) -> tauri::Result<()> {
    if app.tray_by_id("collector-tray").is_some() {
        return Ok(());
    }
    let open = MenuItem::with_id(app, "open", "打开采集端", true, None::<&str>)?;
    let export = MenuItem::with_id(app, "export", "离线导出…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出采集端", true, None::<&str>)?;
    let offline_export_enabled = config.offline_export_enabled;
    let menu = if offline_export_enabled {
        MenuBuilder::new(app)
            .items(&[&open, &export, &quit])
            .build()?
    } else {
        MenuBuilder::new(app).items(&[&open, &quit]).build()?
    };
    let schedule_tip = describe_collector_schedule(&config.collector_schedule);
    let mut tray = TrayIconBuilder::with_id("collector-tray")
        .menu(&menu)
        .tooltip(schedule_tip)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => open_collector_window(app, false),
            "export" if offline_export_enabled => open_collector_window(app, true),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                open_collector_window(tray.app_handle(), offline_export_enabled);
            }
        });
    tray = tray.icon(collector_tray_icon());
    tray.build(app)?;
    Ok(())
}

fn apply_collector_visibility(
    app: &tauri::AppHandle,
    config: &EnterpriseCollectorConfig,
) -> tauri::Result<()> {
    if config.hidden_mode_enabled {
        let _ = app.remove_tray_by_id("collector-tray");
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_skip_taskbar(true);
            let _ = window.hide();
        }
    } else {
        create_collector_tray(app, config)?;
    }
    Ok(())
}

fn open_collector_window(app: &tauri::AppHandle, request_export: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_skip_taskbar(false);
        let _ = window.show();
        let _ = window.set_focus();
        if request_export {
            let _ = window.emit("collector:open-offline-export", ());
        }
    }
}

struct CollectorScheduleTimer {
    schedule: CollectionSchedule,
    next_run: Option<chrono::DateTime<Local>>,
    activation_pending: bool,
}

impl CollectorScheduleTimer {
    fn new(schedule: CollectionSchedule, now: chrono::DateTime<Local>) -> Self {
        Self {
            next_run: next_schedule_at(&schedule, now),
            schedule,
            activation_pending: false,
        }
    }

    fn update(&mut self, schedule: &CollectionSchedule, now: chrono::DateTime<Local>) -> bool {
        if self.schedule == *schedule {
            return false;
        }
        if schedule.mode == CollectionScheduleMode::Disabled {
            self.activation_pending = false;
        } else if self.schedule.mode == CollectionScheduleMode::Disabled {
            self.activation_pending = true;
        }
        self.schedule = schedule.clone();
        // Keep the first run pending while another collection holds the lock.
        // Repeated heartbeats must not move it into the next interval.
        self.next_run = if self.activation_pending {
            Some(now)
        } else {
            next_schedule_at(schedule, now)
        };
        true
    }

    fn completed(&mut self, now: chrono::DateTime<Local>) {
        self.activation_pending = false;
        self.next_run = next_schedule_at(&self.schedule, now);
    }
}

fn collector_scheduler(state: AppState) {
    let config = match load_enterprise_collector_config() {
        Ok(config) => config,
        Err(_) => return,
    };
    let mut timer = CollectorScheduleTimer::new(config.collector_schedule.clone(), Local::now());
    set_next_run(&state, timer.next_run);
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let _ = sync_collector_config_with_status(Some(&state.schedule_status), Some(&state));
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        let latest = match load_enterprise_collector_config() {
            Ok(config) => config,
            Err(_) => continue,
        };
        collector_update::run_pending(&state, &latest);
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        if state
            .schedule_status
            .lock()
            .is_ok_and(|status| status.server_authorization_denied)
        {
            set_next_run(&state, None);
            continue;
        }
        if !latest.enabled {
            timer.update(
                &CollectionSchedule {
                    mode: CollectionScheduleMode::Disabled,
                    ..latest.collector_schedule.clone()
                },
                Local::now(),
            );
            set_next_run(&state, None);
            continue;
        }
        if let Some(request_id) = latest.manual_collection_request_id.clone() {
            let should_run = state
                .schedule_status
                .lock()
                .map(|status| {
                    status.last_manual_collection_request_id.as_deref() != Some(request_id.as_str())
                        && status.manual_collection_attempted_id.as_deref()
                            != Some(request_id.as_str())
                })
                .unwrap_or(false);
            if should_run {
                if let Ok(mut status) = state.schedule_status.lock() {
                    status.manual_collection_attempted_id = Some(request_id.clone());
                }
                if let Some(result) = run_collection_and_record(&state, &latest, false) {
                    if result.is_ok()
                        && let Ok(mut status) = state.schedule_status.lock()
                    {
                        status.last_manual_collection_request_id = Some(request_id);
                    } else if let Ok(mut status) = state.schedule_status.lock() {
                        status.manual_collection_attempted_id = None;
                    }
                    timer.update(&latest.collector_schedule, Local::now());
                    timer.completed(Local::now());
                    set_next_run(&state, timer.next_run);
                    continue;
                } else if let Ok(mut status) = state.schedule_status.lock() {
                    status.manual_collection_attempted_id = None;
                }
            }
        }
        if timer.update(&latest.collector_schedule, Local::now()) {
            set_next_run(&state, timer.next_run);
        }
        if !timer.activation_pending
            && timer.schedule.mode == CollectionScheduleMode::Interval
            && let Ok(status) = state.schedule_status.lock()
            && let Some(value) = status.next_run_at.as_deref()
            && let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        {
            timer.next_run = parsed.and_local_timezone(Local).earliest();
        }
        let Some(due_at) = timer.next_run else {
            continue;
        };
        if Local::now() < due_at {
            continue;
        }
        if run_collection_and_record(&state, &latest, timer.activation_pending).is_none() {
            continue;
        }
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        timer.completed(Local::now());
        set_next_run(&state, timer.next_run);
    }
}

fn run_collection_and_record(
    state: &AppState,
    config: &EnterpriseCollectorConfig,
    force_upload: bool,
) -> Option<Result<ScheduledCollectionOutcome, CommandError>> {
    if state
        .collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return None;
    }
    let _guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(state);
    let result = scheduled_collect_and_upload(state, config, force_upload);
    finish_collector_attempt(state, &result);
    if let Ok(outcome) = &result
        && let Ok(mut status) = state.schedule_status.lock()
    {
        status.last_message_count = Some(outcome.summary.message_count);
        status.last_media_count = Some(outcome.summary.media_count);
        if let Some(completed_at) = outcome.completed_at.as_ref() {
            status.last_success_at = Some(completed_at.clone());
        }
    }
    Some(result)
}

fn set_next_run(state: &AppState, next: Option<chrono::DateTime<Local>>) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.next_run_at = next.map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string());
    }
}

fn set_next_run_from_completion(state: &AppState, schedule: &CollectionSchedule) {
    let next = state
        .schedule_status
        .lock()
        .ok()
        .and_then(|status| status.last_success_at.clone())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Local))
        .and_then(|value| next_schedule_at(schedule, value));
    set_next_run(state, next);
}

fn format_status_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

fn collector_tray_icon() -> tauri::image::Image<'static> {
    let mut pixels = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32_u32 {
        for x in 0..32_u32 {
            let edge = x < 3 || x > 28 || y < 3 || y > 28;
            let paper = x >= 8 && x <= 23 && y >= 6 && y <= 26;
            let line = paper && (y == 13 || y == 17 || y == 21) && x >= 11 && x <= 21;
            let (r, g, b, a) = if edge {
                (22, 128, 60, 255)
            } else if line {
                (22, 128, 60, 255)
            } else if paper {
                (255, 255, 255, 255)
            } else {
                (22, 128, 60, 255)
            };
            pixels.extend([r, g, b, a]);
        }
    }
    tauri::image::Image::new_owned(pixels, 32, 32)
}

fn describe_collector_schedule(schedule: &CollectionSchedule) -> String {
    match schedule.mode {
        CollectionScheduleMode::Disabled => "采集计划未启用".into(),
        CollectionScheduleMode::Interval => {
            format!(
                "采集计划：每 {} 分钟自动采集并上传",
                schedule.interval_minutes.max(1)
            )
        }
        CollectionScheduleMode::Daily => {
            format!("采集计划：每天 {} 自动采集并上传", schedule.daily_time)
        }
    }
}

fn next_schedule_at(
    schedule: &CollectionSchedule,
    now: chrono::DateTime<Local>,
) -> Option<chrono::DateTime<Local>> {
    match schedule.mode {
        CollectionScheduleMode::Disabled => None,
        CollectionScheduleMode::Interval => {
            Some(now + TimeDelta::minutes(i64::from(schedule.interval_minutes.max(1))))
        }
        CollectionScheduleMode::Daily => {
            let time = NaiveTime::parse_from_str(&schedule.daily_time, "%H:%M").ok()?;
            let today = now.date_naive().and_time(time);
            let candidate = today.and_local_timezone(Local).earliest()?;
            if candidate > now {
                Some(candidate)
            } else {
                (today + TimeDelta::days(1))
                    .and_local_timezone(Local)
                    .earliest()
            }
        }
    }
}

struct ScheduledCollectionOutcome {
    summary: CollectionSummary,
    completed_at: Option<String>,
}

fn scheduled_collect_and_upload(
    state: &AppState,
    config: &EnterpriseCollectorConfig,
    force_upload: bool,
) -> Result<ScheduledCollectionOutcome, CommandError> {
    ensure_collector_config_fresh(config)?;
    let candidates = discover_local_sources();
    let media_options = MediaCollectionOptions {
        include_files: config.include_files,
        include_images: config.include_images,
    };
    let (candidate, key) = choose_collectable_source(candidates, &mut |source| {
        resolve_collection_key(
            source,
            source.databases.iter().any(|database| database.encrypted),
            &collector_data_root()?,
        )
    })?;
    let root = collector_work_root("scheduled")?;
    let result = (|| {
        let export = collect_candidate_with_options(
            candidate,
            root.join("work"),
            key.as_ref(),
            CollectionAuthorization {
                notice_version: LOCAL_NOTICE_VERSION.into(),
                notice_displayed_at: Utc::now(),
            },
            media_options,
            None,
            None,
        )?;
        let changed = has_upload_changes(state, &export);
        if !changed && !force_upload {
            let summary = CollectionSummary::from(&export);
            *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
            return Ok(ScheduledCollectionOutcome {
                summary,
                completed_at: None,
            });
        }
        let package = create_enterprise_package(&export, config)?;
        let completed_at =
            post_enterprise_package(&config.upload_url, config.upload_token.as_bytes(), &package)?;
        record_upload_success(state, &export);
        let summary = CollectionSummary::from(&export);
        *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
        Ok::<_, CommandError>(ScheduledCollectionOutcome {
            summary,
            completed_at: Some(completed_at),
        })
    })();
    let _ = fs::remove_dir_all(root);
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_schedule_waits_for_config_refresh_without_accessing_sources() {
        let config: EnterpriseCollectorConfig = serde_json::from_value(serde_json::json!({
            "schemaVersion": "enterprise-collector.v1",
            "organizationId": "synthetic", "organizationName": "synthetic",
            "collectionNotice": "synthetic", "keyId": "synthetic",
            "publicKeyHex": "", "signingPublicKeyHex": "", "signatureHex": "",
            "uploadUrl": "http://127.0.0.1:1/", "uploadToken": "synthetic",
            "configExpiresAt": (Utc::now() - TimeDelta::minutes(1)).to_rfc3339(),
        }))
        .unwrap();
        let state = AppState::default();
        assert!(matches!(
            scheduled_collect_and_upload(&state, &config, false),
            Err(CommandError {
                code: "ENTERPRISE_CONFIG_EXPIRED",
                ..
            })
        ));
        assert!(!state.scheduler_shutdown.load(Ordering::Acquire));
        assert!(state.latest_export.lock().unwrap().is_none());
    }

    #[test]
    fn transient_heartbeat_failures_keep_the_collector_alive() {
        let state = AppState::default();
        for (status, body) in [
            (503, br#"{"code":"UNAUTHORIZED"}"#.as_slice()),
            (502, b"upstream unavailable".as_slice()),
            (401, b"proxy authentication".as_slice()),
        ] {
            let error = collector_heartbeat_failure(
                &collector_http::Response {
                    status,
                    body: body.to_vec(),
                },
                Some(&state.schedule_status),
                Some(&state),
            );
            assert!(error.recoverable);
            assert!(!state.scheduler_shutdown.load(Ordering::Acquire));
            assert!(
                !state
                    .schedule_status
                    .lock()
                    .unwrap()
                    .server_authorization_denied
            );
        }
        let error = collector_heartbeat_failure(
            &collector_http::Response {
                status: 401,
                body: br#"{"code":"UNAUTHORIZED"}"#.to_vec(),
            },
            Some(&state.schedule_status),
            Some(&state),
        );
        assert!(!error.recoverable);
        assert!(state.scheduler_shutdown.load(Ordering::Acquire));
        assert!(
            state
                .schedule_status
                .lock()
                .unwrap()
                .server_authorization_denied
        );
    }

    #[test]
    fn collector_schedule_enabling_runs_now_before_waiting_for_the_interval() {
        let now = Local.with_ymd_and_hms(2026, 9, 24, 9, 0, 0).unwrap();
        let disabled = CollectionSchedule {
            mode: CollectionScheduleMode::Disabled,
            interval_minutes: 60,
            daily_time: "02:00".into(),
        };
        let enabled = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            ..disabled.clone()
        };
        let mut timer = CollectorScheduleTimer::new(disabled.clone(), now);
        assert_eq!(timer.next_run, None);
        assert!(timer.update(&enabled, now));
        assert_eq!(timer.next_run, Some(now));
        assert!(timer.activation_pending);

        // A busy collector receives the same settings on the next heartbeat.
        // Its first due time must remain pending rather than skipping one hour.
        let later = now + TimeDelta::seconds(15);
        assert!(!timer.update(&enabled, later));
        assert_eq!(timer.next_run, Some(now));
        assert!(timer.activation_pending);
        let changed = CollectionSchedule {
            interval_minutes: 30,
            ..enabled.clone()
        };
        assert!(timer.update(&changed, later));
        assert!(timer.activation_pending);
        assert_eq!(timer.next_run, Some(later));

        timer.completed(later);
        assert!(!timer.activation_pending);
        assert_eq!(timer.next_run, Some(later + TimeDelta::minutes(30)));
        assert!(!timer.update(&changed, later + TimeDelta::seconds(15)));
        assert_eq!(timer.next_run, Some(later + TimeDelta::minutes(30)));
        assert!(timer.update(&disabled, later));
        assert_eq!(timer.next_run, None);
        assert!(timer.update(&enabled, later));
        assert_eq!(timer.next_run, Some(later));
    }

    #[test]
    fn collector_schedule_daily_enable_runs_now_then_returns_to_the_daily_time() {
        let now = Local.with_ymd_and_hms(2026, 9, 24, 9, 0, 0).unwrap();
        let disabled = CollectionSchedule {
            mode: CollectionScheduleMode::Disabled,
            interval_minutes: 60,
            daily_time: "20:00".into(),
        };
        let enabled = CollectionSchedule {
            mode: CollectionScheduleMode::Daily,
            ..disabled.clone()
        };
        let mut timer = CollectorScheduleTimer::new(disabled, now);
        assert!(timer.update(&enabled, now));
        assert_eq!(timer.next_run, Some(now));
        timer.completed(now + TimeDelta::minutes(1));
        assert_eq!(
            timer.next_run,
            Some(Local.with_ymd_and_hms(2026, 9, 24, 20, 0, 0).unwrap())
        );
        let already_enabled = CollectorScheduleTimer::new(enabled, now);
        assert!(!already_enabled.activation_pending);
        assert_eq!(already_enabled.next_run, timer.next_run);
    }

    #[test]
    fn collector_initial_empty_export_is_uploaded_before_deduplication() {
        let state = AppState::default();
        let export =
            archive_transfer::create_export("test-client", vec![], vec![], vec![]).unwrap();
        assert!(has_upload_changes(&state, &export));
        record_upload_success(&state, &export);
        state.schedule_status.lock().unwrap().last_success_at = Some("2026-09-24T01:00:00Z".into());
        assert!(!has_upload_changes(&state, &export));
    }

    #[test]
    fn collector_attempt_tracks_first_run_failures_and_no_change_retries() {
        let state = AppState::default();
        assert!(
            state
                .schedule_status
                .lock()
                .unwrap()
                .last_attempt_at
                .is_none()
        );

        begin_collector_attempt(&state);
        let first_attempt = {
            let status = state.schedule_status.lock().unwrap();
            assert!(status.running);
            let attempt = status.last_attempt_at.clone().unwrap();
            assert!(chrono::DateTime::parse_from_rfc3339(&attempt).is_ok());
            attempt
        };
        finish_collector_attempt::<()>(&state, &Err(internal_error()));
        {
            let mut status = state.schedule_status.lock().unwrap();
            assert!(!status.running);
            assert!(status.last_error.is_some());
            assert_eq!(
                status.last_attempt_at.as_deref(),
                Some(first_attempt.as_str())
            );
            status.last_success_at = Some("2026-09-24T10:00:00Z".into());
        }

        begin_collector_attempt(&state);
        // A scan without new messages still counts as an execution, while the
        // successful-upload timestamp continues to describe the last upload.
        finish_collector_attempt(&state, &Ok(UploadResult { message_count: 0 }));
        let status = state.schedule_status.lock().unwrap();
        assert!(!status.running);
        assert!(status.last_attempt_at.is_some());
        assert!(status.last_error.is_none());
        assert_eq!(
            status.last_success_at.as_deref(),
            Some("2026-09-24T10:00:00Z")
        );
    }

    #[test]
    fn custom_source_root_rejects_drive_roots_and_normalizes_explorer_paste() {
        assert_eq!(normalize_source_root("   "), None);
        assert_eq!(normalize_source_root("WXWork"), None);
        assert_eq!(normalize_source_root("D:\\"), None);
        assert_eq!(
            normalize_source_root("  \"D:/WXWork/\"  "),
            Some(PathBuf::from(r"D:\WXWork"))
        );
    }

    #[test]
    fn custom_source_root_limits_collection_to_the_explicit_directory() {
        let roots = collection_roots(
            Some("D:/WXWork/"),
            &[PathBuf::from(r"E:\WXWork"), PathBuf::from(r"D:\WXWork")],
            &[
                PathBuf::from(r"C:\Users\mia\Documents\WXWork"),
                PathBuf::from(r"D:\WXWork"),
            ],
        );
        assert_eq!(roots, vec![PathBuf::from(r"D:\WXWork")]);
        assert!(collection_roots(Some(r"D:\"), &roots, &roots).is_empty());
    }

    #[test]
    fn invalid_explicit_roots_return_a_retry_error_before_automatic_discovery() {
        for raw in [r"C:\", "relative", r"\\server\share\WXWork"] {
            assert_eq!(
                collect_local_export(false, false, Some(raw.to_owned())).unwrap_err(),
                "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。"
            );
        }
    }
    use rusqlite::Connection;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("archive-client-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn local_work_uses_server_data_without_creating_collector_data() {
        let directory = TestDirectory::new();
        let data_root = local_data_root(&directory.0, None);
        assert_eq!(data_root, directory.0.join("serverData"));
        assert!(!data_root.exists());
        let work = create_work_root(&data_root, "local").unwrap();
        assert_eq!(work.parent(), Some(data_root.join("work").as_path()));
        assert!(work.is_dir());
        assert!(!directory.0.join("collectorData").exists());
    }

    #[test]
    fn local_work_respects_the_configured_server_data_directory() {
        let directory = TestDirectory::new();
        let configured = directory.0.join("custom-server-data");
        let data_root = local_data_root(&directory.0, Some(configured.clone()));
        let work = create_work_root(&data_root, "local").unwrap();
        assert!(work.starts_with(configured.join("work")));
        assert!(!directory.0.join("serverData").exists());
        assert!(!directory.0.join("collectorData").exists());
    }

    #[cfg(windows)]
    #[test]
    fn local_probe_cleans_up_failed_snapshots_in_server_data() {
        let directory = TestDirectory::new();
        let data_root = local_data_root(&directory.0, None);
        let candidate = SourceCandidate {
            source_id: "synthetic-source".into(),
            display_path: "synthetic-source".into(),
            root_path: directory.0.clone(),
            databases: vec![archive_domain::SourceDatabase {
                kind: "message".into(),
                path: directory.0.join("message.db"),
                wal_path: None,
                shm_path: None,
                encrypted: true,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![],
            client_version: None,
            capability: SourceCapability::ProbeRequired,
        };
        // Missing and truncated files cover snapshot failure and prefix-read failure.
        for truncated in [false, true] {
            if truncated {
                fs::write(&candidate.databases[0].path, b"short").unwrap();
            }
            assert!(validation_page_prefix(&candidate, &data_root).is_none());
            assert_eq!(fs::read_dir(data_root.join("work")).unwrap().count(), 0);
            assert!(!directory.0.join("collectorData").exists());
        }
    }

    #[test]
    fn remembered_source_roots_survive_reload_and_discover_non_default_data() {
        let directory = TestDirectory::new();
        let config = directory.0.join("source-roots.dpapi");
        let roots = (0..4)
            .map(|index| directory.0.join(format!("custom-{index}")))
            .collect::<Vec<_>>();
        for root in &roots {
            fs::create_dir_all(root).unwrap();
            remember_source_root(&config, root).unwrap();
        }
        remember_source_root(&config, &roots[2]).unwrap();
        let reloaded = load_remembered_source_roots(&config);
        assert_eq!(
            reloaded,
            vec![roots[2].clone(), roots[3].clone(), roots[1].clone()]
        );
        for name in ["message.db", "session.db", "user.db"] {
            let mut header = [0_u8; 100];
            header[..16].copy_from_slice(b"SQLite format 3\0");
            fs::write(roots[2].join(name), header).unwrap();
        }
        let candidates = source_windows::discover_roots(&collection_roots(None, &reloaded, &[]));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].root_path, roots[2]);
        fs::remove_dir(&roots[1]).unwrap();
        assert_eq!(
            load_remembered_source_roots(&config),
            vec![roots[2].clone(), roots[3].clone()]
        );
    }

    #[test]
    fn invalid_remembered_source_roots_fall_back_to_detected_roots() {
        let directory = TestDirectory::new();
        let config = directory.0.join("source-roots.dpapi");
        let detected = vec![directory.0.join("automatic")];
        assert!(load_remembered_source_roots(&config).is_empty());
        fs::write(&config, b"damaged DPAPI fixture").unwrap();
        assert_eq!(
            collection_roots(None, &load_remembered_source_roots(&config), &detected),
            detected
        );
        source_windows::dpapi::store_current_user(&config, b"invalid JSON fixture").unwrap();
        assert!(load_remembered_source_roots(&config).is_empty());
        let payload = serde_json::to_vec(&RememberedSourceRoots {
            schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA + 1,
            roots: vec![directory.0.clone()],
        })
        .unwrap();
        source_windows::dpapi::store_current_user(&config, &payload).unwrap();
        assert_eq!(
            collection_roots(None, &load_remembered_source_roots(&config), &detected),
            detected
        );
    }

    #[test]
    fn concurrent_source_root_saves_keep_a_readable_bounded_config() {
        let directory = TestDirectory::new();
        let config = directory.0.join("source-roots.dpapi");
        let handles = (0..4)
            .map(|index| {
                let root = directory.0.join(format!("fixture-{index}"));
                fs::create_dir_all(&root).unwrap();
                let path = config.clone();
                std::thread::spawn(move || remember_source_root(&path, &root))
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(
            load_remembered_source_roots(&config).len(),
            MAX_REMEMBERED_SOURCE_ROOTS
        );
        assert!(!config.with_extension("dpapi.partial").exists());
    }

    #[test]
    fn remembered_roots_revalidate_paths_before_directory_access() {
        let directory = TestDirectory::new();
        let config = directory.0.join("source-roots.dpapi");
        let payload = serde_json::to_vec(&RememberedSourceRoots {
            schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA,
            roots: vec![
                PathBuf::from(r"C:\"),
                PathBuf::from(r"\\server\share\WXWork"),
                PathBuf::from("relative"),
                directory.0.clone(),
            ],
        })
        .unwrap();
        source_windows::dpapi::store_current_user(&config, &payload).unwrap();
        assert_eq!(
            load_remembered_source_roots(&config),
            vec![directory.0.clone()]
        );
    }

    fn authorization() -> CollectionAuthorization {
        CollectionAuthorization {
            notice_version: "test-notice.v1".into(),
            notice_displayed_at: Utc::now(),
        }
    }

    #[test]
    fn decodes_labeled_profile_attributes_without_importing_unrelated_payload_fields() {
        fn text_field(field: u64, text: &[u8], out: &mut Vec<u8>) {
            write_test_varint((field << 3) | 2, out);
            write_test_varint(text.len() as u64, out);
            out.extend_from_slice(text);
        }
        let mut raw = vec![8, 42];
        text_field(3, b"person@example.invalid", &mut raw);
        text_field(5, b"13800000000", &mut raw);
        text_field(6, b"010-12345678", &mut raw);
        text_field(50, "及时交付".as_bytes(), &mut raw);
        text_field(55, "项目经理".as_bytes(), &mut raw);
        text_field(11, b"https://example.invalid/private-avatar", &mut raw);
        let mut attributes = Vec::new();
        for (label, value) in [
            ("工号", "TEST-001"),
            ("座机", "1001"),
            ("办公地点", "测试楼A栋"),
        ] {
            let mut entry = Vec::new();
            text_field(1, label.as_bytes(), &mut entry);
            text_field(2, value.as_bytes(), &mut entry);
            text_field(1, &entry, &mut attributes);
        }
        text_field(33, &attributes, &mut raw);
        let profile = decode_participant_profile("42", &raw);
        assert_eq!(profile["email"], "person@example.invalid");
        assert_eq!(profile["mobile"], "13800000000");
        assert_eq!(profile["phone"], "010-12345678");
        assert_eq!(profile["signature"], "及时交付");
        assert_eq!(profile["position"], "项目经理");
        assert_eq!(profile["employee_no"], "TEST-001");
        assert_eq!(profile["office_phone"], "1001");
        assert_eq!(profile["custom:办公地点"], "测试楼A栋");
        assert!(
            !serde_json::to_string(&profile)
                .unwrap()
                .contains("private-avatar")
        );
        assert!(decode_participant_profile("43", &raw).is_empty());
        assert!(decode_participant_profile("42", &raw[..raw.len() - 1]).is_empty());
    }

    #[test]
    fn collects_configured_profiles_and_resolves_department_hierarchy() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE user_table (id TEXT, name TEXT, mobile TEXT, office_phone TEXT, email TEXT, number TEXT, corp_id TEXT, position TEXT, external_job TEXT, external_corp_name TEXT, company_name TEXT);
             INSERT INTO user_table VALUES ('member-a', '测试成员甲', '13800000000', '010-00000000', 'member@example.invalid', 'TEST-001', 'corp-a', '', '工程师', '', '');
             INSERT INTO user_table VALUES ('member-b', '测试成员乙', '', '', '', '', 'corp-a', '', '', '', '');
             CREATE TABLE company_group4 (id TEXT, root_department_id TEXT);
             INSERT INTO company_group4 VALUES ('corp-a', 'root');
             CREATE TABLE department_tableV2 (id TEXT, name TEXT, parent_id TEXT, corpany_id TEXT);
             INSERT INTO department_tableV2 VALUES ('root', '测试企业', '0', 'corp-a'), ('sales', '销售部', 'root', 'corp-a'), ('team', '业务组', 'sales', 'corp-a');
             CREATE TABLE user_dept_tableV2 (user_id TEXT, department_id TEXT, job TEXT, is_main_job INTEGER, sort INTEGER);
             INSERT INTO user_dept_tableV2 VALUES ('member-a', 'team', '经理', 1, 0), ('member-b', 'missing', '', 1, 0);"
        ).unwrap();
        let mut index = DisplayNameIndex::default();
        collect_display_names(&connection, &mut index);
        let profile = &index.profiles["member-a"];
        assert_eq!(profile["mobile"], "13800000000");
        assert_eq!(profile["office_phone"], "010-00000000");
        assert_eq!(profile["email"], "member@example.invalid");
        assert_eq!(profile["employee_no"], "TEST-001");
        assert_eq!(profile["department"], "销售部/业务组");
        assert_eq!(profile["organization"], "测试企业");
        assert_eq!(profile["position"], "工程师");
        assert!(!index.profiles["member-b"].contains_key("mobile"));
        assert!(!index.profiles["member-b"].contains_key("department"));
        connection
            .execute("DELETE FROM company_group4", [])
            .unwrap();
        let mut fallback = DisplayNameIndex::default();
        collect_display_names(&connection, &mut fallback);
        assert_eq!(fallback.profiles["member-a"]["organization"], "测试企业");
        assert_eq!(fallback.profiles["member-a"]["department"], "销售部/业务组");
        assert_eq!(conversation_type("T:opaque", 9), "temporary");
        assert_eq!(conversation_type("Y:10024", 1), "system");
    }

    #[test]
    fn collected_contact_numbers_survive_profile_projection_and_redaction() {
        let directory = TestDirectory::new();
        let source_root = directory.0.join("source");
        fs::create_dir_all(&source_root).unwrap();
        let message_path = source_root.join("message.db");
        let connection = Connection::open(&message_path).unwrap();
        connection.execute_batch(
            "CREATE TABLE messages (source_message_id TEXT, conversation_id TEXT, sender_id TEXT, sent_at_unix_ms INTEGER, outgoing INTEGER, raw_type TEXT, payload_json TEXT, recalled INTEGER);
             INSERT INTO messages VALUES ('1', 'conversation', '42', 1700000000000, 0, 'text', '{\"text\":\"测试消息\"}', 0);"
        ).unwrap();
        drop(connection);
        let user_path = source_root.join("user.db");
        let connection = Connection::open(&user_path).unwrap();
        connection
            .execute_batch("CREATE TABLE user_table (id TEXT, name TEXT, pb_content BLOB);")
            .unwrap();
        let attribute = test_protobuf_bytes(&[(1, "座机".as_bytes()), (2, b"010-00000000")]);
        let attributes = test_protobuf_bytes(&[(1, &attribute)]);
        let mut raw = vec![8, 42];
        raw.extend(test_protobuf_bytes(&[
            (5, b"13800000000"),
            (6, b"010-12345678"),
            (33, &attributes),
        ]));
        connection
            .execute(
                "INSERT INTO user_table VALUES ('42', '测试成员', ?1)",
                [raw],
            )
            .unwrap();
        drop(connection);
        let candidate = SourceCandidate {
            source_id: "fixture".into(),
            display_path: "fixture".into(),
            root_path: source_root,
            databases: [("message", message_path), ("user", user_path)]
                .into_iter()
                .map(|(kind, path)| archive_domain::SourceDatabase {
                    kind: kind.into(),
                    path,
                    wal_path: None,
                    shm_path: None,
                    encrypted: false,
                    page_size_hint: Some(4096),
                })
                .collect(),
            media_roots: vec![],
            client_version: None,
            capability: SourceCapability::Supported,
        };
        let export = collect_candidate_with_options(
            candidate,
            directory.0.join("work"),
            None,
            authorization(),
            MediaCollectionOptions::default(),
            None,
            None,
        )
        .unwrap();
        let profile = &export.batches[0].messages[0].raw_payload["participant_profiles"]["42"];
        assert_eq!(profile["mobile"], "13800000000");
        assert_eq!(profile["phone"], "010-12345678");
        assert_eq!(profile["office_phone"], "010-00000000");
        let redacted = redact_sensitive_export(&export);
        let profile = &redacted.batches[0].messages[0].raw_payload["participant_profiles"]["42"];
        for key in ["mobile", "phone", "office_phone"] {
            assert_eq!(profile[key], "[敏感数据已脱敏]");
        }
    }

    #[test]
    fn maps_known_windows_contact_and_session_tables() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE user_table (id TEXT, name TEXT);
                 INSERT INTO user_table VALUES ('0000000000000000', '联系人甲');
                 CREATE TABLE wechat_contactV1 (wxid TEXT, name TEXT);
                 INSERT INTO wechat_contactV1 VALUES ('wxid_1', '微信联系人');
                 CREATE TABLE conversation_table (id TEXT, name TEXT);
                 INSERT INTO conversation_table VALUES ('S:1_2', '项目会话');
                 CREATE TABLE conversation_user_table (
                    conversation_id TEXT, user_id TEXT, nick_name TEXT
                 );
                 INSERT INTO conversation_user_table VALUES ('R:1', 'group-user', '群成员昵称');
                 INSERT INTO conversation_user_table VALUES ('R:1', '0000000000000000', '群内别名');",
            )
            .unwrap();

        let mut names = DisplayNameIndex::default();
        collect_display_names(&connection, &mut names);

        assert_eq!(
            names
                .participants
                .get("0000000000000000")
                .map(String::as_str),
            Some("联系人甲")
        );
        assert_eq!(
            names
                .conversation_members
                .get("R:1")
                .and_then(|members| members.get("0000000000000000"))
                .map(String::as_str),
            Some("群内别名")
        );
        assert_eq!(
            names.participants.get("wxid_1").map(String::as_str),
            Some("微信联系人")
        );
        assert!(!names.participants.contains_key("group-user"));
        assert_eq!(
            names
                .conversation_members
                .get("R:1")
                .and_then(|members| members.get("group-user"))
                .map(String::as_str),
            Some("群成员昵称")
        );
        assert_eq!(
            names.conversations.get("S:1_2").map(String::as_str),
            Some("项目会话")
        );
        assert!(!names.conversations.contains_key("R:1"));
        names.set_participant("self", "当前员工".into(), 100);
        assert_eq!(
            direct_conversation_name(
                "S:self_0000000000000000",
                &names.participants,
                &names.conversation_members,
            )
            .as_deref(),
            Some("当前员工、联系人甲")
        );
        assert_eq!(conversation_type("S:self_0000000000000000", 4), "direct");
        assert_eq!(conversation_type("R:group", 2), "group");

        let session = Connection::open_in_memory().unwrap();
        session
            .execute_batch(
                "CREATE TABLE \"USER\" (RID TEXT, name TEXT);
                 INSERT INTO \"USER\" VALUES ('0000000000000000', '会话别名');",
            )
            .unwrap();
        collect_display_names(&session, &mut names);
        assert_eq!(
            names
                .participants
                .get("0000000000000000")
                .map(String::as_str),
            Some("联系人甲")
        );
    }

    #[test]
    fn extracts_text_from_protobuf_and_rejects_binary_noise() {
        let text = "你好，世界";
        let mut encoded = vec![0x0a, text.len() as u8];
        encoded.extend_from_slice(text.as_bytes());
        assert_eq!(decode_wecom_body(&encoded).as_deref(), Some(text));
        assert_eq!(
            decode_wecom_body(&[0x1e, 0x08, 0x00, 0x12, 0x1a, 0x0a, 0x18]),
            None
        );

        let announcement =
            "各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称";
        let mut encoded = vec![0x0a, 16];
        encoded.extend_from_slice(b"1688856080881486");
        encoded.push(0x12);
        write_test_varint(announcement.len() as u64, &mut encoded);
        encoded.extend_from_slice(announcement.as_bytes());
        assert_eq!(
            decode_wecom_metadata_body(&encoded).as_deref(),
            Some(announcement)
        );

        let locator = "2026/09/photo-0123456789abcdef.png";
        let mut image = vec![0x0a];
        write_test_varint(locator.len() as u64, &mut image);
        image.extend_from_slice(locator.as_bytes());
        let payload = readable_message_payload("3", &image);
        assert_eq!(payload["media_path"], locator);
        assert_eq!(payload["name"], "photo-0123456789abcdef.png");

        let file_payload = readable_message_payload("49", br#"{"name":"report.pdf"}"#);
        assert_eq!(file_payload["media_path"], "report.pdf");
        assert_eq!(file_payload["kind"], "file");
        let installer = "Clash.Verge_2.5.8_x64-setup.exe";
        let file_payload = readable_message_payload(
            "49",
            format!("{{\"name\":\"{installer}\",\"id\":\"1234567890\"}}").as_bytes(),
        );
        assert_eq!(file_payload["media_path"], installer);
        assert_eq!(file_payload["name"], installer);
    }

    #[test]
    fn preserves_rich_text_emoji_numbers_spaces_and_repeated_fragments() {
        // Protobuf writers may omit a zero-valued fragment kind.
        let content = test_protobuf_bytes(&[(1, b"99345")]);
        let fragment = test_protobuf_bytes(&[(2, &content)]);
        let raw = test_protobuf_bytes(&[(1, &fragment)]);
        assert_eq!(decode_wecom_body(&raw).as_deref(), Some("99345"));
        assert_eq!(
            readable_message_payload("2", &raw)["rich_segments"][0]["kind"],
            "text"
        );
        fn rich(parts: &[(u8, &str)]) -> Vec<u8> {
            let mut result = Vec::new();
            for (kind, text) in parts {
                let mut content = vec![0x0a];
                write_test_varint(text.len() as u64, &mut content);
                content.extend_from_slice(text.as_bytes());
                let mut fragment = vec![0x08, *kind, 0x12];
                write_test_varint(content.len() as u64, &mut fragment);
                fragment.extend(content);
                result.push(0x0a);
                write_test_varint(fragment.len() as u64, &mut result);
                result.extend(fragment);
            }
            result
        }
        let bytes = rich(&[(3, "[便便]"), (0, "99345")]);
        let payload = readable_message_payload("2", &bytes);
        assert_eq!(payload["text"], "[便便]99345");
        assert_eq!(payload["rich_segments"][0]["kind"], "emoji");
        assert_eq!(payload["rich_segments"][1]["kind"], "text");
        assert_eq!(payload["rich_segments"][1]["text"], "99345");
        let bytes = rich(&[
            (0, "99345"),
            (0, " "),
            (3, "[便便]"),
            (3, "[便便]"),
            (0, "99345"),
        ]);
        assert_eq!(
            decode_wecom_body(&bytes).as_deref(),
            Some("99345 [便便][便便]99345")
        );
        assert_eq!(decode_wecom_body(b"99345").as_deref(), Some("99345"));
        assert_eq!(
            decode_wecom_body(&rich(&[(0, "1234567890123456")])).as_deref(),
            Some("1234567890123456")
        );
        assert_eq!(
            decode_wecom_body(&rich(&[(0, "a"); 20])).as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn extracts_type_20_file_metadata_without_storing_remote_access_material() {
        let name = "synthetic-setup.exe";
        let md5 = "0123456789abcdef0123456789abcdef";
        let mut raw = Vec::new();
        for (field, value) in [
            (1, "remote-access-material-not-for-archive"),
            (2, name),
            (10, md5),
        ] {
            write_test_varint((field << 3) | 2, &mut raw);
            write_test_varint(value.len() as u64, &mut raw);
            raw.extend_from_slice(value.as_bytes());
        }
        raw.push(0x20);
        write_test_varint(37, &mut raw);
        let payload = readable_message_payload("20", &raw);
        assert_eq!(payload["name"], name);
        assert_eq!(payload["media_path"], name);
        assert_eq!(payload["media_md5"], md5);
        assert_eq!(payload["size"], 37);
        assert!(!payload.to_string().contains("remote-access-material"));
        let mut message = normalize(
            RawMessageRow {
                source_instance_id: "fixture".into(),
                source_message_id: "1".into(),
                conversation_id: "conversation".into(),
                sender_id: None,
                sent_at_unix_ms: 1700000000000,
                outgoing: None,
                raw_type: "20".into(),
                payload,
                recalled: false,
            },
            uuid::Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(message.message_type, MessageType::File);
        assert_eq!(message.media[0].original_name.as_deref(), Some(name));
        let directory = TestDirectory::new();
        fs::write(directory.0.join(md5), [1_u8; 37]).unwrap();
        let candidate = SourceCandidate {
            source_id: "fixture".into(),
            display_path: "fixture".into(),
            root_path: directory.0.clone(),
            databases: Vec::new(),
            media_roots: vec![directory.0.clone()],
            client_version: None,
            capability: SourceCapability::Supported,
        };
        let options = MediaCollectionOptions {
            include_files: true,
            include_images: true,
        };
        let resolver = MediaResolver::new(&candidate, options);
        let blobs = resolve_media_metadata_with_local_paths(
            &candidate,
            &resolver,
            &mut message,
            options,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(message.media[0].integrity, MediaIntegrity::Verified);
        assert_eq!(message.media[0].size_bytes, Some(37));
        assert_eq!(blobs[0].original_name.as_deref(), Some(name));
    }

    fn test_protobuf_bytes(fields: &[(u64, &[u8])]) -> Vec<u8> {
        let mut output = Vec::new();
        for (field, bytes) in fields {
            write_test_varint((field << 3) | 2, &mut output);
            write_test_varint(bytes.len() as u64, &mut output);
            output.extend_from_slice(bytes);
        }
        output
    }

    #[test]
    fn native_images_use_message_cache_paths_without_archiving_private_metadata() {
        let directory = TestDirectory::new();
        let account = directory.0.join("private-account");
        let source_root = account.join("Data");
        let cache = account.join("Cache").join("Image");
        let storage = account.join("FileStorage");
        let emotions = account.join("Emotion");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&storage).unwrap();
        fs::create_dir_all(&emotions).unwrap();
        let sticker = emotions.join("downloaded-sticker");
        fs::write(&sticker, b"GIF89a synthetic sticker").unwrap();
        let original = cache.join("downloaded-cache-object");
        let preview = cache.join("cached-preview");
        fs::write(&original, b"synthetic original").unwrap();
        fs::write(&preview, b"synthetic preview").unwrap();
        let remote = b"remote-access-material-not-for-archive";
        let content = test_protobuf_bytes(&[
            (1, remote),
            (2, b"photo.png"),
            (10, b"0123456789abcdef0123456789abcdef"),
        ]);
        // Previews can precede the primary resource in the local envelope.
        let paths = test_protobuf_bytes(&[
            (1, b"preview-remote-material"),
            (1, preview.to_str().unwrap().as_bytes()),
            (1, remote),
            (1, original.to_str().unwrap().as_bytes()),
        ]);
        let extra = test_protobuf_bytes(&[(8, &paths), (22, b"private OCR must not be archived")]);
        assert_eq!(
            local_media_paths("14", &content, &extra),
            vec![original.clone(), preview.clone()]
        );
        let database = source_root.join("message.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE message_table (send_time INTEGER, conversation_id TEXT, sender_id TEXT, content_type INTEGER, content BLOB, local_extra_content BLOB);").unwrap();
        for raw_type in [14, 29] {
            let content = if raw_type == 14 {
                content.clone()
            } else {
                test_protobuf_bytes(&[(1, remote), (6, b"0123456789abcdef0123456789abcdef")])
            };
            let extra = if raw_type == 14 {
                extra.clone()
            } else {
                test_protobuf_bytes(&[(
                    8,
                    &test_protobuf_bytes(&[(1, remote), (1, sticker.to_str().unwrap().as_bytes())]),
                )])
            };
            connection.execute("INSERT INTO message_table VALUES (1700000000, 'conversation', 'sender', ?1, ?2, ?3)", rusqlite::params![raw_type, content, extra]).unwrap();
        }
        drop(connection);
        let candidate = SourceCandidate {
            source_id: "fixture".into(),
            display_path: "fixture".into(),
            root_path: source_root,
            databases: vec![archive_domain::SourceDatabase {
                kind: "message".into(),
                path: database,
                wal_path: None,
                shm_path: None,
                encrypted: false,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![storage, cache, emotions],
            client_version: None,
            capability: SourceCapability::Supported,
        };
        let options = MediaCollectionOptions {
            include_files: true,
            include_images: true,
        };
        let export = collect_candidate_with_options(
            candidate.clone(),
            directory.0.join("work"),
            None,
            authorization(),
            options,
            None,
            None,
        )
        .unwrap();
        assert_eq!(export.message_count, 2);
        assert_eq!(export.missing_media_count, 0);
        assert_eq!(export.media_blobs.len(), 2);
        for expected in [
            b"synthetic original".as_slice(),
            b"GIF89a synthetic sticker".as_slice(),
        ] {
            assert!(
                export
                    .media_blobs
                    .iter()
                    .any(|blob| hex::decode(&blob.content_hex).unwrap() == expected)
            );
        }
        let serialized = serde_json::to_string(&export).unwrap();
        let image = export.batches[0]
            .messages
            .iter()
            .find(|message| message.raw_type == "14")
            .unwrap();
        assert!(!serialized.contains("remote-access-material"));
        assert!(!serialized.contains("private-account"));
        assert!(!serialized.contains("private OCR"));
        // A deleted original still leaves an authorized preview for this message.
        let missing = original.with_file_name("missing-original");
        let resolver = MediaResolver::new(&candidate, options);
        let mut message = image.clone();
        let blobs = resolve_media_metadata_with_local_paths(
            &candidate,
            &resolver,
            &mut message,
            options,
            None,
            &[missing, preview],
        )
        .unwrap();
        assert_eq!(
            hex::decode(&blobs[0].content_hex).unwrap(),
            b"synthetic preview"
        );
        // Existing but unauthorized files must not be accepted as local hints.
        let outside = directory.0.join("outside.png");
        fs::write(&outside, b"outside").unwrap();
        let mut message = image.clone();
        let blobs = resolve_media_metadata_with_local_paths(
            &candidate,
            &resolver,
            &mut message,
            options,
            None,
            &[outside],
        )
        .unwrap();
        assert!(blobs.is_empty());
        assert_eq!(message.media[0].integrity, MediaIntegrity::Missing);
    }

    #[test]
    fn local_media_hints_ignore_urls_network_paths_parent_traversal_and_broken_envelopes() {
        let paths = test_protobuf_bytes(&[
            (1, b"https://invalid.example/photo.png"),
            (1, br"\\server\share\image.png"),
            (1, br"C:\cache\..\private.png"),
            (1, b"relative.png"),
        ]);
        let extra = test_protobuf_bytes(&[(8, &paths)]);
        assert!(local_media_paths("14", &[], &extra).is_empty());
        assert!(local_media_paths("14", &[], &[0x42, 0xff]).is_empty());
        assert!(local_media_paths("text", &[], &extra).is_empty());
    }

    #[test]
    fn media_lookup_prefers_exact_names_rejects_ambiguity_and_deduplicates_overlapping_roots() {
        let directory = TestDirectory::new();
        let first = directory.0.join("first");
        let second = directory.0.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("photo.png"), b"first").unwrap();
        fs::write(first.join("photo.jpg"), b"second").unwrap();
        let mut candidate = SourceCandidate {
            source_id: "fixture".into(),
            display_path: "fixture".into(),
            root_path: directory.0.clone(),
            databases: vec![],
            media_roots: vec![directory.0.clone(), first.clone()],
            client_version: None,
            capability: SourceCapability::Supported,
        };
        let options = MediaCollectionOptions {
            include_files: true,
            include_images: true,
        };
        let resolver = MediaResolver::new(&candidate, options);
        assert_eq!(
            resolver.resolve(&directory.0, "photo.png"),
            Some(first.join("photo.png").canonicalize().unwrap())
        );
        assert!(resolver.resolve(&directory.0, "photo").is_none());
        fs::write(second.join("photo.png"), b"duplicate").unwrap();
        candidate.media_roots = vec![directory.0.clone()];
        let resolver = MediaResolver::new(&candidate, options);
        assert!(resolver.resolve(&directory.0, "photo.png").is_none());
        candidate.media_roots = vec![first.clone(), second.clone()];
        let resolver = MediaResolver::new(&candidate, options);
        assert!(resolver.resolve(&directory.0, "photo.png").is_none());
        assert_eq!(
            resolver.resolve(&directory.0, first.join("photo.png").to_str().unwrap()),
            Some(first.join("photo.png").canonicalize().unwrap())
        );
    }

    #[test]
    fn native_image_hash_hints_resolve_cache_without_remote_material() {
        let directory = TestDirectory::new();
        let hint = "0123456789abcdef0123456789abcdef";
        fs::write(directory.0.join(format!("{hint}.dat")), b"synthetic image").unwrap();
        let raw = test_protobuf_bytes(&[
            (1, b"remote-secret"),
            (2, b"original.png"),
            (8, b"invalid hint"),
            (30, hint.as_bytes()),
        ]);
        let payload = readable_message_payload("14", &raw);
        assert_eq!(payload["media_lookup_hints"], serde_json::json!([hint]));
        assert!(!payload.to_string().contains("remote-secret"));
        let mut message = normalize(
            RawMessageRow {
                source_instance_id: "fixture".into(),
                source_message_id: "1".into(),
                conversation_id: "conversation".into(),
                sender_id: None,
                sent_at_unix_ms: 1700000000000,
                outgoing: None,
                raw_type: "14".into(),
                payload,
                recalled: false,
            },
            uuid::Uuid::new_v4(),
        )
        .unwrap();
        let candidate = SourceCandidate {
            source_id: "fixture".into(),
            display_path: "fixture".into(),
            root_path: directory.0.clone(),
            databases: vec![],
            media_roots: vec![directory.0.clone()],
            client_version: None,
            capability: SourceCapability::Supported,
        };
        let options = MediaCollectionOptions {
            include_files: false,
            include_images: true,
        };
        let resolver = MediaResolver::new(&candidate, options);
        let blobs = resolve_media_metadata_with_local_paths(
            &candidate,
            &resolver,
            &mut message,
            options,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(message.media[0].integrity, MediaIntegrity::Verified);
    }

    fn write_test_varint(mut value: u64, output: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            output.push(if value == 0 { byte } else { byte | 0x80 });
            if value == 0 {
                return;
            }
        }
    }

    #[test]
    fn reads_document_identity_without_treating_notification_entity_as_a_person() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversation_table (id TEXT, name TEXT, type_content BLOB);",
            )
            .unwrap();
        let mut content = vec![0x08];
        write_test_varint(12345, &mut content);
        for (tag, text) in [
            (
                0x22,
                "https://doc.weixin.qq.com/journal/create?docid=synthetic",
            ),
            (0x2a, "测试工作周报"),
        ] {
            content.push(tag);
            write_test_varint(text.len() as u64, &mut content);
            content.extend_from_slice(text.as_bytes());
        }
        connection
            .execute(
                "INSERT INTO conversation_table VALUES (?1,'',?2)",
                rusqlite::params!["M:12345", content],
            )
            .unwrap();
        let mut names = DisplayNameIndex::default();
        collect_display_names(&connection, &mut names);
        assert_eq!(
            names.conversations.get("M:12345").map(String::as_str),
            Some("测试工作周报")
        );
        assert_eq!(
            names.conversation_types.get("M:12345").map(String::as_str),
            Some("document")
        );
        assert_eq!(
            names.participants.get("12345").map(String::as_str),
            Some("汇报提醒")
        );
        assert_eq!(
            names.participant_kinds.get("12345").map(String::as_str),
            Some("bot")
        );
        assert_eq!(conversation_type("M:unclassified", 8), "application");
        assert!(document_conversation_metadata("M:999", &content).is_none());
        assert!(document_conversation_metadata("S:12345", &content).is_none());
        assert!(document_conversation_metadata("M:12345", &content[..content.len() - 1]).is_none());
    }

    #[test]
    fn reads_group_announcement_from_session_metadata() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversation_table (
                    id TEXT,
                    notice_content BLOB,
                    notice_time INTEGER,
                    modify_time INTEGER
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_table VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![
                    "R:group",
                    "各位同事请及时查看坐车注意事项",
                    1_789_430_400_i64
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_table VALUES (?1, ?2, ?3, 0)",
                rusqlite::params!["R:id-only", "1688856080881486", 1_789_430_400_i64],
            )
            .unwrap();

        let mut announcements = Vec::new();
        collect_group_announcements(&connection, &mut announcements);

        assert_eq!(announcements.len(), 1);
        assert_eq!(announcements[0].0, "R:group");
        assert!(announcements[0].1.contains("坐车注意事项"));
    }

    #[test]
    fn redacts_sensitive_message_text_before_enterprise_upload() {
        let redacted = redact_sensitive_text("账号 admin，密码 secret-123");
        assert_eq!(redacted, "账号 admin，密码 [敏感数据已脱敏]");
        assert!(redacted.contains("admin") && !redacted.contains("secret-123"));

        let mut payload = serde_json::json!({
            "password": "secret-456",
            "profile": { "email": "person@example.com" },
            "participant_profiles": {"synthetic-member": {"mobile": "13800000000", "office_phone": "010-00000000", "email": "member@example.invalid", "department": "研发部", "employee_no": "TEST-001", "custom:办公地点": "测试楼A栋"}},
            "message": "普通内容"
        });
        redact_sensitive_json(&mut payload);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("secret-456"));
        assert!(!serialized.contains("person@example.com"));
        assert!(!serialized.contains("13800000000"));
        assert!(!serialized.contains("010-00000000"));
        assert!(!serialized.contains("member@example.invalid"));
        assert!(!serialized.contains("TEST-001"));
        assert!(!serialized.contains("测试楼A栋"));
        assert!(serialized.contains("研发部"));
        assert!(serialized.contains("普通内容"));
    }

    #[test]
    fn converts_incremental_cutoffs_to_source_timestamp_units() {
        let cutoff = 1_700_000_000_500_i64;
        assert_eq!(
            source_timestamp_cutoff(cutoff, 1_700_000_000),
            1_700_000_000
        );
        assert_eq!(source_timestamp_cutoff(cutoff, cutoff), cutoff);
        assert_eq!(
            source_timestamp_cutoff(cutoff, 1_700_000_000_000_000),
            cutoff.saturating_mul(1_000)
        );
    }

    #[test]
    fn synthetic_plain_source_runs_through_snapshot_and_export_contract() {
        let directory = TestDirectory::new();
        let source_root = directory.0.join("source");
        fs::create_dir_all(&source_root).unwrap();
        let message_path = source_root.join("message.db");
        let connection = Connection::open(&message_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    "message-1",
                    "conversation-1",
                    "participant-1",
                    1_700_000_000_000_i64,
                    0_i64,
                    "text",
                    r#"{"text":"你好","local_path":"C:\\private\\account"}"#,
                    0_i64,
                ],
            )
            .unwrap();
        drop(connection);
        let user = Connection::open(source_root.join("user.db")).unwrap();
        user.execute_batch(
            "CREATE TABLE contacts (user_id TEXT PRIMARY KEY, display_name TEXT NOT NULL);
             INSERT INTO contacts VALUES ('participant-1', '测试成员');",
        )
        .unwrap();
        drop(user);
        let session = Connection::open(source_root.join("session.db")).unwrap();
        session
            .execute_batch(
                "CREATE TABLE sessions (
                    conversation_id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL
                 );
                 INSERT INTO sessions VALUES ('conversation-1', '与测试成员的会话');",
            )
            .unwrap();
        drop(session);

        let candidate = WindowsSourceAdapter
            .discover(Some(source_root))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let work_root = directory.0.join("work");
        let export =
            collect_candidate(candidate.clone(), work_root.clone(), None, authorization()).unwrap();

        assert_eq!(export.schema_version, "client-export.v1");
        assert_eq!(export.message_count, 1);
        assert_eq!(export.batches[0].employee_notice.acknowledged_at, None);
        assert_eq!(
            export.batches[0].employee_notice.evidence_hash,
            notice_evidence_hash("test-notice.v1")
        );
        assert_eq!(
            export.batches[0].messages[0].body_text.as_deref(),
            Some("你好")
        );
        assert_eq!(
            export.participants[0].display_name.as_deref(),
            Some("测试成员")
        );
        assert_eq!(
            export.conversations[0].display_name.as_deref(),
            Some("与测试成员的会话")
        );
        let connection = Connection::open(&message_path).unwrap();
        connection
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    "message-2",
                    "conversation-1",
                    "participant-1",
                    1_700_000_001_000_i64,
                    0_i64,
                    "text",
                    r#"{"text":"增量消息"}"#,
                    0_i64,
                ],
            )
            .unwrap();
        drop(connection);
        let incremental = collect_candidate_with_options_since(
            candidate,
            directory.0.join("work-incremental"),
            None,
            authorization(),
            MediaCollectionOptions::default(),
            None,
            None,
            Some(1_700_000_000_500),
        )
        .unwrap();
        assert_eq!(incremental.message_count, 1);
        assert_eq!(
            incremental.batches[0].messages[0].body_text.as_deref(),
            Some("增量消息")
        );
        let serialized = serde_json::to_string(&export).unwrap();
        assert!(!serialized.contains("private"));
        assert_eq!(fs::read_dir(work_root).unwrap().count(), 0);
    }

    #[test]
    fn media_mode_resolves_nested_files_and_text_mode_keeps_media_relations() {
        let directory = TestDirectory::new();
        let source_root = directory.0.join("source");
        let media_root = source_root.join("FileStorage");
        let nested_media = media_root.join("2026").join("09");
        fs::create_dir_all(&nested_media).unwrap();
        let media_bytes = b"synthetic image bytes";
        fs::write(nested_media.join("photo-fixture.png"), media_bytes).unwrap();
        let message_path = source_root.join("message.db");
        let connection = Connection::open(&message_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );
                INSERT INTO messages VALUES (
                    'text-1', 'conversation-1', 'participant-1', 1700000000000, 0,
                    'text', '{\"text\":\"保留的文本\"}', 0
                );
                INSERT INTO messages VALUES (
                    'image-1', 'conversation-1', 'participant-1', 1700000001000, 0,
                    'image', '{\"name\":\"photo-fixture.png\"}', 0
                );",
            )
            .unwrap();
        drop(connection);
        let candidate = SourceCandidate {
            source_id: "source".into(),
            display_path: "source".into(),
            root_path: source_root,
            databases: vec![archive_domain::SourceDatabase {
                kind: "message".into(),
                path: message_path,
                wal_path: None,
                shm_path: None,
                encrypted: false,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![media_root],
            client_version: None,
            capability: SourceCapability::ProbeRequired,
        };

        let with_media = collect_candidate_with_options(
            candidate.clone(),
            directory.0.join("work-media"),
            None,
            authorization(),
            MediaCollectionOptions {
                include_files: true,
                include_images: true,
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(with_media.message_count, 2);
        assert_eq!(with_media.media_count, 1);
        assert_eq!(with_media.media_blobs.len(), 1);
        assert_eq!(
            hex::decode(&with_media.media_blobs[0].content_hex).unwrap(),
            media_bytes
        );

        let streamed_files = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_files = std::sync::Arc::clone(&streamed_files);
        let media_sink: wecom_archive_server::LocalCollectionMediaSink =
            std::sync::Arc::new(move |path, hash, size| {
                captured_files.lock().unwrap().push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    hash.to_owned(),
                    size,
                ));
                Ok(())
            });
        let streamed_media = collect_candidate_with_options(
            candidate.clone(),
            directory.0.join("work-streamed-media"),
            None,
            authorization(),
            MediaCollectionOptions {
                include_files: true,
                include_images: true,
            },
            Some(&media_sink),
            None,
        )
        .unwrap();
        assert_eq!(streamed_media.media_count, 1);
        assert!(streamed_media.media_blobs.is_empty());
        assert_eq!(streamed_media.media_transport, "metadata_only");
        assert_eq!(streamed_files.lock().unwrap().len(), 1);

        let text_only = collect_candidate_with_options(
            candidate,
            directory.0.join("work-text"),
            None,
            authorization(),
            MediaCollectionOptions::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(text_only.message_count, 2);
        assert_eq!(
            text_only.batches[0].messages[0].message_type,
            MessageType::Text
        );
        assert_eq!(
            text_only.batches[0].messages[1].message_type,
            MessageType::Image
        );
        assert_eq!(text_only.batches[0].messages[1].media.len(), 1);
        assert!(text_only.media_blobs.is_empty());
    }

    #[test]
    fn collectable_source_skips_accounts_whose_key_cannot_open_them() {
        let directory = TestDirectory::new();
        let candidate = |name: &str| SourceCandidate {
            source_id: name.into(),
            display_path: format!("…\\{name}"),
            root_path: directory.0.join(name),
            databases: vec![archive_domain::SourceDatabase {
                kind: "message".into(),
                path: directory.0.join(name).join("message.db"),
                wal_path: None,
                shm_path: None,
                encrypted: true,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![],
            client_version: None,
            capability: SourceCapability::ProbeRequired,
        };

        let mut tried: Vec<String> = Vec::new();
        let (chosen, key) = choose_collectable_source(
            vec![candidate("stale-largest"), candidate("logged-in")],
            &mut |source| {
                tried.push(source.source_id.clone());
                if source.source_id == "logged-in" {
                    Ok(Some(SourceKey::passphrase(b"verified-key")))
                } else {
                    Err(CommandError {
                        code: "SOURCE_KEY_NOT_FOUND",
                        message: "自动解析未找到可验证的数据库密钥。".into(),
                        recoverable: true,
                    })
                }
            },
        )
        .expect("the account whose key opens its database should be the one collected");

        assert_eq!(chosen.source_id, "logged-in");
        assert!(key.is_some());
        assert_eq!(
            tried,
            vec!["stale-largest".to_string(), "logged-in".to_string()]
        );
    }

    #[test]
    fn encrypted_source_requires_a_key_before_snapshot() {
        let directory = TestDirectory::new();
        let candidate = SourceCandidate {
            source_id: "source".into(),
            display_path: "…\\source".into(),
            root_path: directory.0.clone(),
            databases: vec![archive_domain::SourceDatabase {
                kind: "message".into(),
                path: directory.0.join("message.db"),
                wal_path: None,
                shm_path: None,
                encrypted: true,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![],
            client_version: None,
            capability: SourceCapability::ProbeRequired,
        };
        let error = collect_candidate(candidate, directory.0.join("work"), None, authorization())
            .unwrap_err();
        assert_eq!(error.code, "SOURCE_KEY_REQUIRED");
        assert!(!directory.0.join("work").exists());
    }

    #[test]
    fn aes128cbc_fixture_opens_with_the_right_key_and_rejects_the_wrong_key() {
        let directory = TestDirectory::new();
        let source_root = directory.0.join("encrypted-source");
        fs::create_dir_all(&source_root).unwrap();
        let message_path = source_root.join("message.db");
        let correct_key = b"fixture passphrase";
        let connection = sqlite3mc::create_encrypted_fixture(&message_path, correct_key).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );
                INSERT INTO messages VALUES (
                    'encrypted-1', 'conversation-1', 'participant-1',
                    1700000000000, 0, 'text', '{\"text\":\"加密夹具\"}', 0
                );",
            )
            .unwrap();
        drop(connection);
        assert_ne!(
            &fs::read(&message_path).unwrap()[..16],
            b"SQLite format 3\0"
        );
        for name in ["session.db", "user.db"] {
            let auxiliary = Connection::open(source_root.join(name)).unwrap();
            auxiliary
                .execute_batch("CREATE TABLE fixture (id INTEGER PRIMARY KEY);")
                .unwrap();
        }
        let candidate = WindowsSourceAdapter
            .discover(Some(source_root))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let wrong = collect_candidate(
            candidate.clone(),
            directory.0.join("wrong-work"),
            Some(&SourceKey::passphrase(b"wrong passphrase")),
            authorization(),
        )
        .unwrap_err();
        assert_eq!(wrong.code, "SOURCE_KEY_INVALID");
        assert_eq!(
            fs::read_dir(directory.0.join("wrong-work"))
                .unwrap()
                .count(),
            0
        );

        let export = collect_candidate(
            candidate,
            directory.0.join("correct-work"),
            Some(&SourceKey::passphrase(correct_key)),
            authorization(),
        )
        .unwrap();
        assert_eq!(export.message_count, 1);
        assert_eq!(
            export.batches[0].messages[0].body_text.as_deref(),
            Some("加密夹具")
        );
    }

    #[test]
    fn raw_wxsqlite3_decryption_merges_committed_wal_pages() {
        let directory = TestDirectory::new();
        let source = directory.0.join("message.db");
        let snapshot = directory.0.join("snapshot");
        fs::create_dir_all(&snapshot).unwrap();
        let key = b"0123456789abcdef";
        let connection = sqlite3mc::create_encrypted_derived_fixture(&source, key).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                 INSERT INTO messages VALUES (1, '旧消息');
                 PRAGMA wal_checkpoint(TRUNCATE);
                 INSERT INTO messages VALUES (2, '最新消息');",
            )
            .unwrap();
        let snapshot_database = snapshot.join("message.db");
        fs::copy(&source, &snapshot_database).unwrap();
        let source_wal = PathBuf::from(format!("{}-wal", source.display()));
        let snapshot_wal = PathBuf::from(format!("{}-wal", snapshot_database.display()));
        assert!(source_wal.is_file(), "fixture did not leave a WAL sidecar");
        fs::copy(source_wal, snapshot_wal).unwrap();

        let plain = sqlite3mc::decrypt_raw_wxsqlite3_database(&snapshot_database, key).unwrap();
        let opened =
            Connection::open_with_flags(plain, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let bodies = opened
            .prepare("SELECT body FROM messages ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(bodies, vec!["旧消息", "最新消息"]);
    }

    #[test]
    fn derived_aes128_fixture_opens_only_as_a_derived_key() {
        let directory = TestDirectory::new();
        let path = directory.0.join("derived.db");
        let derived_key = b"0123456789abcdef";
        let connection = sqlite3mc::create_encrypted_derived_fixture(&path, derived_key).unwrap();
        connection
            .execute_batch("CREATE TABLE fixture (value TEXT); INSERT INTO fixture VALUES ('ok');")
            .unwrap();
        drop(connection);

        let key = SourceKey::derived_aes128_with_config(derived_key, false, 0).unwrap();
        let opened = open_source_database(&path, Some(&key)).unwrap();
        let value = opened
            .query_row("SELECT value FROM fixture", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        assert_eq!(value, "ok");
        assert!(open_source_database(&path, Some(&SourceKey::passphrase(derived_key))).is_err());
        assert!(sqlite3mc::quick_verify_raw_wxsqlite3_key(
            &path,
            derived_key
        ));
        assert!(!sqlite3mc::quick_verify_raw_wxsqlite3_key(
            &path,
            b"xxxxxxxxxxxxxxxx"
        ));
    }

    #[test]
    fn derived_aes256_fixture_opens_with_the_matching_cipher() {
        let directory = TestDirectory::new();
        let path = directory.0.join("derived-aes256.db");
        let derived_key = b"0123456789abcdef0123456789abcdef";
        let connection =
            sqlite3mc::create_encrypted_derived_aes256_fixture(&path, derived_key).unwrap();
        connection
            .execute_batch("CREATE TABLE fixture (value TEXT); INSERT INTO fixture VALUES ('ok');")
            .unwrap();
        drop(connection);

        let key = SourceKey::derived_aes256_with_config(derived_key, false, 0).unwrap();
        let opened = open_source_database(&path, Some(&key)).unwrap();
        let value = opened
            .query_row("SELECT value FROM fixture", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        assert_eq!(value, "ok");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
    fn authorized_real_probe_finds_a_key_without_exposing_it() {
        let sources = WindowsSourceAdapter.discover(None).unwrap();
        assert!(!sources.is_empty(), "no default data source was discovered");
        let directory = TestDirectory::new();
        let keys = run_isolated_key_probe(&sources[0], &directory.0).unwrap();
        let derived_count = keys
            .iter()
            .filter(|key| key.kind != SourceKeyKind::Passphrase)
            .count();
        let passphrase_count = keys.len().saturating_sub(derived_count);
        let direct_match = sources
            .iter()
            .any(|source| keys.iter().any(|key| key_opens_candidate(source, key)));
        assert!(
            direct_match,
            "probe candidates did not validate against any discovered source (derived candidates: {derived_count}, passphrase candidates: {passphrase_count})"
        );
        let (source, key) = sources
            .iter()
            .flat_map(|source| keys.iter().map(move |key| (source, key)))
            .find(|(source, key)| key_opens_candidate(source, key))
            .expect("validated source/key pair disappeared");
        let validation = TestDirectory::new();
        let export = collect_candidate(
            source.clone(),
            validation.0.join("authorized-collection"),
            Some(key),
            authorization(),
        )
        .expect("validated raw key did not complete a collection");
        assert!(
            export.message_count > 0,
            "authorized source contained no messages"
        );
    }

    #[test]
    #[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
    fn authorized_real_collection_includes_group_metadata_without_exposing_content() {
        let export = collect_local_export(false, false, None).unwrap();
        let message_count = export
            .batches
            .iter()
            .map(|batch| batch.messages.len())
            .sum::<usize>();
        let announcement_count = export
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .filter(|message| message.raw_type == "group_announcement")
            .count();
        assert!(message_count > 0, "authorized source contained no messages");
        assert!(
            announcement_count > 0,
            "authorized source contained no readable group announcements"
        );
        eprintln!(
            "authorized collection verified: {message_count} messages, {announcement_count} group announcements"
        );
    }

    #[test]
    #[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
    fn authorized_real_collection_includes_media_without_exposing_content() {
        let streamed_files = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let streamed_count = std::sync::Arc::clone(&streamed_files);
        let media_sink: wecom_archive_server::LocalCollectionMediaSink =
            std::sync::Arc::new(move |_, _, _| {
                streamed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            });
        let export = collect_local_export_internal(
            true,
            false,
            None,
            None,
            std::sync::Arc::new(|_, _, _| {}),
            Some(&media_sink),
        )
        .unwrap();
        assert!(
            export.message_count > 0,
            "authorized source contained no messages"
        );
        assert!(
            export.media_count > 0,
            "authorized source contained no media references"
        );
        assert!(export.media_blobs.is_empty());
        eprintln!(
            "authorized media collection verified: {} media references, {} streamed files, {} missing files",
            export.media_count,
            streamed_files.load(std::sync::atomic::Ordering::Relaxed),
            export.missing_media_count
        );
        let images = export
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .filter(|message| message.message_type == MessageType::Image)
            .flat_map(|message| &message.media)
            .collect::<Vec<_>>();
        eprintln!(
            "authorized image collection verified: {} image references, {} verified images",
            images.len(),
            images
                .iter()
                .filter(|media| media.integrity == MediaIntegrity::Verified)
                .count()
        );
    }

    #[test]
    fn collector_config_without_log_field_uses_signed_off_default() {
        let (public_key, private_key) =
            source_windows::enterprise_crypto::generate_rsa_key_pair().unwrap();
        let expires_at = (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
        let mut artifact = serde_json::json!({
            "schemaVersion": "enterprise-collector.v1",
            "collectorId": "collector-test",
            "displayName": "测试采集端",
            "configRevision": 1,
            "configExpiresAt": expires_at,
            "organizationId": "org-test",
            "organizationName": "测试组织",
            "collectionNotice": "测试告知",
            "keyId": "key-test",
            "publicKeyHex": "aa",
            "signingPublicKeyHex": hex::encode(&public_key),
            "signatureHex": "",
            "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
            "uploadToken": "test-token",
            "includeFiles": false,
            "includeImages": false,
            "dataRedaction": true,
            "offlineExportEnabled": false,
            "hiddenModeEnabled": false,
            "enabled": true,
            "manualCollectionRequestId": null,
            "collectorSchedule": { "mode": "disabled", "intervalMinutes": 60, "dailyTime": "02:00" },
        });
        let config: EnterpriseCollectorConfig = serde_json::from_value(artifact.clone()).unwrap();
        assert_eq!(config.collector_log_level, "off");
        let signed_payload = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": "collector-control.v1",
            "configRevision": 1,
            "expiresAt": expires_at,
            "config": {
                "collectorId": "collector-test",
                "displayName": "测试采集端",
                "enabled": true,
                "configRevision": 1,
                "organizationId": "org-test",
                "organizationName": "测试组织",
                "collectionNotice": "测试告知",
                "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
                "keyId": "key-test",
                "publicKeyHex": "aa",
                "includeMedia": false,
                "dataRedaction": true,
                "offlineExportEnabled": false,
                "hiddenModeEnabled": false,
                "collectorLogLevel": "off",
                "manualCollectionRequestId": null,
                "schedule": { "mode": "disabled", "intervalMinutes": 60, "dailyTime": "02:00" },
            },
        }))
        .unwrap();
        assert_eq!(
            collector_control_payload(&config, &expires_at).unwrap(),
            signed_payload
        );
        let signature =
            source_windows::enterprise_crypto::sign(&private_key, &signed_payload).unwrap();
        artifact["signatureHex"] = hex::encode(signature).into();
        let loaded: EnterpriseCollectorConfig = serde_json::from_value(artifact).unwrap();
        let signature = hex::decode(&loaded.signature_hex).unwrap();
        source_windows::enterprise_crypto::verify(
            &public_key,
            &collector_control_payload(&loaded, &expires_at).unwrap(),
            &signature,
        )
        .unwrap();
    }

    #[test]
    fn collector_control_envelope_rejects_rollback_expiry_and_bad_signature() {
        let (public_key, private_key) =
            source_windows::enterprise_crypto::generate_rsa_key_pair().unwrap();
        let expires_at = (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
        let control_config = serde_json::json!({
            "collectorId": "collector-test",
            "displayName": "测试采集端",
            "enabled": true,
            "configRevision": 3,
            "organizationId": "org-test",
            "organizationName": "测试组织",
            "collectionNotice": "测试告知",
            "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
            "keyId": "key-test",
            "publicKeyHex": "aa",
            "includeMedia": false,
            "dataRedaction": true,
            "offlineExportEnabled": false,
            "schedule": { "mode": "interval", "intervalMinutes": 15, "dailyTime": "02:00" }
        });
        let payload = serde_json::json!({
            "schemaVersion": "collector-control.v1",
            "configRevision": 3,
            "expiresAt": expires_at,
            "config": control_config,
        });
        let signature = source_windows::enterprise_crypto::sign(
            &private_key,
            &serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        let control = serde_json::json!({
            "schemaVersion": "collector-control.v1",
            "configRevision": 3,
            "expiresAt": payload["expiresAt"],
            "config": payload["config"],
            "signatureHex": hex::encode(signature),
        });
        let public_hex = hex::encode(public_key);
        verify_collector_control_envelope(&control, &public_hex, 2).unwrap();
        assert_eq!(
            verify_collector_control_envelope(&control, &public_hex, 4)
                .unwrap_err()
                .code,
            "COLLECTOR_CONFIG_ROLLBACK"
        );

        let mut expired = control.clone();
        expired["expiresAt"] = (Utc::now() - TimeDelta::minutes(1)).to_rfc3339().into();
        assert_eq!(
            verify_collector_control_envelope(&expired, &public_hex, 0)
                .unwrap_err()
                .code,
            "COLLECTOR_CONFIG_EXPIRED"
        );

        let mut invalid = control;
        invalid["signatureHex"] = "00".into();
        assert_eq!(
            verify_collector_control_envelope(&invalid, &public_hex, 0)
                .unwrap_err()
                .code,
            "COLLECTOR_CONFIG_SIGNATURE_INVALID"
        );
    }
}
