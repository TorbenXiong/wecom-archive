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
use tauri::{Manager, State};
use zeroize::{Zeroize, Zeroizing};

mod sqlite3mc;

const LOCAL_NOTICE_VERSION: &str = "client-notice.v1";
const LOCAL_NOTICE_TEXT: &str = "加密传输本机企业微信聊天记录到服务端";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    organization_name: Option<String>,
    collection_notice: Option<String>,
    offline_export_enabled: bool,
    collector_schedule: CollectionSchedule,
}

#[derive(Debug, Clone, Deserialize)]
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
    include_files: bool,
    #[serde(default)]
    include_images: bool,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    offline_export_enabled: bool,
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

fn default_schedule_daily_time() -> String {
    "02:00".into()
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
        let roots = candidate
            .media_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect::<Vec<_>>();
        let mut files_by_key = BTreeMap::new();
        if options.include_files || options.include_images {
            for root in &roots {
                index_media_directory(root, root, 0, &mut files_by_key);
            }
            for paths in files_by_key.values_mut() {
                paths.sort();
                paths.dedup();
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
        for candidate in direct_candidates {
            if let Some(path) = self.authorized_file(candidate) {
                return Some(path);
            }
        }
        media_lookup_keys(locator).into_iter().find_map(|key| {
            self.files_by_key
                .get(&key)
                .and_then(|paths| paths.first())
                .cloned()
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
    if depth > 12 {
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
                index.entry(key).or_default().push(path.clone());
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
    keys.sort();
    keys.dedup();
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfflineExportResult {
    file_name: String,
    directory: String,
    message_count: u64,
}

#[derive(Clone, Default)]
struct AppState {
    discovered_sources: Arc<Mutex<BTreeMap<String, SourceCandidate>>>,
    latest_export: Arc<Mutex<Option<ClientExportV1>>>,
    collection_running: Arc<AtomicBool>,
    schedule_status: Arc<Mutex<CollectorScheduleStatus>>,
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
}

#[tauri::command]
fn bootstrap() -> Result<BootstrapResponse, CommandError> {
    let config = load_enterprise_collector_config()?;
    Ok(BootstrapResponse {
        organization_name: Some(config.organization_name),
        collection_notice: Some(config.collection_notice),
        offline_export_enabled: config.offline_export_enabled,
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
fn discover_sources(state: State<'_, AppState>) -> Result<Vec<SafeSourceCandidate>, CommandError> {
    let candidates = WindowsSourceAdapter
        .discover(None)
        .map_err(|error| CommandError {
            code: "SOURCE_DISCOVERY_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?;
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
    let authorization = CollectionAuthorization {
        notice_version: LOCAL_NOTICE_VERSION.into(),
        notice_displayed_at: Utc::now(),
    };
    let candidate = state
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
    let media_options = MediaCollectionOptions {
        include_files: config.include_files,
        include_images: config.include_images,
    };
    let root =
        std::env::temp_dir().join(format!("wecom-archive-collector-{}", uuid::Uuid::new_v4()));
    let encrypted = candidate
        .databases
        .iter()
        .any(|database| database.encrypted);
    let key = resolve_collection_key(&candidate, encrypted)?;
    let work_root = root.join("work");
    let result = tauri::async_runtime::spawn_blocking(move || {
        collect_candidate_with_options(
            candidate,
            work_root,
            key.as_ref(),
            authorization,
            media_options,
            None,
            None,
        )
    })
    .await
    .map_err(|_| internal_error())?;
    let export = result?;
    let summary = CollectionSummary::from(&export);
    *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
    let _ = fs::remove_dir_all(&root);
    Ok(summary)
}

struct CollectionRunGuard(Arc<AtomicBool>);

impl Drop for CollectionRunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Collects the currently signed-in local WeCom profile for the embedded
/// enterprise server. The snapshot and key-probe files only live under the
/// system temporary directory and are removed before returning.
pub fn collect_local_export(
    include_media: bool,
    data_redaction: bool,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        None,
        std::sync::Arc::new(|_, _, _| {}),
        None,
    )
}

pub fn collect_local_export_with_progress(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: wecom_archive_server::LocalCollectionMediaSink,
) -> Result<ClientExportV1, String> {
    collect_local_export_internal(
        include_media,
        data_redaction,
        since_unix_ms,
        progress,
        Some(&media_sink),
    )
}

fn collect_local_export_internal(
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    progress: wecom_archive_server::LocalCollectionProgressReporter,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
) -> Result<ClientExportV1, String> {
    progress(6, "发现数据源", "正在查找当前登录的企业微信数据。");
    let candidate = WindowsSourceAdapter
        .discover(None)
        .map_err(|_| "未发现可支持的本机企业微信数据，请确认客户端已登录。".to_owned())?
        .into_iter()
        .next()
        .ok_or_else(|| "未发现可支持的本机企业微信数据，请确认客户端已登录。".to_owned())?;
    progress(15, "读取授权", "已发现数据源，正在准备只读访问。");
    let temporary_root =
        std::env::temp_dir().join(format!("wecom-archive-local-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let encrypted = candidate
            .databases
            .iter()
            .any(|database| database.encrypted);
        let key = resolve_collection_key(&candidate, encrypted)?;
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
    result.map_err(|error| error.message)
}

#[tauri::command]
fn upload_latest_enterprise(state: State<'_, AppState>) -> Result<UploadResult, CommandError> {
    let config = load_enterprise_collector_config()?;
    let export = state
        .latest_export
        .lock()
        .map_err(|_| internal_error())?
        .clone()
        .ok_or(CommandError {
            code: "COLLECTION_RESULT_MISSING",
            message: "没有可上传的采集结果，请先完成采集。".into(),
            recoverable: true,
        })?;
    let package = create_enterprise_package(&export, &config)?;
    post_enterprise_package(&config.upload_url, config.upload_token.as_bytes(), &package)?;
    Ok(UploadResult {
        message_count: export.message_count,
    })
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
    let directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(internal_error)?;
    let file_name = format!(
        "WeComArchive-{}-{}.wca",
        Local::now().format("%Y%m%d-%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let target = directory.join(&file_name);
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
    let directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(internal_error)?;
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
) -> Result<(), CommandError> {
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return Err(CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message: "企业上传地址无效。".into(),
            recoverable: true,
        });
    }
    let mut child = Command::new("curl.exe")
        .args([
            "--fail-with-body",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "-X",
            "POST",
            "-H",
            &format!("Authorization: Bearer {}", String::from_utf8_lossy(token)),
            "-H",
            "Content-Type: application/octet-stream",
            "--data-binary",
            "@-",
            endpoint,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|_| CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message: "系统未找到 curl，无法上传企业加密包。".into(),
            recoverable: true,
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(package).map_err(|_| CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message: "企业加密包上传失败。".into(),
            recoverable: true,
        })?;
    }
    let output = child.wait_with_output().map_err(|_| CommandError {
        code: "ENTERPRISE_UPLOAD_FAILED",
        message: "未收到服务端上传结果。".into(),
        recoverable: true,
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let message = if detail.trim().is_empty() {
            "服务端未接受企业加密包。".to_owned()
        } else {
            format!("服务端未接受企业加密包：{}", detail.trim())
        };
        return Err(CommandError {
            code: "ENTERPRISE_UPLOAD_FAILED",
            message,
            recoverable: true,
        });
    }
    Ok(())
}

fn load_enterprise_collector_config() -> Result<EnterpriseCollectorConfig, CommandError> {
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let bytes = archive_transfer::read_enterprise_collector_config(&executable).map_err(|_| {
        CommandError {
            code: "ENTERPRISE_CONFIG_MISSING",
            message: "采集端内未找到企业配置。".into(),
            recoverable: false,
        }
    })?;
    let config: EnterpriseCollectorConfig =
        serde_json::from_slice(&bytes).map_err(|_| CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置无效。".into(),
            recoverable: false,
        })?;
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
    let signing_payload = collector_signing_payload(&config);
    let signature_valid = source_windows::enterprise_crypto::verify(
        &signing_public_key,
        signing_payload.as_bytes(),
        &signature,
    )
    .is_ok()
        || verify_legacy_collector_signature(&config, &signing_public_key, &signature, true)
        || (!config.offline_export_enabled
            && verify_legacy_collector_signature(&config, &signing_public_key, &signature, false));
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

fn collector_signing_payload(config: &EnterpriseCollectorConfig) -> String {
    format!(
        "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
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
) -> Result<Option<SourceKey>, CommandError> {
    if !encrypted {
        return Ok(None);
    }
    let candidates = run_isolated_key_probe(candidate)?;
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
fn run_isolated_key_probe(candidate: &SourceCandidate) -> Result<Vec<SourceKey>, CommandError> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let validation_page_prefix_hex = validation_page_prefix(candidate);
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
fn validation_page_prefix(candidate: &SourceCandidate) -> Option<String> {
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
    let work_root =
        std::env::temp_dir().join(format!("wecom-archive-probe-{}", uuid::Uuid::new_v4()));
    let receipt = WindowsSourceAdapter
        .snapshot(candidate, work_root.clone())
        .ok()?;
    let copied = receipt.copied_files.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
    })?;
    let result = File::open(copied)
        .and_then(|mut file| file.read_exact(&mut page_prefix))
        .ok()
        .map(|_| hex::encode(page_prefix));
    cleanup_snapshot(&work_root, &receipt.snapshot_root);
    result
}

#[cfg(not(windows))]
fn run_isolated_key_probe() -> Result<Vec<SourceKey>, CommandError> {
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
            ensure_media_reference(&mut message, media_options);
            media_blobs.extend(resolve_media_metadata_with_options(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
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
        let query = if source_cutoff.is_some() {
            "SELECT rowid, send_time, conversation_id, sender_id, content_type, content
             FROM message_table WHERE send_time >= ?1 ORDER BY send_time, rowid"
        } else {
            "SELECT rowid, send_time, conversation_id, sender_id, content_type, content
             FROM message_table ORDER BY send_time, rowid"
        };
        let mut statement = connection
            .prepare(query)
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
                Ok((
                    row_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    raw_type,
                    content,
                ))
            })
            .map_err(|_| unsupported_source_schema())?;
        for row in rows {
            let (source_message_id, conversation_id, sender_id, sent_at_unix_ms, raw_type, content) =
                row.map_err(|_| unsupported_source_schema())?;
            if since_unix_ms.is_some_and(|cutoff| sent_at_unix_ms < cutoff) {
                continue;
            }
            let payload = readable_message_payload(&raw_type, &content);
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
            ensure_media_reference(&mut message, media_options);
            media_blobs.extend(resolve_media_metadata_with_options(
                candidate,
                &media_resolver,
                &mut message,
                media_options,
                media_sink,
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
    messages.retain(|message| match message.message_type {
        MessageType::Image => media_options.include_images,
        MessageType::File => media_options.include_files,
        MessageType::Audio | MessageType::Video => false,
        _ => true,
    });
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
            let conversation_type = conversation_type(&conversation_id, participant_ids.len());
            let metadata_name = || {
                display_names
                    .conversations
                    .get(&conversation_id)
                    .or_else(|| display_names.participants.get(&conversation_id))
                    .cloned()
            };
            let display_name = if conversation_type == "direct" {
                direct_conversation_name(
                    &conversation_id,
                    &display_names.participants,
                    &display_names.conversation_members,
                )
                .or_else(metadata_name)
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
            participant_id,
            participant_kind: None,
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
        let Some(body) = decode_wecom_body(&raw_body) else {
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
    let text = decode_wecom_body(raw);
    let mut payload = serde_json::Map::new();
    if let Some(text) = text {
        let key = match raw_type.trim() {
            "15" | "16" => "name",
            "13" => "title",
            _ => "text",
        };
        payload.insert(key.into(), serde_json::Value::String(text));
    }
    enrich_media_payload(raw_type, &mut payload, &values);
    serde_json::Value::Object(payload)
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
            | "49"
            | "49:file"
            | "file"
            | "attachment"
    )
}

fn media_locator_candidate(raw_type: &str, values: &[String]) -> Option<String> {
    let allow_plain_name = matches!(
        raw_type.trim().to_ascii_lowercase().as_str(),
        "15" | "16" | "49" | "49:file" | "file" | "attachment"
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

fn ensure_media_reference(
    message: &mut archive_domain::MessageV1,
    options: MediaCollectionOptions,
) {
    let enabled = match message.message_type {
        MessageType::Image => options.include_images,
        MessageType::File => options.include_files,
        _ => false,
    };
    if !enabled || !message.media.is_empty() {
        return;
    }
    message.media.push(archive_domain::MediaRefV1 {
        content_hash: None,
        original_name: (message.message_type == MessageType::File)
            .then(|| message.body_text.clone())
            .flatten(),
        mime_type: None,
        size_bytes: None,
        source_locator: message.source_message_id.clone(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("media_locator_not_present_in_message_payload".into()),
    });
}

fn parse_protobuf_text(raw: &[u8], depth: usize, values: &mut Vec<String>) -> Option<()> {
    if raw.is_empty() || depth > 4 {
        return None;
    }
    let mut offset = 0;
    let mut field_count = 0;
    while offset < raw.len() {
        let tag = read_varint(raw, &mut offset)?;
        if tag == 0 {
            return None;
        }
        field_count += 1;
        match tag & 7 {
            0 => {
                read_varint(raw, &mut offset)?;
            }
            1 => offset = offset.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_varint(raw, &mut offset)?).ok()?;
                let end = offset.checked_add(length)?;
                let segment = raw.get(offset..end)?;
                offset = end;
                let mut nested = Vec::new();
                if parse_protobuf_text(segment, depth + 1, &mut nested).is_some()
                    && !nested.is_empty()
                {
                    for text in nested {
                        if !values.contains(&text) {
                            values.push(text);
                        }
                    }
                } else if let Some(text) = readable_protobuf_segment(segment)
                    && !values.contains(&text)
                {
                    values.push(text);
                }
            }
            5 => offset = offset.checked_add(4)?,
            _ => return None,
        }
        if offset > raw.len() {
            return None;
        }
    }
    (field_count > 0).then_some(())
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

fn resolve_media_metadata_with_options(
    candidate: &SourceCandidate,
    resolver: &MediaResolver,
    message: &mut archive_domain::MessageV1,
    options: MediaCollectionOptions,
    media_sink: Option<&wecom_archive_server::LocalCollectionMediaSink>,
) -> Result<Vec<archive_domain::MediaBlobV1>, CommandError> {
    let mut blobs = Vec::new();
    if message.media.is_empty() {
        return Ok(blobs);
    }
    for media in &mut message.media {
        let resolved = resolver
            .resolve(&candidate.root_path, &media.source_locator)
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
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("企业微信记录归档")
            .inner_size(440.0, 360.0)
            .min_inner_size(420.0, 340.0)
            .resizable(true)
            .center()
            .build()?;
            let scheduler_state = app.state::<AppState>().inner().clone();
            std::thread::Builder::new()
                .name("wecom-archive-collector-scheduler".into())
                .spawn(move || collector_scheduler(scheduler_state))
                .map_err(|_| "无法创建采集端后台任务。")?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            get_collector_schedule_status,
            hide_collector_window,
            discover_sources,
            collect_source,
            upload_latest_enterprise,
            export_latest_enterprise,
            open_offline_export_directory,
        ])
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}

fn collector_scheduler(state: AppState) {
    let config = match load_enterprise_collector_config() {
        Ok(config) => config,
        Err(_) => return,
    };
    let schedule = config.collector_schedule.clone();
    let mut next_run = next_schedule_at(&schedule, Local::now());
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let Some(due_at) = next_run else {
            return;
        };
        if Local::now() < due_at {
            continue;
        }
        if state
            .collection_running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let guard = CollectionRunGuard(Arc::clone(&state.collection_running));
            if let Ok(mut status) = state.schedule_status.lock() {
                status.running = true;
                status.last_attempt_at = Some(Utc::now().to_rfc3339());
                status.last_error = None;
            }
            let result = scheduled_collect_and_upload(&state, &config);
            if let Ok(mut status) = state.schedule_status.lock() {
                status.running = false;
                match result {
                    Ok(summary) => {
                        status.last_success_at = Some(Utc::now().to_rfc3339());
                        status.last_message_count = Some(summary.message_count);
                        status.last_media_count = Some(summary.media_count);
                    }
                    Err(error) => status.last_error = Some(error.message),
                }
            }
            drop(guard);
        }
        next_run = next_schedule_at(&schedule, Local::now());
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

fn scheduled_collect_and_upload(
    state: &AppState,
    config: &EnterpriseCollectorConfig,
) -> Result<CollectionSummary, CommandError> {
    let candidate = WindowsSourceAdapter
        .discover(None)
        .map_err(|error| CommandError {
            code: "SOURCE_DISCOVERY_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?
        .into_iter()
        .next()
        .ok_or(CommandError {
            code: "SOURCE_NOT_FOUND",
            message: "未发现可支持的本机企业微信数据。".into(),
            recoverable: true,
        })?;
    let media_options = MediaCollectionOptions {
        include_files: config.include_files,
        include_images: config.include_images,
    };
    let root =
        std::env::temp_dir().join(format!("wecom-archive-collector-{}", uuid::Uuid::new_v4()));
    let encrypted = candidate
        .databases
        .iter()
        .any(|database| database.encrypted);
    let key = resolve_collection_key(&candidate, encrypted)?;
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
        let package = create_enterprise_package(&export, config)?;
        post_enterprise_package(&config.upload_url, config.upload_token.as_bytes(), &package)?;
        let summary = CollectionSummary::from(&export);
        *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
        Ok::<_, CommandError>(summary)
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

    fn authorization() -> CollectionAuthorization {
        CollectionAuthorization {
            notice_version: "test-notice.v1".into(),
            notice_displayed_at: Utc::now(),
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
        assert_eq!(decode_wecom_body(&encoded).as_deref(), Some(announcement));

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
            "message": "普通内容"
        });
        redact_sensitive_json(&mut payload);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("secret-456"));
        assert!(!serialized.contains("person@example.com"));
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
    fn media_mode_resolves_nested_files_and_text_mode_excludes_media_messages() {
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
                    'image', '{\"media_path\":\"photo-fixture.png\",\"name\":\"photo-fixture.png\"}', 0
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
        assert_eq!(text_only.message_count, 1);
        assert_eq!(
            text_only.batches[0].messages[0].message_type,
            MessageType::Text
        );
        assert!(text_only.media_blobs.is_empty());
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
        let keys = run_isolated_key_probe(&sources[0]).unwrap();
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
        let export = collect_local_export(false, false).unwrap();
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
    }
}
