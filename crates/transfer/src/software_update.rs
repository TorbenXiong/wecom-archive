//! Portable update metadata, independent of archive and collector-config schemas.
use serde::{Deserialize, Serialize};

pub const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;

pub fn stable_version(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}

pub fn newer(current: &str, next: &str) -> bool {
    match (stable_version(current), stable_version(next)) {
        (Some(current), Some(next)) => next > current,
        _ => false,
    }
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftwareUpdate {
    pub request_id: String,
    pub collector_id: String,
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedSoftwareUpdate {
    pub update: SoftwareUpdate,
    pub signature_hex: String,
}

pub fn signing_payload(update: &SoftwareUpdate) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(
        &serde_json::json!({"schemaVersion": "software-update.v1", "update": update}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_strict_stable_versions_can_upgrade() {
        assert!(newer("0.0.9", "v0.0.10"));
        assert!(!newer("0.0.10", "0.0.9"));
        assert!(!newer("0.0.10", "0.0.10"));
        for invalid in [
            "0.1",
            "01.2.3",
            "1.2.bad",
            "1.2.3-beta",
            "1.2.3+build",
            "1.2.3.4",
            "99999999999999999999999.0.0",
        ] {
            assert!(!newer("0.0.10", invalid));
        }
    }
}
