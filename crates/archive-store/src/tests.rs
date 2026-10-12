use super::*;
use archive_domain::{ArchiveBatchV1, MessageV1, ParticipantV1};
use archive_domain::{
    BATCH_SCHEMA_VERSION, CollectionScope, EmployeeNoticeEvidence, ExternalContactConsent,
    LifecycleState, MESSAGE_SCHEMA_VERSION, MediaIntegrity, MediaRefV1, MessageDirection,
    RetentionDirective,
};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

fn batch(body: &str) -> ArchiveBatchV1 {
    let batch_id = Uuid::new_v4();
    let message_time = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&Utc);
    ArchiveBatchV1 {
        schema_version: BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: "fixture".into(),
        source_instance_id: "source".into(),
        collected_at: Utc::now(),
        employee_notice: EmployeeNoticeEvidence {
            notice_version: "test".into(),
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
        cursor_after: Some("1".into()),
        messages: vec![MessageV1 {
            schema_version: MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "fixture".into(),
            source_instance_id: "source".into(),
            source_message_id: "one".into(),
            stable_message_id: "stable-one".into(),
            conversation_id: "conversation".into(),
            sender_id: Some("participant".into()),
            sent_at: message_time,
            direction: MessageDirection::Incoming,
            message_type: MessageType::Text,
            body_text: Some(body.into()),
            quoted_message_id: None,
            lifecycle: LifecycleState::Active,
            media: vec![],
            raw_type: "text".into(),
            raw_payload: json!({"text": body}),
            parser_version: "fixture".into(),
            collection_batch_id: batch_id,
        }],
        content_sha256: "fixture".into(),
        retention: RetentionDirective {
            policy_id: None,
            expires_at: None,
            legal_hold: false,
        },
    }
}

#[test]
fn upgrades_v2_indexes_and_keeps_paging_and_streaming_in_one_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let mut store = ArchiveStore::open(&path).unwrap();
    let mut input = batch("needle");
    let template = input.messages[0].clone();
    input.messages = (0..12)
        .map(|index| {
            let mut message = template.clone();
            message.stable_message_id = format!("stable-{index:02}");
            message.source_message_id = format!("source-{index:02}");
            message.sent_at += chrono::TimeDelta::seconds(index / 3);
            message
        })
        .collect();
    store.ingest_batch(&input).unwrap();
    store
        .connection
        .execute_batch("DROP INDEX idx_messages_time")
        .unwrap();
    drop(store);
    let store = ArchiveStore::open(&path).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let plan: String = store.connection.query_row(
        "EXPLAIN QUERY PLAN SELECT rowid FROM messages ORDER BY sent_at, stable_message_id LIMIT 2 OFFSET 8",
        [], |row| row.get(3),
    ).unwrap();
    assert!(plan.contains("COVERING INDEX idx_messages_time"), "{plan}");
    let reader = ArchiveStore::open_read_only(&path).unwrap();
    let snapshot = reader.read_snapshot().unwrap();
    assert_eq!(reader.message_count().unwrap(), 12);
    let mut writer = ArchiveStore::open(&path).unwrap();
    let mut added = batch("newer");
    added.messages[0].stable_message_id = "added".into();
    added.messages[0].source_message_id = "added".into();
    added.messages[0].sent_at += chrono::TimeDelta::days(1);
    writer.ingest_batch(&added).unwrap();
    for ascending in [false, true] {
        let query = MessageQuery {
            limit: 2,
            offset: 8,
            sort_ascending: ascending,
            ..Default::default()
        };
        let mut ids = Vec::new();
        reader
            .visit_messages::<StoreError>(&query, |message| {
                ids.push(message.stable_message_id);
                Ok(())
            })
            .unwrap();
        assert_eq!(ids.len(), 12);
        let page = reader.query_messages(&query).unwrap();
        assert_eq!(
            page.iter()
                .map(|message| &message.stable_message_id)
                .collect::<Vec<_>>(),
            ids[8..10].iter().collect::<Vec<_>>()
        );
        let mut visits = 0;
        let error = reader.visit_messages::<StoreError>(&query, |_| {
            visits += 1;
            Err(StoreError::Invalid("stop".into()))
        });
        assert!(error.is_err());
        assert_eq!(visits, 1);
    }
    assert_eq!(reader.message_count().unwrap(), 12);
    drop(snapshot);
    assert_eq!(reader.message_count().unwrap(), 13);
}

