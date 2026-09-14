use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use archive_domain::MessageType;
use archive_export::{
    ExportFormat, ExportMedia, ExportPackage, ExportParticipant, ExportScope, export_package,
};
use archive_store::{ArchiveStore, IngestSummary, MessageQuery, StoreError};
use archive_transfer::read_json;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
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
const MAX_CLIENT_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ENTERPRISE_CONFIG_FILE: &str = "enterprise-config.json";
const ACCESS_TOKEN_FILE: &str = "access-token.dpapi";
const DEFAULT_COLLECTION_NOTICE: &str = "加密传输本机企业微信聊天记录到服务端";
const LEGACY_COLLECTION_NOTICE: &str = "仅处理您有权归档的数据。";

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

#[derive(Clone)]
struct AppState {
    data_root: Arc<PathBuf>,
    archive_path: Arc<PathBuf>,
    token_sha256: Arc<Mutex<[u8; 32]>>,
    access_token_path: Arc<PathBuf>,
    enterprise_config: Arc<Mutex<EnterpriseConfig>>,
    collector_template: Option<Arc<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseConfig {
    organization_id: String,
    organization_name: String,
    collection_notice: String,
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
    public_key_hex: String,
    private_key_protected_hex: String,
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
struct EnterpriseConfigRequest {
    organization_name: String,
    collection_notice: String,
    key_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnterpriseConfigResponse {
    configured: bool,
    organization_id: String,
    organization_name: String,
    collection_notice: String,
    key_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectorResponse {
    file_name: String,
    organization_id: String,
    key_id: String,
    collector_id: String,
    artifact: String,
    executable_generated: bool,
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    code: &'static str,
    message: &'static str,
    recoverable: bool,
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    limit: Option<u32>,
    offset: Option<u64>,
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportResponse {
    export_id: String,
    file_name: String,
    message_count: u64,
    media_count: u64,
    missing_media_count: u64,
    manifest_sha256: String,
}

struct PreparedServer {
    address: SocketAddr,
    token: Zeroizing<String>,
    router: Router,
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

pub fn start_desktop_server() -> Result<DesktopServer, String> {
    let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
    let collector_template = std::env::current_exe().ok();
    let prepared = prepare_server(collector_template)?;
    let page_url = format!("http://{}/#token={}", prepared.address, &*prepared.token);
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("wecom-archive-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "无法初始化本机服务运行时。".to_owned())?;
            runtime.block_on(serve(
                prepared,
                shutdown_receiver,
                Some(ready_sender),
                false,
            ))
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

pub fn run_standalone() {
    let (_shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
    let result = prepare_server(find_collector_template()).and_then(|prepared| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| "无法初始化服务端运行时。".to_owned())?;
        runtime.block_on(serve(prepared, shutdown_receiver, None, true))
    });
    if let Err(error) = result {
        show_startup_error(&format!("{error}\n\n服务端即将退出。\n"));
        std::process::exit(2);
    }
}

fn prepare_server(collector_template: Option<PathBuf>) -> Result<PreparedServer, String> {
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
        .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787));
    if !address.ip().is_loopback() {
        return Err("服务端只允许监听本机回环地址。".into());
    }

    let state = AppState {
        data_root: Arc::new(data_root.clone()),
        archive_path: Arc::new(data_root.join("archive.db")),
        token_sha256: Arc::new(Mutex::new(Sha256::digest(token.as_bytes()).into())),
        access_token_path: Arc::new(access_token_path),
        enterprise_config: Arc::new(Mutex::new(load_enterprise_config(&data_root))),
        collector_template: collector_template.map(Arc::new),
    };
    ArchiveStore::open(&state.archive_path)
        .map_err(|error| format!("归档数据库初始化失败：{error}"))?;

    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/imports/json", post(import_json))
        .route("/api/v1/imports/enterprise", post(import_enterprise))
        .route("/api/v1/server/access-token", post(update_access_token))
        .route(
            "/api/v1/server/access-token/regenerate",
            post(regenerate_access_token),
        )
        .route(
            "/api/v1/enterprise/config",
            get(get_enterprise_config).put(update_enterprise_config),
        )
        .route("/api/v1/enterprise/key/rotate", post(rotate_enterprise_key))
        .route("/api/v1/enterprise/collectors", post(generate_collector))
        .route("/api/v1/archive/summary", get(archive_summary))
        .route("/api/v1/conversations", get(list_conversations))
        .route("/api/v1/messages", get(list_messages))
        .route(
            "/api/v1/exports",
            post(create_export_job).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
        .fallback(embedded_web);

    Ok(PreparedServer {
        address,
        token,
        router: app,
    })
}

async fn serve(
    prepared: PreparedServer,
    shutdown_receiver: std::sync::mpsc::Receiver<()>,
    ready_sender: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    open_browser: bool,
) -> Result<(), String> {
    let listener = match tokio::net::TcpListener::bind(prepared.address).await {
        Ok(listener) => listener,
        Err(error) => {
            let message = format!(
                "服务端启动失败（{}）。请确认 8787 端口未被占用。",
                error.kind()
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
    if open_browser {
        open_browser_with_token(prepared.address, &prepared.token);
    }
    axum::serve(listener, prepared.router)
        .with_graceful_shutdown(async move {
            let _ = tokio::task::spawn_blocking(move || shutdown_receiver.recv()).await;
        })
        .await
        .map_err(|_| "本机归档服务异常退出。".to_owned())
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

#[cfg(windows)]
fn open_browser_with_token(address: SocketAddr, token: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    let url = format!("http://{address}/#token={token}");
    let url: Vec<u16> = std::ffi::OsStr::new(&url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let operation: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
    }
}

#[cfg(not(windows))]
fn open_browser_with_token(_address: SocketAddr, _token: &str) {}

#[cfg(windows)]
fn show_startup_error(message: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    let text: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = std::ffi::OsStr::new("企业微信记录归档")
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, title: *const u16, kind: u32) -> i32;
    }
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
    }
}

#[cfg(not(windows))]
fn show_startup_error(message: &str) {
    eprintln!("{message}");
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
        key_id: format!("key-{}", Uuid::new_v4().simple()),
        public_key_hex,
        private_key_protected_hex,
        signing_public_key_hex,
        signing_private_key_protected_hex,
        key_history: Vec::new(),
        collectors: Vec::new(),
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
    Ok(Json(EnterpriseConfigResponse {
        configured: !config.organization_name.trim().is_empty(),
        organization_id: config.organization_id,
        organization_name: config.organization_name,
        collection_notice: config.collection_notice,
        key_id: config.key_id,
    }))
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
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    if let Some(key_id) = request.key_id.filter(|value| !value.trim().is_empty()) {
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
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(EnterpriseConfigResponse {
        configured: true,
        organization_id: config.organization_id.clone(),
        organization_name: config.organization_name.clone(),
        collection_notice: config.collection_notice.clone(),
        key_id: config.key_id.clone(),
    }))
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
    Ok(Json(EnterpriseConfigResponse {
        configured: !config.organization_name.trim().is_empty(),
        organization_id: config.organization_id.clone(),
        organization_name: config.organization_name.clone(),
        collection_notice: config.collection_notice.clone(),
        key_id: config.key_id.clone(),
    }))
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
    let signing_private = dpapi_protect::unprotect(
        &decode_hex(&config.signing_private_key_protected_hex).map_err(|_| ApiError::store())?,
    )
    .map_err(|_| ApiError::store())?;
    let collector_id = Uuid::new_v4().to_string();
    let key_id = config.key_id.clone();
    let public_key_hex = config.public_key_hex.clone();
    let signing_payload = collector_signing_payload(&config, &key_id, &public_key_hex);
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
        "offlineOnly": true,
        "formats": ["enterprise-package.v1"],
    });
    let artifact_text = serde_json::to_string_pretty(&artifact).map_err(|_| ApiError::store())?;
    let file_name = format!(
        "WeComArchiveCollector-{}-{}-{}.exe",
        sanitize_file_component(&config.organization_id),
        sanitize_file_component(&key_id),
        sanitize_file_component(&collector_id)
    );
    let collector_root = state.data_root.join("collectors");
    std::fs::create_dir_all(&collector_root).map_err(|_| ApiError::store())?;
    let config_name = format!("{file_name}.wca-collector");
    let partial = collector_root.join(format!("{config_name}.partial"));
    std::fs::write(&partial, artifact_text.as_bytes()).map_err(|_| ApiError::store())?;
    let config_path = collector_root.join(&config_name);
    std::fs::rename(&partial, &config_path).map_err(|_| ApiError::store())?;
    let executable_path = collector_root.join(&file_name);
    let executable_generated = state
        .collector_template
        .as_deref()
        .filter(|template| template.is_file())
        .cloned()
        .or_else(find_collector_template)
        .is_some_and(|template| std::fs::copy(template, &executable_path).is_ok());
    if executable_generated {
        config.collectors.push(EnterpriseCollectorRecord {
            collector_id: collector_id.clone(),
            key_id: key_id.clone(),
            created_at: Utc::now().to_rfc3339(),
            public_key_hex: String::new(),
            private_key_protected_hex: String::new(),
        });
        if let Err(error) = persist_enterprise_config(&state.data_root, &config) {
            config
                .collectors
                .retain(|collector| collector.collector_id != collector_id);
            let _ = std::fs::remove_file(&executable_path);
            let _ = std::fs::remove_file(&config_path);
            return Err(error);
        }
    } else {
        let _ = std::fs::remove_file(config_path);
    }
    Ok(Json(CollectorResponse {
        file_name,
        organization_id: config.organization_id.clone(),
        key_id,
        collector_id,
        artifact: artifact_text,
        executable_generated,
    }))
}

fn collector_signing_payload(
    config: &EnterpriseConfig,
    key_id: &str,
    public_key_hex: &str,
) -> String {
    format!(
        "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n",
        config.organization_id,
        config.organization_name,
        config.collection_notice,
        key_id,
        public_key_hex,
        "aes-256-gcm+rsa-oaep-sha256"
    )
}

fn find_collector_template() -> Option<PathBuf> {
    let beside_server = std::env::current_exe()
        .ok()?
        .parent()?
        .join("WeComArchive.exe");
    if beside_server.is_file() {
        return Some(beside_server);
    }
    std::env::current_dir()
        .ok()
        .map(|root| root.join("portable-client").join("WeComArchive.exe"))
        .filter(|path| path.is_file())
}

async fn import_enterprise(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ImportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
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
    ingest_export(&state, export).await
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
    let export_id = export.export_id.to_string();
    let batch_count = export.batches.len();
    let archive_path = Arc::clone(&state.archive_path);
    let conversations = export.conversations;
    let participants = export.participants;
    let batches = export.batches;
    let summary = tokio::task::spawn_blocking(move || {
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
        Ok::<_, ApiError>(total)
    })
    .await
    .map_err(|_| ApiError::store())??;

    Ok(Json(ImportResponse {
        export_id,
        batch_count,
        inserted: summary.inserted,
        unchanged: summary.unchanged,
        revised: summary.revised,
    }))
}

async fn ingest_export(
    state: &AppState,
    export: archive_domain::ClientExportV1,
) -> Result<Json<ImportResponse>, ApiError> {
    let export_id = export.export_id.to_string();
    let batch_count = export.batches.len();
    let archive_path = Arc::clone(&state.archive_path);
    let conversations = export.conversations;
    let participants = export.participants;
    let batches = export.batches;
    let summary = tokio::task::spawn_blocking(move || {
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
        Ok::<_, ApiError>(total)
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(ImportResponse {
        export_id,
        batch_count,
        inserted: summary.inserted,
        unchanged: summary.unchanged,
        revised: summary.revised,
    }))
}

async fn archive_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<archive_store::ArchiveSummary>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let archive_path = Arc::clone(&state.archive_path);
    let summary = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.summary())
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(summary))
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
) -> Result<Json<Vec<archive_domain::MessageV1>>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
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
        limit: query.limit.unwrap_or(100),
        offset: query.offset.unwrap_or(0),
    };
    let archive_path = Arc::clone(&state.archive_path);
    let messages = tokio::task::spawn_blocking(move || {
        ArchiveStore::open_read_only(&archive_path)
            .and_then(|store| store.query_messages(&message_query))
            .map_err(|_| ApiError::store())
    })
    .await
    .map_err(|_| ApiError::store())??;
    Ok(Json(messages))
}

async fn create_export_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExportRequest>,
) -> Result<Json<ExportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
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
        limit: 500,
        offset: 0,
    };
    let archive_path = Arc::clone(&state.archive_path);
    let data_root = Arc::clone(&state.data_root);
    let format = request.format;
    let scope = request.scope;
    let result = tokio::task::spawn_blocking(move || {
        let store = ArchiveStore::open_read_only(&archive_path).map_err(ApiError::from_store)?;
        let messages = query_all_messages(&store, query).map_err(ApiError::from_store)?;
        let participants = messages
            .iter()
            .filter_map(|message| message.sender_id.as_ref())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|participant_id| ExportParticipant {
                participant_id: participant_id.clone(),
                display_name: None,
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
        let file_name = format!(
            "archive-export-{}-{}.zip",
            Utc::now().format("%Y%m%d-%H%M%S"),
            export_id.simple()
        );
        let target = export_root.join(&file_name);
        let package = ExportPackage {
            archive_id: "default".into(),
            scope,
            format,
            messages,
            participants,
            media,
            generated_at: Utc::now(),
            edge_executable: find_edge_executable(),
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
        message_count: result.1.message_count,
        media_count: result.1.media_count,
        missing_media_count: result.1.missing_media_count,
        manifest_sha256: result.1.manifest_sha256,
    }))
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
            return Ok(messages);
        }
    }
}

