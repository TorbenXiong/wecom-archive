use super::*;
use std::time::Duration;

fn fixture() -> (TestDirectory, AppState) {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic independent export queue",
        Uuid::new_v4(),
        "synthetic-export-queue",
    ))
    .unwrap();
    ArchiveStore::open(&app_state.archive_path)
        .unwrap()
        .ingest_batch(&export.batches[0])
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

async fn start(app_state: &AppState) -> String {
    start_export_job(State(app_state.clone()), authorized_headers(), request())
        .await
        .unwrap()
        .0
        .task_id
}

async fn progress(app_state: &AppState, task_id: &str) -> ExportTaskProgress {
    export_status(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(task_id.into()),
    )
    .await
    .unwrap()
    .0
}

async fn wait_for(
    app_state: &AppState,
    task_id: &str,
    predicate: impl Fn(&ExportTaskProgress) -> bool,
) -> ExportTaskProgress {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = progress(app_state, task_id).await;
            if predicate(&current) {
                return current;
            }
            assert_ne!(current.status, "failed", "EXPORT_QUEUE_UNEXPECTED_FAILURE");
            assert!(
                !matches!(current.status.as_str(), "completed" | "cancelled"),
                "EXPORT_QUEUE_UNEXPECTED_TERMINAL_STATE"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("EXPORT_QUEUE_PROGRESS_TIMEOUT")
}

fn assert_output(directory: &TestDirectory, result: &ExportResponse) -> Vec<u8> {
    assert_eq!(result.message_count, 1);
    let path = directory.0.join("exports").join(&result.file_name);
    assert_eq!(read_json(&path).unwrap().message_count, 1);
    assert_eq!(sha256_file(&path).unwrap(), result.manifest_sha256);
    std::fs::read(path).unwrap()
}

fn assert_no_partial_files(directory: &TestDirectory) {
    assert!(
        std::fs::read_dir(directory.0.join("exports"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial"))
    );
}

#[test]
fn held_export_slot_only_blocks_its_own_state_and_queued_cancellation_needs_no_release() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (directory_a, app_state_a) = fixture();
            let (directory_b, app_state_b) = fixture();
            let state_clone = app_state_a.clone();
            assert!(Arc::ptr_eq(
                &app_state_a.export_slots,
                &state_clone.export_slots
            ));
            let held = Arc::clone(&app_state_a.export_slots)
                .acquire_owned()
                .await
                .unwrap();
            let queued = start(&state_clone).await;
            let independent = start(&app_state_b).await;

            // Keep A's slot held until B finishes; the bound detects a shared queue,
            // without using export speed or a sleep to prove concurrency.
            let completed = wait_for(&app_state_b, &independent, |p| p.status == "completed")
                .await
                .result
                .unwrap();
            assert_output(&directory_b, &completed);
            assert_no_partial_files(&directory_b);
            assert_eq!(
                std::fs::read_dir(directory_b.0.join("exports"))
                    .unwrap()
                    .count(),
                1
            );
            let waiting = progress(&app_state_a, &queued).await;
            assert_eq!(waiting.status, "queued");
            assert_eq!(waiting.percent, 0);
            assert!(waiting.result.is_none());
            assert!(!directory_a.0.join("exports").exists());

            let cancelling = cancel_export(
                State(state_clone.clone()),
                authorized_headers(),
                AxumPath(queued.clone()),
            )
            .await
            .unwrap()
            .0;
            assert!(matches!(
                cancelling.status.as_str(),
                "cancelling" | "cancelled"
            ));
            let cancelled = wait_for(&app_state_a, &queued, |p| p.status == "cancelled").await;
            assert!(cancelled.result.is_none());
            assert_eq!(app_state_a.export_slots.available_permits(), 0);
            assert!(!directory_a.0.join("exports").exists());

            drop(held);
            let retry = start(&app_state_a).await;
            let retried = wait_for(&app_state_a, &retry, |p| p.status == "completed")
                .await
                .result
                .unwrap();
            assert_output(&directory_a, &retried);
            assert_no_partial_files(&directory_a);
            assert_eq!(progress(&app_state_a, &queued).await.status, "cancelled");
            assert_eq!(app_state_a.export_slots.available_permits(), 1);
        });
}