#[test]
fn search_candidates_preserve_filters_counts_and_attachment_matches() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("needle");
    let template = input.messages[0].clone();
    input.messages = (0..6)
        .map(|index| {
            let mut message = template.clone();
            message.stable_message_id = format!("stable-{index}");
            message.source_message_id = format!("source-{index}");
            message.conversation_id = if index < 3 { "first" } else { "second" }.into();
            message.sender_id = Some(if index % 2 == 0 { "alice" } else { "bob" }.into());
            message.sent_at += chrono::TimeDelta::seconds(index / 2);
            message.body_text = Some(
                if index == 0 || index == 2 {
                    "needle"
                } else {
                    "ordinary"
                }
                .into(),
            );
            if index != 2 && index != 5 {
                message.message_type = MessageType::File;
                message.media.push(MediaRefV1 {
                    content_hash: None,
                    original_name: Some(
                        match index {
                            0 | 1 => "needle.txt",
                            3 => "100%_report.txt",
                            _ => "100XXreport.txt",
                        }
                        .into(),
                    ),
                    mime_type: Some("text/plain".into()),
                    size_bytes: None,
                    source_locator: "synthetic".into(),
                    integrity: MediaIntegrity::Missing,
                    missing_reason: Some("synthetic".into()),
                });
            }
            message
        })
        .collect();
    store.ingest_batch(&input).unwrap();
    let check = |query: MessageQuery, expected: &[&str]| {
        assert_eq!(store.count_messages(&query).unwrap(), expected.len() as u64);
        let messages = store.query_messages(&query).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.stable_message_id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    };
    check(
        MessageQuery {
            text: Some("needle".into()),
            limit: 100,
            ..Default::default()
        },
        &["stable-2", "stable-1", "stable-0"],
    );
    check(
        MessageQuery {
            // FTS token matching alone misses a substring inside a token;
            // global search must also find partial and CJK-like text input.
            text: Some("eedl".into()),
            limit: 100,
            ..Default::default()
        },
        &["stable-2", "stable-1", "stable-0"],
    );
    check(
        MessageQuery {
            text: Some("needle".into()),
            conversation_id: Some("first".into()),
            participant_id: Some("alice".into()),
            message_type: Some(MessageType::File),
            media_only: true,
            starts_at: Some(template.sent_at),
            ends_at: Some(template.sent_at),
            limit: 100,
            ..Default::default()
        },
        &["stable-0"],
    );
    check(
        MessageQuery {
            text: Some("100%_report".into()),
            limit: 100,
            ..Default::default()
        },
        &["stable-3"],
    );
    check(
        MessageQuery {
            text: Some("  ".into()),
            conversation_id: Some("first".into()),
            sort_ascending: true,
            limit: 100,
            ..Default::default()
        },
        &["stable-0", "stable-1", "stable-2"],
    );
    let page = MessageQuery {
        text: Some("needle".into()),
        limit: 1,
        offset: 1,
        ..Default::default()
    };
    assert_eq!(store.count_messages(&page).unwrap(), 3);
    assert_eq!(
        store.query_messages(&page).unwrap()[0].stable_message_id,
        "stable-1"
    );
    check(
        MessageQuery {
            text: Some("needle\" OR \"anything".into()),
            limit: 100,
            ..Default::default()
        },
        &[],
    );
}

#[test]
fn literal_search_finds_cjk_partial_text_and_preserves_special_characters() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("fixture");
    let template = input.messages[0].clone();
    input.messages = [
        "我们讨论项目进度和验收结果",
        "The ArchiveBenchmark includes MixedCase text",
        "literal 100%_value at C:\\fixture\\files",
        "literal 100XXvalue at C:fixturefiles",
        "literal needle\" OR \"anything",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, body)| {
        let mut message = template.clone();
        message.stable_message_id = format!("literal-{index}");
        message.source_message_id = format!("literal-source-{index}");
        message.body_text = Some(body.into());
        message
    })
    .collect();
    store.ingest_batch(&input).unwrap();
    for (text, expected) in [
        ("项目进度", "literal-0"),
        ("验收", "literal-0"),
        ("hiveBench", "literal-1"),
        ("mixedcase", "literal-1"),
        ("100%_value", "literal-2"),
        ("C:\\fixture\\files", "literal-2"),
        ("needle\" OR \"anything", "literal-4"),
    ] {
        let query = MessageQuery {
            text: Some(text.into()),
            limit: 100,
            ..Default::default()
        };
        assert_eq!(store.count_messages(&query).unwrap(), 1, "{text}");
        assert_eq!(
            store.query_messages(&query).unwrap()[0].stable_message_id,
            expected,
            "{text}"
        );
    }
}

