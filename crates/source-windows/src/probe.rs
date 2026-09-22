use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvCertFromChain,
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQueryEx,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR};
use zeroize::Zeroize;

// The leaf certificate changes during normal WXWork updates. Trust the
// stable publisher identity instead of pinning a short-lived certificate.
const TRUSTED_PUBLISHER_COMMON_NAME: &str = "Tencent Technology (Shenzhen) Company Limited";
const MAX_SCAN_BYTES: u64 = 1024 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 256 * 1024;
const MAX_DERIVED_CANDIDATES: usize = 512;
const MAX_PASSPHRASE_CANDIDATES: usize = 128;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeRequest {
    pub authorization_token: String,
    pub max_candidates: usize,
    #[serde(default)]
    pub validation_page_prefix_hex: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResponse {
    pub ok: bool,
    pub code: String,
    pub authorization_token: String,
    pub candidates: Vec<ProbeCandidate>,
    pub processes_scanned: u32,
    pub regions_scanned: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProbeCandidateKind {
    DerivedAes128,
    DerivedAes256,
    RawWxSqlite3Key,
    Passphrase,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeCandidate {
    pub kind: ProbeCandidateKind,
    pub value_hex: String,
    pub legacy: Option<bool>,
    pub legacy_page_size: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("probe authorization is invalid")]
    Authorization,
    #[error("no trusted target process was found")]
    TrustedTargetNotFound,
    #[error("target process scan failed")]
    Scan,
}

pub fn scan(request: &ProbeRequest) -> Result<ProbeResponse, ProbeError> {
    if request.authorization_token.len() < 32 || request.max_candidates == 0 {
        return Err(ProbeError::Authorization);
    }
    let validation_target = request
        .validation_page_prefix_hex
        .as_deref()
        .and_then(RawKeyValidationTarget::from_hex);
    let derived_limit = if validation_target.is_some() {
        1
    } else {
        request.max_candidates.min(MAX_DERIVED_CANDIDATES)
    };
    let passphrase_limit = if validation_target.is_some() {
        0
    } else {
        request
            .max_candidates
            .saturating_sub(derived_limit)
            .min(MAX_PASSPHRASE_CANDIDATES)
    };
    let targets = trusted_target_processes()?;
    if targets.is_empty() {
        return Err(ProbeError::TrustedTargetNotFound);
    }
    let mut derived_candidates = Vec::<(Vec<u8>, ProbeCandidateKind, bool, u32)>::new();
    let mut passphrase_candidates = Vec::<Vec<u8>>::new();
    let mut derived_seen = HashSet::<(Vec<u8>, ProbeCandidateKind, bool, u32)>::new();
    let mut passphrase_seen = HashSet::<Vec<u8>>::new();
    let mut regions_scanned = 0_u64;
    let mut scanned_bytes = 0_u64;
    for target in &targets {
        scan_process(
            target.pid,
            derived_limit,
            passphrase_limit,
            &mut derived_candidates,
            &mut passphrase_candidates,
            &mut derived_seen,
            &mut passphrase_seen,
            &mut regions_scanned,
            &mut scanned_bytes,
            validation_target.as_ref(),
        )?;
        if derived_candidates.len() >= derived_limit || scanned_bytes >= MAX_SCAN_BYTES {
            break;
        }
    }
    let mut candidates = Vec::with_capacity(derived_candidates.len() + passphrase_candidates.len());
    candidates.extend(
        derived_candidates
            .iter()
            .map(|(value, kind, legacy, page_size)| ProbeCandidate {
                kind: *kind,
                value_hex: hex::encode(value),
                legacy: Some(*legacy),
                legacy_page_size: Some(*page_size),
            }),
    );
    candidates.extend(passphrase_candidates.iter().map(|value| ProbeCandidate {
        kind: ProbeCandidateKind::Passphrase,
        value_hex: hex::encode(value),
        legacy: None,
        legacy_page_size: None,
    }));
    for (value, _, _, _) in &mut derived_candidates {
        value.zeroize();
    }
    passphrase_candidates.zeroize();
    for (mut value, _, _, _) in derived_seen.drain() {
        value.zeroize();
    }
    for mut value in passphrase_seen.drain() {
        value.zeroize();
    }
    Ok(ProbeResponse {
        ok: true,
        code: "PROBE_COMPLETED".into(),
        authorization_token: request.authorization_token.clone(),
        candidates,
        processes_scanned: targets.len() as u32,
        regions_scanned,
    })
}

struct TrustedTarget {
    pid: u32,
}

fn trusted_target_processes() -> Result<Vec<TrustedTarget>, ProbeError> {
    let mut current_session = 0_u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session) }
        .map_err(|_| ProbeError::Scan)?;
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(|_| ProbeError::Scan)?;
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut targets = Vec::new();
    let mut image_trust = HashMap::<PathBuf, bool>::new();
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_err() {
        return Ok(targets);
    }
    loop {
        if wide_string(&entry.szExeFile).eq_ignore_ascii_case("WXWork.exe") {
            let mut target_session = u32::MAX;
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut target_session) }.is_ok()
                && target_session == current_session
                && let Ok(path) = process_image_path(entry.th32ProcessID)
                && *image_trust
                    .entry(path.clone())
                    .or_insert_with(|| verify_target_image(&path))
            {
                targets.push(TrustedTarget {
                    pid: entry.th32ProcessID,
                });
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Ok(targets)
}

fn process_image_path(pid: u32) -> Result<PathBuf, ProbeError> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|_| ProbeError::Scan)?;
    let process = OwnedHandle(process);
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(|_| ProbeError::Scan)?;
    buffer.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn verify_target_image(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("WXWork.exe"))
        && verify_authenticode(path)
        && verify_publisher(path)
}

