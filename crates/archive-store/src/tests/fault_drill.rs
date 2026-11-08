//! Bounded synthetic faults; no production hooks or host disk exhaustion.
use super::*;
use rusqlite::{ErrorCode, functions::FunctionFlags, types::ValueRef};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn fingerprint(connection: &rusqlite::Connection) -> Vec<(String, Vec<Vec<u8>>)> {
    let names: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names
        .into_iter()
        .map(|name| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
                .unwrap();
            let columns = statement.column_count();
            let mut cursor = statement.query([]).unwrap();
            let mut rows = Vec::new();
            while let Some(row) = cursor.next().unwrap() {
                let mut hash = Sha256::new();
                for column in 0..columns {
                    match row.get_ref(column).unwrap() {
                        ValueRef::Null => hash.update([0]),
                        ValueRef::Integer(v) => {
                            hash.update([1]);
                            hash.update(v.to_le_bytes());
                        }
                        ValueRef::Real(v) => {
                            hash.update([2]);
                            hash.update(v.to_bits().to_le_bytes());
                        }
                        ValueRef::Text(v) | ValueRef::Blob(v) => {
                            hash.update([
                                if matches!(row.get_ref(column).unwrap(), ValueRef::Text(_)) {
                                    3
                                } else {
                                    4
                                },
                            ]);
                            hash.update((v.len() as u64).to_le_bytes());
                            hash.update(v);
                        }
                    }
                }
                rows.push(hash.finalize().to_vec());
            }
            rows.sort();
            (name, rows)
        })
        .collect()
}

fn input() -> Vec<ArchiveBatchV1> {
    let revision = batch("recovered needle revision");
    let mut added = batch("recovered needle new message");
    added.messages[0].stable_message_id = "fault-added".into();
    added.messages[0].source_message_id = "fault-added".into();
    vec![revision, added]
}

fn directory() -> Vec<ParticipantV1> {
    vec![ParticipantV1 {
        participant_id: "fault-checkpoint".into(),
        display_name: Some("synthetic directory".into()),
        participant_kind: None,
    }]
}

fn assert_healthy(store: &ArchiveStore) {
    store.verify_integrity().unwrap();
    store
        .connection
        .execute_batch("INSERT INTO message_fts(message_fts) VALUES('integrity-check')")
        .unwrap();
}

