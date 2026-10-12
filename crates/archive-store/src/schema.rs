pub(super) const SCHEMA_VERSION: i64 = 2;

// Additive indexes also apply to existing v2 archives. They change no fields
// or stored values, and older versions can continue to read the database.
pub(super) const PERFORMANCE_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_messages_time
ON messages(sent_at, stable_message_id);
CREATE INDEX IF NOT EXISTS idx_conversations_recent
ON conversations(last_message_at DESC, conversation_id ASC);
"#;

pub(super) const SCHEMA: &str = r#"
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

pub(super) const MIGRATION_V2: &str = r#"
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
