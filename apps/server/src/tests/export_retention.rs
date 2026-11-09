use super::*;
use std::time::{Duration, Instant};

fn fixture() -> (TestDirectory, AppState) {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let input: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic retained export result",
        Uuid::new_v4(),
        "synthetic-export-retention",
    ))
    .unwrap();
    ArchiveStore::open(&app_state.archive_path)
        .unwrap()
        .ingest_batch(&input.batches[0])
        .unwrap();
    (directory, app_state)
}

fn request() -> Json<ExportRequest> {
    Json(
        serde_json::from_value(json!({
            "scope": "entire_archive",
            "format": "json"
        }))
        .unwrap(),
    )
}

async fn progress(app_state: &AppState, id: &str) -> ExportTaskProgress {
    export_status(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(id.into()),
    )
    .await
    .unwrap()
    .0
}

async fn wait_for(app_state: &AppState, id: &str, expected: &str) -> ExportTaskProgress {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = progress(app_state, id).await;
            if current.status == expected {
                break current;
            }
            assert!(matches!(
                current.status.as_str(),
                "queued" | "running" | "cancelling"
            ));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("EXPORT_RETENTION_STATE_TIMEOUT")
}

async fn completed_export(app_state: &AppState) -> (String, ExportResponse) {
    let started = start_export_job(State(app_state.clone()), authorized_headers(), request())
        .await
        .unwrap()
        .0;
    let completed = wait_for(app_state, &started.task_id, "completed")
        .await
        .result
        .unwrap();
    (started.task_id, completed)
}

fn task(app_state: &AppState, id: &str) -> Arc<ExportTask> {
    app_state
        .export_tasks
        .lock()
        .unwrap()
        .get(id)
        .unwrap()
        .clone()
}

fn failed_record() -> Arc<ExportTask> {
    let record = Arc::new(ExportTask::new());
    record.finish(&Err(ApiError::invalid_query()));
    record
}

fn fill_recent_records(app_state: &AppState) {
    let mut tasks = app_state.export_tasks.lock().unwrap();
    while tasks.len() < 64 {
        tasks.insert(Uuid::new_v4().to_string(), failed_record());
    }
}

async fn cancel_and_wait(app_state: &AppState, id: &str) {
    let cancellation = cancel_export(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(id.into()),
    )
    .await
    .unwrap()
    .0;
    assert!(matches!(
        cancellation.status.as_str(),
        "cancelling" | "cancelled"
    ));
    assert!(wait_for(app_state, id, "cancelled").await.result.is_none());
}

async fn rejected_start(app_state: &AppState, headers: HeaderMap) -> ApiError {
    let outcome = start_export_job(State(app_state.clone()), headers, request()).await;
    // Clean up a regression's unexpectedly accepted task before failing; the
    // caller holds the output slot, so this cannot launch another writer.
    if let Ok(accepted) = &outcome {
        cancel_and_wait(app_state, &accepted.0.task_id).await;
    }
    outcome.expect_err("RECENT_EXPORT_RESULTS_MUST_NOT_BE_EVICTED_AT_CAPACITY")
}

