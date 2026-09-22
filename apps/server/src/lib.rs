use std::collections::BTreeMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use archive_domain::{ClientExportV1, MessageType};
use archive_export::{
    ExportFormat, ExportMedia, ExportPackage, ExportParticipant, ExportResult, ExportScope,
    export_package,
};
use archive_store::{ArchiveStore, IngestSummary, MessageQuery, StoreError};
use archive_transfer::{
    append_enterprise_collector_config, read_json, sort_export_for_output,
    write_importable_json_formatted,
};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
#[cfg(test)]
use chrono::TimeZone;
use chrono::{Local, NaiveTime, TimeDelta, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

#[path = "../../../crates/source-windows/src/enterprise_crypto.rs"]
#[allow(dead_code)]
mod enterprise_crypto;

#[cfg(windows)]
mod dpapi_protect {
    use std::{ffi::c_void, slice};

    #[repr(C)]
    struct Blob {
        cb_data: u32,
        data: *mut u8,
    }
    const UI_FORBIDDEN: u32 = 0x1;
    #[link(name = "crypt32")]
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CryptProtectData(
            input: *const Blob,
            description: *const u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const Blob,
            description: *mut *mut u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    fn transform(bytes: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let input = Blob {
            cb_data: bytes.len() as u32,
            data: bytes.as_ptr() as *mut u8,
        };
        let mut output = Blob {
            cb_data: 0,
            data: std::ptr::null_mut(),
        };
        let status = unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if status == 0 || output.data.is_null() || output.cb_data == 0 {
            return Err("DPAPI operation failed".into());
        }
        let result =
            unsafe { slice::from_raw_parts(output.data, output.cb_data as usize).to_vec() };
        unsafe {
            LocalFree(output.data.cast());
        }
        Ok(result)
    }
    pub fn protect(bytes: &[u8]) -> Result<Vec<u8>, String> {
        transform(bytes, true)
    }
    pub fn unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
        transform(bytes, false)
    }
}

#[cfg(not(windows))]
mod dpapi_protect {
    pub fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("server private key protection requires Windows DPAPI".into())
    }
    pub fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("server private key protection requires Windows DPAPI".into())
    }
}

const DATA_ROOT_ENV: &str = "WECOM_ARCHIVE_SERVER_DATA";
const LISTEN_ENV: &str = "WECOM_ARCHIVE_SERVER_LISTEN";
const DEFAULT_SERVER_PORT: u16 = 9812;
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:9812";
const MAX_CLIENT_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ENTERPRISE_CONFIG_FILE: &str = "enterprise-config.json";
const ACCESS_TOKEN_FILE: &str = "access-token.dpapi";
const DEFAULT_COLLECTION_NOTICE: &str = "加密传输本机企业微信聊天记录到服务端";
const LEGACY_COLLECTION_NOTICE: &str = "仅处理您有权归档的数据。";
const MIN_ENTERPRISE_KEY_ID_LEN: usize = 24;
const MAX_ENTERPRISE_KEY_ID_LEN: usize = 128;

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

#[derive(Clone)]
struct AppState {
    data_root: Arc<PathBuf>,
    archive_path: Arc<PathBuf>,
    token_sha256: Arc<Mutex<[u8; 32]>>,
    access_token_path: Arc<PathBuf>,
    enterprise_config: Arc<Mutex<EnterpriseConfig>>,
    collector_template: Option<Arc<PathBuf>>,
    local_collector: Option<LocalCollector>,
    local_collection_progress: Arc<Mutex<LocalCollectionProgress>>,
    local_collection_running: Arc<AtomicBool>,
    archive_update: watch::Sender<u64>,
}

pub type LocalCollectionProgressReporter = Arc<dyn Fn(u8, &str, &str) + Send + Sync>;
pub type LocalCollectionMediaSink =
    Arc<dyn Fn(&Path, &str, u64) -> Result<(), String> + Send + Sync>;
type LocalCollector = Arc<
    dyn Fn(
            bool,
            bool,
            Option<i64>,
            LocalCollectionProgressReporter,
            LocalCollectionMediaSink,
        ) -> Result<ClientExportV1, String>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCollectionProgress {
    running: bool,
    percent: u8,
    stage: String,
    detail: String,
}

