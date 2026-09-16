use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use archive_domain::{MediaIntegrity, MessageV1};
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
        let missing_media_count = package
            .media
            .iter()
            .filter(|media| media.integrity != MediaIntegrity::Verified)
            .count();
        check_cancelled(cancelled)?;
        render_payload(package, &partial, cancelled)?;
        check_cancelled(cancelled)?;
        publish_without_overwrite(&partial, target)?;
        Ok(ExportResult {
            export_id,
            target: target.to_path_buf(),
            message_count: package.messages.len() as u64,
            media_count: package.media.len() as u64,
            missing_media_count: missing_media_count as u64,
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
        ExportFormat::Json => write_json(target, &package.messages, cancelled),
        ExportFormat::Csv => write_csv(target, package, cancelled),
        ExportFormat::Html => write_html(target, package, cancelled, false),
        ExportFormat::Pdf => write_pdf(target, package, cancelled),
    }
}

fn write_json(
    target: &Path,
    messages: &[MessageV1],
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut output = BufWriter::new(File::create(target)?);
    output.write_all(b"[\n")?;
    for (index, message) in messages.iter().enumerate() {
        check_cancelled(cancelled)?;
        if index > 0 {
            output.write_all(b",\n")?;
        }
        serde_json::to_writer(&mut output, message)?;
    }
    output.write_all(b"\n]\n")?;
    output.flush()?;
    Ok(())
}

fn write_csv(
    target: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut messages = csv_writer(target)?;
    messages.write_record([
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
    ])?;
    for message in &package.messages {
        check_cancelled(cancelled)?;
        let media_names = message
            .media
            .iter()
            .filter_map(|media| media.original_name.as_deref())
            .collect::<Vec<_>>()
            .join(" | ");
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
            message.sent_at.to_rfc3339(),
            enum_text(&message.direction)?,
            enum_text(&message.message_type)?,
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
    output.write_all(html_document_start("归档导出", for_print).as_bytes())?;
    write!(
        output,
        "<section class=\"summary\"><p>{} 条消息 · {} 位参与人 · {} 个媒体记录</p></section>",
        package.messages.len(),
        package.participants.len(),
        package.media.len()
    )?;
    for message in &package.messages {
        check_cancelled(cancelled)?;
        let sender = html_escape(message.sender_id.as_deref().unwrap_or("系统"));
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
            "<article class=\"message\"><header><time>{}</time><strong>{sender}</strong><span>{}</span><span>会话 {}</span></header><p>{body}</p>{}</article>",
            html_escape(&message.sent_at.to_rfc3339()),
            html_escape(&enum_text(&message.message_type)?),
            html_escape(&message.conversation_id),
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

fn html_document_start(title: &str, for_print: bool) -> String {
    let print_style = if for_print {
        "@page{size:A4;margin:18mm 16mm 20mm}@media print{a{color:inherit;text-decoration:none}.message{break-inside:avoid}}"
    } else {
        ""
    };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{BASE_CSS}{print_style}</style></head><body><main><h1>{title}</h1>"
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

const BASE_CSS: &str = r#"
:root{font-family:"Microsoft YaHei UI","Segoe UI",sans-serif;color:#10213c;background:#f5f7fb}
*{box-sizing:border-box}body{margin:0}main{max-width:980px;margin:0 auto;padding:36px;background:#fff;min-height:100vh}
h1{font-size:24px;margin:0 0 24px}.summary{padding:16px 20px;background:#eef4ff;border:1px solid #d7e4ff;border-radius:10px}
.session-list{padding:0;list-style:none}.session-list li{display:flex;justify-content:space-between;padding:14px 0;border-bottom:1px solid #e5eaf2}
a{color:#1959d1}.message{padding:14px 0;border-bottom:1px solid #e5eaf2}.message header{display:flex;gap:12px;color:#5c6b80;font-size:12px}
.message strong{color:#10213c}.message p{white-space:pre-wrap;word-break:break-word;line-height:1.7;margin:8px 0}
.media{display:block;color:#5c6b80;word-break:break-all}
"#;

#[cfg(test)]
mod tests {
    use super::*;

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
