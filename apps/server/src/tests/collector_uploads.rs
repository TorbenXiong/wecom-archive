use super::*;
use axum::body::Bytes;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::time::Duration;

const DEVICE_A: &str = "synthetic-device-a";
const DEVICE_B: &str = "synthetic-device-b";
const TOKEN_A: &str = "synthetic-upload-token-a";
const TOKEN_B: &str = "synthetic-upload-token-b";

struct Package {
    bytes: Vec<u8>,
    export_id: Uuid,
    hash: String,
}

fn devices(state: &AppState) {
    let mut config = state.enterprise_config.lock().unwrap();
    for (id, token) in [(DEVICE_A, TOKEN_A), (DEVICE_B, TOKEN_B)] {
        let record = serde_json::from_value(serde_json::json!({
            "collector_id": id, "key_id": config.key_id,
            "created_at": "2026-01-02T03:04:05Z", "public_key_hex": config.public_key_hex,
            "private_key_protected_hex": config.private_key_protected_hex,
            "upload_token_sha256": encode_hex(&Sha256::digest(token.as_bytes())),
            "schedule": CollectionSchedule::default(), "enabled": true
        }))
        .unwrap();
        config.collectors.push(record);
    }
}

fn package(state: &AppState, text: &str) -> Package {
    let bytes = export_bytes(text, Uuid::new_v4(), "synthetic-upload");
    let export: ClientExportV1 = serde_json::from_slice(&bytes).unwrap();
    let config = state.enterprise_config.lock().unwrap().clone();
    let public_key = decode_hex(&config.public_key_hex).unwrap();
    let encrypted = enterprise_crypto::encrypt(&public_key, &bytes).unwrap();
    let bytes = archive_transfer::create_enterprise_package(
        &export,
        &config.organization_id,
        &config.key_id,
        &encrypted.wrapped_dek,
        &encrypted.nonce,
        &encrypted.ciphertext,
        &encrypted.tag,
    )
    .unwrap();
    let metadata: Value = serde_json::from_slice(&bytes).unwrap();
    Package {
        bytes,
        export_id: export.export_id,
        hash: metadata["plaintext_sha256"].as_str().unwrap().into(),
    }
}

fn device_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

fn upload_headers(token: &str, package: &Package) -> HeaderMap {
    let mut headers = device_headers(token);
    headers.insert(
        "x-collector-export-id",
        package.export_id.to_string().parse().unwrap(),
    );
    headers.insert("x-collector-content-sha256", package.hash.parse().unwrap());
    headers
}

async fn status(state: &AppState, token: &str, id: Uuid) -> Result<Value, ApiError> {
    crate::collector_uploads::status(
        State(state.clone()),
        device_headers(token),
        AxumPath(id.to_string()),
    )
    .await
    .map(|value| serde_json::to_value(value.0).unwrap())
}

async fn wait_for(
    state: &AppState,
    token: &str,
    id: Uuid,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(value) = status(state, token, id).await
                && predicate(&value)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("COLLECTOR_UPLOAD_STATE_TIMEOUT")
}

async fn bounded_start(
    state: State<AppState>,
    headers: HeaderMap,
    id: AxumPath<String>,
    body: Body,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    tokio::time::timeout(
        Duration::from_secs(8),
        crate::collector_uploads::start(state, headers, id, body),
    )
    .await
    .expect("COLLECTOR_UPLOAD_ACCEPT_TIMEOUT")
}

async fn start(state: &AppState, token: &str, id: Uuid, package: &Package) {
    let (code, Json(accepted)) = bounded_start(
        State(state.clone()),
        upload_headers(token, package),
        AxumPath(id.to_string()),
        Body::from(package.bytes.clone()),
    )
    .await
    .unwrap();
    assert_eq!(code, StatusCode::ACCEPTED);
    assert_eq!(
        accepted,
        serde_json::json!({"schema":"collector-upload.v1", "uploadId":id.to_string(), "status":"accepted"})
    );
}