impl Default for LocalCollectionProgress {
    fn default() -> Self {
        Self {
            running: false,
            percent: 0,
            stage: "等待开始".into(),
            detail: "选择采集范围后开始处理。".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseConfig {
    organization_id: String,
    organization_name: String,
    collection_notice: String,
    upload_url: String,
    #[serde(default)]
    include_files: bool,
    #[serde(default)]
    include_images: bool,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    offline_export_enabled: bool,
    #[serde(default = "default_super_admin_enabled")]
    super_admin_enabled: bool,
    #[serde(default)]
    local_collection_plans: Vec<LocalCollectionPlan>,
    #[serde(default, skip_serializing)]
    server_include_media: bool,
    #[serde(default)]
    #[serde(skip_serializing)]
    server_schedule: CollectionSchedule,
    #[serde(default)]
    collector_schedule: CollectionSchedule,
    key_id: String,
    public_key_hex: String,
    private_key_protected_hex: String,
    signing_public_key_hex: String,
    signing_private_key_protected_hex: String,
    #[serde(default)]
    key_history: Vec<EnterpriseKeyRecord>,
    #[serde(default)]
    collectors: Vec<EnterpriseCollectorRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionSchedule {
    #[serde(default)]
    mode: CollectionScheduleMode,
    #[serde(default = "default_schedule_interval_minutes")]
    interval_minutes: u32,
    #[serde(default = "default_schedule_daily_time")]
    daily_time: String,
}

impl Default for CollectionSchedule {
    fn default() -> Self {
        Self {
            mode: CollectionScheduleMode::Disabled,
            interval_minutes: default_schedule_interval_minutes(),
            daily_time: default_schedule_daily_time(),
        }
    }
}

impl CollectionSchedule {
    fn validate(&self) -> Result<(), ApiError> {
        if !(1..=10_080).contains(&self.interval_minutes)
            || NaiveTime::parse_from_str(&self.daily_time, "%H:%M").is_err()
        {
            return Err(ApiError::invalid_query());
        }
        Ok(())
    }

    fn next_after(&self, now: chrono::DateTime<Local>) -> Option<chrono::DateTime<Local>> {
        match self.mode {
            CollectionScheduleMode::Disabled => None,
            CollectionScheduleMode::Interval => {
                Some(now + TimeDelta::minutes(i64::from(self.interval_minutes)))
            }
            CollectionScheduleMode::Daily => {
                let time = NaiveTime::parse_from_str(&self.daily_time, "%H:%M").ok()?;
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CollectionScheduleMode {
    #[default]
    Disabled,
    Interval,
    Daily,
}

impl CollectionScheduleMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Interval => "interval",
            Self::Daily => "daily",
        }
    }
}

fn default_schedule_interval_minutes() -> u32 {
    60
}

fn default_super_admin_enabled() -> bool {
    true
}

fn default_schedule_daily_time() -> String {
    "02:00".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnterpriseKeyRecord {
    key_id: String,
    public_key_hex: String,
    private_key_protected_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnterpriseCollectorRecord {
    collector_id: String,
    key_id: String,
    created_at: String,
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    organization_name: String,
    #[serde(default)]
    upload_url: String,
    #[serde(default)]
    include_media: bool,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    offline_export_enabled: bool,
    #[serde(default)]
    schedule: CollectionSchedule,
    #[serde(default)]
    last_upload_at: Option<String>,
    public_key_hex: String,
    private_key_protected_hex: String,
    upload_token_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessTokenRequest {
    access_token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccessTokenResponse {
    updated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegeneratedAccessTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuperAdminRequest {
    enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SuperAdminResponse {
    super_admin_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseConfigRequest {
    organization_name: String,
    collection_notice: String,
    upload_url: String,
    key_id: Option<String>,
    #[serde(default)]
    include_media: bool,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    offline_export_enabled: bool,
    #[serde(default)]
    super_admin_enabled: Option<bool>,
    #[serde(default)]
    server_schedule: Option<CollectionSchedule>,
    #[serde(default)]
    collector_schedule: Option<CollectionSchedule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseConfigResponse {
    configured: bool,
    organization_id: String,
    organization_name: String,
    collection_notice: String,
    upload_url: String,
    key_id: String,
    include_media: bool,
    data_redaction: bool,
    offline_export_enabled: bool,
    super_admin_enabled: bool,
    server_schedule: CollectionSchedule,
    collector_schedule: CollectionSchedule,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalCollectionPlanRequest {
    name: String,
    schedule: CollectionSchedule,
    include_media: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalCollectionPlan {
    id: String,
    name: String,
    include_media: bool,
    schedule: CollectionSchedule,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    last_run_at: Option<String>,
    #[serde(default)]
    last_status: Option<String>,
    #[serde(default)]
    last_detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalCollectionPlanResponse {
    #[serde(flatten)]
    plan: LocalCollectionPlan,
    next_run_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionSchedulesResponse {
    local_plans: Vec<LocalCollectionPlanResponse>,
    collector_plans: Vec<CollectorPlanResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorPlanResponse {
    collector_id: String,
    file_name: String,
    organization_name: String,
    upload_url: String,
    include_media: bool,
    data_redaction: bool,
    offline_export_enabled: bool,
    schedule: CollectionSchedule,
    created_at: String,
    last_upload_at: Option<String>,
    next_run_at: Option<String>,
    executable_available: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorListResponse {
    collectors: Vec<CollectorPlanResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorResponse {
    file_name: String,
    directory: String,
    organization_id: String,
    key_id: String,
    collector_id: String,
    artifact: String,
    executable_generated: bool,
}

#[derive(Debug, Serialize)]
struct OpenDirectoryResponse {
    opened: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    schema: &'static str,
    network_mode: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportResponse {
    export_id: String,
    batch_count: usize,
    inserted: u64,
    unchanged: u64,
    revised: u64,
    media_count: u64,
    missing_media_count: u64,
}

#[derive(Debug, Serialize)]
struct ArchiveSummaryResponse {
    #[serde(flatten)]
    summary: archive_store::ArchiveSummary,
    local_collection_available: bool,
}

#[derive(Debug, Deserialize)]
struct ArchiveUpdateQuery {
    since: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveUpdateResponse {
    revision: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    code: &'static str,
    message: String,
    recoverable: bool,
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    limit: Option<u32>,
    offset: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct LocalCollectionQuery {
    include_media: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct OpenMediaQuery {
    name: String,
}

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    conversation_id: String,
    participant_id: Option<String>,
    text: Option<String>,
    message_type: Option<String>,
    media_only: Option<bool>,
    limit: Option<u32>,
    offset: Option<u64>,
    sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GlobalSearchQuery {
    q: String,
    limit: Option<u32>,
    sort: Option<String>,
}

#[derive(Debug, Serialize)]
struct MessageResponse {
    #[serde(flatten)]
    message: archive_domain::MessageV1,
    sender_name: Option<String>,
    sender_kind: Option<String>,
    quoted_message: Option<QuotedMessageResponse>,
}

#[derive(Debug, Serialize)]
struct GlobalSearchResponse {
    #[serde(flatten)]
    message: MessageResponse,
    conversation_name: Option<String>,
    offset_in_conversation: u64,
}

#[derive(Debug, Serialize)]
struct MessageCountResponse {
    total: u64,
}

#[derive(Debug, Serialize)]
struct QuotedMessageResponse {
    stable_message_id: String,
    sender_id: Option<String>,
    sender_name: Option<String>,
    sent_at: chrono::DateTime<Utc>,
    body_text: Option<String>,
    message_type: archive_domain::MessageType,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportRequest {
    scope: ExportScope,
    format: ExportFormat,
    conversation_id: Option<String>,
    participant_id: Option<String>,
    text: Option<String>,
    message_type: Option<String>,
    media_only: Option<bool>,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    simplify: bool,
    #[serde(default)]
    pretty: bool,
    #[serde(default)]
    conversation_order: ExportOrder,
    #[serde(default)]
    message_order: ExportOrder,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalExportRequest {
    format: ExportFormat,
    #[serde(default)]
    data_redaction: bool,
    #[serde(default)]
    simplify: bool,
    #[serde(default)]
    pretty: bool,
    #[serde(default)]
    conversation_order: ExportOrder,
    #[serde(default)]
    message_order: ExportOrder,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ExportOrder {
    Ascending,
    #[default]
    Descending,
}

impl ExportOrder {
    fn is_ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportResponse {
    export_id: String,
    file_name: String,
    directory: String,
    message_count: u64,
    media_count: u64,
    missing_media_count: u64,
    manifest_sha256: String,
}

struct PreparedServer {
    address: SocketAddr,
    token: Zeroizing<String>,
    router: Router,
    state: AppState,
}

pub struct DesktopServer {
    page_url: String,
    shutdown_sender: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    worker: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

impl DesktopServer {
    pub fn page_url(&self) -> &str {
        &self.page_url
    }

    pub fn is_running(&self) -> bool {
        self.worker
            .lock()
            .ok()
            .and_then(|worker| worker.as_ref().map(|worker| !worker.is_finished()))
            .unwrap_or(false)
    }

    pub fn shutdown(&self) {
        if let Ok(mut sender) = self.shutdown_sender.lock()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(());
        }
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for DesktopServer {
    fn drop(&mut self) {
        if let Ok(sender) = self.shutdown_sender.get_mut()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(());
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

pub fn start_desktop_server_with_local_collector<F>(collector: F) -> Result<DesktopServer, String>
where
    F: Fn(
            bool,
            bool,
            Option<i64>,
            LocalCollectionProgressReporter,
            LocalCollectionMediaSink,
        ) -> Result<ClientExportV1, String>
        + Send
        + Sync
        + 'static,
{
    start_desktop_server_inner(Some(Arc::new(collector)))
}

fn start_desktop_server_inner(
    local_collector: Option<LocalCollector>,
) -> Result<DesktopServer, String> {
    let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
    let collector_template = std::env::current_exe().ok();
    let prepared = prepare_server(collector_template, local_collector)?;
    let page_url = format!(
        "http://{}/#token={}",
        prepared.address,
        prepared.token.as_str()
    );
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("wecom-archive-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "无法初始化本机服务运行时。".to_owned())?;
            runtime.block_on(serve(prepared, shutdown_receiver, Some(ready_sender)))
        })
        .map_err(|_| "无法创建本机服务线程。".to_owned())?;

    match ready_receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(DesktopServer {
            page_url,
            shutdown_sender: Mutex::new(Some(shutdown_sender)),
            worker: Mutex::new(Some(worker)),
        }),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => {
            let _ = shutdown_sender.send(());
            let _ = worker.join();
            Err("本机归档服务启动超时。".into())
        }
    }
}

fn prepare_server(
    collector_template: Option<PathBuf>,
    local_collector: Option<LocalCollector>,
) -> Result<PreparedServer, String> {
    let data_root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_data_root);
    std::fs::create_dir_all(&data_root)
        .map_err(|error| format!("服务端数据目录不可用（{}），请检查目录权限。", error.kind()))?;
    let access_token_path = data_root.join(ACCESS_TOKEN_FILE);
    let token = load_or_generate_access_token(&access_token_path)
        .map_err(|_| "服务端访问令牌初始化失败，请检查 serverData 目录权限。".to_owned())?;
    let address = std::env::var(LISTEN_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_SERVER_PORT,
        ));
    if !address.ip().is_loopback() {
        return Err("服务端只允许监听本机回环地址。".into());
    }

    let archive_path = Arc::new(data_root.join("archive.db"));
    let initial_revision = open_archive_for_startup(&archive_path)?;
    let (archive_update, _) = watch::channel(initial_revision);
    let state = AppState {
        data_root: Arc::new(data_root.clone()),
        archive_path,
        token_sha256: Arc::new(Mutex::new(Sha256::digest(token.as_bytes()).into())),
        access_token_path: Arc::new(access_token_path),
        enterprise_config: Arc::new(Mutex::new(load_enterprise_config(&data_root))),
        collector_template: collector_template.map(Arc::new),
        local_collector,
        local_collection_progress: Arc::new(Mutex::new(LocalCollectionProgress::default())),
        local_collection_running: Arc::new(AtomicBool::new(false)),
        archive_update,
    };

    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/imports/json", post(import_json))
        .route("/api/v1/imports/enterprise", post(import_enterprise))
        .route("/api/v1/collections/local", post(collect_local))
        .route(
            "/api/v1/collections/local/progress",
            get(local_collection_progress),
        )
        .route(
            "/api/v1/collection-schedules",
            get(get_collection_schedules).post(create_local_collection_plan),
        )
        .route(
            "/api/v1/collection-schedules/{plan_id}",
            axum::routing::put(update_local_collection_plan).delete(delete_local_collection_plan),
        )
        .route("/api/v1/server/access-token", post(update_access_token))
        .route(
            "/api/v1/server/access-token/regenerate",
            post(regenerate_access_token),
        )
        .route(
            "/api/v1/enterprise/config",
            get(get_enterprise_config).put(update_enterprise_config),
        )
        .route(
            "/api/v1/settings/super-admin",
            axum::routing::put(update_super_admin),
        )
        .route("/api/v1/enterprise/key/rotate", post(rotate_enterprise_key))
        .route(
            "/api/v1/enterprise/collectors",
            get(list_collectors).post(generate_collector),
        )
        .route(
            "/api/v1/enterprise/collectors/open-directory",
            post(open_collector_directory),
        )
        .route("/api/v1/archive/summary", get(archive_summary))
        .route("/api/v1/archive/updates", get(archive_updates))
        .route("/api/v1/archive/collected-users", get(collected_users))
        .route("/api/v1/conversations", get(list_conversations))
        .route(
            "/api/v1/conversations/{conversation_id}/participants",
            get(list_conversation_participants),
        )
        .route("/api/v1/media/{content_hash}", get(get_media))
        .route("/api/v1/media/{content_hash}/open", post(open_media))
        .route("/api/v1/messages/count", get(count_messages))
        .route("/api/v1/messages", get(list_messages))
        .route("/api/v1/search/messages", get(search_messages))
        .route(
            "/api/v1/exports",
            post(create_export_job).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/exports/local",
            post(create_local_export).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/exports/open-directory",
            post(open_export_directory),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state.clone())
        .fallback(embedded_web);

    Ok(PreparedServer {
        address,
        token,
        router: app,
        state,
    })
}

fn open_archive_for_startup(path: &Path) -> Result<u64, String> {
    let mut last_error = None;
    for attempt in 0..45 {
        match ArchiveStore::open_with_busy_timeout(path, Duration::from_secs(1))
            .and_then(|store| store.summary())
        {
            Ok(summary) => {
                return Ok(summary.revision);
            }
            Err(error) => {
                let detail = error.to_string();
                let locked = detail.contains("database is locked")
                    || detail.contains("database table is locked");
                last_error = Some(detail);
                if !locked || attempt == 44 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(format!(
        "归档数据库初始化失败：{}。如果刚刚中断了采集，请等待上一次采集完全退出后重试。",
        last_error.unwrap_or_else(|| "未知数据库错误".into())
    ))
}

async fn serve(
    prepared: PreparedServer,
    shutdown_receiver: std::sync::mpsc::Receiver<()>,
    ready_sender: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
) -> Result<(), String> {
    let listener = match tokio::net::TcpListener::bind(prepared.address).await {
        Ok(listener) => listener,
        Err(error) => {
            let message = format!(
                "服务端启动失败（{}）。请确认 {} 端口未被占用。",
                error.kind(),
                prepared.address.port()
            );
            if let Some(sender) = ready_sender {
                let _ = sender.send(Err(message.clone()));
            }
            return Err(message);
        }
    };
    if let Some(sender) = ready_sender {
        let _ = sender.send(Ok(()));
    }
    let scheduler = tokio::spawn(run_local_collection_scheduler(prepared.state.clone()));
    axum::serve(listener, prepared.router)
        .with_graceful_shutdown(async move {
            let _ = tokio::task::spawn_blocking(move || shutdown_receiver.recv()).await;
        })
        .await
        .map_err(|_| "本机归档服务异常退出。".to_owned())?;
    scheduler.abort();
    Ok(())
}

fn load_or_generate_access_token(path: &Path) -> Result<Zeroizing<String>, String> {
    if path.is_file() {
        let protected = std::fs::read(path).map_err(|_| "access token file is unavailable")?;
        let token = dpapi_protect::unprotect(&protected)?;
        let token = String::from_utf8(token).map_err(|_| "access token file is invalid")?;
        if token.len() < 24 {
            return Err("access token is too short".into());
        }
        return Ok(Zeroizing::new(token));
    }
    let token = random_access_token();
    persist_access_token(path, token.as_bytes())?;
    Ok(Zeroizing::new(token))
}

fn random_access_token() -> String {
    let bytes = enterprise_crypto::random_bytes::<32>().unwrap_or_else(|_| {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(first.as_bytes());
        bytes[16..].copy_from_slice(second.as_bytes());
        bytes
    });
    encode_hex(&bytes)
}

fn persist_access_token(path: &Path, token: &[u8]) -> Result<(), String> {
    if token.len() < 24 {
        return Err("access token is too short".into());
    }
    let protected = dpapi_protect::protect(token)?;
    let partial = path.with_extension("dpapi.partial");
    std::fs::write(&partial, protected).map_err(|_| "access token write failed")?;
    if path.exists() {
        std::fs::remove_file(path).map_err(|_| "access token replacement failed")?;
    }
    std::fs::rename(partial, path).map_err(|_| "access token publish failed".to_owned())
}

fn default_data_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("serverData")
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        schema: "server-api.v1",
        network_mode: "loopback_only",
    })
}

async fn update_access_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AccessTokenRequest>,
) -> Result<Json<AccessTokenResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let token = request.access_token.trim();
    if token.len() < 24 {
        return Err(ApiError::invalid_query());
    }
    persist_access_token(&state.access_token_path, token.as_bytes())
        .map_err(|_| ApiError::store())?;
    *state.token_sha256.lock().map_err(|_| ApiError::store())? =
        Sha256::digest(token.as_bytes()).into();
    Ok(Json(AccessTokenResponse { updated: true }))
}

async fn regenerate_access_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RegeneratedAccessTokenResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let token = random_access_token();
    persist_access_token(&state.access_token_path, token.as_bytes())
        .map_err(|_| ApiError::store())?;
    *state.token_sha256.lock().map_err(|_| ApiError::store())? =
        Sha256::digest(token.as_bytes()).into();
    Ok(Json(RegeneratedAccessTokenResponse {
        access_token: token,
    }))
}

async fn embedded_web(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            Body::from(
                r#"{"code":"API_NOT_FOUND","message":"请求的接口不存在。","recoverable":false}"#,
            ),
        )
            .into_response();
    }
    let requested = if uri.path() == "/" {
        "/index.html"
    } else {
        uri.path()
    };
    let asset = embedded_asset(requested).or_else(|| embedded_asset("/index.html"));
    match asset {
        Some((bytes, content_type)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            Body::from(bytes),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn load_enterprise_config(root: &Path) -> EnterpriseConfig {
    let path = root.join(ENTERPRISE_CONFIG_FILE);
    let Some(mut config) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    else {
        return new_enterprise_config();
    };
    let mut migrated = discard_unused_historical_keys(&mut config);
    if config.collection_notice == LEGACY_COLLECTION_NOTICE {
        config.collection_notice = DEFAULT_COLLECTION_NOTICE.into();
        migrated = true;
    }
    if config.local_collection_plans.is_empty()
        && config.server_schedule.mode != CollectionScheduleMode::Disabled
    {
        let now = Utc::now().to_rfc3339();
        config.local_collection_plans.push(LocalCollectionPlan {
            id: Uuid::new_v4().to_string(),
            name: "本机采集计划".into(),
            include_media: config.server_include_media,
            schedule: config.server_schedule.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_run_at: None,
            last_status: None,
            last_detail: None,
        });
        config.server_schedule = CollectionSchedule::default();
        config.server_include_media = false;
        migrated = true;
    }
    if migrated {
        let _ = persist_enterprise_config(root, &config);
    }
    config
}

fn new_enterprise_config() -> EnterpriseConfig {
    let (
        public_key_hex,
        private_key_protected_hex,
        signing_public_key_hex,
        signing_private_key_protected_hex,
    ) = enterprise_crypto::generate_rsa_key_pair()
        .and_then(|(public_key, private_key)| {
            dpapi_protect::protect(&private_key).and_then(|protected| {
                enterprise_crypto::generate_rsa_key_pair().and_then(
                    |(signing_public, signing_private)| {
                        dpapi_protect::protect(&signing_private).map(|signing_protected| {
                            (
                                encode_hex(&public_key),
                                encode_hex(&protected),
                                encode_hex(&signing_public),
                                encode_hex(&signing_protected),
                            )
                        })
                    },
                )
            })
        })
        .unwrap_or_default();
    EnterpriseConfig {
        organization_id: Uuid::new_v4().to_string(),
        organization_name: String::new(),
        collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
        upload_url: default_upload_url(),
        include_files: false,
        include_images: false,
        data_redaction: false,
        offline_export_enabled: false,
        super_admin_enabled: default_super_admin_enabled(),
        local_collection_plans: Vec::new(),
        server_include_media: false,
        server_schedule: CollectionSchedule::default(),
        collector_schedule: CollectionSchedule::default(),
        key_id: format!("key-{}", Uuid::new_v4().simple()),
        public_key_hex,
        private_key_protected_hex,
        signing_public_key_hex,
        signing_private_key_protected_hex,
        key_history: Vec::new(),
        collectors: Vec::new(),
    }
}

fn enterprise_config_response(config: &EnterpriseConfig) -> EnterpriseConfigResponse {
    EnterpriseConfigResponse {
        configured: !config.organization_name.trim().is_empty(),
        organization_id: config.organization_id.clone(),
        organization_name: config.organization_name.clone(),
        collection_notice: config.collection_notice.clone(),
        upload_url: config.upload_url.clone(),
        key_id: config.key_id.clone(),
        include_media: config.include_files || config.include_images,
        data_redaction: config.data_redaction,
        offline_export_enabled: config.offline_export_enabled,
        super_admin_enabled: config.super_admin_enabled,
        server_schedule: config.server_schedule.clone(),
        collector_schedule: config.collector_schedule.clone(),
    }
}

fn collection_schedules_response(
    config: &EnterpriseConfig,
    data_root: &Path,
) -> CollectionSchedulesResponse {
    let now = Local::now();
    let local_plans = config
        .local_collection_plans
        .iter()
        .cloned()
        .map(|plan| LocalCollectionPlanResponse {
            next_run_at: estimated_next_run(
                &plan.schedule,
                plan.last_run_at
                    .as_deref()
                    .or(Some(plan.created_at.as_str())),
                now,
            ),
            plan,
        })
        .collect();
    CollectionSchedulesResponse {
        local_plans,
        collector_plans: collector_plan_responses(config, Some(&data_root.join("collectors")), now),
    }
}

fn estimated_next_run(
    schedule: &CollectionSchedule,
    reference: Option<&str>,
    now: chrono::DateTime<Local>,
) -> Option<String> {
    let next = match schedule.mode {
        CollectionScheduleMode::Disabled => None,
        CollectionScheduleMode::Daily => schedule.next_after(now),
        CollectionScheduleMode::Interval => {
            let from_reference = reference
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| {
                    value.with_timezone(&Local)
                        + TimeDelta::minutes(i64::from(schedule.interval_minutes))
                });
            Some(
                from_reference
                    .filter(|value| *value > now)
                    .unwrap_or_else(|| {
                        now + TimeDelta::minutes(i64::from(schedule.interval_minutes))
                    }),
            )
        }
    }?;
    Some(next.to_rfc3339())
}

fn collector_plan_responses(
    config: &EnterpriseConfig,
    collector_root: Option<&Path>,
    now: chrono::DateTime<Local>,
) -> Vec<CollectorPlanResponse> {
    let mut plans = config
        .collectors
        .iter()
        .map(|collector| CollectorPlanResponse {
            collector_id: collector.collector_id.clone(),
            file_name: collector.file_name.clone(),
            organization_name: collector.organization_name.clone(),
            upload_url: collector.upload_url.clone(),
            include_media: collector.include_media,
            data_redaction: collector.data_redaction,
            offline_export_enabled: collector.offline_export_enabled,
            schedule: collector.schedule.clone(),
            created_at: collector.created_at.clone(),
            last_upload_at: collector.last_upload_at.clone(),
            next_run_at: estimated_next_run(
                &collector.schedule,
                collector
                    .last_upload_at
                    .as_deref()
                    .or(Some(collector.created_at.as_str())),
                now,
            ),
            executable_available: collector_root
                .filter(|_| !collector.file_name.is_empty())
                .is_some_and(|root| root.join(&collector.file_name).is_file()),
        })
        .collect::<Vec<_>>();
    plans.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    plans
}

fn default_upload_url() -> String {
    DEFAULT_SERVER_URL.into()
}

fn normalize_upload_url(value: &str) -> Result<String, ApiError> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty()
        || value.len() > 2048
        || value.chars().any(char::is_whitespace)
        || !(value.starts_with("http://") || value.starts_with("https://"))
    {
        return Err(ApiError::invalid_query());
    }
    Ok(value.to_owned())
}

fn upload_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/api/v1/imports/enterprise") {
        base_url.to_owned()
    } else {
        format!("{base_url}/api/v1/imports/enterprise")
    }
}

fn persist_enterprise_config(root: &Path, config: &EnterpriseConfig) -> Result<(), ApiError> {
    let path = root.join(ENTERPRISE_CONFIG_FILE);
    let partial = path.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(config).map_err(|_| ApiError::store())?;
    std::fs::write(&partial, bytes).map_err(|_| ApiError::store())?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| ApiError::store())?;
    }
    std::fs::rename(&partial, &path).map_err(|_| ApiError::store())
}

fn discard_unused_historical_keys(config: &mut EnterpriseConfig) -> bool {
    let used_key_ids: Vec<String> = config
        .collectors
        .iter()
        .map(|collector| collector.key_id.clone())
        .collect();
    let previous_count = config.key_history.len();
    config
        .key_history
        .retain(|record| used_key_ids.contains(&record.key_id));
    config.key_history.len() != previous_count
}

fn preserve_current_key_if_used(config: &mut EnterpriseConfig) {
    let key_was_used = config
        .collectors
        .iter()
        .any(|collector| collector.key_id == config.key_id);
    let key_is_preserved = config
        .key_history
        .iter()
        .any(|record| record.key_id == config.key_id);
    if key_was_used && !key_is_preserved && !config.private_key_protected_hex.is_empty() {
        config.key_history.push(EnterpriseKeyRecord {
            key_id: config.key_id.clone(),
            public_key_hex: config.public_key_hex.clone(),
            private_key_protected_hex: config.private_key_protected_hex.clone(),
        });
    }
}

async fn get_enterprise_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    Ok(Json(enterprise_config_response(&config)))
}

async fn get_collection_schedules(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

async fn update_super_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SuperAdminRequest>,
) -> Result<Json<SuperAdminResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    config.super_admin_enabled = request.enabled;
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(SuperAdminResponse {
        super_admin_enabled: config.super_admin_enabled,
    }))
}

async fn create_local_collection_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LocalCollectionPlanRequest>,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    request.schedule.validate()?;
    let name = validate_plan_name(&request.name)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let now = Utc::now().to_rfc3339();
    config.local_collection_plans.push(LocalCollectionPlan {
        id: Uuid::new_v4().to_string(),
        name,
        include_media: request.include_media,
        schedule: request.schedule,
        created_at: now.clone(),
        updated_at: now,
        last_run_at: None,
        last_status: None,
        last_detail: None,
    });
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

async fn update_local_collection_plan(
    State(state): State<AppState>,
    AxumPath(plan_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<LocalCollectionPlanRequest>,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    request.schedule.validate()?;
    let name = validate_plan_name(&request.name)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let plan = config
        .local_collection_plans
        .iter_mut()
        .find(|plan| plan.id == plan_id)
        .ok_or_else(ApiError::invalid_query)?;
    plan.name = name;
    plan.include_media = request.include_media;
    plan.schedule = request.schedule;
    plan.updated_at = Utc::now().to_rfc3339();
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

async fn delete_local_collection_plan(
    State(state): State<AppState>,
    AxumPath(plan_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let previous_len = config.local_collection_plans.len();
    config
        .local_collection_plans
        .retain(|plan| plan.id != plan_id);
    if config.local_collection_plans.len() == previous_len {
        return Err(ApiError::invalid_query());
    }
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

fn validate_plan_name(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 80 {
        return Err(ApiError::invalid_query());
    }
    Ok(value.to_owned())
}

async fn update_enterprise_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EnterpriseConfigRequest>,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    if request.organization_name.trim().is_empty() || request.collection_notice.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let upload_url = normalize_upload_url(&request.upload_url)?;
    if let Some(schedule) = &request.server_schedule {
        schedule.validate()?;
    }
    if let Some(schedule) = &request.collector_schedule {
        schedule.validate()?;
    }
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    if let Some(key_id) = request
        .key_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        if !(MIN_ENTERPRISE_KEY_ID_LEN..=MAX_ENTERPRISE_KEY_ID_LEN).contains(&key_id.len()) {
            return Err(ApiError::invalid_query());
        }
        if key_id != config.key_id {
            preserve_current_key_if_used(&mut config);
            config.key_id = key_id;
            let (public_key, private_key) =
                enterprise_crypto::generate_rsa_key_pair().map_err(|_| ApiError::store())?;
            config.public_key_hex = encode_hex(&public_key);
            config.private_key_protected_hex =
                encode_hex(&dpapi_protect::protect(&private_key).map_err(|_| ApiError::store())?);
        }
    }
    config.organization_name = request.organization_name.trim().to_owned();
    config.collection_notice = request.collection_notice.trim().to_owned();
    config.upload_url = upload_url;
    config.include_files = request.include_media;
    config.include_images = request.include_media;
    config.data_redaction = request.data_redaction;
    config.offline_export_enabled = request.offline_export_enabled;
    if let Some(enabled) = request.super_admin_enabled {
        config.super_admin_enabled = enabled;
    }
    if let Some(schedule) = request.server_schedule {
        config.server_schedule = schedule;
    }
    if let Some(schedule) = request.collector_schedule {
        config.collector_schedule = schedule;
    }
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(enterprise_config_response(&config)))
}

async fn rotate_enterprise_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    preserve_current_key_if_used(&mut config);
    let (public_key, private_key) =
        enterprise_crypto::generate_rsa_key_pair().map_err(|_| ApiError::store())?;
    config.key_id = format!("key-{}", Uuid::new_v4().simple());
    config.public_key_hex = encode_hex(&public_key);
    config.private_key_protected_hex =
        encode_hex(&dpapi_protect::protect(&private_key).map_err(|_| ApiError::store())?);
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(enterprise_config_response(&config)))
}

async fn generate_collector(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectorResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    if config.organization_name.trim().is_empty()
        || config.public_key_hex.is_empty()
        || config.private_key_protected_hex.is_empty()
        || config.signing_public_key_hex.is_empty()
        || config.signing_private_key_protected_hex.is_empty()
    {
        return Err(ApiError::invalid_query());
    }
    let upload_url = upload_endpoint(&config.upload_url);
    let signing_private = dpapi_protect::unprotect(
        &decode_hex(&config.signing_private_key_protected_hex).map_err(|_| ApiError::store())?,
    )
    .map_err(|_| ApiError::store())?;
    let collector_id = Uuid::new_v4().to_string();
    let upload_token = random_access_token();
    let upload_token_sha256 = encode_hex(&Sha256::digest(upload_token.as_bytes()));
    let key_id = config.key_id.clone();
    let public_key_hex = config.public_key_hex.clone();
    let signing_payload = collector_signing_payload(&config, &upload_url, &upload_token);
    let signature = enterprise_crypto::sign(&signing_private, signing_payload.as_bytes())
        .map_err(|_| ApiError::store())?;
    let artifact = serde_json::json!({
        "schemaVersion": "enterprise-collector.v1",
        "organizationId": config.organization_id,
        "organizationName": config.organization_name,
        "collectionNotice": config.collection_notice,
        "keyId": key_id,
        "encryption": "aes-256-gcm+rsa-oaep-sha256",
        "publicKeyHex": public_key_hex,
        "signingPublicKeyHex": config.signing_public_key_hex,
        "signatureHex": encode_hex(&signature),
        "uploadUrl": upload_url,
        "uploadToken": upload_token,
        "includeFiles": config.include_files,
        "includeImages": config.include_images,
        "includeMedia": config.include_files || config.include_images,
        "dataRedaction": config.data_redaction,
        "offlineExportEnabled": config.offline_export_enabled,
        "collectorSchedule": &config.collector_schedule,
        "formats": ["enterprise-package.v1"],
    });
    let artifact_text = serde_json::to_string_pretty(&artifact).map_err(|_| ApiError::store())?;
    let file_name = format!(
        "{}-{}.exe",
        sanitize_file_component(&key_id),
        &collector_id[..8]
    );
    let collector_root = state.data_root.join("collectors");
    std::fs::create_dir_all(&collector_root).map_err(|_| ApiError::store())?;
    let executable_path = collector_root.join(&file_name);
    let partial_executable = collector_root.join(format!(".{file_name}.{collector_id}.partial"));
    let executable_generated = state
        .collector_template
        .as_deref()
        .filter(|template| template.is_file())
        .cloned()
        .or_else(find_collector_template)
        .is_some_and(|template| {
            let generated = (|| -> Result<(), std::io::Error> {
                std::fs::copy(template, &partial_executable)?;
                append_enterprise_collector_config(&partial_executable, artifact_text.as_bytes())
                    .map_err(std::io::Error::other)?;
                if executable_path.exists() {
                    std::fs::remove_file(&executable_path)?;
                }
                std::fs::rename(&partial_executable, &executable_path)?;
                Ok(())
            })()
            .is_ok();
            if !generated {
                let _ = std::fs::remove_file(&partial_executable);
            }
            generated
        });
    if executable_generated {
        let legacy_config_path = collector_root.join(format!("{file_name}.wca-collector"));
        if legacy_config_path.is_file() {
            let _ = std::fs::remove_file(legacy_config_path);
        }
        let collector_organization_name = config.organization_name.clone();
        let collector_include_media = config.include_files || config.include_images;
        let collector_data_redaction = config.data_redaction;
        let collector_offline_export_enabled = config.offline_export_enabled;
        let collector_schedule = config.collector_schedule.clone();
        config.collectors.push(EnterpriseCollectorRecord {
            collector_id: collector_id.clone(),
            key_id: key_id.clone(),
            created_at: Utc::now().to_rfc3339(),
            file_name: file_name.clone(),
            organization_name: collector_organization_name,
            upload_url: upload_url.clone(),
            include_media: collector_include_media,
            data_redaction: collector_data_redaction,
            offline_export_enabled: collector_offline_export_enabled,
            schedule: collector_schedule,
            last_upload_at: None,
            public_key_hex: String::new(),
            private_key_protected_hex: String::new(),
            upload_token_sha256,
        });
        if let Err(error) = persist_enterprise_config(&state.data_root, &config) {
            config
                .collectors
                .retain(|collector| collector.collector_id != collector_id);
            let _ = std::fs::remove_file(&executable_path);
            return Err(error);
        }
    }
    Ok(Json(CollectorResponse {
        file_name,
        directory: collector_root.display().to_string(),
        organization_id: config.organization_id.clone(),
        key_id,
        collector_id,
        artifact: artifact_text,
        executable_generated,
    }))
}

async fn list_collectors(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectorListResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    Ok(Json(CollectorListResponse {
        collectors: collector_plan_responses(
            &config,
            Some(&state.data_root.join("collectors")),
            Local::now(),
        ),
    }))
}

async fn open_collector_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OpenDirectoryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let directory = state.data_root.join("collectors");
    if !directory.is_dir() {
        return Err(ApiError::store());
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(&directory)
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|_| ApiError::store())?;
        Ok(Json(OpenDirectoryResponse { opened: true }))
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(ApiError::invalid_query())
    }
}

fn collector_signing_payload(
    config: &EnterpriseConfig,
    upload_url: &str,
    upload_token: &str,
) -> String {
    format!(
        "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        config.organization_id,
        config.organization_name,
        config.collection_notice,
        config.key_id,
        config.public_key_hex,
        "aes-256-gcm+rsa-oaep-sha256",
        upload_url,
        upload_token,
        config.include_files,
        config.include_images,
        config.data_redaction,
        config.offline_export_enabled,
        config.collector_schedule.mode.as_str(),
        config.collector_schedule.interval_minutes,
        config.collector_schedule.daily_time
    )
}

fn find_collector_template() -> Option<PathBuf> {
    let executable = std::env::current_exe()
        .ok()?
        .parent()?
        .join("WeComArchive.exe");
    executable.is_file().then_some(executable)
}

async fn import_enterprise(
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
    if result.is_ok()
        && let Some(collector_id) = collector_id
        && let Ok(mut config) = state.enterprise_config.lock()
        && let Some(collector) = config
            .collectors
            .iter_mut()
            .find(|collector| collector.collector_id == collector_id)
    {
        collector.last_upload_at = Some(Utc::now().to_rfc3339());
        let _ = persist_enterprise_config(&state.data_root, &config);
    }
    result
}

async fn import_json(
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

async fn ingest_export(
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
    let _ = state.archive_update.send(revision);
    Ok(Json(ImportResponse {
        export_id,
        batch_count,
        inserted: summary.inserted,
        unchanged: summary.unchanged,
        revised: summary.revised,
        media_count,
        missing_media_count,
    }))
}

fn persist_media_blobs(
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

fn persist_local_media_file(
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

async fn get_media(
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

async fn open_media(
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
fn open_file_with_default_application(path: &Path) -> Result<(), ApiError> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> isize;
    }
    let operation = "open\0".encode_utf16().collect::<Vec<_>>();
    let file = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: the UTF-16 buffers are null terminated and stay alive for the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    (result > 32).then_some(()).ok_or_else(ApiError::store)
}

#[cfg(not(windows))]
fn open_file_with_default_application(_: &Path) -> Result<(), ApiError> {
    Err(ApiError::invalid_query())
}

async fn archive_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ArchiveSummaryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let local_collection_available = state.local_collector.is_some();
    let archive_path = Arc::clone(&state.archive_path);
    let summary = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.summary())
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(ArchiveSummaryResponse {
        summary,
        local_collection_available,
    }))
}

async fn archive_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ArchiveUpdateQuery>,
) -> Result<Json<ArchiveUpdateResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut receiver = state.archive_update.subscribe();
    if *receiver.borrow() <= query.since {
        receiver.changed().await.map_err(|_| ApiError::store())?;
    }
    Ok(Json(ArchiveUpdateResponse {
        revision: *receiver.borrow(),
    }))
}

async fn collected_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<archive_store::CollectedUser>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let archive_path = Arc::clone(&state.archive_path);
    let users = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.list_collected_users())
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(users))
}

async fn collect_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LocalCollectionQuery>,
) -> Result<Json<ImportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let collector = state
        .local_collector
        .clone()
        .ok_or_else(ApiError::local_collection_unavailable)?;
    if state
        .local_collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(ApiError::local_collection_running());
    }
    let _run_guard = LocalCollectionRunGuard(Arc::clone(&state.local_collection_running));
    let (configured_include_media, data_redaction) = {
        let config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        (config.server_include_media, config.data_redaction)
    };
    let include_media = query.include_media.unwrap_or(configured_include_media);
    set_local_collection_progress(&state, true, 3, "准备采集", "正在初始化本机采集任务。");
    let progress = Arc::clone(&state.local_collection_progress);
    let reporter: LocalCollectionProgressReporter = Arc::new(move |percent, stage, detail| {
        if let Ok(mut current) = progress.lock() {
            *current = LocalCollectionProgress {
                running: true,
                percent: percent.min(95),
                stage: stage.to_owned(),
                detail: detail.to_owned(),
            };
        }
    });
    let media_root = Arc::clone(&state.data_root);
    let media_sink: LocalCollectionMediaSink =
        Arc::new(move |source, expected_hash, expected_size| {
            persist_local_media_file(&media_root, source, expected_hash, expected_size)
                .map_err(|_| "媒体文件写入归档失败。".to_owned())
        });
    let export = match tokio::task::spawn_blocking(move || {
        collector(include_media, data_redaction, None, reporter, media_sink)
    })
    .await
    {
        Ok(Ok(export)) => export,
        Ok(Err(message)) => {
            set_local_collection_progress(&state, false, 0, "采集失败", &message);
            return Err(ApiError::local_collection_failed(message));
        }
        Err(_) => {
            let message = "本机采集进程异常结束，请重试。".to_owned();
            set_local_collection_progress(&state, false, 0, "采集失败", &message);
            return Err(ApiError::local_collection_failed(message));
        }
    };
    set_local_collection_progress(
        &state,
        true,
        92,
        "写入归档",
        "正在校验并写入消息、图片和文件。",
    );
    let result = ingest_export(&state, export).await;
    if result.is_ok() {
        set_local_collection_progress(
            &state,
            false,
            100,
            "采集完成",
            "本机数据已经合并到当前归档。",
        );
    } else {
        set_local_collection_progress(
            &state,
            false,
            0,
            "写入失败",
            "采集结果未能写入归档，请重试。",
        );
    }
    result
}

struct LocalCollectionRunGuard(Arc<AtomicBool>);

impl Drop for LocalCollectionRunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn run_local_collection_scheduler(state: AppState) {
    let mut next_runs: BTreeMap<String, (CollectionSchedule, chrono::DateTime<Local>)> =
        BTreeMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let plans = match state.enterprise_config.lock() {
            Ok(config) => config.local_collection_plans.clone(),
            Err(_) => continue,
        };
        let active_ids = plans
            .iter()
            .map(|plan| plan.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        next_runs.retain(|id, _| active_ids.contains(id.as_str()));
        let now = Local::now();
        for plan in &plans {
            if plan.schedule.mode == CollectionScheduleMode::Disabled {
                next_runs.remove(&plan.id);
                continue;
            }
            let should_reset = next_runs
                .get(&plan.id)
                .is_none_or(|(schedule, _)| schedule != &plan.schedule);
            if should_reset {
                if let Some(next) = plan.schedule.next_after(now) {
                    next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
                }
            }
        }
        for plan in plans {
            let Some((_, due_at)) = next_runs.get(&plan.id).cloned() else {
                continue;
            };
            if now < due_at {
                continue;
            }
            if state
                .local_collection_running
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                if let Some(next) = plan.schedule.next_after(now) {
                    next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
                }
                continue;
            }
            let guard = LocalCollectionRunGuard(Arc::clone(&state.local_collection_running));
            let since = plan
                .last_run_at
                .as_deref()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis().saturating_sub(5 * 60 * 1000));
            let result = run_local_collection_once(&state, plan.include_media, since).await;
            drop(guard);
            update_local_plan_result(&state, &plan.id, &result);
            if let Err(error) = result {
                set_local_collection_progress(
                    &state,
                    false,
                    0,
                    "定时采集失败",
                    &error.body.message,
                );
            }
            if let Some(next) = plan.schedule.next_after(Local::now()) {
                next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
            }
        }
    }
}

fn update_local_plan_result(
    state: &AppState,
    plan_id: &str,
    result: &Result<Json<ImportResponse>, ApiError>,
) {
    if let Ok(mut config) = state.enterprise_config.lock()
        && let Some(plan) = config
            .local_collection_plans
            .iter_mut()
            .find(|plan| plan.id == plan_id)
    {
        if result.is_ok() {
            plan.last_run_at = Some(Utc::now().to_rfc3339());
        }
        plan.last_status = Some(if result.is_ok() { "success" } else { "error" }.into());
        plan.last_detail = result
            .as_ref()
            .err()
            .map(|error| error.body.message.clone());
        plan.updated_at = Utc::now().to_rfc3339();
        let _ = persist_enterprise_config(&state.data_root, &config);
    }
}

async fn run_local_collection_once(
    state: &AppState,
    include_media: bool,
    since_unix_ms: Option<i64>,
) -> Result<Json<ImportResponse>, ApiError> {
    let collector = state
        .local_collector
        .clone()
        .ok_or_else(ApiError::local_collection_unavailable)?;
    let data_redaction = {
        let config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        config.data_redaction
    };
    set_local_collection_progress(state, true, 3, "准备采集", "正在初始化定时采集任务。");
    let progress = Arc::clone(&state.local_collection_progress);
    let reporter: LocalCollectionProgressReporter = Arc::new(move |percent, stage, detail| {
        if let Ok(mut current) = progress.lock() {
            *current = LocalCollectionProgress {
                running: true,
                percent: percent.min(95),
                stage: stage.to_owned(),
                detail: detail.to_owned(),
            };
        }
    });
    let media_root = Arc::clone(&state.data_root);
    let media_sink: LocalCollectionMediaSink =
        Arc::new(move |source, expected_hash, expected_size| {
            persist_local_media_file(&media_root, source, expected_hash, expected_size)
                .map_err(|_| "媒体文件写入归档失败。".to_owned())
        });
    let export = tokio::task::spawn_blocking(move || {
        collector(
            include_media,
            data_redaction,
            since_unix_ms,
            reporter,
            media_sink,
        )
    })
    .await
    .map_err(|_| ApiError::local_collection_failed("本机采集进程异常结束，请重试。".into()))?
    .map_err(ApiError::local_collection_failed)?;
    set_local_collection_progress(
        state,
        true,
        92,
        "写入归档",
        "正在校验并写入消息、图片和文件。",
    );
    let result = ingest_export(state, export).await;
    if result.is_ok() {
        set_local_collection_progress(
            state,
            false,
            100,
            "采集完成",
            "本机数据已经合并到当前归档。",
        );
    } else {
        set_local_collection_progress(
            state,
            false,
            0,
            "写入失败",
            "采集结果未能写入归档，请重试。",
        );
    }
    result
}

async fn local_collection_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LocalCollectionProgress>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let progress = state
        .local_collection_progress
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    Ok(Json(progress))
}

fn set_local_collection_progress(
    state: &AppState,
    running: bool,
    percent: u8,
    stage: &str,
    detail: &str,
) {
    if let Ok(mut progress) = state.local_collection_progress.lock() {
        *progress = LocalCollectionProgress {
            running,
            percent: percent.min(100),
            stage: stage.to_owned(),
            detail: detail.to_owned(),
        };
    }
}

async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<Vec<archive_store::ConversationListItem>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let archive_path = Arc::clone(&state.archive_path);
    let conversations = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| {
                store.list_conversations(query.limit.unwrap_or(50), query.offset.unwrap_or(0))
            })
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(conversations))
}

