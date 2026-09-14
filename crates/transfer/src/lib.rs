use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use archive_domain::{
    ArchiveBatchV1, CLIENT_EXPORT_SCHEMA_VERSION, ClientExportV1, ConversationV1, DomainError,
    ParticipantV1,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("target already exists")]
    TargetExists,
    #[error("unsafe target path")]
    UnsafeTarget,
    #[error("client export is invalid: {0}")]
    Invalid(#[from] DomainError),
    #[error("client export checksum mismatch")]
    ChecksumMismatch,
    #[error("client export I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("client export JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("client export CSV failed: {0}")]
    Csv(#[from] csv::Error),
    #[error("enterprise package is invalid")]
    InvalidEnterprisePackage,
    #[error("enterprise package authentication failed")]
    EnterpriseAuthenticationFailed,
    #[error("enterprise package organization or key mismatch")]
    EnterpriseKeyMismatch,
}

pub const ENTERPRISE_PACKAGE_SCHEMA_VERSION: &str = "enterprise-package.v1";

/// Offline authenticated package used by an enterprise-generated collector.
/// The package deliberately wraps the unchanged ClientExportV1 JSON so the
/// server can decrypt it and reuse the existing validation and ingest path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnterprisePackageV1 {
    pub schema_version: String,
    pub organization_id: String,
    pub key_id: String,
    pub export_id: Uuid,
    pub encryption: String,
    pub wrapped_dek_hex: String,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub plaintext_sha256: String,
    pub authentication_tag_hex: String,
}

pub fn create_enterprise_package(
    export: &ClientExportV1,
    organization_id: &str,
    key_id: &str,
    wrapped_dek: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
    authentication_tag: &[u8],
) -> Result<Vec<u8>, TransferError> {
    export.validate()?;
    if organization_id.trim().is_empty()
        || key_id.trim().is_empty()
        || wrapped_dek.is_empty()
        || nonce.len() != 12
        || ciphertext.is_empty()
        || authentication_tag.len() != 16
    {
        return Err(TransferError::InvalidEnterprisePackage);
    }
    let plaintext = serde_json::to_vec(export)?;
    let plaintext_sha256 = hex::encode(Sha256::digest(&plaintext));
    let package = EnterprisePackageV1 {
        schema_version: ENTERPRISE_PACKAGE_SCHEMA_VERSION.into(),
        organization_id: organization_id.into(),
        key_id: key_id.into(),
        export_id: export.export_id,
        encryption: "aes-256-gcm+rsa-oaep-sha256".into(),
        wrapped_dek_hex: hex::encode(wrapped_dek),
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(ciphertext),
        plaintext_sha256,
        authentication_tag_hex: hex::encode(authentication_tag),
    };
    Ok(serde_json::to_vec(&package)?)
}

pub fn parse_enterprise_package(
    package_bytes: &[u8],
    expected_organization_id: &str,
    expected_key_id: &str,
) -> Result<EnterprisePackageV1, TransferError> {
    let package =
        parse_enterprise_package_for_organization(package_bytes, expected_organization_id)?;
    if package.key_id != expected_key_id {
        return Err(TransferError::EnterpriseKeyMismatch);
    }
    Ok(package)
}

pub fn parse_enterprise_package_for_organization(
    package_bytes: &[u8],
    expected_organization_id: &str,
) -> Result<EnterprisePackageV1, TransferError> {
    let package: EnterprisePackageV1 = serde_json::from_slice(package_bytes)
        .map_err(|_| TransferError::InvalidEnterprisePackage)?;
    if package.schema_version != ENTERPRISE_PACKAGE_SCHEMA_VERSION
        || package.encryption != "aes-256-gcm+rsa-oaep-sha256"
        || package.organization_id != expected_organization_id
    {
        return Err(TransferError::EnterpriseKeyMismatch);
    }
    let wrapped_dek = hex::decode(&package.wrapped_dek_hex)
        .map_err(|_| TransferError::InvalidEnterprisePackage)?;
    let nonce =
        hex::decode(&package.nonce_hex).map_err(|_| TransferError::InvalidEnterprisePackage)?;
    let ciphertext = hex::decode(&package.ciphertext_hex)
        .map_err(|_| TransferError::InvalidEnterprisePackage)?;
    let tag = hex::decode(&package.authentication_tag_hex)
        .map_err(|_| TransferError::InvalidEnterprisePackage)?;
    if wrapped_dek.is_empty() || nonce.len() != 12 || ciphertext.is_empty() || tag.len() != 16 {
        return Err(TransferError::InvalidEnterprisePackage);
    }
    Ok(package)
}

