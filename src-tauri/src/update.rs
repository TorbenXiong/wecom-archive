use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use archive_transfer::software_update::{MAX_EXECUTABLE_BYTES, newer, stable_version, valid_hash};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DEFAULT_API: &str = "https://api.github.com/repos/TorbenXiong/wecom-archive/releases/latest";
const RELEASE_ASSET_NAME: &str = "WeComArchive.exe";
const READY_ENV: &str = "WECOM_ARCHIVE_UPDATE_READY";
static CHECKED: Mutex<Option<UpdateInfo>> = Mutex::new(None);
static INSTALLING: Mutex<bool> = Mutex::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub asset_name: String,
    pub sha256: String,
    #[serde(skip)]
    download_url: String,
}

fn https(url: &str) -> Result<(), String> {
    let parsed = tauri::Url::parse(url).map_err(|_| "更新地址无效。")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("更新地址必须使用不含凭据的 HTTPS URL。".into());
    }
    Ok(())
}

/// Stream to an exclusive local file, with bounded size and no shell expansion.
pub(super) fn download(
    url: &str,
    target: &Path,
    limit: u64,
    token: Option<&str>,
) -> Result<(), String> {
    if token.is_none() {
        https(url)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|_| "无法创建更新临时文件。")?;
    let mut command = Command::new("curl.exe");
    command.args([
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "15",
        "--max-time",
        "600",
        "--max-filesize",
        &limit.to_string(),
        "--user-agent",
        "WeComArchive-Updater",
    ]);
    // Device credentials must never follow redirects or appear in the process list.
    if token.is_some() {
        command.args(["--proto", "=http,https", "--header", "@-"]);
    } else {
        command.args([
            "--location",
            "--max-redirs",
            "5",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
        ]);
    }
    command
        .args(["--url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    source_windows::updater::hide(&mut command);
    let result = (|| {
        let mut child = command.spawn().map_err(|_| "无法启动系统 curl。")?;
        if let Some(token) = token {
            if token.contains(['\r', '\n']) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("更新凭据无效。".into());
            }
            if writeln!(
                child.stdin.take().ok_or("更新请求失败。")?,
                "Authorization: Bearer {token}"
            )
            .is_err()
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err("更新请求失败。".into());
            }
        } else {
            drop(child.stdin.take());
        }
        let mut output = child.stdout.take().ok_or("更新请求失败。")?.take(limit + 1);
        let mut file = file;
        let copied = std::io::copy(&mut output, &mut file);
        if copied.as_ref().is_err() || copied.as_ref().is_ok_and(|size| *size > limit) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("更新文件过大或写入失败。".into());
        }
        let status = child.wait().map_err(|_| "更新下载失败。")?;
        if !status.success() {
            return Err("更新下载失败，请检查网络或 Release 地址。".into());
        }
        file.sync_all().map_err(|_| "更新文件写入失败。")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(target);
    }
    result
}