async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Vec<MessageResponse>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if query.conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let message_type = query
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let sort_ascending = parse_sort_ascending(query.sort.as_deref())?;
    let message_query = MessageQuery {
        conversation_id: Some(query.conversation_id),
        participant_id: query.participant_id,
        text: query.text,
        starts_at: None,
        ends_at: None,
        message_type,
        media_only: query.media_only.unwrap_or(false),
        limit: query.limit.unwrap_or(100),
        offset: query.offset.unwrap_or(0),
        sort_ascending,
    };
    let archive_path = Arc::clone(&state.archive_path);
    let messages = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(|_| ApiError::store())?;
        let messages = store
            .query_messages(&message_query)
            .map_err(|_| ApiError::store())?;
        let participants = store
            .list_conversation_participants(
                message_query.conversation_id.as_deref().unwrap_or_default(),
            )
            .map_err(|_| ApiError::store())?
            .into_iter()
            .map(|participant| (participant.participant_id.clone(), participant))
            .collect::<std::collections::BTreeMap<_, _>>();
        messages
            .into_iter()
            .map(|message| {
                let participant = message
                    .sender_id
                    .as_ref()
                    .and_then(|sender_id| participants.get(sender_id));
                let quoted_message = message
                    .quoted_message_id
                    .as_deref()
                    .map(|quoted_id| store.get_message(quoted_id))
                    .transpose()
                    .map_err(|_| ApiError::store())?
                    .flatten()
                    .map(|quoted| {
                        let quoted_name = quoted
                            .sender_id
                            .as_ref()
                            .and_then(|sender_id| participants.get(sender_id))
                            .and_then(|participant| participant.display_name.clone());
                        QuotedMessageResponse {
                            stable_message_id: quoted.stable_message_id,
                            sender_id: quoted.sender_id,
                            sender_name: quoted_name,
                            sent_at: quoted.sent_at,
                            body_text: quoted.body_text,
                            message_type: quoted.message_type,
                        }
                    });
                Ok(MessageResponse {
                    sender_name: participant.and_then(|item| item.display_name.clone()),
                    sender_kind: participant.and_then(|item| item.participant_kind.clone()),
                    message,
                    quoted_message,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(messages))
}

async fn count_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<MessageCountResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if query.conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let message_type = query
        .message_type
        .as_deref()
        .map(parse_message_type)
        .transpose()?;
    let message_query = MessageQuery {
        conversation_id: Some(query.conversation_id),
        participant_id: query.participant_id,
        text: query.text,
        starts_at: None,
        ends_at: None,
        message_type,
        media_only: query.media_only.unwrap_or(false),
        limit: 1,
        offset: 0,
        sort_ascending: false,
    };
    let archive_path = Arc::clone(&state.archive_path);
    let total = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.count_messages(&message_query))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(MessageCountResponse { total }))
}

