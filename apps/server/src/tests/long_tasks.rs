use super::*;
use crate::tasks::{Task, TaskProgress};
use axum::body::Bytes;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn tracked_headers(id: Uuid) -> HeaderMap {
    let mut headers = authorized_headers();
    headers.insert("x-archive-task-id", id.to_string().parse().unwrap());
    headers
}

fn collection_query() -> Query<LocalCollectionQuery> {
    Query(LocalCollectionQuery {
        include_media: Some(false),
        data_redaction: Some(true),
        source_root: None,
    })
}

async fn wait_for_progress(
    state: &AppState,
    id: Uuid,
    matches: impl Fn(&TaskProgress) -> bool,
) -> TaskProgress {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Json(progress)) = crate::tasks::status(
                State(state.clone()),
                authorized_headers(),
                AxumPath(id.to_string()),
            )
            .await
                && matches(&progress)
            {
                return progress;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("LONG_TASK_PROGRESS_TIMEOUT")
}

#[tokio::test]
async fn cancelled_collection_request_keeps_run_lock_and_commits_queryable_result() {
    let directory = TestDirectory::new();
    let mut app_state = state(&directory);
    let started = Arc::new(tokio::sync::Notify::new());
    let reported_start = Arc::clone(&started);
    let (release, wait) = std::sync::mpsc::channel();
    let wait = Mutex::new(wait);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic detached collection",
        Uuid::new_v4(),
        "synthetic-collection",
    ))
    .unwrap();
    app_state.local_collector = Some(Arc::new(move |_, _, _, _, reporter, _| {
        reporter(40, "合成采集阶段", "等待合成数据准备完成。");
        reported_start.notify_one();
        wait.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "SYNTHETIC_COLLECTION_RELEASE_TIMEOUT".to_owned())?;
        Ok(export.clone())
    }));
    let id = Uuid::new_v4();
    let request_state = app_state.clone();
    let request = tokio::spawn(async move {
        collect_local(
            State(request_state),
            tracked_headers(id),
            collection_query(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(app_state.local_collection_running.load(Ordering::Acquire));
    let active = wait_for_progress(&app_state, id, |p| p.status == "running").await;
    assert_eq!(active.kind, "collection");
    assert!(active.result.is_none());
    let error = collect_local(
        State(app_state.clone()),
        authorized_headers(),
        collection_query(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.body.code, "LOCAL_COLLECTION_RUNNING");
    release.send(()).unwrap();
    let completed = wait_for_progress(&app_state, id, |p| p.status == "completed").await;
    assert_eq!(completed.result.unwrap().inserted, 1);
    assert!(!app_state.local_collection_running.load(Ordering::Acquire));
    assert!(!app_state.local_collection_progress.lock().unwrap().running);
    let store = ArchiveStore::open_read_only(&app_state.archive_path).unwrap();
    assert_eq!(store.summary().unwrap().message_count, 1);
    store.verify_integrity().unwrap();
}

#[tokio::test]
async fn cancelled_tracked_import_request_finishes_received_payload_and_retains_result() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let id = Uuid::new_v4();
    let bytes = export_bytes(
        "synthetic detached import",
        Uuid::new_v4(),
        "synthetic-import",
    );
    let expected_bytes = bytes.len() as u64;
    let (send, receive) = tokio::sync::mpsc::channel(1);
    let body = Body::from_stream(futures_util::stream::unfold(receive, |mut receive| async {
        receive.recv().await.map(|chunk| (chunk, receive))
    }));
    let request_state = app_state.clone();
    let request =
        tokio::spawn(
            async move { import_json(State(request_state), tracked_headers(id), body).await },
        );
    send.send(Ok::<_, std::io::Error>(Bytes::from(bytes)))
        .await
        .unwrap();
    // All payload bytes have arrived; keep EOF controlled so cancellation is
    // deterministic and does not depend on the speed of SQLite or the disk.
    let received = wait_for_progress(&app_state, id, |p| p.received_bytes == expected_bytes).await;
    assert_eq!(received.status, "receiving");
    assert_eq!(app_state.import_slots.available_permits(), 0);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(app_state.import_slots.available_permits(), 0);
    drop(send);
    let completed = wait_for_progress(&app_state, id, |p| p.status == "completed").await;
    assert_eq!(completed.kind, "import");
    assert_eq!(completed.received_bytes, expected_bytes);
    assert_eq!(completed.result.unwrap().inserted, 1);
    assert_eq!(app_state.import_slots.available_permits(), 1);
    assert_eq!(
        std::fs::read_dir(directory.0.join("imports"))
            .unwrap()
            .count(),
        0
    );
    let store = ArchiveStore::open_read_only(&app_state.archive_path).unwrap();
    assert_eq!(store.summary().unwrap().message_count, 1);
    store.verify_integrity().unwrap();
}

#[tokio::test]
async fn failed_tracked_import_is_queryable_after_cleanup_and_cannot_be_resubmitted() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let id = Uuid::new_v4();
    let error = import_json(
        State(app_state.clone()),
        tracked_headers(id),
        Body::from("synthetic invalid JSON"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.body.code, "INVALID_CLIENT_EXPORT");
    let failed = wait_for_progress(&app_state, id, |p| p.status == "failed").await;
    assert_eq!(failed.error_code, Some("INVALID_CLIENT_EXPORT"));
    assert_eq!(failed.error_status, Some(422));
    assert!(failed.result.is_none());
    assert!(failed.received_bytes > 0);
    assert_eq!(app_state.import_slots.available_permits(), 1);
    assert_eq!(
        std::fs::read_dir(directory.0.join("imports"))
            .unwrap()
            .count(),
        0
    );
    assert!(!app_state.archive_path.exists());

    let duplicate = import_json(
        State(app_state.clone()),
        tracked_headers(id),
        Body::from(export_bytes("must not ingest", Uuid::new_v4(), "duplicate")),
    )
    .await
    .unwrap_err();
    assert_eq!(duplicate.body.code, "TASK_ALREADY_EXISTS");
    assert!(!app_state.archive_path.exists());
    assert_eq!(
        std::fs::read_dir(directory.0.join("imports"))
            .unwrap()
            .count(),
        0
    );
    let still_failed = wait_for_progress(&app_state, id, |p| p.status == "failed").await;
    assert_eq!(still_failed.updated_at, failed.updated_at);
}

#[tokio::test]
async fn task_identity_and_status_require_valid_authorized_requests() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    for invalid in ["invalid-task", "00000000-0000-0000-0000-000000000000"] {
        let mut headers = authorized_headers();
        headers.insert("x-archive-task-id", invalid.parse().unwrap());
        let error = import_json(State(app_state.clone()), headers, Body::from("{}"))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.body.code, "INVALID_TASK_ID");
    }
    assert_eq!(app_state.import_slots.available_permits(), 1);
    assert!(!directory.0.join("imports").exists());
    let id = Uuid::new_v4();
    let error = crate::tasks::status(
        State(app_state.clone()),
        HeaderMap::new(),
        AxumPath(id.to_string()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    let missing = crate::tasks::status(
        State(app_state),
        authorized_headers(),
        AxumPath(id.to_string()),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body.code, "TASK_NOT_FOUND");
}

#[tokio::test]
async fn queued_import_identity_rejects_duplicates_without_consuming_another_slot() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let permit = acquire_import_slot(&app_state, Duration::from_secs(1))
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let request_state = app_state.clone();
    let request = tokio::spawn(async move {
        import_json(
            State(request_state),
            tracked_headers(id),
            Body::from(export_bytes("queued import", Uuid::new_v4(), "queued")),
        )
        .await
    });
    wait_for_progress(&app_state, id, |p| p.status == "queued").await;
    let duplicate = import_json(
        State(app_state.clone()),
        tracked_headers(id),
        Body::from("{}"),
    )
    .await
    .unwrap_err();
    assert_eq!(duplicate.body.code, "TASK_ALREADY_EXISTS");
    assert_eq!(app_state.import_slots.available_permits(), 0);
    assert!(!directory.0.join("imports").exists());
    drop(permit);
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.0.inserted, 1);
    assert_eq!(
        wait_for_progress(&app_state, id, |p| p.status == "completed")
            .await
            .result
            .unwrap()
            .inserted,
        1
    );
}

#[tokio::test]
async fn panicking_task_records_terminal_failure_and_ignores_late_progress() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let id = Uuid::new_v4();
    let task = Task::register(&app_state, &tracked_headers(id), "import")
        .unwrap()
        .unwrap();
    async fn panic_worker() -> Result<Json<ImportResponse>, ApiError> {
        panic!("SYNTHETIC_TASK_PANIC");
    }
    let error = crate::tasks::run_detached(Some(Arc::clone(&task)), panic_worker())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "ARCHIVE_STORE_FAILED");
    let failed = wait_for_progress(&app_state, id, |p| p.status == "failed").await;
    assert_eq!(failed.error_status, Some(500));
    task.update("running", "late synthetic progress", Some(100));
    let terminal = wait_for_progress(&app_state, id, |p| p.status == "failed").await;
    assert_eq!(terminal.updated_at, failed.updated_at);
    assert_eq!(terminal.stage, failed.stage);
    assert_eq!(terminal.received_bytes, failed.received_bytes);
}

