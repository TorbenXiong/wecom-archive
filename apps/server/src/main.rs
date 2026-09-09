use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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

const TOKEN_ENV: &str = "WECOM_ARCHIVE_SERVER_TOKEN";
const DATA_ROOT_ENV: &str = "WECOM_ARCHIVE_SERVER_DATA";
const LISTEN_ENV: &str = "WECOM_ARCHIVE_SERVER_LISTEN";
const MAX_CLIENT_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

#[derive(Clone)]
struct AppState {
    data_root: Arc<PathBuf>,
    archive_path: Arc<PathBuf>,
    token_sha256: [u8; 32],
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

#[tokio::main]
async fn main() {
    let token = Zeroizing::new(std::env::var(TOKEN_ENV).unwrap_or_else(|_| {
        eprintln!("missing required server access token environment variable");
        std::process::exit(2);
    }));
    if token.len() < 24 {
        eprintln!("server access token must be at least 24 bytes");
        std::process::exit(2);
    }
    let data_root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_data_root);
    if let Err(error) = std::fs::create_dir_all(&data_root) {
        eprintln!("server data directory is unavailable: {}", error.kind());
        std::process::exit(3);
    }
    let address = std::env::var(LISTEN_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787));
    if !address.ip().is_loopback() {
        eprintln!("refusing non-loopback binding without a TLS reverse proxy configuration");
        std::process::exit(4);
    }

    let state = AppState {
        data_root: Arc::new(data_root.clone()),
        archive_path: Arc::new(data_root.join("archive.db")),
        token_sha256: Sha256::digest(token.as_bytes()).into(),
    };
    if let Err(error) = ArchiveStore::open(&state.archive_path) {
        eprintln!("archive database initialization failed: {error}");
        std::process::exit(5);
    }

    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/imports/json", post(import_json))
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

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .unwrap_or_else(|error| {
            eprintln!("server listener failed: {}", error.kind());
            std::process::exit(6);
        });
    println!("archive server listening on http://{address}");
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|_| std::process::exit(7));
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

async fn embedded_web(uri: Uri) -> Response {
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

fn authorize(headers: &HeaderMap, expected: &[u8; 32]) -> Result<(), ApiError> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(ApiError::unauthorized());
    };
    let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    let difference = actual
        .iter()
        .zip(expected)
        .fold(0_u8, |state, (left, right)| state | (left ^ right));
    if difference == 0 {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
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
            token_sha256: Sha256::digest(TEST_TOKEN.as_bytes()).into(),
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
