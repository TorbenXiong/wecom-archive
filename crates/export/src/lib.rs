use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use archive_domain::{MediaIntegrity, MessageDirection, MessageV1};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Json,
    Csv,
    Html,
    Pdf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportScope {
    CurrentConversation,
    CurrentFilter,
    EntireArchive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportParticipant {
    pub participant_id: String,
    pub display_name: Option<String>,
    pub participant_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMedia {
    pub content_sha256: Option<String>,
    pub original_name: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub source_path: Option<PathBuf>,
    pub integrity: MediaIntegrity,
    pub missing_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExportPackage {
    pub archive_id: String,
    pub scope: ExportScope,
    pub format: ExportFormat,
    pub messages: Vec<MessageV1>,
    pub participants: Vec<ExportParticipant>,
    pub conversation_names: BTreeMap<String, String>,
    pub simplify: bool,
    pub pretty: bool,
    pub media: Vec<ExportMedia>,
    pub generated_at: DateTime<Utc>,
    pub edge_executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    pub export_id: Uuid,
    pub target: PathBuf,
    pub message_count: u64,
    pub media_count: u64,
    pub missing_media_count: u64,
    pub manifest_sha256: String,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("target already exists")]
    TargetExists,
    #[error("export was cancelled")]
    Cancelled,
    #[error("unsafe export path")]
    UnsafePath,
    #[error("PDF renderer is unavailable")]
    PdfRendererUnavailable,
    #[error("PDF rendering failed")]
    PdfRenderFailed,
    #[error("export I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("CSV serialization failed: {0}")]
    Csv(#[from] csv::Error),
}

pub fn export_package(
    package: &ExportPackage,
    target: &Path,
    cancelled: &AtomicBool,
) -> Result<ExportResult, ExportError> {
    if target.exists() {
        return Err(ExportError::TargetExists);
    }
    let parent = target.parent().ok_or(ExportError::UnsafePath)?;
    fs::create_dir_all(parent)?;
    let partial = partial_path(target)?;
    if partial.exists() {
        fs::remove_file(&partial)?;
    }

    let result = (|| {
        let export_id = Uuid::new_v4();
        let (message_count, media_count, missing_media_count) =
            selected_messages(package).fold((0, 0, 0), |(messages, media, missing), message| {
                (
                    messages + 1,
                    media + message.media.len() as u64,
                    missing
                        + message
                            .media
                            .iter()
                            .filter(|media| media.integrity != MediaIntegrity::Verified)
                            .count() as u64,
                )
            });
        check_cancelled(cancelled)?;
        render_payload(package, &partial, cancelled)?;
        check_cancelled(cancelled)?;
        publish_without_overwrite(&partial, target)?;
        Ok(ExportResult {
            export_id,
            target: target.to_path_buf(),
            message_count,
            media_count,
            missing_media_count,
            manifest_sha256: hash_file(target)?,
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn render_payload(
    package: &ExportPackage,
    target: &Path,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    match package.format {
        ExportFormat::Json => write_json(target, package, cancelled),
        ExportFormat::Csv => write_csv(target, package, cancelled),
        ExportFormat::Html => write_html(target, package, cancelled, false),
        ExportFormat::Pdf => write_pdf(target, package, cancelled),
    }
}

fn write_json(
    target: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut output = BufWriter::new(File::create(target)?);
    let names = ExportNames::new(package);
    output.write_all(if package.pretty { b"[\n" } else { b"[" })?;
    for (index, message) in selected_messages(package).enumerate() {
        check_cancelled(cancelled)?;
        if index > 0 {
            output.write_all(if package.pretty { b",\n" } else { b"," })?;
        }
        if package.simplify {
            let view = SimplifiedMessage {
                conversation: names.conversation(message),
                sender: names.sender(message),
                content: message.body_text.as_deref(),
                media: message
                    .media
                    .iter()
                    .map(|media| media.original_name.as_deref().unwrap_or("未命名附件"))
                    .collect(),
                message_type: &message.message_type,
                sent_at: &message.sent_at,
            };
            write_json_record(&mut output, &view, package.pretty)?;
        } else {
            write_json_record(&mut output, message, package.pretty)?;
        }
    }
    output.write_all(if package.pretty { b"\n]\n" } else { b"]" })?;
    output.flush()?;
    Ok(())
}

// Serialize the reading projection directly: JSON Value maps sort keys alphabetically.
#[derive(Serialize)]
struct SimplifiedMessage<'a> {
    conversation: &'a str,
    sender: &'a str,
    content: Option<&'a str>,
    media: Vec<&'a str>,
    message_type: &'a archive_domain::MessageType,
    sent_at: &'a DateTime<Utc>,
}

fn write_json_record<T: Serialize>(
    output: &mut impl Write,
    value: &T,
    pretty: bool,
) -> Result<(), ExportError> {
    if pretty {
        let formatted = serde_json::to_string_pretty(value)?;
        for (index, line) in formatted.lines().enumerate() {
            if index > 0 {
                output.write_all(b"\n")?;
            }
            write!(output, "  {line}")?;
        }
    } else {
        serde_json::to_writer(output, value)?;
    }
    Ok(())
}

fn selected_messages(package: &ExportPackage) -> impl Iterator<Item = &MessageV1> {
    package.messages.iter().filter(|message| {
        !package.simplify
            || !matches!(
                message.raw_type.as_str(),
                "group_announcement" | "group_board"
            )
    })
}

struct ExportNames<'a> {
    participants: BTreeMap<&'a str, &'a str>,
    conversations: &'a BTreeMap<String, String>,
}

impl<'a> ExportNames<'a> {
    fn new(package: &'a ExportPackage) -> Self {
        Self {
            participants: package
                .participants
                .iter()
                .filter_map(|participant| {
                    participant
                        .display_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty() && *name != participant.participant_id)
                        .map(|name| (participant.participant_id.as_str(), name))
                })
                .collect(),
            conversations: &package.conversation_names,
        }
    }

    fn sender(&self, message: &MessageV1) -> &str {
        if message.direction == MessageDirection::System {
            return "系统";
        }
        message
            .sender_id
            .as_deref()
            .and_then(|id| self.participants.get(id).copied())
            .unwrap_or(if message.direction == MessageDirection::Outgoing {
                "我"
            } else {
                "未知联系人"
            })
    }

    fn conversation(&self, message: &MessageV1) -> &str {
        self.conversations
            .get(&message.conversation_id)
            .map(String::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && *name != message.conversation_id)
            .unwrap_or("未命名会话")
    }
}

fn write_csv(
    target: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut messages = csv_writer(target)?;
    let names = ExportNames::new(package);
    let headers = [
        "stable_message_id",
        "conversation_id",
        "sender_id",
        "sent_at",
        "direction",
        "message_type",
        "body_text",
        "lifecycle",
        "quoted_message_id",
        "raw_type",
        "media_names",
        "media_hashes",
    ];
    let pretty_headers = [
        "消息 ID",
        "会话 ID",
        "发送人 ID",
        "时间",
        "方向",
        "消息类型",
        "正文",
        "消息状态",
        "引用消息 ID",
        "原始类型",
        "附件名称",
        "附件哈希",
    ];
    let simple_headers = [
        "conversation",
        "sender",
        "content",
        "media",
        "message_type",
        "sent_at",
    ];
    let pretty_simple_headers = ["会话", "发送人", "正文", "附件", "消息类型", "时间"];
    messages.write_record(match (package.simplify, package.pretty) {
        (true, true) => &pretty_simple_headers[..],
        (true, false) => &simple_headers[..],
        (false, true) => &pretty_headers[..],
        (false, false) => &headers[..],
    })?;
    for message in selected_messages(package) {
        check_cancelled(cancelled)?;
        let media_names = message
            .media
            .iter()
            .filter_map(|media| media.original_name.as_deref())
            .collect::<Vec<_>>()
            .join(" | ");
        if package.simplify {
            messages.write_record([
                safe_csv(names.conversation(message)),
                safe_csv(names.sender(message)),
                safe_csv(message.body_text.as_deref().unwrap_or_default()),
                safe_csv(&media_names),
                display_message_type(message, package.pretty)?,
                display_time(message, package.pretty),
            ])?;
            continue;
        }
        let media_hashes = message
            .media
            .iter()
            .filter_map(|media| media.content_hash.as_deref())
            .collect::<Vec<_>>()
            .join(" | ");
        messages.write_record([
            safe_csv(&message.stable_message_id),
            safe_csv(&message.conversation_id),
            safe_csv(message.sender_id.as_deref().unwrap_or_default()),
            display_time(message, package.pretty),
            enum_text(&message.direction)?,
            display_message_type(message, package.pretty)?,
            safe_csv(message.body_text.as_deref().unwrap_or_default()),
            enum_text(&message.lifecycle)?,
            safe_csv(message.quoted_message_id.as_deref().unwrap_or_default()),
            safe_csv(&message.raw_type),
            safe_csv(&media_names),
            safe_csv(&media_hashes),
        ])?;
    }
    messages.flush()?;
    Ok(())
}

fn csv_writer(path: &Path) -> Result<csv::Writer<File>, ExportError> {
    let mut file = File::create(path)?;
    file.write_all(&[0xEF, 0xBB, 0xBF])?;
    Ok(csv::WriterBuilder::new().from_writer(file))
}

fn safe_csv(value: &str) -> String {
    if value
        .trim_start_matches([' ', '\t', '\r', '\n'])
        .starts_with(['=', '+', '-', '@'])
    {
        format!("'{value}")
    } else {
        value.to_owned()
    }
}

fn write_html(
    target: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
    for_print: bool,
) -> Result<(), ExportError> {
    let mut output = BufWriter::new(File::create(target)?);
    let names = ExportNames::new(package);
    output.write_all(html_document_start("归档导出", for_print, package.pretty).as_bytes())?;
    let summary_details = if package.simplify {
        String::new()
    } else {
        format!(
            " · {} 位参与人 · {} 个媒体记录",
            package.participants.len(),
            package.media.len()
        )
    };
    write!(
        output,
        "<section class=\"summary\"><p>{} 条消息{summary_details}</p></section>",
        selected_messages(package).count(),
    )?;
    for message in selected_messages(package) {
        check_cancelled(cancelled)?;
        let sender = if package.simplify {
            html_escape(names.sender(message))
        } else if package.pretty {
            html_escape(&format!(
                "{} ({})",
                names.sender(message),
                message.sender_id.as_deref().unwrap_or("系统")
            ))
        } else {
            html_escape(message.sender_id.as_deref().unwrap_or("系统"))
        };
        let conversation = if package.simplify {
            html_escape(names.conversation(message))
        } else if package.pretty {
            html_escape(&format!(
                "{} ({})",
                names.conversation(message),
                message.conversation_id
            ))
        } else {
            html_escape(&message.conversation_id)
        };
        let body = html_escape(message.body_text.as_deref().unwrap_or("[不支持的消息类型]"));
        let media = message
            .media
            .iter()
            .filter_map(|item| item.original_name.as_deref())
            .map(html_escape)
            .collect::<Vec<_>>()
            .join(" · ");
        write!(
            output,
            "<article class=\"message\"><header><time>{}</time><strong>{sender}</strong><span>{}</span><span>会话 {conversation}</span></header><p>{body}</p>{}</article>",
            html_escape(&display_time(message, package.pretty)),
            html_escape(&display_message_type(message, package.pretty)?),
            if media.is_empty() {
                String::new()
            } else {
                format!("<small class=\"media\">附件：{media}</small>")
            },
        )?;
    }
    output.write_all(b"</main></body></html>")?;
    output.flush()?;
    Ok(())
}

fn html_document_start(title: &str, for_print: bool, pretty: bool) -> String {
    let print_style = if for_print {
        "@page{size:A4;margin:18mm 16mm 20mm}@media print{a{color:inherit;text-decoration:none}.message{break-inside:avoid}}"
    } else {
        ""
    };
    let style = if pretty { BASE_CSS } else { COMPACT_CSS };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{style}{print_style}</style></head><body><main><h1>{title}</h1>"
    )
}

fn write_pdf(
    target: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let edge = package
        .edge_executable
        .as_deref()
        .filter(|path| path.is_file())
        .ok_or(ExportError::PdfRendererUnavailable)?;
    let directory = tempfile::tempdir_in(target.parent().ok_or(ExportError::UnsafePath)?)?;
    let html = directory.path().join("export.html");
    write_html(&html, package, cancelled, true)?;
    check_cancelled(cancelled)?;
    render_one_pdf(edge, &html, target)
}

fn render_one_pdf(edge: &Path, html: &Path, output: &Path) -> Result<(), ExportError> {
    let html = html.canonicalize()?;
    let output = output.to_path_buf();
    let status = Command::new(edge)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-pdf-header-footer")
        .arg(format!("--print-to-pdf={}", output.display()))
        .arg(format!(
            "file:///{}",
            html.to_string_lossy().replace('\\', "/")
        ))
        .status()?;
    if !status.success() || !output.is_file() {
        return Err(ExportError::PdfRenderFailed);
    }
    Ok(())
}

fn partial_path(target: &Path) -> Result<PathBuf, ExportError> {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ExportError::UnsafePath)?;
    Ok(target.with_file_name(format!("{name}.partial")))
}

fn publish_without_overwrite(partial: &Path, target: &Path) -> Result<(), ExportError> {
    match fs::hard_link(partial, target) {
        Ok(()) => {
            fs::remove_file(partial)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(ExportError::TargetExists)
        }
        Err(error) => Err(error.into()),
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ExportError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

fn hash_file(path: &Path) -> Result<String, ExportError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn enum_text<T: Serialize>(value: &T) -> Result<String, ExportError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

fn display_time(message: &MessageV1, pretty: bool) -> String {
    if pretty {
        message.sent_at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    } else {
        message.sent_at.to_rfc3339()
    }
}

fn display_message_type(message: &MessageV1, pretty: bool) -> Result<String, ExportError> {
    if !pretty {
        return enum_text(&message.message_type);
    }
    use archive_domain::MessageType;
    Ok(match message.message_type {
        MessageType::Text => "文本",
        MessageType::Image => "图片",
        MessageType::Audio => "语音",
        MessageType::Video => "视频",
        MessageType::File => "文件",
        MessageType::Link => "链接",
        MessageType::Reply => "回复",
        MessageType::System => "系统消息",
        MessageType::Unsupported => "暂不支持的消息",
    }
    .into())
}

const COMPACT_CSS: &str = "body{font:12px/1.3 sans-serif;margin:8px}h1{font-size:16px;margin:0}.summary p,.message p{margin:0}.message{border-bottom:1px solid #ddd;padding:2px 0;overflow-wrap:anywhere}.message header{display:flex;flex-wrap:wrap;gap:8px}.message p{white-space:pre-wrap}";

const BASE_CSS: &str = r#"
:root{font-family:"Microsoft YaHei UI","Segoe UI",sans-serif;color:#10213c;background:#f5f7fb}
*{box-sizing:border-box}body{margin:0}main{max-width:980px;margin:0 auto;padding:36px;background:#fff;min-height:100vh}
h1{font-size:24px;margin:0 0 24px}.summary{padding:16px 20px;background:#eef4ff;border:1px solid #d7e4ff;border-radius:10px}
.session-list{padding:0;list-style:none}.session-list li{display:flex;justify-content:space-between;padding:14px 0;border-bottom:1px solid #e5eaf2}
a{color:#1959d1}.message{padding:14px 0;border-bottom:1px solid #e5eaf2}.message header{display:flex;gap:12px;color:#5c6b80;font-size:12px}
.message header{flex-wrap:wrap}.message strong{color:#10213c}.message p{white-space:pre-wrap;word-break:break-word;line-height:1.7;margin:8px 0}
.media{display:block;color:#5c6b80;word-break:break-all}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(format: ExportFormat, simplify: bool, pretty: bool) -> ExportPackage {
        let sent_at = "2026-09-16T08:30:00Z".parse::<DateTime<Utc>>().unwrap();
        let message = MessageV1 {
            schema_version: archive_domain::MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "fixture".into(),
            source_instance_id: "internal-source".into(),
            source_message_id: "internal-source-message".into(),
            stable_message_id: "internal-message".into(),
            conversation_id: "internal-conversation".into(),
            sender_id: Some("internal-sender".into()),
            sent_at,
            direction: MessageDirection::Incoming,
            message_type: archive_domain::MessageType::Text,
            body_text: Some("=1+1\n普通消息提及群公告 <script>".into()),
            quoted_message_id: Some("internal-quote".into()),
            lifecycle: archive_domain::LifecycleState::Active,
            media: vec![archive_domain::MediaRefV1 {
                content_hash: Some("internal-hash".into()),
                original_name: Some("@附件.txt".into()),
                mime_type: None,
                size_bytes: Some(1),
                source_locator: "internal-location".into(),
                integrity: MediaIntegrity::Missing,
                missing_reason: Some("internal-reason".into()),
            }],
            raw_type: "text".into(),
            raw_payload: serde_json::json!({"members": ["internal-member"]}),
            parser_version: "fixture.v1".into(),
            collection_batch_id: Uuid::nil(),
        };
        let mut announcement = message.clone();
        announcement.raw_type = "group_announcement".into();
        announcement.body_text = Some("仅群公告内容".into());
        let mut board = announcement.clone();
        board.raw_type = "group_board".into();
        board.body_text = Some("仅群看板内容".into());
        ExportPackage {
            archive_id: "fixture".into(),
            scope: ExportScope::EntireArchive,
            format,
            messages: vec![message, announcement, board],
            participants: vec![ExportParticipant {
                participant_id: "internal-sender".into(),
                display_name: Some("+测试成员".into()),
                participant_kind: None,
            }],
            conversation_names: BTreeMap::from([(
                "internal-conversation".into(),
                "=测试会话".into(),
            )]),
            simplify,
            pretty,
            media: vec![],
            generated_at: sent_at,
            edge_executable: None,
        }
    }

    #[test]
    fn json_options_preserve_content_and_remove_only_non_core_information() {
        let directory = tempfile::tempdir().unwrap();
        for simplify in [false, true] {
            let mut documents = Vec::new();
            for pretty in [false, true] {
                let package = fixture(ExportFormat::Json, simplify, pretty);
                let path = directory.path().join(format!("{simplify}-{pretty}.json"));
                let result = export_package(&package, &path, &AtomicBool::new(false)).unwrap();
                let text = fs::read_to_string(&path).unwrap();
                let document: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(text.contains('\n'), pretty);
                assert_eq!(result.message_count, if simplify { 1 } else { 3 });
                assert_eq!(result.media_count, result.message_count);
                assert_eq!(result.missing_media_count, result.message_count);
                if simplify {
                    assert!(!text.contains("internal-"));
                    assert!(!text.contains("仅群"));
                    assert_eq!(document[0]["sender"], "+测试成员");
                    assert_eq!(document[0]["conversation"], "=测试会话");
                    assert_eq!(
                        document[0]["content"],
                        package.messages[0].body_text.as_deref().unwrap()
                    );
                    assert_eq!(document[0]["media"][0], "@附件.txt");
                    assert_eq!(document[0].as_object().unwrap().len(), 6);
                    let positions = [
                        "conversation",
                        "sender",
                        "content",
                        "media",
                        "message_type",
                        "sent_at",
                    ]
                    .map(|key| text.find(&format!("\"{key}\":")).unwrap());
                    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
                } else {
                    let restored: Vec<MessageV1> = serde_json::from_str(&text).unwrap();
                    assert_eq!(restored, package.messages);
                }
                assert!(!partial_path(&path).unwrap().exists());
                assert!(matches!(
                    export_package(&package, &path, &AtomicBool::new(false)),
                    Err(ExportError::TargetExists)
                ));
                assert_eq!(fs::read_to_string(path).unwrap(), text);
                documents.push(document);
            }
            assert_eq!(documents[0], documents[1]);
        }
    }

    #[test]
    fn simplified_csv_escapes_names_and_preserves_required_record_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        for simplify in [false, true] {
            for pretty in [false, true] {
                let package = fixture(ExportFormat::Csv, simplify, pretty);
                let path = directory.path().join(format!("{simplify}-{pretty}.csv"));
                export_package(&package, &path, &AtomicBool::new(false)).unwrap();
                let text = fs::read_to_string(path).unwrap();
                let mut reader =
                    csv::Reader::from_reader(text.trim_start_matches('\u{feff}').as_bytes());
                let headers = reader.headers().unwrap().clone();
                let rows = reader.records().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows.len(), if simplify { 1 } else { 3 });
                if simplify {
                    assert!(!text.contains("internal-"));
                    assert_eq!(&rows[0][0], "'=测试会话");
                    assert_eq!(&rows[0][1], "'+测试成员");
                    assert_eq!(&rows[0][2], "'=1+1\n普通消息提及群公告 <script>");
                    assert_eq!(&rows[0][3], "'@附件.txt");
                    assert_eq!(&headers[0], if pretty { "会话" } else { "conversation" });
                    assert_eq!(&rows[0][4], if pretty { "文本" } else { "text" });
                } else {
                    assert_eq!(&rows[0][0], "internal-message");
                    assert_eq!(
                        &headers[0],
                        if pretty {
                            "消息 ID"
                        } else {
                            "stable_message_id"
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn html_and_pdf_source_use_selected_layout_and_escape_content() {
        let directory = tempfile::tempdir().unwrap();
        for simplify in [false, true] {
            for pretty in [false, true] {
                for for_print in [false, true] {
                    let package = fixture(ExportFormat::Html, simplify, pretty);
                    let path = directory.path().join("render.html");
                    write_html(&path, &package, &AtomicBool::new(false), for_print).unwrap();
                    let html = fs::read_to_string(path).unwrap();
                    assert!(html.contains("&lt;script&gt;"));
                    assert!(!html.contains("<script>"));
                    assert!(html.contains("default-src 'none'"));
                    assert_eq!(html.contains("max-width:980px"), pretty);
                    assert_eq!(html.contains("@page"), for_print);
                    assert_eq!(html.contains("internal-sender"), !simplify);
                    assert_eq!(html.contains("仅群公告内容"), !simplify);
                    assert!(html.contains("普通消息提及群公告"));
                    if simplify {
                        assert!(html.contains("+测试成员"));
                    }
                }
            }
        }
    }

    #[test]
    fn missing_display_names_never_fall_back_to_ids_in_simplified_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut package = fixture(ExportFormat::Json, true, false);
        package.participants[0].display_name = Some("internal-sender".into());
        package.conversation_names.insert(
            "internal-conversation".into(),
            "internal-conversation".into(),
        );
        let path = directory.path().join("unknown.json");
        export_package(&package, &path, &AtomicBool::new(false)).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("internal-"));
        assert!(text.contains("未知联系人"));
        assert!(text.contains("未命名会话"));
    }

    #[test]
    fn empty_and_cancelled_exports_leave_no_partial_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut package = fixture(ExportFormat::Json, true, false);
        package.messages.clear();
        let empty = directory.path().join("empty.json");
        assert_eq!(
            export_package(&package, &empty, &AtomicBool::new(false))
                .unwrap()
                .message_count,
            0
        );
        assert_eq!(fs::read_to_string(empty).unwrap(), "[]");
        let cancelled = directory.path().join("cancelled.json");
        assert!(matches!(
            export_package(&package, &cancelled, &AtomicBool::new(true)),
            Err(ExportError::Cancelled)
        ));
        assert!(!cancelled.exists());
        assert!(!partial_path(&cancelled).unwrap().exists());
    }

    #[test]
    fn csv_formula_prefix_is_neutralized() {
        assert_eq!(safe_csv("=2+2"), "'=2+2");
        assert_eq!(safe_csv("  @SUM(A1)"), "'  @SUM(A1)");
        assert_eq!(safe_csv("普通文本"), "普通文本");
    }

    #[test]
    fn html_content_is_escaped() {
        assert_eq!(
            html_escape("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }
}
