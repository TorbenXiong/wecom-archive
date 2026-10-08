use std::ffi::c_void;
use std::path::PathBuf;

use super::roots::{normalize_source_root, resolve_source_root};

const WXWORK_REGISTRY_KEY: &str = r"Software\Tencent\WXWork";
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const RRF_RT_REG_EXPAND_SZ: u32 = 0x0000_0004;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
const RRF_SUBKEY_WOW6432KEY: u32 = 0x0002_0000;
const MAX_REGISTRY_VALUE_BYTES: usize = 32 * 1024;
// Windows defines predefined handles via signed LONG -> pointer-sized cast.
const HKEY_CURRENT_USER: *mut c_void = -2_147_483_647_isize as *mut c_void;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: *mut c_void,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        value_type: *mut u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> i32;
}

/// Read only the current user's WeCom storage setting. This is a hint; all
/// candidates still have to pass normal database discovery and key probing.
pub fn configured_data_root() -> Option<PathBuf> {
    let app_data = std::env::var_os("APPDATA");
    // Query each view separately so path and location always come from the
    // same setting, including when a 32-bit client is used with a 64-bit host.
    [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY]
        .into_iter()
        .find_map(|view| {
            let path = read_registry_string("DataLocationPath", view);
            let location = read_registry_dword("DataLocation", view);
            configured_data_root_from_values(path.as_deref(), location, app_data.as_deref())
        })
}

fn configured_data_root_from_values(
    path: Option<&str>,
    location: Option<u32>,
    app_data: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(path) = path.and_then(valid_data_root) {
        return Some(path);
    }
    if location == Some(1) {
        return app_data
            .map(PathBuf::from)
            .map(|root| root.join("Tencent").join("WXWork").join("Data"))
            .and_then(|root| resolve_source_root(&root));
    }
    None
}

fn valid_data_root(raw: &str) -> Option<PathBuf> {
    resolve_source_root(&normalize_source_root(raw)?)
}

fn read_registry_string(name: &str, view: u32) -> Option<String> {
    let (kind, bytes) = read_registry_value(name, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | view)?;
    if !matches!(kind, REG_SZ | REG_EXPAND_SZ) || bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn read_registry_dword(name: &str, view: u32) -> Option<u32> {
    let (kind, bytes) = read_registry_value(name, RRF_RT_REG_DWORD | view)?;
    if kind != REG_DWORD || bytes.len() != 4 {
        return None;
    }
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_registry_value(name: &str, flags: u32) -> Option<(u32, Vec<u8>)> {
    let subkey = wide(WXWORK_REGISTRY_KEY);
    let value = wide(name);
    let mut data = vec![0_u8; MAX_REGISTRY_VALUE_BYTES];
    let mut size = data.len() as u32;
    let mut kind = 0_u32;
    // HKEY_CURRENT_USER is a predefined Windows registry handle.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            &mut kind,
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 || size as usize > data.len() {
        return None;
    }
    data.truncate(size as usize);
    Some((kind, data))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_path_requires_an_existing_non_root_directory() {
        assert!(valid_data_root(r"Z:\").is_none());
        assert!(valid_data_root("relative").is_none());
        let existing = std::env::temp_dir();
        assert_eq!(valid_data_root(&existing.to_string_lossy()), Some(existing));
    }

    #[test]
    fn explicit_registry_path_takes_precedence_over_location_flag() {
        let existing = std::env::temp_dir();
        let configured = existing.to_string_lossy();
        assert_eq!(
            configured_data_root_from_values(Some(&configured), Some(1), None),
            Some(existing)
        );
    }

    #[test]
    fn unknown_location_does_not_guess_another_directory() {
        assert_eq!(configured_data_root_from_values(None, Some(0), None), None);
        assert_eq!(configured_data_root_from_values(None, Some(2), None), None);
    }

    #[test]
    fn missing_or_invalid_custom_location_uses_the_appdata_setting() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Tencent").join("WXWork").join("Data");
        std::fs::create_dir_all(&data).unwrap();
        for path in [None, Some(r"C:\"), Some("relative")] {
            assert_eq!(
                configured_data_root_from_values(path, Some(1), Some(directory.path().as_os_str())),
                Some(data.clone())
            );
        }
        std::fs::remove_dir(&data).unwrap();
        assert!(
            configured_data_root_from_values(None, Some(1), Some(directory.path().as_os_str()))
                .is_none()
        );
    }
}