#[test]
fn ingest_is_idempotent_and_records_revision() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();

    let first = store.ingest_batch(&batch("第一版")).unwrap();
    assert_eq!(first.inserted, 1);
    let second = store.ingest_batch(&batch("第一版")).unwrap();
    assert_eq!(second.unchanged, 1);
    let third = store.ingest_batch(&batch("第二版")).unwrap();
    assert_eq!(third.revised, 1);
    assert_eq!(store.message_count().unwrap(), 1);
    assert_eq!(store.summary().unwrap().revision, 3);
}

#[test]
fn locates_message_offset_in_descending_conversation_order() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("第一条");
    for (index, seconds) in [1_i64, 2].into_iter().enumerate() {
        let mut message = input.messages[0].clone();
        message.source_message_id = format!("message-{}", index + 2);
        message.stable_message_id = format!("stable-{}", index + 2);
        message.sent_at += chrono::Duration::seconds(seconds);
        message.body_text = Some(format!("第{}条", index + 2));
        input.messages.push(message);
    }
    store.ingest_batch(&input).unwrap();

    assert_eq!(
        store
            .message_offset_in_conversation("stable-3", false)
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        store
            .message_offset_in_conversation("stable-2", false)
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        store
            .message_offset_in_conversation("stable-one", false)
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        store
            .message_offset_in_conversation("missing", false)
            .unwrap(),
        None
    );
    let ascending = store
        .query_messages(&MessageQuery {
            conversation_id: Some("conversation".into()),
            limit: 10,
            sort_ascending: true,
            ..MessageQuery::default()
        })
        .unwrap();
    assert_eq!(
        ascending
            .iter()
            .map(|message| message.stable_message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["stable-one", "stable-2", "stable-3"]
    );
}

#[test]
fn lists_collected_source_accounts_instead_of_chat_participants() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("第一条");
    input.messages[0].direction = MessageDirection::Outgoing;
    let mut second = input.messages[0].clone();
    second.source_message_id = "two".into();
    second.stable_message_id = "stable-two".into();
    second.sender_id = Some("participant-2".into());
    second.direction = MessageDirection::Incoming;
    input.messages.push(second);
    let mut third = input.messages[0].clone();
    third.source_message_id = "three".into();
    third.stable_message_id = "stable-three".into();
    input.messages.push(third);
    store
        .upsert_directory(
            &[],
            &[
                ParticipantV1 {
                    participant_id: "participant".into(),
                    display_name: Some("甲用户".into()),
                    participant_kind: None,
                },
                ParticipantV1 {
                    participant_id: "participant-2".into(),
                    display_name: Some("乙用户".into()),
                    participant_kind: None,
                },
            ],
        )
        .unwrap();
    store.ingest_batch(&input).unwrap();

    let users = store.list_collected_users().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].source_instance_id, "source");
    assert_eq!(users[0].display_name.as_deref(), Some("甲用户"));
}

#[test]
fn merges_same_message_from_different_collectors() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let first = batch("同一条消息");
    store.ingest_batch(&first).unwrap();
    let mut second = batch("同一条消息");
    second.batch_id = Uuid::new_v4();
    second.source_instance_id = "collector-2".into();
    second.collection_scope.source_ids = vec!["collector-2".into()];
    second.messages[0].collection_batch_id = second.batch_id;
    second.messages[0].source_instance_id = "collector-2".into();
    second.messages[0].stable_message_id = "stable-one-from-collector-2".into();
    assert_eq!(store.ingest_batch(&second).unwrap().unchanged, 1);
    assert_eq!(store.message_count().unwrap(), 1);
}

#[test]
fn missing_media_metadata_is_retained_and_counted() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("附件");
    input.messages[0].media.push(MediaRefV1 {
        content_hash: None,
        original_name: Some("附件.pdf".into()),
        mime_type: Some("application/pdf".into()),
        size_bytes: Some(128),
        source_locator: "unavailable".into(),
        integrity: MediaIntegrity::Missing,
        missing_reason: Some("metadata_only".into()),
    });
    store.ingest_batch(&input).unwrap();
    assert_eq!(store.summary().unwrap().media_count, 1);
    assert_eq!(store.list_conversations(10, 0).unwrap()[0].media_count, 1);
}

