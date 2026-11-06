use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use archive_domain::{DomainError, SnapshotReceipt, SourceCandidate};
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod cache;
pub use cache::CachedSnapshot;

#[derive(Debug, Clone, Copy)]
pub struct SnapshotOptions {
    pub attempts: u8,
    pub initial_backoff: Duration,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial_backoff: Duration::from_millis(75),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

pub fn create_consistent_snapshot(
    candidate: &SourceCandidate,
    work_root: &Path,
    options: SnapshotOptions,
) -> Result<SnapshotReceipt, DomainError> {
    snapshot_with_copy(candidate, work_root, options, |source, target| {
        fs::copy(source, target)
    })
}

fn snapshot_with_copy(
    candidate: &SourceCandidate,
    work_root: &Path,
    options: SnapshotOptions,
    mut copy: impl FnMut(&Path, &Path) -> io::Result<u64>,
) -> Result<SnapshotReceipt, DomainError> {
    fs::create_dir_all(work_root).map_err(io_error)?;
    let canonical_work_root = work_root.canonicalize().map_err(io_error)?;
    let run_id = Uuid::new_v4();
    let run_root = canonical_work_root.join(run_id.to_string());
    fs::create_dir(&run_root).map_err(io_error)?;
    let mut run = SnapshotRun {
        work_root: canonical_work_root,
        root: run_root,
        published: false,
    };
    let attempts = options.attempts.max(1);
    let mut copied_stamps = BTreeMap::new();

    for attempt in 0..attempts {
        let before = metadata_map(candidate)?;
        let targets = snapshot_targets(&before, &run.root)?;
        // SQLite opens sidecars by name, even if absent from copied_files.
        // Remove sidecars that disappeared before retrying in this directory.
        remove_obsolete_files(&run.root, &targets)?;
        copied_stamps.retain(|path, stamp| before.get(path) == Some(stamp));
        let result = copy_files(&before, &targets, &mut copied_stamps, &mut copy);
        let after = metadata_map(candidate)?;
        // Only copies stable over the entire attempt may survive a retry.
        // This cache never outlives this unpublished, private snapshot.
        copied_stamps.retain(|path, stamp| after.get(path) == Some(stamp));

        if result.is_ok() && before == after {
            run.published = true;
            return Ok(SnapshotReceipt {
                run_id,
                snapshot_root: run.root.clone(),
                copied_files: targets.into_values().collect(),
                source_fingerprint: fingerprint(&before),
            });
        }

        if attempt + 1 < attempts {
            let multiplier = 1_u32.checked_shl(u32::from(attempt)).unwrap_or(u32::MAX);
            thread::sleep(options.initial_backoff.saturating_mul(multiplier));
        }
    }
    Err(DomainError::UnstableSource)
}

struct SnapshotRun {
    work_root: PathBuf,
    root: PathBuf,
    published: bool,
}

impl Drop for SnapshotRun {
    fn drop(&mut self) {
        if !self.published {
            cleanup_run_root(&self.work_root, &self.root);
        }
    }
}

fn source_files(candidate: &SourceCandidate) -> BTreeMap<PathBuf, bool> {
    let mut paths = BTreeMap::new();
    for database in &candidate.databases {
        paths.insert(database.path.clone(), true);
        if let Some(path) = &database.wal_path {
            paths.entry(path.clone()).or_insert(false);
        }
        if let Some(path) = &database.shm_path {
            paths.entry(path.clone()).or_insert(false);
        }
        // Discovery may predate sidecar creation or a WAL checkpoint.
        for suffix in ["-wal", "-shm"] {
            let mut path = database.path.as_os_str().to_os_string();
            path.push(suffix);
            paths.entry(PathBuf::from(path)).or_insert(false);
        }
    }
    paths
}

fn metadata_map(candidate: &SourceCandidate) -> Result<BTreeMap<PathBuf, FileStamp>, DomainError> {
    let mut stamps = BTreeMap::new();
    for (path, required) in source_files(candidate) {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(error)),
        };
        stamps.insert(
            path,
            FileStamp {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            },
        );
    }
    Ok(stamps)
}

fn snapshot_targets(
    stamps: &BTreeMap<PathBuf, FileStamp>,
    root: &Path,
) -> Result<BTreeMap<PathBuf, PathBuf>, DomainError> {
    let mut names = BTreeSet::new();
    let mut targets = BTreeMap::new();
    for source in stamps.keys() {
        let file_name = source
            .file_name()
            .ok_or_else(|| DomainError::Io("source file has no name".into()))?;
        if !names.insert(file_name.to_string_lossy().to_lowercase()) {
            return Err(DomainError::Io("snapshot file names collide".into()));
        }
        targets.insert(source.clone(), root.join(file_name));
    }
    Ok(targets)
}

