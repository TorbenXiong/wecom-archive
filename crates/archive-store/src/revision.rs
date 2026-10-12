use crate::sql::{enum_text, execute_cached};
use crate::{IngestSummary, StoreError};
use archive_domain::MessageV1;
use chrono::Utc;
use rusqlite::{OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

pub(super) fn merge_message(
    transaction: &Transaction<'_>,
    message: &MessageV1,
    summary: &mut IngestSummary,
) -> Result<bool, StoreError> {
    let payload_json = serde_json::to_string(message)?;
    let mut content_message = message.clone();
    content_message.collection_batch_id = uuid::Uuid::nil();
    let content_json = serde_json::to_vec(&content_message)?;
    let content_hash = hex::encode(Sha256::digest(&content_json));
    let duplicate: Option<String> = transaction
        .prepare_cached(
            "SELECT payload_json
             FROM messages
             WHERE conversation_id = ?1
               AND source_message_id = ?2
               AND sender_id IS ?3
               AND sent_at = ?4
             LIMIT 1",
        )?
        .query_row(
            params![
                message.conversation_id,
                message.source_message_id,
                message.sender_id,
                message.sent_at.to_rfc3339(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    if duplicate.as_deref().is_some_and(|payload| {
        serde_json::from_str::<MessageV1>(payload)
            .ok()
            .and_then(|existing| canonical_message_hash(&existing).ok())
            .is_some_and(|hash| hash == canonical_message_hash(message).unwrap_or_default())
    }) {
        summary.unchanged += 1;
        return Ok(false);
    }
    let existing: Option<String> = transaction
        .prepare_cached("SELECT content_sha256 FROM messages WHERE stable_message_id = ?1")?
        .query_row([message.stable_message_id.as_str()], |row| row.get(0))
        .optional()?;

    let inserted = existing.is_none();
    match existing {
        None => {
            execute_cached(
                transaction,
                "INSERT OR IGNORE INTO conversations (conversation_id, last_message_at)
                 VALUES (?1, ?2)",
                params![message.conversation_id, message.sent_at.to_rfc3339()],
            )?;
            if let Some(sender_id) = &message.sender_id {
                execute_cached(
                    transaction,
                    "INSERT OR IGNORE INTO participants (participant_id) VALUES (?1)",
                    [sender_id],
                )?;
                execute_cached(transaction,
                    "INSERT OR IGNORE INTO conversation_participants (conversation_id, participant_id)
                     VALUES (?1, ?2)",
                    params![message.conversation_id, sender_id],
                )?;
            }
            execute_cached(transaction,
                "INSERT INTO messages (
                    stable_message_id, source_kind, source_instance_id, source_message_id,
                    conversation_id, sender_id, sent_at, direction, message_type, body_text,
                    quoted_message_id, lifecycle, raw_type, parser_version, first_batch_id,
                    last_batch_id, content_sha256, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, ?16, ?17)",
                params![
                    message.stable_message_id,
                    message.source_kind,
                    message.source_instance_id,
                    message.source_message_id,
                    message.conversation_id,
                    message.sender_id,
                    message.sent_at.to_rfc3339(),
                    enum_text(&message.direction)?,
                    enum_text(&message.message_type)?,
                    message.body_text,
                    message.quoted_message_id,
                    enum_text(&message.lifecycle)?,
                    message.raw_type,
                    message.parser_version,
                    message.collection_batch_id.to_string(),
                    content_hash,
                    payload_json,
                ],
            )?;
            crate::derived::insert_fts(transaction, message)?;
            summary.inserted += 1;
        }
        Some(previous_hash) if previous_hash == content_hash => {
            execute_cached(
                transaction,
                "UPDATE messages SET last_batch_id = ?2 WHERE stable_message_id = ?1",
                params![
                    message.stable_message_id,
                    message.collection_batch_id.to_string()
                ],
            )?;
            summary.unchanged += 1;
        }
        Some(previous_hash) => {
            let previous_payload: String = transaction
                .prepare_cached("SELECT payload_json FROM messages WHERE stable_message_id = ?1")?
                .query_row([message.stable_message_id.as_str()], |row| row.get(0))?;
            execute_cached(
                transaction,
                "INSERT INTO message_revisions (
                    stable_message_id, observed_batch_id, observed_at, content_sha256, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    message.stable_message_id,
                    message.collection_batch_id.to_string(),
                    Utc::now().to_rfc3339(),
                    previous_hash,
                    previous_payload,
                ],
            )?;
            // Capture/delete FTS rows before the messages UPDATE invalidates the
            // mapping. The deletion and replacement commit with the batch.
            execute_cached(
                transaction,
                "DELETE FROM message_fts WHERE rowid IN (
                    SELECT fts_rowid FROM archive_fts_rows WHERE stable_message_id = ?1
                 )",
                [message.stable_message_id.as_str()],
            )?;
            execute_cached(
                transaction,
                "UPDATE messages SET body_text = ?2, lifecycle = ?3, last_batch_id = ?4,
                    content_sha256 = ?5, payload_json = ?6
                 WHERE stable_message_id = ?1",
                params![
                    message.stable_message_id,
                    message.body_text,
                    enum_text(&message.lifecycle)?,
                    message.collection_batch_id.to_string(),
                    content_hash,
                    payload_json,
                ],
            )?;
            crate::derived::insert_fts(transaction, message)?;
            summary.revised += 1;
        }
    }

    execute_cached(
        transaction,
        "DELETE FROM message_media WHERE stable_message_id = ?1",
        [message.stable_message_id.as_str()],
    )?;
    for (ordinal, media) in message.media.iter().enumerate() {
        let media_id = if let Some(hash) = &media.content_hash {
            format!("sha256:{hash}")
        } else {
            let identity = serde_json::to_vec(&(
                &message.stable_message_id,
                ordinal,
                &media.original_name,
                &media.mime_type,
                &media.source_locator,
            ))?;
            format!("missing:{}", hex::encode(Sha256::digest(identity)))
        };
        execute_cached(transaction,
            "INSERT INTO media (
                media_id, content_sha256, original_name, mime_type, size_bytes, archive_relative_path, source_locator,
                integrity, missing_reason, first_batch_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(media_id) DO UPDATE SET
                original_name = coalesce(excluded.original_name, media.original_name),
                mime_type = coalesce(excluded.mime_type, media.mime_type),
                size_bytes = coalesce(excluded.size_bytes, media.size_bytes),
                archive_relative_path = coalesce(excluded.archive_relative_path, media.archive_relative_path),
                integrity = excluded.integrity,
                missing_reason = excluded.missing_reason",
            params![
                media_id,
                media.content_hash,
                media.original_name,
                media.mime_type,
                media.size_bytes.map(|value| value as i64),
                media.content_hash.as_ref().map(|hash| format!("media/{hash}")),
                media.source_locator,
                enum_text(&media.integrity)?,
                media.missing_reason,
                message.collection_batch_id.to_string(),
            ],
        )?;
        execute_cached(
            transaction,
            "INSERT INTO message_media (stable_message_id, media_id, ordinal)
             VALUES (?1, ?2, ?3)",
            params![message.stable_message_id, media_id, ordinal as i64],
        )?;
    }
    Ok(inserted)
}

fn canonical_message_hash(message: &MessageV1) -> Result<String, serde_json::Error> {
    let mut canonical = message.clone();
    canonical.source_instance_id.clear();
    canonical.source_message_id.clear();
    canonical.stable_message_id.clear();
    canonical.collection_batch_id = uuid::Uuid::nil();
    let bytes = serde_json::to_vec(&canonical)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}
