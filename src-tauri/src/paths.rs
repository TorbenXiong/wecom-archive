use crate::state::{CommandError, internal_error};
use archive_domain::SourceCandidate;
use chrono::Local;
use serde::{Deserialize, Serialize};
use source_windows::roots::normalize_source_root;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub(super) static OFFLINE_EXPORT_FILE_LOCK: Mutex<()> = Mutex::new(());

pub(super) static SOURCE_ROOTS_FILE_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn next_offline_export_file(directory: &Path) -> (String, PathBuf) {
    loop {
        let file_name = format!("wecom-{}.wca", Local::now().format("%Y%m%d-%H%M%S"));
        let target = directory.join(&file_name);
        if !target.exists() {
            return (file_name, target);
        }
        // A repeated export in the same second must never replace the first file.
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn collector_data_root() -> Result<PathBuf, CommandError> {
    std::env::current_exe()
        .map_err(|_| internal_error())?
        .parent()
        .map(|parent| parent.join("collectorData"))
        .ok_or_else(internal_error)
}

pub(super) fn collector_runtime_config_path(collector_id: &str) -> Result<PathBuf, CommandError> {
    let collector_id = uuid::Uuid::parse_str(collector_id).map_err(|_| internal_error())?;
    Ok(collector_data_root()?
        .join("instances")
        .join(collector_id.to_string())
        .join("collector-config.dpapi"))
}

pub(super) fn legacy_collector_runtime_config_path() -> Result<PathBuf, CommandError> {
    Ok(collector_data_root()?
        .parent()
        .ok_or_else(internal_error)?
        .join("collector-config.dpapi"))
}

pub(super) fn collector_work_root(prefix: &str) -> Result<PathBuf, CommandError> {
    create_work_root(&collector_data_root()?, prefix)
}

pub(super) fn local_data_root(executable_directory: &Path, configured: Option<PathBuf>) -> PathBuf {
    configured.unwrap_or_else(|| executable_directory.join("serverData"))
}

pub(super) fn create_work_root(data_root: &Path, prefix: &str) -> Result<PathBuf, CommandError> {
    let root = data_root
        .join("work")
        .join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).map_err(|_| internal_error())?;
    Ok(root)
}

pub(super) const REMEMBERED_SOURCE_ROOTS_SCHEMA: u32 = 1;

pub(super) const MAX_REMEMBERED_SOURCE_ROOTS: usize = 3;

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct RememberedSourceRoots {
    pub(super) schema_version: u32,
    pub(super) roots: Vec<PathBuf>,
}

pub(super) fn source_roots_config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("source-roots.dpapi")
}

pub(super) fn load_remembered_source_roots(path: &Path) -> Vec<PathBuf> {
    let Ok(plaintext) = source_windows::dpapi::load_current_user(path) else {
        return Vec::new();
    };
    let Ok(stored) = serde_json::from_slice::<RememberedSourceRoots>(&plaintext) else {
        return Vec::new();
    };
    if stored.schema_version != REMEMBERED_SOURCE_ROOTS_SCHEMA {
        return Vec::new();
    }
    stored
        .roots
        .into_iter()
        .filter_map(|root| source_windows::roots::resolve_source_root(&root))
        .take(MAX_REMEMBERED_SOURCE_ROOTS)
        .collect()
}

pub(super) fn remember_source_root(path: &Path, root: &Path) -> Result<(), String> {
    // Local collection and quick export can finish concurrently. Serialize the
    // read-modify-write so they don't share a DPAPI partial file or lose roots.
    let _guard = SOURCE_ROOTS_FILE_LOCK
        .lock()
        .map_err(|_| "无法保存企业微信数据目录。".to_owned())?;
    let mut roots = vec![root.to_path_buf()];
    roots.extend(
        load_remembered_source_roots(path)
            .into_iter()
            .filter(|existing| existing != root),
    );
    roots.truncate(MAX_REMEMBERED_SOURCE_ROOTS);
    let payload = serde_json::to_vec(&RememberedSourceRoots {
        schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA,
        roots,
    })
    .map_err(|_| "无法保存企业微信数据目录。".to_owned())?;
    source_windows::dpapi::store_current_user(path, &payload)
        .map_err(|_| "无法保存企业微信数据目录。".to_owned())
}

pub(super) fn collection_roots(
    pasted_root: Option<&str>,
    remembered: &[PathBuf],
    detected: &[PathBuf],
) -> Vec<PathBuf> {
    // An explicit retry must never silently collect a different account from
    // another root. Automatic discovery still combines saved and default roots.
    if let Some(raw) = pasted_root {
        return normalize_source_root(raw).into_iter().collect();
    }
    let mut roots = Vec::new();
    for root in remembered.iter().chain(detected) {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    roots
}

pub(super) fn discover_local_sources() -> Vec<SourceCandidate> {
    source_windows::discover_roots(&collection_roots(
        None,
        &load_remembered_source_roots(&source_roots_config_path()),
        &source_windows::default_roots(),
    ))
}