#[test]
fn migrates_version_one_media_schema() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE media (
                    content_sha256 TEXT PRIMARY KEY,
                    original_name TEXT,
                    mime_type TEXT,
                    size_bytes INTEGER,
                    archive_relative_path TEXT,
                    source_locator TEXT NOT NULL,
                    integrity TEXT NOT NULL,
                    missing_reason TEXT,
                    first_batch_id TEXT NOT NULL
                );
                CREATE TABLE message_media (
                    stable_message_id TEXT NOT NULL,
                    content_sha256 TEXT NOT NULL REFERENCES media(content_sha256),
                    ordinal INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (stable_message_id, content_sha256)
                );
                INSERT INTO media VALUES (
                    'abc', 'old.bin', NULL, 3, NULL, 'sha256:abc', 'verified', NULL, 'batch'
                );
                PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let store = ArchiveStore::open(&path).unwrap();
    assert_eq!(store.summary().unwrap().media_count, 1);
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

fn shared_media(hash: &str) -> MediaRefV1 {
    MediaRefV1 {
        content_hash: Some(hash.into()),
        original_name: Some("synthetic.bin".into()),
        mime_type: None,
        size_bytes: Some(1),
        source_locator: "fixture".into(),
        integrity: MediaIntegrity::Verified,
        missing_reason: None,
    }
}

#[test]
fn batched_conversation_times_preserve_unsorted_inserts_and_legacy_revision_semantics() {
    let directory = tempdir().unwrap();
    let mut store = ArchiveStore::open(&directory.path().join("archive.db")).unwrap();
    let mut input = batch("timefixture");
    let template = input.messages[0].clone();
    input.messages = [10, -30, 0, 100, 100, -10]
        .into_iter()
        .enumerate()
        .map(|(index, seconds)| {
            let mut message = template.clone();
            message.stable_message_id = format!("time-stable-{index}");
            message.source_message_id = format!("time-source-{index}");
            message.sent_at += chrono::TimeDelta::seconds(seconds);
            message
        })
        .collect();
    store.ingest_batch(&input).unwrap();
    let expected = template.sent_at + chrono::TimeDelta::seconds(100);
    assert_eq!(
        store.list_conversations(1, 0).unwrap()[0].last_message_at,
        expected
    );

    let mut older = batch("olderfixture");
    older.messages[0].sent_at += chrono::TimeDelta::seconds(80);
    store.ingest_batch(&older).unwrap();
    assert_eq!(
        store.list_conversations(1, 0).unwrap()[0].last_message_at,
        expected
    );

    // Existing revisions intentionally keep the old projection's time.
    let mut revision = batch("revisedfixture");
    revision.messages[0].sent_at += chrono::TimeDelta::seconds(2_000);
    assert_eq!(store.ingest_batch(&revision).unwrap().revised, 1);
    assert_eq!(
        store.list_conversations(1, 0).unwrap()[0].last_message_at,
        expected
    );

    let mut failed = batch("uncommittedfixture");
    failed.messages[0].stable_message_id = "uncommitted".into();
    failed.messages[0].source_message_id = "uncommitted".into();
    failed.messages[0].sent_at += chrono::TimeDelta::seconds(3_000);
    let mut invalid = failed.messages[0].clone();
    invalid.schema_version = "invalid".into();
    failed.messages.push(invalid);
    assert!(store.ingest_batch(&failed).is_err());
    assert_eq!(
        store.list_conversations(1, 0).unwrap()[0].last_message_at,
        expected
    );
    assert_eq!(store.message_count().unwrap(), 7);
}

fn assert_conversation_statistics(store: &ArchiveStore) {
    for item in store.list_conversations(200, 0).unwrap() {
        let (messages, media): (u64, u64) = store
            .connection
            .query_row(
                "SELECT (SELECT count(*) FROM messages WHERE conversation_id = ?1),
                    (SELECT count(DISTINCT mm.media_id) FROM message_media mm
                     JOIN messages m ON m.stable_message_id = mm.stable_message_id
                     WHERE m.conversation_id = ?1)",
                [&item.conversation_id],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )
            .unwrap();
        assert_eq!((item.message_count, item.media_count), (messages, media));
    }
}