async fn search_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GlobalSearchQuery>,
) -> Result<Json<Vec<GlobalSearchResponse>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    let search_text = query.q.trim().to_owned();
    if search_text.is_empty() || search_text.chars().count() > 200 {
        return Err(ApiError::invalid_query());
    }
    let sort_ascending = parse_sort_ascending(query.sort.as_deref())?;
    let archive_path = Arc::clone(&state.archive_path);
    let results = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(|_| ApiError::store())?;
        let messages = store
            .query_messages(&MessageQuery {
                conversation_id: None,
                participant_id: None,
                text: Some(search_text),
                starts_at: None,
                ends_at: None,
                message_type: None,
                media_only: false,
                limit: query.limit.unwrap_or(50).clamp(1, 100),
                offset: 0,
                sort_ascending,
            })
            .map_err(|_| ApiError::store())?;
        let mut participant_cache = std::collections::BTreeMap::new();
        let mut results = Vec::with_capacity(messages.len());
        for message in messages {
            if !participant_cache.contains_key(&message.conversation_id) {
                let participants = store
                    .list_conversation_participants(&message.conversation_id)
                    .map_err(|_| ApiError::store())?
                    .into_iter()
                    .map(|participant| (participant.participant_id.clone(), participant))
                    .collect::<std::collections::BTreeMap<_, _>>();
                participant_cache.insert(message.conversation_id.clone(), participants);
            }
            let participants = participant_cache
                .get(&message.conversation_id)
                .ok_or_else(ApiError::store)?;
            let participant = message
                .sender_id
                .as_ref()
                .and_then(|sender_id| participants.get(sender_id));
            let quoted_message = message
                .quoted_message_id
                .as_deref()
                .map(|quoted_id| store.get_message(quoted_id))
                .transpose()
                .map_err(|_| ApiError::store())?
                .flatten()
                .map(|quoted| QuotedMessageResponse {
                    sender_name: quoted
                        .sender_id
                        .as_ref()
                        .and_then(|sender_id| participants.get(sender_id))
                        .and_then(|item| item.display_name.clone()),
                    stable_message_id: quoted.stable_message_id,
                    sender_id: quoted.sender_id,
                    sent_at: quoted.sent_at,
                    body_text: quoted.body_text,
                    message_type: quoted.message_type,
                });
            let conversation_name = store
                .conversation_display_name(&message.conversation_id)
                .map_err(|_| ApiError::store())?
                .or_else(|| {
                    message.conversation_id.starts_with("S:").then(|| {
                        participants
                            .values()
                            .filter_map(|item| item.display_name.clone())
                            .collect::<Vec<_>>()
                            .join("、")
                    })
                })
                .filter(|name| !name.is_empty());
            let offset_in_conversation = store
                .message_offset_in_conversation(&message.stable_message_id, sort_ascending)
                .map_err(|_| ApiError::store())?
                .unwrap_or(0);
            results.push(GlobalSearchResponse {
                conversation_name,
                offset_in_conversation,
                message: MessageResponse {
                    sender_name: participant.and_then(|item| item.display_name.clone()),
                    sender_kind: participant.and_then(|item| item.participant_kind.clone()),
                    message,
                    quoted_message,
                },
            });
        }
        Ok::<_, ApiError>(results)
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(results))
}

