//! Offline synthetic comparison. No source discovery or real account access.
use archive_domain::{SourceCandidate, SourceCapability, SourceDatabase};
use archive_store::ArchiveStore;
use sha2::{Digest, Sha256};
use source_windows::{CachedSnapshot, SnapshotOptions, create_consistent_snapshot};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

struct WorkDirectory {
    root: PathBuf,
    parent: PathBuf,
}

impl WorkDirectory {
    fn new() -> std::io::Result<Self> {
        let parent = std::env::temp_dir().canonicalize()?;
        let root = parent.join(format!("snapshot-copy-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        Ok(Self { root, parent })
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        if self.root.parent() == Some(self.parent.as_path()) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn digest(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().to_vec())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mib = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "64".into())
        .parse::<usize>()?;
    if !(1..=256).contains(&mib) {
        return Err("synthetic size must be 1..=256 MiB".into());
    }
    let root = WorkDirectory::new()?;
    // Keep the production SQLite symbol provider linked in debug builds too;
    // source-windows includes an optional native key-verification bridge.
    let _native_provider = ArchiveStore::open(&root.path().join("provider.db"))?;
    let source = root.path().join("message.db");
    let mut block = [0; 64 * 1024];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = ((index * 37 + index / 127) % 251) as u8;
    }
    let mut output = File::create(&source)?;
    for _ in 0..mib * 16 {
        output.write_all(&block)?;
    }
    drop(output);
    let candidate = SourceCandidate {
        source_id: "synthetic".into(),
        display_path: "synthetic".into(),
        root_path: root.path().into(),
        databases: vec![SourceDatabase {
            kind: "message".into(),
            path: source.clone(),
            wal_path: None,
            shm_path: None,
            encrypted: true,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let work = root.path().join("work");
    let cache = root.path().join("cache");
    let options = SnapshotOptions::default();
    drop(CachedSnapshot::create(&candidate, &work, &cache, options)?);
    for changed in [false, true] {
        let mut full_times = Vec::new();
        let mut cached_times = Vec::new();
        let mut cached_written = Vec::new();
        for sample in 0..5 {
            if changed {
                let mut file = File::options().write(true).open(&source)?;
                file.seek(SeekFrom::Start(64 * 1024 + 11))?;
                file.write_all(&[sample as u8 + 150])?;
            }
            let expected = digest(&source)?;
            // Alternate ordering so both paths see warmed file-system caches.
            for cached_first in [sample % 2 == 0, sample % 2 != 0] {
                let start = Instant::now();
                if cached_first {
                    let snapshot = CachedSnapshot::create(&candidate, &work, &cache, options)?;
                    cached_times.push(start.elapsed().as_micros());
                    cached_written.push(snapshot.written_bytes());
                    assert_eq!(
                        snapshot.written_bytes(),
                        if changed { 64 * 1024 } else { 0 }
                    );
                    assert_eq!(
                        digest(&snapshot.receipt().snapshot_root.join("message.db"))?,
                        expected
                    );
                    drop(snapshot);
                } else {
                    let snapshot = create_consistent_snapshot(&candidate, &work, options)?;
                    full_times.push(start.elapsed().as_micros());
                    assert_eq!(
                        digest(&snapshot.snapshot_root.join("message.db"))?,
                        expected
                    );
                    fs::remove_dir_all(snapshot.snapshot_root)?;
                }
            }
        }
        full_times.sort();
        cached_times.sort();
        println!(
            "scenario={} mib={} samples=5 full_p50_us={} cached_p50_us={} full_written_bytes={} cached_written_bytes={:?}",
            if changed {
                "one-block-changed"
            } else {
                "unchanged"
            },
            mib,
            full_times[2],
            cached_times[2],
            mib * 1024 * 1024,
            cached_written
        );
    }
    Ok(())
}