pub fn validate_enterprise_plaintext(
    package: &EnterprisePackageV1,
    plaintext: &[u8],
) -> Result<ClientExportV1, TransferError> {
    if hex::encode(Sha256::digest(&plaintext)) != package.plaintext_sha256 {
        return Err(TransferError::EnterpriseAuthenticationFailed);
    }
    let export: ClientExportV1 = serde_json::from_slice(&plaintext)?;
    if export.export_id != package.export_id {
        return Err(TransferError::InvalidEnterprisePackage);
    }
    export.validate()?;
    Ok(export)
}

pub fn create_export(
    client_version: &str,
    mut batches: Vec<ArchiveBatchV1>,
    conversations: Vec<ConversationV1>,
    participants: Vec<ParticipantV1>,
) -> Result<ClientExportV1, TransferError> {
    sanitize_batches(&mut batches);
    let message_count = batches
        .iter()
        .map(|batch| batch.messages.len() as u64)
        .sum();
    let media = batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .flat_map(|message| &message.media)
        .collect::<Vec<_>>();
    let media_count = media.len() as u64;
    let missing_media_count = media
        .iter()
        .filter(|item| item.content_hash.is_none())
        .count() as u64;
    let content_sha256 = hash_content(&conversations, &participants, &batches)?;
    let export = ClientExportV1 {
        schema_version: CLIENT_EXPORT_SCHEMA_VERSION.into(),
        export_id: Uuid::new_v4(),
        generated_at: Utc::now(),
        client_version: client_version.into(),
        media_transport: "metadata_only".into(),
        conversations,
        participants,
        batches,
        message_count,
        media_count,
        missing_media_count,
        content_sha256,
    };
    export.validate()?;
    Ok(export)
}

fn sanitize_batches(batches: &mut [ArchiveBatchV1]) {
    for batch in batches {
        for message in &mut batch.messages {
            sanitize_json(&mut message.raw_payload, None);
            for media in &mut message.media {
                media.source_locator = media
                    .content_hash
                    .as_deref()
                    .map(|hash| format!("sha256:{hash}"))
                    .unwrap_or_else(|| "unavailable".into());
            }
        }
    }
}

fn sanitize_json(value: &mut serde_json::Value, key: Option<&str>) {
    match value {
        serde_json::Value::String(text)
            if key.is_some_and(|key| {
                let key = key.to_ascii_lowercase();
                key.contains("path") || key.contains("local_uri")
            }) =>
        {
            let digest = hex::encode(Sha256::digest(text.as_bytes()));
            *text = format!("redacted:sha256:{digest}");
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_json(item, key);
            }
        }
        serde_json::Value::Object(entries) => {
            for (child_key, child) in entries {
                sanitize_json(child, Some(child_key));
            }
        }
        _ => {}
    }
}

pub fn write_json(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    let readable = readable_view(export)?;
    atomic_write(target, |writer| {
        serde_json::to_writer_pretty(writer, &readable)?;
        Ok(())
    })
}

#[derive(Debug, Serialize)]
struct ReadableExport {
    schema_version: &'static str,
    exported_at: String,
    message_count: u64,
    conversation_count: usize,
    conversations: Vec<ReadableConversation>,
}

#[derive(Debug, Serialize)]
struct ReadableConversation {
    conversation_name: String,
    participants: Vec<String>,
    messages: Vec<ReadableMessage>,
}

#[derive(Debug, Serialize)]
struct ReadableMessage {
    sender: String,
    direction: String,
    sent_at: String,
    message_type: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    media: Vec<ReadableMedia>,
}

#[derive(Debug, Serialize)]
struct ReadableMedia {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    integrity: String,
}

