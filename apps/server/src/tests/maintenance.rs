use super::*;
use crate::maintenance::{
    RESTORE_PENDING_FILE, activate_restored, create_backup, lock_data_root, restore_backup,
    verify_backup,
};

mod fault_drill;

struct Fixture {
    _base: TestDirectory,
    source: PathBuf,
    executable: PathBuf,
    output: PathBuf,
    hash: String,
}

impl Fixture {
    fn new() -> Self {
        let base = TestDirectory::new();
        let source = base.0.join("serverData");
        let output = base.0.join("backups");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&output).unwrap();
        let executable = base.0.join("WeComArchive.exe");
        std::fs::write(&executable, b"synthetic executable, never launched").unwrap();
        let mut config = new_enterprise_config();
        config.upload_url = "http://10.20.30.40:9812".into();
        let schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 1,
            ..Default::default()
        };
        config.local_collection_plans.push(LocalCollectionPlan {
            id: "synthetic-plan".into(),
            name: "测试计划".into(),
            include_media: true,
            schedule: schedule.clone(),
            created_at: "2026-01-02T03:04:05Z".into(),
            updated_at: "2026-01-02T03:04:05Z".into(),
            last_run_at: None,
            last_status: None,
            last_detail: None,
        });
        config.collectors.push(serde_json::from_value(json!({
            "collector_id": "synthetic-collector", "key_id": config.key_id,
            "created_at": "2026-01-02T03:04:05Z", "public_key_hex": config.public_key_hex,
            "private_key_protected_hex": config.private_key_protected_hex,
            "upload_token_sha256": "0".repeat(64), "schedule": schedule,
            "manual_collection_request_id": "synthetic-pending-task", "software_update_request": "synthetic-update"
        })).unwrap());
        persist_enterprise_config(&source, &config).unwrap();
        persist_access_token(&source.join(ACCESS_TOKEN_FILE), TEST_TOKEN.as_bytes()).unwrap();
        let bytes = b"synthetic attachment";
        let hash = encode_hex(&Sha256::digest(bytes));
        let mut export: ClientExportV1 =
            serde_json::from_slice(&export_bytes("backup fixture", Uuid::new_v4(), "synthetic"))
                .unwrap();
        export.batches[0].messages[0]
            .media
            .push(archive_domain::MediaRefV1 {
                content_hash: Some(hash.clone()),
                original_name: Some("synthetic.txt".into()),
                mime_type: None,
                size_bytes: Some(bytes.len() as u64),
                source_locator: "synthetic".into(),
                integrity: archive_domain::MediaIntegrity::Verified,
                missing_reason: None,
            });
        let mut store = ArchiveStore::open(&source.join("archive.db")).unwrap();
        store
            .ingest_batches_with_directory(
                export.batches,
                &export.conversations,
                &export.participants,
            )
            .unwrap();
        let mut revision: ClientExportV1 = serde_json::from_slice(&export_bytes(
            "revised backup fixture",
            Uuid::new_v4(),
            "revision",
        ))
        .unwrap();
        revision.batches[0].messages[0].media = store
            .query_messages(&archive_store::MessageQuery {
                limit: 1,
                ..Default::default()
            })
            .unwrap()[0]
            .media
            .clone();
        store.ingest_batch(&revision.batches[0]).unwrap();
        std::fs::create_dir(source.join("media")).unwrap();
        std::fs::write(source.join("media").join(&hash), bytes).unwrap();
        std::fs::write(
            base.0.join("source-roots.dpapi"),
            source_windows::secrets::protect(b"[]").unwrap(),
        )
        .unwrap();
        std::fs::create_dir(source.join("work")).unwrap();
        std::fs::write(source.join("work/synthetic-transient"), b"must not back up").unwrap();
        Self {
            _base: base,
            source,
            executable,
            output,
            hash,
        }
    }

    fn backup(&self) -> PathBuf {
        PathBuf::from(
            create_backup(&self.source, &self.executable, &self.output)
                .unwrap()
                .directory,
        )
    }
}

