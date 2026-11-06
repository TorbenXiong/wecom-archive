//! Generate synthetic contracts for desktop acceptance. Never discovers source data.
use archive_domain::{ArchiveBatchV1, ConversationV1, MediaBlobV1, ParticipantV1};
// source-windows' debug objects also reference the workspace's native SQLite provider.
use archive_store as _;
use archive_transfer::{
    create_enterprise_package, create_export_with_media, write_importable_json_formatted,
};
use chrono::{TimeDelta, TimeZone, Utc};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use source_windows::enterprise_crypto;
use std::{fs, io::Write, path::Path};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Enrollment {
    organization_id: String,
    key_id: String,
    public_key_hex: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Result<Vec<u8>, &'static str> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("INVALID_SYNTHETIC_PUBLIC_KEY");
    }
    pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| "INVALID_SYNTHETIC_PUBLIC_KEY")?;
            u8::from_str_radix(pair, 16).map_err(|_| "INVALID_SYNTHETIC_PUBLIC_KEY")
        })
        .collect()
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension("partial");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&partial, path)
    })();
    let _ = fs::remove_file(partial);
    result
}

fn generate(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/acceptance")
        .canonicalize()?;
    let root = root.canonicalize()?;
    if root == allowed
        || !root.starts_with(&allowed)
        || fs::read(root.join(".synthetic-run"))? != b"desktop-acceptance.v1"
    {
        return Err("FRESH_SYNTHETIC_RUN_REQUIRED".into());
    }
    let enrollment: Enrollment = serde_json::from_slice(&fs::read(root.join("enrollment.json"))?)?;
    let public_key = decode_hex(&enrollment.public_key_hex)?;
    let blobs = [
        ("fixture.png", "image/png"),
        ("fixture.txt", "text/plain"),
        ("fixture.silk", "audio/silk"),
    ]
    .into_iter()
    .map(|(name, mime)| {
        let bytes = fs::read(root.join(name))?;
        Ok(MediaBlobV1 {
            content_hash: hex(&Sha256::digest(&bytes)),
            original_name: Some(name.into()),
            mime_type: Some(mime.into()),
            size_bytes: bytes.len() as u64,
            content_hex: hex(&bytes),
        })
    })
    .collect::<Result<Vec<_>, std::io::Error>>()?;
    let conversations = (0..125)
        .map(|index| ConversationV1 {
            conversation_id: format!("R:acceptance-{index:03}"),
            display_name: Some(format!("合成验收会话 {index:03}")),
            conversation_type: Some("group".into()),
            participant_ids: vec!["acceptance-a".into(), "acceptance-b".into()],
        })
        .collect::<Vec<_>>();
    let participants = [
        ("acceptance-a", "测试成员甲"),
        ("acceptance-b", "测试成员乙"),
    ]
    .into_iter()
    .map(|(id, name)| ParticipantV1 {
        participant_id: id.into(),
        display_name: Some(name.into()),
        participant_kind: Some("employee".into()),
    })
    .collect::<Vec<_>>();
    // Export date ranges use UTC calendar days.
    let january_two = Utc
        .with_ymd_and_hms(2026, 1, 2, 0, 0, 0)
        .single()
        .ok_or("INVALID_SYNTHETIC_DATE")?;
    for batch_index in 0..10 {
        let batch_id = Uuid::from_u128(0xacce_0000 + batch_index);
        let count = if batch_index == 0 { 6200 } else { 1 };
        let messages = (0..count).map(|index| {
            let conversation = if batch_index != 0 || index < 6000 { 0 }
                else if index < 6152 { 1 + (index - 6000) / 2 }
                else { 77 + index - 6152 };
            let sent_at = if batch_index != 0 { january_two + TimeDelta::days(3) + TimeDelta::seconds(batch_index as i64) }
                else if index < 201 { january_two + TimeDelta::seconds(index) }
                else { january_two + TimeDelta::days(1) + TimeDelta::seconds(index) };
            let mut media = Vec::new();
            let mut message_type = "text";
            if batch_index == 0 && matches!(index, 11 | 12 | 14 | 15) {
                let blob = &blobs[match index { 11 => 0, 14 => 2, _ => 1 }];
                media.push(json!({"content_hash": blob.content_hash, "original_name": blob.original_name,
                    "mime_type": blob.mime_type, "size_bytes": blob.size_bytes, "source_locator": "synthetic",
                    "integrity": "verified", "missing_reason": null}));
                message_type = match index { 11 => "image", 14 => "audio", _ => "file" };
            }
            if batch_index == 0 && index == 16 {
                media.push(json!({"content_hash": null, "original_name": "synthetic-missing.txt", "mime_type": "text/plain",
                    "size_bytes": null, "source_locator": "synthetic-missing", "integrity": "missing", "missing_reason": "synthetic fixture"}));
                message_type = "file";
            }
            let id = if batch_index == 0 { format!("acceptance-{index:05}") } else { format!("acceptance-update-{batch_index:02}") };
            let body = if batch_index == 0 && index == 11 { "合成图片".into() }
                else if batch_index == 0 && index == 15 { "shared-match".into() }
                else { format!("合成项目记录 {id} {}", "验收内容 ".repeat(24)) };
            json!({"schema_version":"message.v1", "source_kind":"synthetic", "source_instance_id":"synthetic-acceptance",
                "source_message_id":id, "stable_message_id":id, "conversation_id":format!("R:acceptance-{conversation:03}"),
                "sender_id":if index % 2 == 0 {"acceptance-a"} else {"acceptance-b"}, "sent_at":sent_at,
                "direction":"incoming", "message_type":message_type, "body_text":body,
                "quoted_message_id":if batch_index == 0 && index == 13 {Some("acceptance-00000")} else {None},
                "lifecycle":"active", "media":media, "raw_type":if index == 14 {"16"} else {"synthetic"},
                "raw_payload":{}, "parser_version":"synthetic-fixture.v1", "collection_batch_id":batch_id})
        }).collect::<Vec<_>>();
        let batch: ArchiveBatchV1 = serde_json::from_value(json!({
            "schema_version":"archive-batch.v1", "batch_id":batch_id, "source_adapter":"synthetic", "source_instance_id":"synthetic-acceptance",
            "collected_at":january_two, "employee_notice":{"notice_version":"synthetic", "displayed_at":january_two,
                "acknowledged_at":january_two, "evidence_hash":"synthetic"}, "external_contact_consent":"not_applicable",
            "collection_scope":{"source_ids":["synthetic-acceptance"], "conversation_ids":[], "starts_at":null,"ends_at":null,"message_types":[]},
            "cursor_before":null,"cursor_after":null,"messages":messages,"content_sha256":"synthetic",
            "retention":{"policy_id":null,"expires_at":null,"legal_hold":false}}))?;
        let export = create_export_with_media(
            "synthetic-acceptance",
            vec![batch],
            conversations.clone(),
            participants.clone(),
            if batch_index == 0 {
                blobs.clone()
            } else {
                vec![]
            },
        )?;
        write_importable_json_formatted(
            &export,
            &root.join(format!("fixture-{batch_index}.json")),
            false,
        )?;
        if batch_index == 1 {
            let encrypted = enterprise_crypto::encrypt(&public_key, &serde_json::to_vec(&export)?)?;
            let package = create_enterprise_package(
                &export,
                &enrollment.organization_id,
                &enrollment.key_id,
                &encrypted.wrapped_dek,
                &encrypted.nonce,
                &encrypted.ciphertext,
                &encrypted.tag,
            )?;
            write_new(&root.join("fixture-1.wca"), &package)?;
        }
    }
    Ok(())
}

fn main() {
    let result = std::env::args_os()
        .nth(1)
        .ok_or("SYNTHETIC_RUN_REQUIRED")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })
        .and_then(|root| generate(Path::new(&root)));
    if result.is_err() {
        eprintln!("SYNTHETIC_FIXTURE_GENERATION_FAILED");
        std::process::exit(1);
    }
    println!("Synthetic acceptance fixtures generated.");
}
