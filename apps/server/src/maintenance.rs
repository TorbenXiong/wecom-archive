//! Offline maintenance. These operations never start a listener or a collector.
use crate::{ACCESS_TOKEN_FILE, DATA_ROOT_ENV, ENTERPRISE_CONFIG_FILE, EnterpriseConfig};
use archive_store::{ArchiveStore, StoreError};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use source_windows::{atomic_file, secrets};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) const RESTORE_PENDING_FILE: &str = "restore-pending.json";
const MANIFEST: &str = "backup-manifest.json";
const FAILED: &str = "备份或恢复未完成，原数据已保留。请检查磁盘空间、目录权限及文件占用后重试。";
const INVALID: &str = "备份校验失败：文件、数据库、附件或格式不完整。原数据未修改。";
const CREDENTIALS: &str =
    "无法解密备份凭据。请使用备份所属电脑及 Windows 用户恢复，不会重新生成身份或密钥。";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Manifest {
    schema: String,
    version: String,
    created_at: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ManifestFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceResult {
    pub directory: String,
    pub file_count: usize,
    pub bytes: u64,
}

pub fn data_root() -> PathBuf {
    let root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(crate::default_data_root);
    root.canonicalize().unwrap_or(root)
}

pub fn restore_pending() -> Result<bool, String> {
    let local = crate::default_data_root();
    if local
        .join(RESTORE_PENDING_FILE)
        .try_exists()
        .map_err(|_| FAILED)?
        && data_root().canonicalize().ok() != local.canonicalize().ok()
    {
        return Err("检测到本程序旁的待恢复数据，但 WECOM_ARCHIVE_SERVER_DATA 指向其他目录。请先取消该环境变量，再打开恢复后的程序。".into());
    }
    data_root()
        .join(RESTORE_PENDING_FILE)
        .try_exists()
        .map_err(|_| FAILED.into())
}

pub fn backup_location() -> Result<bool, String> {
    std::env::current_exe()
        .map_err(|_| FAILED.to_owned())?
        .parent()
        .ok_or(FAILED)?
        .join(MANIFEST)
        .try_exists()
        .map_err(|_| FAILED.into())
}

pub(crate) fn lock_data_root(root: &Path) -> Result<File, String> {
    // Existing deployments may configure a relative directory. Resolve it
    // before locking so aliases cannot obtain independent workspace locks.
    let root = root.canonicalize().map_err(|_| FAILED)?;
    if root
        .parent()
        .ok_or(FAILED)?
        .join(MANIFEST)
        .try_exists()
        .map_err(|_| FAILED)?
    {
        return Err("此目录属于完整备份，仅可校验或恢复到新目录，不可作为运行数据目录。".into());
    }
    checked_directory(&root).map_err(|_| FAILED)?;
    atomic_file::lock_exclusive(&root.join("workspace.lock"))
        .map_err(|_| "数据目录正在使用或无法锁定，请先退出使用此目录的工作台再重试。".into())
}

fn checked_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(INVALID.into());
    }
    #[cfg(windows)]
    if source_windows::roots::normalize_source_root(path.to_str().ok_or(INVALID)?).is_none() {
        return Err(INVALID.into());
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| INVALID)?;
        if !metadata.is_dir() || atomic_file::is_link(&metadata) {
            return Err(INVALID.into());
        }
    }
    Ok(())
}

fn checked_file(path: &Path) -> Result<(), String> {
    checked_directory(path.parent().ok_or(INVALID)?)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| INVALID)?;
    if !metadata.is_file() || atomic_file::is_link(&metadata) {
        return Err(INVALID.into());
    }
    Ok(())
}

fn relative_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.contains('\\') || value.chars().any(char::is_control) {
        return Err(INVALID.into());
    }
    for part in value.split('/') {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part.contains([':', '<', '>', '"', '|', '?', '*'])
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(INVALID.into());
        }
    }
    if value != "WeComArchive.exe"
        && value != "source-roots.dpapi"
        && !value.starts_with("serverData/")
    {
        return Err(INVALID.into());
    }
    if let Some(relative) = value.strip_prefix("serverData/")
        && excluded_source_entry(relative.split('/').next().ok_or(INVALID)?)
    {
        return Err(INVALID.into());
    }
    Ok(PathBuf::from(value))
}

