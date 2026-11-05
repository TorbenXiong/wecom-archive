use super::ArchiveExportSource;
use archive_domain::{ArchiveBatchV1, ConversationV1, MessageV1, ParticipantV1};
use archive_export::{ExportError, ExportResult, ExportSource};
use archive_store::ArchiveStore;
use serde::ser::{Error as _, SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

// Spool only this export's batch streams. Handles close before this guard runs.
// Message memory stays bounded even when one original batch spans the archive.
struct SpoolFiles(Vec<PathBuf>);
impl Drop for SpoolFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

struct Batch {
    metadata: ArchiveBatchV1,
    path: PathBuf,
}
struct Batches<'a> {
    rows: &'a [Batch],
    cancelled: &'a AtomicBool,
}
struct BatchMessages<'a> {
    path: &'a Path,
    cancelled: &'a AtomicBool,
}
impl Serialize for BatchMessages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let input = File::open(self.path).map_err(S::Error::custom)?;
        let mut sequence = serializer.serialize_seq(None)?;
        for message in
            serde_json::Deserializer::from_reader(BufReader::new(input)).into_iter::<MessageV1>()
        {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(S::Error::custom("EXPORT_CANCELLED"));
            }
            sequence.serialize_element(&message.map_err(S::Error::custom)?)?;
        }
        sequence.end()
    }
}
struct BatchDocument<'a> {
    batch: &'a Batch,
    cancelled: &'a AtomicBool,
}
impl Serialize for BatchDocument<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let b = &self.batch.metadata;
        // Field order is the unchanged ArchiveBatchV1 order used by its checksum.
        let mut output = serializer.serialize_struct("ArchiveBatchV1", 13)?;
        output.serialize_field("schema_version", &b.schema_version)?;
        output.serialize_field("batch_id", &b.batch_id)?;
        output.serialize_field("source_adapter", &b.source_adapter)?;
        output.serialize_field("source_instance_id", &b.source_instance_id)?;
        output.serialize_field("collected_at", &b.collected_at)?;
        output.serialize_field("employee_notice", &b.employee_notice)?;
        output.serialize_field("external_contact_consent", &b.external_contact_consent)?;
        output.serialize_field("collection_scope", &b.collection_scope)?;
        output.serialize_field("cursor_before", &b.cursor_before)?;
        output.serialize_field("cursor_after", &b.cursor_after)?;
        output.serialize_field(
            "messages",
            &BatchMessages {
                path: &self.batch.path,
                cancelled: self.cancelled,
            },
        )?;
        output.serialize_field("content_sha256", &b.content_sha256)?;
        output.serialize_field("retention", &b.retention)?;
        output.end()
    }
}
impl Serialize for Batches<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.rows.len()))?;
        for batch in self.rows {
            sequence.serialize_element(&BatchDocument {
                batch,
                cancelled: self.cancelled,
            })?;
        }
        sequence.end()
    }
}

struct Conversations<'a> {
    rows: &'a [ConversationV1],
    source: &'a ArchiveExportSource<'a>,
    cancelled: &'a AtomicBool,
}
struct ConversationMessages<'a> {
    id: &'a str,
    source: &'a ArchiveExportSource<'a>,
    cancelled: &'a AtomicBool,
}
impl Serialize for ConversationMessages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        self.source
            .visit_messages(self.id, &mut |message| {
                check_cancelled(self.cancelled)?;
                let mut message = message.clone();
                archive_transfer::sanitize_export_message(&mut message);
                sequence
                    .serialize_element(&message)
                    .map_err(|_| std::io::Error::other("JSON write failed").into())
            })
            .map_err(S::Error::custom)?;
        sequence.end()
    }
}
impl Serialize for Conversations<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Conversation<'a> {
            #[serde(flatten)]
            directory: &'a ConversationV1,
            conversation_name: &'a str,
            messages: ConversationMessages<'a>,
        }
        let mut sequence = serializer.serialize_seq(Some(self.rows.len()))?;
        for row in self.rows {
            sequence.serialize_element(&Conversation {
                directory: row,
                conversation_name: row.display_name.as_deref().unwrap_or("未命名会话"),
                messages: ConversationMessages {
                    id: &row.conversation_id,
                    source: self.source,
                    cancelled: self.cancelled,
                },
            })?;
        }
        sequence.end()
    }
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ExportError> {
    if cancelled.load(Ordering::Acquire) {
        Err(ExportError::Cancelled)
    } else {
        Ok(())
    }
}