#[test]
fn capacity_full_in_later_batch_and_directory_rolls_back_and_retries() {
    for in_directory in [false, true] {
        let root = tempdir().unwrap();
        let path = root.path().join("archive.db");
        let mut store = ArchiveStore::open(&path).unwrap();
        store.ingest_batch(&batch("original")).unwrap();
        let before = fingerprint(&store.connection);
        let pages: i64 = store
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        store
            .connection
            .pragma_update(None, "max_page_count", pages + 64)
            .unwrap();
        let mut imports = input();
        let mut participants = directory();
        // Enough capacity for the first revision, but not this bounded payload.
        if in_directory {
            participants[0].display_name = Some("synthetic ".repeat(200_000));
        } else {
            imports[1].messages[0].raw_payload = json!({"synthetic": "x".repeat(2_000_000)});
        }
        let yielded = std::cell::Cell::new(0);
        let error = store
            .ingest_batches_with_directory(
                imports
                    .clone()
                    .into_iter()
                    .inspect(|_| yielded.set(yielded.get() + 1)),
                &[],
                &participants,
            )
            .unwrap_err();
        assert_eq!(yielded.get(), 2, "FIRST_BATCH_MUST_COMPLETE_BEFORE_FULL");
        assert!(
            matches!(error, StoreError::Sql(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::DiskFull)
        );
        assert!(store.connection.is_autocommit());
        assert_eq!(fingerprint(&store.connection), before);
        assert_healthy(&store);
        drop(store);
        let mut store = ArchiveStore::open(&path).unwrap();
        assert_eq!(fingerprint(&store.connection), before);
        store
            .connection
            .pragma_update(None, "max_page_count", 2_147_483_646_i64)
            .unwrap();
        let outcome = store
            .ingest_batches_with_directory(imports.clone(), &[], &participants)
            .unwrap();
        assert_eq!((outcome.revised, outcome.inserted), (1, 1));
        let after = fingerprint(&store.connection);
        let repeated = store
            .ingest_batches_with_directory(imports, &[], &participants)
            .unwrap();
        assert_eq!(
            (repeated.revised, repeated.inserted, repeated.unchanged),
            (0, 0, 2)
        );
        // An idempotent import still records audit/batch status; assert message revisions separately.
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM message_revisions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_ne!(after, before);
        assert_healthy(&store);
    }
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn signal_and_wait(root: &Path) {
    fs::write(root.join("ready"), b"ready").unwrap();
    // Bound a lost parent even when a test runner is interrupted.
    std::thread::sleep(Duration::from_secs(30));
    panic!("FAULT_PARENT_DID_NOT_TERMINATE_WORKER");
}

#[test]
#[ignore = "subprocess worker; invoked only by the bounded crash drill"]
fn process_worker() {
    let root = std::path::PathBuf::from(
        std::env::var_os("WECOM_STORE_FAULT_ROOT").expect("FAULT_ROOT_REQUIRED"),
    );
    assert_eq!(
        fs::read(root.join(".fault-owned")).unwrap(),
        b"store-fault.v1"
    );
    let imports: Vec<ArchiveBatchV1> =
        serde_json::from_slice(&fs::read(root.join("input.json")).unwrap()).unwrap();
    let mut store = ArchiveStore::open(&root.join("archive.db")).unwrap();
    store
        .connection
        .pragma_update(None, "cache_size", 8)
        .unwrap();
    store
        .connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let before_commit = std::env::var("WECOM_STORE_FAULT_PHASE").unwrap() == "before";
    if before_commit {
        let root = root.clone();
        store
            .connection
            .create_scalar_function(
                "fault_pause",
                0,
                FunctionFlags::SQLITE_UTF8,
                move |_| -> rusqlite::Result<i32> {
                    signal_and_wait(&root);
                    Ok(0)
                },
            )
            .unwrap();
        store.connection.execute_batch("CREATE TEMP TRIGGER pause_directory BEFORE INSERT ON participants WHEN new.participant_id='fault-checkpoint' BEGIN SELECT fault_pause(); END;").unwrap();
    }
    store
        .ingest_batches_with_directory(imports, &[], &directory())
        .unwrap();
    fs::write(
        root.join("committed-fingerprint.json"),
        serde_json::to_vec(&fingerprint(&store.connection)).unwrap(),
    )
    .unwrap();
    signal_and_wait(&root);
}

#[test]
fn killed_import_before_commit_rolls_back_and_after_commit_recovers() {
    for phase in ["before", "after"] {
        let root = tempdir().unwrap();
        fs::write(root.path().join(".fault-owned"), b"store-fault.v1").unwrap();
        let path = root.path().join("archive.db");
        let mut store = ArchiveStore::open(&path).unwrap();
        store.ingest_batch(&batch("original")).unwrap();
        let before = fingerprint(&store.connection);
        drop(store);
        let mut imports = input();
        // Force dirty pages to spill to WAL before the directory checkpoint.
        imports[1].messages[0].raw_payload = json!({"synthetic": "x".repeat(1_000_000)});
        fs::write(
            root.path().join("input.json"),
            serde_json::to_vec(&imports).unwrap(),
        )
        .unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::fault_drill::process_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WECOM_STORE_FAULT_ROOT", root.path())
                .env("WECOM_STORE_FAULT_PHASE", phase)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "FAULT_WORKER_EXITED_EARLY"
            );
            assert!(Instant::now() < deadline, "FAULT_CHECKPOINT_TIMEOUT");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fs::metadata(path.with_extension("db-wal")).unwrap().len() > 32);
        worker.0.kill().unwrap();
        assert!(!worker.0.wait().unwrap().success());
        let mut recovered = ArchiveStore::open(&path).unwrap();
        assert_healthy(&recovered);
        if phase == "before" {
            assert_eq!(fingerprint(&recovered.connection), before);
        } else {
            let committed: Vec<(String, Vec<Vec<u8>>)> = serde_json::from_slice(
                &fs::read(root.path().join("committed-fingerprint.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(fingerprint(&recovered.connection), committed);
            assert_eq!(recovered.summary().unwrap().message_count, 2);
            assert_eq!(
                recovered
                    .count_messages(&MessageQuery {
                        text: Some("needle".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                2
            );
        }
        let result = recovered
            .ingest_batches_with_directory(imports.clone(), &[], &directory())
            .unwrap();
        assert_eq!(
            (result.revised, result.inserted),
            if phase == "before" { (1, 1) } else { (0, 0) }
        );
        assert_eq!(
            recovered
                .ingest_batches_with_directory(imports, &[], &directory())
                .unwrap()
                .unchanged,
            2
        );
        assert_eq!(
            recovered
                .connection
                .query_row("SELECT count(*) FROM message_revisions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_healthy(&recovered);
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires the administrator-created bounded virtual NTFS test volume"]
fn bounded_volume_full_import_rolls_back_then_retries() {
    use std::io::Write;
    let volume = std::path::PathBuf::from(
        std::env::var_os("WECOM_FAULT_VOLUME").expect("BOUNDED_VOLUME_REQUIRED"),
    );
    assert_eq!(
        fs::read(volume.join(".bounded-fault-volume")).unwrap(),
        b"bounded-volume.v1"
    );
    let root = tempfile::tempdir_in(&volume).unwrap();
    let path = root.path().join("archive.db");
    let mut store = ArchiveStore::open(&path).unwrap();
    store.ingest_batch(&batch("original")).unwrap();
    store
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let before = fingerprint(&store.connection);
    let mut imports = input();
    imports[1].messages[0].raw_payload = json!({"synthetic": "x".repeat(2_000_000)});
    let filler = root.path().join("filler");
    let mut fill = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&filler)
        .unwrap();
    let block = vec![0x5a; 1024 * 1024];
    let mut bytes = 0;
    let error = loop {
        match fill.write_all(&block).and_then(|_| fill.sync_all()) {
            Ok(()) => {
                bytes += block.len();
                assert!(bytes <= 512 * 1024 * 1024, "VOLUME_BOUND_EXCEEDED");
            }
            Err(error) => break error,
        }
    };
    assert!(
        matches!(error.raw_os_error(), Some(39 | 112)),
        "EXPECTED_WINDOWS_DISK_FULL"
    );
    println!(
        "PASS NTFS_IMPORT_VOLUME_FULL os_error={} fill_bytes={}",
        error.raw_os_error().unwrap(),
        fill.metadata().unwrap().len()
    );
    fill.set_len(
        fill.metadata()
            .unwrap()
            .len()
            .checked_sub(256 * 1024)
            .unwrap(),
    )
    .unwrap();
    fill.sync_all().unwrap();
    drop(fill);
    let yielded = std::cell::Cell::new(0);
    let error = store
        .ingest_batches_with_directory(
            imports
                .clone()
                .into_iter()
                .inspect(|_| yielded.set(yielded.get() + 1)),
            &[],
            &directory(),
        )
        .unwrap_err();
    assert_eq!(yielded.get(), 2, "FIRST_BATCH_MUST_COMPLETE_BEFORE_FULL");
    assert!(
        matches!(error, StoreError::Sql(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::DiskFull)
    );
    assert!(store.connection.is_autocommit());
    assert_eq!(fingerprint(&store.connection), before);
    drop(store);
    fs::remove_file(filler).unwrap();
    let mut recovered = ArchiveStore::open(&path).unwrap();
    assert_eq!(fingerprint(&recovered.connection), before);
    assert_healthy(&recovered);
    let result = recovered
        .ingest_batches_with_directory(imports.clone(), &[], &directory())
        .unwrap();
    assert_eq!((result.revised, result.inserted), (1, 1));
    assert_eq!(
        recovered
            .ingest_batches_with_directory(imports, &[], &directory())
            .unwrap()
            .unchanged,
        2
    );
    assert_healthy(&recovered);
    println!("PASS BOUNDED_NTFS_FULL_IMPORT_ROLLBACK_AND_RETRY");
}