fn readable_view(export: &ClientExportV1) -> Result<ReadableExport, serde_json::Error> {
    let mut participant_names: BTreeMap<_, _> = export
        .participants
        .iter()
        .map(|participant| {
            let display_name = participant
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty() && *name != participant.participant_id)
                .map(str::to_owned)
                .unwrap_or_else(|| "未知联系人".into());
            (participant.participant_id.as_str(), display_name)
        })
        .collect();
    let mut self_sender_ids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for message in export.batches.iter().flat_map(|batch| &batch.messages) {
        if message.direction == archive_domain::MessageDirection::Outgoing {
            if let Some(sender_id) = &message.sender_id {
                self_sender_ids
                    .entry(message.conversation_id.clone())
                    .or_default()
                    .insert(sender_id.clone());
            }
        }
    }
    for conversation in &export.conversations {
        let Some(display_name) = conversation
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if conversation.conversation_id.starts_with("S:") {
            let other_ids = conversation
                .participant_ids
                .iter()
                .filter(|id| {
                    !self_sender_ids
                        .get(&conversation.conversation_id)
                        .is_some_and(|ids| ids.contains(id.as_str()))
                })
                .collect::<Vec<_>>();
            if let [other_id] = other_ids.as_slice()
                && participant_names
                    .get(other_id.as_str())
                    .is_some_and(|name| name.starts_with("未知联系人"))
            {
                participant_names.insert(other_id.as_str(), display_name.to_owned());
            }
        }
    }

    let default_self_name = self_sender_ids
        .values()
        .flat_map(|ids| ids.iter())
        .find_map(|id| participant_names.get(id.as_str()))
        .filter(|name| !name.starts_with("未知联系人"))
        .cloned()
        .unwrap_or_else(|| "我".into());

    let mut messages_by_conversation: BTreeMap<String, Vec<ReadableMessage>> = BTreeMap::new();
    for message in export.batches.iter().flat_map(|batch| &batch.messages) {
        let sender_id = message.sender_id.as_deref().unwrap_or("system");
        let sender_name = match message.direction {
            archive_domain::MessageDirection::Outgoing => participant_names
                .get(sender_id)
                .filter(|name| !name.starts_with("未知联系人"))
                .cloned()
                .unwrap_or_else(|| default_self_name.clone()),
            archive_domain::MessageDirection::System => "系统".into(),
            _ => participant_names
                .get(sender_id)
                .cloned()
                .unwrap_or_else(|| "未知联系人".into()),
        };
        let media = message
            .media
            .iter()
            .map(|item| {
                Ok(ReadableMedia {
                    name: item
                        .original_name
                        .clone()
                        .unwrap_or_else(|| "未命名媒体".into()),
                    mime_type: item.mime_type.clone(),
                    size_bytes: item.size_bytes,
                    integrity: readable_media_integrity(&item.integrity).into(),
                })
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        let direction = match message.direction {
            archive_domain::MessageDirection::Incoming => "对方发送".into(),
            archive_domain::MessageDirection::Outgoing => format!("{sender_name}发送"),
            archive_domain::MessageDirection::System => "系统消息".into(),
            archive_domain::MessageDirection::Unknown => "方向未知".into(),
        };
        messages_by_conversation
            .entry(message.conversation_id.clone())
            .or_default()
            .push(ReadableMessage {
                sender: sender_name,
                direction,
                sent_at: message.sent_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                message_type: readable_message_type(&message.message_type, &message.raw_type),
                content: readable_message_content(message),
                media,
            });
    }
    let mut conversations: Vec<ReadableConversation> = export
        .conversations
        .iter()
        .map(|conversation| {
            let self_name = self_sender_ids
                .get(&conversation.conversation_id)
                .and_then(|ids| ids.iter().find_map(|id| participant_names.get(id.as_str())))
                .filter(|name| !name.starts_with("未知联系人"))
                .unwrap_or(&default_self_name)
                .clone();
            let mut participants = conversation
                .participant_ids
                .iter()
                .filter(|id| {
                    !self_sender_ids
                        .get(&conversation.conversation_id)
                        .is_some_and(|ids| ids.contains(id.as_str()))
                })
                .map(|id| {
                    participant_names
                        .get(id.as_str())
                        .cloned()
                        .unwrap_or_else(|| "未知联系人".into())
                })
                .collect::<Vec<_>>();
            if !participants
                .iter()
                .any(|participant| participant == &self_name)
            {
                participants.insert(0, self_name.clone());
            }
            let conversation_name = conversation.display_name.clone().unwrap_or_else(|| {
                let others = participants
                    .iter()
                    .filter(|participant| participant.as_str() != self_name.as_str())
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                match others.as_slice() {
                    [name] => format!("{self_name} 与 {name}"),
                    [] if conversation.conversation_id.starts_with("S:") => {
                        format!("{self_name} 的单聊")
                    }
                    [] => "群聊".into(),
                    names => format!("群聊：{}", names.join("、")),
                }
            });
            let mut messages = messages_by_conversation
                .remove(&conversation.conversation_id)
                .unwrap_or_default();
            messages.sort_by(|left, right| left.sent_at.cmp(&right.sent_at));
            ReadableConversation {
                conversation_name,
                participants,
                messages,
            }
        })
        .collect();
    conversations.sort_by(|left, right| {
        latest_message_time(right)
            .cmp(latest_message_time(left))
            .then_with(|| left.conversation_name.cmp(&right.conversation_name))
    });
    Ok(ReadableExport {
        schema_version: "readable-export.v1",
        exported_at: export.generated_at.to_rfc3339(),
        message_count: export.message_count,
        conversation_count: conversations.len(),
        conversations,
    })
}

fn latest_message_time(conversation: &ReadableConversation) -> &str {
    conversation
        .messages
        .last()
        .map(|message| message.sent_at.as_str())
        .unwrap_or("")
}

fn readable_message_type(value: &archive_domain::MessageType, raw_type: &str) -> &'static str {
    match raw_type.trim() {
        "6" => return "位置",
        "13" => return "文档链接",
        "40" => return "音视频通话",
        "1018" => return "语音通话",
        "70" => return "待办",
        "215" => return "笔记",
        "1001" | "1073" | "580" | "581" | "582" => return "会议",
        "10" | "20" | "21" | "22" | "105" | "123" | "145" | "221" | "516" | "561" | "565"
        | "570" | "573" | "579" => return "卡片",
        _ => {}
    }
    match value {
        archive_domain::MessageType::Text => "文本",
        archive_domain::MessageType::Image => "图片",
        archive_domain::MessageType::Audio => "语音",
        archive_domain::MessageType::Video => "视频",
        archive_domain::MessageType::File => "文件",
        archive_domain::MessageType::Link => "链接",
        archive_domain::MessageType::Reply => "回复",
        archive_domain::MessageType::System => "系统消息",
        archive_domain::MessageType::Unsupported => "暂不支持的消息",
    }
}

