use archive_domain::{
    LifecycleState, MESSAGE_SCHEMA_VERSION, MediaIntegrity, MediaRefV1, MessageDirection,
    MessageType, MessageV1,
};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const PARSER_VERSION: &str = "windows-message-parser/0.1.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawMessageRow {
    pub source_instance_id: String,
    pub source_message_id: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub sent_at_unix_ms: i64,
    pub outgoing: Option<bool>,
    pub raw_type: String,
    pub payload: Value,
    pub recalled: bool,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("timestamp is outside the supported range")]
    InvalidTimestamp,
    #[error("message source identifier is empty")]
    MissingSourceId,
}

pub fn normalize(row: RawMessageRow, batch_id: Uuid) -> Result<MessageV1, ParseError> {
    if row.source_instance_id.trim().is_empty() || row.source_message_id.trim().is_empty() {
        return Err(ParseError::MissingSourceId);
    }

    let sent_at = timestamp(row.sent_at_unix_ms)?;
    let message_type = classify(&row.raw_type, &row.payload);
    let direction = match row.outgoing {
        Some(true) => MessageDirection::Outgoing,
        Some(false) => MessageDirection::Incoming,
        None if message_type == MessageType::System => MessageDirection::System,
        None => MessageDirection::Unknown,
    };

    let body_text = extract_text(message_type.clone(), &row.payload);
    let quoted_message_id = row
        .payload
        .get("quote_id")
        .or_else(|| row.payload.get("reply_to"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let media = extract_media(&row.payload);

    Ok(MessageV1 {
        schema_version: MESSAGE_SCHEMA_VERSION.into(),
        source_kind: "windows_local".into(),
        stable_message_id: stable_id(&row.source_instance_id, &row.source_message_id),
        source_instance_id: row.source_instance_id,
        source_message_id: row.source_message_id,
        conversation_id: row.conversation_id,
        sender_id: row.sender_id,
        sent_at,
        direction,
        message_type,
        body_text,
        quoted_message_id,
        lifecycle: if row.recalled {
            LifecycleState::Recalled
        } else {
            LifecycleState::Active
        },
        media,
        raw_type: row.raw_type,
        raw_payload: row.payload,
        parser_version: PARSER_VERSION.into(),
        collection_batch_id: batch_id,
    })
}

fn timestamp(unix_ms: i64) -> Result<DateTime<Utc>, ParseError> {
    Utc.timestamp_millis_opt(unix_ms)
        .single()
        .ok_or(ParseError::InvalidTimestamp)
}

fn stable_id(source: &str, source_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"message.v1\0");
    hasher.update(source.as_bytes());
    hasher.update(b"\0");
    hasher.update(source_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn classify(raw_type: &str, payload: &Value) -> MessageType {
    let normalized = raw_type.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "0" | "1" | "2" | "text" => MessageType::Text,
        "3" | "4" | "14" | "29" | "image" | "img" => MessageType::Image,
        "34" | "voice" | "audio" => MessageType::Audio,
        "43" | "video" => MessageType::Video,
        "15" | "16" | "49:file" | "file" | "attachment" => MessageType::File,
        "13" | "49:link" | "link" | "url" => MessageType::Link,
        "49:quote" | "reply" | "quote" => MessageType::Reply,
        "38" | "40" | "501" | "503" | "671" | "10000" | "1001" | "1002" | "1004" | "1006"
        | "1017" | "1018" | "1022" | "1051" | "1052" | "1055" | "1073" | "system" | "event" => {
            MessageType::System
        }
        "49" => match payload.get("kind").and_then(Value::as_str) {
            Some("file") => MessageType::File,
            Some("link") => MessageType::Link,
            Some("quote") | Some("reply") => MessageType::Reply,
            _ => MessageType::Unsupported,
        },
        _ => MessageType::Unsupported,
    }
}

fn extract_text(message_type: MessageType, payload: &Value) -> Option<String> {
    let keys: &[&str] = match message_type {
        MessageType::Text | MessageType::Reply | MessageType::System | MessageType::Unsupported => {
            &["text", "content", "title", "description"]
        }
        MessageType::Link => &["title", "description", "url"],
        MessageType::File => &["name", "filename"],
        _ => &[],
    };

    keys.iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_str))
        .find(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn extract_media(payload: &Value) -> Vec<MediaRefV1> {
    let Some(locator) = payload
        .get("media_path")
        .or_else(|| payload.get("file_path"))
        .and_then(Value::as_str)
    else {
        return vec![];
    };

    vec![MediaRefV1 {
        content_hash: None,
        original_name: payload
            .get("name")
            .or_else(|| payload.get("filename"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        mime_type: payload
            .get("mime")
            .and_then(Value::as_str)
            .map(str::to_owned),
        size_bytes: payload.get("size").and_then(Value::as_u64),
        source_locator: locator.to_owned(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("not_resolved".into()),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(raw_type: &str, payload: Value) -> RawMessageRow {
        RawMessageRow {
            source_instance_id: "fixture".into(),
            source_message_id: "42".into(),
            conversation_id: "conversation".into(),
            sender_id: Some("participant".into()),
            sent_at_unix_ms: 1_700_000_000_000,
            outgoing: Some(false),
            raw_type: raw_type.into(),
            payload,
            recalled: false,
        }
    }

    #[test]
    fn parses_text_without_losing_raw_payload() {
        let payload = json!({"text": "你好", "extra": {"opaque": true}});
        let message = normalize(row("text", payload.clone()), Uuid::nil()).unwrap();
        assert_eq!(message.message_type, MessageType::Text);
        assert_eq!(message.body_text.as_deref(), Some("你好"));
        assert_eq!(message.raw_payload, payload);
    }

    #[test]
    fn keeps_unknown_type_as_unsupported() {
        let payload = json!({"opaque": [1, 2, 3]});
        let message = normalize(row("future-type", payload.clone()), Uuid::nil()).unwrap();
        assert_eq!(message.message_type, MessageType::Unsupported);
        assert_eq!(message.raw_payload, payload);
    }
}