fn find_edge_executable() -> Option<PathBuf> {
    let suffix = PathBuf::from("Microsoft")
        .join("Edge")
        .join("Application")
        .join("msedge.exe");
    ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .map(|root| root.join(&suffix))
        .find(|path| path.is_file())
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
    if bytes.len() % 2 != 0 {
        return Err(());
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or(())? as u8;
            let low = (pair[1] as char).to_digit(16).ok_or(())? as u8;
            Ok((high << 4) | low)
        })
        .collect()
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
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            body: ErrorResponse {
                code: "UNAUTHORIZED",
                message: "需要有效的服务端访问令牌。",
                recoverable: true,
            },
        }
    }

    fn invalid_export() -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ErrorResponse {
                code: "INVALID_CLIENT_EXPORT",
                message: "客户端 JSON 格式、关系或校验值无效。",
                recoverable: true,
            },
        }
    }

    fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: ErrorResponse {
                code: "CLIENT_EXPORT_TOO_LARGE",
                message: "客户端 JSON 超过服务端单次导入上限。",
                recoverable: true,
            },
        }
    }

    fn invalid_query() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: ErrorResponse {
                code: "INVALID_QUERY",
                message: "检索参数无效。",
                recoverable: true,
            },
        }
    }

    fn store() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorResponse {
                code: "ARCHIVE_STORE_FAILED",
                message: "服务端归档写入失败，诊断信息已脱敏。",
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
                    message: "同一采集批次标识已存在，但内容校验值不同。",
                    recoverable: false,
                },
            }
        } else {
            Self::store()
        }
    }

    fn from_export(error: archive_export::ExportError) -> Self {
        if matches!(error, archive_export::ExportError::PdfRendererUnavailable) {
            Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: ErrorResponse {
                    code: "PDF_RENDERER_UNAVAILABLE",
                    message: "未找到可用的本机 Edge PDF 渲染器。",
                    recoverable: true,
                },
            }
        } else {
            Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: ErrorResponse {
                    code: "ARCHIVE_EXPORT_FAILED",
                    message: "服务端导出失败；未完成文件已清理。",
                    recoverable: true,
                },
            }
        }
    }
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
        MessageDirection, MessageType, MessageV1, ParticipantV1, RetentionDirective,
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

        let before = app_state.enterprise_config.lock().unwrap().clone();
        let Json(response) = generate_collector(State(app_state.clone()), authorized_headers())
            .await
            .unwrap();

        assert!(response.executable_generated);
        assert_eq!(response.key_id, before.key_id);
        let artifact: serde_json::Value = serde_json::from_str(&response.artifact).unwrap();
        assert_eq!(artifact["keyId"], before.key_id);
        assert_eq!(artifact["publicKeyHex"], before.public_key_hex);
        let after = app_state.enterprise_config.lock().unwrap().clone();
        assert!(after.collectors.iter().any(|collector| {
            collector.collector_id == response.collector_id
                && collector.key_id == before.key_id
                && collector.private_key_protected_hex.is_empty()
        }));
        let executable = directory.0.join("collectors").join(&response.file_name);
        assert_eq!(
            std::fs::read(executable).unwrap(),
            b"single executable template"
        );
        assert!(
            directory
                .0
                .join("collectors")
                .join(format!("{}.wca-collector", response.file_name))
                .is_file()
        );
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
    async fn creates_authenticated_server_export_zip() {
        let directory = TestDirectory::new();
        let app_state = state(&directory);
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
        assert!(!result.manifest_sha256.is_empty());
    }
}
