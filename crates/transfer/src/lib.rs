use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use archive_domain::{
    ArchiveBatchV1, CLIENT_EXPORT_SCHEMA_VERSION, ClientExportV1, ConversationV1, DomainError,
    ParticipantV1,
};
use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("target already exists")]
    TargetExists,
    #[error("unsafe target path")]
    UnsafeTarget,
    #[error("client export is invalid: {0}")]
    Invalid(#[from] DomainError),
    #[error("client export checksum mismatch")]
    ChecksumMismatch,
    #[error("client export I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("client export JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("client export CSV failed: {0}")]
    Csv(#[from] csv::Error),
}

pub fn create_export(
    client_version: &str,
    mut batches: Vec<ArchiveBatchV1>,
    conversations: Vec<ConversationV1>,
    participants: Vec<ParticipantV1>,
) -> Result<ClientExportV1, TransferError> {
    sanitize_batches(&mut batches);
    let message_count = batches
        .iter()
        .map(|batch| batch.messages.len() as u64)
        .sum();
    let media = batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .flat_map(|message| &message.media)
        .collect::<Vec<_>>();
    let media_count = media.len() as u64;
    let missing_media_count = media
        .iter()
        .filter(|item| item.content_hash.is_none())
        .count() as u64;
    let content_sha256 = hash_content(&conversations, &participants, &batches)?;
    let export = ClientExportV1 {
        schema_version: CLIENT_EXPORT_SCHEMA_VERSION.into(),
        export_id: Uuid::new_v4(),
        generated_at: Utc::now(),
        client_version: client_version.into(),
        media_transport: "metadata_only".into(),
        conversations,
        participants,
        batches,
        message_count,
        media_count,
        missing_media_count,
        content_sha256,
    };
    export.validate()?;
    Ok(export)
}

fn sanitize_batches(batches: &mut [ArchiveBatchV1]) {
    for batch in batches {
        for message in &mut batch.messages {
            sanitize_json(&mut message.raw_payload, None);
            for media in &mut message.media {
                media.source_locator = media
                    .content_hash
                    .as_deref()
                    .map(|hash| format!("sha256:{hash}"))
                    .unwrap_or_else(|| "unavailable".into());
            }
        }
    }
}

fn sanitize_json(value: &mut serde_json::Value, key: Option<&str>) {
    match value {
        serde_json::Value::String(text)
            if key.is_some_and(|key| {
                let key = key.to_ascii_lowercase();
                key.contains("path") || key.contains("local_uri")
            }) =>
        {
            let digest = hex::encode(Sha256::digest(text.as_bytes()));
            *text = format!("redacted:sha256:{digest}");
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_json(item, key);
            }
        }
        serde_json::Value::Object(entries) => {
            for (child_key, child) in entries {
                sanitize_json(child, Some(child_key));
            }
        }
        _ => {}
    }
}

pub fn write_json(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    atomic_write(target, |writer| {
        serde_json::to_writer(writer, export)?;
        Ok(())
    })
}

pub fn write_csv(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    atomic_write(target, |writer| {
        writer.write_all(&[0xEF, 0xBB, 0xBF])?;
        let mut csv = csv::WriterBuilder::new().from_writer(writer);
        csv.write_record([
            "batch_id",
            "stable_message_id",
            "conversation_id",
            "sender_id",
            "sent_at",
            "direction",
            "message_type",
            "body_text",
            "lifecycle",
            "raw_type",
        ])?;
        for batch in &export.batches {
            for message in &batch.messages {
                csv.write_record([
                    batch.batch_id.to_string(),
                    safe_csv(&message.stable_message_id),
                    safe_csv(&message.conversation_id),
                    safe_csv(message.sender_id.as_deref().unwrap_or_default()),
                    message.sent_at.to_rfc3339(),
                    enum_text(&message.direction)?,
                    enum_text(&message.message_type)?,
                    safe_csv(message.body_text.as_deref().unwrap_or_default()),
                    enum_text(&message.lifecycle)?,
                    safe_csv(&message.raw_type),
                ])?;
            }
        }
        csv.flush()?;
        Ok(())
    })
}