async fn list_conversation_participants(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Vec<archive_store::ParticipantListItem>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    require_super_admin(&state)?;
    if conversation_id.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let archive_path = Arc::clone(&state.archive_path);
    let participants = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.list_conversation_participants(&conversation_id))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(participants))
}

async fn create_export_job(
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
    let conversation_id = match request.scope {
        ExportScope::CurrentConversation | ExportScope::CurrentFilter => Some(
            request
                .conversation_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(ApiError::invalid_query)?,
        ),
        ExportScope::EntireArchive => None,
    };
    let query = MessageQuery {
        conversation_id,
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
        starts_at: None,
        ends_at: None,
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
        let mut messages = query_all_messages(&store, query).map_err(ApiError::from_store)?;
        if request.data_redaction {
            redact_sensitive_messages(&mut messages);
        }
        let conversation_ids = messages
            .iter()
            .map(|message| message.conversation_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut participant_directory = std::collections::BTreeMap::new();
        let mut conversation_names = std::collections::BTreeMap::new();
        if request.simplify || request.pretty {
            for id in &conversation_ids {
                for participant in store
                    .list_conversation_participants(id)
                    .map_err(ApiError::from_store)?
                {
                    participant_directory.insert(participant.participant_id.clone(), participant);
                }
            }
        }
        let mut offset = 0;
        let mut remaining_conversations = conversation_ids.clone();
        while !remaining_conversations.is_empty() {
            let page = store
                .list_conversations(200, offset)
                .map_err(ApiError::from_store)?;
            if page.is_empty() {
                break;
            }
            offset += page.len() as u64;
            for conversation in page {
                if remaining_conversations.remove(conversation.conversation_id.as_str())
                    && let Some(name) = conversation.display_name
                {
                    conversation_names.insert(conversation.conversation_id, name);
                }
            }
        }
        let participants = messages
            .iter()
            .filter_map(|message| message.sender_id.as_ref())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|id| ExportParticipant {
                participant_id: id.clone(),
                display_name: participant_directory
                    .get(id)
                    .and_then(|participant| participant.display_name.clone()),
                participant_kind: None,
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
        let export_id = Uuid::new_v4();
        let export_root = data_root.join("exports");
        std::fs::create_dir_all(&export_root).map_err(|_| ApiError::store())?;
        let extension = match format {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Html => "html",
            ExportFormat::Md => "md",
        };
        let file_name = format!(
            "archive-export-{}-{}.{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            export_id.simple(),
            extension
        );
        let target = export_root.join(&file_name);
        let package = ExportPackage {
            archive_id: "default".into(),
            scope,
            format,
            messages,
            participants,
            conversation_names,
            simplify: request.simplify,
            pretty: request.pretty,
            media,
            generated_at: Utc::now(),
            conversation_order_ascending: request.conversation_order.is_ascending(),
            message_order_ascending: request.message_order.is_ascending(),
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

/// Export the current local WeCom snapshot directly. This keeps the fast
/// standalone export flow separate from archive ingestion: it avoids writing
/// the snapshot to SQLite and then reading the same messages back for export.
async fn create_local_export(
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
        let mut export = collector(include_media, data_redaction, None, reporter, media_sink)
            .map_err(ApiError::local_collection_failed)?;
        let export_id = export.export_id;
        let export_root = data_root.join("exports");
        std::fs::create_dir_all(&export_root).map_err(|_| ApiError::store())?;
        let extension = match format {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Html => "html",
            ExportFormat::Md => "md",
        };
        let file_name = format!(
            "local-export-{}-{}.{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            export_id.simple(),
            extension
        );
        let target = export_root.join(&file_name);
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

async fn open_export_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OpenDirectoryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let directory = state.data_root.join("exports");
    std::fs::create_dir_all(&directory).map_err(|_| ApiError::store())?;
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(&directory)
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|_| ApiError::store())?;
        Ok(Json(OpenDirectoryResponse { opened: true }))
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(ApiError::invalid_query())
    }
}

fn redact_sensitive_messages(messages: &mut [archive_domain::MessageV1]) {
    archive_transfer::redact_sensitive_messages(messages);
}

fn query_all_messages(
    store: &ArchiveStore,
    mut query: MessageQuery,
) -> Result<Vec<archive_domain::MessageV1>, StoreError> {
    let mut messages = Vec::new();
    loop {
        query.offset = messages.len() as u64;
        let page = store.query_messages(&query)?;
        let count = page.len();
        messages.extend(page);
        if count < query.limit as usize {
            messages.sort_by(|left, right| {
                left.sent_at
                    .cmp(&right.sent_at)
                    .then_with(|| left.stable_message_id.cmp(&right.stable_message_id))
            });
            return Ok(messages);
        }
    }
}

fn parse_message_type(value: &str) -> Result<MessageType, ApiError> {
    match value {
        "text" => Ok(MessageType::Text),
        "image" => Ok(MessageType::Image),
        "audio" => Ok(MessageType::Audio),
        "video" => Ok(MessageType::Video),
        "file" => Ok(MessageType::File),
        "link" => Ok(MessageType::Link),
        "reply" => Ok(MessageType::Reply),
        "system" => Ok(MessageType::System),
        "unsupported" => Ok(MessageType::Unsupported),
        _ => Err(ApiError::invalid_query()),
    }
}

fn parse_sort_ascending(value: Option<&str>) -> Result<bool, ApiError> {
    match value.unwrap_or("desc") {
        "asc" => Ok(true),
        "desc" => Ok(false),
        _ => Err(ApiError::invalid_query()),
    }
}

async fn receive_json(root: &Path, body: Body) -> Result<PathBuf, ApiError> {
    receive_json_with_limit(root, body, MAX_CLIENT_EXPORT_BYTES).await
}

async fn receive_json_with_limit(root: &Path, body: Body, limit: u64) -> Result<PathBuf, ApiError> {
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

async fn receive_package(root: &Path, body: Body) -> Result<PathBuf, ApiError> {
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

fn authorize(headers: &HeaderMap, expected: &Arc<Mutex<[u8; 32]>>) -> Result<(), ApiError> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(ApiError::unauthorized());
    };
    let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    let expected = expected.lock().map_err(|_| ApiError::store())?;
    let difference = actual
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |state, (left, right)| state | (left ^ right));
    if difference == 0 {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn authorize_enterprise_upload(
    headers: &HeaderMap,
    server_token_sha256: &Arc<Mutex<[u8; 32]>>,
    enterprise_config: &Arc<Mutex<EnterpriseConfig>>,
) -> Result<Option<String>, ApiError> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(ApiError::unauthorized());
    };
    let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    let server_matches = {
        let expected = server_token_sha256.lock().map_err(|_| ApiError::store())?;
        constant_time_equal(&actual, expected.as_slice())
    };
    if server_matches {
        return Ok(None);
    }
    let actual_hex = encode_hex(&actual);
    let config = enterprise_config.lock().map_err(|_| ApiError::store())?;
    config
        .collectors
        .iter()
        .find(|collector| {
            constant_time_equal(
                actual_hex.as_bytes(),
                collector.upload_token_sha256.as_bytes(),
            )
        })
        .map(|collector| Some(collector.collector_id.clone()))
        .ok_or_else(ApiError::unauthorized)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(encode_hex(&hasher.finalize()))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(());
    }
    pairs
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or(())? as u8;
            let low = (pair[1] as char).to_digit(16).ok_or(())? as u8;
            Ok((high << 4) | low)
        })
        .collect()
}