fn excluded_source_entry(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "work"
            | "imports"
            | "snapshot-cache"
            | "workspace.lock"
            | "archive.db-wal"
            | "archive.db-shm"
            | RESTORE_PENDING_FILE
    )
}

fn files_in(root: &Path, skip_runtime: bool) -> Result<Vec<String>, String> {
    checked_directory(root)?;
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        if directory.components().count() > 128 {
            return Err(INVALID.into());
        }
        for entry in fs::read_dir(&directory).map_err(|_| FAILED)? {
            let path = entry.map_err(|_| FAILED)?.path();
            if skip_runtime
                && directory == root
                && path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(excluded_source_entry)
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| FAILED)?;
            if atomic_file::is_link(&metadata) {
                return Err(INVALID.into());
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(root)
                        .map_err(|_| INVALID)?
                        .to_str()
                        .ok_or(INVALID)?
                        .replace('\\', "/"),
                );
            } else {
                return Err(INVALID.into());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn copy_file(source: &Path, target: &Path) -> Result<(), String> {
    checked_file(source)?;
    fs::create_dir_all(target.parent().ok_or(INVALID)?).map_err(|_| FAILED)?;
    checked_directory(target.parent().ok_or(INVALID)?)?;
    let mut input = File::open(source).map_err(|_| FAILED)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|_| FAILED)?;
    std::io::copy(&mut input, &mut output).map_err(|_| FAILED)?;
    output.sync_all().map_err(|_| FAILED.into())
}

struct OwnedDirectory {
    path: PathBuf,
    parent: PathBuf,
    published: bool,
}
impl OwnedDirectory {
    fn new(parent: &Path) -> Result<Self, String> {
        checked_directory(parent)?;
        let parent = parent.canonicalize().map_err(|_| FAILED)?;
        let path = parent.join(format!(".wecom-{}.partial", Uuid::new_v4().simple()));
        fs::create_dir(&path).map_err(|_| FAILED)?;
        Ok(Self {
            path,
            parent,
            published: false,
        })
    }
    fn publish(mut self, kind: &str) -> Result<PathBuf, String> {
        let target = self.parent.join(format!(
            "WeComArchive-{kind}-{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            Uuid::new_v4().simple()
        ));
        if target.try_exists().map_err(|_| FAILED)? {
            return Err(FAILED.into());
        }
        // Windows directory rename refuses an existing target. The whole tree
        // becomes visible only after verification, including on cancellation.
        fs::rename(&self.path, &target).map_err(|_| FAILED)?;
        self.published = true;
        Ok(target)
    }
}
impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        if !self.published
            && checked_directory(&self.path).is_ok()
            && self
                .path
                .canonicalize()
                .is_ok_and(|p| p.parent() == Some(self.parent.as_path()))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn create_backup(
    root: &Path,
    executable: &Path,
    destination_parent: &Path,
) -> Result<MaintenanceResult, String> {
    let _lock = lock_data_root(root)?;
    checked_file(executable)?;
    checked_directory(destination_parent)?;
    if destination_parent
        .canonicalize()
        .map_err(|_| FAILED)?
        .starts_with(root.canonicalize().map_err(|_| FAILED)?)
    {
        return Err("请选择服务端数据目录以外的备份位置。".into());
    }
    let stage = OwnedDirectory::new(destination_parent)?;
    let server = stage.path.join("serverData");
    fs::create_dir(&server).map_err(|_| FAILED)?;
    for relative in files_in(root, true)? {
        if relative == "archive.db" {
            continue;
        }
        relative_path(&format!("serverData/{relative}"))?;
        copy_file(&root.join(&relative), &server.join(&relative))?;
    }
    checked_file(&root.join("archive.db"))?;
    ArchiveStore::backup_to(&root.join("archive.db"), &server.join("archive.db"))
        .map_err(|_| FAILED)?;
    copy_file(executable, &stage.path.join("WeComArchive.exe"))?;
    let roots = executable
        .parent()
        .ok_or(INVALID)?
        .join("source-roots.dpapi");
    if roots.try_exists().map_err(|_| FAILED)? {
        copy_file(&roots, &stage.path.join("source-roots.dpapi"))?;
    }
    let mut manifest = Manifest {
        schema: "wecom-backup.v1".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        created_at: Utc::now().to_rfc3339(),
        files: Vec::new(),
    };
    for path in files_in(&stage.path, false)? {
        relative_path(&path)?;
        let file = stage.path.join(&path);
        manifest.files.push(ManifestFile {
            path,
            bytes: fs::metadata(&file).map_err(|_| FAILED)?.len(),
            sha256: crate::sha256_file(&file).map_err(|_| FAILED)?,
        });
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| FAILED)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.path.join(MANIFEST))
        .map_err(|_| FAILED)?;
    output
        .write_all(&bytes)
        .and_then(|_| output.sync_all())
        .map_err(|_| FAILED)?;
    drop(output);
    verify_backup_contents(&stage.path)?;
    let target = stage.publish("backup")?;
    Ok(result(&target, &manifest))
}

