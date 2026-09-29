use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

// WeCom keeps every account profile under one folder named `WXWork`; only the
// parent of that folder moves when storage is relocated.
pub const DATA_DIR_NAME: &str = "WXWork";

// OneDrive renames the display label, but the physical folder name follows the
// Windows UI language, so both spellings have to be probed.
const DOCUMENTS_DIR_NAMES: [&str; 2] = ["Documents", "文档"];

/// Every directory the adapter walks when the caller did not select one.
///
/// Only roots that actually exist are walked deeper, so guessing extra
/// locations costs a handful of failed path checks on a normal machine.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = roots_from_env(|key| std::env::var_os(key));
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        roots.extend(one_drive_roots(Path::new(&profile)));
    }
    roots.extend(drive_roots());
    roots.sort();
    roots.dedup();
    roots
}

fn roots_from_env(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for profile_folder in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(value) = lookup(profile_folder) {
            roots.push(PathBuf::from(value).join("Tencent").join(DATA_DIR_NAME));
        }
    }
    if let Some(profile) = lookup("USERPROFILE") {
        roots.push(PathBuf::from(profile).join("Documents").join(DATA_DIR_NAME));
    }
    roots
}

/// Guesses the relocated-`Documents` layouts OneDrive creates. Unlike the
/// environment-based roots these are guesses, so only folders that exist are
/// returned; otherwise a machine with many profile subfolders would grow the
/// scan list for no reason.
fn one_drive_roots(profile: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(profile) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("OneDrive") {
            continue;
        }
        if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }
        for documents in DOCUMENTS_DIR_NAMES {
            let candidate = entry.path().join(documents).join(DATA_DIR_NAME);
            if candidate.is_dir() {
                roots.push(candidate);
            }
        }
    }
    roots
}

fn drive_roots() -> Vec<PathBuf> {
    let letters: Vec<char> = ('A'..='Z').collect();
    drive_roots_for(&letters)
        .into_iter()
        .filter(|root| root.is_dir())
        .collect()
}

/// Every fixed-disk root is a plausible place for `WXWork`, because that is
/// what the WeCom storage picker creates. Only the drive root itself is
/// probed, never a recursive walk of a whole volume.
fn drive_roots_for(letters: &[char]) -> Vec<PathBuf> {
    letters
        .iter()
        .map(|letter| PathBuf::from(format!("{letter}:\\")).join(DATA_DIR_NAME))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(*value)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn env_roots_cover_both_app_data_folders_and_documents() {
        let roots = roots_from_env(env(&[
            ("APPDATA", r"C:\Users\mia\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\mia\AppData\Local"),
            ("USERPROFILE", r"C:\Users\mia"),
        ]));
        assert_eq!(
            roots,
            vec![
                PathBuf::from(r"C:\Users\mia\AppData\Roaming")
                    .join("Tencent")
                    .join(DATA_DIR_NAME),
                PathBuf::from(r"C:\Users\mia\AppData\Local")
                    .join("Tencent")
                    .join(DATA_DIR_NAME),
                PathBuf::from(r"C:\Users\mia")
                    .join("Documents")
                    .join(DATA_DIR_NAME),
            ]
        );
    }

    #[test]
    fn env_roots_skip_variables_that_are_not_set() {
        let roots = roots_from_env(env(&[("USERPROFILE", r"D:\home\mia")]));
        assert_eq!(
            roots,
            vec![
                PathBuf::from(r"D:\home\mia")
                    .join("Documents")
                    .join(DATA_DIR_NAME)
            ]
        );
    }

    #[test]
    fn one_drive_roots_use_english_and_localized_document_names() {
        let profile = tempdir().unwrap();
        let redirected = profile
            .path()
            .join("OneDrive - 睿米")
            .join("文档")
            .join(DATA_DIR_NAME);
        let plain = profile
            .path()
            .join("OneDrive")
            .join("Documents")
            .join(DATA_DIR_NAME);
        fs::create_dir_all(&redirected).unwrap();
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(profile.path().join("Downloads")).unwrap();

        let mut roots = one_drive_roots(profile.path());
        roots.sort();
        assert_eq!(roots, vec![plain, redirected]);
    }

    #[test]
    fn one_drive_roots_are_empty_without_a_redirect() {
        let profile = tempdir().unwrap();
        fs::create_dir_all(profile.path().join("Documents")).unwrap();
        assert!(one_drive_roots(profile.path()).is_empty());
    }

    #[test]
    fn drive_roots_only_guess_the_data_folder_directly_under_a_drive() {
        let roots = drive_roots_for(&['C', 'D']);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("C:\\").join(DATA_DIR_NAME),
                PathBuf::from("D:\\").join(DATA_DIR_NAME),
            ]
        );
    }
}
