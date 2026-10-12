use crate::sql::table_count;
use crate::{ArchiveStore, ArchiveSummary, StoreError};
use rusqlite::OptionalExtension;

impl ArchiveStore {
    pub fn media_mime(&self, content_hash: &str) -> Result<Option<String>, StoreError> {
        self.connection
            .query_row(
                "SELECT mime_type FROM media WHERE content_sha256 = ?1",
                [content_hash],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(StoreError::from)
            .map(|value| value.flatten())
    }

    pub fn message_count(&self) -> Result<u64, StoreError> {
        Ok(self
            .connection
            .query_row("SELECT count(*) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })? as u64)
    }

    pub fn summary(&self) -> Result<ArchiveSummary, StoreError> {
        Ok(ArchiveSummary {
            conversation_count: table_count(&self.connection, "conversations")?,
            message_count: table_count(&self.connection, "messages")?,
            media_count: table_count(&self.connection, "media")?,
            revision: table_count(&self.connection, "audit_events")?,
        })
    }
}
