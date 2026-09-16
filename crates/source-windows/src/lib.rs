pub mod enterprise_crypto;
mod snapshot;

#[cfg(windows)]
pub mod dialog;
#[cfg(windows)]
pub mod dpapi;
#[cfg(windows)]
pub mod probe;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use archive_domain::{
    DomainError, SnapshotReceipt, SourceAdapter, SourceCandidate, SourceCapability, SourceDatabase,
};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub use snapshot::{SnapshotOptions, create_consistent_snapshot};

pub const ADAPTER_ID: &str = "windows-local-v1";
const DATABASE_NAMES: &[(&str, &str)] = &[
    ("message.db", "message"),
    ("session.db", "session"),
    ("user.db", "user"),
];

#[derive(Debug, Default)]
pub struct WindowsSourceAdapter;

impl SourceAdapter for WindowsSourceAdapter {
    fn adapter_id(&self) -> &'static str {
        ADAPTER_ID
    }

    fn discover(
        &self,
        selected_root: Option<PathBuf>,
    ) -> Result<Vec<SourceCandidate>, DomainError> {
        let roots = match selected_root {
            Some(root) => vec![root],
            None => default_roots(),
        };
        Ok(roots
            .into_iter()
            .filter(|root| root.is_dir())
            .flat_map(|root| discover_under(&root))
            .collect())
    }

    fn snapshot(
        &self,
        candidate: &SourceCandidate,
        work_root: PathBuf,
    ) -> Result<SnapshotReceipt, DomainError> {
        create_consistent_snapshot(candidate, &work_root, SnapshotOptions::default())
    }
}

fn discover_under(root: &Path) -> Vec<SourceCandidate> {
    let mut grouped: BTreeMap<PathBuf, Vec<SourceDatabase>> = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let Some(file_name) = entry.file_name().to_str() else {
            continue;
        };
        let Some((_, kind)) = DATABASE_NAMES
            .iter()
            .find(|(expected, _)| file_name.eq_ignore_ascii_case(expected))
        else {
            continue;
        };
        let path = entry.into_path();
        let parent = path.parent().unwrap_or(root).to_path_buf();
        let header = inspect_header(&path).ok();
        grouped.entry(parent).or_default().push(SourceDatabase {
            kind: (*kind).into(),
            wal_path: sidecar(&path, "-wal"),
            shm_path: sidecar(&path, "-shm"),
            encrypted: !header.as_ref().is_some_and(|value| value.plain_sqlite),
            page_size_hint: header.and_then(|value| value.page_size),
            path,
        });
    }

    let mut candidates = grouped
        .into_iter()
        .filter(|(_, databases)| databases.iter().any(|database| database.kind == "message"))
        .map(|(data_root, mut databases)| {
            databases.sort_by(|left, right| left.kind.cmp(&right.kind));
            let source_id = path_fingerprint(&data_root);
            let display_path = data_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| format!("…\\{name}"))
                .unwrap_or_else(|| "已选择的数据目录".into());
            let media_roots = ["File", "Image", "Video", "Voice", "Media"]
                .iter()
                .map(|name| data_root.join(name))
                .filter(|path| path.is_dir())
                .collect();
            let capability = if databases.len() >= 3 {
                SourceCapability::ProbeRequired
            } else {
                SourceCapability::Unsupported
            };
            SourceCandidate {
                source_id,
                display_path,
                root_path: data_root,
                databases,
                media_roots,
                client_version: None,
                capability,
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        message_database_size(right)
            .cmp(&message_database_size(left))
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    candidates
}

fn message_database_size(candidate: &SourceCandidate) -> u64 {
    candidate
        .databases
        .iter()
        .find(|database| database.kind == "message")
        .and_then(|database| database.path.metadata().ok())
        .map_or(0, |metadata| metadata.len())
}

#[derive(Debug)]
struct HeaderInfo {
    plain_sqlite: bool,
    page_size: Option<u32>,
}

fn inspect_header(path: &Path) -> std::io::Result<HeaderInfo> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header)?;
    let plain_sqlite = &header[..16] == b"SQLite format 3\0";
    let page_size = if plain_sqlite {
        let encoded = u16::from_be_bytes([header[16], header[17]]);
        Some(if encoded == 1 {
            65_536
        } else {
            u32::from(encoded)
        })
    } else {
        let candidates = [512_u32, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536];
        let encoded = u16::from_be_bytes([header[16], header[17]]);
        let value = if encoded == 1 {
            65_536
        } else {
            u32::from(encoded)
        };
        candidates.contains(&value).then_some(value)
    };
    Ok(HeaderInfo {
        plain_sqlite,
        page_size,
    })
}

fn sidecar(database: &Path, suffix: &str) -> Option<PathBuf> {
    let name = database.file_name()?.to_str()?;
    let path = database.with_file_name(format!("{name}{suffix}"));
    path.is_file().then_some(path)
}

fn path_fingerprint(path: &Path) -> String {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    format!("src-{}", &hex::encode(digest)[..20])
}

fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(app_data) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(app_data).join("Tencent").join("WXWork"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        roots.push(PathBuf::from(profile).join("Documents").join("WXWork"));
    }
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovers_split_database_layout() {
        let directory = tempdir().unwrap();
        let data = directory.path().join("account").join("Data");
        fs::create_dir_all(&data).unwrap();
        for name in ["message.db", "session.db", "user.db"] {
            let mut bytes = vec![0_u8; 100];
            bytes[..16].copy_from_slice(b"SQLite format 3\0");
            bytes[16..18].copy_from_slice(&4096_u16.to_be_bytes());
            fs::write(data.join(name), bytes).unwrap();
        }
        fs::write(data.join("message.db-wal"), b"fixture").unwrap();

        let candidates = WindowsSourceAdapter
            .discover(Some(directory.path().to_path_buf()))
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].databases.len(), 3);
        assert!(
            candidates[0]
                .databases
                .iter()
                .any(|database| database.wal_path.is_some())
        );
    }
}