fn read_manifest(root: &Path) -> Result<Manifest, String> {
    checked_file(&root.join(MANIFEST))?;
    let file = File::open(root.join(MANIFEST)).map_err(|_| INVALID)?;
    if file.metadata().map_err(|_| INVALID)?.len() > 64 * 1024 * 1024 {
        return Err(INVALID.into());
    }
    let manifest: Manifest =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(|_| INVALID)?;
    if manifest.schema != "wecom-backup.v1" || manifest.version != env!("CARGO_PKG_VERSION") {
        return Err("备份格式或程序版本不匹配，请使用备份对应版本恢复。".into());
    }
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        relative_path(&file.path)?;
        if !paths.insert(file.path.to_lowercase()) || !crate::is_valid_content_hash(&file.sha256) {
            return Err(INVALID.into());
        }
    }
    for required in [
        "WeComArchive.exe",
        "serverData/archive.db",
        "serverData/enterprise-config.json",
        "serverData/access-token.dpapi",
    ] {
        if !manifest.files.iter().any(|f| f.path == required) {
            return Err(INVALID.into());
        }
    }
    Ok(manifest)
}

pub fn verify_backup(root: &Path) -> Result<MaintenanceResult, String> {
    if root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".partial"))
    {
        return Err("该目录尚未完成备份，请选择已完成的备份目录。".into());
    }
    verify_backup_contents(root)
}

fn verify_backup_contents(root: &Path) -> Result<MaintenanceResult, String> {
    let manifest = read_manifest(root)?;
    let mut expected = manifest
        .files
        .iter()
        .map(|f| f.path.clone())
        .collect::<Vec<_>>();
    expected.push(MANIFEST.into());
    expected.sort();
    if files_in(root, false)? != expected {
        return Err(INVALID.into());
    }
    for file in &manifest.files {
        let path = root.join(relative_path(&file.path)?);
        checked_file(&path)?;
        if fs::metadata(&path).map_err(|_| INVALID)?.len() != file.bytes
            || crate::sha256_file(&path).map_err(|_| INVALID)? != file.sha256
        {
            return Err(INVALID.into());
        }
    }
    validate_payload(root)?;
    Ok(result(root, &manifest))
}

fn validate_payload(root: &Path) -> Result<(), String> {
    validate_server(&root.join("serverData"))?;
    let roots = root.join("source-roots.dpapi");
    if roots.try_exists().map_err(|_| INVALID)? {
        let _plain = Zeroizing::new(
            secrets::unprotect(&fs::read(roots).map_err(|_| INVALID)?).map_err(|_| CREDENTIALS)?,
        );
    }
    Ok(())
}