fn release_info(value: &serde_json::Value, current: &str) -> Result<Option<UpdateInfo>, String> {
    if value.get("draft").and_then(|v| v.as_bool()) == Some(true)
        || value.get("prerelease").and_then(|v| v.as_bool()) == Some(true)
    {
        return Ok(None);
    }
    let version = value["tag_name"]
        .as_str()
        .ok_or("Release 缺少版本号。")?
        .trim_start_matches('v');
    if stable_version(version).is_none() {
        return Err("更新仅支持 x.y.z 正式版本。".into());
    }
    if !newer(current, version) {
        return Ok(None);
    }
    let assets = value["assets"]
        .as_array()
        .or_else(|| value["assets"]["links"].as_array())
        .ok_or("Release 缺少附件。")?;
    let legacy_asset_name = format!("WeComArchive-{version}.exe");
    let (asset_name, asset) = {
        let stable = assets
            .iter()
            .filter(|a| a["name"].as_str() == Some(RELEASE_ASSET_NAME))
            .collect::<Vec<_>>();
        if !stable.is_empty() {
            if stable.len() != 1 {
                return Err(format!("Release 必须包含唯一附件 {RELEASE_ASSET_NAME}。"));
            }
            (RELEASE_ASSET_NAME.to_owned(), stable[0])
        } else {
            let legacy = assets
                .iter()
                .filter(|a| a["name"].as_str() == Some(legacy_asset_name.as_str()))
                .collect::<Vec<_>>();
            if legacy.len() != 1 {
                return Err(format!(
                    "Release 必须包含唯一附件 {RELEASE_ASSET_NAME}（兼容旧版时可使用 {legacy_asset_name}）。"
                ));
            }
            (legacy_asset_name, legacy[0])
        }
    };
    let url = asset["browser_download_url"]
        .as_str()
        .or_else(|| asset["direct_asset_url"].as_str())
        .or_else(|| asset["url"].as_str())
        .ok_or("附件地址无效。")?;
    https(url)?;
    let release_url = value["html_url"]
        .as_str()
        .or_else(|| value["_links"]["self"].as_str())
        .unwrap_or(url);
    https(release_url)?;
    let digest = asset["digest"]
        .as_str()
        .and_then(|s| s.strip_prefix("sha256:"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !digest.is_empty() && !valid_hash(&digest) {
        return Err("Release SHA-256 无效。".into());
    }
    Ok(Some(UpdateInfo {
        current_version: current.into(),
        latest_version: version.into(),
        release_url: release_url.into(),
        asset_name,
        download_url: url.into(),
        sha256: digest,
    }))
}

pub fn check() -> Result<Option<UpdateInfo>, String> {
    let mut checked = CHECKED.lock().map_err(|_| "更新状态不可用。")?;
    *checked = None;
    let api = std::env::var("WECOM_ARCHIVE_UPDATE_URL").unwrap_or_else(|_| DEFAULT_API.into());
    https(&api)?;
    let directory = std::env::temp_dir().join(format!("wecom-release-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory).map_err(|_| "无法准备版本检查。")?;
    let result = (|| {
        let metadata = directory.join("release.json");
        download(&api, &metadata, 2 * 1024 * 1024, None)?;
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(metadata).map_err(|_| "无法读取 Release。")?)
                .map_err(|_| "Release 数据格式无效。")?;
        let Some(mut update) = release_info(&value, env!("CARGO_PKG_VERSION"))? else {
            return Ok(None);
        };
        // GitLab and older GitHub assets have no digest. Require a checksum asset
        // from the same HTTPS release; never accept a checksum from the WebView.
        if update.sha256.is_empty() {
            let name = format!("{}.sha256", update.asset_name);
            let assets = value["assets"]
                .as_array()
                .or_else(|| value["assets"]["links"].as_array())
                .ok_or("Release 缺少附件。")?;
            let asset = assets
                .iter()
                .find(|a| a["name"].as_str() == Some(&name))
                .ok_or("Release 缺少 EXE SHA-256；请发布同名 .exe.sha256 附件。")?;
            let url = asset["browser_download_url"]
                .as_str()
                .or_else(|| asset["direct_asset_url"].as_str())
                .or_else(|| asset["url"].as_str())
                .ok_or("校验文件地址无效。")?;
            let path = directory.join("checksum");
            download(url, &path, 4096, None)?;
            update.sha256 = fs::read_to_string(path)
                .map_err(|_| "校验文件无效。")?
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !valid_hash(&update.sha256) {
                return Err("SHA-256 校验值无效。".into());
            }
        }
        Ok(Some(update))
    })();
    let _ = fs::remove_dir_all(&directory);
    *checked = result.as_ref().ok().and_then(Clone::clone);
    result
}

pub(super) fn hash(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|_| "无法读取更新文件。")?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|_| "无法校验更新文件。")?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub(super) fn validate_executable(path: &Path, expected: &str) -> Result<(), String> {
    if !valid_hash(expected) || hash(path)? != expected.to_ascii_lowercase() {
        return Err("更新文件 SHA-256 校验失败。".into());
    }
    let mut file = File::open(path).map_err(|_| "无法读取更新文件。")?;
    let mut header = [0; 64];
    file.read_exact(&mut header)
        .map_err(|_| "更新文件不是有效 EXE。")?;
    if &header[..2] != b"MZ" {
        return Err("更新文件不是有效 EXE。".into());
    }
    use std::io::{Seek, SeekFrom};
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap());
    file.seek(SeekFrom::Start(u64::from(offset)))
        .map_err(|_| "EXE 头无效。")?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe).map_err(|_| "EXE 头无效。")?;
    if &pe[..4] != b"PE\0\0" || pe[4..] != [0x64, 0x86] {
        return Err("更新文件必须是 Windows x64 EXE。".into());
    }
    Ok(())
}

