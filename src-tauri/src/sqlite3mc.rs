use std::ffi::{c_char, c_int, c_void};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use libsqlite3_sys::{SQLITE_OK, sqlite3};
use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

const AES128CBC: &[u8] = b"aes128cbc\0";
const AES256CBC: &[u8] = b"aes256cbc\0";
const CIPHER: &[u8] = b"cipher\0";
const LEGACY: &[u8] = b"legacy\0";
const LEGACY_PAGE_SIZE: &[u8] = b"legacy_page_size\0";

unsafe extern "C" {
    fn sqlite3_key(db: *mut sqlite3, key: *const c_void, key_len: c_int) -> c_int;
    fn sqlite3mc_key_aes128_derived(db: *mut sqlite3, key: *const u8, key_len: c_int) -> c_int;
    fn sqlite3mc_key_aes256_derived(db: *mut sqlite3, key: *const u8, key_len: c_int) -> c_int;
    fn sqlite3mc_verify_wxsqlite3_raw_key(
        page: *const u8,
        page_len: c_int,
        raw_key: *const u8,
        raw_key_len: c_int,
    ) -> c_int;
    fn sqlite3mc_decrypt_wxsqlite3_raw_page(
        input: *const u8,
        page_len: c_int,
        page_number: c_int,
        raw_key: *const u8,
        raw_key_len: c_int,
        output: *mut u8,
    ) -> c_int;
    fn sqlite3mc_cipher_index(cipher_name: *const c_char) -> c_int;
    fn sqlite3mc_config(db: *mut sqlite3, parameter: *const c_char, value: c_int) -> c_int;
    fn sqlite3mc_config_cipher(
        db: *mut sqlite3,
        cipher_name: *const c_char,
        parameter: *const c_char,
        value: c_int,
    ) -> c_int;
}

/// Validate a raw 16-byte wxSQLite3 key against the clear page-1 fragment.
/// Only the first 4 KiB are read and no SQL is executed.
pub fn quick_verify_raw_wxsqlite3_key(path: &Path, key: &[u8]) -> bool {
    if key.len() != 16 {
        return false;
    }
    let mut page = [0_u8; 4096];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let Ok(read) = file.read(&mut page) else {
        return false;
    };
    if read < 32 {
        return false;
    }
    // SAFETY: both buffers remain valid for the duration of the pure verifier
    // call; the C helper does not retain pointers or touch the source file.
    quick_verify_raw_wxsqlite3_page(&page[..read], key)
}

pub fn quick_verify_raw_wxsqlite3_page(page: &[u8], key: &[u8]) -> bool {
    if page.len() < 32 || key.len() != 16 {
        return false;
    }
    // SAFETY: both buffers remain valid for the duration of the pure verifier
    // call; the C helper does not retain pointers or touch the source file.
    unsafe {
        sqlite3mc_verify_wxsqlite3_raw_key(
            page.as_ptr(),
            page.len().min(c_int::MAX as usize) as c_int,
            key.as_ptr(),
            key.len() as c_int,
        ) == 1
    }
}

pub fn decrypt_raw_wxsqlite3_database(
    path: &Path,
    key: &[u8],
) -> Result<std::path::PathBuf, CipherDatabaseError> {
    if key.len() != 16 {
        return Err(CipherDatabaseError::InvalidKey);
    }
    let mut input = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut input))
        .map_err(|_| CipherDatabaseError::Open)?;
    if input.len() < 32 {
        return Err(CipherDatabaseError::InvalidKey);
    }
    let encoded = u16::from_be_bytes([input[16], input[17]]);
    let page_size = if encoded == 1 {
        65_536
    } else {
        u32::from(encoded)
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(CipherDatabaseError::InvalidKey);
    }
    let page_size = page_size as usize;
    if input.len() % page_size != 0 {
        input.resize(input.len().div_ceil(page_size) * page_size, 0);
    }
    let mut output = vec![0_u8; input.len()];
    for (index, (source, destination)) in input
        .chunks(page_size)
        .zip(output.chunks_mut(page_size))
        .enumerate()
    {
        let status = unsafe {
            sqlite3mc_decrypt_wxsqlite3_raw_page(
                source.as_ptr(),
                source.len() as c_int,
                (index + 1) as c_int,
                key.as_ptr(),
                key.len() as c_int,
                destination.as_mut_ptr(),
            )
        };
        if status != SQLITE_OK {
            return Err(CipherDatabaseError::InvalidKey);
        }
    }
    let output_path = path.with_extension("plain.sqlite");
    std::fs::write(&output_path, &output).map_err(|_| CipherDatabaseError::Open)?;
    Ok(output_path)
}

#[derive(Debug, Error)]
pub enum CipherDatabaseError {
    #[error("database open failed")]
    Open,
    #[error("cipher configuration failed")]
    Configuration,
    #[error("database key is invalid")]
    InvalidKey,
}

pub fn open_read_only(path: &Path, key: Option<&[u8]>) -> Result<Connection, CipherDatabaseError> {
    let connection = open_read_only_connection(path)?;
    if let Some(key) = key {
        apply_aes128_key(&connection, key)?;
    }
    finish_read_only_open(connection)
}

pub fn open_read_only_derived_aes128(
    path: &Path,
    key: &[u8],
    legacy: bool,
    legacy_page_size: u32,
) -> Result<Connection, CipherDatabaseError> {
    let connection = open_read_only_connection(path)?;
    apply_aes128_derived_key(&connection, key, legacy, legacy_page_size)?;
    finish_read_only_open(connection)
}

