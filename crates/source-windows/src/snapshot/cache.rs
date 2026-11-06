use super::{
    SnapshotOptions, cleanup_run_root, make_private_copy_writable, snapshot_with_copy, source_files,
};
use archive_domain::{DomainError, SnapshotReceipt, SourceCandidate};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BLOCK_BYTES: usize = 64 * 1024;
const READ_BYTES: usize = 1024 * 1024;

/// Exclusively owns the working files until parsing finishes. Cache contents
/// are untrusted hints: every byte is compared with the live source each run.
pub struct CachedSnapshot {
    receipt: SnapshotReceipt,
    cache: Option<CacheLease>,
    written_bytes: u64,
}

impl CachedSnapshot {
    pub fn create(
        candidate: &SourceCandidate,
        work_root: &Path,
        cache_root: &Path,
        options: SnapshotOptions,
    ) -> Result<Self, DomainError> {
        // A busy, missing or unusable cache must never prevent collection.
        let cache = CacheLease::acquire(candidate, cache_root).ok();
        let mut written_bytes = 0;
        let receipt = snapshot_with_copy(candidate, work_root, options, |source, target| {
            if let Some(cache) = &cache {
                cache.seed(source, target);
                if target.try_exists()? {
                    make_private_copy_writable(target)?;
                    if let Ok((bytes, written)) = update_copy(source, target) {
                        written_bytes += written;
                        return Ok(bytes);
                    }
                    // Discard an unusable private seed and retry with the
                    // normal copy path; a cache failure cannot certify data.
                    fs::remove_file(target)?;
                }
            }
            let bytes = fs::copy(source, target)?;
            written_bytes += bytes;
            Ok(bytes)
        })?;
        Ok(Self {
            receipt,
            cache,
            written_bytes,
        })
    }

    pub fn receipt(&self) -> &SnapshotReceipt {
        &self.receipt
    }

    /// Logical bytes written by successful copy passes, not OS physical I/O.
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes
    }
}

impl Drop for CachedSnapshot {
    fn drop(&mut self) {
        if let Some(cache) = &self.cache {
            cache.store(&self.receipt);
        }
        if let Some(work_root) = self.receipt.snapshot_root.parent() {
            cleanup_run_root(work_root, &self.receipt.snapshot_root);
        }
    }
}

struct CacheLease {
    files: PathBuf,
    names: BTreeSet<std::ffi::OsString>,
    _lock: File,
}

impl CacheLease {
    fn acquire(candidate: &SourceCandidate, root: &Path) -> io::Result<Self> {
        checked_directory(root)?;
        // Include the actual source paths so account IDs/aliases cannot make
        // different sources share mutable working files.
        let mut hash = Sha256::new();
        hash.update(b"snapshot-cache.v1");
        hash.update(candidate.source_id.as_bytes());
        let paths = source_files(candidate);
        for path in paths.keys() {
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let root = root.join(hex::encode(hash.finalize()));
        checked_directory(&root)?;
        let lock_path = root.join("lease.lock");
        if lock_path.try_exists()? && !regular_file(&lock_path)? {
            return Err(io::Error::other("invalid snapshot cache lock"));
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open a raced-in reparse point itself rather than its target.
            options.custom_flags(0x0020_0000);
        }
        let lock = options.open(lock_path)?;
        lock.try_lock()
            .map_err(|_| io::Error::other("snapshot cache busy"))?;
        let files = root.join("files");
        checked_directory(&files)?;
        let names = paths
            .keys()
            .filter_map(|path| path.file_name().map(|name| name.to_os_string()))
            .collect::<BTreeSet<_>>();
        for entry in fs::read_dir(&files)? {
            let entry = entry?;
            if !names.contains(&entry.file_name()) || !regular_file(&entry.path())? {
                return Err(io::Error::other("invalid snapshot cache entry"));
            }
        }
        Ok(Self {
            files,
            names,
            _lock: lock,
        })
    }

    fn seed(&self, source: &Path, target: &Path) {
        if target.exists() {
            return;
        }
        if let Some(name) = source.file_name() {
            let cached = self.files.join(name);
            if regular_file(&cached).unwrap_or(false) {
                // Rename consumes the cache file; no hard links or second
                // writable reader can share it with the current snapshot.
                let _ = fs::rename(cached, target);
            }
        }
    }

    fn store(&self, receipt: &SnapshotReceipt) {
        // The cache is expendable. Partial return, process exit or disk errors
        // only cost a future full copy; no cache generation is trusted as data.
        for name in &self.names {
            let cached = self.files.join(name);
            if regular_file(&cached).unwrap_or(false) {
                let _ = make_private_copy_writable(&cached);
                let _ = fs::remove_file(cached);
            }
        }
        for source in &receipt.copied_files {
            if let Some(name) = source.file_name()
                && self.names.contains(name)
                && regular_file(source).unwrap_or(false)
            {
                let _ = make_private_copy_writable(source);
                let _ = fs::rename(source, self.files.join(name));
            }
        }
    }
}

fn checked_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(io::Error::other("invalid snapshot cache directory"));
    }
    Ok(())
}