fn remove_obsolete_files(
    root: &Path,
    targets: &BTreeMap<PathBuf, PathBuf>,
) -> Result<(), DomainError> {
    let expected = targets.values().collect::<BTreeSet<_>>();
    for entry in fs::read_dir(root).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if !expected.contains(&path) {
            fs::remove_file(path).map_err(io_error)?;
        }
    }
    Ok(())
}

fn copy_files(
    stamps: &BTreeMap<PathBuf, FileStamp>,
    targets: &BTreeMap<PathBuf, PathBuf>,
    copied: &mut BTreeMap<PathBuf, FileStamp>,
    copy: &mut impl FnMut(&Path, &Path) -> io::Result<u64>,
) -> io::Result<()> {
    for (source, stamp) in stamps {
        if stamp.modified.is_some() && copied.get(source) == Some(stamp) {
            continue;
        }
        copied.remove(source);
        let result = copy(source, &targets[source]);
        // Windows copies the source's read-only attribute, including when a
        // copy fails after creating its destination. Only our private copy
        // must be writable for a retry and for eventual snapshot cleanup.
        make_private_copy_writable(&targets[source])?;
        if result? != stamp.len {
            return Err(io::Error::other("source changed while copying"));
        }
        copied.insert(source.clone(), stamp.clone());
    }
    Ok(())
}