fn verify_authenticode(path: &Path) -> bool {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        );
    }
    status == 0
}

fn verify_publisher(path: &Path) -> bool {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        )
    };
    let trusted = if status == 0 {
        unsafe {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            let signer = if provider.is_null() {
                std::ptr::null_mut()
            } else {
                WTHelperGetProvSignerFromChain(provider, 0, false, 0)
            };
            let provider_cert = if signer.is_null() {
                std::ptr::null_mut()
            } else {
                WTHelperGetProvCertFromChain(signer, 0)
            };
            let cert = if provider_cert.is_null() {
                std::ptr::null()
            } else {
                (*provider_cert).pCert
            };
            certificate_common_name(cert)
                .as_deref()
                .is_some_and(is_trusted_publisher_common_name)
        }
    } else {
        false
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        );
    }
    trusted
}

fn is_trusted_publisher_common_name(common_name: &str) -> bool {
    common_name.eq_ignore_ascii_case(TRUSTED_PUBLISHER_COMMON_NAME)
}

fn certificate_common_name(
    certificate: *const windows::Win32::Security::Cryptography::CERT_CONTEXT,
) -> Option<String> {
    if certificate.is_null() {
        return None;
    }
    let size =
        unsafe { CertGetNameStringW(certificate, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None) };
    if size <= 1 {
        return None;
    }
    let mut name = vec![0_u16; size as usize];
    let written = unsafe {
        CertGetNameStringW(
            certificate,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            None,
            Some(&mut name),
        )
    };
    if written <= 1 {
        return None;
    }
    name.truncate(written as usize - 1);
    String::from_utf16(&name).ok()
}