#[tokio::test]
async fn tracking_capacity_preserves_existing_active_results() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let first = Uuid::new_v4();
    Task::register(&app_state, &tracked_headers(first), "import")
        .unwrap()
        .unwrap()
        .update("running", "synthetic active task", None);
    for _ in 1..128 {
        Task::register(&app_state, &tracked_headers(Uuid::new_v4()), "import").unwrap();
    }
    let error = Task::register(&app_state, &tracked_headers(Uuid::new_v4()), "import")
        .err()
        .unwrap();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.body.code, "TASK_CAPACITY_REACHED");
    assert_eq!(
        wait_for_progress(&app_state, first, |p| p.status == "running")
            .await
            .stage,
        "synthetic active task"
    );
    assert!(!directory.0.join("imports").exists());
}

#[tokio::test]
async fn collector_credentials_cannot_consume_workspace_tracking_records() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let device_token = "synthetic-device-upload-token";
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        let record = serde_json::from_value(serde_json::json!({
            "collector_id": "synthetic-device", "key_id": config.key_id,
            "created_at": "2026-01-02T03:04:05Z", "public_key_hex": config.public_key_hex,
            "private_key_protected_hex": config.private_key_protected_hex,
            "upload_token_sha256": encode_hex(&Sha256::digest(device_token.as_bytes())),
            "schedule": CollectionSchedule::default(), "enabled": true
        }))
        .unwrap();
        config.collectors.push(record);
    }
    let id = Uuid::new_v4();
    let mut headers = tracked_headers(id);
    headers.insert(
        "authorization",
        format!("Bearer {device_token}").parse().unwrap(),
    );
    let error = import_enterprise(State(app_state.clone()), headers.clone(), Body::from("{}"))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    assert!(!directory.0.join("imports").exists());
    // The refused device request must not reserve the ID for the workspace.
    let error = import_json(
        State(app_state.clone()),
        tracked_headers(id),
        Body::from("{}"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.body.code, "INVALID_CLIENT_EXPORT");
    headers.remove("x-archive-task-id");
    let legacy = import_enterprise(State(app_state), headers, Body::from("{}"))
        .await
        .unwrap_err();
    assert_eq!(legacy.body.code, "INVALID_CLIENT_EXPORT");
}