fn make_private_copy_writable(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
        }

        const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
        const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("snapshot copy is not a regular file"));
        }
        if metadata.permissions().readonly() {
            // Change only the Windows read-only attribute, preserving the
            // other attributes and avoiding Unix permission semantics.
            let attributes = metadata.file_attributes() & !FILE_ATTRIBUTE_READONLY;
            let attributes = if attributes == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attributes
            };
            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            // SAFETY: wide is a live, NUL-terminated Windows path. Attribute
            // changes apply only to the regular file in our private run.
            if unsafe { SetFileAttributesW(wide.as_ptr(), attributes) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

fn cleanup_run_root(work_root: &Path, run_root: &Path) {
    let safe = run_root.parent() == Some(work_root) && run_root.starts_with(work_root);
    if safe {
        let _ = fs::remove_dir_all(run_root);
    }
}

fn fingerprint(metadata: &BTreeMap<PathBuf, FileStamp>) -> String {
    let mut hasher = Sha256::new();
    for (path, stamp) in metadata {
        if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            hasher.update(name.as_bytes());
        }
        hasher.update(stamp.len.to_le_bytes());
        if let Some(modified) = stamp
            .modified
            .and_then(|value| value.duration_since(SystemTime::UNIX_EPOCH).ok())
        {
            hasher.update(modified.as_nanos().to_le_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

fn io_error(error: std::io::Error) -> DomainError {
    DomainError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{SourceCapability, SourceDatabase};
    use std::io::Write;

    fn candidate(root: &Path) -> SourceCandidate {
        SourceCandidate {
            source_id: "synthetic".into(),
            display_path: "synthetic".into(),
            root_path: root.into(),
            databases: vec![SourceDatabase {
                kind: "message".into(),
                path: root.join("message.db"),
                wal_path: None,
                shm_path: None,
                encrypted: true,
                page_size_hint: Some(4096),
            }],
            media_roots: vec![],
            client_version: None,
            capability: SourceCapability::Supported,
        }
    }

    fn options(attempts: u8) -> SnapshotOptions {
        SnapshotOptions {
            attempts,
            initial_backoff: Duration::ZERO,
        }
    }

    #[test]
    fn retries_copy_only_changed_sidecars_and_preserve_final_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut candidate = candidate(directory.path());
        let database = candidate.databases[0].path.clone();
        let wal = directory.path().join("message.db-wal");
        let shm = directory.path().join("message.db-shm");
        candidate.databases[0].wal_path = Some(wal.clone());
        candidate.databases[0].shm_path = Some(shm.clone());
        let database_bytes = vec![42; 2 * 1024 * 1024];
        fs::write(&database, &database_bytes).unwrap();
        fs::write(&wal, vec![1; 1024]).unwrap();
        fs::write(&shm, vec![2; 256]).unwrap();
        let mut counts = BTreeMap::<PathBuf, u64>::new();
        let mut copied_bytes = 0;
        let receipt = snapshot_with_copy(
            &candidate,
            &directory.path().join("work"),
            options(3),
            |source, target| {
                let count = counts.entry(source.into()).or_default();
                *count += 1;
                let bytes = fs::copy(source, target)?;
                copied_bytes += bytes;
                if source == wal && *count < 3 {
                    fs::write(&wal, vec![*count as u8 + 1; 1024 + *count as usize])?;
                }
                Ok(bytes)
            },
        )
        .unwrap();
        assert_eq!(counts[&database], 1);
        assert_eq!(counts[&shm], 1);
        assert_eq!(counts[&wal], 3);
        assert_eq!(
            copied_bytes,
            database_bytes.len() as u64 + 256 + 1024 + 1025 + 1026
        );
        assert_eq!(receipt.copied_files.len(), 3);
        assert_eq!(
            fs::read(receipt.snapshot_root.join("message.db")).unwrap(),
            database_bytes
        );
        for name in ["message.db-wal", "message.db-shm"] {
            assert_eq!(
                fs::read(receipt.snapshot_root.join(name)).unwrap(),
                fs::read(directory.path().join(name)).unwrap()
            );
        }
        assert_eq!(
            receipt.source_fingerprint,
            fingerprint(&metadata_map(&candidate).unwrap())
        );
    }

    #[test]
    fn retry_recopies_same_size_database_changes() {
        let directory = tempfile::tempdir().unwrap();
        let candidate = candidate(directory.path());
        let database = &candidate.databases[0].path;
        let wal = directory.path().join("message.db-wal");
        fs::write(database, vec![1; 4096]).unwrap();
        fs::write(&wal, b"synthetic WAL").unwrap();
        let mut database_copies = 0;
        let mut wal_copies = 0;
        let receipt = snapshot_with_copy(
            &candidate,
            &directory.path().join("work"),
            options(2),
            |source, target| {
                let bytes = fs::copy(source, target)?;
                if source == database {
                    database_copies += 1;
                } else if source == wal {
                    wal_copies += 1;
                    fs::write(database, vec![2; 4096])?;
                    fs::OpenOptions::new()
                        .write(true)
                        .open(database)?
                        .set_times(fs::FileTimes::new().set_modified(
                            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
                        ))?;
                }
                Ok(bytes)
            },
        )
        .unwrap();
        assert_eq!(database_copies, 2);
        assert_eq!(wal_copies, 1);
        assert_eq!(
            fs::read(receipt.snapshot_root.join("message.db")).unwrap(),
            vec![2; 4096]
        );
    }

    #[test]
    fn retries_discover_new_sidecars_and_remove_disappeared_copies() {
        for suffix in ["-wal", "-shm"] {
            for initially_present in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut candidate = candidate(directory.path());
                let database = candidate.databases[0].path.clone();
                let sidecar = directory.path().join(format!("message.db{suffix}"));
                fs::write(&database, b"synthetic database").unwrap();
                if initially_present {
                    fs::write(&sidecar, b"synthetic sidecar").unwrap();
                    if suffix == "-wal" {
                        candidate.databases[0].wal_path = Some(sidecar.clone());
                    } else {
                        candidate.databases[0].shm_path = Some(sidecar.clone());
                    }
                }
                let mut database_copies = 0;
                let receipt = snapshot_with_copy(
                    &candidate,
                    &directory.path().join("work"),
                    options(2),
                    |source, target| {
                        let bytes = fs::copy(source, target)?;
                        if source == database {
                            database_copies += 1;
                            if !initially_present {
                                fs::write(&sidecar, b"new synthetic sidecar")?;
                            }
                        } else if source == sidecar && initially_present {
                            fs::remove_file(&sidecar)?;
                        }
                        Ok(bytes)
                    },
                )
                .unwrap();
                assert_eq!(database_copies, 1);
                assert_eq!(
                    receipt.copied_files.len(),
                    if initially_present { 1 } else { 2 }
                );
                let copied_sidecar = receipt.snapshot_root.join(format!("message.db{suffix}"));
                assert_eq!(copied_sidecar.exists(), !initially_present);
                if !initially_present {
                    assert_eq!(fs::read(copied_sidecar).unwrap(), b"new synthetic sidecar");
                }
            }
        }
    }

    #[test]
    fn failed_and_short_copies_are_never_reused() {
        for fail_with_error in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let candidate = candidate(directory.path());
            let database = &candidate.databases[0].path;
            fs::write(database, b"synthetic complete database").unwrap();
            let mut calls = 0;
            let receipt = snapshot_with_copy(
                &candidate,
                &directory.path().join("work"),
                options(2),
                |source, target| {
                    calls += 1;
                    if calls == 1 {
                        fs::write(target, b"short")?;
                        return if fail_with_error {
                            Err(io::Error::other("synthetic copy failure"))
                        } else {
                            Ok(5)
                        };
                    }
                    fs::copy(source, target)
                },
            )
            .unwrap();
            assert_eq!(calls, 2);
            assert_eq!(
                fs::read(receipt.snapshot_root.join("message.db")).unwrap(),
                fs::read(database).unwrap()
            );
        }
    }

    #[test]
    fn failures_clean_only_the_current_unpublished_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let candidate = candidate(directory.path());
        let database = &candidate.databases[0].path;
        let work = directory.path().join("work");
        let sibling = work.join("other-run");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("keep"), b"synthetic existing snapshot").unwrap();
        fs::write(database, b"synthetic database").unwrap();
        let result = snapshot_with_copy(&candidate, &work, options(2), |source, target| {
            let bytes = fs::copy(source, target)?;
            fs::OpenOptions::new()
                .append(true)
                .open(source)?
                .write_all(b"x")?;
            Ok(bytes)
        });
        assert!(matches!(result, Err(DomainError::UnstableSource)));
        fs::remove_file(database).unwrap();
        assert!(create_consistent_snapshot(&candidate, &work, options(1)).is_err());
        assert_eq!(fs::read_dir(&work).unwrap().count(), 1);
        assert_eq!(
            fs::read(sibling.join("keep")).unwrap(),
            b"synthetic existing snapshot"
        );
    }

    #[test]
    fn separate_runs_copy_fresh_independent_files() {
        let directory = tempfile::tempdir().unwrap();
        let candidate = candidate(directory.path());
        let database = &candidate.databases[0].path;
        let work = directory.path().join("work");
        fs::write(database, b"first synthetic database").unwrap();
        let first = create_consistent_snapshot(&candidate, &work, options(1)).unwrap();
        fs::write(database, b"later synthetic database").unwrap();
        let second = create_consistent_snapshot(&candidate, &work, options(1)).unwrap();
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(
            fs::read(first.snapshot_root.join("message.db")).unwrap(),
            b"first synthetic database"
        );
        assert_eq!(
            fs::read(second.snapshot_root.join("message.db")).unwrap(),
            b"later synthetic database"
        );
        fs::write(
            first.snapshot_root.join("message.db"),
            b"edited private copy",
        )
        .unwrap();
        assert_eq!(fs::read(database).unwrap(), b"later synthetic database");
        assert_eq!(
            fs::read(second.snapshot_root.join("message.db")).unwrap(),
            b"later synthetic database"
        );
    }

    #[test]
    fn colliding_file_names_are_rejected_before_copying() {
        let directory = tempfile::tempdir().unwrap();
        let mut candidate = candidate(directory.path());
        fs::write(&candidate.databases[0].path, b"first synthetic database").unwrap();
        let other = directory.path().join("other");
        fs::create_dir(&other).unwrap();
        let mut database = candidate.databases[0].clone();
        database.path = other.join("MESSAGE.db");
        fs::write(&database.path, b"second synthetic database").unwrap();
        candidate.databases.push(database);
        let work = directory.path().join("work");
        let result = snapshot_with_copy(&candidate, &work, options(1), |_, _| {
            panic!("colliding snapshot must not start copying")
        });
        assert!(result.is_err());
        assert_eq!(fs::read_dir(work).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn retries_can_replace_readonly_private_copies_without_changing_source_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let candidate = candidate(directory.path());
        let database = &candidate.databases[0].path;
        fs::write(database, b"synthetic readonly database").unwrap();
        let original = fs::metadata(database).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(database, readonly).unwrap();
        let mut calls = 0;
        let work = directory.path().join("work");
        let result = snapshot_with_copy(&candidate, &work, options(2), |source, target| {
            calls += 1;
            let bytes = fs::copy(source, target)?;
            if calls == 1 {
                Err(io::Error::other(
                    "synthetic failure after partial completion",
                ))
            } else {
                Ok(bytes)
            }
        });
        let source_still_readonly = fs::metadata(database).unwrap().permissions().readonly();
        fs::set_permissions(database, original).unwrap();
        let receipt = result.unwrap();
        assert!(source_still_readonly);
        assert_eq!(calls, 2);
        assert_eq!(
            fs::read(receipt.snapshot_root.join("message.db")).unwrap(),
            b"synthetic readonly database"
        );
        fs::remove_dir_all(receipt.snapshot_root).unwrap();
        assert_eq!(fs::read_dir(work).unwrap().count(), 0);
    }

    #[test]
    fn retry_without_a_modification_time_always_copies_again() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("synthetic-source");
        let target = directory.path().join("synthetic-target");
        let stamps = BTreeMap::from([(
            source.clone(),
            FileStamp {
                len: 4,
                modified: None,
            },
        )]);
        let targets = BTreeMap::from([(source, target)]);
        let mut copied = stamps.clone();
        let mut calls = 0;
        copy_files(&stamps, &targets, &mut copied, &mut |_, _| {
            calls += 1;
            Ok(4)
        })
        .unwrap();
        assert_eq!(calls, 1);
    }
}
