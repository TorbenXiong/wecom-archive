use std::path::Path;
use std::time::Duration;

use archive_domain::{ArchiveBatchV1, ConversationV1, MessageType, MessageV1, ParticipantV1};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid archive data: {0}")]
    Invalid(String),
    #[error("collection batch identifier already exists with different content")]
    BatchConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageQuery {
    pub conversation_id: Option<String>,
    pub participant_id: Option<String>,
    pub text: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub message_type: Option<MessageType>,
    pub media_only: bool,
    pub limit: u32,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestSummary {
    pub inserted: u64,
    pub unchanged: u64,
    pub revised: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveSummary {
    pub conversation_count: u64,
    pub message_count: u64,
    pub media_count: u64,
    /// Monotonically increases whenever a collection batch is ingested, even
    /// when that batch only revises existing messages.
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationListItem {
    pub conversation_id: String,
    pub display_name: Option<String>,
    pub conversation_type: Option<String>,
    pub participant_names: Vec<String>,
    pub last_message_at: DateTime<Utc>,
    pub message_count: u64,
    pub media_count: u64,
    pub participant_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantListItem {
    pub participant_id: String,
    pub display_name: Option<String>,
    pub participant_kind: Option<String>,
}

pub struct ArchiveStore {
    connection: Connection,
}

impl ArchiveStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.pragma_update(None, "query_only", true)?;
        Ok(Self { connection })
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::Invalid(format!(
                "archive schema {version} is newer than supported {SCHEMA_VERSION}"
            )));
        }
        if version == 1 {
            transaction.execute_batch(MIGRATION_V2)?;
        }
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    }

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
        for message in &batch.messages {
            message
                .validate()
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            merge_message(&transaction, message, &mut summary)?;
        }
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

    pub fn query_messages(&self, query: &MessageQuery) -> Result<Vec<MessageV1>, StoreError> {
        // Keep interactive requests bounded while allowing the export worker to
        // read large archives in a few indexed pages instead of hundreds of
        // progressively slower OFFSET queries.
        let limit = query.limit.clamp(1, 50_000) as i64;
        let message_type = query.message_type.as_ref().map(enum_text).transpose()?;
        let search_text = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let search = search_text.map(fts_query);
        let media_search =
            search_text.map(|value| format!("%{}%", value.replace('%', "\\%").replace('_', "\\_")));

        let mut statement = self.connection.prepare(
            "SELECT m.payload_json
             FROM messages m
             WHERE (?1 IS NULL OR m.conversation_id = ?1)
               AND (?2 IS NULL OR m.sender_id = ?2)
               AND (?3 IS NULL OR m.sent_at >= ?3)
               AND (?4 IS NULL OR m.sent_at <= ?4)
               AND (?5 IS NULL OR m.message_type = ?5)
               AND (?6 = 0 OR EXISTS (
                    SELECT 1 FROM message_media mm WHERE mm.stable_message_id = m.stable_message_id
               ))
               AND (?7 IS NULL OR EXISTS (
                    SELECT 1 FROM message_fts f
                    WHERE f.stable_message_id = m.stable_message_id AND message_fts MATCH ?7
               ) OR EXISTS (
                    SELECT 1
                    FROM message_media mm
                    JOIN media media_search ON media_search.media_id = mm.media_id
                    WHERE mm.stable_message_id = m.stable_message_id
                      AND media_search.original_name LIKE ?8 ESCAPE '\\'
               ))
             ORDER BY m.sent_at DESC, m.stable_message_id DESC
             LIMIT ?9 OFFSET ?10",
        )?;

        let rows = statement.query_map(
            params![
                query.conversation_id,
                query.participant_id,
                query.starts_at.map(|value| value.to_rfc3339()),
                query.ends_at.map(|value| value.to_rfc3339()),
                message_type,
                query.media_only as i64,
                search,
                media_search,
                limit,
                query.offset as i64,
            ],
            |row| row.get::<_, String>(0),
        )?;

        rows.map(|row| {
            let json = row?;
            Ok(serde_json::from_str(&json)?)
        })
        .collect()
    }

    pub fn count_messages(&self, query: &MessageQuery) -> Result<u64, StoreError> {
        let message_type = query.message_type.as_ref().map(enum_text).transpose()?;
        let search_text = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let search = search_text.map(fts_query);
        let media_search =
            search_text.map(|value| format!("%{}%", value.replace('%', "\\%").replace('_', "\\_")));
        let count = self.connection.query_row(
            "SELECT count(*)
             FROM messages m
             WHERE (?1 IS NULL OR m.conversation_id = ?1)
               AND (?2 IS NULL OR m.sender_id = ?2)
               AND (?3 IS NULL OR m.sent_at >= ?3)
               AND (?4 IS NULL OR m.sent_at <= ?4)
               AND (?5 IS NULL OR m.message_type = ?5)
               AND (?6 = 0 OR EXISTS (
                    SELECT 1 FROM message_media mm WHERE mm.stable_message_id = m.stable_message_id
               ))
               AND (?7 IS NULL OR EXISTS (
                    SELECT 1 FROM message_fts f
                    WHERE f.stable_message_id = m.stable_message_id AND message_fts MATCH ?7
               ) OR EXISTS (
                    SELECT 1
                    FROM message_media mm
                    JOIN media media_search ON media_search.media_id = mm.media_id
                    WHERE mm.stable_message_id = m.stable_message_id
                      AND media_search.original_name LIKE ?8 ESCAPE '\\'
               ))",
            params![
                query.conversation_id,
                query.participant_id,
                query.starts_at.map(|value| value.to_rfc3339()),
                query.ends_at.map(|value| value.to_rfc3339()),
                message_type,
                query.media_only as i64,
                search,
                media_search,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count.max(0) as u64)
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

    pub fn get_message(&self, stable_message_id: &str) -> Result<Option<MessageV1>, StoreError> {
        let payload = self
            .connection
            .query_row(
                "SELECT payload_json FROM messages WHERE stable_message_id = ?1",
                [stable_message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload
            .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
            .transpose()
    }

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

    pub fn list_conversations(
        &self,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<ConversationListItem>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT
                c.conversation_id,
                c.display_name,
                c.conversation_type,
                c.last_message_at,
                (SELECT count(*) FROM messages m WHERE m.conversation_id = c.conversation_id),
                (SELECT count(DISTINCT mm.media_id)
                 FROM messages m JOIN message_media mm ON mm.stable_message_id = m.stable_message_id
                 WHERE m.conversation_id = c.conversation_id),
                (SELECT count(*) FROM conversation_participants cp
                 WHERE cp.conversation_id = c.conversation_id)
             FROM conversations c
             ORDER BY c.last_message_at DESC, c.conversation_id ASC
             LIMIT ?1 OFFSET ?2",
        )?;
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

fn table_count(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    let sql = match table {
        "conversations" => "SELECT count(*) FROM conversations",
        "messages" => "SELECT count(*) FROM messages",
        "media" => "SELECT count(*) FROM media",
        "audit_events" => "SELECT count(*) FROM audit_events",
        _ => return Err(StoreError::Invalid("invalid count target".into())),
    };
    Ok(connection.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64)
}

fn merge_message(
    transaction: &Transaction<'_>,
    message: &MessageV1,
    summary: &mut IngestSummary,
) -> Result<(), StoreError> {
    let payload_json = serde_json::to_string(message)?;
    let mut content_message = message.clone();
    content_message.collection_batch_id = uuid::Uuid::nil();
    let content_json = serde_json::to_vec(&content_message)?;
    let content_hash = hex::encode(Sha256::digest(&content_json));
    let duplicate: Option<String> = transaction
        .query_row(
            "SELECT payload_json
             FROM messages
             WHERE conversation_id = ?1
               AND source_message_id = ?2
               AND sender_id IS ?3
               AND sent_at = ?4
             LIMIT 1",
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
        return Ok(());
    }
    let existing: Option<String> = transaction
        .query_row(
            "SELECT content_sha256 FROM messages WHERE stable_message_id = ?1",
            [message.stable_message_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;

    match existing {
        None => {
            transaction.execute(
                "INSERT OR IGNORE INTO conversations (conversation_id, last_message_at)
                 VALUES (?1, ?2)",
                params![message.conversation_id, message.sent_at.to_rfc3339()],
            )?;
            transaction.execute(
                "UPDATE conversations SET last_message_at = max(last_message_at, ?2)
                 WHERE conversation_id = ?1",
                params![message.conversation_id, message.sent_at.to_rfc3339()],
            )?;
            if let Some(sender_id) = &message.sender_id {
                transaction.execute(
                    "INSERT OR IGNORE INTO participants (participant_id) VALUES (?1)",
                    [sender_id],
                )?;
                transaction.execute(
                    "INSERT OR IGNORE INTO conversation_participants (conversation_id, participant_id)
                     VALUES (?1, ?2)",
                    params![message.conversation_id, sender_id],
                )?;
            }
            transaction.execute(
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
            transaction.execute(
                "INSERT INTO message_fts (stable_message_id, body_text) VALUES (?1, ?2)",
                params![message.stable_message_id, message.body_text],
            )?;
            summary.inserted += 1;
        }
        Some(previous_hash) if previous_hash == content_hash => {
            transaction.execute(
                "UPDATE messages SET last_batch_id = ?2 WHERE stable_message_id = ?1",
                params![
                    message.stable_message_id,
                    message.collection_batch_id.to_string()
                ],
            )?;
            summary.unchanged += 1;
        }
        Some(previous_hash) => {
            let previous_payload: String = transaction.query_row(
                "SELECT payload_json FROM messages WHERE stable_message_id = ?1",
                [message.stable_message_id.as_str()],
                |row| row.get(0),
            )?;
            transaction.execute(
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
            transaction.execute(
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
            transaction.execute(
                "DELETE FROM message_fts WHERE stable_message_id = ?1",
                [message.stable_message_id.as_str()],
            )?;
            transaction.execute(
                "INSERT INTO message_fts (stable_message_id, body_text) VALUES (?1, ?2)",
                params![message.stable_message_id, message.body_text],
            )?;
            summary.revised += 1;
        }
    }

    transaction.execute(
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
        transaction.execute(
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
        transaction.execute(
            "INSERT INTO message_media (stable_message_id, media_id, ordinal)
             VALUES (?1, ?2, ?3)",
            params![message.stable_message_id, media_id, ordinal as i64],
        )?;
    }
    Ok(())
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

fn enum_text<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_value(value).and_then(|value| match value {
        serde_json::Value::String(text) => Ok(text),
        _ => serde_json::to_string(&value),
    })
}

fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS archives (
    archive_id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    retention_policy_id TEXT,
    retention_expires_at TEXT,
    legal_hold INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS data_sources (
    source_instance_id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    client_version TEXT,
    last_cursor TEXT,
    last_success_at TEXT
);
CREATE TABLE IF NOT EXISTS collection_batches (
    batch_id TEXT PRIMARY KEY,
    source_adapter TEXT NOT NULL,
    source_instance_id TEXT NOT NULL,
    collected_at TEXT NOT NULL,
    cursor_before TEXT,
    cursor_after TEXT,
    content_sha256 TEXT NOT NULL,
    employee_notice_json TEXT NOT NULL,
    external_consent TEXT NOT NULL,
    collection_scope_json TEXT NOT NULL,
    retention_json TEXT NOT NULL,
    status TEXT NOT NULL,
    message_count INTEGER NOT NULL DEFAULT 0,
    media_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS conversations (
    conversation_id TEXT PRIMARY KEY,
    display_name TEXT,
    conversation_type TEXT,
    last_message_at TEXT NOT NULL,
    retention_policy_id TEXT,
    retention_expires_at TEXT,
    legal_hold INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS participants (
    participant_id TEXT PRIMARY KEY,
    display_name TEXT,
    participant_kind TEXT,
    metadata_json TEXT
);
CREATE TABLE IF NOT EXISTS conversation_participants (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
    participant_id TEXT NOT NULL REFERENCES participants(participant_id),
    PRIMARY KEY (conversation_id, participant_id)
);
CREATE TABLE IF NOT EXISTS messages (
    stable_message_id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    source_instance_id TEXT NOT NULL,
    source_message_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
    sender_id TEXT,
    sent_at TEXT NOT NULL,
    direction TEXT NOT NULL,
    message_type TEXT NOT NULL,
    body_text TEXT,
    quoted_message_id TEXT,
    lifecycle TEXT NOT NULL,
    raw_type TEXT NOT NULL,
    parser_version TEXT NOT NULL,
    first_batch_id TEXT NOT NULL,
    last_batch_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    retention_expires_at TEXT,
    legal_hold INTEGER NOT NULL DEFAULT 0,
    UNIQUE(source_instance_id, source_message_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_time
ON messages(conversation_id, sent_at, stable_message_id);
CREATE INDEX IF NOT EXISTS idx_messages_sender_time ON messages(sender_id, sent_at);
CREATE INDEX IF NOT EXISTS idx_messages_type_time ON messages(message_type, sent_at);
CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
    stable_message_id UNINDEXED,
    body_text,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TABLE IF NOT EXISTS message_revisions (
    revision_id INTEGER PRIMARY KEY AUTOINCREMENT,
    stable_message_id TEXT NOT NULL REFERENCES messages(stable_message_id),
    observed_batch_id TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE(stable_message_id, content_sha256)
);
CREATE TABLE IF NOT EXISTS media (
    media_id TEXT PRIMARY KEY,
    content_sha256 TEXT UNIQUE,
    original_name TEXT,
    mime_type TEXT,
    size_bytes INTEGER,
    archive_relative_path TEXT,
    source_locator TEXT NOT NULL,
    integrity TEXT NOT NULL,
    missing_reason TEXT,
    first_batch_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS message_media (
    stable_message_id TEXT NOT NULL REFERENCES messages(stable_message_id),
    media_id TEXT NOT NULL REFERENCES media(media_id),
    ordinal INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (stable_message_id, ordinal)
);
CREATE TABLE IF NOT EXISTS export_jobs (
    export_id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    finished_at TEXT,
    scope TEXT NOT NULL,
    format TEXT NOT NULL,
    target_fingerprint TEXT NOT NULL,
    status TEXT NOT NULL,
    message_count INTEGER,
    media_count INTEGER,
    manifest_sha256 TEXT,
    error_code TEXT
);
CREATE TABLE IF NOT EXISTS audit_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT UNIQUE NOT NULL,
    occurred_at TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    subject_id TEXT,
    detail_json TEXT NOT NULL,
    previous_event_hash TEXT,
    event_hash TEXT
);
"#;

const MIGRATION_V2: &str = r#"
ALTER TABLE message_media RENAME TO message_media_v1;
ALTER TABLE media RENAME TO media_v1;
CREATE TABLE media (
    media_id TEXT PRIMARY KEY,
    content_sha256 TEXT UNIQUE,
    original_name TEXT,
    mime_type TEXT,
    size_bytes INTEGER,
    archive_relative_path TEXT,
    source_locator TEXT NOT NULL,
    integrity TEXT NOT NULL,
    missing_reason TEXT,
    first_batch_id TEXT NOT NULL
);
CREATE TABLE message_media (
    stable_message_id TEXT NOT NULL REFERENCES messages(stable_message_id),
    media_id TEXT NOT NULL REFERENCES media(media_id),
    ordinal INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (stable_message_id, ordinal)
);
INSERT INTO media (
    media_id, content_sha256, original_name, mime_type, size_bytes,
    archive_relative_path, source_locator, integrity, missing_reason, first_batch_id
)
SELECT
    'sha256:' || content_sha256, content_sha256, original_name, mime_type, size_bytes,
    archive_relative_path, source_locator, integrity, missing_reason, first_batch_id
FROM media_v1;
INSERT INTO message_media (stable_message_id, media_id, ordinal)
SELECT stable_message_id, 'sha256:' || content_sha256,
       row_number() OVER (PARTITION BY stable_message_id ORDER BY content_sha256) - 1
FROM message_media_v1;
DROP TABLE message_media_v1;
DROP TABLE media_v1;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{
        BATCH_SCHEMA_VERSION, CollectionScope, EmployeeNoticeEvidence, ExternalContactConsent,
        LifecycleState, MESSAGE_SCHEMA_VERSION, MediaIntegrity, MediaRefV1, MessageDirection,
        RetentionDirective,
    };
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn batch(body: &str) -> ArchiveBatchV1 {
        let batch_id = Uuid::new_v4();
        let message_time = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        ArchiveBatchV1 {
            schema_version: BATCH_SCHEMA_VERSION.into(),
            batch_id,
            source_adapter: "fixture".into(),
            source_instance_id: "source".into(),
            collected_at: Utc::now(),
            employee_notice: EmployeeNoticeEvidence {
                notice_version: "test".into(),
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
            cursor_after: Some("1".into()),
            messages: vec![MessageV1 {
                schema_version: MESSAGE_SCHEMA_VERSION.into(),
                source_kind: "fixture".into(),
                source_instance_id: "source".into(),
                source_message_id: "one".into(),
                stable_message_id: "stable-one".into(),
                conversation_id: "conversation".into(),
                sender_id: Some("participant".into()),
                sent_at: message_time,
                direction: MessageDirection::Incoming,
                message_type: MessageType::Text,
                body_text: Some(body.into()),
                quoted_message_id: None,
                lifecycle: LifecycleState::Active,
                media: vec![],
                raw_type: "text".into(),
                raw_payload: json!({"text": body}),
                parser_version: "fixture".into(),
                collection_batch_id: batch_id,
            }],
            content_sha256: "fixture".into(),
            retention: RetentionDirective {
                policy_id: None,
                expires_at: None,
                legal_hold: false,
            },
        }
    }

    #[test]
    fn ingest_is_idempotent_and_records_revision() {
        let directory = tempdir().unwrap();
        let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();

        let first = store.ingest_batch(&batch("第一版")).unwrap();
        assert_eq!(first.inserted, 1);
        let second = store.ingest_batch(&batch("第一版")).unwrap();
        assert_eq!(second.unchanged, 1);
        let third = store.ingest_batch(&batch("第二版")).unwrap();
        assert_eq!(third.revised, 1);
        assert_eq!(store.message_count().unwrap(), 1);
        assert_eq!(store.summary().unwrap().revision, 3);
    }

    #[test]
    fn merges_same_message_from_different_collectors() {
        let directory = tempdir().unwrap();
        let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
        let first = batch("同一条消息");
        store.ingest_batch(&first).unwrap();
        let mut second = batch("同一条消息");
        second.batch_id = Uuid::new_v4();
        second.source_instance_id = "collector-2".into();
        second.collection_scope.source_ids = vec!["collector-2".into()];
        second.messages[0].collection_batch_id = second.batch_id;
        second.messages[0].source_instance_id = "collector-2".into();
        second.messages[0].stable_message_id = "stable-one-from-collector-2".into();
        assert_eq!(store.ingest_batch(&second).unwrap().unchanged, 1);
        assert_eq!(store.message_count().unwrap(), 1);
    }

    #[test]
    fn missing_media_metadata_is_retained_and_counted() {
        let directory = tempdir().unwrap();
        let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
        let mut input = batch("附件");
        input.messages[0].media.push(MediaRefV1 {
            content_hash: None,
            original_name: Some("附件.pdf".into()),
            mime_type: Some("application/pdf".into()),
            size_bytes: Some(128),
            source_locator: "unavailable".into(),
            integrity: MediaIntegrity::Missing,
            missing_reason: Some("metadata_only".into()),
        });
        store.ingest_batch(&input).unwrap();
        assert_eq!(store.summary().unwrap().media_count, 1);
        assert_eq!(store.list_conversations(10, 0).unwrap()[0].media_count, 1);
    }

    #[test]
    fn migrates_version_one_media_schema() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("archive.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE media (
                    content_sha256 TEXT PRIMARY KEY,
                    original_name TEXT,
                    mime_type TEXT,
                    size_bytes INTEGER,
                    archive_relative_path TEXT,
                    source_locator TEXT NOT NULL,
                    integrity TEXT NOT NULL,
                    missing_reason TEXT,
                    first_batch_id TEXT NOT NULL
                );
                CREATE TABLE message_media (
                    stable_message_id TEXT NOT NULL,
                    content_sha256 TEXT NOT NULL REFERENCES media(content_sha256),
                    ordinal INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (stable_message_id, content_sha256)
                );
                INSERT INTO media VALUES (
                    'abc', 'old.bin', NULL, 3, NULL, 'sha256:abc', 'verified', NULL, 'batch'
                );
                PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let store = ArchiveStore::open(&path).unwrap();
        assert_eq!(store.summary().unwrap().media_count, 1);
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }
}
