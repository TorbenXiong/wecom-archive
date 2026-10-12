use crate::{ArchiveStore, CollectedUser, ParticipantListItem, StoreError};
use rusqlite::OptionalExtension;

impl ArchiveStore {
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
            "SELECT p.participant_id, p.display_name, p.participant_kind
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