#[allow(clippy::too_many_arguments)]
fn scan_process(
    pid: u32,
    derived_limit: usize,
    passphrase_limit: usize,
    derived_candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    passphrase_candidates: &mut Vec<Vec<u8>>,
    derived_seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    passphrase_seen: &mut HashSet<Vec<u8>>,
    regions_scanned: &mut u64,
    scanned_bytes: &mut u64,
    validation_target: Option<&RawKeyValidationTarget>,
) -> Result<(), ProbeError> {
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
    }
    .map_err(|_| ProbeError::Scan)?;
    let process = OwnedHandle(process);
    let mut address = 0_usize;
    while derived_candidates.len() < derived_limit && *scanned_bytes < MAX_SCAN_BYTES {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        let queried = unsafe {
            VirtualQueryEx(
                process.0,
                Some(address as *const c_void),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let next = base.saturating_add(info.RegionSize);
        if next <= address {
            break;
        }
        if info.State == MEM_COMMIT
            && info.Type == MEM_PRIVATE
            && is_readable_writable(info.Protect.0)
        {
            *regions_scanned += 1;
            let mut cursor = base;
            let mut carry = Vec::new();
            while cursor < next
                && derived_candidates.len() < derived_limit
                && *scanned_bytes < MAX_SCAN_BYTES
            {
                let requested = READ_CHUNK_BYTES.min(next - cursor);
                let mut buffer = vec![0_u8; requested];
                let mut read = 0_usize;
                if unsafe {
                    ReadProcessMemory(
                        process.0,
                        cursor as *const c_void,
                        buffer.as_mut_ptr().cast(),
                        requested,
                        Some(&mut read),
                    )
                }
                .is_ok()
                    && read > 0
                {
                    buffer.truncate(read);
                    *scanned_bytes += read as u64;
                    carry.extend_from_slice(&buffer);
                    if let Some(target) = validation_target {
                        extract_verified_raw_keys(
                            &carry,
                            target,
                            derived_limit,
                            derived_candidates,
                            derived_seen,
                        );
                    } else {
                        extract_candidates(
                            &carry,
                            Some(process.0),
                            derived_limit,
                            passphrase_limit,
                            derived_candidates,
                            passphrase_candidates,
                            derived_seen,
                            passphrase_seen,
                        );
                    }
                    let keep_from = carry.len().saturating_sub(256);
                    let suffix = carry.split_off(keep_from);
                    carry.zeroize();
                    carry = suffix;
                }
                buffer.zeroize();
                cursor = cursor.saturating_add(requested);
            }
            carry.zeroize();
        }
        address = next;
    }
    Ok(())
}

#[allow(dead_code)]
struct RawKeyValidationTarget {
    fragment: [u8; 8],
    cipher_block: [u8; 16],
}

impl RawKeyValidationTarget {
    fn from_hex(value: &str) -> Option<Self> {
        let page = hex::decode(value).ok()?;
        if page.len() < 32 || !plausible_plain_header_fragment(&page[16..24]) {
            return None;
        }
        let mut fragment = [0_u8; 8];
        fragment.copy_from_slice(&page[16..24]);
        let mut cipher_block = [0_u8; 16];
        cipher_block[..8].copy_from_slice(&page[8..16]);
        cipher_block[8..].copy_from_slice(&page[24..32]);
        Some(Self {
            fragment,
            cipher_block,
        })
    }
}

fn plausible_plain_header_fragment(value: &[u8]) -> bool {
    if value.len() != 8 {
        return false;
    }
    let encoded = u16::from_be_bytes([value[0], value[1]]);
    let page_size = if encoded == 1 {
        65_536
    } else {
        u32::from(encoded)
    };
    (512..=65_536).contains(&page_size)
        && page_size.is_power_of_two()
        && value[5..8] == [0x40, 0x20, 0x20]
}

fn extract_verified_raw_keys(
    bytes: &[u8],
    target: &RawKeyValidationTarget,
    limit: usize,
    candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
) {
    if bytes.len() < 16 || candidates.len() >= limit {
        return;
    }
    for offset in (0..=bytes.len() - 16).step_by(8) {
        let raw_key = &bytes[offset..offset + 16];
        let non_ascii = raw_key
            .iter()
            .filter(|byte| !(0x20..=0x7e).contains(&**byte))
            .count();
        let distinct = raw_key.iter().copied().collect::<HashSet<_>>().len();
        if non_ascii >= 3 && distinct >= 11 && verify_raw_key(target, raw_key) {
            add_derived_candidate(
                raw_key.to_vec(),
                ProbeCandidateKind::RawWxSqlite3Key,
                false,
                0,
                limit,
                candidates,
                seen,
            );
            if candidates.len() >= limit {
                break;
            }
        }
    }
}

fn verify_raw_key(target: &RawKeyValidationTarget, raw_key: &[u8]) -> bool {
    #[cfg(not(test))]
    {
        let mut page = [0_u8; 32];
        page[8..16].copy_from_slice(&target.cipher_block[..8]);
        page[16..24].copy_from_slice(&target.fragment);
        page[24..32].copy_from_slice(&target.cipher_block[8..]);
        unsafe {
            sqlite3mc_verify_wxsqlite3_raw_key(
                page.as_ptr(),
                page.len() as i32,
                raw_key.as_ptr(),
                raw_key.len() as i32,
            ) == 1
        }
    }
    #[cfg(test)]
    {
        let _ = (target, raw_key);
        false
    }
}

#[cfg(not(test))]
unsafe extern "C" {
    fn sqlite3mc_verify_wxsqlite3_raw_key(
        page: *const u8,
        page_len: i32,
        raw_key: *const u8,
        raw_key_len: i32,
    ) -> i32;
}

fn is_readable_writable(protection: u32) -> bool {
    if protection & (PAGE_GUARD.0 | PAGE_NOACCESS.0) != 0 {
        return false;
    }
    matches!(protection & 0xff, 0x04 | 0x08)
}

#[allow(clippy::too_many_arguments)]
fn extract_candidates(
    bytes: &[u8],
    process: Option<HANDLE>,
    derived_limit: usize,
    passphrase_limit: usize,
    derived_candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    passphrase_candidates: &mut Vec<Vec<u8>>,
    derived_seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    passphrase_seen: &mut HashSet<Vec<u8>>,
) {
    extract_aes128_struct_candidates(
        bytes,
        process,
        derived_limit,
        derived_candidates,
        derived_seen,
    );
    for run in printable_runs(bytes) {
        add_encoded_derived_candidates(run, derived_limit, derived_candidates, derived_seen);
        add_printable_candidate(
            run,
            passphrase_limit,
            passphrase_candidates,
            passphrase_seen,
        );
        if passphrase_candidates.len() >= passphrase_limit {
            break;
        }
    }
    let mut decoded_utf16 = Vec::new();
    for alignment in [0_usize, 1] {
        let mut index = alignment;
        while index + 1 < bytes.len() {
            if bytes[index].is_ascii_graphic() && bytes[index + 1] == 0 {
                decoded_utf16.push(bytes[index]);
            } else if !decoded_utf16.is_empty() {
                add_printable_candidate(
                    &decoded_utf16,
                    passphrase_limit,
                    passphrase_candidates,
                    passphrase_seen,
                );
                decoded_utf16.clear();
            }
            index += 2;
        }
        if !decoded_utf16.is_empty() {
            add_printable_candidate(
                &decoded_utf16,
                passphrase_limit,
                passphrase_candidates,
                passphrase_seen,
            );
            decoded_utf16.clear();
        }
    }
    decoded_utf16.zeroize();
}

fn add_encoded_derived_candidates(
    run: &[u8],
    limit: usize,
    candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
) {
    let kind = match run.len() {
        32 if run.iter().all(u8::is_ascii_hexdigit) => ProbeCandidateKind::DerivedAes128,
        64 if run.iter().all(u8::is_ascii_hexdigit) => ProbeCandidateKind::DerivedAes256,
        _ => return,
    };
    if let Ok(decoded) = hex::decode(run) {
        add_derived_candidate(decoded, kind, false, 0, limit, candidates, seen);
    }
}

fn add_derived_candidate(
    value: Vec<u8>,
    kind: ProbeCandidateKind,
    legacy: bool,
    legacy_page_size: u32,
    limit: usize,
    candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
) {
    let value = (value, kind, legacy, legacy_page_size);
    if candidates.len() < limit && seen.insert(value.clone()) {
        candidates.push(value);
    }
}

fn extract_aes128_struct_candidates(
    bytes: &[u8],
    process: Option<HANDLE>,
    limit: usize,
    candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
) {
    const STRUCT32_BYTES: usize = 32;
    if limit == 0 || bytes.len() < STRUCT32_BYTES {
        return;
    }
    for offset in (0..=bytes.len() - STRUCT32_BYTES).step_by(4) {
        let legacy = i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let page_size = i32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        if !matches!(legacy, 0 | 1) || !valid_legacy_page_size(page_size) {
            continue;
        }
        let key_length = i32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap());
        if key_length == 16 {
            let pointer32 =
                u32::from_le_bytes(bytes[offset + 28..offset + 32].try_into().unwrap()) as u64;
            let valid32 = valid_user_pointer32(pointer32)
                && process.is_none_or(|handle| plausible_rijndael(handle, pointer32));
            let valid64 = if offset + 40 <= bytes.len() {
                let pointer64 =
                    u64::from_le_bytes(bytes[offset + 32..offset + 40].try_into().unwrap());
                valid_user_pointer64(pointer64)
                    && process.is_none_or(|handle| plausible_rijndael(handle, pointer64))
            } else {
                false
            };
            let key = &bytes[offset + 12..offset + 28];
            if (valid32 || valid64) && key.iter().any(|byte| *byte != 0) {
                add_derived_candidate(
                    key.to_vec(),
                    ProbeCandidateKind::DerivedAes128,
                    legacy != 0,
                    page_size as u32,
                    limit,
                    candidates,
                    seen,
                );
                extract_raw_key_neighbors(bytes, offset, limit, candidates, seen);
                if candidates.len() >= limit {
                    return;
                }
            }
        }

        if offset + 52 <= bytes.len() {
            let kdf_iterations = key_length;
            let aes256_key_length =
                i32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
            if (1..=1_000_000).contains(&kdf_iterations) && aes256_key_length == 32 {
                let pointer32 =
                    u32::from_le_bytes(bytes[offset + 48..offset + 52].try_into().unwrap()) as u64;
                let valid32 = valid_user_pointer32(pointer32)
                    && process.is_none_or(|handle| plausible_rijndael(handle, pointer32));
                let valid64 = if offset + 56 <= bytes.len() {
                    let pointer64 =
                        u64::from_le_bytes(bytes[offset + 48..offset + 56].try_into().unwrap());
                    valid_user_pointer64(pointer64)
                        && process.is_none_or(|handle| plausible_rijndael(handle, pointer64))
                } else {
                    false
                };
                let key = &bytes[offset + 16..offset + 48];
                if (valid32 || valid64) && key.iter().any(|byte| *byte != 0) {
                    add_derived_candidate(
                        key.to_vec(),
                        ProbeCandidateKind::DerivedAes256,
                        legacy != 0,
                        page_size as u32,
                        limit,
                        candidates,
                        seen,
                    );
                    if candidates.len() >= limit {
                        return;
                    }
                }
            }
        }
    }
}

