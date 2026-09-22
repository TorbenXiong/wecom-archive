use std::ffi::{c_char, c_int, c_void};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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
) -> Result<PathBuf, CipherDatabaseError> {
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
    merge_raw_wxsqlite3_wal(path, &output_path, key, page_size)?;
    Ok(output_path)
}

fn merge_raw_wxsqlite3_wal(
    encrypted_database: &Path,
    plain_database: &Path,
    key: &[u8],
    page_size: usize,
) -> Result<(), CipherDatabaseError> {
    let wal_path = PathBuf::from(format!("{}-wal", encrypted_database.display()));
    let Ok(wal) = std::fs::read(&wal_path) else {
        return Ok(());
    };
    if wal.is_empty() {
        return Ok(());
    }
    if wal.len() < 32 {
        return Err(CipherDatabaseError::Open);
    }

    let magic = read_be_u32(&wal[0..4]);
    if magic & !1 != 0x377f_0682 || read_be_u32(&wal[8..12]) as usize != page_size {
        return Err(CipherDatabaseError::Open);
    }
    let checksum_endian = if magic & 1 == 1 {
        ChecksumEndian::Big
    } else {
        ChecksumEndian::Little
    };
    let mut checksum = wal_checksum(checksum_endian, &wal[..24], (0, 0));
    if checksum != (read_be_u32(&wal[24..28]), read_be_u32(&wal[28..32])) {
        return Err(CipherDatabaseError::Open);
    }

    let salt = &wal[16..24];
    let frame_size = 24_usize
        .checked_add(page_size)
        .ok_or(CipherDatabaseError::Open)?;
    let mut frames = Vec::<(u32, Vec<u8>)>::new();
    let mut committed_frame_count = 0_usize;
    let mut committed_database_pages = 0_u32;
    let mut offset = 32_usize;
    while offset
        .checked_add(frame_size)
        .is_some_and(|end| end <= wal.len())
    {
        let frame = &wal[offset..offset + frame_size];
        if &frame[8..16] != salt {
            break;
        }
        checksum = wal_checksum(checksum_endian, &frame[..8], checksum);
        checksum = wal_checksum(checksum_endian, &frame[24..], checksum);
        if checksum != (read_be_u32(&frame[16..20]), read_be_u32(&frame[20..24])) {
            break;
        }
        let page_number = read_be_u32(&frame[..4]);
        if page_number == 0 || page_number > i32::MAX as u32 {
            break;
        }
        let mut decrypted = vec![0_u8; page_size];
        let status = unsafe {
            sqlite3mc_decrypt_wxsqlite3_raw_page(
                frame[24..].as_ptr(),
                page_size as c_int,
                page_number as c_int,
                key.as_ptr(),
                key.len() as c_int,
                decrypted.as_mut_ptr(),
            )
        };
        if status != SQLITE_OK {
            return Err(CipherDatabaseError::InvalidKey);
        }
        frames.push((page_number, decrypted));
        let database_pages = read_be_u32(&frame[4..8]);
        if database_pages != 0 {
            committed_frame_count = frames.len();
            committed_database_pages = database_pages;
        }
        offset += frame_size;
    }

    if committed_frame_count == 0 {
        return Ok(());
    }
    let final_len = u64::from(committed_database_pages)
        .checked_mul(page_size as u64)
        .ok_or(CipherDatabaseError::Open)?;
    let mut output = OpenOptions::new()
        .read(true)
        .write(true)
        .open(plain_database)
        .map_err(|_| CipherDatabaseError::Open)?;
    output
        .set_len(final_len)
        .map_err(|_| CipherDatabaseError::Open)?;
    for (page_number, page) in frames.into_iter().take(committed_frame_count) {
        if page_number > committed_database_pages {
            continue;
        }
        let page_offset = u64::from(page_number - 1)
            .checked_mul(page_size as u64)
            .ok_or(CipherDatabaseError::Open)?;
        output
            .seek(SeekFrom::Start(page_offset))
            .and_then(|_| output.write_all(&page))
            .map_err(|_| CipherDatabaseError::Open)?;
    }
    output.flush().map_err(|_| CipherDatabaseError::Open)
}

#[derive(Clone, Copy)]
enum ChecksumEndian {
    Little,
    Big,
}

fn wal_checksum(endian: ChecksumEndian, bytes: &[u8], initial: (u32, u32)) -> (u32, u32) {
    debug_assert!(bytes.len() >= 8 && bytes.len().is_multiple_of(8));
    let mut first = initial.0;
    let mut second = initial.1;
    for words in bytes.chunks_exact(8) {
        let left = match endian {
            ChecksumEndian::Little => u32::from_le_bytes(words[..4].try_into().unwrap()),
            ChecksumEndian::Big => u32::from_be_bytes(words[..4].try_into().unwrap()),
        };
        let right = match endian {
            ChecksumEndian::Little => u32::from_le_bytes(words[4..].try_into().unwrap()),
            ChecksumEndian::Big => u32::from_be_bytes(words[4..].try_into().unwrap()),
        };
        first = first.wrapping_add(left).wrapping_add(second);
        second = second.wrapping_add(right).wrapping_add(first);
    }
    (first, second)
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four-byte SQLite WAL field"))
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
