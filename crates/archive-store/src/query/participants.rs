use crate::{ArchiveStore, CollectedUser, ParticipantListItem, StoreError};
use rusqlite::OptionalExtension;

impl ArchiveStore {
    pub fn conversation_directory(
        &self,
        conversation_id: &str,
    ) -> Result<archive_domain::ConversationV1, StoreError> {
        let (display_name, conversation_type) = self.connection.query_row(
            "SELECT display_name, conversation_type FROM conversations WHERE conversation_id = ?1",
            [conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(archive_domain::ConversationV1 {
            conversation_id: conversation_id.into(),
            display_name,
            conversation_type,
            participant_ids: self
                .list_conversation_participants(conversation_id)?
                .into_iter()
                .map(|item| item.participant_id)
                .collect(),
        })
    }

    /// Reads provenance without retaining the batch's archived messages.
    pub fn collection_batch_metadata(
        &self,
        batch_id: uuid::Uuid,
    ) -> Result<archive_domain::ArchiveBatchV1, StoreError> {
        let value = self.connection.query_row(
            "SELECT source_adapter, source_instance_id, collected_at, cursor_before, cursor_after,
                    content_sha256, employee_notice_json, external_consent, collection_scope_json, retention_json
             FROM collection_batches WHERE batch_id = ?1",
            [batch_id.to_string()],
            |row| Ok(serde_json::json!({
                "schema_version": archive_domain::BATCH_SCHEMA_VERSION, "batch_id": batch_id,
                "source_adapter": row.get::<_, String>(0)?, "source_instance_id": row.get::<_, String>(1)?,
                "collected_at": row.get::<_, String>(2)?, "cursor_before": row.get::<_, Option<String>>(3)?,
                "cursor_after": row.get::<_, Option<String>>(4)?, "content_sha256": row.get::<_, String>(5)?,
                "employee_notice": row.get::<_, serde_json::Value>(6)?, "external_contact_consent": row.get::<_, String>(7)?,
                "collection_scope": row.get::<_, serde_json::Value>(8)?, "retention": row.get::<_, serde_json::Value>(9)?, "messages": []
            })),
        )?;
        Ok(serde_json::from_value(value)?)
    }

    pub fn conversation_display_name(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT display_name FROM conversations WHERE conversation_id = ?1",
                [conversation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn list_conversation_participants(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ParticipantListItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT p.participant_id, p.display_name, p.participant_kind, p.metadata_json
             FROM conversation_participants cp
             JOIN participants p ON p.participant_id = cp.participant_id
             WHERE cp.conversation_id = ?1
             ORDER BY coalesce(p.display_name, p.participant_id) COLLATE NOCASE ASC",
        )?;
        let rows = statement.query_map([conversation_id], |row| {
            Ok(ParticipantListItem {
                participant_id: row.get(0)?,
                display_name: row.get(1)?,
                participant_kind: row.get(2)?,
                profile: row
                    .get::<_, Option<String>>(3)?
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .map(|value| crate::participant_profile(&value))
                    .filter(|profile| !profile.is_empty()),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_collected_users(&self) -> Result<Vec<CollectedUser>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sources.source_instance_id,
                    (SELECT coalesce(nullif(trim(p.display_name), ''), m.sender_id)
                     FROM messages m
                     LEFT JOIN participants p ON p.participant_id = m.sender_id
                     WHERE m.source_instance_id = sources.source_instance_id
                       AND m.direction = 'outgoing'
                       AND m.sender_id IS NOT NULL
                     GROUP BY m.sender_id, p.display_name
                     ORDER BY count(*) DESC, m.sender_id ASC
                     LIMIT 1) AS display_name
             FROM (
                SELECT DISTINCT source_instance_id
                FROM collection_batches
                WHERE status = 'completed' AND trim(source_instance_id) <> ''
             ) sources
             ORDER BY coalesce(display_name, sources.source_instance_id) COLLATE NOCASE ASC,
                      sources.source_instance_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CollectedUser {
                source_instance_id: row.get(0)?,
                display_name: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}
