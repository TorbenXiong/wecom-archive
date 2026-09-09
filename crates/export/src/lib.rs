use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use archive_domain::{MediaIntegrity, MessageV1};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use thiserror::Error;
use uuid::Uuid;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;

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

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    schema_version: &'static str,
    export_id: Uuid,
    archive_id: &'a str,
    scope: ExportScope,
    format: ExportFormat,
    generated_at: DateTime<Utc>,
    message_count: usize,
    participant_count: usize,
    media_count: usize,
    missing_media_count: usize,
    conversation_count: usize,
}

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("target already exists")]
    TargetExists,
    #[error("export was cancelled")]
    Cancelled,
    #[error("unsafe archive path")]
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
    #[error("ZIP generation failed: {0}")]
    Zip(#[from] zip::result::ZipError),
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
        let staging = tempfile::tempdir_in(parent)?;
        check_cancelled(cancelled)?;
        render_payload(package, &staging, cancelled)?;
        copy_media(package, &staging, cancelled)?;

        let export_id = Uuid::new_v4();
        let conversations = package
            .messages
            .iter()
            .map(|message| message.conversation_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let missing_media_count = package
            .media
            .iter()
            .filter(|media| media.integrity != MediaIntegrity::Verified)
            .count();
        let manifest = Manifest {
            schema_version: "export-manifest.v1",
            export_id,
            archive_id: &package.archive_id,
            scope: package.scope,
            format: package.format,
            generated_at: package.generated_at,
            message_count: package.messages.len(),
            participant_count: package.participants.len(),
            media_count: package.media.len(),
            missing_media_count,
            conversation_count: conversations,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(staging.path().join("manifest.json"), &manifest_bytes)?;
        write_checksums(staging.path())?;
        check_cancelled(cancelled)?;
        write_zip(staging.path(), &partial, cancelled)?;
        fs::rename(&partial, target)?;
        Ok(ExportResult {
            export_id,
            target: target.to_path_buf(),
            message_count: package.messages.len() as u64,
            media_count: package.media.len() as u64,
            missing_media_count: missing_media_count as u64,
            manifest_sha256: hex::encode(Sha256::digest(&manifest_bytes)),
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn render_payload(
    package: &ExportPackage,
    staging: &TempDir,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    match package.format {
        ExportFormat::Json => write_json(staging.path(), &package.messages, cancelled),
        ExportFormat::Csv => write_csv(staging.path(), package, cancelled),
        ExportFormat::Html => write_html(staging.path(), package, cancelled, false),
        ExportFormat::Pdf => {
            write_html(staging.path(), package, cancelled, true)?;
            render_pdfs(staging.path(), package, cancelled)
        }
    }
}

fn write_json(
    root: &Path,
    messages: &[MessageV1],
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut output = BufWriter::new(File::create(root.join("messages.json"))?);
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
    root: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let mut messages = csv_writer(root.join("messages.csv"))?;
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
    ])?;
    for message in &package.messages {
        check_cancelled(cancelled)?;
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
        ])?;
    }
    messages.flush()?;

    let mut participants = csv_writer(root.join("participants.csv"))?;
    participants.write_record(["participant_id", "display_name", "participant_kind"])?;
    for participant in &package.participants {
        participants.write_record([
            safe_csv(&participant.participant_id),
            safe_csv(participant.display_name.as_deref().unwrap_or_default()),
            safe_csv(participant.participant_kind.as_deref().unwrap_or_default()),
        ])?;
    }
    participants.flush()?;

    let mut media = csv_writer(root.join("media.csv"))?;
    media.write_record([
        "content_sha256",
        "original_name",
        "mime_type",
        "size_bytes",
        "integrity",
        "missing_reason",
    ])?;
    for item in &package.media {
        media.write_record([
            safe_csv(item.content_sha256.as_deref().unwrap_or_default()),
            safe_csv(item.original_name.as_deref().unwrap_or_default()),
            safe_csv(item.mime_type.as_deref().unwrap_or_default()),
            item.size_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            enum_text(&item.integrity)?,
            safe_csv(item.missing_reason.as_deref().unwrap_or_default()),
        ])?;
    }
    media.flush()?;
    Ok(())
}

fn csv_writer(path: PathBuf) -> Result<csv::Writer<File>, ExportError> {
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
    root: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
    for_print: bool,
) -> Result<(), ExportError> {
    let session_root = root.join(if for_print { "print" } else { "sessions" });
    fs::create_dir_all(&session_root)?;
    let mut grouped: BTreeMap<&str, Vec<&MessageV1>> = BTreeMap::new();
    for message in &package.messages {
        grouped
            .entry(&message.conversation_id)
            .or_default()
            .push(message);
    }

    let mut index_items = String::new();
    for (conversation, messages) in grouped {
        for (part_index, chunk) in messages.chunks(5_000).enumerate() {
            check_cancelled(cancelled)?;
            let slug = short_hash(conversation);
            let name = format!("session-{slug}-{:03}.html", part_index + 1);
            let mut rows = String::new();
            for message in chunk {
                let sender = html_escape(message.sender_id.as_deref().unwrap_or("系统"));
                let body =
                    html_escape(message.body_text.as_deref().unwrap_or("[不支持的消息类型]"));
                rows.push_str(&format!(
                    "<article class=\"message\"><header><time>{}</time><strong>{sender}</strong><span>{}</span></header><p>{body}</p></article>",
                    html_escape(&message.sent_at.to_rfc3339()),
                    html_escape(&enum_text(&message.message_type)?),
                ));
            }
            let title = format!(
                "会话 {} · 第 {} 册",
                html_escape(conversation),
                part_index + 1
            );
            fs::write(
                session_root.join(&name),
                html_document(&title, &rows, for_print),
            )?;
            index_items.push_str(&format!(
                "<li><a href=\"{}/{}\">{}</a><span>{} 条消息</span></li>",
                if for_print { "print" } else { "sessions" },
                name,
                title,
                chunk.len()
            ));
        }
    }

    let index = format!(
        "<section class=\"summary\"><p>{} 条消息 · {} 位参与人 · {} 个媒体记录</p></section><ol class=\"session-list\">{index_items}</ol>",
        package.messages.len(),
        package.participants.len(),
        package.media.len()
    );
    fs::write(
        root.join("index.html"),
        html_document("归档导出索引", &index, for_print),
    )?;
    Ok(())
}

fn html_document(title: &str, body: &str, for_print: bool) -> String {
    let print_style = if for_print {
        "@page{size:A4;margin:18mm 16mm 20mm}@media print{a{color:inherit;text-decoration:none}.message{break-inside:avoid}}"
    } else {
        ""
    };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{BASE_CSS}{print_style}</style></head><body><main><h1>{title}</h1>{body}</main></body></html>"
    )
}