fn assert_clean(state: &AppState) {
    assert_eq!(state.import_slots.available_permits(), 1);
    assert_eq!(
        std::fs::read_dir(state.data_root.join("imports"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn device_upload_results_bind_payload_and_are_isolated_between_devices() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let first = package(&state, "synthetic first device upload");
    let id = Uuid::new_v4();
    start(&state, TOKEN_A, id, &first).await;
    let result = wait_for(&state, TOKEN_A, id, |value| value["status"] == "completed").await;
    assert_eq!(result["schema"], "collector-upload.v1");
    assert_eq!(result["uploadId"], id.to_string());
    assert_eq!(result["exportId"], first.export_id.to_string());
    assert_eq!(result["contentSha256"], first.hash);
    assert_eq!(result["receivedBytes"], first.bytes.len());
    assert_eq!(result["result"]["inserted"], 1);
    assert_eq!(result["result"]["exportId"], first.export_id.to_string());
    assert_clean(&state);

    let other_device = status(&state, TOKEN_B, id).await.unwrap_err();
    assert_eq!(other_device.status, StatusCode::NOT_FOUND);
    assert_eq!(other_device.body.code, "COLLECTOR_UPLOAD_NOT_FOUND");
    let workspace = crate::tasks::status(
        State(state.clone()),
        authorized_headers(),
        AxumPath(id.to_string()),
    )
    .await
    .unwrap_err();
    assert_eq!(workspace.body.code, "TASK_NOT_FOUND");

    let second = package(&state, "synthetic second device upload");
    start(&state, TOKEN_B, id, &second).await;
    let other = wait_for(&state, TOKEN_B, id, |value| value["status"] == "completed").await;
    assert_eq!(other["exportId"], second.export_id.to_string());
    assert_eq!(status(&state, TOKEN_A, id).await.unwrap(), result);
    assert_clean(&state);
    ArchiveStore::open_read_only(&state.archive_path)
        .unwrap()
        .verify_integrity()
        .unwrap();
}

#[tokio::test]
async fn duplicate_upload_ids_do_not_consume_or_replace_a_new_payload() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let package = package(&state, "synthetic original upload");
    let id = Uuid::new_v4();
    start(&state, TOKEN_A, id, &package).await;
    let original = wait_for(&state, TOKEN_A, id, |value| value["status"] == "completed").await;
    let consumed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&consumed);
    let body = Body::from_stream(futures_util::stream::once(async move {
        observed.store(true, Ordering::Release);
        Ok::<_, std::io::Error>(Bytes::from_static(b"must not read"))
    }));
    let error = bounded_start(
        State(state.clone()),
        upload_headers(TOKEN_A, &package),
        AxumPath(id.to_string()),
        body,
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.body.code, "COLLECTOR_UPLOAD_EXISTS");
    assert!(!consumed.load(Ordering::Acquire));
    assert_eq!(status(&state, TOKEN_A, id).await.unwrap(), original);
    assert_clean(&state);
}

#[tokio::test]
async fn mismatched_upload_identity_fails_before_any_archive_write() {
    for wrong_export_id in [false, true] {
        let directory = TestDirectory::new();
        let state = state(&directory);
        devices(&state);
        let package = package(&state, "synthetic bound upload");
        let mut headers = upload_headers(TOKEN_A, &package);
        if wrong_export_id {
            headers.insert(
                "x-collector-export-id",
                Uuid::new_v4().to_string().parse().unwrap(),
            );
        } else {
            headers.insert(
                "x-collector-content-sha256",
                "0".repeat(64).parse().unwrap(),
            );
        }
        let id = Uuid::new_v4();
        let (code, _) = bounded_start(
            State(state.clone()),
            headers,
            AxumPath(id.to_string()),
            Body::from(package.bytes),
        )
        .await
        .unwrap();
        assert_eq!(code, StatusCode::ACCEPTED);
        let failed = wait_for(&state, TOKEN_A, id, |value| value["status"] == "failed").await;
        assert_eq!(failed["errorCode"], "INVALID_CLIENT_EXPORT");
        assert!(failed["result"].is_null());
        assert!(!state.archive_path.exists());
        assert!(
            state.enterprise_config.lock().unwrap().collectors[0]
                .last_upload_at
                .is_none()
        );
        assert_clean(&state);
    }
}

#[tokio::test]
async fn invalid_metadata_and_workspace_credentials_are_rejected_before_reading() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let package = package(&state, "synthetic invalid metadata");
    let id = Uuid::new_v4();
    for (name, value) in [
        ("x-collector-export-id", "invalid".to_owned()),
        ("x-collector-export-id", Uuid::nil().to_string()),
        ("x-collector-content-sha256", "A".repeat(64)),
        ("x-collector-content-sha256", "a".repeat(63)),
    ] {
        let mut headers = upload_headers(TOKEN_A, &package);
        headers.insert(name, value.parse().unwrap());
        let error = bounded_start(
            State(state.clone()),
            headers,
            AxumPath(id.to_string()),
            Body::from("{}"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
    let error = bounded_start(
        State(state.clone()),
        upload_headers(TOKEN_A, &package),
        AxumPath(Uuid::nil().to_string()),
        Body::from("{}"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    let denied = bounded_start(
        State(state.clone()),
        authorized_headers(),
        AxumPath(id.to_string()),
        Body::from("{}"),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.body.code, "COLLECTOR_IDENTITY_REQUIRED");
    let denied = crate::collector_uploads::status(
        State(state.clone()),
        authorized_headers(),
        AxumPath(id.to_string()),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(!directory.0.join("imports").exists());
    let missing = status(&state, TOKEN_A, id).await.unwrap_err();
    assert_eq!(missing.body.code, "COLLECTOR_UPLOAD_NOT_FOUND");
}

#[tokio::test]
async fn device_status_queries_recheck_disabled_and_revoked_credentials() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let package = package(&state, "synthetic authorization upload");
    let id = Uuid::new_v4();
    start(&state, TOKEN_A, id, &package).await;
    wait_for(&state, TOKEN_A, id, |value| value["status"] == "completed").await;
    state.enterprise_config.lock().unwrap().collectors[0].enabled = false;
    assert_eq!(
        status(&state, TOKEN_A, id).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
    {
        let mut config = state.enterprise_config.lock().unwrap();
        config.collectors[0].enabled = true;
        config.collectors[0].revoked_at = Some("2026-01-02T03:04:05Z".into());
    }
    assert_eq!(
        status(&state, TOKEN_A, id).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&state, "synthetic-wrong-token", id)
            .await
            .unwrap_err()
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_clean(&state);
}

#[tokio::test]
async fn start_waits_for_eof_and_cancelled_request_still_finishes_its_received_payload() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let package = package(&state, "synthetic detached upload");
    let expected_bytes = package.bytes.len() as u64;
    let id = Uuid::new_v4();
    let (send, receive) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let body = Body::from_stream(futures_util::stream::unfold(receive, |mut receive| async {
        receive.recv().await.map(|chunk| (chunk, receive))
    }));
    let request_state = state.clone();
    let headers = upload_headers(TOKEN_A, &package);
    let request = tokio::spawn(async move {
        bounded_start(
            State(request_state),
            headers,
            AxumPath(id.to_string()),
            body,
        )
        .await
    });
    send.send(Ok(Bytes::from(package.bytes))).await.unwrap();
    wait_for(&state, TOKEN_A, id, |value| {
        value["receivedBytes"] == expected_bytes
    })
    .await;
    assert!(!request.is_finished(), "UPLOAD_ACK_MUST_WAIT_FOR_EOF");
    assert_eq!(state.import_slots.available_permits(), 0);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(state.import_slots.available_permits(), 0);
    drop(send);
    let completed = wait_for(&state, TOKEN_A, id, |value| value["status"] == "completed").await;
    assert_eq!(completed["result"]["inserted"], 1);
    assert_eq!(
        state.enterprise_config.lock().unwrap().collectors[0]
            .last_upload_at
            .as_deref(),
        completed["result"]["completedAt"].as_str()
    );
    assert_clean(&state);
}

#[tokio::test]
async fn per_device_active_limits_do_not_consume_other_device_or_workspace_slots() {
    let directory = TestDirectory::new();
    let state = state(&directory);
    devices(&state);
    let package = package(&state, "synthetic queued device upload");
    let held = acquire_import_slot(&state, Duration::from_secs(1))
        .await
        .unwrap();
    let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let mut requests = Vec::new();
    for (token, id) in [(TOKEN_A, ids[0]), (TOKEN_A, ids[1]), (TOKEN_B, ids[2])] {
        let request_state = state.clone();
        let headers = upload_headers(token, &package);
        let body = Body::from(package.bytes.clone());
        requests.push(tokio::spawn(async move {
            bounded_start(
                State(request_state),
                headers,
                AxumPath(id.to_string()),
                body,
            )
            .await
        }));
        wait_for(&state, token, id, |value| value["status"] == "queued").await;
    }
    let error = bounded_start(
        State(state.clone()),
        upload_headers(TOKEN_A, &package),
        AxumPath(Uuid::new_v4().to_string()),
        Body::from("{}"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.body.code, "COLLECTOR_UPLOAD_BUSY");
    let mut workspace_headers = authorized_headers();
    workspace_headers.insert("x-archive-task-id", ids[0].to_string().parse().unwrap());
    assert!(
        crate::tasks::Task::register(&state, &workspace_headers, "import")
            .unwrap()
            .is_some()
    );
    assert!(!directory.0.join("imports").exists());
    drop(held);
    for request in requests {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(8), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .0,
            StatusCode::ACCEPTED
        );
    }
    for (token, id) in [(TOKEN_A, ids[0]), (TOKEN_A, ids[1]), (TOKEN_B, ids[2])] {
        wait_for(&state, token, id, |value| value["status"] == "completed").await;
    }
    assert_clean(&state);
}
