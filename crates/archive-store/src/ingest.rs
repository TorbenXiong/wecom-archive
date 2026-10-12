use crate::revision::merge_message;
use crate::sql::enum_text;
use crate::{ArchiveStore, IngestSummary, StoreError};
use archive_domain::{ArchiveBatchV1, ConversationV1, ParticipantV1};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

impl ArchiveStore {
    pub fn ingest_batch(&mut self, batch: &ArchiveBatchV1) -> Result<IngestSummary, StoreError> {
        let transaction = self.connection.transaction()?;
        let existing_batch_hash: Option<String> = transaction
            .query_row(
                "SELECT content_sha256 FROM collection_batches WHERE batch_id = ?1",
                [batch.batch_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if existing_batch_hash
            .as_deref()
            .is_some_and(|hash| hash != batch.content_sha256)
        {
            return Err(StoreError::BatchConflict);
        }
        transaction.execute(
            "INSERT OR IGNORE INTO collection_batches (
                batch_id, source_adapter, source_instance_id, collected_at,
                cursor_before, cursor_after, content_sha256, employee_notice_json,
                external_consent, collection_scope_json, retention_json, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running')",
            params![
                batch.batch_id.to_string(),
                batch.source_adapter,
                batch.source_instance_id,
                batch.collected_at.to_rfc3339(),
                batch.cursor_before,
                batch.cursor_after,
                batch.content_sha256,
                serde_json::to_string(&batch.employee_notice)?,
                enum_text(&batch.external_contact_consent)?,
                serde_json::to_string(&batch.collection_scope)?,
                serde_json::to_string(&batch.retention)?,
            ],
        )?;

        let mut summary = IngestSummary {
            inserted: 0,
            unchanged: 0,
            revised: 0,
        };
        crate::derived::repair_fts_rows(&transaction, batch)?;
        let mut conversation_times = BTreeMap::<&str, String>::new();
        for message in &batch.messages {
            message
                .validate()
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            if merge_message(&transaction, message, &mut summary)? {
                // Preserve SQLite's original max(TEXT, TEXT) semantics. Delay
                // recent-conversation index writes until all messages are merged.
                let sent_at = message.sent_at.to_rfc3339();
                conversation_times
                    .entry(&message.conversation_id)
                    .and_modify(|latest| {
                        if sent_at > *latest {
                            latest.clone_from(&sent_at);
                        }
                    })
                    .or_insert(sent_at);
            }
        }
        let mut update_conversation = transaction.prepare_cached(
            "UPDATE conversations SET last_message_at = max(last_message_at, ?2)
             WHERE conversation_id = ?1",
        )?;
        for (conversation, sent_at) in conversation_times {
            update_conversation.execute(params![conversation, sent_at])?;
        }
        drop(update_conversation);
        transaction.execute(
            "UPDATE collection_batches SET status = 'completed', message_count = ?2 WHERE batch_id = ?1",
            params![batch.batch_id.to_string(), batch.messages.len() as i64],
        )?;
        transaction.execute(
            "INSERT INTO audit_events (event_id, occurred_at, event_type, actor_kind, subject_id, detail_json)
             VALUES (lower(hex(randomblob(16))), ?1, 'batch_ingested', 'local_user', ?2, ?3)",
            params![
                Utc::now().to_rfc3339(),
                batch.batch_id.to_string(),
                serde_json::to_string(&summary)?,
            ],
        )?;
        transaction.commit()?;
        Ok(summary)
    }

    pub fn upsert_directory(
        &mut self,
        conversations: &[ConversationV1],
        participants: &[ParticipantV1],
    ) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        for participant in participants {
            transaction.execute(
                "INSERT INTO participants (participant_id, display_name, participant_kind)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(participant_id) DO UPDATE SET
                    display_name = coalesce(excluded.display_name, participants.display_name),
                    participant_kind = coalesce(excluded.participant_kind, participants.participant_kind)",
                params![
                    participant.participant_id,
                    participant.display_name,
                    participant.participant_kind,
                ],
            )?;
        }
        for conversation in conversations {
            transaction.execute(
                "UPDATE conversations SET
                    display_name = coalesce(?2, display_name),
                    conversation_type = coalesce(?3, conversation_type)
                 WHERE conversation_id = ?1",
                params![
                    conversation.conversation_id,
                    conversation.display_name,
                    conversation.conversation_type,
                ],
            )?;
            for participant_id in &conversation.participant_ids {
                transaction.execute(
                    "INSERT OR IGNORE INTO participants (participant_id) VALUES (?1)",
                    [participant_id],
                )?;
                transaction.execute(
                    "INSERT OR IGNORE INTO conversation_participants (conversation_id, participant_id)
                     SELECT ?1, ?2 WHERE EXISTS (
                        SELECT 1 FROM conversations WHERE conversation_id = ?1
                     )",
                    params![conversation.conversation_id, participant_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}