#[test]
fn conversation_statistics_track_shared_media_revisions_moves_and_rollback() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let mut store = ArchiveStore::open(&path).unwrap();
    let mut input = batch("originalword");
    input.messages[0].media = vec![shared_media("a"), shared_media("a")];
    let mut second = input.messages[0].clone();
    second.stable_message_id = "stable-two".into();
    second.source_message_id = "two".into();
    second.media = vec![shared_media("a"), shared_media("b")];
    let mut third = second.clone();
    third.stable_message_id = "stable-three".into();
    third.source_message_id = "three".into();
    third.conversation_id = "other".into();
    third.media = vec![shared_media("a")];
    input.messages.extend([second, third]);
    store.ingest_batch(&input).unwrap();
    assert_conversation_statistics(&store);
    let first = store.list_conversations(1, 0).unwrap().remove(0);
    assert_eq!((first.message_count, first.media_count), (2, 2));
    assert_eq!(store.ingest_batch(&input).unwrap().unchanged, 3);
    assert_conversation_statistics(&store);

    // Two ordinals pointing to the same media still move one distinct media.
    store
        .connection
        .execute(
            "UPDATE messages SET conversation_id = 'other' WHERE stable_message_id = 'stable-one'",
            [],
        )
        .unwrap();
    assert_conversation_statistics(&store);
    assert_eq!(store.list_conversations(10, 0).unwrap()[1].media_count, 1);
    store.connection.execute(
        "UPDATE messages SET conversation_id = 'conversation' WHERE stable_message_id = 'stable-one'",
        [],
    ).unwrap();
    assert_conversation_statistics(&store);

    let mut revision = batch("updatedword");
    revision.messages[0].media = vec![];
    store.ingest_batch(&revision).unwrap();
    assert_conversation_statistics(&store);
    assert_eq!(store.list_conversations(1, 0).unwrap()[0].media_count, 2);

    // Moving a stored projection must move all of its distinct/shared references.
    store
        .connection
        .execute(
            "UPDATE messages SET conversation_id = 'other' WHERE stable_message_id = 'stable-two'",
            [],
        )
        .unwrap();
    assert_conversation_statistics(&store);
    let items = store.list_conversations(10, 0).unwrap();
    assert_eq!((items[0].message_count, items[0].media_count), (1, 0));
    assert_eq!((items[1].message_count, items[1].media_count), (2, 2));

    let before = store.list_conversations(10, 0).unwrap();
    let mut failed = batch("rolledbackword");
    failed.messages[0].media = vec![shared_media("rollback")];
    let mut invalid = failed.messages[0].clone();
    invalid.schema_version = "invalid".into();
    failed.messages.push(invalid);
    assert!(store.ingest_batch(&failed).is_err());
    assert_eq!(store.list_conversations(10, 0).unwrap(), before);
    assert_conversation_statistics(&store);
    assert_eq!(
        store
            .count_messages(&MessageQuery {
                text: Some("updatedword".into()),
                ..Default::default()
            })
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_messages(&MessageQuery {
                text: Some("rolledbackword".into()),
                ..Default::default()
            })
            .unwrap(),
        0
    );

    // Remove links first as required by the base foreign keys.
    store
        .connection
        .execute_batch(
            "DELETE FROM message_media WHERE stable_message_id = 'stable-two';
         DELETE FROM messages WHERE stable_message_id = 'stable-two';",
        )
        .unwrap();
    assert_conversation_statistics(&store);
    assert_eq!(store.list_conversations(10, 0).unwrap()[1].media_count, 1);
    drop(store);
    assert_conversation_statistics(&ArchiveStore::open(&path).unwrap());
}

fn remove_derived_tables(connection: &Connection) {
    let triggers = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'trigger' AND name LIKE 'archive_%'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for trigger in triggers {
        connection
            .execute_batch(&format!(
                "DROP TRIGGER \"{}\"",
                trigger.replace('"', "\"\"")
            ))
            .unwrap();
    }
    connection
        .execute_batch(
            "DROP TABLE archive_conversation_stats;
         DROP TABLE archive_conversation_media_refs;
         DROP TABLE archive_fts_rows;
         DROP INDEX idx_conversations_recent;",
        )
        .unwrap();
}

