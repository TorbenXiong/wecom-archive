use crate::{ArchiveStore, ConversationListItem, StoreError};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

impl ArchiveStore {
    pub fn list_conversations(
        &self,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<ConversationListItem>, StoreError> {
        // A read-only pre-accelerator v2 archive is still readable. Writable
        // opens build statistics; materializing the page bounds work in either path.
        let has_stats: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'archive_conversation_stats')",
            [],
            |row| row.get(0),
        )?;
        let (message_count, media_count) = if has_stats {
            (
                "coalesce((SELECT message_count FROM archive_conversation_stats s
                           WHERE s.conversation_id = c.conversation_id), 0)",
                "coalesce((SELECT media_count FROM archive_conversation_stats s
                           WHERE s.conversation_id = c.conversation_id), 0)",
            )
        } else {
            (
                "(SELECT count(*) FROM messages m WHERE m.conversation_id = c.conversation_id)",
                "(SELECT count(DISTINCT mm.media_id)
                  FROM messages m JOIN message_media mm ON mm.stable_message_id = m.stable_message_id
                  WHERE m.conversation_id = c.conversation_id)",
            )
        };
        let sql = format!(
            "WITH page AS MATERIALIZED (
                SELECT conversation_id, display_name, conversation_type, last_message_at
                FROM conversations ORDER BY last_message_at DESC, conversation_id ASC
                LIMIT ?1 OFFSET ?2
             ) SELECT
                c.conversation_id,
                c.display_name,
                c.conversation_type,
                c.last_message_at,
                {message_count},
                {media_count},
                (SELECT count(*) FROM conversation_participants cp
                 WHERE cp.conversation_id = c.conversation_id)
             FROM page c
             ORDER BY c.last_message_at DESC, c.conversation_id ASC"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                limit.clamp(1, 200) as i64,
                offset.min(i64::MAX as u64) as i64
            ],
            |row| {
                let last_message_at: String = row.get(3)?;
                let parsed = DateTime::parse_from_rfc3339(&last_message_at)
                    .map(|value| value.with_timezone(&Utc))
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(ConversationListItem {
                    conversation_id: row.get(0)?,
                    display_name: row.get(1)?,
                    conversation_type: row.get(2)?,
                    participant_names: Vec::new(),
                    last_message_at: parsed,
                    message_count: row.get::<_, i64>(4)? as u64,
                    media_count: row.get::<_, i64>(5)? as u64,
                    participant_count: row.get::<_, i64>(6)? as u64,
                })
            },
        )?;
        let mut conversations = rows.collect::<Result<Vec<_>, _>>()?;
        for conversation in &mut conversations {
            // The source identifier is authoritative for the conversation kind. Older
            // archives may have been ingested before the explicit type was persisted,
            // or may contain a stale participant-count inference.
            if conversation.conversation_id.starts_with("S:") {
                conversation.conversation_type = Some("direct".into());
            } else if conversation.conversation_id.starts_with("R:") {
                conversation.conversation_type = Some("group".into());
            }

            if conversation.conversation_type.as_deref() == Some("direct") {
                let mut names = self
                    .connection
                    .prepare(
                        "SELECT p.display_name
                         FROM conversation_participants cp
                         JOIN participants p ON p.participant_id = cp.participant_id
                         WHERE cp.conversation_id = ?1
                           AND p.display_name IS NOT NULL
                           AND trim(p.display_name) <> ''
                         ORDER BY cp.participant_id ASC",
                    )?
                    .query_map([conversation.conversation_id.as_str()], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                // Direct conversations created by older source adapters may not have
                // conversation_participants rows yet; recover the two IDs from S:a_b.
                if names.is_empty()
                    && let Some(member_ids) = conversation.conversation_id.strip_prefix("S:")
                {
                    names = member_ids
                        .split('_')
                        .filter(|member_id| !member_id.is_empty())
                        .filter_map(|member_id| {
                            self.connection
                                .query_row(
                                    "SELECT display_name FROM participants WHERE participant_id = ?1",
                                    [member_id],
                                    |row| row.get::<_, Option<String>>(0),
                                )
                                .optional()
                                .ok()
                                .flatten()
                                .flatten()
                                .filter(|name| !name.trim().is_empty())
                        })
                        .collect();
                }
                names.dedup();
                conversation.participant_names = names;
            }
        }
        Ok(conversations)
    }
}
