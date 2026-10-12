use sha2::Digest;
use sha2::Sha256;
use std::io::Read;
use std::path::Path;

pub(super) fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(encode_hex(&hasher.finalize()))
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

pub(super) fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(());
    }
    pairs
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or(())? as u8;
            let low = (pair[1] as char).to_digit(16).ok_or(())? as u8;
            Ok((high << 4) | low)
        })
        .collect()
}

pub(super) fn is_valid_content_hash(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
}

pub(super) fn sanitize_file_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        "item".into()
    } else {
        cleaned.into()
    }
}
