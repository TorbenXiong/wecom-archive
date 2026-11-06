//! Synthetic comparison of the original allocation strategies and current APIs.
//! No source discovery, networking, real keys, or real user data is involved.
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use archive_domain::*;
use archive_transfer::*;
use chrono::{TimeZone, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

struct MeasuredAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn add_live(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add_live(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            add_live(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe {
            System.dealloc(pointer, layout);
        }
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            if size >= layout.size() {
                add_live(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn fixture(media_bytes: usize) -> ClientExportV1 {
    let time = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let batch_id = Uuid::from_u128(1);
    let mut hash = Sha256::new();
    let chunk = [42_u8; 64 * 1024];
    let mut remaining = media_bytes;
    while remaining > 0 {
        let size = remaining.min(chunk.len());
        hash.update(&chunk[..size]);
        remaining -= size;
    }
    let hash = hex::encode(hash.finalize());
    let messages = vec![MessageV1 {
        schema_version: MESSAGE_SCHEMA_VERSION.into(),
        source_kind: "synthetic".into(),
        source_instance_id: "synthetic-source".into(),
        source_message_id: "synthetic-message".into(),
        stable_message_id: "synthetic-message".into(),
        conversation_id: "synthetic-conversation".into(),
        sender_id: None,
        sent_at: time,
        direction: MessageDirection::Incoming,
        message_type: MessageType::File,
        body_text: Some("synthetic attachment".into()),
        quoted_message_id: None,
        lifecycle: LifecycleState::Active,
        media: vec![MediaRefV1 {
            content_hash: Some(hash.clone()),
            original_name: Some("synthetic.bin".into()),
            mime_type: None,
            size_bytes: Some(media_bytes as u64),
            source_locator: format!("sha256:{hash}"),
            integrity: MediaIntegrity::Verified,
            missing_reason: None,
        }],
        raw_type: "synthetic".into(),
        raw_payload: serde_json::json!({"synthetic": true}),
        parser_version: "synthetic.v1".into(),
        collection_batch_id: batch_id,
    }];
    let batch = ArchiveBatchV1 {
        schema_version: BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: "synthetic".into(),
        source_instance_id: "synthetic-source".into(),
        collected_at: time,
        employee_notice: EmployeeNoticeEvidence {
            notice_version: "synthetic.v1".into(),
            displayed_at: time,
            acknowledged_at: None,
            evidence_hash: "synthetic".into(),
        },
        external_contact_consent: ExternalContactConsent::NotApplicable,
        collection_scope: CollectionScope {
            source_ids: vec!["synthetic-source".into()],
            conversation_ids: vec!["synthetic-conversation".into()],
            starts_at: None,
            ends_at: None,
            message_types: vec![],
        },
        cursor_before: None,
        cursor_after: None,
        content_sha256: json_sha256(&messages).unwrap(),
        messages,
        retention: RetentionDirective {
            policy_id: None,
            expires_at: None,
            legal_hold: false,
        },
    };
    let mut export = create_export_with_media(
        "synthetic-client",
        vec![batch],
        vec![ConversationV1 {
            conversation_id: "synthetic-conversation".into(),
            display_name: None,
            conversation_type: None,
            participant_ids: vec![],
        }],
        vec![],
        vec![MediaBlobV1 {
            content_hash: hash,
            original_name: Some("synthetic.bin".into()),
            mime_type: None,
            size_bytes: media_bytes as u64,
            content_hex: "2a".repeat(media_bytes),
        }],
    )
    .unwrap();
    // Keep bytes reproducible across isolated processes.
    export.export_id = Uuid::from_u128(2);
    export.generated_at = time;
    export
}

fn legacy_encode(export: &ClientExportV1, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let plaintext = serde_json::to_vec(export)?;
    let package = EnterprisePackageV1 {
        schema_version: ENTERPRISE_PACKAGE_SCHEMA_VERSION.into(),
        organization_id: "synthetic-organization".into(),
        key_id: "synthetic-key".into(),
        export_id: export.export_id,
        encryption: "aes-256-gcm+rsa-oaep-sha256".into(),
        wrapped_dek_hex: hex::encode([1_u8; 384]),
        nonce_hex: hex::encode([2_u8; 12]),
        ciphertext_hex: hex::encode(ciphertext),
        plaintext_sha256: hex::encode(Sha256::digest(&plaintext)),
        authentication_tag_hex: hex::encode([3_u8; 16]),
    };
    Ok(serde_json::to_vec(&package)?)
}

fn encode(export: &ClientExportV1, ciphertext: &[u8]) -> Result<Vec<u8>> {
    Ok(create_enterprise_package(
        export,
        "synthetic-organization",
        "synthetic-key",
        &[1; 384],
        &[2; 12],
        ciphertext,
        &[3; 16],
    )?)
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str).unwrap_or("json-current");
    let mib = args
        .get(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(16);
    if !(1..=128).contains(&mib)
        || ![
            "json-current",
            "json-legacy",
            "encode-current",
            "encode-legacy",
            "decode-current",
            "decode-legacy",
        ]
        .contains(&mode)
    {
        return Err("invalid synthetic benchmark parameters".into());
    }
    let media_bytes = mib * 1024 * 1024;
    let directory = tempfile::tempdir()?;
    let json_path = directory.path().join("synthetic.json");
    let package_path = directory.path().join("synthetic.wca");
    let export = fixture(media_bytes);
    let mut writer = BufWriter::new(File::create(&json_path)?);
    serde_json::to_writer(&mut writer, &export)?;
    writer.flush()?;
    drop(writer);
    let json_bytes = std::fs::metadata(&json_path)?.len();
    let ciphertext = if mode.starts_with("encode") || mode.starts_with("decode") {
        vec![42_u8; json_bytes as usize]
    } else {
        vec![]
    };
    if mode.starts_with("decode") {
        std::fs::write(&package_path, encode(&export, &ciphertext)?)?;
    }
    let expected_id = export.export_id;
    // Import phases should not retain the fixture graph or encrypted input.
    let retained_export = mode.starts_with("encode").then_some(export);
    let retained_ciphertext = mode.starts_with("encode").then_some(ciphertext);
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let started = Instant::now();
    let (output_bytes, checksum) = match mode {
        "json-current" | "json-legacy" => {
            let export: ClientExportV1 = if mode == "json-current" {
                read_json(&json_path)?
            } else {
                let document: serde_json::Value =
                    serde_json::from_reader(BufReader::new(File::open(&json_path)?))?;
                let value: ClientExportV1 = serde_json::from_value(document)?;
                value.validate()?;
                let checksum = hex::encode(Sha256::digest(serde_json::to_vec(&(
                    &value.conversations,
                    &value.participants,
                    &value.batches,
                    &value.media_blobs,
                ))?));
                if value.content_sha256 != checksum {
                    return Err("synthetic checksum mismatch".into());
                }
                value
            };
            assert_eq!(export.export_id, expected_id);
            assert_eq!(export.media_blobs[0].content_hex.len(), media_bytes * 2);
            (json_bytes, export.content_sha256)
        }
        "encode-current" | "encode-legacy" => {
            let export = retained_export
                .as_ref()
                .ok_or("missing synthetic fixture")?;
            let ciphertext = retained_ciphertext
                .as_ref()
                .ok_or("missing synthetic ciphertext")?;
            let bytes = if mode == "encode-current" {
                encode(export, ciphertext)?
            } else {
                legacy_encode(export, ciphertext)?
            };
            (bytes.len() as u64, hex::encode(Sha256::digest(&bytes)))
        }
        _ => {
            let ciphertext = if mode == "decode-current" {
                read_enterprise_package(&package_path, "synthetic-organization")?.ciphertext
            } else {
                let mut package: EnterprisePackageV1 = {
                    let bytes = std::fs::read(&package_path)?;
                    let package: EnterprisePackageV1 = serde_json::from_slice(&bytes)?;
                    // Original validation decoded ciphertext once before the caller decoded it again.
                    std::hint::black_box(hex::decode(&package.ciphertext_hex)?);
                    package
                };
                hex::decode(std::mem::take(&mut package.ciphertext_hex))?
            };
            assert_eq!(ciphertext.len() as u64, json_bytes);
            (
                ciphertext.len() as u64,
                hex::encode(Sha256::digest(&ciphertext)),
            )
        }
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let peak = PEAK.load(Ordering::Relaxed);
    println!(
        "{}",
        serde_json::json!({"schema_version": "package-memory.v1", "mode": mode,
        "synthetic_media_bytes": media_bytes, "json_bytes": json_bytes, "output_bytes": output_bytes,
        "output_sha256": checksum, "elapsed_ms": elapsed_ms, "retained_input_heap_bytes": before,
        "peak_heap_bytes": peak, "peak_heap_growth_bytes": peak.saturating_sub(before),
        "measurement": "Rust allocator requested live bytes; excludes OS working set, CNG and allocator bookkeeping"})
    );
    Ok(())
}

fn main() {
    if run().is_err() {
        eprintln!("synthetic_package_memory_benchmark_failed");
        std::process::exit(1);
    }
}
