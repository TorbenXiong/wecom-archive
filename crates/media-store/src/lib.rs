use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use archive_domain::MediaIntegrity;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMedia {
    pub content_sha256: String,
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub integrity: MediaIntegrity,
}

#[derive(Debug, Error)]
pub enum MediaStoreError {
    #[error("media source is missing")]
    Missing,
    #[error("media source must be a regular file")]
    NotAFile,
    #[error("media I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("stored media escaped archive root")]
    PathBoundary,
}

pub struct MediaStore {
    root: PathBuf,
}

pub fn hash_source(path: &Path) -> Result<(String, u64), MediaStoreError> {
    if !path.exists() {
        return Err(MediaStoreError::Missing);
    }
    if !path.is_file() {
        return Err(MediaStoreError::NotAFile);
    }
    hash_file(path).map_err(MediaStoreError::from)
}

impl MediaStore {
    pub fn new(archive_root: &Path) -> Result<Self, MediaStoreError> {
        let root = archive_root.join("media");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn import(
        &self,
        source: &Path,
        original_name: Option<&str>,
    ) -> Result<StoredMedia, MediaStoreError> {
        if !source.exists() {
            return Err(MediaStoreError::Missing);
        }
        if !source.is_file() {
            return Err(MediaStoreError::NotAFile);
        }

        let (hash, size) = hash_file(source)?;
        let extension =
            safe_extension(original_name.or_else(|| source.file_name().and_then(|v| v.to_str())));
        let file_name = match extension {
            Some(extension) => format!("{hash}.{extension}"),
            None => hash.clone(),
        };
        let target = self.root.join(&file_name);
        if !target.exists() {
            let partial = self
                .root
                .join(format!(".{file_name}.{}.partial", Uuid::new_v4()));
            let copy_result = (|| -> Result<(), io::Error> {
                fs::copy(source, &partial)?;
                let copied_hash = hash_file(&partial)?.0;
                if copied_hash != hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "media hash changed while copying",
                    ));
                }
                if target.exists() {
                    fs::remove_file(&partial)?;
                } else {
                    fs::rename(&partial, &target)?;
                }
                Ok(())
            })();
            if copy_result.is_err() {
                let _ = fs::remove_file(&partial);
            }
            copy_result?;
        }

        Ok(StoredMedia {
            content_sha256: hash,
            relative_path: PathBuf::from("media").join(file_name),
            size_bytes: size,
            integrity: MediaIntegrity::Verified,
        })
    }

    pub fn resolve(&self, hash: &str, extension: Option<&str>) -> Result<PathBuf, MediaStoreError> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(MediaStoreError::PathBoundary);
        }
        let name = match extension.and_then(|value| safe_extension(Some(value))) {
            Some(extension) => format!("{hash}.{extension}"),
            None => hash.to_owned(),
        };
        let candidate = self.root.join(name);
        let canonical_root = self.root.canonicalize()?;
        let canonical_candidate = candidate.canonicalize()?;
        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(MediaStoreError::PathBoundary);
        }
        Ok(canonical_candidate)
    }
}

fn hash_file(path: &Path) -> Result<(String, u64), io::Error> {
    let mut input = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn safe_extension(value: Option<&str>) -> Option<String> {
    let extension = Path::new(value?)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    if extension.len() <= 10 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Some(extension)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn deduplicates_by_content_hash() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.png");
        let second = directory.path().join("second.png");
        File::create(&first)
            .unwrap()
            .write_all(b"same bytes")
            .unwrap();
        File::create(&second)
            .unwrap()
            .write_all(b"same bytes")
            .unwrap();
        let store = MediaStore::new(&directory.path().join("archive")).unwrap();

        let a = store.import(&first, Some("one.png")).unwrap();
        let b = store.import(&second, Some("two.png")).unwrap();
        assert_eq!(a.content_sha256, b.content_sha256);
        assert_eq!(a.relative_path, b.relative_path);
    }

    #[test]
    fn rejects_unsafe_extension() {
        assert_eq!(safe_extension(Some("photo.png.exe/../x")), None);
    }
}
