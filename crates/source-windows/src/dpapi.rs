use std::ffi::c_void;
use std::path::Path;
use std::{fs, slice};

use thiserror::Error;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum DpapiError {
    #[error("DPAPI operation failed")]
    Protect,
    #[error("key file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("key file is empty")]
    Empty,
}

pub fn store_current_user(path: &Path, plaintext: &[u8]) -> Result<(), DpapiError> {
    if plaintext.is_empty() {
        return Err(DpapiError::Empty);
    }
    let protected = protect(plaintext)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("dpapi.partial");
    fs::write(&partial, &protected)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(partial, path)?;
    Ok(())
}

pub fn load_current_user(path: &Path) -> Result<Zeroizing<Vec<u8>>, DpapiError> {
    let protected = Zeroizing::new(fs::read(path)?);
    if protected.is_empty() {
        return Err(DpapiError::Empty);
    }
    unprotect(&protected)
}

pub fn clear(path: &Path) -> Result<(), DpapiError> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn protect(plaintext: &[u8]) -> Result<Vec<u8>, DpapiError> {
    let input = blob(plaintext);
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| DpapiError::Protect)?;
        take_blob(output)
    }
}

fn unprotect(protected: &[u8]) -> Result<Zeroizing<Vec<u8>>, DpapiError> {
    let input = blob(protected);
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| DpapiError::Protect)?;
        Ok(Zeroizing::new(take_blob(output)?))
    }
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    }
}

unsafe fn take_blob(output: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>, DpapiError> {
    if output.pbData.is_null() || output.cbData == 0 {
        return Err(DpapiError::Empty);
    }
    let value = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast::<c_void>())));
    }
    Ok(value)
}