fn validate_server(server: &Path) -> Result<(), String> {
    checked_directory(server)?;
    for name in [ENTERPRISE_CONFIG_FILE, ACCESS_TOKEN_FILE, "archive.db"] {
        checked_file(&server.join(name))?;
    }
    let config: EnterpriseConfig = serde_json::from_slice(
        &fs::read(server.join(ENTERPRISE_CONFIG_FILE)).map_err(|_| INVALID)?,
    )
    .map_err(|_| INVALID)?;
    let token = Zeroizing::new(
        secrets::unprotect(&fs::read(server.join(ACCESS_TOKEN_FILE)).map_err(|_| INVALID)?)
            .map_err(|_| CREDENTIALS)?,
    );
    if std::str::from_utf8(&token).map_err(|_| CREDENTIALS)?.len() < 24 {
        return Err(CREDENTIALS.into());
    }
    let keys = std::iter::once(&config.private_key_protected_hex)
        .chain(std::iter::once(&config.signing_private_key_protected_hex))
        .chain(
            config
                .key_history
                .iter()
                .map(|r| &r.private_key_protected_hex),
        )
        .chain(
            config
                .collectors
                .iter()
                .filter(|r| !r.private_key_protected_hex.is_empty())
                .map(|r| &r.private_key_protected_hex),
        );
    for key in keys {
        let protected = crate::decode_hex(key).map_err(|_| CREDENTIALS)?;
        let private = Zeroizing::new(secrets::unprotect(&protected).map_err(|_| CREDENTIALS)?);
        if private.is_empty() {
            return Err(CREDENTIALS.into());
        }
    }
    let store = ArchiveStore::open_read_only(&server.join("archive.db")).map_err(|_| INVALID)?;
    store.verify_integrity().map_err(|_| INVALID)?;
    store
        .visit_verified_media(|hash, size| {
            let invalid = || StoreError::Invalid("BACKUP_MEDIA_INVALID".into());
            if !crate::is_valid_content_hash(hash) {
                return Err(invalid());
            }
            let media = server.join("media").join(hash);
            checked_file(&media).map_err(|_| invalid())?;
            if size.is_some_and(|s| fs::metadata(&media).map(|m| m.len() != s).unwrap_or(true))
                || crate::sha256_file(&media).map_err(|_| invalid())? != hash
            {
                return Err(invalid());
            }
            Ok(())
        })
        .map_err(|_| INVALID)?;
    Ok(())
}

pub fn restore_backup(
    backup: &Path,
    destination_parent: &Path,
) -> Result<MaintenanceResult, String> {
    verify_backup(backup)?;
    let manifest = read_manifest(backup)?;
    checked_directory(destination_parent)?;
    if destination_parent
        .canonicalize()
        .map_err(|_| FAILED)?
        .starts_with(backup.canonicalize().map_err(|_| FAILED)?)
    {
        return Err("恢复位置不能在备份目录内部。".into());
    }
    let stage = OwnedDirectory::new(destination_parent)?;
    for file in &manifest.files {
        let relative = relative_path(&file.path)?;
        let target = stage.path.join(&relative);
        copy_file(&backup.join(relative), &target)?;
        if fs::metadata(&target).map_err(|_| INVALID)?.len() != file.bytes
            || crate::sha256_file(&target).map_err(|_| INVALID)? != file.sha256
        {
            return Err(INVALID.into());
        }
    }
    validate_payload(&stage.path)?;
    atomic_file::write(
        &stage.path.join("serverData").join(RESTORE_PENDING_FILE),
        b"{\"schema\":\"restore-review.v1\"}",
    )
    .map_err(|_| FAILED)?;
    let target = stage.publish("restored")?;
    Ok(result(&target, &manifest))
}

pub fn activate_restored(root: &Path) -> Result<(), String> {
    let _lock = lock_data_root(root)?;
    checked_file(&root.join(RESTORE_PENDING_FILE))?;
    let pending: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(RESTORE_PENDING_FILE)).map_err(|_| INVALID)?)
            .map_err(|_| INVALID)?;
    if pending["schema"] != "restore-review.v1" {
        return Err(INVALID.into());
    }
    validate_server(root)?;
    let mut config: EnterpriseConfig =
        serde_json::from_slice(&fs::read(root.join(ENTERPRISE_CONFIG_FILE)).map_err(|_| INVALID)?)
            .map_err(|_| INVALID)?;
    for plan in &mut config.local_collection_plans {
        plan.schedule = Default::default();
    }
    config.server_schedule = Default::default();
    config.collector_schedule = Default::default();
    config.upload_url = crate::DEFAULT_SERVER_URL.into();
    for collector in &mut config.collectors {
        collector.enabled = false;
        collector.schedule = Default::default();
        collector.manual_collection_request_id = None;
        collector.software_update_request = None;
        collector.config_revision = collector.config_revision.checked_add(1).ok_or(INVALID)?;
    }
    crate::persist_enterprise_config(root, &config).map_err(|_| FAILED)?;
    fs::remove_file(root.join(RESTORE_PENDING_FILE)).map_err(|_| FAILED.into())
}

fn result(root: &Path, manifest: &Manifest) -> MaintenanceResult {
    MaintenanceResult {
        directory: root.to_string_lossy().into(),
        file_count: manifest.files.len(),
        bytes: manifest.files.iter().map(|f| f.bytes).sum(),
    }
}