fn extract_raw_key_neighbors(
    bytes: &[u8],
    structure_offset: usize,
    limit: usize,
    candidates: &mut Vec<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
    seen: &mut HashSet<(Vec<u8>, ProbeCandidateKind, bool, u32)>,
) {
    const WINDOW_RADIUS: usize = 256;
    let start = structure_offset.saturating_sub(WINDOW_RADIUS);
    let end = (structure_offset + WINDOW_RADIUS).min(bytes.len().saturating_sub(16));
    for offset in (start..=end).step_by(8) {
        let value = bytes[offset..offset + 16].to_vec();
        if value.iter().any(|byte| *byte != 0) {
            add_derived_candidate(
                value,
                ProbeCandidateKind::RawWxSqlite3Key,
                false,
                0,
                limit,
                candidates,
                seen,
            );
        }
        if candidates.len() >= limit {
            break;
        }
    }
}

fn plausible_rijndael(process: HANDLE, address: u64) -> bool {
    let mut bytes = [0_u8; 32];
    let mut read = 0_usize;
    if unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            Some(&mut read),
        )
    }
    .is_err()
        || read < bytes.len()
    {
        return false;
    }
    let state = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if state == 1 {
        return true;
    }
    let mode = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let direction = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let rounds = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    state == 0
        && (0..=2).contains(&mode)
        && matches!(direction, 0 | 1)
        && matches!(rounds, 10 | 12 | 14)
}

