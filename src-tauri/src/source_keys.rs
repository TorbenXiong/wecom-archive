use crate::paths::create_work_root;
use crate::sqlite3mc;
use crate::state::{CommandError, internal_error};
use archive_domain::{SourceAdapter, SourceCandidate};
use source_windows::WindowsSourceAdapter;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceKeyKind {
    Passphrase,
    DerivedAes128,
    DerivedAes256,
    RawWxSqlite3Key,
}

pub(super) struct SourceKey {
    pub(super) kind: SourceKeyKind,
    pub(super) bytes: Zeroizing<Vec<u8>>,
    pub(super) legacy: bool,
    pub(super) legacy_page_size: u32,
}

impl SourceKey {
    pub(super) fn passphrase(bytes: &[u8]) -> Self {
        Self {
            kind: SourceKeyKind::Passphrase,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy: false,
            legacy_page_size: 0,
        }
    }

    pub(super) fn derived_aes128_with_config(
        bytes: &[u8],
        legacy: bool,
        legacy_page_size: u32,
    ) -> Result<Self, CommandError> {
        if bytes.len() != 16 {
            return Err(CommandError {
                code: "SOURCE_KEY_FORMAT_INVALID",
                message: "AES-128 派生密钥必须恰好为 16 字节。".into(),
                recoverable: true,
            });
        }
        Ok(Self {
            kind: SourceKeyKind::DerivedAes128,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy,
            legacy_page_size,
        })
    }

    pub(super) fn derived_aes256_with_config(
        bytes: &[u8],
        legacy: bool,
        legacy_page_size: u32,
    ) -> Result<Self, CommandError> {
        if bytes.len() != 32 {
            return Err(CommandError {
                code: "SOURCE_KEY_FORMAT_INVALID",
                message: "AES-256 派生密钥必须恰好为 32 字节。".into(),
                recoverable: true,
            });
        }
        Ok(Self {
            kind: SourceKeyKind::DerivedAes256,
            bytes: Zeroizing::new(bytes.to_vec()),
            legacy,
            legacy_page_size,
        })
    }
}

pub(super) fn resolve_collection_key(
    candidate: &SourceCandidate,
    encrypted: bool,
    data_root: &Path,
) -> Result<Option<SourceKey>, CommandError> {
    if !encrypted {
        return Ok(None);
    }
    let candidates = run_isolated_key_probe(candidate, data_root)?;
    for key in candidates {
        if key_opens_candidate(candidate, &key) {
            return Ok(Some(key));
        }
    }
    Err(CommandError {
        code: "SOURCE_KEY_NOT_FOUND",
        message: "自动解析未找到可验证的数据库密钥；未读取或导出任何消息。".into(),
        recoverable: true,
    })
}

pub(super) fn choose_collectable_source(
    candidates: Vec<SourceCandidate>,
    resolve_key: &mut dyn FnMut(&SourceCandidate) -> Result<Option<SourceKey>, CommandError>,
) -> Result<(SourceCandidate, Option<SourceKey>), CommandError> {
    // Only the logged-in account's key is present in memory, so an account whose key
    // cannot be resolved is not a failure — it is just not the account to collect.
    let mut last_error = None;
    for candidate in candidates {
        match resolve_key(&candidate) {
            Ok(key) => return Ok((candidate, key)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or(CommandError {
        code: "SOURCE_NOT_FOUND",
        message: "未发现可支持的本机企业微信数据，请确认客户端已登录。".into(),
        recoverable: true,
    }))
}

pub(super) fn key_opens_candidate(candidate: &SourceCandidate, key: &SourceKey) -> bool {
    candidate
        .databases
        .iter()
        .find(|database| database.kind == "message")
        .is_some_and(|database| match key.kind {
            SourceKeyKind::RawWxSqlite3Key => {
                sqlite3mc::quick_verify_raw_wxsqlite3_key(&database.path, &key.bytes)
            }
            _ => open_source_database(&database.path, Some(key)).is_ok(),
        })
}

pub(super) fn open_source_database(
    path: &Path,
    key: Option<&SourceKey>,
) -> Result<rusqlite::Connection, sqlite3mc::CipherDatabaseError> {
    match key {
        None => sqlite3mc::open_read_only(path, None),
        Some(SourceKey {
            kind: SourceKeyKind::Passphrase,
            bytes,
            ..
        }) => sqlite3mc::open_read_only(path, Some(bytes)),
        Some(SourceKey {
            kind: SourceKeyKind::DerivedAes128,
            bytes,
            legacy,
            legacy_page_size,
        }) => sqlite3mc::open_read_only_derived_aes128(path, bytes, *legacy, *legacy_page_size),
        Some(SourceKey {
            kind: SourceKeyKind::RawWxSqlite3Key,
            bytes,
            ..
        }) => sqlite3mc::open_read_only_derived_aes128(path, bytes, false, 0),
        Some(SourceKey {
            kind: SourceKeyKind::DerivedAes256,
            bytes,
            legacy,
            legacy_page_size,
        }) => sqlite3mc::open_read_only_derived_aes256(path, bytes, *legacy, *legacy_page_size),
    }
}

#[cfg(windows)]
pub(super) fn run_isolated_key_probe(
    candidate: &SourceCandidate,
    data_root: &Path,
) -> Result<Vec<SourceKey>, CommandError> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let validation_page_prefix_hex = validation_page_prefix(candidate, data_root);
    let request = source_windows::probe::ProbeRequest {
        authorization_token: token.clone(),
        max_candidates: 640,
        validation_page_prefix_hex,
    };
    #[cfg(test)]
    let executable = std::env::var_os("WECOM_ARCHIVE_PROBE_EXE")
        .map(PathBuf::from)
        .ok_or_else(internal_error)?;
    #[cfg(not(test))]
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let mut child = Command::new(executable)
        .arg("--key-probe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| probe_error("KEY_PROBE_START_FAILED", "无法启动一次性只读密钥探针。"))?;
    let request_bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| internal_error())?);
    child
        .stdin
        .take()
        .ok_or_else(internal_error)?
        .write_all(&request_bytes)
        .map_err(|_| probe_error("KEY_PROBE_PROTOCOL_FAILED", "无法授权一次性只读密钥探针。"))?;
    let output = child
        .wait_with_output()
        .map_err(|_| probe_error("KEY_PROBE_FAILED", "一次性只读密钥探针未完成。"))?;
    let stdout = Zeroizing::new(output.stdout);
    let mut response: source_windows::probe::ProbeResponse = serde_json::from_slice(&stdout)
        .map_err(|_| probe_error("KEY_PROBE_PROTOCOL_FAILED", "密钥探针返回了无效响应。"))?;
    if !output.status.success()
        || !response.ok
        || response.authorization_token != token
        || response.code != "PROBE_COMPLETED"
    {
        response.authorization_token.zeroize();
        for candidate in &mut response.candidates {
            candidate.value_hex.zeroize();
        }
        return Err(probe_error(
            "KEY_PROBE_TRUST_FAILED",
            "未找到与已验证企业微信版本匹配的签名可信进程。",
        ));
    }
    response.authorization_token.zeroize();
    let mut candidates = Vec::with_capacity(response.candidates.len());
    for candidate in &mut response.candidates {
        if let Ok(decoded) = hex::decode(candidate.value_hex.as_bytes()) {
            let key = match candidate.kind {
                source_windows::probe::ProbeCandidateKind::DerivedAes128 => {
                    SourceKey::derived_aes128_with_config(
                        &decoded,
                        candidate.legacy.unwrap_or(false),
                        candidate.legacy_page_size.unwrap_or(0),
                    )
                    .ok()
                }
                source_windows::probe::ProbeCandidateKind::DerivedAes256 => {
                    SourceKey::derived_aes256_with_config(
                        &decoded,
                        candidate.legacy.unwrap_or(false),
                        candidate.legacy_page_size.unwrap_or(0),
                    )
                    .ok()
                }
                source_windows::probe::ProbeCandidateKind::RawWxSqlite3Key => {
                    if decoded.len() == 16 {
                        Some(SourceKey {
                            kind: SourceKeyKind::RawWxSqlite3Key,
                            bytes: Zeroizing::new(decoded),
                            legacy: false,
                            legacy_page_size: 0,
                        })
                    } else {
                        None
                    }
                }
                source_windows::probe::ProbeCandidateKind::Passphrase => {
                    Some(SourceKey::passphrase(&decoded))
                }
            };
            if let Some(key) = key {
                candidates.push(key);
            }
        }
        candidate.value_hex.zeroize();
    }
    Ok(candidates)
}

