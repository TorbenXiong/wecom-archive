use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use archive_domain::ClientExportV1;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::collector_config::EnterpriseCollectorConfig;
use crate::state::{CommandError, internal_error};

const OVERLAP_MS: i64 = 5 * 60 * 1000;
const FULL_SCAN_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
// Contextual redaction considers the preceding message for at most ten minutes.
const REDACTION_CONTEXT_MS: i64 = 10 * 60 * 1000;

struct Checkpoint {
    scope: String,
    scan_started_ms: i64,
    full_scan_started_ms: i64,
    last_upload_at: String,
    uploaded_count: i64,
}

pub(super) struct IncrementalCollector {
    connection: Connection,
    scope: String,
    checkpoint: Option<Checkpoint>,
}

pub(super) struct ScanPlan {
    pub since_unix_ms: Option<i64>,
    read_since_unix_ms: Option<i64>,
}

impl ScanPlan {
    pub fn read_since_unix_ms(&self) -> Option<i64> {
        self.read_since_unix_ms
    }
}

#[derive(Debug)]
pub(super) struct UploadOutcome {
    pub completed_at: Option<String>,
    pub last_upload_at: String,
    pub uploaded_count: u64,
}

pub(super) fn progress_path(root: &Path, collector_id: &str, source_id: &str) -> PathBuf {
    // Never use an externally supplied identifier as a path component.
    root.join("instances")
        .join(
            uuid::Uuid::parse_str(collector_id)
                .map(|id| id.to_string())
                .unwrap_or_else(|_| hash_bytes(collector_id.as_bytes())),
        )
        .join(format!(
            "collection-progress-{}.db",
            hash_bytes(source_id.as_bytes())
        ))
}

pub(super) fn scope_hash(config: &EnterpriseCollectorConfig) -> Result<String, CommandError> {
    hash_value(&(
        "collector-incremental.v1",
        env!("CARGO_PKG_VERSION"),
        &config.collector_id,
        &config.organization_id,
        &config.upload_url,
        &config.upload_token,
        &config.key_id,
        &config.public_key_hex,
        &config.signing_public_key_hex,
        &config.collection_notice,
        config.include_files,
        config.include_images,
        config.data_redaction,
    ))
}

impl IncrementalCollector {
    pub fn open(path: &Path, scope: String) -> Result<Self, CommandError> {
        Self::open_inner(path, scope).map_err(|_| progress_error())
    }

