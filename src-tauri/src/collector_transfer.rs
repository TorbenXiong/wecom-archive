use crate::collector_config::EnterpriseCollectorConfig;
use crate::state::{AppState, CommandError, internal_error};
use archive_domain::ClientExportV1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

pub(super) fn message_fingerprints(export: &ClientExportV1) -> BTreeMap<String, String> {
    export
        .batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .filter_map(|message| {
            let mut stable = message.clone();
            stable.collection_batch_id = uuid::Uuid::nil();
            serde_json::to_vec(&stable).ok().map(|bytes| {
                (
                    message.stable_message_id.clone(),
                    hex::encode(Sha256::digest(bytes)),
                )
            })
        })
        .collect()
}

pub(super) fn has_upload_changes(state: &AppState, export: &ClientExportV1) -> bool {
    if state
        .schedule_status
        .lock()
        .map_or(true, |status| status.last_success_at.is_none())
    {
        return true;
    }
    let current = message_fingerprints(export);
    state.uploaded_messages.lock().map_or(true, |uploaded| {
        current
            .iter()
            .any(|(id, fingerprint)| uploaded.get(id) != Some(fingerprint))
    })
}

pub(super) fn record_upload_success(state: &AppState, export: &ClientExportV1) {
    let current = message_fingerprints(export);
    if let (Ok(mut uploaded), Ok(mut status)) =
        (state.uploaded_messages.lock(), state.schedule_status.lock())
    {
        uploaded.extend(current);
        status.uploaded_count = uploaded.len() as u64;
    }
}

pub(super) fn post_enterprise_package(
    state: &AppState,
    endpoint: &str,
    token: &[u8],
    package: &[u8],
) -> Result<String, CommandError> {
    crate::collector_upload::upload(state, endpoint, token, package)
}

pub(super) fn create_enterprise_package(
    export: &ClientExportV1,
    config: &EnterpriseCollectorConfig,
) -> Result<Vec<u8>, CommandError> {
    if config.data_redaction {
        encrypt_enterprise_export(&redact_sensitive_export(export.clone())?, config)
    } else {
        encrypt_enterprise_export(export, config)
    }
}

pub(super) fn encrypt_enterprise_export(
    export: &ClientExportV1,
    config: &EnterpriseCollectorConfig,
) -> Result<Vec<u8>, CommandError> {
    let public_key = hex::decode(&config.public_key_hex).map_err(|_| internal_error())?;
    let plaintext = Zeroizing::new(serde_json::to_vec(export).map_err(|_| internal_error())?);
    let encrypted =
        source_windows::enterprise_crypto::encrypt(&public_key, &plaintext).map_err(|_| {
            CommandError {
                code: "ENTERPRISE_ENCRYPTION_FAILED",
                message: "企业加密包生成失败；不会降级为明文导出。".into(),
                recoverable: true,
            }
        })?;
    // The envelope encoder hashes JSON incrementally. It does not need to keep
    // the plaintext while emitting the much larger hexadecimal ciphertext.
    drop(plaintext);
    archive_transfer::create_enterprise_package(
        export,
        &config.organization_id,
        &config.key_id,
        &encrypted.wrapped_dek,
        &encrypted.nonce,
        &encrypted.ciphertext,
        &encrypted.tag,
    )
    .map_err(|_| CommandError {
        code: "ENTERPRISE_ENCRYPTION_FAILED",
        message: "企业加密包生成失败；不会降级为明文导出。".into(),
        recoverable: true,
    })
}

pub(super) fn redact_sensitive_export(
    mut redacted: ClientExportV1,
) -> Result<ClientExportV1, CommandError> {
    for batch in &mut redacted.batches {
        archive_transfer::redact_sensitive_messages(&mut batch.messages);
        batch.content_sha256 =
            archive_transfer::json_sha256(&batch.messages).map_err(|_| internal_error())?;
    }
    let checksum_value = if redacted.media_transport == "embedded_hex_v1" {
        archive_transfer::json_sha256(&(
            &redacted.conversations,
            &redacted.participants,
            &redacted.batches,
            &redacted.media_blobs,
        ))
    } else {
        archive_transfer::json_sha256(&(
            &redacted.conversations,
            &redacted.participants,
            &redacted.batches,
        ))
    };
    redacted.content_sha256 = checksum_value.map_err(|_| internal_error())?;
    Ok(redacted)
}

#[cfg(test)]
pub(super) fn redact_sensitive_text(value: &str) -> String {
    archive_transfer::redact_sensitive_text(value)
}

#[cfg(test)]
pub(super) fn redact_sensitive_json(value: &mut serde_json::Value) {
    archive_transfer::redact_sensitive_json(value);
}