fn render_pdfs(
    root: &Path,
    package: &ExportPackage,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let edge = package
        .edge_executable
        .as_deref()
        .filter(|path| path.is_file())
        .ok_or(ExportError::PdfRendererUnavailable)?;
    let output_root = root.join("pdf");
    fs::create_dir_all(&output_root)?;
    let print_root = root.join("print");
    for entry in WalkDir::new(&print_root).min_depth(1).max_depth(1) {
        check_cancelled(cancelled)?;
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("html")
        {
            continue;
        }
        let output = output_root.join(
            entry
                .path()
                .with_extension("pdf")
                .file_name()
                .ok_or(ExportError::UnsafePath)?,
        );
        render_one_pdf(edge, entry.path(), &output)?;
    }
    render_one_pdf(
        edge,
        &root.join("index.html"),
        &output_root.join("index.pdf"),
    )?;
    fs::remove_dir_all(print_root)?;
    Ok(())
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

fn copy_media(
    package: &ExportPackage,
    staging: &TempDir,
    cancelled: &AtomicBool,
) -> Result<(), ExportError> {
    let media_root = staging.path().join("media");
    fs::create_dir_all(&media_root)?;
    let mut missing = BufWriter::new(File::create(staging.path().join("missing-media.jsonl"))?);
    for media in &package.media {
        check_cancelled(cancelled)?;
        match (&media.content_sha256, &media.source_path, &media.integrity) {
            (Some(hash), Some(source), MediaIntegrity::Verified) if source.is_file() => {
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(ExportError::UnsafePath);
                }
                let extension = source
                    .extension()
                    .and_then(|value| value.to_str())
                    .filter(|value| {
                        value.len() <= 10 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    })
                    .map(|value| format!(".{}", value.to_ascii_lowercase()))
                    .unwrap_or_default();
                let target = media_root.join(format!("{hash}{extension}"));
                if !target.exists() {
                    fs::copy(source, target)?;
                }
            }
            _ => {
                serde_json::to_writer(&mut missing, media)?;
                missing.write_all(b"\n")?;
            }
        }
    }
    missing.flush()?;
    Ok(())
}

fn write_checksums(root: &Path) -> Result<(), ExportError> {
    let target = root.join("checksums.sha256");
    let mut records = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if !entry.file_type().is_file() || entry.path() == target {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| ExportError::UnsafePath)?;
        validate_relative(relative)?;
        records.push((relative.to_path_buf(), hash_file(entry.path())?));
    }
    records.sort_by(|left, right| left.0.cmp(&right.0));
    let mut output = BufWriter::new(File::create(target)?);
    for (path, hash) in records {
        writeln!(
            output,
            "{hash}  {}",
            path.to_string_lossy().replace('\\', "/")
        )?;
    }
    Ok(())
}

fn write_zip(root: &Path, partial: &Path, cancelled: &AtomicBool) -> Result<(), ExportError> {
    let file = File::create(partial)?;
    let mut zip = zip::ZipWriter::new(BufWriter::new(file));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::other(error.to_string()))?;
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in entries {
        check_cancelled(cancelled)?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| ExportError::UnsafePath)?;
        validate_relative(relative)?;
        let name = relative.to_string_lossy().replace('\\', "/");
        zip.start_file(name, options)?;
        let mut input = File::open(entry.path())?;
        io::copy(&mut input, &mut zip)?;
    }
    zip.finish()?.flush()?;
    Ok(())
}

fn partial_path(target: &Path) -> Result<PathBuf, ExportError> {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ExportError::UnsafePath)?;
    Ok(target.with_file_name(format!("{name}.partial")))
}

fn validate_relative(path: &Path) -> Result<(), ExportError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ExportError::UnsafePath);
    }
    Ok(())
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
    Ok(hex::encode(hasher.finalize()))
}

fn short_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..16].to_owned()
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

    #[test]
    fn path_traversal_is_rejected() {
        assert!(matches!(
            validate_relative(Path::new("../secret")),
            Err(ExportError::UnsafePath)
        ));
    }
}
