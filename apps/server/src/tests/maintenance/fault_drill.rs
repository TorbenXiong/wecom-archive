//! Faults operate exclusively on synthetic fixtures and owned child processes.
use super::*;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tree(root: &Path) -> Vec<(PathBuf, String)> {
    let mut result = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            // SQLite may create empty WAL/shared-memory sidecars on a read-only
            // WAL snapshot connection; they are not persistent archive content.
            if matches!(
                path.file_name().unwrap().to_str(),
                Some("workspace.lock" | "archive.db-wal" | "archive.db-shm")
            ) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!source_windows::atomic_file::is_link(&metadata));
            if metadata.is_dir() {
                pending.push(path);
            } else {
                result.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    sha256_file(&path).unwrap(),
                ));
            }
        }
    }
    result.sort();
    result
}

#[test]
#[ignore = "subprocess worker; invoked only by the bounded maintenance crash drill"]
fn process_worker() {
    let root = PathBuf::from(
        std::env::var_os("WECOM_MAINTENANCE_FAULT_ROOT").expect("FAULT_ROOT_REQUIRED"),
    );
    assert_eq!(
        fs::read(root.join(".fault-owned")).unwrap(),
        b"maintenance-fault.v1"
    );
    let output = root.join("interrupted");
    match std::env::var("WECOM_MAINTENANCE_FAULT_PHASE")
        .unwrap()
        .as_str()
    {
        "backup" => {
            create_backup(
                &root.join("serverData"),
                &root.join("WeComArchive.exe"),
                &output,
            )
            .unwrap();
        }
        "restore" => {
            let backup = fs::read_to_string(root.join("backup-location")).unwrap();
            restore_backup(Path::new(&backup), &output).unwrap();
        }
        _ => panic!("UNKNOWN_FAULT_PHASE"),
    }
}

#[test]
fn killed_backup_and_restore_leave_rejected_partials_and_allow_independent_retry() {
    let fixture = Fixture::new();
    // A bounded synthetic executable supplies a visible copying/hashing window.
    fs::File::options()
        .write(true)
        .open(&fixture.executable)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let source_before = tree(&fixture.source);
    let backup = fixture.backup();
    let backup_before = tree(&backup);
    fs::write(
        fixture._base.0.join(".fault-owned"),
        b"maintenance-fault.v1",
    )
    .unwrap();
    fs::write(
        fixture._base.0.join("backup-location"),
        backup.to_str().unwrap(),
    )
    .unwrap();
    for phase in ["backup", "restore"] {
        let output = fixture._base.0.join("interrupted");
        fs::create_dir_all(&output).unwrap();
        let prior: Vec<PathBuf> = fs::read_dir(&output)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::maintenance::fault_drill::process_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WECOM_MAINTENANCE_FAULT_ROOT", &fixture._base.0)
                .env("WECOM_MAINTENANCE_FAULT_PHASE", phase)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let partial = loop {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "FAULT_WORKER_PUBLISHED_BEFORE_INTERRUPTION"
            );
            let found = fs::read_dir(&output)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    !prior.contains(p)
                        && p.extension().is_some_and(|e| e == "partial")
                        && fs::metadata(p.join("WeComArchive.exe")).is_ok_and(|m| m.len() > 0)
                });
            if let Some(path) = found {
                break path;
            }
            assert!(Instant::now() < deadline, "FAULT_CHECKPOINT_TIMEOUT");
            std::thread::sleep(Duration::from_millis(1));
        };
        worker.0.kill().unwrap();
        assert!(!worker.0.wait().unwrap().success());
        let new: Vec<_> = fs::read_dir(&output)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| !prior.contains(p))
            .collect();
        assert_eq!(new, vec![partial.clone()]);
        assert!(verify_backup(&partial).is_err());
        assert!(restore_backup(&partial, &fixture.output).is_err());
        // The retry must neither adopt nor overwrite the interrupted directory.
        let leftover = tree(&partial);
        if phase == "backup" {
            let retry = PathBuf::from(
                create_backup(&fixture.source, &fixture.executable, &output)
                    .unwrap()
                    .directory,
            );
            verify_backup(&retry).unwrap();
        } else {
            let retry = PathBuf::from(restore_backup(&backup, &output).unwrap().directory);
            assert!(
                retry
                    .join("serverData")
                    .join(RESTORE_PENDING_FILE)
                    .is_file()
            );
            ArchiveStore::open_read_only(&retry.join("serverData/archive.db"))
                .unwrap()
                .verify_integrity()
                .unwrap();
        }
        assert_eq!(tree(&partial), leftover);
        assert_eq!(tree(&fixture.source), source_before);
        assert_eq!(tree(&backup), backup_before);
        verify_backup(&backup).unwrap();
    }
}