#[test]
fn full_backup_restores_messages_revisions_media_credentials_and_disabled_execution() {
    let fixture = Fixture::new();
    let source_config = std::fs::read(fixture.source.join(ENTERPRISE_CONFIG_FILE)).unwrap();
    let source_db = std::fs::read(fixture.source.join("archive.db")).unwrap();
    let backup = fixture.backup();
    assert!(!backup.join("serverData/work").exists());
    verify_backup(&backup).unwrap();
    assert!(lock_data_root(&backup.join("serverData")).is_err());
    assert!(!backup.join("serverData/workspace.lock").exists());
    let restored = PathBuf::from(restore_backup(&backup, &fixture.output).unwrap().directory);
    let root = restored.join("serverData");
    assert!(root.join(RESTORE_PENDING_FILE).is_file());
    assert_eq!(
        std::fs::read(root.join(ENTERPRISE_CONFIG_FILE)).unwrap(),
        source_config
    );
    let original = ArchiveStore::open_read_only(&fixture.source.join("archive.db")).unwrap();
    let copy = ArchiveStore::open_read_only(&root.join("archive.db")).unwrap();
    assert_eq!(copy.summary().unwrap(), original.summary().unwrap());
    assert_eq!(
        copy.query_messages(&archive_store::MessageQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap(),
        original
            .query_messages(&archive_store::MessageQuery {
                limit: 100,
                ..Default::default()
            })
            .unwrap()
    );
    assert_eq!(
        std::fs::read(root.join("media").join(&fixture.hash)).unwrap(),
        b"synthetic attachment"
    );
    assert_eq!(
        std::fs::read(root.join(ACCESS_TOKEN_FILE)).unwrap(),
        std::fs::read(fixture.source.join(ACCESS_TOKEN_FILE)).unwrap()
    );
    activate_restored(&root).unwrap();
    assert!(!root.join(RESTORE_PENDING_FILE).exists());
    let config: EnterpriseConfig =
        serde_json::from_slice(&std::fs::read(root.join(ENTERPRISE_CONFIG_FILE)).unwrap()).unwrap();
    assert_eq!(
        config.local_collection_plans[0].schedule.mode,
        CollectionScheduleMode::Disabled
    );
    assert!(!config.collectors[0].enabled);
    assert_eq!(config.upload_url, DEFAULT_SERVER_URL);
    assert!(config.collectors[0].manual_collection_request_id.is_none());
    assert!(config.collectors[0].software_update_request.is_none());
    assert_eq!(
        std::fs::read(fixture.source.join(ENTERPRISE_CONFIG_FILE)).unwrap(),
        source_config
    );
    assert_eq!(
        std::fs::read(fixture.source.join("archive.db")).unwrap(),
        source_db
    );
    verify_backup(&backup).unwrap();
}

#[test]
fn busy_directory_and_corrupt_media_do_not_publish_a_backup() {
    let fixture = Fixture::new();
    let lock = lock_data_root(&fixture.source).unwrap();
    assert!(create_backup(&fixture.source, &fixture.executable, &fixture.output).is_err());
    assert_eq!(std::fs::read_dir(&fixture.output).unwrap().count(), 0);
    drop(lock);
    std::fs::write(
        fixture.source.join("media").join(&fixture.hash),
        b"corrupt media",
    )
    .unwrap();
    assert!(create_backup(&fixture.source, &fixture.executable, &fixture.output).is_err());
    assert_eq!(std::fs::read_dir(&fixture.output).unwrap().count(), 0);
    assert!(fixture.source.join("archive.db").is_file());
}

#[test]
fn tampered_missing_extra_and_traversal_files_are_rejected_before_restore() {
    let fixture = Fixture::new();
    let backup = fixture.backup();
    let manifest = backup.join("backup-manifest.json");
    let original_manifest = std::fs::read(&manifest).unwrap();
    let media = backup.join("serverData/media").join(&fixture.hash);
    let bytes = std::fs::read(&media).unwrap();
    std::fs::write(&media, vec![b'x'; bytes.len()]).unwrap();
    assert!(restore_backup(&backup, &fixture.output).is_err());
    std::fs::remove_file(&media).unwrap();
    assert!(verify_backup(&backup).is_err());
    std::fs::write(&media, bytes).unwrap();
    let extra = backup.join("unexpected.txt");
    std::fs::write(&extra, b"unlisted content").unwrap();
    assert!(verify_backup(&backup).is_err());
    std::fs::remove_file(extra).unwrap();
    for path in [
        "../outside",
        "serverData/../../outside",
        "serverData/C:outside",
        "serverData/NUL",
        "serverData/./alias",
        "serverData\\escape",
    ] {
        let mut tampered: serde_json::Value = serde_json::from_slice(&original_manifest).unwrap();
        tampered["files"][0]["path"] = json!(path);
        std::fs::write(&manifest, serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(restore_backup(&backup, &fixture.output).is_err());
    }
    std::fs::write(&manifest, original_manifest).unwrap();
    verify_backup(&backup).unwrap();
    assert_eq!(std::fs::read_dir(&fixture.output).unwrap().count(), 1);
}

#[test]
fn undecryptable_credentials_cannot_be_restored_even_with_a_matching_file_hash() {
    let fixture = Fixture::new();
    let backup = fixture.backup();
    let token = backup.join("serverData/access-token.dpapi");
    std::fs::write(&token, b"not a DPAPI credential").unwrap();
    let path = backup.join("backup-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let file = manifest["files"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["path"] == "serverData/access-token.dpapi")
        .unwrap();
    file["bytes"] = json!(std::fs::metadata(&token).unwrap().len());
    file["sha256"] = json!(sha256_file(&token).unwrap());
    std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(
        restore_backup(&backup, &fixture.output)
            .unwrap_err()
            .contains("无法解密")
    );
    assert_eq!(std::fs::read_dir(&fixture.output).unwrap().count(), 1);
}

#[test]
fn preexisting_files_and_nested_backup_destinations_are_preserved() {
    let fixture = Fixture::new();
    let unrelated = fixture.output.join("existing.txt");
    std::fs::write(&unrelated, b"preserve this").unwrap();
    assert!(create_backup(&fixture.source, &fixture.executable, &fixture.source).is_err());
    let backup = fixture.backup();
    assert!(restore_backup(&backup, &backup).is_err());
    assert_eq!(std::fs::read(unrelated).unwrap(), b"preserve this");
    verify_backup(&backup).unwrap();
}

#[test]
fn relative_data_directories_share_the_same_lock_as_their_absolute_path() {
    let relative = PathBuf::from("target").join(format!("maintenance-lock-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&relative).unwrap();
    let directory = TestDirectory(relative.canonicalize().unwrap());
    let guard = lock_data_root(&relative).unwrap();
    assert!(lock_data_root(&directory.0).is_err());
    drop(guard);
    assert!(lock_data_root(&directory.0).is_ok());
}

#[cfg(windows)]
#[test]
fn unreadable_source_and_locked_restored_config_preserve_originals_and_pending_review() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let source_config = fixture.source.join(ENTERPRISE_CONFIG_FILE);
    let before = std::fs::read(&source_config).unwrap();
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&source_config)
        .unwrap();
    assert!(create_backup(&fixture.source, &fixture.executable, &fixture.output).is_err());
    assert_eq!(std::fs::read_dir(&fixture.output).unwrap().count(), 0);
    drop(locked);
    assert_eq!(std::fs::read(&source_config).unwrap(), before);
    let backup = fixture.backup();
    let restored = PathBuf::from(restore_backup(&backup, &fixture.output).unwrap().directory);
    let root = restored.join("serverData");
    let target = root.join(ENTERPRISE_CONFIG_FILE);
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&target)
        .unwrap();
    assert!(activate_restored(&root).is_err());
    assert!(root.join(RESTORE_PENDING_FILE).is_file());
    assert_eq!(std::fs::read(&target).unwrap(), before);
    drop(locked);
    activate_restored(&root).unwrap();
}