#[tokio::test]
async fn full_capacity_rejects_a_new_export_and_preserves_recent_completed_results() {
    let (directory, app_state) = fixture();
    let (original_id, completed) = completed_export(&app_state).await;
    assert_eq!(completed.message_count, 1);
    let output = directory.0.join("exports").join(&completed.file_name);
    let original_bytes = std::fs::read(&output).unwrap();
    fill_recent_records(&app_state);
    let expected = serde_json::to_value(progress(&app_state, &original_id).await).unwrap();
    let held = Arc::clone(&app_state.export_slots)
        .acquire_owned()
        .await
        .unwrap();
    let error = rejected_start(&app_state, authorized_headers()).await;
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.body.code, "EXPORT_TASK_CAPACITY_REACHED");
    assert_eq!(app_state.export_tasks.lock().unwrap().len(), 64);
    assert_eq!(
        serde_json::to_value(progress(&app_state, &original_id).await).unwrap(),
        expected
    );
    assert_eq!(std::fs::read(&output).unwrap(), original_bytes);
    assert_eq!(sha256_file(&output).unwrap(), completed.manifest_sha256);
    assert_eq!(
        std::fs::read_dir(directory.0.join("exports"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(app_state.export_slots.available_permits(), 0);
    let mut duplicate_headers = authorized_headers();
    duplicate_headers.insert("x-archive-task-id", original_id.parse().unwrap());
    let duplicate = rejected_start(&app_state, duplicate_headers).await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body.code, "TASK_ALREADY_EXISTS");
    assert_eq!(
        serde_json::to_value(progress(&app_state, &original_id).await).unwrap(),
        expected
    );
    drop(held);
}

#[tokio::test]
async fn expired_results_are_pruned_by_lookup_cancel_and_registration_without_deleting_output() {
    let (directory, app_state) = fixture();
    let (original_id, completed) = completed_export(&app_state).await;
    let output = directory.0.join("exports").join(&completed.file_name);
    let original_bytes = std::fs::read(&output).unwrap();
    let expired_at = Instant::now() - Duration::from_secs(3601);
    task(&app_state, &original_id)
        .progress
        .lock()
        .unwrap()
        .finished_at = Some(expired_at);
    let failed_id = Uuid::new_v4().to_string();
    let failed = failed_record();
    failed.progress.lock().unwrap().finished_at = Some(expired_at);
    let cancelled_id = Uuid::new_v4().to_string();
    let cancelled = Arc::new(ExportTask::new());
    cancelled.cancel();
    cancelled.finish(&Err(ApiError::from_export(
        archive_export::ExportError::Cancelled,
    )));
    cancelled.progress.lock().unwrap().finished_at = Some(expired_at);
    {
        let mut tasks = app_state.export_tasks.lock().unwrap();
        tasks.insert(failed_id.clone(), failed);
        tasks.insert(cancelled_id.clone(), cancelled);
    }
    fill_recent_records(&app_state);
    let recent_ids = app_state
        .export_tasks
        .lock()
        .unwrap()
        .keys()
        .filter(|id| ![&original_id, &failed_id, &cancelled_id].contains(id))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(recent_ids.len(), 61);

    let durable = export_status(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(original_id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(durable.status, "completed");
    assert_eq!(
        durable.result.unwrap().manifest_sha256,
        completed.manifest_sha256
    );
    let missing = cancel_export(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(failed_id.clone()),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert_eq!(missing.body.code, "INVALID_QUERY");
    assert!(
        app_state
            .export_tasks
            .lock()
            .unwrap()
            .contains_key(&cancelled_id)
    );
    let held = Arc::clone(&app_state.export_slots)
        .acquire_owned()
        .await
        .unwrap();
    let started = start_export_job(State(app_state.clone()), authorized_headers(), request())
        .await
        .unwrap()
        .0;
    {
        let tasks = app_state.export_tasks.lock().unwrap();
        assert_eq!(tasks.len(), 62);
        assert!(recent_ids.iter().all(|id| tasks.contains_key(id)));
        for id in [&original_id, &failed_id, &cancelled_id] {
            assert!(!tasks.contains_key(id));
        }
        assert!(tasks.contains_key(&started.task_id));
    }
    assert_eq!(
        progress(&app_state, &started.task_id).await.status,
        "queued"
    );
    cancel_and_wait(&app_state, &started.task_id).await;
    assert_eq!(std::fs::read(&output).unwrap(), original_bytes);
    assert_eq!(read_json(&output).unwrap().message_count, 1);
    assert_eq!(sha256_file(&output).unwrap(), completed.manifest_sha256);
    assert_eq!(
        std::fs::read_dir(directory.0.join("exports"))
            .unwrap()
            .count(),
        1
    );
    drop(held);
}

#[tokio::test]
async fn capacity_preserves_active_unknown_and_poisoned_records() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let expired_at = Instant::now() - Duration::from_secs(3601);
    let mut protected = Vec::new();
    for (status, finished_at) in [
        ("queued", Some(expired_at)),
        ("running", Some(expired_at)),
        ("cancelling", Some(expired_at)),
        ("completed", None),
        ("failed", None),
        ("cancelled", None),
        ("synthetic_unknown", Some(expired_at)),
    ] {
        let record = Arc::new(ExportTask::new());
        {
            let mut saved = record.progress.lock().unwrap();
            saved.status = status.into();
            saved.finished_at = finished_at;
        }
        protected.push((Uuid::new_v4().to_string(), record));
    }
    let poisoned = Arc::new(ExportTask::new());
    let poison = Arc::clone(&poisoned);
    assert!(
        std::thread::spawn(move || {
            let _held = poison.progress.lock().unwrap();
            panic!("SYNTHETIC_EXPORT_RETENTION_POISON");
        })
        .join()
        .is_err()
    );
    let poisoned_id = Uuid::new_v4().to_string();
    protected.push((poisoned_id.clone(), poisoned));
    {
        let mut tasks = app_state.export_tasks.lock().unwrap();
        tasks.extend(protected.iter().cloned());
    }
    fill_recent_records(&app_state);
    let held = Arc::clone(&app_state.export_slots)
        .acquire_owned()
        .await
        .unwrap();
    let error = rejected_start(&app_state, authorized_headers()).await;
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.body.code, "EXPORT_TASK_CAPACITY_REACHED");
    for (id, record) in &protected {
        assert!(
            app_state
                .export_tasks
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|retained| Arc::ptr_eq(retained, record))
        );
        if id != &poisoned_id {
            let retained = progress(&app_state, id).await;
            assert_eq!(retained.status, record.progress.lock().unwrap().status);
        }
    }
    let unreadable = export_status(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(poisoned_id.clone()),
    )
    .await
    .unwrap_err();
    assert_eq!(unreadable.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        app_state
            .export_tasks
            .lock()
            .unwrap()
            .contains_key(&poisoned_id)
    );
    assert_eq!(app_state.export_tasks.lock().unwrap().len(), 64);
    assert!(!directory.0.join("exports").exists());
    drop(held);
}

#[tokio::test]
async fn completed_results_ignore_late_progress_cancellation_and_refinalization() {
    let (_directory, app_state) = fixture();
    let (id, completed) = completed_export(&app_state).await;
    let record = task(&app_state, &id);
    let original = progress(&app_state, &id).await;
    let finished_at = original
        .finished_at
        .expect("COMPLETED_EXPORT_MUST_START_RETENTION");
    let expected = serde_json::to_value(&original).unwrap();
    assert!(expected.get("finishedAt").is_none());
    assert!(expected.get("finished_at").is_none());
    record.update(1, "synthetic late progress");
    record.finish(&Err(ApiError::invalid_query()));
    let cancellation = cancel_export(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(cancellation.status, "completed");
    record.finish(&Ok(completed));
    assert_eq!(
        serde_json::to_value(progress(&app_state, &id).await).unwrap(),
        expected
    );
    assert_eq!(
        record.progress.lock().unwrap().finished_at,
        Some(finished_at)
    );
    assert!(!record.cancelled.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn failed_and_cancelled_exports_record_terminal_time_once() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let invalid = Json(
        serde_json::from_value(json!({
            "scope": "current_conversation", "format": "json"
        }))
        .unwrap(),
    );
    let failed = start_export_job(State(app_state.clone()), authorized_headers(), invalid)
        .await
        .unwrap()
        .0;
    let failed_progress = wait_for(&app_state, &failed.task_id, "failed").await;
    assert!(failed_progress.finished_at.is_some());
    assert_eq!(failed_progress.error_code.as_deref(), Some("INVALID_QUERY"));
    assert!(failed_progress.result.is_none());
    let failed_record = task(&app_state, &failed.task_id);
    failed_record.update(50, "synthetic late failure progress");
    failed_record.finish(&Err(ApiError::store()));
    assert_eq!(
        serde_json::to_value(progress(&app_state, &failed.task_id).await).unwrap(),
        serde_json::to_value(&failed_progress).unwrap()
    );
    assert_eq!(
        failed_record.progress.lock().unwrap().finished_at,
        failed_progress.finished_at
    );

    let held = Arc::clone(&app_state.export_slots)
        .acquire_owned()
        .await
        .unwrap();
    let cancelled = start_export_job(State(app_state.clone()), authorized_headers(), request())
        .await
        .unwrap()
        .0;
    assert!(
        task(&app_state, &cancelled.task_id)
            .progress
            .lock()
            .unwrap()
            .finished_at
            .is_none()
    );
    cancel_and_wait(&app_state, &cancelled.task_id).await;
    let cancelled_progress = progress(&app_state, &cancelled.task_id).await;
    assert!(cancelled_progress.finished_at.is_some());
    let cancelled_record = task(&app_state, &cancelled.task_id);
    cancelled_record.update(50, "synthetic late cancelled progress");
    cancelled_record.finish(&Err(ApiError::invalid_query()));
    assert_eq!(
        serde_json::to_value(progress(&app_state, &cancelled.task_id).await).unwrap(),
        serde_json::to_value(&cancelled_progress).unwrap()
    );
    assert_eq!(
        cancelled_record.progress.lock().unwrap().finished_at,
        cancelled_progress.finished_at
    );
    assert!(!directory.0.join("exports").exists());
    drop(held);
}
