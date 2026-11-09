use super::*;
use crate::{
    task_history::{HistoryQuery, TaskHistory},
    tasks::Task,
};
use archive_domain::{MediaIntegrity, MediaRefV1};
use std::time::Duration;

#[tokio::test]
async fn export_status_recovers_durable_output_and_preserves_interrupted_identity_after_restart() {
    let directory = TestDirectory::new();
    let app = state(&directory);
    enable_super_admin(&app);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic export",
        Uuid::new_v4(),
        "synthetic-export",
    ))
    .unwrap();
    ArchiveStore::open(&app.archive_path)
        .unwrap()
        .ingest_batch(&export.batches[0])
        .unwrap();
    let response = create_export_job(
        State(app.clone()),
        authorized_headers(),
        Json(
            serde_json::from_value(json!({ "scope": "entire_archive", "format": "json" })).unwrap(),
        ),
    )
    .await
    .unwrap()
    .0;
    let completed_id = response.task_id.clone().unwrap();
    let pending_id = Uuid::new_v4().to_string();
    app.task_history
        .begin(pending_id.clone(), "export")
        .unwrap()
        .progress("running", 0);
    let restarted = state(&directory);
    enable_super_admin(&restarted);
    let completed = export_status(
        State(restarted.clone()),
        authorized_headers(),
        AxumPath(completed_id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(completed.status, "completed");
    assert_eq!(
        completed.result.unwrap().manifest_sha256,
        response.manifest_sha256
    );
    let unknown = export_status(
        State(restarted.clone()),
        authorized_headers(),
        AxumPath(pending_id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(unknown.status, "unknown");
    assert!(unknown.result.is_none());
    let mut headers = authorized_headers();
    headers.insert("x-archive-task-id", pending_id.parse().unwrap());
    let error = start_export_job(
        State(restarted),
        headers,
        Json(
            serde_json::from_value(json!({ "scope": "entire_archive", "format": "json" })).unwrap(),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(error.body.code, "TASK_ALREADY_EXISTS");
    let stored = std::fs::read_to_string(
        directory
            .0
            .join("task-history")
            .join(format!("{completed_id}.json")),
    )
    .unwrap();
    assert!(!stored.contains("directory"));
    assert_eq!(
        sha256_file(&directory.0.join("exports").join(response.file_name)).unwrap(),
        response.manifest_sha256
    );
}

#[tokio::test]
async fn durable_import_results_and_interrupted_tasks_survive_restart_without_resubmission() {
    let directory = TestDirectory::new();
    let app = state(&directory);
    let id = Uuid::new_v4();
    let mut headers = authorized_headers();
    headers.insert("x-archive-task-id", id.to_string().parse().unwrap());
    let response = import_json(
        State(app.clone()),
        headers.clone(),
        Body::from(export_bytes(
            "synthetic private body",
            Uuid::new_v4(),
            "synthetic",
        )),
    )
    .await
    .unwrap()
    .0;
    let failed_id = Uuid::new_v4();
    let mut failed_headers = authorized_headers();
    failed_headers.insert("x-archive-task-id", failed_id.to_string().parse().unwrap());
    assert!(
        import_json(
            State(app.clone()),
            failed_headers,
            Body::from("synthetic invalid JSON")
        )
        .await
        .is_err()
    );
    let interrupted_id = Uuid::new_v4().to_string();
    let interrupted = Task::create_tracked(&app, interrupted_id.clone(), "collection").unwrap();
    interrupted.update("running", "synthetic private source path", Some(500));
    let mut restarted = state(&directory);
    let saved = crate::tasks::status(
        State(restarted.clone()),
        authorized_headers(),
        AxumPath(id.to_string()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(saved.status, "completed");
    assert_eq!(saved.result.unwrap().inserted, response.inserted);
    let failure = crate::tasks::status(
        State(restarted.clone()),
        authorized_headers(),
        AxumPath(failed_id.to_string()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(failure.status, "failed");
    assert_eq!(failure.error_code.as_deref(), Some("INVALID_CLIENT_EXPORT"));
    assert_eq!(failure.error_status, Some(422));
    let unknown = crate::tasks::status(
        State(restarted.clone()),
        authorized_headers(),
        AxumPath(interrupted_id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(unknown.status, "unknown");
    assert!(unknown.result.is_none());
    assert_eq!(
        Task::register(&restarted, &headers, "import")
            .err()
            .unwrap()
            .body
            .code,
        "TASK_ALREADY_EXISTS"
    );
    let Json(page) = crate::task_history::list(
        State(restarted.clone()),
        authorized_headers(),
        Query(HistoryQuery {
            kind: Some("collection".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.records[0].status, "unknown");
    let stored = std::fs::read_to_string(
        directory
            .0
            .join("task-history")
            .join(format!("{interrupted_id}.json")),
    )
    .unwrap();
    assert!(!stored.contains("synthetic private source path"));
    let records = serde_json::to_string(&page).unwrap();
    assert!(!records.contains("synthetic private body"));
    assert_eq!(
        ArchiveStore::open_read_only(&restarted.archive_path)
            .unwrap()
            .summary()
            .unwrap()
            .message_count,
        1
    );
    restarted.task_history = TaskHistory::open(&directory.0).unwrap();
    assert_eq!(
        restarted
            .task_history
            .get(&interrupted_id)
            .unwrap()
            .unwrap()
            .status,
        "unknown"
    );
}

#[tokio::test]
async fn history_write_failure_is_visible_and_registration_failure_starts_no_task() {
    let directory = TestDirectory::new();
    let app = state(&directory);
    let id = Uuid::new_v4().to_string();
    let handle = app.task_history.begin(id.clone(), "import").unwrap();
    let path = directory.0.join("task-history").join(format!("{id}.json"));
    let saved = path.with_extension("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    handle.progress("running", 100);
    let Json(page) = crate::task_history::list(
        State(app.clone()),
        authorized_headers(),
        Query(HistoryQuery::default()),
    )
    .await
    .unwrap();
    assert!(page.persistence_error);
    assert_eq!(page.records[0].status, "running");
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(saved, &path).unwrap();
    assert_eq!(
        TaskHistory::open(&directory.0)
            .unwrap()
            .get(&id)
            .unwrap()
            .unwrap()
            .status,
        "unknown"
    );
    let rejected = Uuid::new_v4();
    std::fs::create_dir(
        directory
            .0
            .join("task-history")
            .join(format!("{rejected}.json")),
    )
    .unwrap();
    let mut headers = authorized_headers();
    headers.insert("x-archive-task-id", rejected.to_string().parse().unwrap());
    assert!(Task::register(&app, &headers, "import").is_err());
    assert!(
        app.task_history
            .get(&rejected.to_string())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn operations_require_workspace_authentication_and_history_pages_are_bounded() {
    let directory = TestDirectory::new();
    let app = state(&directory);
    assert!(
        crate::archive_health::summary(State(app.clone()), HeaderMap::new())
            .await
            .is_err()
    );
    assert!(
        crate::archive_health::start(State(app.clone()), HeaderMap::new())
            .await
            .is_err()
    );
    assert!(
        crate::task_history::list(
            State(app.clone()),
            HeaderMap::new(),
            Query(HistoryQuery::default())
        )
        .await
        .is_err()
    );
    assert!(!app.archive_path.exists());
    for _ in 0..3 {
        app.task_history
            .begin(Uuid::new_v4().to_string(), "import")
            .unwrap();
    }
    let Json(page) = crate::task_history::list(
        State(app),
        authorized_headers(),
        Query(HistoryQuery {
            offset: Some(1),
            limit: Some(1),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.records.len(), 1);
}

#[test]
fn corrupt_history_is_not_silently_overwritten_or_treated_as_a_new_installation() {
    let directory = TestDirectory::new();
    let history = TaskHistory::open(&directory.0).unwrap();
    let id = Uuid::new_v4().to_string();
    history.begin(id.clone(), "import").unwrap();
    let path = directory.0.join("task-history").join(format!("{id}.json"));
    std::fs::write(&path, b"{truncated").unwrap();
    assert!(TaskHistory::open(&directory.0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{truncated");
}

#[test]
fn incomplete_completion_receipts_are_not_presented_as_confirmed_results() {
    let directory = TestDirectory::new();
    let history = TaskHistory::open(&directory.0).unwrap();
    let id = Uuid::new_v4().to_string();
    history.begin(id.clone(), "import").unwrap();
    let path = directory.0.join("task-history").join(format!("{id}.json"));
    let original = std::fs::read(&path).unwrap();
    for kind in [
        "import",
        "collection",
        "collector_upload",
        "export",
        "local_export",
        "health_check",
    ] {
        let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
        record["kind"] = json!(kind);
        record["status"] = json!("completed");
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            TaskHistory::open(&directory.0).is_err(),
            "INCOMPLETE_COMPLETION_ACCEPTED"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn invalid_history_identity_and_state_are_rejected_before_recovery_writes() {
    let directory = TestDirectory::new();
    let history = TaskHistory::open(&directory.0).unwrap();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    history.begin(id.into(), "import").unwrap();
    let path = directory.0.join("task-history").join(format!("{id}.json"));
    let original = std::fs::read(&path).unwrap();
    for (field, value) in [
        ("taskId", id.to_ascii_uppercase()),
        ("kind", "invalid_kind".into()),
        ("status", "invalid_status".into()),
        ("startedAt", "invalid_time".into()),
        ("updatedAt", "invalid_time".into()),
    ] {
        let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
        record[field] = json!(value);
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            TaskHistory::open(&directory.0).is_err(),
            "INVALID_HISTORY_ACCEPTED"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn health_distinguishes_source_missing_media_from_lost_corrupt_and_unreadable_archive_files()
{
    let directory = TestDirectory::new();
    let app = state(&directory);
    let mut export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic private body",
        Uuid::new_v4(),
        "synthetic-health",
    ))
    .unwrap();
    let message = &mut export.batches[0].messages[0];
    message.message_type = MessageType::Unsupported;
    let media_root = directory.0.join("media");
    std::fs::create_dir_all(&media_root).unwrap();
    for (index, bytes) in [b"good".as_slice(), b"lost", b"hash", b"deny"]
        .into_iter()
        .enumerate()
    {
        let hash = encode_hex(&Sha256::digest(bytes));
        message.media.push(MediaRefV1 {
            content_hash: Some(hash.clone()),
            original_name: None,
            mime_type: None,
            size_bytes: Some(4),
            source_locator: "synthetic private locator".into(),
            integrity: MediaIntegrity::Verified,
            missing_reason: None,
        });
        match index {
            0 => std::fs::write(media_root.join(hash), bytes).unwrap(),
            2 => std::fs::write(media_root.join(hash), b"xxxx").unwrap(),
            3 => std::fs::create_dir(media_root.join(hash)).unwrap(),
            _ => {}
        }
    }
    message.media.push(MediaRefV1 {
        content_hash: None,
        original_name: None,
        mime_type: None,
        size_bytes: None,
        source_locator: "synthetic missing source".into(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("not_found_in_authorized_media_roots".into()),
    });
    ArchiveStore::open(&app.archive_path)
        .unwrap()
        .ingest_batch(&export.batches[0])
        .unwrap();
    let Json(summary) = crate::archive_health::summary(State(app.clone()), authorized_headers())
        .await
        .unwrap();
    assert_eq!(summary.archive.missing_media, 1);
    assert_eq!(summary.archive.missing_in_source, 1);
    assert_eq!(summary.archive.verified_media, 4);
    assert_eq!(summary.archive.unsupported_messages, 1);
    assert_eq!(summary.archive.restricted_batches, 1);
    assert!(
        !serde_json::to_string(&summary)
            .unwrap()
            .contains("synthetic private")
    );
    app.health_check_running
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        crate::archive_health::start(State(app.clone()), authorized_headers())
            .await
            .err()
            .unwrap()
            .body
            .code,
        "HEALTH_CHECK_RUNNING"
    );
    app.health_check_running
        .store(false, std::sync::atomic::Ordering::Release);
    let (_, Json(started)) = crate::archive_health::start(State(app.clone()), authorized_headers())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = app.task_history.get(&started.task_id).unwrap().unwrap();
            if let Some(result) = record.verification {
                break result;
            }
            assert_ne!(record.status, "failed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(result.database_ok);
    assert_eq!(result.checked_media, 4);
    assert_eq!(result.missing_files, 1);
    assert_eq!(result.damaged_files, 1);
    assert_eq!(result.unreadable_files, 1);
    assert_eq!(
        std::fs::read(media_root.join(encode_hex(&Sha256::digest(b"hash")))).unwrap(),
        b"xxxx"
    );
    let restored = TaskHistory::open(&directory.0).unwrap();
    assert_eq!(
        restored
            .get(&started.task_id)
            .unwrap()
            .unwrap()
            .verification
            .unwrap()
            .damaged_files,
        1
    );
}