pub fn read_json(path: &Path) -> Result<ClientExportV1, TransferError> {
    let reader = BufReader::new(File::open(path)?);
    let export: ClientExportV1 = serde_json::from_reader(reader)?;
    export.validate()?;
    verify_checksum(&export)?;
    Ok(export)
}

pub fn read_json_slice(bytes: &[u8]) -> Result<ClientExportV1, TransferError> {
    let export: ClientExportV1 = serde_json::from_slice(bytes)?;
    export.validate()?;
    verify_checksum(&export)?;
    Ok(export)
}

fn verify_checksum(export: &ClientExportV1) -> Result<(), TransferError> {
    if hash_content(&export.conversations, &export.participants, &export.batches)?
        == export.content_sha256
    {
        Ok(())
    } else {
        Err(TransferError::ChecksumMismatch)
    }
}

fn hash_content(
    conversations: &[ConversationV1],
    participants: &[ParticipantV1],
    batches: &[ArchiveBatchV1],
) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(&(conversations, participants, batches))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn atomic_write<F>(target: &Path, action: F) -> Result<(), TransferError>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(), TransferError>,
{
    if target.exists() {
        return Err(TransferError::TargetExists);
    }
    let parent = target.parent().ok_or(TransferError::UnsafeTarget)?;
    fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(TransferError::UnsafeTarget)?;
    let partial = target.with_file_name(format!("{file_name}.partial"));
    if partial.exists() {
        fs::remove_file(&partial)?;
    }
    let result = (|| {
        let mut writer = BufWriter::new(File::create(&partial)?);
        action(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&partial, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(partial);
    }
    result
}

fn safe_csv(value: &str) -> String {
    if value
        .trim_start_matches([' ', '\t', '\r', '\n'])
        .starts_with(['=', '+', '-', '@'])
    {
        format!("'{value}")
    } else {
        value.into()
    }
}

fn enum_text<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{
        BATCH_SCHEMA_VERSION, CollectionScope, EmployeeNoticeEvidence, ExternalContactConsent,
        RetentionDirective,
    };

    fn empty_batch() -> ArchiveBatchV1 {
        ArchiveBatchV1 {
            schema_version: BATCH_SCHEMA_VERSION.into(),
            batch_id: Uuid::new_v4(),
            source_adapter: "fixture".into(),
            source_instance_id: "source".into(),
            collected_at: Utc::now(),
            employee_notice: EmployeeNoticeEvidence {
                notice_version: "fixture".into(),
                displayed_at: Utc::now(),
                acknowledged_at: Some(Utc::now()),
                evidence_hash: "fixture".into(),
            },
            external_contact_consent: ExternalContactConsent::NotApplicable,
            collection_scope: CollectionScope {
                source_ids: vec!["source".into()],
                conversation_ids: vec![],
                starts_at: None,
                ends_at: None,
                message_types: vec![],
            },
            cursor_before: None,
            cursor_after: None,
            messages: vec![],
            content_sha256: "fixture".into(),
            retention: RetentionDirective {
                policy_id: None,
                expires_at: None,
                legal_hold: false,
            },
        }
    }

    #[test]
    fn json_round_trip_is_importable() {
        let export = create_export("fixture-client", vec![empty_batch()], vec![], vec![]).unwrap();
        let bytes = serde_json::to_vec(&export).unwrap();
        let imported = read_json_slice(&bytes).unwrap();
        assert_eq!(imported.export_id, export.export_id);
    }

    #[test]
    fn rejects_modified_batch_content() {
        let mut export =
            create_export("fixture-client", vec![empty_batch()], vec![], vec![]).unwrap();
        export.batches[0].cursor_after = Some("modified".into());
        assert!(matches!(
            verify_checksum(&export),
            Err(TransferError::ChecksumMismatch)
        ));
    }
}