pub fn install(version: &str) -> Result<(), String> {
    let mut installing = INSTALLING.lock().map_err(|_| "更新状态不可用。")?;
    if *installing {
        return Err("已有更新正在进行。".into());
    }
    let update = CHECKED
        .lock()
        .map_err(|_| "更新状态不可用。")?
        .clone()
        .filter(|u| u.latest_version == version && valid_hash(&u.sha256))
        .ok_or("请先检查并选择可用更新。")?;
    let directory = staging_directory()?;
    let result = (|| {
        let next = directory.join("next.exe");
        download(&update.download_url, &next, MAX_EXECUTABLE_BYTES, None)?;
        validate_executable(&next, &update.sha256)?;
        if archive_transfer::read_enterprise_collector_config(&next).is_ok() {
            return Err("工作台更新不能包含采集端绑定配置。".into());
        }
        schedule(&directory)
    })();
    if result.is_ok() {
        *installing = true;
    } else {
        let _ = fs::remove_dir_all(&directory);
    }
    result
}

pub(super) fn staging_directory() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|_| "无法定位程序。")?;
    let directory = executable
        .parent()
        .ok_or("程序目录无效。")?
        .join(format!(".wecom-update-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory).map_err(|_| "程序所在目录不可写，无法升级。")?;
    Ok(directory)
}

#[derive(Serialize, Deserialize)]
struct Replacement {
    target_name: String,
    sha256: String,
    parent_pid: u32,
    token: String,
}

pub(super) fn schedule(directory: &Path) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|_| "无法定位程序。")?;
    let token = uuid::Uuid::new_v4().to_string();
    let plan = Replacement {
        target_name: executable
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("程序名称无效。")?
            .into(),
        sha256: hash(&directory.join("next.exe"))?,
        parent_pid: std::process::id(),
        token: token.clone(),
    };
    fs::write(
        directory.join("plan.json"),
        serde_json::to_vec(&plan).map_err(|_| "无法创建升级计划。")?,
    )
    .map_err(|_| "无法保存升级计划。")?;
    let helper = directory.join("helper.exe");
    fs::copy(&executable, &helper).map_err(|_| "无法创建替换助手。")?;
    let mut command = Command::new(helper);
    command
        .args(["--apply-update", &token])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    source_windows::updater::hide(&mut command);
    let mut child = command.spawn().map_err(|_| "无法启动替换助手。")?;
    let mut ready = String::new();
    BufReader::new(child.stdout.take().ok_or("替换助手未就绪。")?)
        .read_line(&mut ready)
        .map_err(|_| "替换助手未就绪。")?;
    if ready.trim() != "ready" {
        let _ = child.kill();
        let _ = child.wait();
        return Err("替换助手初始化失败，原程序保留。".into());
    }
    Ok(())
}