#[tokio::test]
async fn same_state_exports_publish_distinct_files_and_preserve_the_first_output() {
    let (directory, app_state) = fixture();
    let first = start(&app_state).await;
    let first_result = wait_for(&app_state, &first, |p| p.status == "completed")
        .await
        .result
        .unwrap();
    let first_bytes = assert_output(&directory, &first_result);

    let second = start(&app_state.clone()).await;
    let second_result = wait_for(&app_state, &second, |p| p.status == "completed")
        .await
        .result
        .unwrap();
    assert_output(&directory, &second_result);
    assert_ne!(first_result.file_name, second_result.file_name);
    assert_eq!(assert_output(&directory, &first_result), first_bytes);
    assert_no_partial_files(&directory);
    assert_eq!(
        std::fs::read_dir(directory.0.join("exports"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(app_state.export_slots.available_permits(), 1);
}

#[tokio::test]
async fn cancelling_filename_collision_wait_preserves_every_existing_file() {
    let (directory, app_state) = fixture();
    let export_root = directory.0.join("exports");
    std::fs::create_dir(&export_root).unwrap();
    let now = Local::now();
    let sentinel = b"synthetic existing export must remain unchanged";
    let existing = (0..=10)
        .map(|seconds| {
            let name = format!(
                "wecom-{}.json",
                (now + TimeDelta::seconds(seconds)).format("%Y%m%d-%H%M%S")
            );
            let path = export_root.join(name);
            std::fs::write(&path, sentinel).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let task_id = start(&app_state).await;
    wait_for(&app_state, &task_id, |p| p.percent == 25).await;
    assert_eq!(app_state.export_slots.available_permits(), 0);
    let cancelling = cancel_export(
        State(app_state.clone()),
        authorized_headers(),
        AxumPath(task_id.clone()),
    )
    .await
    .unwrap()
    .0;
    assert!(matches!(
        cancelling.status.as_str(),
        "cancelling" | "cancelled"
    ));
    let cancelled = wait_for(&app_state, &task_id, |p| p.status == "cancelled").await;
    assert!(cancelled.result.is_none());
    for path in &existing {
        assert_eq!(std::fs::read(path).unwrap(), sentinel);
    }
    assert_eq!(
        std::fs::read_dir(&export_root).unwrap().count(),
        existing.len()
    );
    assert_no_partial_files(&directory);
    assert_eq!(app_state.export_slots.available_permits(), 1);
}

#[tokio::test]
async fn cancelled_local_export_waiter_keeps_collection_queue_and_directory_lock_until_publication()
{
    let (directory, mut app_state) = fixture();
    app_state._data_lock = Some(Arc::new(
        crate::maintenance::lock_data_root(&directory.0).unwrap(),
    ));
    let started = Arc::new(tokio::sync::Notify::new());
    let collected = Arc::new(tokio::sync::Notify::new());
    let started_reporter = Arc::clone(&started);
    let collected_reporter = Arc::clone(&collected);
    let (release, waiting) = std::sync::mpsc::channel();
    let waiting = Mutex::new(waiting);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "synthetic local export after cancelled request",
        Uuid::new_v4(),
        "synthetic-local-export-detached",
    ))
    .unwrap();
    let expected_export_id = export.export_id;
    app_state.local_collector = Some(Arc::new(move |_, _, _, _, _, _| {
        started_reporter.notify_one();
        waiting
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "SYNTHETIC_LOCAL_EXPORT_RELEASE_TIMEOUT".to_owned())?;
        collected_reporter.notify_one();
        Ok(export.clone())
    }));
    let export_slots = Arc::clone(&app_state.export_slots);
    let held = Arc::clone(&export_slots).acquire_owned().await.unwrap();
    let request_state = app_state.clone();
    let waiter = tokio::spawn(async move {
        create_local_export(
            State(request_state),
            authorized_headers(),
            Json(serde_json::from_value(json!({"format": "json"})).unwrap()),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    // No caller-owned state remains; the detached operation must own the
    // workspace lock during collection and while awaiting the output slot.
    drop(app_state);
    assert!(crate::maintenance::lock_data_root(&directory.0).is_err());
    assert!(!directory.0.join("exports").exists());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), collected.notified())
        .await
        .unwrap();
    assert_eq!(export_slots.available_permits(), 0);
    assert!(!directory.0.join("exports").exists());
    assert!(crate::maintenance::lock_data_root(&directory.0).is_err());
    drop(held);

    let published = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let export_root = directory.0.join("exports");
            if export_root.is_dir() {
                let paths = std::fs::read_dir(&export_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect::<Vec<_>>();
                if paths.len() == 1
                    && paths[0]
                        .extension()
                        .is_some_and(|extension| extension == "json")
                    && export_slots.available_permits() == 1
                    && let Ok(verified) = read_json(&paths[0])
                    && let Ok(lock) = crate::maintenance::lock_data_root(&directory.0)
                {
                    drop(lock);
                    break verified;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("DETACHED_LOCAL_EXPORT_PUBLICATION_TIMEOUT");
    assert_eq!(published.export_id, expected_export_id);
    assert_eq!(published.message_count, 1);
    assert_no_partial_files(&directory);
    assert_eq!(export_slots.available_permits(), 1);
    let unlocked = crate::maintenance::lock_data_root(&directory.0).unwrap();
    drop(unlocked);
}

#[tokio::test]
async fn dropped_synchronous_export_waiter_keeps_queued_work_and_directory_lock_until_publication()
{
    use std::future::Future;
    use std::task::Poll;

    let (directory, mut app_state) = fixture();
    app_state._data_lock = Some(Arc::new(
        crate::maintenance::lock_data_root(&directory.0).unwrap(),
    ));
    let export_slots = Arc::clone(&app_state.export_slots);
    let held = Arc::clone(&export_slots).acquire_owned().await.unwrap();
    let request_state = app_state.clone();
    let mut waiter = Box::pin(create_export_job(
        State(request_state),
        authorized_headers(),
        request(),
    ));
    // Poll the handler so it starts its owner before simulating disconnect.
    // Dropping an unpolled future would never have started an export.
    std::future::poll_fn(|context| {
        assert!(waiter.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(waiter);
    drop(app_state);
    assert_eq!(export_slots.available_permits(), 0);
    assert!(!directory.0.join("exports").exists());
    assert!(crate::maintenance::lock_data_root(&directory.0).is_err());
    drop(held);

    let published = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let export_root = directory.0.join("exports");
            if export_root.is_dir() {
                let paths = std::fs::read_dir(&export_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect::<Vec<_>>();
                if paths.len() == 1
                    && paths[0]
                        .extension()
                        .is_some_and(|extension| extension == "json")
                    && export_slots.available_permits() == 1
                    && let Ok(verified) = read_json(&paths[0])
                    && let Ok(lock) = crate::maintenance::lock_data_root(&directory.0)
                {
                    drop(lock);
                    break verified;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("DETACHED_SYNCHRONOUS_EXPORT_PUBLICATION_TIMEOUT");
    assert_eq!(published.message_count, 1);
    assert_eq!(
        published.batches[0].messages[0].body_text.as_deref(),
        Some("synthetic independent export queue")
    );
    assert_no_partial_files(&directory);
    assert_eq!(export_slots.available_permits(), 1);
    let unlocked = crate::maintenance::lock_data_root(&directory.0).unwrap();
    drop(unlocked);
}