#[tokio::test]
async fn export_creation_keeps_the_preassigned_identity_and_rejects_replays() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let input: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic export identity",
        Uuid::new_v4(),
        "export-identity",
    ))
    .unwrap();
    ArchiveStore::open(&app_state.archive_path)
        .unwrap()
        .ingest_batch(&input.batches[0])
        .unwrap();
    let request = || {
        Json(
            serde_json::from_value::<ExportRequest>(
                serde_json::json!({"scope":"entire_archive","format":"json"}),
            )
            .unwrap(),
        )
    };
    let id = Uuid::new_v4();
    let started = start_export_job(State(app_state.clone()), tracked_headers(id), request())
        .await
        .unwrap()
        .0;
    assert_eq!(started.task_id, id.to_string());
    let error = start_export_job(State(app_state.clone()), tracked_headers(id), request())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "TASK_ALREADY_EXISTS");
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let progress = export_status(
                State(app_state.clone()),
                authorized_headers(),
                AxumPath(id.to_string()),
            )
            .await
            .unwrap()
            .0;
            if progress.status == "completed" {
                break progress.result.unwrap();
            }
            assert_ne!(progress.status, "failed");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed.message_count, 1);
    let error = start_export_job(State(app_state), tracked_headers(id), request())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "TASK_ALREADY_EXISTS");
    assert_eq!(
        std::fs::read_dir(directory.0.join("exports"))
            .unwrap()
            .count(),
        1
    );
}