fn is_valid_content_hash(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn sanitize_file_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        "item".into()
    } else {
        cleaned.into()
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: ErrorResponse,
}

impl ApiError {
    fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            body: ErrorResponse {
                code: "SUPER_ADMIN_REQUIRED",
                message: "当前未启用超管模式，请在设置中开启后再访问会话内容。".into(),
                recoverable: true,
            },
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorResponse {
                code: "MEDIA_NOT_FOUND",
                message: "媒体内容不存在。".into(),
                recoverable: true,
            },
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            body: ErrorResponse {
                code: "UNAUTHORIZED",
                message: "需要有效的服务端访问令牌。".into(),
                recoverable: true,
            },
        }
    }

    fn invalid_export() -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "INVALID_CLIENT_EXPORT",
                message: "客户端 JSON 格式、关系或校验值无效。".into(),
                recoverable: true,
            },
        }
    }

    fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: ErrorResponse {
                code: "CLIENT_EXPORT_TOO_LARGE",
                message: "客户端 JSON 超过服务端单次导入上限。".into(),
                recoverable: true,
            },
        }
    }

    fn invalid_query() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: ErrorResponse {
                code: "INVALID_QUERY",
                message: "检索参数无效。".into(),
                recoverable: true,
            },
        }
    }

    fn local_collection_unavailable() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_UNAVAILABLE",
                message: "当前访问方式不支持采集本机数据，请在桌面工作台中使用。".into(),
                recoverable: true,
            },
        }
    }

    fn local_collection_running() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_RUNNING",
                message: "已有本机采集任务正在运行，请稍后重试。".into(),
                recoverable: true,
            },
        }
    }

    fn local_collection_failed(message: String) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "LOCAL_COLLECTION_FAILED",
                message,
                recoverable: true,
            },
        }
    }

    fn store() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorResponse {
                code: "ARCHIVE_STORE_FAILED",
                message: "服务端归档写入失败，诊断信息已脱敏。".into(),
                recoverable: true,
            },
        }
    }

    fn from_store(error: StoreError) -> Self {
        if matches!(error, StoreError::BatchConflict) {
            Self {
                status: StatusCode::CONFLICT,
                body: ErrorResponse {
                    code: "BATCH_CONFLICT",
                    message: "同一采集批次标识已存在，但内容校验值不同。".into(),
                    recoverable: false,
                },
            }
        } else {
            Self::store()
        }
    }

    fn from_export(error: archive_export::ExportError) -> Self {
        let _ = error;
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorResponse {
                code: "ARCHIVE_EXPORT_FAILED",
                message: "服务端导出失败；未完成文件已清理。".into(),
                recoverable: true,
            },
        }
    }
}

