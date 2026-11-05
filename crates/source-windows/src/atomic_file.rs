//! Replace local configuration without an interval where the destination is absent.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct PartialFile(PathBuf);

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let partial = parent.join(format!(".{}.partial", Uuid::new_v4().simple()));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)?;
    let _cleanup = PartialFile(partial.clone());
    output.write_all(bytes)?;
    output.sync_all()?;
    drop(output);
    // std::fs::rename also replaces existing files on Windows. Never delete
    // the destination first: a failed replacement must retain the old file.
    fs::rename(partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_complete_files_and_preserves_unrelated_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let unrelated = root.path().join("settings.json.partial");
        fs::write(&unrelated, b"interrupted previous operation").unwrap();
        write(&path, b"first complete configuration").unwrap();
        write(&path, b"second complete configuration").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second complete configuration");
        assert_eq!(
            fs::read(&unrelated).unwrap(),
            b"interrupted previous operation"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn failed_replacement_keeps_the_old_file_and_cleans_its_partial() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        fs::write(&path, b"original configuration").unwrap();
        // Deny delete sharing so Windows rejects the replacement.
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(write(&path, b"replacement configuration").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original configuration");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        drop(locked);
        write(&path, b"replacement configuration").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement configuration");
    }
}