fn readable_content(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            matches!(character, '\n' | '\r' | '\t')
                || (!character.is_control() && *character != '\u{fffd}')
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn readable_message_content(message: &archive_domain::MessageV1) -> String {
    let content = readable_content(message.body_text.as_deref());
    if !content.is_empty() {
        return content;
    }
    match message.message_type {
        archive_domain::MessageType::Image => "[图片]",
        archive_domain::MessageType::Audio => "[语音]",
        archive_domain::MessageType::Video => "[视频]",
        archive_domain::MessageType::File => "[文件]",
        archive_domain::MessageType::Link => "[链接]",
        archive_domain::MessageType::Reply => "[回复]",
        archive_domain::MessageType::Unsupported => "[暂不支持的消息]",
        archive_domain::MessageType::Text | archive_domain::MessageType::System => "",
    }
    .into()
}

fn readable_media_integrity(value: &archive_domain::MediaIntegrity) -> &'static str {
    match value {
        archive_domain::MediaIntegrity::Verified => "完整",
        archive_domain::MediaIntegrity::Missing => "缺失",
        archive_domain::MediaIntegrity::SizeMismatch => "大小不匹配",
        archive_domain::MediaIntegrity::HashMismatch => "校验失败",
        archive_domain::MediaIntegrity::Unreadable => "无法读取",
    }
}

pub fn write_csv(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    let readable = readable_view(export)?;
    atomic_write(target, |writer| {
        writer.write_all(&[0xEF, 0xBB, 0xBF])?;
        let mut csv = csv::WriterBuilder::new().from_writer(writer);
        csv.write_record([
            "conversation_name",
            "sender",
            "direction",
            "sent_at",
            "message_type",
            "content",
            "media",
        ])?;
        for conversation in &readable.conversations {
            for message in &conversation.messages {
                let media = message
                    .media
                    .iter()
                    .map(|item| item.name.as_str())
                    .collect::<Vec<_>>()
                    .join("；");
                csv.write_record([
                    safe_csv(&conversation.conversation_name),
                    safe_csv(&message.sender),
                    message.direction.clone(),
                    message.sent_at.clone(),
                    message.message_type.into(),
                    safe_csv(&message.content),
                    safe_csv(&media),
                ])?;
            }
        }
        csv.flush()?;
        Ok(())
    })
}

