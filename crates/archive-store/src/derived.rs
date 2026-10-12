//! Rebuildable accelerators. Base tables and the original FTS remain authoritative.
use crate::StoreError;
use archive_domain::{ArchiveBatchV1, MessageV1};
use rusqlite::{Transaction, params};

const TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS archive_conversation_stats (
    conversation_id TEXT PRIMARY KEY,
    message_count INTEGER NOT NULL DEFAULT 0,
    media_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS archive_conversation_media_refs (
    conversation_id TEXT NOT NULL,
    media_id TEXT NOT NULL,
    ref_count INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, media_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS archive_fts_rows (
    fts_rowid INTEGER PRIMARY KEY,
    stable_message_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_archive_fts_rows_message
ON archive_fts_rows(stable_message_id);
"#;

// Triggers also run for older writers that do not know about the derived tables.
// Never depend on FTS shadow tables or on messages.rowid matching FTS rowid.
const TRIGGERS: &str = r#"
CREATE TRIGGER IF NOT EXISTS archive_stats_message_insert AFTER INSERT ON messages
BEGIN
    INSERT INTO archive_conversation_stats(conversation_id, message_count)
    VALUES (NEW.conversation_id, 1)
    ON CONFLICT(conversation_id) DO UPDATE SET message_count = message_count + 1;
END;
CREATE TRIGGER IF NOT EXISTS archive_stats_message_delete AFTER DELETE ON messages
BEGIN
    UPDATE archive_conversation_stats SET message_count = message_count - 1
    WHERE conversation_id = OLD.conversation_id;
END;
CREATE TRIGGER IF NOT EXISTS archive_stats_media_insert AFTER INSERT ON message_media
BEGIN
    INSERT INTO archive_conversation_media_refs(conversation_id, media_id, ref_count)
    SELECT conversation_id, NEW.media_id, 1 FROM messages
    WHERE stable_message_id = NEW.stable_message_id
    ON CONFLICT(conversation_id, media_id) DO UPDATE SET ref_count = ref_count + 1;
    UPDATE archive_conversation_stats SET media_count = media_count + 1
    WHERE conversation_id = (SELECT conversation_id FROM messages
                              WHERE stable_message_id = NEW.stable_message_id)
      AND (SELECT ref_count FROM archive_conversation_media_refs
           WHERE conversation_id = archive_conversation_stats.conversation_id
             AND media_id = NEW.media_id) = 1;
END;
CREATE TRIGGER IF NOT EXISTS archive_stats_media_delete AFTER DELETE ON message_media
BEGIN
    UPDATE archive_conversation_media_refs SET ref_count = ref_count - 1
    WHERE conversation_id = (SELECT conversation_id FROM messages
                              WHERE stable_message_id = OLD.stable_message_id)
      AND media_id = OLD.media_id;
    UPDATE archive_conversation_stats SET media_count = media_count - 1
    WHERE conversation_id = (SELECT conversation_id FROM messages
                              WHERE stable_message_id = OLD.stable_message_id)
      AND (SELECT ref_count FROM archive_conversation_media_refs
           WHERE conversation_id = archive_conversation_stats.conversation_id
             AND media_id = OLD.media_id) = 0;
    DELETE FROM archive_conversation_media_refs
    WHERE conversation_id = (SELECT conversation_id FROM messages
                              WHERE stable_message_id = OLD.stable_message_id)
      AND media_id = OLD.media_id AND ref_count = 0;
END;
CREATE TRIGGER IF NOT EXISTS archive_stats_message_move
AFTER UPDATE OF conversation_id ON messages
WHEN OLD.conversation_id <> NEW.conversation_id
BEGIN
    UPDATE archive_conversation_stats SET message_count = message_count - 1
    WHERE conversation_id = OLD.conversation_id;
    INSERT INTO archive_conversation_stats(conversation_id, message_count)
    VALUES (NEW.conversation_id, 1)
    ON CONFLICT(conversation_id) DO UPDATE SET message_count = message_count + 1;
    UPDATE archive_conversation_media_refs SET ref_count = ref_count - (
        SELECT count(*) FROM message_media
        WHERE stable_message_id = NEW.stable_message_id
          AND media_id = archive_conversation_media_refs.media_id
    ) WHERE conversation_id = OLD.conversation_id AND media_id IN (
        SELECT media_id FROM message_media WHERE stable_message_id = NEW.stable_message_id
    );
    DELETE FROM archive_conversation_media_refs
    WHERE conversation_id = OLD.conversation_id AND ref_count = 0;
    INSERT INTO archive_conversation_media_refs(conversation_id, media_id, ref_count)
    SELECT NEW.conversation_id, media_id, count(*) FROM message_media
    WHERE stable_message_id = NEW.stable_message_id GROUP BY media_id
    ON CONFLICT(conversation_id, media_id) DO UPDATE SET ref_count = ref_count + excluded.ref_count;
    UPDATE archive_conversation_stats SET media_count = (
        SELECT count(*) FROM archive_conversation_media_refs
        WHERE conversation_id = archive_conversation_stats.conversation_id
    ) WHERE conversation_id IN (OLD.conversation_id, NEW.conversation_id);
END;
CREATE TRIGGER IF NOT EXISTS archive_fts_invalidate_insert AFTER INSERT ON messages
BEGIN
    DELETE FROM archive_fts_rows WHERE stable_message_id = NEW.stable_message_id;
END;
CREATE TRIGGER IF NOT EXISTS archive_fts_invalidate_update
AFTER UPDATE OF body_text, payload_json, lifecycle ON messages
BEGIN
    DELETE FROM archive_fts_rows WHERE stable_message_id = OLD.stable_message_id;
END;
CREATE TRIGGER IF NOT EXISTS archive_fts_invalidate_delete AFTER DELETE ON messages
BEGIN
    DELETE FROM archive_fts_rows WHERE stable_message_id = OLD.stable_message_id;
END;
"#;

pub(super) fn initialize(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let has_stats: bool = transaction.query_row(
        "SELECT count(*) = 2 FROM sqlite_schema WHERE type = 'table'
         AND name IN ('archive_conversation_stats', 'archive_conversation_media_refs')",
        [],
        |row| row.get(0),
    )?;
    let has_fts_rows: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'archive_fts_rows')",
        [],
        |row| row.get(0),
    )?;
    transaction.execute_batch(TABLES)?;
    if !has_stats {
        transaction.execute_batch(
            "DELETE FROM archive_conversation_media_refs;
             DELETE FROM archive_conversation_stats;
             INSERT INTO archive_conversation_media_refs(conversation_id, media_id, ref_count)
             SELECT m.conversation_id, mm.media_id, count(*)
             FROM message_media mm JOIN messages m ON m.stable_message_id = mm.stable_message_id
             GROUP BY m.conversation_id, mm.media_id;
             INSERT INTO archive_conversation_stats(conversation_id, message_count, media_count)
             SELECT c.conversation_id, coalesce(mc.message_count, 0), coalesce(rc.media_count, 0)
             FROM conversations c
             LEFT JOIN (SELECT conversation_id, count(*) AS message_count FROM messages
                        GROUP BY conversation_id) mc USING (conversation_id)
             LEFT JOIN (SELECT conversation_id, count(*) AS media_count
                        FROM archive_conversation_media_refs GROUP BY conversation_id) rc
             USING (conversation_id);",
        )?;
    }
    if !has_fts_rows {
        transaction.execute(
            "INSERT INTO archive_fts_rows(fts_rowid, stable_message_id)
             SELECT f.rowid, f.stable_message_id FROM message_fts f
             WHERE EXISTS(SELECT 1 FROM messages m WHERE m.stable_message_id = f.stable_message_id)",
            [],
        )?;
    }
    transaction.execute_batch(TRIGGERS)?;
    Ok(())
}

/// Repair entries invalidated by an older writer with at most one FTS scan per
/// batch. Only existing messages in this batch are staged, keeping memory bounded.
pub(super) fn repair_fts_rows(
    transaction: &Transaction<'_>,
    batch: &ArchiveBatchV1,
) -> Result<(), StoreError> {
    transaction.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS archive_fts_pending(stable_message_id TEXT PRIMARY KEY);
         DELETE FROM archive_fts_pending;",
    )?;
    let mut statement = transaction.prepare_cached(
        "INSERT OR IGNORE INTO archive_fts_pending(stable_message_id)
         SELECT ?1 WHERE EXISTS(SELECT 1 FROM messages WHERE stable_message_id = ?1)
         AND NOT EXISTS(SELECT 1 FROM archive_fts_rows WHERE stable_message_id = ?1)",
    )?;
    for message in &batch.messages {
        statement.execute([message.stable_message_id.as_str()])?;
    }
    let has_pending: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM archive_fts_pending)",
        [],
        |row| row.get(0),
    )?;
    if has_pending {
        transaction.execute(
            "INSERT INTO archive_fts_rows(fts_rowid, stable_message_id)
             SELECT rowid, stable_message_id FROM message_fts
             WHERE stable_message_id IN (SELECT stable_message_id FROM archive_fts_pending)",
            [],
        )?;
    }
    Ok(())
}

pub(super) fn insert_fts(
    transaction: &Transaction<'_>,
    message: &MessageV1,
) -> Result<(), StoreError> {
    transaction
        .prepare_cached("INSERT INTO message_fts(stable_message_id, body_text) VALUES (?1, ?2)")?
        .execute(params![message.stable_message_id, message.body_text])?;
    let fts_rowid = transaction.last_insert_rowid();
    transaction
        .prepare_cached(
            "INSERT INTO archive_fts_rows(fts_rowid, stable_message_id) VALUES (?1, ?2)",
        )?
        .execute(params![fts_rowid, message.stable_message_id])?;
    Ok(())
}