#[test]
fn legacy_v2_statistics_and_arbitrary_fts_rowids_are_backfilled_once() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let mut store = ArchiveStore::open(&path).unwrap();
    let mut input = batch("oldneedle");
    input.messages[0].media = vec![shared_media("a"), shared_media("a")];
    store.ingest_batch(&input).unwrap();
    remove_derived_tables(&store.connection);
    store
        .connection
        .execute_batch(
            "DELETE FROM message_fts;
         INSERT INTO message_fts(rowid, stable_message_id, body_text)
         VALUES (5000, 'stable-one', 'oldneedle'), (7000, 'stable-one', 'oldneedle');",
        )
        .unwrap();
    drop(store);

    let reader = ArchiveStore::open_read_only(&path).unwrap();
    assert_conversation_statistics(&reader);
    assert_eq!(reader.list_conversations(10, 0).unwrap()[0].media_count, 1);
    drop(reader);

    let mut store = ArchiveStore::open(&path).unwrap();
    assert_conversation_statistics(&store);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM archive_fts_rows WHERE stable_message_id = 'stable-one'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    // A later writable open must not rewrite the backfilled mapping.
    drop(store);
    store = ArchiveStore::open(&path).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT min(fts_rowid) FROM archive_fts_rows", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        5000
    );
    store.ingest_batch(&batch("newneedle")).unwrap();
    assert_eq!(
        store
            .count_messages(&MessageQuery {
                text: Some("oldneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .count_messages(&MessageQuery {
                text: Some("newneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM message_fts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_conversation_statistics(&store);
    let plan: Vec<String> = store
        .connection
        .prepare(
            "EXPLAIN QUERY PLAN DELETE FROM message_fts WHERE rowid IN (
            SELECT fts_rowid FROM archive_fts_rows WHERE stable_message_id = 'stable-one'
         )",
        )
        .unwrap()
        .query_map([], |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("INDEX idx_archive_fts_rows_message")),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|line| line.contains("VIRTUAL TABLE INDEX") && line.contains('=')),
        "{plan:?}"
    );
}

#[test]
fn repairs_fts_mapping_after_legacy_writes_without_deleting_other_messages() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("archive.db");
    let mut store = ArchiveStore::open(&path).unwrap();
    let mut input = batch("firstneedle");
    let mut other = input.messages[0].clone();
    other.stable_message_id = "unrelated".into();
    other.source_message_id = "unrelated".into();
    other.body_text = Some("unrelatedneedle".into());
    input.messages.push(other);
    store.ingest_batch(&input).unwrap();
    let reader = ArchiveStore::open_read_only(&path).unwrap();
    let snapshot = reader.read_snapshot().unwrap();
    assert_eq!(
        reader
            .count_messages(&MessageQuery {
                text: Some("firstneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        1
    );

    // Model the old revision order, which allocates a completely different FTS rowid.
    store
        .connection
        .execute_batch(
            "BEGIN;
         UPDATE messages SET body_text = 'legacyneedle' WHERE stable_message_id = 'stable-one';
         DELETE FROM message_fts WHERE stable_message_id = 'stable-one';
         INSERT INTO message_fts(rowid, stable_message_id, body_text)
         VALUES (9000, 'stable-one', 'legacyneedle');
         COMMIT;",
        )
        .unwrap();
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM archive_fts_rows WHERE stable_message_id = 'stable-one'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    store.ingest_batch(&batch("finalneedle")).unwrap();
    for (term, expected) in [
        ("firstneedle", 0),
        ("legacyneedle", 0),
        ("finalneedle", 1),
        ("unrelatedneedle", 1),
    ] {
        assert_eq!(
            store
                .count_messages(&MessageQuery {
                    text: Some(term.into()),
                    ..Default::default()
                })
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        reader
            .count_messages(&MessageQuery {
                text: Some("firstneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        1
    );
    assert_eq!(
        reader
            .count_messages(&MessageQuery {
                text: Some("finalneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        0
    );
    drop(snapshot);

    let mut empty = batch("unused");
    empty.messages[0].body_text = None;
    store.ingest_batch(&empty).unwrap();
    assert_eq!(
        store
            .count_messages(&MessageQuery {
                text: Some("finalneedle".into()),
                ..Default::default()
            })
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM message_fts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM archive_fts_rows", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}
