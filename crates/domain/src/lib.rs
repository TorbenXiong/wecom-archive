use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const MESSAGE_SCHEMA_VERSION: &str = "message.v1";
pub const BATCH_SCHEMA_VERSION: &str = "archive-batch.v1";
pub const UPLOAD_SCHEMA_VERSION: &str = "upload-envelope.v1";
pub const CLIENT_EXPORT_SCHEMA_VERSION: &str = "client-export.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    Incoming,
    Outgoing,
    System,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Text,
    Image,
    Audio,
    Video,
    File,
    Link,
    Reply,
    System,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Active,
    Recalled,
    DeletedAtSource,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaIntegrity {
    Verified,
    Missing,
    SizeMismatch,
    HashMismatch,
    Unreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaRefV1 {
    pub content_hash: Option<String>,
    pub original_name: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub source_locator: String,
    pub integrity: MediaIntegrity,
    pub missing_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageV1 {
    pub schema_version: String,
    pub source_kind: String,
    pub source_instance_id: String,
    pub source_message_id: String,
    pub stable_message_id: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub sent_at: DateTime<Utc>,
    pub direction: MessageDirection,
    pub message_type: MessageType,
    pub body_text: Option<String>,
    pub quoted_message_id: Option<String>,
    pub lifecycle: LifecycleState,
    pub media: Vec<MediaRefV1>,
    pub raw_type: String,
    pub raw_payload: Value,
    pub parser_version: String,
    pub collection_batch_id: Uuid,
}

impl MessageV1 {
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.schema_version != MESSAGE_SCHEMA_VERSION {
            return Err(DomainError::UnsupportedSchema(self.schema_version.clone()));
        }
        for (name, value) in [
            ("source_kind", self.source_kind.as_str()),
            ("source_instance_id", self.source_instance_id.as_str()),
            ("source_message_id", self.source_message_id.as_str()),
            ("stable_message_id", self.stable_message_id.as_str()),
            ("conversation_id", self.conversation_id.as_str()),
            ("raw_type", self.raw_type.as_str()),
            ("parser_version", self.parser_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(DomainError::MissingField(name));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmployeeNoticeEvidence {
    pub notice_version: String,
    pub displayed_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub evidence_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContactConsent {
    Granted,
    Denied,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionScope {
    pub source_ids: Vec<String>,
    pub conversation_ids: Vec<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub message_types: Vec<MessageType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionDirective {
    pub policy_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub legal_hold: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchiveBatchV1 {
    pub schema_version: String,
    pub batch_id: Uuid,
    pub source_adapter: String,
    pub source_instance_id: String,
    pub collected_at: DateTime<Utc>,
    pub employee_notice: EmployeeNoticeEvidence,
    pub external_contact_consent: ExternalContactConsent,
    pub collection_scope: CollectionScope,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
    pub messages: Vec<MessageV1>,
    pub content_sha256: String,
    pub retention: RetentionDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadChunkV1 {
    pub index: u32,
    pub offset: u64,
    pub plaintext_size: u64,
    pub ciphertext_sha256: String,
    pub nonce_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadEnvelopeV1 {
    pub schema_version: String,
    pub envelope_id: Uuid,
    pub batch_id: Uuid,
    pub idempotency_key: String,
    pub encryption: String,
    pub wrapped_dek_base64: String,
    pub key_id: String,
    pub chunks: Vec<UploadChunkV1>,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientExportV1 {
    pub schema_version: String,
    pub export_id: Uuid,
    pub generated_at: DateTime<Utc>,
    pub client_version: String,
    pub media_transport: String,
    #[serde(default)]
    pub media_blobs: Vec<MediaBlobV1>,
    pub conversations: Vec<ConversationV1>,
    pub participants: Vec<ParticipantV1>,
    pub batches: Vec<ArchiveBatchV1>,
    pub message_count: u64,
    pub media_count: u64,
    pub missing_media_count: u64,
    pub content_sha256: String,
}

impl ClientExportV1 {
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.schema_version != CLIENT_EXPORT_SCHEMA_VERSION {
            return Err(DomainError::UnsupportedSchema(self.schema_version.clone()));
        }
        if self.client_version.trim().is_empty() {
            return Err(DomainError::MissingField("client_version"));
        }
        if !matches!(
            self.media_transport.as_str(),
            "metadata_only" | "embedded_hex_v1"
        ) {
            return Err(DomainError::UnsupportedMediaTransport(
                self.media_transport.clone(),
            ));
        }
        if self.media_transport == "metadata_only" && !self.media_blobs.is_empty() {
            return Err(DomainError::InvalidRelationship(
                "metadata-only export cannot carry media blobs",
            ));
        }
        let mut media_blob_ids = std::collections::BTreeSet::new();
        for blob in &self.media_blobs {
            if blob.content_hash.trim().is_empty() || blob.content_hex.trim().is_empty() {
                return Err(DomainError::MissingField("media_blob"));
            }
            if !media_blob_ids.insert(blob.content_hash.as_str()) {
                return Err(DomainError::DuplicateIdentifier("media_blob.content_hash"));
            }
        }
        let conversation_ids = self
            .conversations
            .iter()
            .map(|conversation| conversation.conversation_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if conversation_ids.len() != self.conversations.len() {
            return Err(DomainError::DuplicateIdentifier("conversation_id"));
        }
        let participant_ids = self
            .participants
            .iter()
            .map(|participant| participant.participant_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if participant_ids.len() != self.participants.len() {
            return Err(DomainError::DuplicateIdentifier("participant_id"));
        }
        let actual_messages = self
            .batches
            .iter()
            .map(|batch| batch.messages.len() as u64)
            .sum::<u64>();
        if actual_messages != self.message_count {
            return Err(DomainError::CountMismatch {
                field: "message_count",
                expected: self.message_count,
                actual: actual_messages,
            });
        }
        let actual_media = self
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .flat_map(|message| &message.media)
            .count() as u64;
        if actual_media != self.media_count {
            return Err(DomainError::CountMismatch {
                field: "media_count",
                expected: self.media_count,
                actual: actual_media,
            });
        }
        let actual_missing_media = self
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .flat_map(|message| &message.media)
            .filter(|media| media.content_hash.is_none())
            .count() as u64;
        if actual_missing_media != self.missing_media_count {
            return Err(DomainError::CountMismatch {
                field: "missing_media_count",
                expected: self.missing_media_count,
                actual: actual_missing_media,
            });
        }
        for batch in &self.batches {
            if batch.schema_version != BATCH_SCHEMA_VERSION {
                return Err(DomainError::UnsupportedSchema(batch.schema_version.clone()));
            }
            for message in &batch.messages {
                message.validate()?;
                if message.collection_batch_id != batch.batch_id {
                    return Err(DomainError::InvalidRelationship(
                        "message collection_batch_id does not match its batch",
                    ));
                }
                if !conversation_ids.contains(message.conversation_id.as_str()) {
                    return Err(DomainError::InvalidRelationship(
                        "message references a conversation missing from the directory",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationV1 {
    pub conversation_id: String,
    pub display_name: Option<String>,
    pub conversation_type: Option<String>,
    pub participant_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantV1 {
    pub participant_id: String,
    pub display_name: Option<String>,
    pub participant_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaBlobV1 {
    pub content_hash: String,
    pub original_name: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    pub content_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceCandidate {
    pub source_id: String,
    pub display_path: String,
    pub root_path: PathBuf,
    pub databases: Vec<SourceDatabase>,
    pub media_roots: Vec<PathBuf>,
    pub client_version: Option<String>,
    pub capability: SourceCapability,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceDatabase {
    pub kind: String,
    pub path: PathBuf,
    pub wal_path: Option<PathBuf>,
    pub shm_path: Option<PathBuf>,
    pub encrypted: bool,
    pub page_size_hint: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceCapability {
    Supported,
    ProbeRequired,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceCursor {
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotReceipt {
    pub run_id: Uuid,
    pub snapshot_root: PathBuf,
    pub copied_files: Vec<PathBuf>,
    pub source_fingerprint: String,
}

pub trait SourceAdapter {
    fn adapter_id(&self) -> &'static str;
    fn discover(&self, selected_root: Option<PathBuf>)
    -> Result<Vec<SourceCandidate>, DomainError>;
    fn snapshot(
        &self,
        candidate: &SourceCandidate,
        work_root: PathBuf,
    ) -> Result<SnapshotReceipt, DomainError>;
}

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    #[error("required field is empty: {0}")]
    MissingField(&'static str),
    #[error("count mismatch for {field}: expected {expected}, actual {actual}")]
    CountMismatch {
        field: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("invalid relationship: {0}")]
    InvalidRelationship(&'static str),
    #[error("duplicate identifier: {0}")]
    DuplicateIdentifier(&'static str),
    #[error("unsupported media transport: {0}")]
    UnsupportedMediaTransport(String),
    #[error("source is not stable enough to snapshot")]
    UnstableSource,
    #[error("source is not supported: {0}")]
    UnsupportedSource(String),
    #[error("access denied: {0}")]
    AccessDenied(String),
    #[error("I/O error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_stable_id() {
        let message = MessageV1 {
            schema_version: MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "windows_local".into(),
            source_instance_id: "source".into(),
            source_message_id: "1".into(),
            stable_message_id: "".into(),
            conversation_id: "conversation".into(),
            sender_id: None,
            sent_at: Utc::now(),
            direction: MessageDirection::Unknown,
            message_type: MessageType::Unsupported,
            body_text: None,
            quoted_message_id: None,
            lifecycle: LifecycleState::Unknown,
            media: vec![],
            raw_type: "unknown".into(),
            raw_payload: Value::Null,
            parser_version: "test".into(),
            collection_batch_id: Uuid::nil(),
        };

        assert!(matches!(
            message.validate(),
            Err(DomainError::MissingField("stable_message_id"))
        ));
    }
}
