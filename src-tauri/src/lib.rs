use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use archive_domain::{
    ArchiveBatchV1, ClientExportV1, CollectionScope, ConversationV1, EmployeeNoticeEvidence,
    ExternalContactConsent, MediaIntegrity, MessageType, ParticipantV1, RetentionDirective,
    SourceAdapter, SourceCandidate, SourceCapability,
};
use chrono::{Local, Utc};
use message_parser::{RawMessageRow, normalize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use source_windows::WindowsSourceAdapter;
use tauri::State;
use zeroize::{Zeroize, Zeroizing};

mod sqlite3mc;

const LOCAL_NOTICE_VERSION: &str = "client-notice.v1";
const LOCAL_NOTICE_TEXT: &str = "仅处理您有权归档的数据。";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    portable_root: String,
    portable_root_writable: bool,
    source_key_saved: bool,
    automatic_refresh: bool,
    runtime_network_enabled: bool,
    implementation_stage: &'static str,
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DirectoryPurpose {
    Source,
    Export,
    PortableRoot,
}

#[derive(Debug)]
struct DirectoryGrant {
    path: PathBuf,
    purpose: DirectoryPurpose,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafeDirectorySelection {
    handle: String,
    display_path: String,
}

#[derive(Debug)]
struct CollectionAuthorization {
    notice_version: String,
    notice_displayed_at: chrono::DateTime<Utc>,
    remember_key: bool,
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

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientExportFormat {
    Json,
    Csv,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientExportResult {
    file_name: String,
    format: &'static str,
    message_count: u64,
}

#[derive(Default)]
struct AppState {
    portable_root_override: Mutex<Option<PathBuf>>,
    discovered_sources: Mutex<BTreeMap<String, SourceCandidate>>,
    directory_grants: Mutex<BTreeMap<String, DirectoryGrant>>,
    latest_export: Mutex<Option<ClientExportV1>>,
    last_export_root: Mutex<Option<PathBuf>>,
}

#[tauri::command]
fn bootstrap(state: State<'_, AppState>) -> Result<BootstrapResponse, CommandError> {
    let portable_root = selected_portable_root(&state)?;
    let writable = verify_portable_root(&portable_root).is_ok();
    Ok(BootstrapResponse {
        source_key_saved: portable_root
            .join("secrets")
            .join("source-keys.dpapi")
            .is_file(),
        portable_root: redacted_path(&portable_root),
        portable_root_writable: writable,
        automatic_refresh: false,
        runtime_network_enabled: false,
        implementation_stage: "windows-archive-mvp",
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
fn pick_directory(
    state: State<'_, AppState>,
    purpose: DirectoryPurpose,
) -> Result<Option<SafeDirectorySelection>, CommandError> {
    let initial_folder = if purpose == DirectoryPurpose::Export {
        let root = selected_portable_root(&state)?.join("exports");
        fs::create_dir_all(&root).ok();
        Some(root)
    } else {
        None
    };
    #[cfg(windows)]
    let selected =
        source_windows::dialog::pick_folder(initial_folder).map_err(|_| CommandError {
            code: "DIRECTORY_PICKER_FAILED",
            message: "无法打开系统目录选择器。".into(),
            recoverable: true,
        })?;
    #[cfg(not(windows))]
    let selected: Option<PathBuf> = None;

    let Some(path) = selected else {
        return Ok(None);
    };
    let path = path.canonicalize().map_err(|_| CommandError {
        code: "DIRECTORY_UNAVAILABLE",
        message: "所选目录不可用。".into(),
        recoverable: true,
    })?;
    let handle = uuid::Uuid::new_v4().to_string();
    state
        .directory_grants
        .lock()
        .map_err(|_| internal_error())?
        .insert(
            handle.clone(),
            DirectoryGrant {
                path: path.clone(),
                purpose,
            },
        );
    Ok(Some(SafeDirectorySelection {
        handle,
        display_path: redacted_path(&path),
    }))
}

#[tauri::command]
fn discover_selected_source(
    state: State<'_, AppState>,
    selection_handle: String,
) -> Result<Vec<SafeSourceCandidate>, CommandError> {
    let root = consume_directory_grant(&state, &selection_handle, DirectoryPurpose::Source)?;
    let candidates = WindowsSourceAdapter
        .discover(Some(root))
        .map_err(|error| CommandError {
            code: "SOURCE_DISCOVERY_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?;
    store_candidates(&state, candidates)
}

#[tauri::command]
fn set_portable_root(
    state: State<'_, AppState>,
    selection_handle: String,
) -> Result<BootstrapResponse, CommandError> {
    let root = consume_directory_grant(&state, &selection_handle, DirectoryPurpose::PortableRoot)?;
    verify_portable_root(&root)?;
    *state
        .portable_root_override
        .lock()
        .map_err(|_| internal_error())? = Some(root);
    bootstrap(state)
}

#[tauri::command]
async fn collect_source(
    state: State<'_, AppState>,
    source_id: String,
) -> Result<CollectionSummary, CommandError> {
    let authorization = CollectionAuthorization {
        notice_version: LOCAL_NOTICE_VERSION.into(),
        notice_displayed_at: Utc::now(),
        remember_key: true,
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
    let root = selected_portable_root(&state)?;
    verify_portable_root(&root)?;
    let encrypted = candidate
        .databases
        .iter()
        .any(|database| database.encrypted);
    let key = resolve_collection_key(&root, &candidate, encrypted)?;
    let remember_key = encrypted && authorization.remember_key && key.is_some();
    let saved_source_id = candidate.source_id.clone();
    let work_root = root.join("work");
    let (result, key) = tauri::async_runtime::spawn_blocking(move || {
        let result = collect_candidate(candidate, work_root, key.as_ref(), authorization);
        (result, key)
    })
    .await
    .map_err(|_| internal_error())?;
    let export = result?;
    if remember_key {
        let key = key.as_ref().ok_or_else(internal_error)?;
        store_saved_key(&root, &saved_source_id, key)?;
    }
    let summary = CollectionSummary {
        export_id: export.export_id.to_string(),
        generated_at: export.generated_at.to_rfc3339(),
        message_count: export.message_count,
        media_count: export.media_count,
        missing_media_count: export.missing_media_count,
        content_sha256_prefix: export.content_sha256.chars().take(12).collect(),
    };
    *state.latest_export.lock().map_err(|_| internal_error())? = Some(export);
    Ok(summary)
}

#[tauri::command]
fn export_latest(
    state: State<'_, AppState>,
    format: ClientExportFormat,
    selection_handle: String,
) -> Result<ClientExportResult, CommandError> {
    let target_root = consume_directory_grant(&state, &selection_handle, DirectoryPurpose::Export)?;
    export_latest_to_root(&state, format, target_root)
}

#[tauri::command]
#[cfg(windows)]
fn open_export_folder(state: State<'_, AppState>) -> Result<(), CommandError> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = state
        .last_export_root
        .lock()
        .map_err(|_| internal_error())?
        .clone()
        .ok_or(CommandError {
            code: "EXPORT_DIRECTORY_UNKNOWN",
            message: "尚未完成导出，暂时没有可打开的导出文件夹。".into(),
            recoverable: true,
        })?;
    if !root.is_dir() {
        return Err(CommandError {
            code: "EXPORT_DIRECTORY_UNAVAILABLE",
            message: "上次导出文件夹已不可用，请重新导出并选择目录。".into(),
            recoverable: true,
        });
    }
    Command::new("explorer.exe")
        .arg(&root)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| CommandError {
            code: "EXPORT_DIRECTORY_OPEN_FAILED",
            message: "无法打开导出文件夹。".into(),
            recoverable: true,
        })?;
    Ok(())
}

#[tauri::command]
#[cfg(not(windows))]
fn open_export_folder(_state: State<'_, AppState>) -> Result<(), CommandError> {
    Err(CommandError {
        code: "PLATFORM_UNSUPPORTED",
        message: "当前平台暂不支持打开导出文件夹。".into(),
        recoverable: false,
    })
}

fn export_latest_to_root(
    state: &State<'_, AppState>,
    format: ClientExportFormat,
    target_root: PathBuf,
) -> Result<ClientExportResult, CommandError> {
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
    verify_export_root(&target_root)?;
    let extension = match format {
        ClientExportFormat::Json => "json",
        ClientExportFormat::Csv => "csv",
    };
    let username = current_username_component();
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let file_name =
        build_client_export_file_name(&username, &timestamp, export.export_id, extension);
    let target = target_root.join(&file_name);
    let result = match format {
        ClientExportFormat::Json => archive_transfer::write_json(&export, &target),
        ClientExportFormat::Csv => archive_transfer::write_csv(&export, &target),
    };
    result.map_err(|_| CommandError {
        code: "CLIENT_EXPORT_FAILED",
        message: "客户端导出失败；未完成文件已清理，也不会覆盖现有文件。".into(),
        recoverable: true,
    })?;
    *state
        .last_export_root
        .lock()
        .map_err(|_| internal_error())? = Some(target_root);
    Ok(ClientExportResult {
        file_name,
        format: extension,
        message_count: export.message_count,
    })
}

fn current_username_component() -> String {
    std::env::var_os("USERNAME")
        .and_then(|value| value.into_string().ok())
        .map(|value| sanitize_filename_component(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "本机用户".into())
}

fn sanitize_filename_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(48)
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches([' ', '.']).to_string();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved
        .iter()
        .any(|name| cleaned.eq_ignore_ascii_case(name))
    {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

fn build_client_export_file_name(
    username: &str,
    timestamp: &str,
    export_id: uuid::Uuid,
    extension: &str,
) -> String {
    format!(
        "{username}-{timestamp}-client-export-{}.{}",
        export_id.simple(),
        extension
    )
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

fn consume_directory_grant(
    state: &State<'_, AppState>,
    handle: &str,
    expected_purpose: DirectoryPurpose,
) -> Result<PathBuf, CommandError> {
    let grant = state
        .directory_grants
        .lock()
        .map_err(|_| internal_error())?
        .remove(handle)
        .ok_or(CommandError {
            code: "DIRECTORY_HANDLE_EXPIRED",
            message: "目录授权已使用或失效，请重新选择。".into(),
            recoverable: true,
        })?;
    if grant.purpose != expected_purpose {
        return Err(CommandError {
            code: "DIRECTORY_HANDLE_PURPOSE_MISMATCH",
            message: "目录授权用途不匹配，请重新选择。".into(),
            recoverable: true,
        });
    }
    Ok(grant.path)
}

fn resolve_collection_key(
    portable_root: &Path,
    candidate: &SourceCandidate,
    encrypted: bool,
) -> Result<Option<SourceKey>, CommandError> {
    if !encrypted {
        return Ok(None);
    }
    if let Ok(saved) = load_saved_key(portable_root, &candidate.source_id)
        && key_opens_candidate(candidate, &saved)
    {
        return Ok(Some(saved));
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

fn saved_key_path(portable_root: &Path) -> PathBuf {
    portable_root.join("secrets").join("source-keys.dpapi")
}

#[cfg(windows)]
fn store_saved_key(
    portable_root: &Path,
    source_id: &str,
    key: &SourceKey,
) -> Result<(), CommandError> {
    let source_bytes = source_id.as_bytes();
    let source_length = u16::try_from(source_bytes.len()).map_err(|_| internal_error())?;
    let mut payload = Zeroizing::new(Vec::with_capacity(
        13 + source_bytes.len() + key.bytes.len(),
    ));
    payload.extend_from_slice(b"WCAK2");
    payload.push(match key.kind {
        SourceKeyKind::Passphrase => 1,
        SourceKeyKind::DerivedAes128 => 2,
        SourceKeyKind::DerivedAes256 => 3,
        SourceKeyKind::RawWxSqlite3Key => 4,
    });
    payload.push(u8::from(key.legacy));
    payload.extend_from_slice(&key.legacy_page_size.to_be_bytes());
    payload.extend_from_slice(&source_length.to_be_bytes());
    payload.extend_from_slice(source_bytes);
    payload.extend_from_slice(&key.bytes);
    source_windows::dpapi::store_current_user(&saved_key_path(portable_root), &payload).map_err(
        |_| CommandError {
            code: "KEY_SAVE_FAILED",
            message: "采集已完成，但无法使用 Windows 当前用户保护保存密钥。".into(),
            recoverable: true,
        },
    )
}

#[cfg(not(windows))]
fn store_saved_key(
    _portable_root: &Path,
    _source_id: &str,
    _key: &SourceKey,
) -> Result<(), CommandError> {
    Err(CommandError {
        code: "KEY_SAVE_UNAVAILABLE",
        message: "当前平台不支持 Windows DPAPI。".into(),
        recoverable: true,
    })
}

#[cfg(windows)]
fn load_saved_key(
    portable_root: &Path,
    expected_source_id: &str,
) -> Result<SourceKey, CommandError> {
    let payload = source_windows::dpapi::load_current_user(&saved_key_path(portable_root))
        .map_err(|_| internal_error())?;
    if payload.len() < 7 {
        return Err(internal_error());
    }
    let (kind, legacy, legacy_page_size, length_offset, source_offset) = match &payload[..5] {
        b"WCAK1" => (SourceKeyKind::Passphrase, false, 0, 5_usize, 7_usize),
        b"WCAK2" if payload.len() >= 13 => {
            let kind = match payload[5] {
                1 => SourceKeyKind::Passphrase,
                2 => SourceKeyKind::DerivedAes128,
                3 => SourceKeyKind::DerivedAes256,
                4 => SourceKeyKind::RawWxSqlite3Key,
                _ => return Err(internal_error()),
            };
            let legacy = match payload[6] {
                0 => false,
                1 => true,
                _ => return Err(internal_error()),
            };
            let legacy_page_size =
                u32::from_be_bytes(payload[7..11].try_into().map_err(|_| internal_error())?);
            (kind, legacy, legacy_page_size, 11_usize, 13_usize)
        }
        _ => return Err(internal_error()),
    };
    let source_length =
        u16::from_be_bytes([payload[length_offset], payload[length_offset + 1]]) as usize;
    let key_offset = source_offset
        .checked_add(source_length)
        .ok_or_else(internal_error)?;
    if key_offset >= payload.len()
        || payload.get(source_offset..key_offset) != Some(expected_source_id.as_bytes())
    {
        return Err(internal_error());
    }
    match kind {
        SourceKeyKind::Passphrase => Ok(SourceKey::passphrase(&payload[key_offset..])),
        SourceKeyKind::DerivedAes128 => {
            SourceKey::derived_aes128_with_config(&payload[key_offset..], legacy, legacy_page_size)
        }
        SourceKeyKind::DerivedAes256 => {
            SourceKey::derived_aes256_with_config(&payload[key_offset..], legacy, legacy_page_size)
        }
        SourceKeyKind::RawWxSqlite3Key => {
            if payload[key_offset..].len() == 16 {
                Ok(SourceKey {
                    kind: SourceKeyKind::RawWxSqlite3Key,
                    bytes: Zeroizing::new(payload[key_offset..].to_vec()),
                    legacy,
                    legacy_page_size,
                })
            } else {
                Err(internal_error())
            }
        }
    }
}

#[cfg(not(windows))]
fn load_saved_key(
    _portable_root: &Path,
    _expected_source_id: &str,
) -> Result<SourceKey, CommandError> {
    Err(internal_error())
}

fn collect_candidate(
    candidate: SourceCandidate,
    work_root: PathBuf,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
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
    let receipt = WindowsSourceAdapter
        .snapshot(&candidate, work_root.clone())
        .map_err(|error| CommandError {
            code: "SOURCE_SNAPSHOT_FAILED",
            message: sanitize_error(&error.to_string()),
            recoverable: true,
        })?;
    let result = read_snapshot_export(&candidate, &receipt, key, authorization);
    cleanup_snapshot(&work_root, &receipt.snapshot_root);
    result
}

fn read_snapshot_export(
    candidate: &SourceCandidate,
    receipt: &archive_domain::SnapshotReceipt,
    key: Option<&SourceKey>,
    authorization: CollectionAuthorization,
) -> Result<ClientExportV1, CommandError> {
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
    let allowed_media_roots = candidate
        .media_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .collect::<Vec<_>>();
    let has_normalized = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='messages')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| unsupported_source_schema())?
        != 0;
    if has_normalized {
        let mut statement = connection
            .prepare(
                "SELECT source_message_id, conversation_id, sender_id, sent_at_unix_ms,
                        outgoing, raw_type, payload_json, recalled
                 FROM messages ORDER BY sent_at_unix_ms, source_message_id",
            )
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map([], |row| {
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
            resolve_media_metadata(candidate, &allowed_media_roots, &mut message);
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
        let mut statement = connection
            .prepare(
                "SELECT rowid, send_time, conversation_id, sender_id, content_type, content
                 FROM message_table ORDER BY send_time, rowid",
            )
            .map_err(|_| unsupported_source_schema())?;
        let rows = statement
            .query_map([], |row| {
                let row_id = sqlite_value_to_string(row.get_ref(0)?);
                let send_time = sqlite_value_to_i64(row.get_ref(1)?).unwrap_or_default();
                let sent_at_unix_ms = normalize_source_timestamp(send_time);
                let conversation_id = sqlite_value_to_string(row.get_ref(2)?);
                let sender_id = match row.get_ref(3)? {
                    rusqlite::types::ValueRef::Null => None,
                    value => Some(sqlite_value_to_string(value)),
                };
                let raw_type = sqlite_value_to_string(row.get_ref(4)?);
                let content = sqlite_value_to_string(row.get_ref(5)?);
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
            let mut message = normalize(
                RawMessageRow {
                    source_instance_id: candidate.source_id.clone(),
                    source_message_id,
                    conversation_id,
                    sender_id,
                    sent_at_unix_ms,
                    outgoing: None,
                    raw_type,
                    payload: serde_json::json!({"text": content}),
                    recalled: false,
                },
                batch_id,
            )
            .map_err(|_| unsupported_source_schema())?;
            resolve_media_metadata(candidate, &allowed_media_roots, &mut message);
            messages.push(message);
        }
    }

    let mut conversation_participants = BTreeMap::<String, Vec<String>>::new();
    let mut participant_ids = std::collections::BTreeSet::new();
    for message in &messages {
        let members = conversation_participants
            .entry(message.conversation_id.clone())
            .or_default();
        if let Some(sender_id) = &message.sender_id {
            if !members.contains(sender_id) {
                members.push(sender_id.clone());
            }
            participant_ids.insert(sender_id.clone());
        }
    }
    let conversations = conversation_participants
        .into_iter()
        .map(|(conversation_id, participant_ids)| ConversationV1 {
            conversation_id,
            display_name: None,
            conversation_type: None,
            participant_ids,
        })
        .collect::<Vec<_>>();
    let participants = participant_ids
        .into_iter()
        .map(|participant_id| ParticipantV1 {
            participant_id,
            display_name: None,
            participant_kind: None,
        })
        .collect::<Vec<_>>();
    let content_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&messages).map_err(|_| internal_error())?,
    ));
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
    archive_transfer::create_export(
        candidate.client_version.as_deref().unwrap_or("unverified"),
        vec![batch],
        conversations,
        participants,
    )
    .map_err(|_| CommandError {
        code: "COLLECTION_EXPORT_INVALID",
        message: "采集结果未通过本地完整性校验。".into(),
        recoverable: false,
    })
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

fn resolve_media_metadata(
    candidate: &SourceCandidate,
    allowed_roots: &[PathBuf],
    message: &mut archive_domain::MessageV1,
) {
    if message.media.is_empty() {
        return;
    }
    for media in &mut message.media {
        let source = PathBuf::from(&media.source_locator);
        let candidate_path = if source.is_absolute() {
            source
        } else {
            candidate.root_path.join(source)
        };
        let resolved = candidate_path.canonicalize().ok().filter(|path| {
            allowed_roots
                .iter()
                .any(|allowed_root| path.starts_with(allowed_root))
        });
        match resolved.and_then(|path| media_store::hash_source(&path).ok()) {
            Some((hash, size)) => {
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

fn verify_export_root(root: &Path) -> Result<(), CommandError> {
    if !root.is_dir() {
        return Err(CommandError {
            code: "EXPORT_DIRECTORY_UNAVAILABLE",
            message: "所选导出目录不可用。".into(),
            recoverable: true,
        });
    }
    let probe = root.join(format!(
        ".export-write-test-{}.partial",
        uuid::Uuid::new_v4()
    ));
    let result = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .and_then(|mut file| file.write_all(b"export-root-check"))
        .and_then(|_| fs::remove_file(&probe));
    if result.is_err() {
        let _ = fs::remove_file(probe);
        return Err(CommandError {
            code: "EXPORT_DIRECTORY_NOT_WRITABLE",
            message: "所选导出目录不可写。".into(),
            recoverable: true,
        });
    }
    Ok(())
}

#[tauri::command]
fn clear_saved_key(state: State<'_, AppState>) -> Result<(), CommandError> {
    let root = selected_portable_root(&state)?;
    let path = root.join("secrets").join("source-keys.dpapi");
    #[cfg(windows)]
    source_windows::dpapi::clear(&path).map_err(|_| CommandError {
        code: "KEY_CLEAR_FAILED",
        message: "无法清除本机保存的授权密钥。".into(),
        recoverable: true,
    })?;
    #[cfg(not(windows))]
    if path.exists() {
        fs::remove_file(path).map_err(|_| internal_error())?;
    }
    Ok(())
}

fn selected_portable_root(state: &State<'_, AppState>) -> Result<PathBuf, CommandError> {
    if let Some(path) = state
        .portable_root_override
        .lock()
        .map_err(|_| internal_error())?
        .clone()
    {
        return Ok(path);
    }
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let parent = executable.parent().ok_or_else(internal_error)?;
    Ok(parent.join("userData"))
}

fn verify_portable_root(root: &Path) -> Result<(), CommandError> {
    fs::create_dir_all(root).map_err(|_| portable_root_error())?;
    let probe = root.join(format!(".write-test-{}.partial", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&probe)?;
        file.write_all(b"portable-root-check")?;
        file.sync_all()?;
        fs::remove_file(&probe)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(probe);
        return Err(portable_root_error());
    }
    Ok(())
}

fn portable_root_error() -> CommandError {
    CommandError {
        code: "PORTABLE_ROOT_NOT_WRITABLE",
        message: "程序所在目录不可写，请选择一个可写的便携目录。".into(),
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

fn redacted_path(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|value| format!("…\\{value}"))
        .unwrap_or_else(|| "已选择的便携目录".into())
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
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            discover_sources,
            pick_directory,
            discover_selected_source,
            set_portable_root,
            collect_source,
            export_latest,
            open_export_folder,
            clear_saved_key,
        ])
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
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
            remember_key: false,
        }
    }

    #[test]
    fn export_filename_starts_with_safe_username_date_and_time() {
        let export_id = uuid::Uuid::nil();
        let username = sanitize_filename_component("熊<测试>/.. ");
        let file_name =
            build_client_export_file_name(&username, "20260909-143015", export_id, "json");

        assert!(file_name.starts_with("熊_测试__-20260909-143015-"));
        assert!(!file_name.contains('/') && !file_name.contains('<') && !file_name.contains('>'));
        assert!(!file_name.contains(".."));
        assert!(file_name.ends_with(".json"));
        assert_eq!(sanitize_filename_component("CON"), "_CON");
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
        let work_root = directory.0.join("work");
        let export =
            collect_candidate(candidate, work_root.clone(), None, authorization()).unwrap();

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
        let serialized = serde_json::to_string(&export).unwrap();
        assert!(!serialized.contains("private"));
        assert_eq!(fs::read_dir(work_root).unwrap().count(), 0);
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
}