/// Adds the existing import contract while retaining conversations[].messages.
/// Media remains metadata-only, as in the other server archive export formats.
pub(crate) fn export_importable_json(
    store: &ArchiveStore,
    source: &ArchiveExportSource<'_>,
    target: &Path,
    pretty: bool,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str),
) -> Result<ExportResult, ExportError> {
    if target.exists() {
        return Err(ExportError::TargetExists);
    }
    check_cancelled(cancelled)?;
    let parent = target.parent().ok_or(ExportError::UnsafePath)?;
    let mut cleanup = SpoolFiles(Vec::new());
    let mut batches = Vec::<Batch>::new();
    let mut batch_indexes = BTreeMap::new();
    let mut writers = Vec::<BufWriter<File>>::new();
    let mut conversations = Vec::new();
    let mut participants = BTreeMap::<String, ParticipantV1>::new();
    let mut done = 0;
    let mut media_count = 0;
    let mut missing_media_count = 0;
    let statistics = source.statistics();
    let store_error = |_| std::io::Error::other("archive export metadata read failed");
    source.visit_conversations(&mut |id| {
        check_cancelled(cancelled)?;
        conversations.push(store.conversation_directory(id).map_err(store_error)?);
        for row in store
            .list_conversation_participants(id)
            .map_err(store_error)?
        {
            participants.insert(
                row.participant_id.clone(),
                ParticipantV1 {
                    participant_id: row.participant_id,
                    display_name: row.display_name,
                    participant_kind: row.participant_kind,
                },
            );
        }
        source.visit_messages(id, &mut |message| {
            check_cancelled(cancelled)?;
            let batch_id = message.collection_batch_id;
            let index = if let Some(index) = batch_indexes.get(&batch_id) {
                *index
            } else {
                let metadata = store
                    .collection_batch_metadata(batch_id)
                    .map_err(store_error)?;
                let path = parent.join(format!(".{}.batch.partial", Uuid::new_v4()));
                let file = File::options().write(true).create_new(true).open(&path)?;
                cleanup.0.push(path.clone());
                let index = batches.len();
                batches.push(Batch { metadata, path });
                writers.push(BufWriter::new(file));
                batch_indexes.insert(batch_id, index);
                index
            };
            let mut message = message.clone();
            archive_transfer::sanitize_export_message(&mut message);
            message
                .validate()
                .map_err(|_| std::io::Error::other("invalid archive message"))?;
            media_count += message.media.len() as u64;
            missing_media_count += message
                .media
                .iter()
                .filter(|m| m.content_hash.is_none())
                .count() as u64;
            serde_json::to_writer(&mut writers[index], &message)?;
            writers[index].write_all(b"\n")?;
            done += 1;
            if done % 128 == 0 {
                progress(done, statistics.message_count, "writing");
            }
            Ok(())
        })
    })?;
    for writer in &mut writers {
        writer.flush()?;
    }
    drop(writers);
    if done != statistics.message_count || media_count != statistics.media_count {
        return Err(std::io::Error::other("archive export counts changed").into());
    }
    let participants: Vec<_> = participants.into_values().collect();
    let batches = Batches {
        rows: &batches,
        cancelled,
    };
    let mut checksum = HashWriter(Sha256::new());
    let result = (|| {
        serde_json::to_writer(&mut checksum, &(&conversations, &participants, &batches))?;
        let checksum = crate::encode_hex(&checksum.0.finalize());
        let export_id = Uuid::new_v4();
        #[derive(Serialize)]
        struct Document<'a> {
            schema_version: &'static str,
            export_id: Uuid,
            generated_at: chrono::DateTime<chrono::Utc>,
            client_version: &'static str,
            media_transport: &'static str,
            media_blobs: Vec<archive_domain::MediaBlobV1>,
            conversations: Conversations<'a>,
            participants: &'a [ParticipantV1],
            batches: Batches<'a>,
            message_count: u64,
            media_count: u64,
            missing_media_count: u64,
            content_sha256: String,
        }
        let document = Document {
            schema_version: archive_domain::CLIENT_EXPORT_SCHEMA_VERSION,
            export_id,
            generated_at: chrono::Utc::now(),
            client_version: env!("CARGO_PKG_VERSION"),
            media_transport: "metadata_only",
            media_blobs: Vec::new(),
            conversations: Conversations {
                rows: &conversations,
                source,
                cancelled,
            },
            participants: &participants,
            batches,
            message_count: done,
            media_count,
            missing_media_count,
            content_sha256: checksum,
        };
        let partial = target.with_file_name(format!(
            "{}.partial",
            target
                .file_name()
                .ok_or(ExportError::UnsafePath)?
                .to_string_lossy()
        ));
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        cleanup.0.push(partial.clone());
        let mut writer = BufWriter::new(file);
        if pretty {
            serde_json::to_writer_pretty(&mut writer, &document)?;
        } else {
            serde_json::to_writer(&mut writer, &document)?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        progress(done, statistics.message_count, "finalizing");
        let manifest_sha256 = crate::sha256_file(&partial)?;
        check_cancelled(cancelled)?;
        match fs::hard_link(&partial, target) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ExportError::TargetExists);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(ExportResult {
            export_id,
            target: target.into(),
            message_count: done,
            media_count,
            missing_media_count,
            manifest_sha256,
        })
    })();
    if cancelled.load(Ordering::Acquire) && result.is_err() {
        Err(ExportError::Cancelled)
    } else {
        result
    }
}
