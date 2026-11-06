use std::fmt;
use std::path::Path;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use super::{ENTERPRISE_PACKAGE_SCHEMA_VERSION, TransferError};

/// Binary fields decoded once while reading the unchanged JSON envelope.
#[derive(Deserialize)]
pub struct DecodedEnterprisePackage {
    schema_version: String,
    organization_id: String,
    pub key_id: String,
    pub export_id: Uuid,
    encryption: String,
    #[serde(rename = "wrapped_dek_hex", deserialize_with = "decode_hex")]
    pub wrapped_dek: Vec<u8>,
    #[serde(rename = "nonce_hex", deserialize_with = "decode_hex")]
    pub nonce: Vec<u8>,
    #[serde(rename = "ciphertext_hex", deserialize_with = "decode_hex")]
    pub ciphertext: Vec<u8>,
    pub plaintext_sha256: String,
    #[serde(rename = "authentication_tag_hex", deserialize_with = "decode_hex")]
    pub authentication_tag: Vec<u8>,
}

pub fn read_enterprise_package(
    path: &Path,
    organization_id: &str,
) -> Result<DecodedEnterprisePackage, TransferError> {
    // Parsing a slice lets serde_json borrow unescaped hex directly. A reader's
    // scratch string can grow to almost twice the hex length, erasing the saving
    // from decoding only once. This input buffer drops before decryption starts.
    let input = std::fs::read(path)?;
    let package: DecodedEnterprisePackage =
        serde_json::from_slice(&input).map_err(|_| TransferError::InvalidEnterprisePackage)?;
    if package.schema_version != ENTERPRISE_PACKAGE_SCHEMA_VERSION
        || package.encryption != "aes-256-gcm+rsa-oaep-sha256"
        || package.organization_id != organization_id
    {
        return Err(TransferError::EnterpriseKeyMismatch);
    }
    if package.wrapped_dek.is_empty()
        || package.nonce.len() != 12
        || package.ciphertext.is_empty()
        || package.authentication_tag.len() != 16
    {
        return Err(TransferError::InvalidEnterprisePackage);
    }
    Ok(package)
}

fn decode_hex<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct HexVisitor;
    impl Visitor<'_> for HexVisitor {
        type Value = Vec<u8>;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a hexadecimal string")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Vec<u8>, E> {
            if !value.len().is_multiple_of(2) {
                return Err(E::custom("invalid enterprise hex field"));
            }
            // Result<Vec<_>>::collect grows geometrically; reserve the exact
            // decoded length instead, especially around large power-of-two sizes.
            let mut decoded = vec![0; value.len() / 2];
            hex::decode_to_slice(value, &mut decoded)
                .map_err(|_| E::custom("invalid enterprise hex field"))?;
            Ok(decoded)
        }
    }
    deserializer.deserialize_str(HexVisitor)
}

pub(super) struct Hex<'a>(pub &'a [u8]);
impl fmt::Display for Hex<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = [0_u8; 8192];
        for chunk in self.0.chunks(encoded.len() / 2) {
            for (byte, pair) in chunk.iter().zip(encoded.as_chunks_mut::<2>().0) {
                pair[0] = DIGITS[(byte >> 4) as usize];
                pair[1] = DIGITS[(byte & 15) as usize];
            }
            formatter.write_str(
                std::str::from_utf8(&encoded[..chunk.len() * 2]).map_err(|_| fmt::Error)?,
            )?;
        }
        Ok(())
    }
}
impl Serialize for Hex<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // serde_json writes Display fragments into the final JSON string.
        serializer.collect_str(self)
    }
}
