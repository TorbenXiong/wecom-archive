//! Local HTTP/browser acceptance host. Never discovers or reads real source data.
use archive_domain::{ArchiveBatchV1, ConversationV1, ParticipantV1};
use archive_store::ArchiveStore;
use chrono::{TimeDelta, Utc};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::var_os("WECOM_ARCHIVE_SERVER_DATA")
            .ok_or("acceptance data directory required")?,
    );
    let allowed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-acceptance");
    std::fs::create_dir_all(&allowed)?;
    std::fs::create_dir_all(&root)?;
    if !root.canonicalize()?.starts_with(allowed.canonicalize()?)
        || root.join("archive.db").exists()
    {
        return Err("acceptance requires a fresh directory under target/ui-acceptance".into());
    }
    let batch_id = Uuid::new_v4();
    let time = chrono::DateTime::parse_from_rfc3339("2026-10-13T01:00:00Z")?.with_timezone(&Utc);
    let messages = (0..6000).map(|index| {
        let media = if index % 20 == 0 { vec![json!({
            "content_hash": null, "original_name": "合成附件.txt", "mime_type": "text/plain",
            "size_bytes": null, "source_locator": "synthetic", "integrity": "missing", "missing_reason": "synthetic fixture",
        })] } else { vec![] };
        json!({
            "schema_version": "message.v1", "source_kind": "synthetic", "source_instance_id": "synthetic-source",
            "source_message_id": format!("synthetic-{index:05}"), "stable_message_id": format!("synthetic-{index:05}"),
            "conversation_id": if index < 5200 { "R:synthetic-project" } else { "R:synthetic-check" },
            "sender_id": if index % 2 == 0 { "synthetic-member-a" } else { "synthetic-member-b" },
            "sent_at": time + TimeDelta::seconds(index), "direction": "incoming",
            "message_type": if media.is_empty() { "text" } else { "file" },
            "body_text": format!("合成项目记录 {index:05}：界面验收与查询验证。{}", "合成内容 ".repeat(30)),
            "quoted_message_id": null, "lifecycle": "active", "media": media, "raw_type": "synthetic",
            "raw_payload": {}, "parser_version": "synthetic-fixture.v1", "collection_batch_id": batch_id,
        })
    }).collect::<Vec<_>>();
    let batch: ArchiveBatchV1 = serde_json::from_value(json!({
        "schema_version": "archive-batch.v1", "batch_id": batch_id, "source_adapter": "synthetic",
        "source_instance_id": "synthetic-source", "collected_at": time,
        "employee_notice": { "notice_version": "synthetic", "displayed_at": time, "acknowledged_at": time, "evidence_hash": "synthetic" },
        "external_contact_consent": "not_applicable", "collection_scope": { "source_ids": ["synthetic-source"], "conversation_ids": ["R:synthetic-project", "R:synthetic-check"], "starts_at": null, "ends_at": null, "message_types": [] },
        "cursor_before": null, "cursor_after": null, "messages": messages, "content_sha256": "synthetic",
        "retention": { "policy_id": null, "expires_at": null, "legal_hold": false },
    }))?;
    let conversations = vec![
        ConversationV1 {
            conversation_id: "R:synthetic-project".into(),
            display_name: Some("合成项目讨论".into()),
            conversation_type: Some("group".into()),
            participant_ids: vec!["synthetic-member-a".into(), "synthetic-member-b".into()],
        },
        ConversationV1 {
            conversation_id: "R:synthetic-check".into(),
            display_name: Some("合成验收记录".into()),
            conversation_type: Some("group".into()),
            participant_ids: vec!["synthetic-member-a".into(), "synthetic-member-b".into()],
        },
    ];
    let participants = vec![
        ParticipantV1 {
            participant_id: "synthetic-member-a".into(),
            display_name: Some("测试成员甲".into()),
            participant_kind: Some("employee".into()),
        },
        ParticipantV1 {
            participant_id: "synthetic-member-b".into(),
            display_name: Some("测试成员乙".into()),
            participant_kind: Some("employee".into()),
        },
    ];
    let export =
        archive_transfer::create_export("synthetic", vec![batch], conversations, participants)?;
    let archive_path = root.join("archive.db");
    let mut store = ArchiveStore::open(&archive_path)?;
    store.ingest_batch(&export.batches[0])?;
    store.upsert_directory(&export.conversations, &export.participants)?;
    drop(store);
    let server = wecom_archive_server::start_desktop_server_with_local_collector(
        move |_, _, _, _, _, _| Ok(export.clone()),
    )?;
    // The URL contains a synthetic-host token; never print it or commit this file.
    std::fs::write(
        root.join("connection.json"),
        serde_json::to_vec(&json!({ "pageUrl": server.page_url() }))?,
    )?;
    println!("synthetic acceptance host ready");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    server.shutdown();
    Ok(())
}