fn require_super_admin(state: &AppState) -> Result<(), ApiError> {
    let enabled = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .super_admin_enabled;
    enabled.then_some(()).ok_or_else(ApiError::forbidden)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{
        ArchiveBatchV1, BATCH_SCHEMA_VERSION, CollectionScope, ConversationV1,
        EmployeeNoticeEvidence, ExternalContactConsent, LifecycleState, MESSAGE_SCHEMA_VERSION,
        MediaBlobV1, MessageDirection, MessageType, MessageV1, ParticipantV1, RetentionDirective,
    };
    use archive_transfer::create_export;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    const TEST_TOKEN: &str = "0123456789abcdefghijklmnop";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("archive-server-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state(directory: &TestDirectory) -> AppState {
        AppState {
            data_root: Arc::new(directory.0.clone()),
            archive_path: Arc::new(directory.0.join("archive.db")),
            token_sha256: Arc::new(Mutex::new(Sha256::digest(TEST_TOKEN.as_bytes()).into())),
            access_token_path: Arc::new(directory.0.join(ACCESS_TOKEN_FILE)),
            enterprise_config: Arc::new(Mutex::new(new_enterprise_config())),
            collector_template: None,
            local_collector: None,
            local_collection_progress: Arc::new(Mutex::new(LocalCollectionProgress::default())),
            local_collection_running: Arc::new(AtomicBool::new(false)),
            archive_update: watch::channel(0).0,
        }
    }

    fn authorized_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        headers
    }

    fn enable_super_admin(state: &AppState) {
        state.enterprise_config.lock().unwrap().super_admin_enabled = true;
    }

    #[tokio::test]
    async fn super_admin_defaults_to_enabled_and_can_be_disabled_for_server_export() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        assert!(
            app_state
                .enterprise_config
                .lock()
                .unwrap()
                .super_admin_enabled
        );
        app_state
            .enterprise_config
            .lock()
            .unwrap()
            .super_admin_enabled = false;

        let message_error = list_messages(
            State(app_state.clone()),
            authorized_headers(),
            Query(MessagesQuery {
                conversation_id: "conversation".into(),
                participant_id: None,
                text: None,
                message_type: None,
                media_only: None,
                limit: None,
                offset: None,
                sort: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(message_error.status, StatusCode::FORBIDDEN);

        let error = create_export_job(
            State(app_state),
            authorized_headers(),
            Json(ExportRequest {
                scope: ExportScope::EntireArchive,
                format: ExportFormat::Json,
                conversation_id: None,
                participant_id: None,
                text: None,
                message_type: None,
                media_only: None,
                data_redaction: false,
                simplify: false,
                pretty: false,
                conversation_order: ExportOrder::Descending,
                message_order: ExportOrder::Descending,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "SUPER_ADMIN_REQUIRED");
    }

    #[tokio::test]
    async fn super_admin_setting_is_persisted() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let Json(response) = update_super_admin(
            State(app_state),
            authorized_headers(),
            Json(SuperAdminRequest { enabled: true }),
        )
        .await
        .unwrap();
        assert!(response.super_admin_enabled);
        assert!(load_enterprise_config(&directory.0).super_admin_enabled);
    }

    #[test]
    fn collection_schedule_calculates_interval_and_daily_runs() {
        let now = Local
            .with_ymd_and_hms(2026, 1, 15, 1, 30, 0)
            .single()
            .unwrap();
        let interval = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 45,
            daily_time: "02:00".into(),
        };
        assert_eq!(
            interval.next_after(now).unwrap() - now,
            TimeDelta::minutes(45)
        );

        let daily = CollectionSchedule {
            mode: CollectionScheduleMode::Daily,
            interval_minutes: 60,
            daily_time: "02:00".into(),
        };
        let next = daily.next_after(now).unwrap();
        assert_eq!(next.date_naive(), now.date_naive());
        assert_eq!(next.time(), NaiveTime::from_hms_opt(2, 0, 0).unwrap());
        assert!(
            CollectionSchedule {
                mode: CollectionScheduleMode::Interval,
                interval_minutes: 1,
                daily_time: "02:00".into(),
            }
            .validate()
            .is_ok()
        );
        assert!(
            CollectionSchedule {
                mode: CollectionScheduleMode::Interval,
                interval_minutes: 0,
                daily_time: "02:00".into(),
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn local_collection_plans_are_independent_and_persisted() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let first_schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Daily,
            interval_minutes: 60,
            daily_time: "03:15".into(),
        };
        let second_schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 30,
            daily_time: "02:00".into(),
        };

        let Json(first_response) = create_local_collection_plan(
            State(app_state.clone()),
            authorized_headers(),
            Json(LocalCollectionPlanRequest {
                name: "早班采集".into(),
                schedule: first_schedule.clone(),
                include_media: true,
            }),
        )
        .await
        .unwrap();
        assert_eq!(first_response.local_plans.len(), 1);
        assert_eq!(first_response.local_plans[0].plan.schedule, first_schedule);

        let Json(second_response) = create_local_collection_plan(
            State(app_state.clone()),
            authorized_headers(),
            Json(LocalCollectionPlanRequest {
                name: "晚班采集".into(),
                schedule: second_schedule.clone(),
                include_media: false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(second_response.local_plans.len(), 2);

        let persisted = load_enterprise_config(&directory.0);
        assert_eq!(persisted.local_collection_plans.len(), 2);
        assert_eq!(
            persisted.local_collection_plans[1].schedule,
            second_schedule
        );
    }

    #[tokio::test]
    async fn enterprise_config_update_without_schedules_preserves_existing_plans() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        {
            let mut config = app_state.enterprise_config.lock().unwrap();
            config.local_collection_plans.push(LocalCollectionPlan {
                id: "local-plan".into(),
                name: "已有计划".into(),
                include_media: true,
                schedule: CollectionSchedule::default(),
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                last_run_at: None,
                last_status: None,
                last_detail: None,
            });
            config.collector_schedule = CollectionSchedule {
                mode: CollectionScheduleMode::Interval,
                interval_minutes: 90,
                daily_time: "02:00".into(),
            };
        }

        let _ = update_enterprise_config(
            State(app_state.clone()),
            authorized_headers(),
            Json(EnterpriseConfigRequest {
                organization_name: "测试企业".into(),
                collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
                upload_url: default_upload_url(),
                key_id: None,
                include_media: true,
                data_redaction: false,
                offline_export_enabled: false,
                super_admin_enabled: None,
                server_schedule: None,
                collector_schedule: None,
            }),
        )
        .await
        .unwrap();

        let config = app_state.enterprise_config.lock().unwrap();
        assert_eq!(config.local_collection_plans.len(), 1);
        assert_eq!(config.collector_schedule.interval_minutes, 90);
    }

    #[tokio::test]
    async fn local_collection_plan_rejects_an_invalid_interval() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);

        let error = create_local_collection_plan(
            State(app_state),
            authorized_headers(),
            Json(LocalCollectionPlanRequest {
                name: "无效计划".into(),
                include_media: false,
                schedule: CollectionSchedule {
                    mode: CollectionScheduleMode::Interval,
                    interval_minutes: 0,
                    daily_time: "02:00".into(),
                },
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn streams_local_media_to_the_content_addressed_store() {
        let directory = TestDirectory::new();
        let source = directory.0.join("source.bin");
        let bytes = b"streamed local media";
        std::fs::write(&source, bytes).unwrap();
        let hash = encode_hex(&Sha256::digest(bytes));

        persist_local_media_file(&directory.0, &source, &hash, bytes.len() as u64).unwrap();

        assert_eq!(
            std::fs::read(directory.0.join("media").join(hash)).unwrap(),
            bytes
        );
        assert!(
            std::fs::read_dir(directory.0.join("media"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".partial"))
        );
    }

    fn export_bytes(body: &str, batch_id: Uuid, batch_hash: &str) -> Vec<u8> {
        let sent_at = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let batch = ArchiveBatchV1 {
            schema_version: BATCH_SCHEMA_VERSION.into(),
            batch_id,
            source_adapter: "synthetic_fixture".into(),
            source_instance_id: "source-fixture".into(),
            collected_at: sent_at,
            employee_notice: EmployeeNoticeEvidence {
                notice_version: "test-notice".into(),
                displayed_at: sent_at,
                acknowledged_at: Some(sent_at),
                evidence_hash: "test-evidence".into(),
            },
            external_contact_consent: ExternalContactConsent::NotApplicable,
            collection_scope: CollectionScope {
                source_ids: vec!["source-fixture".into()],
                conversation_ids: vec!["conversation-fixture".into()],
                starts_at: None,
                ends_at: None,
                message_types: vec![MessageType::Text],
            },
            cursor_before: None,
            cursor_after: Some("cursor-1".into()),
            messages: vec![MessageV1 {
                schema_version: MESSAGE_SCHEMA_VERSION.into(),
                source_kind: "synthetic_fixture".into(),
                source_instance_id: "source-fixture".into(),
                source_message_id: "message-1".into(),
                stable_message_id: "stable-message-1".into(),
                conversation_id: "conversation-fixture".into(),
                sender_id: Some("participant-fixture".into()),
                sent_at,
                direction: MessageDirection::Incoming,
                message_type: MessageType::Text,
                body_text: Some(body.into()),
                quoted_message_id: None,
                lifecycle: LifecycleState::Active,
                media: vec![],
                raw_type: "text".into(),
                raw_payload: json!({"text": body}),
                parser_version: "synthetic-fixture.v1".into(),
                collection_batch_id: batch_id,
            }],
            content_sha256: batch_hash.into(),
            retention: RetentionDirective {
                policy_id: None,
                expires_at: None,
                legal_hold: false,
            },
        };
        let export = create_export(
            "test-client",
            vec![batch],
            vec![ConversationV1 {
                conversation_id: "conversation-fixture".into(),
                display_name: Some("测试会话".into()),
                conversation_type: Some("direct".into()),
                participant_ids: vec!["participant-fixture".into()],
            }],
            vec![ParticipantV1 {
                participant_id: "participant-fixture".into(),
                display_name: Some("测试成员".into()),
                participant_kind: Some("employee".into()),
            }],
        )
        .unwrap();
        serde_json::to_vec(&export).unwrap()
    }

    #[tokio::test]
    async fn rejects_missing_and_incorrect_tokens() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let missing = import_json(State(app_state.clone()), HeaderMap::new(), Body::from("{}"))
            .await
            .unwrap_err();
        assert_eq!(missing.status, StatusCode::UNAUTHORIZED);

        let mut wrong_headers = HeaderMap::new();
        wrong_headers.insert(
            "authorization",
            "Bearer 000000000000000000000000".parse().unwrap(),
        );
        let wrong = import_json(State(app_state), wrong_headers, Body::from("{}"))
            .await
            .unwrap_err();
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn local_collection_query_overrides_the_configured_media_mode() {
        let directory = TestDirectory::new();
        let mut app_state = state(&directory);
        app_state.enterprise_config.lock().unwrap().include_files = false;
        app_state.enterprise_config.lock().unwrap().include_images = false;
        let selected_modes = Arc::new(Mutex::new(Vec::new()));
        let captured_modes = Arc::clone(&selected_modes);
        let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
            "本机消息",
            Uuid::new_v4(),
            "local-collection",
        ))
        .unwrap();
        app_state.local_collector = Some(Arc::new(move |include_media, _, _, reporter, _| {
            captured_modes.lock().unwrap().push(include_media);
            reporter(55, "解析消息", "正在解析测试消息。");
            Ok(export.clone())
        }));

        let _ = collect_local(
            State(app_state.clone()),
            authorized_headers(),
            Query(LocalCollectionQuery {
                include_media: Some(true),
            }),
        )
        .await
        .unwrap();

        assert_eq!(*selected_modes.lock().unwrap(), vec![true]);
        let progress = app_state.local_collection_progress.lock().unwrap().clone();
        assert!(!progress.running);
        assert_eq!(progress.percent, 100);
        assert_eq!(progress.stage, "采集完成");
    }

    #[tokio::test]
    async fn generated_collector_copies_the_configured_single_executable_template() {
        let directory = TestDirectory::new();
        let template = directory.0.join("enterprise-template.exe");
        std::fs::write(&template, b"single executable template").unwrap();
        let mut app_state = state(&directory);
        app_state.collector_template = Some(Arc::new(template));
        app_state
            .enterprise_config
            .lock()
            .unwrap()
            .organization_name = "测试企业".into();
        app_state
            .enterprise_config
            .lock()
            .unwrap()
            .collector_schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 45,
            daily_time: "02:00".into(),
        };

        let before = app_state.enterprise_config.lock().unwrap().clone();
        let Json(response) = generate_collector(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();

        assert!(response.executable_generated);
        assert_eq!(response.key_id, before.key_id);
        assert!(
            response
                .file_name
                .starts_with(&format!("{}-", before.key_id))
        );
        assert!(response.file_name.ends_with(".exe"));
        assert_eq!(
            response.directory,
            directory.0.join("collectors").display().to_string()
        );
        let artifact: serde_json::Value = serde_json::from_str(&response.artifact).unwrap();
        assert_eq!(artifact["keyId"], before.key_id);
        assert_eq!(artifact["publicKeyHex"], before.public_key_hex);
        assert_eq!(artifact["collectorSchedule"]["mode"], "interval");
        assert_eq!(artifact["collectorSchedule"]["intervalMinutes"], 45);
        let after = app_state.enterprise_config.lock().unwrap().clone();
        assert!(after.collectors.iter().any(|collector| {
            collector.collector_id == response.collector_id
                && collector.key_id == before.key_id
                && collector.file_name == response.file_name
                && collector.schedule.mode == CollectionScheduleMode::Interval
                && collector.private_key_protected_hex.is_empty()
        }));
        let executable = directory.0.join("collectors").join(&response.file_name);
        let executable_bytes = std::fs::read(&executable).unwrap();
        assert!(executable_bytes.starts_with(b"single executable template"));
        let embedded_config =
            archive_transfer::read_enterprise_collector_config(&executable).unwrap();
        assert_eq!(embedded_config, response.artifact.as_bytes());
        assert!(
            !directory
                .0
                .join("collectors")
                .join(format!("{}.wca-collector", response.file_name))
                .exists()
        );

        let Json(second) = generate_collector(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();
        assert_ne!(second.file_name, response.file_name);
        let Json(list) = list_collectors(State(app_state), authorized_headers())
            .await
            .unwrap();
        assert_eq!(list.collectors.len(), 2);
        assert!(list.collectors.iter().all(|collector| {
            collector.schedule.mode == CollectionScheduleMode::Interval
                && collector.executable_available
        }));
    }

    #[tokio::test]
    async fn regenerated_access_token_replaces_the_persisted_token_and_authentication_hash() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);

        let Json(response) =
            regenerate_access_token(State(app_state.clone()), authorized_headers())
                .await
                .unwrap();

        assert_eq!(response.access_token.len(), 64);
        assert!(authorize(&authorized_headers(), &app_state.token_sha256).is_err());
        let mut new_headers = HeaderMap::new();
        new_headers.insert(
            "authorization",
            format!("Bearer {}", response.access_token).parse().unwrap(),
        );
        assert!(authorize(&new_headers, &app_state.token_sha256).is_ok());
        assert!(app_state.access_token_path.is_file());
    }

    #[tokio::test]
    async fn rotating_unused_enterprise_key_does_not_preserve_it() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let before = app_state.enterprise_config.lock().unwrap().clone();

        let Json(response) = rotate_enterprise_key(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();
        let after = app_state.enterprise_config.lock().unwrap().clone();

        assert_ne!(response.key_id, before.key_id);
        assert_ne!(
            after.private_key_protected_hex,
            before.private_key_protected_hex
        );
        assert!(
            !after
                .key_history
                .iter()
                .any(|record| record.key_id == before.key_id)
        );
    }

    #[tokio::test]
    async fn custom_enterprise_key_id_replaces_the_key_pair() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let before = app_state.enterprise_config.lock().unwrap().clone();
        let custom_key_id = "custom-enterprise-key-20260914".to_owned();

        let Json(response) = update_enterprise_config(
            State(app_state.clone()),
            authorized_headers(),
            Json(EnterpriseConfigRequest {
                organization_name: "测试企业".into(),
                collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
                upload_url: default_upload_url(),
                key_id: Some(custom_key_id.clone()),
                include_media: false,
                data_redaction: false,
                offline_export_enabled: false,
                super_admin_enabled: None,
                server_schedule: None,
                collector_schedule: None,
            }),
        )
        .await
        .unwrap();
        let after = app_state.enterprise_config.lock().unwrap().clone();

        assert_eq!(response.key_id, custom_key_id);
        assert_eq!(after.key_id, custom_key_id);
        assert_ne!(after.public_key_hex, before.public_key_hex);
        assert_ne!(
            after.private_key_protected_hex,
            before.private_key_protected_hex
        );
    }

    #[tokio::test]
    async fn rotating_used_enterprise_key_preserves_it_for_existing_collectors() {
        let directory = TestDirectory::new();
        let template = directory.0.join("enterprise-template.exe");
        std::fs::write(&template, b"single executable template").unwrap();
        let mut app_state = state(&directory);
        app_state.collector_template = Some(Arc::new(template));
        app_state
            .enterprise_config
            .lock()
            .unwrap()
            .organization_name = "测试企业".into();
        let before = app_state.enterprise_config.lock().unwrap().clone();

        let Json(collector) = generate_collector(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();
        let Json(response) = rotate_enterprise_key(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();
        let after = app_state.enterprise_config.lock().unwrap().clone();

        assert_eq!(collector.key_id, before.key_id);
        assert_ne!(response.key_id, before.key_id);
        assert!(after.key_history.iter().any(|record| {
            record.key_id == before.key_id
                && record.private_key_protected_hex == before.private_key_protected_hex
        }));

        let plaintext = export_bytes("轮换后的历史密钥仍可解密", Uuid::new_v4(), "batch-history");
        let export: archive_domain::ClientExportV1 = serde_json::from_slice(&plaintext).unwrap();
        let encrypted =
            enterprise_crypto::encrypt(&decode_hex(&before.public_key_hex).unwrap(), &plaintext)
                .unwrap();
        let package = archive_transfer::create_enterprise_package(
            &export,
            &before.organization_id,
            &before.key_id,
            &encrypted.wrapped_dek,
            &encrypted.nonce,
            &encrypted.ciphertext,
            &encrypted.tag,
        )
        .unwrap();
        let Json(imported) =
            import_enterprise(State(app_state), authorized_headers(), Body::from(package))
                .await
                .unwrap();
        assert_eq!(imported.inserted, 1);
    }

    #[tokio::test]
    async fn rejects_invalid_schema_and_modified_hash() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let bytes = export_bytes("原始内容", Uuid::new_v4(), "batch-a");

        let mut invalid_schema: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        invalid_schema["schema_version"] = json!("client-export.invalid");
        let schema_error = import_json(
            State(app_state.clone()),
            authorized_headers(),
            Body::from(serde_json::to_vec(&invalid_schema).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(schema_error.status, StatusCode::UNPROCESSABLE_ENTITY);

        let mut modified_hash: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        modified_hash["content_sha256"] = json!("00");
        let hash_error = import_json(
            State(app_state),
            authorized_headers(),
            Body::from(serde_json::to_vec(&modified_hash).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(hash_error.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn repeated_import_is_idempotent() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let bytes = export_bytes("同一内容", Uuid::new_v4(), "batch-a");

        let first = import_json(
            State(app_state.clone()),
            authorized_headers(),
            Body::from(bytes.clone()),
        )
        .await
        .unwrap();
        assert_eq!(first.inserted, 1);
        let repeated = import_json(State(app_state), authorized_headers(), Body::from(bytes))
            .await
            .unwrap();
        assert_eq!(repeated.inserted, 0);
        assert_eq!(repeated.unchanged, 1);
        assert_eq!(repeated.revised, 0);
    }

    #[tokio::test]
    async fn rejects_conflicting_batch_identifier() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        let batch_id = Uuid::new_v4();
        let first = export_bytes("第一份内容", batch_id, "batch-a");
        let conflict = export_bytes("冲突内容", batch_id, "batch-b");
        let _ = import_json(
            State(app_state.clone()),
            authorized_headers(),
            Body::from(first),
        )
        .await
        .unwrap();
        let error = import_json(State(app_state), authorized_headers(), Body::from(conflict))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.body.code, "BATCH_CONFLICT");
    }

    #[tokio::test]
    async fn rejects_empty_and_oversized_bodies_without_partial_files() {
        let directory = TestDirectory::new();
        let empty = receive_json_with_limit(&directory.0, Body::empty(), 8)
            .await
            .unwrap_err();
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
        let oversized = receive_json_with_limit(&directory.0, Body::from("123456789"), 8)
            .await
            .unwrap_err();
        assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
        let import_root = directory.0.join("imports");
        assert_eq!(std::fs::read_dir(import_root).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn creates_authenticated_server_export_file() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
        enable_super_admin(&app_state);
        let bytes = export_bytes("服务端导出内容", Uuid::new_v4(), "batch-a");
        let _ = import_json(
            State(app_state.clone()),
            authorized_headers(),
            Body::from(bytes),
        )
        .await
        .unwrap();
        let result = create_export_job(
            State(app_state),
            authorized_headers(),
            Json(ExportRequest {
                scope: ExportScope::EntireArchive,
                format: ExportFormat::Json,
                conversation_id: None,
                participant_id: None,
                text: None,
                message_type: None,
                media_only: None,
                data_redaction: false,
                simplify: false,
                pretty: false,
                conversation_order: ExportOrder::Descending,
                message_order: ExportOrder::Descending,
            }),
        )
        .await
        .unwrap();
        assert_eq!(result.message_count, 1);
        assert!(
            directory
                .0
                .join("exports")
                .join(&result.file_name)
                .is_file()
        );
        assert_eq!(
            std::path::Path::new(&result.file_name)
                .extension()
                .and_then(|value| value.to_str()),
            Some("json")
        );
        assert!(
            !directory
                .0
                .join("exports")
                .join("checksums.sha256")
                .exists()
        );
        assert!(!result.manifest_sha256.is_empty());
    }

    #[test]
    fn concurrent_media_persistence_is_idempotent() {
        let directory = TestDirectory::new();
        let bytes = b"shared media";
        let blob = MediaBlobV1 {
            content_hash: encode_hex(&Sha256::digest(bytes)),
            original_name: Some("shared.bin".into()),
            mime_type: Some("application/octet-stream".into()),
            size_bytes: bytes.len() as u64,
            content_hex: encode_hex(bytes),
        };
        let root = Arc::new(directory.0.clone());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                let blob = blob.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    persist_media_blobs(&root, &[blob])
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(
            std::fs::read(directory.0.join("media").join(&blob.content_hash)).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read_dir(directory.0.join("media"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn export_options_reach_both_writers_and_keep_full_local_json_importable() {
        let directory = TestDirectory::new();
        let mut app_state = state(&directory);
        enable_super_admin(&app_state);
        let bytes = export_bytes("普通消息\n第二行", Uuid::new_v4(), "fixture");
        let export: ClientExportV1 = serde_json::from_slice(&bytes).unwrap();
        app_state.local_collector = Some(Arc::new(move |_, _, _, _, _| {
            let mut snapshot = export.clone();
            snapshot.export_id = Uuid::new_v4();
            Ok(snapshot)
        }));
        let _ = import_json(
            State(app_state.clone()),
            authorized_headers(),
            Body::from(bytes),
        )
        .await
        .unwrap();
        for simplify in [false, true] {
            for pretty in [false, true] {
                for local in [false, true] {
                    let payload = json!({"scope": "entire_archive", "format": "json", "simplify": simplify, "pretty": pretty});
                    let response = if local {
                        create_local_export(
                            State(app_state.clone()),
                            authorized_headers(),
                            Json(serde_json::from_value(payload).unwrap()),
                        )
                        .await
                        .unwrap()
                    } else {
                        create_export_job(
                            State(app_state.clone()),
                            authorized_headers(),
                            Json(serde_json::from_value(payload).unwrap()),
                        )
                        .await
                        .unwrap()
                    };
                    let path = directory.0.join("exports").join(&response.file_name);
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert_eq!(text.contains('\n'), pretty);
                    assert_eq!(response.message_count, 1);
                    let document: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if simplify {
                        let conversation = &document["conversations"][0];
                        assert_eq!(conversation["conversation"], "测试会话");
                        assert_eq!(conversation["messages"][0]["sender"], "测试成员");
                        assert_eq!(conversation["messages"][0]["content"], "普通消息\n第二行");
                        assert!(!text.contains("participant-fixture"));
                        assert!(!text.contains("raw_payload"));
                        assert!(!text.contains("participants"));
                    } else if local {
                        assert_eq!(read_json(&path).unwrap().message_count, 1);
                    } else {
                        assert_eq!(
                            document["conversations"][0]["conversation_id"],
                            "conversation-fixture"
                        );
                        assert_eq!(
                            document["conversations"][0]["messages"][0]["stable_message_id"],
                            "stable-message-1"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_export_requests_default_to_full_compact_output() {
        let local: LocalExportRequest = serde_json::from_value(json!({"format":"json"})).unwrap();
        let server: ExportRequest =
            serde_json::from_value(json!({"scope":"entire_archive", "format":"json"})).unwrap();
        assert!(!local.simplify && !local.pretty && !server.simplify && !server.pretty);
    }

    #[test]
    fn data_redaction_removes_plaintext_values() {
        let export: archive_domain::ClientExportV1 = serde_json::from_slice(&export_bytes(
            "账号 admin 密码 secret",
            Uuid::new_v4(),
            "fixture",
        ))
        .unwrap();
        let mut messages = export.batches[0].messages.clone();
        messages[0].raw_payload = serde_json::json!({"password": "secret-456"});
        redact_sensitive_messages(&mut messages);

        assert_eq!(
            messages[0].body_text.as_deref(),
            Some("账号 admin 密码 [敏感数据已脱敏]")
        );
        assert!(!serde_json::to_string(&messages).unwrap().contains("secret"));
    }
}