fn target_path(directory: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name.contains(['/', '\\', ':'])
        || name == "."
        || name == ".."
        || !name.to_ascii_lowercase().ends_with(".exe")
    {
        return Err("升级目标无效。".into());
    }
    if !directory
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| {
            s.strip_prefix(".wecom-update-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        })
    {
        return Err("升级目录无效。".into());
    }
    Ok(directory.parent().ok_or("升级目录无效。")?.join(name))
}

pub fn run_helper(token: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|_| "无法定位助手。")?;
    let directory = exe.parent().ok_or("助手目录无效。")?;
    let plan: Replacement = serde_json::from_slice(
        &fs::read(directory.join("plan.json")).map_err(|_| "升级计划不可用。")?,
    )
    .map_err(|_| "升级计划无效。")?;
    if plan.token != token {
        return Err("升级授权无效。".into());
    }
    let target = target_path(directory, &plan.target_name)?;
    let next = directory.join("next.exe");
    validate_executable(&next, &plan.sha256)?;
    source_windows::updater::wait_for_exit(plan.parent_pid, 120_000, || {
        println!("ready");
        std::io::stdout().flush()
    })
    .map_err(|_| "等待程序退出超时，原文件未替换。")?;
    // Recheck after shutdown. No paths or scripts supplied by the WebView run here.
    validate_executable(&next, &plan.sha256)?;
    let backup = directory.join("previous.exe");
    replace_with_backup(&target, &next, &backup)?;
    let ready_path = directory.join("started");
    let mut child = match Command::new(&target)
        .current_dir(target.parent().ok_or("程序目录无效。")?)
        .env(READY_ENV, &ready_path)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            rollback(&target, &backup, &next)?;
            let _ = Command::new(&target).spawn();
            return Err("新版本启动失败，已恢复旧版本。".into());
        }
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if ready_path.is_file() {
            let _ = fs::remove_file(directory.join("plan.json"));
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|_| "无法确认新版本状态。")?
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
    rollback(&target, &backup, &next)?;
    let _ = Command::new(&target).spawn();
    Err("新版本未完成启动，已恢复旧版本。".into())
}