#[cfg(windows)]
pub(super) fn validation_page_prefix(
    candidate: &SourceCandidate,
    data_root: &Path,
) -> Option<String> {
    let message_path = candidate
        .databases
        .iter()
        .find(|database| database.kind == "message")
        .map(|database| database.path.clone())?;
    let mut page_prefix = [0_u8; 32];
    if File::open(&message_path)
        .and_then(|mut file| file.read_exact(&mut page_prefix))
        .is_ok()
    {
        return Some(hex::encode(page_prefix));
    }
    let work_root = create_work_root(data_root, "probe").ok()?;
    let result = (|| {
        let receipt = WindowsSourceAdapter
            .snapshot(candidate, work_root.clone())
            .ok()?;
        let copied = receipt.copied_files.iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("message.db"))
        })?;
        File::open(copied)
            .and_then(|mut file| file.read_exact(&mut page_prefix))
            .ok()
            .map(|_| hex::encode(page_prefix))
    })();
    let _ = fs::remove_dir_all(&work_root);
    result
}

#[cfg(not(windows))]
pub(super) fn run_isolated_key_probe(
    _candidate: &SourceCandidate,
    _data_root: &Path,
) -> Result<Vec<SourceKey>, CommandError> {
    Err(probe_error(
        "KEY_PROBE_UNAVAILABLE",
        "当前平台不支持 Windows 密钥探针。",
    ))
}

pub(super) fn probe_error(code: &'static str, message: &str) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        recoverable: true,
    }
}

pub fn run_key_probe() -> i32 {
    let mut input = Zeroizing::new(Vec::new());
    if std::io::stdin()
        .take(64 * 1024)
        .read_to_end(&mut input)
        .is_err()
    {
        return 20;
    }
    let request: source_windows::probe::ProbeRequest = match serde_json::from_slice(&input) {
        Ok(request) => request,
        Err(_) => return 20,
    };
    let response = match source_windows::probe::scan(&request) {
        Ok(response) => response,
        Err(error) => source_windows::probe::ProbeResponse {
            ok: false,
            code: match error {
                source_windows::probe::ProbeError::Authorization => "PROBE_AUTHORIZATION_REQUIRED",
                source_windows::probe::ProbeError::TrustedTargetNotFound => {
                    "PROBE_TRUSTED_TARGET_NOT_FOUND"
                }
                source_windows::probe::ProbeError::Scan => "PROBE_SCAN_FAILED",
            }
            .into(),
            authorization_token: request.authorization_token,
            candidates: Vec::new(),
            processes_scanned: 0,
            regions_scanned: 0,
        },
    };
    let ok = response.ok;
    if serde_json::to_writer(std::io::stdout().lock(), &response).is_err() {
        return 21;
    }
    if ok { 0 } else { 22 }
}