fn regular_file(path: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || is_link(&metadata) {
        return Ok(false);
    }
    single_link(&File::open(path)?)
}

fn single_link(file: &File) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::os::windows::io::AsRawHandle;

        #[repr(C)]
        #[derive(Default)]
        struct StandardInfo {
            allocation_size: i64,
            end_of_file: i64,
            links: u32,
            delete_pending: u8,
            directory: u8,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFileInformationByHandleEx(
                handle: *mut c_void,
                class: i32,
                info: *mut c_void,
                length: u32,
            ) -> i32;
        }
        let mut info = StandardInfo::default();
        // SAFETY: FileStandardInfo (1) receives a correctly laid out, writable
        // FILE_STANDARD_INFO buffer; the file handle remains open throughout.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                1,
                (&mut info as *mut StandardInfo).cast(),
                std::mem::size_of::<StandardInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(info.links == 1 && info.directory == 0)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink() == 1)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Ok(false)
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Restore a private, potentially stale or damaged cache file from the source.
/// Both streams are read in full; only unequal blocks are written. Truncation
/// also removes stale tail bytes. No source metadata shortcut is used here.
fn update_copy(source: &Path, target: &Path) -> io::Result<(u64, u64)> {
    let mut input = File::open(source)?;
    let length = input.metadata()?.len();
    let mut output = OpenOptions::new().read(true).write(true).open(target)?;
    if !single_link(&output)? {
        return Err(io::Error::other("snapshot copy has multiple links"));
    }
    let previous_length = output.metadata()?.len();
    let mut old_input = File::open(target)?;
    // Large sequential reads avoid a syscall for every comparison block;
    // keep the write granularity small when just a few SQLite pages changed.
    let mut current = vec![0; READ_BYTES];
    let mut previous = vec![0; READ_BYTES];
    let mut offset = 0;
    let mut written = 0;
    while offset < length {
        let count = (length - offset).min(READ_BYTES as u64) as usize;
        input.read_exact(&mut current[..count])?;
        let old_count = previous_length.saturating_sub(offset).min(count as u64) as usize;
        old_input.read_exact(&mut previous[..old_count])?;
        for start in (0..count).step_by(BLOCK_BYTES) {
            let end = (start + BLOCK_BYTES).min(count);
            if end > old_count || current[start..end] != previous[start..end] {
                output.seek(SeekFrom::Start(offset + start as u64))?;
                output.write_all(&current[start..end])?;
                written += (end - start) as u64;
            }
        }
        offset += count as u64;
    }
    if previous_length != length {
        output.set_len(length)?;
    }
    Ok((length, written))
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{SourceCapability, SourceDatabase};
    use std::time::Duration;

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

    fn snapshot(candidate: &SourceCandidate, root: &Path) -> CachedSnapshot {
        CachedSnapshot::create(
            candidate,
            &root.join("work"),
            &root.join("cache"),
            SnapshotOptions {
                attempts: 3,
                initial_backoff: Duration::ZERO,
            },
        )
        .unwrap()
    }

    fn copied_bytes(snapshot: &CachedSnapshot) -> Vec<u8> {
        fs::read(snapshot.receipt().snapshot_root.join("message.db")).unwrap()
    }

    #[test]
    fn unchanged_cycles_write_no_database_blocks_and_changed_blocks_are_refreshed() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        let source = &candidate.databases[0].path;
        let mut bytes = vec![42; 2 * 1024 * 1024 + 7];
        fs::write(source, &bytes).unwrap();
        let first = snapshot(&candidate, root.path());
        assert!(first.cache.is_some());
        assert_eq!(first.written_bytes(), bytes.len() as u64);
        drop(first);
        assert_eq!(fs::read_dir(root.path().join("work")).unwrap().count(), 0);
        let second = snapshot(&candidate, root.path());
        assert_eq!(second.written_bytes(), 0);
        assert_eq!(copied_bytes(&second), bytes);
        drop(second);

        // Preserve both size and timestamp: metadata must not certify reuse.
        let modified = fs::metadata(source).unwrap().modified().unwrap();
        bytes[BLOCK_BYTES + 11] = 77;
        fs::write(source, &bytes).unwrap();
        File::options()
            .write(true)
            .open(source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let changed = snapshot(&candidate, root.path());
        assert_eq!(changed.written_bytes(), BLOCK_BYTES as u64);
        assert_eq!(copied_bytes(&changed), bytes);
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn stale_parser_modified_and_truncated_cache_contents_are_fully_repaired() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        let source = &candidate.databases[0].path;
        let bytes = vec![9; BLOCK_BYTES * 2 + 13];
        fs::write(source, &bytes).unwrap();
        let first = snapshot(&candidate, root.path());
        let work = first.receipt().snapshot_root.clone();
        fs::write(work.join("message.db"), b"synthetic damaged cache").unwrap();
        fs::write(work.join("decrypted.db"), b"synthetic derived plaintext").unwrap();
        drop(first);
        assert!(!work.exists());
        let repaired = snapshot(&candidate, root.path());
        assert_eq!(copied_bytes(&repaired), bytes);
        assert_eq!(repaired.written_bytes(), bytes.len() as u64);
        assert!(
            !repaired
                .receipt()
                .snapshot_root
                .join("decrypted.db")
                .exists()
        );
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn source_growth_truncation_and_sidecar_lifecycle_do_not_leave_stale_bytes() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        let source = &candidate.databases[0].path;
        let wal = root.path().join("message.db-wal");
        fs::write(source, vec![3; BLOCK_BYTES + 11]).unwrap();
        fs::write(&wal, b"old synthetic WAL").unwrap();
        drop(snapshot(&candidate, root.path()));
        for length in [BLOCK_BYTES * 3 + 17, BLOCK_BYTES + 5, 0, 37] {
            let bytes = vec![3; length];
            fs::write(source, &bytes).unwrap();
            fs::remove_file(&wal).ok();
            let current = snapshot(&candidate, root.path());
            assert_eq!(copied_bytes(&current), bytes);
            assert!(
                !current
                    .receipt()
                    .snapshot_root
                    .join("message.db-wal")
                    .exists()
            );
            drop(current);
        }
        fs::write(&wal, b"new synthetic WAL").unwrap();
        let current = snapshot(&candidate, root.path());
        assert_eq!(
            fs::read(current.receipt().snapshot_root.join("message.db-wal")).unwrap(),
            b"new synthetic WAL"
        );
        assert_eq!(current.written_bytes(), b"new synthetic WAL".len() as u64);
    }

    #[test]
    fn busy_cache_uses_independent_full_copy_until_the_lease_is_returned() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        fs::write(&candidate.databases[0].path, b"synthetic database").unwrap();
        let first = snapshot(&candidate, root.path());
        let concurrent = snapshot(&candidate, root.path());
        assert!(concurrent.cache.is_none());
        assert_ne!(
            first.receipt().snapshot_root,
            concurrent.receipt().snapshot_root
        );
        fs::write(
            concurrent.receipt().snapshot_root.join("message.db"),
            b"private edit",
        )
        .unwrap();
        assert_eq!(copied_bytes(&first), b"synthetic database");
        drop(concurrent);
        drop(first);
        let next = snapshot(&candidate, root.path());
        assert!(next.cache.is_some());
        assert_eq!(next.written_bytes(), 0);
        assert_eq!(copied_bytes(&next), b"synthetic database");
    }

    #[test]
    fn missing_source_and_invalid_cache_do_not_publish_or_delete_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        let cache = root.path().join("cache");
        fs::write(&cache, b"synthetic unrelated file").unwrap();
        fs::write(&candidate.databases[0].path, b"synthetic database").unwrap();
        let current = snapshot(&candidate, root.path());
        assert!(current.cache.is_none());
        assert_eq!(copied_bytes(&current), b"synthetic database");
        drop(current);
        assert_eq!(fs::read(&cache).unwrap(), b"synthetic unrelated file");
        fs::remove_file(&cache).unwrap();
        drop(snapshot(&candidate, root.path()));
        fs::remove_file(&candidate.databases[0].path).unwrap();
        assert!(
            CachedSnapshot::create(
                &candidate,
                &root.path().join("work"),
                &cache,
                SnapshotOptions::default()
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(root.path().join("work")).unwrap().count(), 0);
        fs::write(&candidate.databases[0].path, b"new synthetic database").unwrap();
        assert_eq!(
            copied_bytes(&snapshot(&candidate, root.path())),
            b"new synthetic database"
        );
    }

    #[test]
    fn hardlinked_cache_files_are_rejected_without_mutating_the_source() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate(root.path());
        let source = &candidate.databases[0].path;
        fs::write(source, b"synthetic original database").unwrap();
        let first = snapshot(&candidate, root.path());
        let files = first.cache.as_ref().unwrap().files.clone();
        drop(first);
        let cached = files.join("message.db");
        fs::remove_file(&cached).unwrap();
        fs::hard_link(source, &cached).unwrap();
        let current = snapshot(&candidate, root.path());
        assert!(current.cache.is_none());
        fs::write(
            current.receipt().snapshot_root.join("message.db"),
            b"private edit",
        )
        .unwrap();
        assert_eq!(fs::read(source).unwrap(), b"synthetic original database");
        assert_eq!(fs::read(cached).unwrap(), b"synthetic original database");
    }

    #[test]
    fn identical_account_ids_at_different_paths_have_separate_cache_leases() {
        let root = tempfile::tempdir().unwrap();
        let first_root = root.path().join("first");
        let second_root = root.path().join("second");
        fs::create_dir(&first_root).unwrap();
        fs::create_dir(&second_root).unwrap();
        let first = candidate(&first_root);
        let second = candidate(&second_root);
        fs::write(&first.databases[0].path, b"first synthetic account").unwrap();
        fs::write(&second.databases[0].path, b"other synthetic account").unwrap();
        let a = snapshot(&first, root.path());
        let b = snapshot(&second, root.path());
        assert!(a.cache.is_some() && b.cache.is_some());
        assert_ne!(
            a.cache.as_ref().unwrap().files,
            b.cache.as_ref().unwrap().files
        );
        assert_eq!(copied_bytes(&a), b"first synthetic account");
        assert_eq!(copied_bytes(&b), b"other synthetic account");
    }
}