fn replace_with_backup(target: &Path, next: &Path, backup: &Path) -> Result<(), String> {
    if backup.exists() {
        return Err("升级备份已存在，已停止替换。".into());
    }
    let mut moved = false;
    for _ in 0..40 {
        if fs::rename(target, backup).is_ok() {
            moved = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !moved {
        return Err("程序文件仍被占用，原文件保留。".into());
    }
    if fs::rename(next, target).is_err() {
        fs::rename(backup, target).map_err(|_| "替换失败，旧文件保存在升级备份目录。")?;
        return Err("更新文件替换失败，旧文件已恢复。".into());
    }
    Ok(())
}

fn rollback(target: &Path, backup: &Path, failed: &Path) -> Result<(), String> {
    fs::rename(target, failed).map_err(|_| "无法移走失败的新版本，旧文件保存在升级备份目录。")?;
    fs::rename(backup, target)
        .map_err(|_| "旧文件保存在升级备份目录，请恢复 previous.exe。".to_owned())
}

pub fn acknowledge_startup() {
    let Some(path) = std::env::var_os(READY_ENV).map(PathBuf::from) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(directory) = path
        .parent()
        .filter(|dir| dir.parent() == exe.parent() && target_path(dir, "test.exe").is_ok())
    else {
        return;
    };
    if path.file_name().is_some_and(|n| n == "started") && fs::write(&path, b"ready").is_ok() {
        let directory = directory.to_path_buf();
        std::thread::spawn(move || {
            for _ in 0..120 {
                if cleanup_completed_update(&directory).unwrap_or(false) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
    }
}

fn cleanup_completed_update(directory: &Path) -> std::io::Result<bool> {
    // The helper removes the plan only after it accepts our startup acknowledgement.
    if directory.join("plan.json").try_exists()? {
        return Ok(false);
    }
    // On Windows the running helper is locked. Delete it first so that a partial
    // recursive deletion cannot remove the rollback backup while it is still running.
    match fs::remove_file(directory.join("helper.exe")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::remove_dir_all(directory)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_selects_exact_asset_and_rejects_invalid_versions() {
        let release = serde_json::json!({"tag_name":"v0.0.11", "html_url":"https://github.com/example/app/releases/0.0.11", "assets":[{"name":"WeComArchive-0.0.11.exe","browser_download_url":"https://github.com/example/app/update.exe", "digest":format!("sha256:{}", "a".repeat(64))}]});
        assert_eq!(
            release_info(&release, "0.0.10")
                .unwrap()
                .unwrap()
                .latest_version,
            "0.0.11"
        );
        assert!(release_info(&release, "0.0.11").unwrap().is_none());
        let mut invalid = release.clone();
        invalid["tag_name"] = "9.bad.0".into();
        assert!(release_info(&invalid, "0.0.10").is_err());
        let mut invalid = release.clone();
        invalid["assets"][0]["name"] = "../WeComArchive.exe".into();
        assert!(release_info(&invalid, "0.0.10").is_err());
        let mut invalid = release;
        invalid["assets"][0]["browser_download_url"] = "http://example.org/evil.exe".into();
        assert!(release_info(&invalid, "0.0.10").is_err());
    }

    #[test]
    fn release_prefers_the_stable_asset_name() {
        let release = serde_json::json!({"tag_name":"0.0.11", "html_url":"https://github.com/example/app/releases/0.0.11", "assets":[{"name":"WeComArchive.exe","browser_download_url":"https://github.com/example/app/update.exe", "digest":format!("sha256:{}", "a".repeat(64))}]});
        let update = release_info(&release, "0.0.10").unwrap().unwrap();
        assert_eq!(update.asset_name, "WeComArchive.exe");
    }

    #[test]
    fn gitlab_release_links_are_supported() {
        let release = serde_json::json!({"tag_name":"0.0.11","assets":{"links":[{"name":"WeComArchive-0.0.11.exe","direct_asset_url":"https://gitlab.example/app/update.exe"}]}});
        assert!(release_info(&release, "0.0.10").unwrap().is_some());
    }

    #[test]
    fn replace_failure_restores_old_executable_and_backup_is_preserved() {
        let dir = std::env::temp_dir().join(format!("update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("app.exe");
        let next = dir.join("next.exe");
        let backup = dir.join("previous.exe");
        fs::write(&old, b"old").unwrap();
        assert!(replace_with_backup(&old, &next, &backup).is_err());
        assert_eq!(fs::read(&old).unwrap(), b"old");
        fs::write(&next, b"new").unwrap();
        replace_with_backup(&old, &next, &backup).unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        rollback(&old, &backup, &next).unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"old");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_paths_cannot_escape_the_executable_directory() {
        let directory = PathBuf::from(format!(".wecom-update-{}", uuid::Uuid::new_v4()));
        for name in ["../app.exe", "C:\\app.exe", "sub/app.exe", "app.exe:stream"] {
            assert!(target_path(&directory, name).is_err());
        }
        assert!(target_path(Path::new("other"), "app.exe").is_err());
    }

    #[test]
    fn cleanup_waits_for_helper_success_and_preserves_sibling_data() {
        let root = std::env::temp_dir().join(format!("update-test-{}", uuid::Uuid::new_v4()));
        let directory = root.join(format!(".wecom-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(root.join("archive.db"), b"data").unwrap();
        for name in ["plan.json", "helper.exe", "previous.exe", "started"] {
            fs::write(directory.join(name), b"test").unwrap();
        }
        assert!(!cleanup_completed_update(&directory).unwrap());
        assert!(directory.join("previous.exe").is_file());
        fs::remove_file(directory.join("plan.json")).unwrap();
        fs::remove_file(directory.join("helper.exe")).unwrap();
        fs::create_dir(directory.join("helper.exe")).unwrap();
        assert!(cleanup_completed_update(&directory).is_err());
        assert!(directory.join("previous.exe").is_file());
        fs::remove_dir(directory.join("helper.exe")).unwrap();
        fs::write(directory.join("helper.exe"), b"test").unwrap();
        assert!(cleanup_completed_update(&directory).unwrap());
        assert!(!directory.exists());
        assert_eq!(fs::read(root.join("archive.db")).unwrap(), b"data");
        fs::remove_dir_all(root).unwrap();
    }
}