    fn open_inner(path: &Path, scope: String) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| rusqlite::Error::InvalidQuery)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.execute_batch(
            "PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS checkpoint (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 scope TEXT NOT NULL,
                 scan_started_ms INTEGER NOT NULL,
                 full_scan_started_ms INTEGER NOT NULL,
                 last_upload_at TEXT NOT NULL,
                 uploaded_count INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS fingerprints (
                 message_hash TEXT PRIMARY KEY,
                 content_hash TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS directory_fingerprints (
                 entry_hash TEXT PRIMARY KEY,
                 content_hash TEXT NOT NULL
             ) WITHOUT ROWID;",
        )?;
        let checkpoint = connection
            .query_row(
                "SELECT scope, scan_started_ms, full_scan_started_ms, last_upload_at,
                        uploaded_count FROM checkpoint WHERE id = 1",
                [],
                |row| {
                    Ok(Checkpoint {
                        scope: row.get(0)?,
                        scan_started_ms: row.get(1)?,
                        full_scan_started_ms: row.get(2)?,
                        last_upload_at: row.get(3)?,
                        uploaded_count: row.get(4)?,
                    })
                },
            )
            .optional()?
            .filter(|checkpoint| checkpoint.scope == scope);
        Ok(Self {
            connection,
            scope,
            checkpoint,
        })
    }

    pub fn plan(&self, started_ms: i64, force_full: bool, redact: bool) -> ScanPlan {
        let since_unix_ms = self.checkpoint.as_ref().and_then(|checkpoint| {
            (!force_full
                && started_ms >= checkpoint.scan_started_ms
                && started_ms.saturating_sub(checkpoint.full_scan_started_ms)
                    < FULL_SCAN_INTERVAL_MS)
                .then(|| checkpoint.scan_started_ms.saturating_sub(OVERLAP_MS))
        });
        ScanPlan {
            since_unix_ms,
            read_since_unix_ms: since_unix_ms.map(|since| {
                if redact {
                    since.saturating_sub(REDACTION_CONTEXT_MS)
                } else {
                    since
                }
            }),
        }
    }

    /// The caller supplies the complete redaction window. Filter only after
    /// redaction, so an unchanged context message can still protect a new one.
    pub fn upload(
        &mut self,
        mut export: ClientExportV1,
        plan: &ScanPlan,
        started_ms: i64,
        force_upload: bool,
        upload: impl FnOnce(&ClientExportV1) -> Result<String, CommandError>,
    ) -> Result<UploadOutcome, CommandError> {
        let eligible_conversations = export
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .filter(|message| {
                plan.since_unix_ms
                    .is_none_or(|since| message.sent_at.timestamp_millis() >= since)
                    || message.raw_type == "group_announcement"
            })
            .map(|message| message.conversation_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut profiles = export
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .filter(|message| eligible_conversations.contains(message.conversation_id.as_str()))
            .filter_map(|message| {
                message
                    .raw_payload
                    .get("participant_profiles")
                    .map(|profiles| (message.conversation_id.clone(), profiles.clone()))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut directory_changes = Vec::new();
        let mut changed_profiles = BTreeSet::new();
        {
            let mut lookup = self
                .connection
                .prepare_cached(
                    "SELECT content_hash FROM directory_fingerprints WHERE entry_hash = ?1",
                )
                .map_err(|_| progress_error())?;
            let mut compare =
                |kind: &str, id: &str, content_hash: String| -> Result<bool, CommandError> {
                    let entry_hash = hash_value(&(kind, id))?;
                    let previous: Option<String> = if self.checkpoint.is_some() {
                        lookup
                            .query_row([&entry_hash], |row| row.get(0))
                            .optional()
                            .map_err(|_| progress_error())?
                    } else {
                        None
                    };
                    let changed = previous.as_deref() != Some(content_hash.as_str());
                    if changed {
                        directory_changes.push((entry_hash, content_hash));
                    }
                    Ok(changed)
                };
            for conversation in &export.conversations {
                compare(
                    "conversation",
                    &conversation.conversation_id,
                    hash_value(conversation)?,
                )?;
            }
            for participant in &export.participants {
                compare(
                    "participant",
                    &participant.participant_id,
                    hash_value(participant)?,
                )?;
            }
            for (conversation, profile) in &profiles {
                if compare("profile", conversation, hash_value(profile)?)? {
                    changed_profiles.insert(conversation.clone());
                }
            }
        }
        let mut changes = Vec::new();
        let mut uploaded_count = self
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.uploaded_count);
        let mut lookup = self
            .connection
            .prepare_cached("SELECT content_hash FROM fingerprints WHERE message_hash = ?1")
            .map_err(|_| progress_error())?;
        for batch in &mut export.batches {
            let mut retained = Vec::new();
            for mut message in std::mem::take(&mut batch.messages) {
                if plan
                    .since_unix_ms
                    .is_some_and(|since| message.sent_at.timestamp_millis() < since)
                    && message.raw_type != "group_announcement"
                {
                    continue;
                }
                if let Some(payload) = message.raw_payload.as_object_mut() {
                    payload.remove("participant_profiles");
                    if let Some(profile) = profiles.remove(&message.conversation_id) {
                        payload.insert("participant_profiles".into(), profile);
                    }
                }
                let message_hash = hash_bytes(message.stable_message_id.as_bytes());
                let batch_id = message.collection_batch_id;
                message.collection_batch_id = uuid::Uuid::nil();
                // The parser carries profiles on the first message in each
                // scanned conversation. Window movement must not turn that
                // incidental carrier choice into a message revision.
                let profile = message
                    .raw_payload
                    .as_object_mut()
                    .and_then(|payload| payload.remove("participant_profiles"));
                let content_hash = hash_value(&message)?;
                let carries_profile = profile.is_some();
                if let Some(profile) = profile {
                    message
                        .raw_payload
                        .as_object_mut()
                        .ok_or_else(internal_error)?
                        .insert("participant_profiles".into(), profile);
                }
                message.collection_batch_id = batch_id;
                let previous: Option<String> = if self.checkpoint.is_some() {
                    lookup
                        .query_row([&message_hash], |row| row.get(0))
                        .optional()
                        .map_err(|_| progress_error())?
                } else {
                    None
                };
                if previous.as_deref() != Some(content_hash.as_str()) {
                    if previous.is_none() {
                        uploaded_count += 1;
                    }
                    changes.push((message_hash, content_hash));
                    retained.push(message);
                } else if carries_profile && changed_profiles.contains(&message.conversation_id) {
                    retained.push(message);
                }
            }
            batch.messages = retained;
            batch.content_sha256 = hash_value(&batch.messages)?;
        }
        drop(lookup);
        let referenced_media = export
            .batches
            .iter()
            .flat_map(|batch| &batch.messages)
            .flat_map(|message| &message.media)
            .filter_map(|media| media.content_hash.as_deref())
            .collect::<BTreeSet<_>>();
        export
            .media_blobs
            .retain(|blob| referenced_media.contains(blob.content_hash.as_str()));
        let needs_upload = force_upload
            || self.checkpoint.is_none()
            || !directory_changes.is_empty()
            || !changes.is_empty();
        let export = archive_transfer::create_export_with_media(
            &export.client_version,
            export.batches,
            export.conversations,
            export.participants,
            export.media_blobs,
        )
        .map_err(|_| internal_error())?;
        let completed_at = if needs_upload {
            Some(upload(&export)?)
        } else {
            None
        };
        let checkpoint = Checkpoint {
            scope: self.scope.clone(),
            scan_started_ms: started_ms,
            full_scan_started_ms: if plan.since_unix_ms.is_none() {
                started_ms
            } else {
                self.checkpoint
                    .as_ref()
                    .ok_or_else(progress_error)?
                    .full_scan_started_ms
            },
            last_upload_at: completed_at
                .clone()
                .or_else(|| {
                    self.checkpoint
                        .as_ref()
                        .map(|checkpoint| checkpoint.last_upload_at.clone())
                })
                .ok_or_else(progress_error)?,
            uploaded_count,
        };
        // A crash or failed save must never leave advanced progress alongside
        // incomplete fingerprints. A lost upload acknowledgement safely replays.
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| progress_error())?;
        if self.checkpoint.is_none() {
            transaction
                .execute("DELETE FROM fingerprints", [])
                .map_err(|_| progress_error())?;
            transaction
                .execute("DELETE FROM directory_fingerprints", [])
                .map_err(|_| progress_error())?;
        }
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO directory_fingerprints (entry_hash, content_hash) VALUES (?1, ?2)
                 ON CONFLICT(entry_hash) DO UPDATE SET content_hash = excluded.content_hash",
                )
                .map_err(|_| progress_error())?;
            for (entry, content) in directory_changes {
                insert
                    .execute(params![entry, content])
                    .map_err(|_| progress_error())?;
            }
        }
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO fingerprints (message_hash, content_hash) VALUES (?1, ?2)
                 ON CONFLICT(message_hash) DO UPDATE SET content_hash = excluded.content_hash",
                )
                .map_err(|_| progress_error())?;
            for (message, content) in changes {
                insert
                    .execute(params![message, content])
                    .map_err(|_| progress_error())?;
            }
        }
        transaction
            .execute(
                "INSERT OR REPLACE INTO checkpoint VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    checkpoint.scope,
                    checkpoint.scan_started_ms,
                    checkpoint.full_scan_started_ms,
                    checkpoint.last_upload_at,
                    checkpoint.uploaded_count
                ],
            )
            .map_err(|_| progress_error())?;
        transaction.commit().map_err(|_| progress_error())?;
        let last_upload_at = checkpoint.last_upload_at.clone();
        self.checkpoint = Some(checkpoint);
        Ok(UploadOutcome {
            completed_at,
            last_upload_at,
            uploaded_count: uploaded_count as u64,
        })
    }
}