pub fn read_json(path: &Path) -> Result<ClientExportV1, TransferError> {
    let reader = BufReader::new(File::open(path)?);
    let export: ClientExportV1 = serde_json::from_reader(reader)?;
    export.validate()?;
    verify_checksum(&export)?;
    Ok(export)
}

/// A reading copy, without executable content or external resources.
pub fn write_html(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    let readable = readable_view(export)?;
    atomic_write(target, |writer| {
        writer.write_all(br#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'"><title>"#)?;
        writer.write_all("企业微信记录归档</title><style>body{max-width:960px;margin:32px auto;padding:0 20px;font:15px/1.7 system-ui,sans-serif;color:#17243a}section{margin:32px 0}article{padding:16px 0;border-top:1px solid #ddd;break-inside:avoid}h2{font-size:20px;margin:0 0 8px}h3{font-size:16px;margin:0}p{white-space:pre-wrap;overflow-wrap:anywhere}small{color:#536176}ul{overflow-wrap:anywhere}</style></head><body><h1>企业微信记录归档</h1>".as_bytes())?;
        writeln!(
            writer,
            "<p>共 {} 个会话 · {} 条消息</p>",
            readable.conversation_count, readable.message_count
        )?;
        for conversation in &readable.conversations {
            writeln!(
                writer,
                "<section><h2>{}</h2>",
                escape_html(&conversation.conversation_name)
            )?;
            for message in &conversation.messages {
                let content = if message.content.is_empty() {
                    "[无文本内容]"
                } else {
                    &message.content
                };
                writeln!(
                    writer,
                    "<article><h3>{}</h3><small>{} · {} · {}</small><p>{}</p>",
                    escape_html(&message.sender),
                    escape_html(&message.sent_at),
                    escape_html(&message.direction),
                    escape_html(message.message_type),
                    escape_html(content)
                )?;
                if !message.media.is_empty() {
                    writer.write_all("<ul aria-label=\"媒体引用\">".as_bytes())?;
                    for media in &message.media {
                        writeln!(
                            writer,
                            "<li>{} · {}</li>",
                            escape_html(&media.name),
                            escape_html(&media.integrity)
                        )?;
                    }
                    writer.write_all(b"</ul>")?;
                }
                writer.write_all(b"</article>\n")?;
            }
            writer.write_all(b"</section>\n")?;
        }
        writer.write_all(b"</body></html>")?;
        Ok(())
    })
}

pub fn write_txt(export: &ClientExportV1, target: &Path) -> Result<(), TransferError> {
    export.validate()?;
    verify_checksum(export)?;
    let readable = readable_view(export)?;
    atomic_write(target, |writer| {
        writer.write_all(&[0xEF, 0xBB, 0xBF])?;
        writeln!(
            writer,
            "企业微信记录归档\r\n共 {} 个会话 · {} 条消息\r\n",
            readable.conversation_count, readable.message_count
        )?;
        for conversation in &readable.conversations {
            writeln!(
                writer,
                "会话：{}\r",
                plain_text(&conversation.conversation_name)
            )?;
            for message in &conversation.messages {
                writeln!(
                    writer,
                    "[{}] {} | {} | {}\r",
                    message.sent_at,
                    plain_text(&message.sender),
                    message.direction,
                    message.message_type
                )?;
                let content = if message.content.is_empty() {
                    "[无文本内容]"
                } else {
                    &message.content
                };
                writeln!(writer, "{}\r", plain_text(content))?;
                for media in &message.media {
                    writeln!(
                        writer,
                        "[媒体引用] {} · {}\r",
                        plain_text(&media.name),
                        media.integrity
                    )?;
                }
                writer.write_all(b"\r\n")?;
            }
        }
        Ok(())
    })
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn plain_text(value: &str) -> String {
    let mut result = String::new();
    let mut emoji_placeholder = false;
    for character in value.chars() {
        let code = character as u32;
        if character == '\u{fffd}' {
            result.push_str("[未知字符]");
            emoji_placeholder = false;
        } else if is_emoji(code) {
            if !emoji_placeholder {
                result.push_str("[表情]");
                emoji_placeholder = true;
            }
        } else if matches!(code, 0x200d | 0xfe0e | 0xfe0f) {
            // Emoji joiners and variation selectors have no standalone text form.
        } else if !character.is_control() || matches!(character, '\n' | '\r' | '\t') {
            result.push(character);
            emoji_placeholder = false;
        }
    }
    result
}

fn is_emoji(code: u32) -> bool {
    (0x1f000..=0x1faff).contains(&code) || (0x2600..=0x27bf).contains(&code)
}

pub fn read_json_slice(bytes: &[u8]) -> Result<ClientExportV1, TransferError> {
    let export: ClientExportV1 = serde_json::from_slice(bytes)?;
    export.validate()?;
    verify_checksum(&export)?;
    Ok(export)
}

fn verify_checksum(export: &ClientExportV1) -> Result<(), TransferError> {
    if hash_content(&export.conversations, &export.participants, &export.batches)?
        == export.content_sha256
    {
        Ok(())
    } else {
        Err(TransferError::ChecksumMismatch)
    }
}

fn hash_content(
    conversations: &[ConversationV1],
    participants: &[ParticipantV1],
    batches: &[ArchiveBatchV1],
) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(&(conversations, participants, batches))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn atomic_write<F>(target: &Path, action: F) -> Result<(), TransferError>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(), TransferError>,
{
    if target.exists() {
        return Err(TransferError::TargetExists);
    }
    let parent = target.parent().ok_or(TransferError::UnsafeTarget)?;
    fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(TransferError::UnsafeTarget)?;
    let partial = target.with_file_name(format!("{file_name}.partial"));
    // Exclusively own this partial file; never remove another export's work.
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let result = (|| {
        let mut writer = BufWriter::new(file);
        action(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        // Linking publishes complete data without replacing a target created
        // after the initial existence check. Unsupported filesystems fail closed.
        fs::hard_link(&partial, target)?;
        Ok(())
    })();
    let _ = fs::remove_file(partial);
    result
}

fn safe_csv(value: &str) -> String {
    if value
        .trim_start_matches([' ', '\t', '\r', '\n'])
        .starts_with(['=', '+', '-', '@'])
    {
        format!("'{value}")
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{
        BATCH_SCHEMA_VERSION, CollectionScope, EmployeeNoticeEvidence, ExternalContactConsent,
        RetentionDirective,
    };

    fn empty_batch() -> ArchiveBatchV1 {
        ArchiveBatchV1 {
            schema_version: BATCH_SCHEMA_VERSION.into(),
            batch_id: Uuid::new_v4(),
            source_adapter: "fixture".into(),
            source_instance_id: "source".into(),
            collected_at: Utc::now(),
            employee_notice: EmployeeNoticeEvidence {
                notice_version: "fixture".into(),
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
            cursor_after: None,
            messages: vec![],
            content_sha256: "fixture".into(),
            retention: RetentionDirective {
                policy_id: None,
                expires_at: None,
                legal_hold: false,
            },
        }
    }

    #[test]
    fn internal_contract_remains_parseable_and_readable_json_is_slim() {
        let export = create_export("fixture-client", vec![empty_batch()], vec![], vec![]).unwrap();
        let bytes = serde_json::to_vec(&export).unwrap();
        let imported = read_json_slice(&bytes).unwrap();
        assert_eq!(imported.export_id, export.export_id);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pretty.json");
        write_json(&export, &path).unwrap();
        let pretty = fs::read_to_string(path).unwrap();
        assert!(pretty.contains("\n  \"schema_version\""));
        assert!(pretty.contains("\"readable-export.v1\""));
        assert!(!pretty.contains("\"batches\""));
        assert!(!pretty.contains("\"raw_payload\""));
        assert!(pretty.ends_with("}\n") || pretty.ends_with('}'));
    }

    #[test]
    fn rejects_modified_batch_content() {
        let mut export =
            create_export("fixture-client", vec![empty_batch()], vec![], vec![]).unwrap();
        export.batches[0].cursor_after = Some("modified".into());
        assert!(matches!(
            verify_checksum(&export),
            Err(TransferError::ChecksumMismatch)
        ));
    }

    fn message_export() -> ClientExportV1 {
        let mut batch = empty_batch();
        batch.messages.push(archive_domain::MessageV1 {
            schema_version: archive_domain::MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "fixture".into(),
            source_instance_id: "source".into(),
            source_message_id: "message-1".into(),
            stable_message_id: "message-1".into(),
            conversation_id: "fixture-conversation".into(),
            sender_id: Some("participant-1".into()),
            sent_at: Utc::now(),
            direction: archive_domain::MessageDirection::Incoming,
            message_type: archive_domain::MessageType::Image,
            body_text: Some("=1+1 📣📣\n中文 <script>alert(1)</script> & \"引用\"".into()),
            quoted_message_id: None,
            lifecycle: archive_domain::LifecycleState::Recalled,
            media: vec![archive_domain::MediaRefV1 {
                content_hash: None,
                original_name: Some("<img src=x onerror=alert(1)>".into()),
                mime_type: None,
                size_bytes: None,
                source_locator: "fixture".into(),
                integrity: archive_domain::MediaIntegrity::Missing,
                missing_reason: None,
            }],
            raw_type: "image".into(),
            raw_payload: serde_json::json!({}),
            parser_version: "fixture".into(),
            collection_batch_id: batch.batch_id,
        });
        create_export(
            "fixture",
            vec![batch],
            vec![ConversationV1 {
                conversation_id: "fixture-conversation".into(),
                display_name: Some("合成会话 <test>".into()),
                conversation_type: None,
                participant_ids: vec!["participant-1".into()],
            }],
            vec![ParticipantV1 {
                participant_id: "participant-1".into(),
                display_name: Some("测试成员".into()),
                participant_kind: None,
            }],
        )
        .unwrap()
    }

    type Writer = fn(&ClientExportV1, &Path) -> Result<(), TransferError>;
    const WRITERS: [(&str, Writer); 4] = [
        ("json", write_json),
        ("csv", write_csv),
        ("html", write_html),
        ("txt", write_txt),
    ];

    #[test]
    fn reading_formats_escape_content_and_preserve_unicode_and_media_references() {
        let directory = tempfile::tempdir().unwrap();
        let export = message_export();
        let json_path = directory.path().join("export.json");
        write_json(&export, &json_path).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
        assert_eq!(json["conversation_count"], 1);
        let conversation = &json["conversations"][0];
        assert_eq!(conversation["conversation_name"], "合成会话 <test>");
        assert_eq!(
            conversation["participants"],
            serde_json::json!(["我", "测试成员"])
        );
        assert_eq!(conversation["messages"][0]["sender"], "测试成员");
        assert_eq!(conversation["messages"][0]["direction"], "对方发送");
        assert_eq!(conversation["messages"][0]["message_type"], "图片");
        assert!(conversation.get("conversation_id").is_none());
        assert!(conversation["messages"][0].get("sender_id").is_none());
        assert!(conversation["messages"][0].get("sender_name").is_none());
        assert!(conversation["messages"][0].get("raw_payload").is_none());
        assert!(json.get("batches").is_none());

        let mut unnamed = export.clone();
        unnamed.conversations[0].display_name = None;
        let readable = readable_view(&unnamed).unwrap();
        assert_eq!(
            readable.conversations[0].conversation_name,
            "我 与 测试成员"
        );

        let mut unresolved = export.clone();
        unresolved.participants[0].display_name = None;
        unresolved.conversations[0].display_name = None;
        let readable = readable_view(&unresolved).unwrap();
        assert_eq!(readable.conversations[0].participants[1], "未知联系人");
        assert_eq!(readable.conversations[0].messages[0].sender, "未知联系人");
        assert_eq!(
            readable_content(Some("\u{001e}\u{0008}\0\u{0012}\u{001a}\n\u{0018}")),
            ""
        );

        let mut named_self = export.clone();
        named_self.conversations[0].display_name = None;
        named_self.conversations[0]
            .participant_ids
            .push("self-participant".into());
        named_self.participants.push(ParticipantV1 {
            participant_id: "self-participant".into(),
            display_name: Some("当前用户".into()),
            participant_kind: None,
        });
        named_self.batches[0].messages[0].sender_id = Some("self-participant".into());
        named_self.batches[0].messages[0].direction = archive_domain::MessageDirection::Outgoing;
        let readable = readable_view(&named_self).unwrap();
        assert_eq!(
            readable.conversations[0].participants,
            ["当前用户", "测试成员"]
        );
        assert_eq!(
            readable.conversations[0].conversation_name,
            "当前用户 与 测试成员"
        );
        assert_eq!(readable.conversations[0].messages[0].sender, "当前用户");
        assert_eq!(
            readable.conversations[0].messages[0].direction,
            "当前用户发送"
        );

        let html_path = directory.path().join("export.html");
        write_html(&export, &html_path).unwrap();
        let html = fs::read_to_string(html_path).unwrap();
        assert!(html.contains("中文 &lt;script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("default-src 'none'"));
        assert!(html.contains("测试成员"));
        assert!(!html.contains("participant-1"));

        let txt_path = directory.path().join("export.txt");
        write_txt(&export, &txt_path).unwrap();
        let txt = fs::read_to_string(txt_path).unwrap();
        assert!(txt.starts_with('\u{feff}'));
        assert!(txt.contains("=1+1 [表情]\n中文"));
        assert!(txt.contains("=1+1 [表情]"));
        assert!(!txt.contains('📣'));
        assert!(txt.contains("[媒体引用]"));
        assert!(txt.contains("测试成员"));
        assert!(!txt.contains("participant-1"));

        let csv_path = directory.path().join("export.csv");
        write_csv(&export, &csv_path).unwrap();
        let csv = fs::read_to_string(csv_path).unwrap();
        assert!(csv.contains("'=1+1"));
        assert!(csv.contains("测试成员"));
        assert!(!csv.contains("participant-1"));
    }

    #[test]
    fn puts_recent_conversations_first() {
        let mut export = message_export();
        let mut older_message = export.batches[0].messages[0].clone();
        older_message.source_message_id = "message-older".into();
        older_message.stable_message_id = "message-older".into();
        older_message.conversation_id = "older-conversation".into();
        older_message.sent_at = Utc::now() - chrono::Duration::days(1);
        export.batches[0].messages.push(older_message);
        export.conversations.push(ConversationV1 {
            conversation_id: "older-conversation".into(),
            display_name: Some("较早会话".into()),
            conversation_type: None,
            participant_ids: vec!["participant-1".into()],
        });
        let export = create_export(
            "fixture",
            export.batches,
            export.conversations,
            export.participants,
        )
        .unwrap();
        let readable = readable_view(&export).unwrap();
        assert_eq!(
            readable
                .conversations
                .iter()
                .map(|conversation| conversation.conversation_name.as_str())
                .collect::<Vec<_>>(),
            ["合成会话 <test>", "较早会话"]
        );
    }

    #[test]
    fn all_formats_publish_complete_files_and_reject_overwrites_and_tampering() {
        let directory = tempfile::tempdir().unwrap();
        for (extension, write) in WRITERS {
            let mut export = message_export();
            let target = directory.path().join(format!("export.{extension}"));
            write(&export, &target).unwrap();
            let original = fs::read(&target).unwrap();
            assert!(
                !target
                    .with_file_name(format!("export.{extension}.partial"))
                    .exists()
            );
            assert!(matches!(
                write(&export, &target),
                Err(TransferError::TargetExists)
            ));
            assert_eq!(fs::read(&target).unwrap(), original);
            export.batches[0].messages[0].body_text = None;
            let tampered = directory.path().join(format!("tampered.{extension}"));
            assert!(matches!(
                write(&export, &tampered),
                Err(TransferError::ChecksumMismatch)
            ));
            assert!(!tampered.exists());
        }
    }

    #[test]
    fn empty_exports_and_messages_without_text_are_supported() {
        let directory = tempfile::tempdir().unwrap();
        let empty = create_export("fixture", vec![empty_batch()], vec![], vec![]).unwrap();
        for (extension, write) in WRITERS {
            write(&empty, &directory.path().join(format!("empty.{extension}"))).unwrap();
        }
        let mut export = message_export();
        export.batches[0].messages[0].body_text = None;
        let export =
            create_export("fixture", export.batches, export.conversations, vec![]).unwrap();
        let target = directory.path().join("no-text.html");
        write_html(&export, &target).unwrap();
        assert!(fs::read_to_string(target).unwrap().contains("[图片]"));
    }

    #[test]
    fn atomic_write_preserves_existing_partial_and_cleans_own_failed_output() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("export.txt");
        let partial = directory.path().join("export.txt.partial");
        fs::write(&partial, "other export").unwrap();
        assert!(atomic_write(&target, |_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(&partial).unwrap(), "other export");
        fs::remove_file(&partial).unwrap();
        assert!(
            atomic_write(&target, |writer| {
                writer.write_all(b"incomplete")?;
                Err(TransferError::UnsafeTarget)
            })
            .is_err()
        );
        assert!(!partial.exists());
        assert!(!target.exists());
    }

    #[test]
    fn atomic_publish_does_not_replace_target_created_during_write() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("export.txt");
        assert!(
            atomic_write(&target, |writer| {
                writer.write_all(b"new export")?;
                fs::write(&target, "existing export")?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing export");
        assert!(!directory.path().join("export.txt.partial").exists());
    }
}