fn valid_legacy_page_size(value: i32) -> bool {
    value == 0 || (512..=65_536).contains(&value) && (value as u32).is_power_of_two()
}

fn valid_user_pointer32(value: u64) -> bool {
    (0x1_0000..=0x7fff_ffff).contains(&value) && value.is_multiple_of(4)
}

fn valid_user_pointer64(value: u64) -> bool {
    (0x1_0000..=0x0000_7fff_ffff_ffff).contains(&value) && value.is_multiple_of(8)
}

fn printable_runs(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| !byte.is_ascii_graphic())
        .filter(|run| !run.is_empty())
}

fn add_printable_candidate(
    run: &[u8],
    limit: usize,
    candidates: &mut Vec<Vec<u8>>,
    seen: &mut HashSet<Vec<u8>>,
) {
    if candidates.len() >= limit {
        return;
    }
    if matches!(run.len(), 16 | 24 | 32 | 64) {
        add_candidate(run.to_vec(), limit, candidates, seen);
    }
    if matches!(run.len(), 32 | 64)
        && run.iter().all(u8::is_ascii_hexdigit)
        && let Ok(decoded) = hex::decode(run)
    {
        add_candidate(decoded, limit, candidates, seen);
    }
}

fn add_candidate(
    value: Vec<u8>,
    limit: usize,
    candidates: &mut Vec<Vec<u8>>,
    seen: &mut HashSet<Vec<u8>>,
) {
    if !value.is_empty() && candidates.len() < limit && seen.insert(value.clone()) {
        candidates.push(value);
    }
}