fn hash_value(value: &impl Serialize) -> Result<String, CommandError> {
    archive_transfer::json_sha256(value).map_err(|_| internal_error())
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn progress_error() -> CommandError {
    CommandError {
        code: "COLLECTION_PROGRESS_FAILED",
        message: "采集进度读取或保存失败；原进度不会前移，请检查采集端目录权限后重试。".into(),
        recoverable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_domain::{
        ArchiveBatchV1, CollectionScope, EmployeeNoticeEvidence, ExternalContactConsent,
        RetentionDirective,
    };
    use chrono::{TimeZone, Utc};
    use message_parser::{RawMessageRow, normalize};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "collector-incremental-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self) -> PathBuf {
            self.0.join("progress.db")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn export(rows: &[(&str, i64, &str)]) -> ClientExportV1 {
        let batch_id = uuid::Uuid::new_v4();
        let messages = rows
            .iter()
            .map(|(id, timestamp, text)| {
                normalize(
                    RawMessageRow {
                        source_instance_id: "synthetic-source".into(),
                        source_message_id: (*id).into(),
                        conversation_id: "synthetic-conversation".into(),
                        sender_id: None,
                        sent_at_unix_ms: *timestamp,
                        outgoing: None,
                        raw_type: "text".into(),
                        payload: serde_json::json!({"text": text}),
                        recalled: false,
                    },
                    batch_id,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let batch = ArchiveBatchV1 {
            schema_version: archive_domain::BATCH_SCHEMA_VERSION.into(),
            batch_id,
            source_adapter: "synthetic-adapter".into(),
            source_instance_id: "synthetic-source".into(),
            collected_at: Utc::now(),
            employee_notice: EmployeeNoticeEvidence {
                notice_version: "synthetic-notice".into(),
                displayed_at: Utc::now(),
                acknowledged_at: None,
                evidence_hash: "synthetic-evidence".into(),
            },
            external_contact_consent: ExternalContactConsent::Unknown,
            collection_scope: CollectionScope {
                source_ids: vec!["synthetic-source".into()],
                conversation_ids: vec!["synthetic-conversation".into()],
                starts_at: None,
                ends_at: None,
                message_types: vec![],
            },
            cursor_before: None,
            cursor_after: None,
            content_sha256: hash_value(&messages).unwrap(),
            messages,
            retention: RetentionDirective {
                policy_id: None,
                expires_at: None,
                legal_hold: false,
            },
        };
        archive_transfer::create_export(
            "synthetic-client",
            vec![batch],
            vec![archive_domain::ConversationV1 {
                conversation_id: "synthetic-conversation".into(),
                display_name: None,
                conversation_type: None,
                participant_ids: vec![],
            }],
            vec![],
        )
        .unwrap()
    }

    const START: i64 = 1_700_000_000_000;

    fn success(_: &ClientExportV1) -> Result<String, CommandError> {
        Ok(Utc.timestamp_millis_opt(START).unwrap().to_rfc3339())
    }

    fn open(directory: &TestDirectory) -> IncrementalCollector {
        IncrementalCollector::open(&directory.path(), "synthetic-scope".into()).unwrap()
    }

    #[test]
    fn restart_reuses_successful_cursor_and_only_uploads_changed_messages() {
        let directory = TestDirectory::new();
        let first = export(&[("old", START - 60_000, "original")]);
        let mut collector = open(&directory);
        let plan = collector.plan(START, false, false);
        assert_eq!(plan.since_unix_ms, None);
        collector
            .upload(first.clone(), &plan, START, false, success)
            .unwrap();
        drop(collector);

        let mut collector = open(&directory);
        let next = START + 60_000;
        let plan = collector.plan(next, false, false);
        assert_eq!(plan.since_unix_ms, Some(START - OVERLAP_MS));
        let repeated = export(&[("old", START - 60_000, "original")]);
        let result = collector
            .upload(repeated, &plan, next, false, |_| {
                panic!("unchanged scan uploaded")
            })
            .unwrap();
        assert_eq!(result.completed_at, None);
        assert_eq!(result.uploaded_count, 1);
        assert_eq!(result.last_upload_at, success(&first).unwrap());
        let plan = collector.plan(next + 60_000, false, false);
        assert_eq!(plan.since_unix_ms, Some(next - OVERLAP_MS));
        let changed = export(&[
            ("old", START - 60_000, "revised"),
            ("new", next, "new message"),
        ]);
        let result = collector
            .upload(changed, &plan, next + 60_000, false, |delta| {
                assert_eq!(delta.message_count, 2);
                // Reconstructed counts and both checksum layers remain importable.
                archive_transfer::read_json_slice(&serde_json::to_vec(delta).unwrap()).unwrap();
                success(delta)
            })
            .unwrap();
        assert_eq!(result.uploaded_count, 2);
        let plan = collector.plan(next + 120_000, false, false);
        collector
            .upload(
                export(&[
                    ("old", START - 60_000, "revised"),
                    ("new", next, "new message"),
                    ("newer", next + 1, "only new"),
                ]),
                &plan,
                next + 120_000,
                false,
                |delta| {
                    assert_eq!(delta.message_count, 1);
                    assert_eq!(delta.batches[0].messages[0].source_message_id, "newer");
                    success(delta)
                },
            )
            .unwrap();
    }

    #[test]
    fn failures_do_not_advance_cursor_or_fingerprints_and_save_is_atomic() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let first = export(&[("old", START, "original")]);
        let plan = collector.plan(START, false, false);
        assert!(
            collector
                .upload(
                    first.clone(),
                    &plan,
                    START,
                    false,
                    |_| Err(internal_error())
                )
                .is_err()
        );
        assert_eq!(collector.plan(START + 1, false, false).since_unix_ms, None);
        collector
            .upload(first, &plan, START, false, success)
            .unwrap();
        let plan = collector.plan(START + 60_000, false, false);
        let changed = export(&[("new", START + 1, "new")]);
        assert!(
            collector
                .upload(changed.clone(), &plan, START + 60_000, false, |_| Err(
                    internal_error()
                ))
                .is_err()
        );
        collector
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_progress BEFORE INSERT ON checkpoint
            BEGIN SELECT RAISE(ABORT, 'synthetic save failure'); END;",
            )
            .unwrap();
        let error = collector
            .upload(changed.clone(), &plan, START + 60_000, false, success)
            .unwrap_err();
        assert_eq!(error.code, "COLLECTION_PROGRESS_FAILED");
        drop(collector);
        let mut collector = open(&directory);
        assert_eq!(
            collector.plan(START + 120_000, false, false).since_unix_ms,
            Some(START - OVERLAP_MS)
        );
        collector
            .connection
            .execute_batch("DROP TRIGGER fail_progress")
            .unwrap();
        collector
            .upload(changed, &plan, START + 120_000, false, |delta| {
                assert_eq!(delta.message_count, 1);
                success(delta)
            })
            .unwrap();
    }

    #[test]
    fn daily_reconciliation_finds_old_revisions_and_scope_changes_force_full_scan() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let old = START - 2 * FULL_SCAN_INTERVAL_MS;
        let plan = collector.plan(START, false, false);
        collector
            .upload(
                export(&[("old", old, "original")]),
                &plan,
                START,
                false,
                success,
            )
            .unwrap();
        let next = START + FULL_SCAN_INTERVAL_MS;
        let plan = collector.plan(next, false, false);
        assert_eq!(plan.since_unix_ms, None);
        collector
            .upload(
                export(&[("old", old, "revised")]),
                &plan,
                next,
                false,
                |delta| {
                    assert_eq!(delta.message_count, 1);
                    success(delta)
                },
            )
            .unwrap();
        assert!(
            collector
                .plan(next + 1, false, false)
                .since_unix_ms
                .is_some()
        );
        assert_eq!(collector.plan(next - 1, false, false).since_unix_ms, None);
        assert_eq!(collector.plan(next + 1, true, false).since_unix_ms, None);
        drop(collector);
        let mut collector =
            IncrementalCollector::open(&directory.path(), "changed-scope".into()).unwrap();
        let plan = collector.plan(next + 1, false, false);
        assert_eq!(plan.since_unix_ms, None);
        collector
            .upload(
                export(&[("old", old, "revised")]),
                &plan,
                next + 1,
                false,
                |delta| {
                    assert_eq!(delta.message_count, 1);
                    success(delta)
                },
            )
            .unwrap();
    }

    #[test]
    fn redaction_precedes_delta_filtering_and_context_rows_are_never_uploaded() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let plan = collector.plan(START, false, true);
        collector
            .upload(export(&[]), &plan, START, false, success)
            .unwrap();
        let plan = collector.plan(START + 60_000, false, true);
        let cutoff = plan.since_unix_ms.unwrap();
        assert_eq!(
            plan.read_since_unix_ms(),
            Some(cutoff - REDACTION_CONTEXT_MS)
        );
        let raw = export(&[
            ("context", cutoff - 1, "账号密码如下"),
            ("credential", cutoff, "user123跟secret456"),
        ]);
        let redacted = crate::collector_transfer::redact_sensitive_export(raw).unwrap();
        collector
            .upload(redacted, &plan, START + 60_000, false, |delta| {
                assert_eq!(delta.message_count, 1);
                let json = serde_json::to_string(delta).unwrap();
                assert!(!json.contains("user123"));
                assert!(!json.contains("secret456"));
                assert_eq!(delta.batches[0].messages[0].source_message_id, "credential");
                archive_transfer::read_json_slice(json.as_bytes()).unwrap();
                success(delta)
            })
            .unwrap();
    }

    #[test]
    fn empty_first_and_forced_uploads_work_and_source_paths_are_isolated() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let plan = collector.plan(START, false, false);
        assert!(
            collector
                .upload(export(&[]), &plan, START, false, success)
                .unwrap()
                .completed_at
                .is_some()
        );
        let plan = collector.plan(START + 1, false, false);
        assert!(
            collector
                .upload(export(&[]), &plan, START + 1, true, success)
                .unwrap()
                .completed_at
                .is_some()
        );
        let first = progress_path(&directory.0, "../invalid", "source-one");
        let second = progress_path(&directory.0, "../invalid", "source-two");
        assert_ne!(first, second);
        assert!(first.starts_with(directory.0.join("instances")));
        assert!(!first.to_string_lossy().contains(".."));
    }

    #[test]
    fn profile_carriers_move_out_of_context_without_revising_unchanged_messages() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let profile =
            serde_json::json!({"synthetic-member": {"department": "synthetic-department"}});
        let mut first = export(&[
            ("context", START - OVERLAP_MS - 1, "earlier"),
            ("recent", START, "unchanged"),
        ]);
        first.batches[0].messages[0].raw_payload["participant_profiles"] = profile.clone();
        let plan = collector.plan(START, false, true);
        collector
            .upload(first.clone(), &plan, START, false, success)
            .unwrap();
        let plan = collector.plan(START + 1, false, true);
        collector
            .upload(first.clone(), &plan, START + 1, false, |_| {
                panic!("carrier movement uploaded unchanged messages")
            })
            .unwrap();
        first.batches[0].messages[0].raw_payload["participant_profiles"]["synthetic-member"]["department"] =
            "updated-department".into();
        collector
            .upload(first, &plan, START + 2, false, |delta| {
                assert_eq!(delta.message_count, 1);
                let message = &delta.batches[0].messages[0];
                assert_eq!(message.source_message_id, "recent");
                assert_eq!(
                    message.raw_payload["participant_profiles"]["synthetic-member"]["department"],
                    "updated-department"
                );
                success(delta)
            })
            .unwrap();
    }

    #[test]
    fn shrinking_directory_windows_skip_uploads_but_metadata_changes_are_delivered() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let mut first = export(&[("recent", START, "unchanged")]);
        first.conversations.push(archive_domain::ConversationV1 {
            conversation_id: "synthetic-historical-conversation".into(),
            display_name: None,
            conversation_type: None,
            participant_ids: vec![],
        });
        let plan = collector.plan(START, false, false);
        collector
            .upload(first, &plan, START, false, success)
            .unwrap();
        let plan = collector.plan(START + 1, false, false);
        collector
            .upload(
                export(&[("recent", START, "unchanged")]),
                &plan,
                START + 1,
                false,
                |_| panic!("shrinking directory caused an upload"),
            )
            .unwrap();
        let mut changed = export(&[("recent", START, "unchanged")]);
        changed.conversations[0].display_name = Some("updated synthetic name".into());
        collector
            .upload(changed, &plan, START + 2, false, |delta| {
                assert_eq!(delta.message_count, 0);
                assert_eq!(
                    delta.conversations[0].display_name.as_deref(),
                    Some("updated synthetic name")
                );
                archive_transfer::read_json_slice(&serde_json::to_vec(delta).unwrap()).unwrap();
                success(delta)
            })
            .unwrap();
    }

    #[test]
    fn changed_media_is_uploaded_with_valid_checksums_and_unchanged_blobs_are_removed() {
        let directory = TestDirectory::new();
        let mut collector = open(&directory);
        let make_export = |second_bytes: &[u8]| {
            let mut data = export(&[
                ("first", START, "first attachment"),
                ("second", START, "second attachment"),
            ]);
            let mut blobs = Vec::new();
            for (message, bytes) in data.batches[0]
                .messages
                .iter_mut()
                .zip([b"first synthetic file".as_slice(), second_bytes])
            {
                let hash = hash_bytes(bytes);
                message.media = vec![archive_domain::MediaRefV1 {
                    content_hash: Some(hash.clone()),
                    original_name: Some("synthetic.bin".into()),
                    mime_type: None,
                    size_bytes: Some(bytes.len() as u64),
                    source_locator: format!("sha256:{hash}"),
                    integrity: archive_domain::MediaIntegrity::Verified,
                    missing_reason: None,
                }];
                blobs.push(archive_domain::MediaBlobV1 {
                    content_hash: hash,
                    original_name: Some("synthetic.bin".into()),
                    mime_type: None,
                    size_bytes: bytes.len() as u64,
                    content_hex: hex::encode(bytes),
                });
            }
            data.batches[0].content_sha256 = hash_value(&data.batches[0].messages).unwrap();
            archive_transfer::create_export_with_media(
                &data.client_version,
                data.batches,
                data.conversations,
                data.participants,
                blobs,
            )
            .unwrap()
        };
        let plan = collector.plan(START, false, false);
        collector
            .upload(
                make_export(b"old synthetic file"),
                &plan,
                START,
                false,
                success,
            )
            .unwrap();
        let plan = collector.plan(START + 1, false, false);
        collector
            .upload(
                make_export(b"new synthetic file"),
                &plan,
                START + 1,
                false,
                |delta| {
                    assert_eq!(delta.message_count, 1);
                    assert_eq!(delta.batches[0].messages[0].source_message_id, "second");
                    assert_eq!(delta.media_count, 1);
                    assert_eq!(delta.media_blobs.len(), 1);
                    assert_eq!(
                        delta.media_blobs[0].content_hex,
                        hex::encode(b"new synthetic file")
                    );
                    archive_transfer::read_json_slice(&serde_json::to_vec(delta).unwrap()).unwrap();
                    success(delta)
                },
            )
            .unwrap();
        drop(collector);
        let bytes = std::fs::read(directory.path()).unwrap();
        for plaintext in [
            b"second attachment".as_slice(),
            b"synthetic-conversation",
            b"synthetic-source",
        ] {
            assert!(
                !bytes
                    .windows(plaintext.len())
                    .any(|window| window == plaintext)
            );
        }
    }

    #[test]
    fn scope_ignores_scheduling_but_invalidates_changed_collection_and_destination() {
        let config: EnterpriseCollectorConfig = serde_json::from_value(serde_json::json!({
            "schemaVersion": "enterprise-collector.v1", "organizationId": "synthetic-organization",
            "organizationName": "synthetic-name", "collectionNotice": "synthetic-notice",
            "keyId": "synthetic-key", "publicKeyHex": "synthetic-public",
            "signingPublicKeyHex": "synthetic-signing", "signatureHex": "synthetic-signature",
            "uploadUrl": "http://127.0.0.1:1/api/v1/imports/enterprise",
            "uploadToken": "synthetic-placeholder-token", "collectorId": uuid::Uuid::new_v4().to_string()
        })).unwrap();
        let original = scope_hash(&config).unwrap();
        let mut scheduling = config.clone();
        scheduling.config_revision += 1;
        scheduling.collector_schedule.interval_minutes += 1;
        assert_eq!(original, scope_hash(&scheduling).unwrap());
        for index in 0..5 {
            let mut changed = config.clone();
            match index {
                0 => changed.include_files = !changed.include_files,
                1 => changed.include_images = !changed.include_images,
                2 => changed.data_redaction = !changed.data_redaction,
                3 => changed.upload_url.push_str("/new"),
                _ => changed.upload_token.push_str("-rotated"),
            }
            assert_ne!(original, scope_hash(&changed).unwrap());
        }
    }
}
