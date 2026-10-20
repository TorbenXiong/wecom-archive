use archive_domain::MessageType;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    #[serde(default)]
    pub sort_ascending: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<std::collections::BTreeMap<String, String>>,
}

fn participant_profile(value: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    const FIELDS: &[&str] = &[
        "mobile",
        "phone",
        "office_phone",
        "email",
        "employee_no",
        "department",
        "organization",
        "position",
        "address",
        "english_name",
        "signature",
    ];
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| {
            FIELDS.contains(&key.as_str())
                || (key.starts_with("custom:")
                    && key.len() <= 135
                    && !key.chars().any(char::is_control))
        })
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty() && text.len() <= 2048)
                .map(|text| (key.clone(), text.into()))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectedUser {
    pub source_instance_id: String,
    pub display_name: Option<String>,
}

pub struct ArchiveStore {
    connection: Connection,
}

/// Keeps metadata and streamed messages in one SQLite read snapshot. Dropping
/// the guard ends the read transaction without changing archive data.
pub struct ArchiveReadSnapshot<'a> {
    _transaction: rusqlite::Transaction<'a>,
}

mod connection;
mod derived;
mod ingest;
mod migration;
mod query;
mod revision;
mod schema;
mod sql;

#[cfg(test)]
mod tests;