fn wide_string(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_tencent_publisher_identity_without_leaf_certificate_pinning() {
        assert!(is_trusted_publisher_common_name(
            "Tencent Technology (Shenzhen) Company Limited"
        ));
        assert!(is_trusted_publisher_common_name(
            "tencent technology (shenzhen) company limited"
        ));
        assert!(!is_trusted_publisher_common_name("Other Publisher"));
    }

    #[test]
    fn extracts_ascii_hex_and_utf16_candidates_without_surrounding_data() {
        let hex_key = b"00112233445566778899aabbccddeeff";
        let mut bytes = vec![0, 1, 2];
        bytes.extend_from_slice(hex_key);
        bytes.push(0);
        bytes.push(0);
        for byte in b"sixteen-char-key" {
            bytes.extend_from_slice(&[*byte, 0]);
        }
        let mut derived = Vec::new();
        let mut passphrases = Vec::new();
        let mut derived_seen = HashSet::new();
        let mut passphrase_seen = HashSet::new();
        extract_candidates(
            &bytes,
            None,
            20,
            20,
            &mut derived,
            &mut passphrases,
            &mut derived_seen,
            &mut passphrase_seen,
        );
        assert!(passphrases.contains(&hex::decode(hex_key).unwrap()));
        assert!(passphrases.contains(&b"sixteen-char-key".to_vec()));
        assert!(derived.contains(&(
            hex::decode(hex_key).unwrap(),
            ProbeCandidateKind::DerivedAes128,
            false,
            0,
        )));
    }

    #[test]
    fn extracts_only_plausible_aes128_cipher_structures() {
        let expected = b"0123456789abcdef";
        let mut bytes = vec![0_u8; 96];
        bytes[16..20].copy_from_slice(&0_i32.to_le_bytes());
        bytes[20..24].copy_from_slice(&4096_i32.to_le_bytes());
        bytes[24..28].copy_from_slice(&16_i32.to_le_bytes());
        bytes[28..44].copy_from_slice(expected);
        bytes[48..56].copy_from_slice(&0x0000_0123_4567_8000_u64.to_le_bytes());
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        extract_aes128_struct_candidates(&bytes, None, 10, &mut candidates, &mut seen);
        assert!(candidates.iter().any(|candidate| {
            candidate
                == &(
                    expected.to_vec(),
                    ProbeCandidateKind::DerivedAes128,
                    false,
                    4096,
                )
        }));

        bytes[24..28].copy_from_slice(&32_i32.to_le_bytes());
        candidates.clear();
        seen.clear();
        extract_aes128_struct_candidates(&bytes, None, 10, &mut candidates, &mut seen);
        assert!(candidates.is_empty());
    }

    #[test]
    fn extracts_x86_aes128_cipher_structure() {
        let expected = b"fedcba9876543210";
        let mut bytes = vec![0_u8; 64];
        bytes[8..12].copy_from_slice(&1_i32.to_le_bytes());
        bytes[12..16].copy_from_slice(&0_i32.to_le_bytes());
        bytes[16..20].copy_from_slice(&16_i32.to_le_bytes());
        bytes[20..36].copy_from_slice(expected);
        bytes[36..40].copy_from_slice(&0x1234_5000_u32.to_le_bytes());
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        extract_aes128_struct_candidates(&bytes, None, 10, &mut candidates, &mut seen);
        assert!(candidates.iter().any(|candidate| {
            candidate
                == &(
                    expected.to_vec(),
                    ProbeCandidateKind::DerivedAes128,
                    true,
                    0,
                )
        }));
    }

    #[test]
    fn extracts_x86_aes256_cipher_structure() {
        let expected = b"0123456789abcdef0123456789abcdef";
        let mut bytes = vec![0_u8; 80];
        bytes[4..8].copy_from_slice(&0_i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&4096_i32.to_le_bytes());
        bytes[12..16].copy_from_slice(&4001_i32.to_le_bytes());
        bytes[16..20].copy_from_slice(&32_i32.to_le_bytes());
        bytes[20..52].copy_from_slice(expected);
        bytes[52..56].copy_from_slice(&0x2345_6000_u32.to_le_bytes());
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        extract_aes128_struct_candidates(&bytes, None, 10, &mut candidates, &mut seen);
        assert_eq!(
            candidates,
            vec![(
                expected.to_vec(),
                ProbeCandidateKind::DerivedAes256,
                false,
                4096
            )]
        );
    }
}
