use crate::{ArchiveStore, StoreError};
use serde::Serialize;

/// Aggregate metadata only: never return source identifiers, names or payloads.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveHealth {
    pub revision: u64,
    pub message_count: u64,
    pub conversation_count: u64,
    pub batch_count: u64,
    pub source_count: u64,
    pub first_message_at: Option<String>,
    pub last_message_at: Option<String>,
    pub last_collected_at: Option<String>,
    pub unsupported_messages: u64,
    pub verified_media: u64,
    pub missing_media: u64,
    pub invalid_media: u64,
    pub restricted_batches: u64,
    pub missing_locator: u64,
    pub missing_in_source: u64,
    pub missing_other: u64,
}

impl ArchiveStore {
    pub fn archive_health(&self) -> Result<ArchiveHealth, StoreError> {
        let summary = self.summary()?;
        let (first_message_at, last_message_at, unsupported_messages) = self.connection.query_row(
            "SELECT min(sent_at), max(sent_at), coalesce(sum(message_type = 'unsupported'), 0) FROM messages",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64)),
        )?;
        let (verified_media, missing_media, invalid_media) = self.connection.query_row(
            "SELECT coalesce(sum(integrity = 'verified'), 0), coalesce(sum(integrity = 'missing'), 0), coalesce(sum(integrity NOT IN ('verified', 'missing')), 0) FROM media",
            [], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64)),
        )?;
        let (batch_count, source_count, last_collected_at, restricted_batches) = self.connection.query_row(
            "SELECT count(*), count(DISTINCT source_instance_id), max(collected_at), coalesce(sum(json_extract(collection_scope_json, '$.starts_at') IS NOT NULL OR json_extract(collection_scope_json, '$.ends_at') IS NOT NULL OR coalesce(json_array_length(collection_scope_json, '$.conversation_ids'), 0) > 0 OR coalesce(json_array_length(collection_scope_json, '$.message_types'), 0) > 0), 0) FROM collection_batches",
            [], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get(2)?, r.get::<_, i64>(3)? as u64)),
        )?;
        let (missing_locator, missing_in_source) = self.connection.query_row(
            "SELECT coalesce(sum(missing_reason IN ('media_locator_not_present_in_message_payload', 'not_resolved')), 0), coalesce(sum(missing_reason = 'not_found_in_authorized_media_roots'), 0) FROM media WHERE integrity = 'missing'",
            [], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        Ok(ArchiveHealth {
            revision: summary.revision,
            message_count: summary.message_count,
            conversation_count: summary.conversation_count,
            batch_count,
            source_count,
            first_message_at,
            last_message_at,
            last_collected_at,
            unsupported_messages,
            verified_media,
            missing_media,
            invalid_media,
            restricted_batches,
            missing_locator,
            missing_in_source,
            missing_other: missing_media.saturating_sub(missing_locator + missing_in_source),
        })
    }
}