pub fn open_read_only_derived_aes256(
    path: &Path,
    key: &[u8],
    legacy: bool,
    legacy_page_size: u32,
) -> Result<Connection, CipherDatabaseError> {
    let connection = open_read_only_connection(path)?;
    apply_aes256_derived_key(&connection, key, legacy, legacy_page_size)?;
    finish_read_only_open(connection)
}

fn open_read_only_connection(path: &Path) -> Result<Connection, CipherDatabaseError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| CipherDatabaseError::Open)
}

fn finish_read_only_open(connection: Connection) -> Result<Connection, CipherDatabaseError> {
    validate_schema(&connection)?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|_| CipherDatabaseError::Open)?;
    Ok(connection)
}

fn apply_aes128_key(connection: &Connection, key: &[u8]) -> Result<(), CipherDatabaseError> {
    if key.is_empty() || key.len() > c_int::MAX as usize {
        return Err(CipherDatabaseError::InvalidKey);
    }
    // SAFETY: rusqlite owns a live sqlite3 connection for the duration of this
    // call. All string constants are NUL-terminated, and the key buffer remains
    // valid until sqlite3_key returns. No SQL has run on this connection yet.
    unsafe {
        let handle = connection.handle();
        configure_aes128(handle, false, 0)?;
        let result = sqlite3_key(handle, key.as_ptr().cast(), key.len() as c_int);
        if result != SQLITE_OK {
            return Err(CipherDatabaseError::InvalidKey);
        }
    }
    Ok(())
}

fn apply_aes128_derived_key(
    connection: &Connection,
    key: &[u8],
    legacy: bool,
    legacy_page_size: u32,
) -> Result<(), CipherDatabaseError> {
    if key.len() != 16 || legacy_page_size > 65_536 {
        return Err(CipherDatabaseError::InvalidKey);
    }
    // SAFETY: the connection handle is live, the bridge is linked from the
    // verified SQLite3MultipleCiphers amalgamation, and the 16-byte key buffer
    // remains valid for the duration of this call. No SQL has run yet.
    unsafe {
        let handle = connection.handle();
        configure_aes128(handle, legacy, legacy_page_size)?;
        let result = sqlite3mc_key_aes128_derived(handle, key.as_ptr(), key.len() as c_int);
        if result != SQLITE_OK {
            return Err(CipherDatabaseError::InvalidKey);
        }
    }
    Ok(())
}

fn apply_aes256_derived_key(
    connection: &Connection,
    key: &[u8],
    legacy: bool,
    legacy_page_size: u32,
) -> Result<(), CipherDatabaseError> {
    if key.len() != 32 || legacy_page_size > 65_536 {
        return Err(CipherDatabaseError::InvalidKey);
    }
    // SAFETY: identical lifetime and initialization guarantees to the AES-128
    // bridge above, with the exact 32-byte key length required by AES-256-CBC.
    unsafe {
        let handle = connection.handle();
        configure_cipher(handle, AES256CBC, legacy, legacy_page_size)?;
        let result = sqlite3mc_key_aes256_derived(handle, key.as_ptr(), key.len() as c_int);
        if result != SQLITE_OK {
            return Err(CipherDatabaseError::InvalidKey);
        }
    }
    Ok(())
}

unsafe fn configure_aes128(
    handle: *mut sqlite3,
    legacy: bool,
    legacy_page_size: u32,
) -> Result<(), CipherDatabaseError> {
    unsafe { configure_cipher(handle, AES128CBC, legacy, legacy_page_size) }
}

unsafe fn configure_cipher(
    handle: *mut sqlite3,
    cipher_name: &[u8],
    legacy: bool,
    legacy_page_size: u32,
) -> Result<(), CipherDatabaseError> {
    // SAFETY: callers provide a live sqlite3 handle. All string constants are
    // NUL-terminated and remain valid for each FFI call.
    let cipher_index = unsafe { sqlite3mc_cipher_index(cipher_name.as_ptr().cast()) };
    if cipher_index < 0
        || unsafe { sqlite3mc_config(handle, CIPHER.as_ptr().cast(), cipher_index) } < 0
        || unsafe {
            sqlite3mc_config_cipher(
                handle,
                cipher_name.as_ptr().cast(),
                LEGACY.as_ptr().cast(),
                i32::from(legacy),
            )
        } < 0
        || unsafe {
            sqlite3mc_config_cipher(
                handle,
                cipher_name.as_ptr().cast(),
                LEGACY_PAGE_SIZE.as_ptr().cast(),
                legacy_page_size as c_int,
            )
        } < 0
    {
        return Err(CipherDatabaseError::Configuration);
    }
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<(), CipherDatabaseError> {
    connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|_| ())
        .map_err(|_| CipherDatabaseError::InvalidKey)
}

#[cfg(test)]
pub fn create_encrypted_fixture(
    path: &Path,
    key: &[u8],
) -> Result<Connection, CipherDatabaseError> {
    let connection = Connection::open(path).map_err(|_| CipherDatabaseError::Open)?;
    apply_aes128_key(&connection, key)?;
    Ok(connection)
}

#[cfg(test)]
pub fn create_encrypted_derived_fixture(
    path: &Path,
    key: &[u8],
) -> Result<Connection, CipherDatabaseError> {
    let connection = Connection::open(path).map_err(|_| CipherDatabaseError::Open)?;
    apply_aes128_derived_key(&connection, key, false, 0)?;
    Ok(connection)
}

#[cfg(test)]
pub fn create_encrypted_derived_aes256_fixture(
    path: &Path,
    key: &[u8],
) -> Result<Connection, CipherDatabaseError> {
    let connection = Connection::open(path).map_err(|_| CipherDatabaseError::Open)?;
    apply_aes256_derived_key(&connection, key, false, 0)?;
    Ok(connection)
}