/// Only the VHD wrapper supplies this environment. This is never run by default.
#[test]
#[ignore = "requires the administrator-created bounded virtual NTFS test volume"]
fn bounded_volume_full_backup_and_restore_fail_without_publication_then_retry() {
    let root =
        PathBuf::from(std::env::var_os("WECOM_FAULT_VOLUME").expect("BOUNDED_VOLUME_REQUIRED"));
    assert_eq!(
        fs::read(root.join(".bounded-fault-volume")).unwrap(),
        b"bounded-volume.v1"
    );
    let fixture = Fixture::new();
    fs::File::options()
        .write(true)
        .open(&fixture.executable)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let source_before = tree(&fixture.source);
    let backup = fixture.backup();
    let backup_before = tree(&backup);
    let output = root.join(format!("maintenance-{}", Uuid::new_v4()));
    fs::create_dir(&output).unwrap();
    let sentinel = output.join("existing");
    fs::write(&sentinel, b"preserve existing file").unwrap();
    for phase in ["backup", "restore"] {
        let filler = root.join(format!("filler-{}", Uuid::new_v4()));
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
        // Windows ERROR_HANDLE_DISK_FULL / ERROR_DISK_FULL; fail for unrelated errors.
        assert!(
            matches!(error.raw_os_error(), Some(39 | 112)),
            "EXPECTED_WINDOWS_DISK_FULL"
        );
        println!(
            "PASS NTFS_MAINTENANCE_VOLUME_FULL phase={phase} os_error={} fill_bytes={}",
            error.raw_os_error().unwrap(),
            fill.metadata().unwrap().len()
        );
        // Leave room for staging and the DB, but less than the 32 MiB copy.
        fill.set_len(
            fill.metadata()
                .unwrap()
                .len()
                .checked_sub(1024 * 1024)
                .unwrap(),
        )
        .unwrap();
        fill.sync_all().unwrap();
        drop(fill);
        let result = if phase == "backup" {
            create_backup(&fixture.source, &fixture.executable, &output)
        } else {
            restore_backup(&backup, &output)
        };
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve existing file");
        fs::remove_file(&filler).unwrap();
        if phase == "backup" {
            let retry = PathBuf::from(
                create_backup(&fixture.source, &fixture.executable, &output)
                    .unwrap()
                    .directory,
            );
            verify_backup(&retry).unwrap();
            // Only this newly created child of the validated test output is removed.
            assert_eq!(
                retry.canonicalize().unwrap().parent(),
                Some(output.canonicalize().unwrap().as_path())
            );
            fs::remove_dir_all(retry).unwrap();
        } else {
            let retry = PathBuf::from(restore_backup(&backup, &output).unwrap().directory);
            assert!(
                retry
                    .join("serverData")
                    .join(RESTORE_PENDING_FILE)
                    .is_file()
            );
        }
        assert_eq!(tree(&fixture.source), source_before);
        assert_eq!(tree(&backup), backup_before);
    }
    println!("PASS BOUNDED_NTFS_FULL_BACKUP_RESTORE_AND_RETRY");
}
