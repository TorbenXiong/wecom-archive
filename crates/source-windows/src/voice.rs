//! Content lookup for local SILK caches. MD5 is a source identifier only;
//! archived bytes continue to use SHA-256 for integrity.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::ptr::{null, null_mut};

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptOpenAlgorithmProvider(
        algorithm: *mut *mut c_void,
        identifier: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    fn BCryptCreateHash(
        algorithm: *mut c_void,
        hash: *mut *mut c_void,
        object: *mut u8,
        object_len: u32,
        secret: *const u8,
        secret_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptHashData(hash: *mut c_void, input: *const u8, length: u32, flags: u32) -> i32;
    fn BCryptFinishHash(hash: *mut c_void, output: *mut u8, length: u32, flags: u32) -> i32;
    fn BCryptDestroyHash(hash: *mut c_void) -> i32;
}

struct Algorithm(*mut c_void);

impl Drop for Algorithm {
    fn drop(&mut self) {
        // SAFETY: this handle belongs to this provider and is closed once.
        unsafe { BCryptCloseAlgorithmProvider(self.0, 0) };
    }
}

struct Hash(*mut c_void);

impl Drop for Hash {
    fn drop(&mut self) {
        // SAFETY: the hash is destroyed before its provider is dropped.
        unsafe { BCryptDestroyHash(self.0) };
    }
}

fn check(status: i32) -> io::Result<()> {
    if status < 0 {
        Err(io::Error::other("voice cache digest failed"))
    } else {
        Ok(())
    }
}

fn md5(reader: &mut impl Read) -> io::Result<String> {
    let mut provider = null_mut();
    // SAFETY: all pointers are valid for the call; MD5 is NUL-terminated UTF-16.
    check(unsafe {
        BCryptOpenAlgorithmProvider(&mut provider, [77, 68, 53, 0].as_ptr(), null(), 0)
    })?;
    let provider = Algorithm(provider);
    let mut hash = null_mut();
    // SAFETY: Windows owns the hash object's buffer when object/length are zero.
    check(unsafe { BCryptCreateHash(provider.0, &mut hash, null_mut(), 0, null(), 0, 0) })?;
    let hash = Hash(hash);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        // SAFETY: count is bounded by buffer length and fits in u32.
        check(unsafe { BCryptHashData(hash.0, buffer.as_ptr(), count as u32, 0) })?;
    }
    let mut output = [0_u8; 16];
    // SAFETY: MD5 produces exactly 16 bytes, and output is writable.
    check(unsafe { BCryptFinishHash(hash.0, output.as_mut_ptr(), output.len() as u32, 0) })?;
    Ok(hex::encode(output))
}

/// Inspect an already authorized regular file without changing its contents.
/// A recognized header prevents unrelated cache files becoming voice media.
pub fn silk_file_md5(path: &Path) -> io::Result<Option<(String, u64)>> {
    let mut input = File::open(path)?;
    let before = input.metadata()?;
    if !before.is_file() || before.len() < 10 {
        return Ok(None);
    }
    let mut prefix = [0_u8; 10];
    input.read_exact(&mut prefix)?;
    if !prefix.starts_with(b"#!SILK_V3") && !prefix.starts_with(b"\x02#!SILK_V3") {
        return Ok(None);
    }
    let digest = md5(&mut prefix.as_slice().chain(&mut input))?;
    let after = input.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(io::Error::other("voice cache changed while reading"));
    }
    Ok(Some((digest, after.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_md5_matches_known_vectors_and_streams_multiple_chunks() {
        for (mut data, expected) in [
            (b"".as_slice(), "d41d8cd98f00b204e9800998ecf8427e"),
            (b"abc".as_slice(), "900150983cd24fb0d6963f7d28e17f72"),
        ] {
            assert_eq!(md5(&mut data).unwrap(), expected);
        }
        assert_eq!(
            md5(&mut vec![b'a'; 1_000_000].as_slice()).unwrap(),
            "7707d6ae4e027c70eea2a935c2296f21"
        );
    }

    #[test]
    fn requires_a_silk_header_and_hashes_the_entire_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("timestamp.silk");
        for bytes in [
            b"#!SILK_V3 synthetic".as_slice(),
            b"\x02#!SILK_V3 synthetic",
        ] {
            std::fs::write(&path, bytes).unwrap();
            let mut input = bytes;
            assert_eq!(
                silk_file_md5(&path).unwrap(),
                Some((md5(&mut input).unwrap(), bytes.len() as u64))
            );
        }
        for bytes in [b"not a voice file".as_slice(), b"#!SILK", b""] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(silk_file_md5(&path).unwrap(), None);
        }
    }
}
