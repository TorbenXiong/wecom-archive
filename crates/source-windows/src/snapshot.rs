use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use archive_domain::{DomainError, SnapshotReceipt, SourceCandidate};
use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    fs::create_dir_all(work_root).map_err(io_error)?;
    let canonical_work_root = work_root.canonicalize().map_err(io_error)?;
    let attempts = options.attempts.max(1);

    for attempt in 0..attempts {
        let run_id = Uuid::new_v4();
        let run_root = canonical_work_root.join(run_id.to_string());
        fs::create_dir(&run_root).map_err(io_error)?;
        let sources = source_files(candidate);
        let before = metadata_map(&sources)?;
        let result = copy_files(&sources, &run_root);
        let after = metadata_map(&sources)?;

        if let Ok(copied_files) = result
            && before == after
        {
            return Ok(SnapshotReceipt {
                run_id,
                snapshot_root: run_root,
                copied_files,
                source_fingerprint: fingerprint(&before),
            });
        }

        cleanup_run_root(&canonical_work_root, &run_root);
        if attempt + 1 < attempts {
            let multiplier = 1_u32 << u32::from(attempt);
            thread::sleep(options.initial_backoff.saturating_mul(multiplier));
        }
    }
    Err(DomainError::UnstableSource)
}

fn source_files(candidate: &SourceCandidate) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for database in &candidate.databases {
        paths.push(database.path.clone());
        if let Some(path) = &database.wal_path {
            paths.push(path.clone());
        }
        if let Some(path) = &database.shm_path {
            paths.push(path.clone());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn metadata_map(paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, FileStamp>, DomainError> {
    paths
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path).map_err(io_error)?;
            Ok((
                path.clone(),
                FileStamp {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                },
            ))
        })
        .collect()
}

fn copy_files(paths: &[PathBuf], target: &Path) -> Result<Vec<PathBuf>, DomainError> {
    let mut copied = Vec::with_capacity(paths.len());
    for source in paths {
        let file_name = source
            .file_name()
            .ok_or_else(|| DomainError::Io("source file has no name".into()))?;
        let destination = target.join(file_name);
        fs::copy(source, &destination).map_err(io_error)?;
        copied.push(destination);
    }
    Ok(copied)
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
