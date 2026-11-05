//! Synthetic, authenticated loopback HTTP benchmark; never reads a source account.
use archive_domain::{ArchiveBatchV1, ConversationV1, ParticipantV1};
use archive_store::ArchiveStore;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct WorkDirectory(PathBuf);
impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/benchmarks");
        if let (Ok(parent), Ok(work)) = (parent.canonicalize(), self.0.canonicalize())
            && work.parent() == Some(parent.as_path())
            && work
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("search-api-"))
        {
            let _ = std::fs::remove_dir_all(work);
        }
    }
}

fn time(index: usize) -> DateTime<Utc> {
    DateTime::from_timestamp(1_735_689_600 + (index / 3) as i64, 0).unwrap()
}

fn conversation(index: usize) -> String {
    if index.is_multiple_of(2) {
        "R:synthetic-hot".into()
    } else {
        format!("R:synthetic-{:04}", index % 1000)
    }
}

fn ingest_fixture(root: &std::path::Path, count: usize) -> Result<()> {
    let mut store = ArchiveStore::open(&root.join("archive.db"))?;
    let mut conversations = BTreeMap::new();
    for start in (0..count).step_by(2000) {
        let batch_id = Uuid::from_u128(start as u128 + 1);
        let messages = (start..(start + 2000).min(count)).map(|index| {
            let id = conversation(index);
            conversations.entry(id.clone()).or_insert(ConversationV1 {
                conversation_id: id.clone(), display_name: Some("合成基准会话".into()),
                conversation_type: Some("group".into()), participant_ids: Vec::new(),
            });
            let media = if index % 20 == 0 { vec![json!({
                "content_hash": null, "original_name": "合成附件专项.txt", "mime_type": "text/plain",
                "size_bytes": null, "source_locator": "synthetic", "integrity": "missing", "missing_reason": "synthetic",
            })] } else { vec![] };
            json!({
                "schema_version": "message.v1", "source_kind": "synthetic", "source_instance_id": "synthetic-search",
                "source_message_id": format!("synthetic-{index:09}"), "stable_message_id": format!("synthetic-{index:09}"),
                "conversation_id": id, "sender_id": format!("person-{:02}", index % 20),
                "sent_at": time(index), "direction": "incoming", "message_type": if media.is_empty() { "text" } else { "file" },
                "body_text": format!("commonword 项目归档 合成记录 {index} {} {}", if index % 1000 == 0 { "uniqueneedle" } else { "" }, "x".repeat(128)),
                "quoted_message_id": null, "lifecycle": "active", "media": media,
                "raw_type": "synthetic", "raw_payload": {}, "parser_version": "search-benchmark.v1", "collection_batch_id": batch_id,
            })
        }).collect::<Vec<_>>();
        let batch: ArchiveBatchV1 = serde_json::from_value(json!({
            "schema_version": "archive-batch.v1", "batch_id": batch_id,
            "source_adapter": "synthetic", "source_instance_id": "synthetic-search", "collected_at": time(start),
            "employee_notice": { "notice_version": "synthetic", "displayed_at": time(0), "acknowledged_at": time(0), "evidence_hash": "synthetic" },
            "external_contact_consent": "not_applicable", "collection_scope": { "source_ids": ["synthetic-search"], "conversation_ids": [], "starts_at": null, "ends_at": null, "message_types": [] },
            "cursor_before": null, "cursor_after": null, "messages": messages, "content_sha256": "synthetic",
            "retention": { "policy_id": null, "expires_at": null, "legal_hold": false },
        }))?;
        let export = archive_transfer::create_export(
            "synthetic",
            vec![batch],
            conversations.values().cloned().collect(),
            vec![],
        )?;
        store.ingest_batch(&export.batches[0])?;
    }
    let participants = (0..20)
        .map(|index| ParticipantV1 {
            participant_id: format!("person-{index:02}"),
            display_name: Some(format!("合成成员 {index}")),
            participant_kind: Some("employee".into()),
        })
        .collect::<Vec<_>>();
    store.upsert_directory(
        &conversations.into_values().collect::<Vec<_>>(),
        &participants,
    )?;
    assert_eq!(store.summary()?.message_count, count as u64);
    Ok(())
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn request(address: SocketAddr, token: &str, path: &str) -> Result<Value> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    let headers = std::str::from_utf8(&response[..split])?;
    if !headers.starts_with("HTTP/1.1 200 ") {
        return Err("unexpected benchmark API status".into());
    }
    // Axum's JSON responses have a known content length. Reject another transfer
    // framing rather than accidentally measuring or accepting an incomplete body.
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .ok_or("benchmark requires a content-length JSON response")?;
    let body = &response[split + 4..];
    if body.len() != length {
        return Err("incomplete benchmark response".into());
    }
    Ok(serde_json::from_slice(body)?)
}

fn measure(samples: usize, mut action: impl FnMut() -> Result<()>) -> Result<Value> {
    let first = Instant::now();
    action()?;
    let first_ms = first.elapsed().as_secs_f64() * 1000.0;
    let mut times = Vec::new();
    for _ in 0..samples {
        let start = Instant::now();
        action()?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mut ordered = times.clone();
    ordered.sort_by(f64::total_cmp);
    Ok(
        json!({ "first_ms": first_ms, "p50_ms": ordered[ordered.len() / 2],
        "p95_ms": ordered[(ordered.len() * 95).div_ceil(100).saturating_sub(1)], "samples_ms": times }),
    )
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let count: usize = args.next().ok_or("message count required")?.parse()?;
    let samples: usize = args.next().unwrap_or_else(|| "5".into()).parse()?;
    if !(1000..=1_000_000).contains(&count) || !(1..=30).contains(&samples) {
        return Err("invalid benchmark size".into());
    }
    let root = PathBuf::from(
        std::env::var_os("WECOM_ARCHIVE_SERVER_DATA").ok_or("synthetic work directory required")?,
    );
    let allowed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/benchmarks")
        .canonicalize()?;
    let root = root.canonicalize()?;
    if root.parent() != Some(allowed.as_path())
        || !root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("search-api-"))
        || std::fs::read_dir(&root)?.next().is_some()
    {
        return Err(
            "benchmark requires a fresh search-api directory under target/benchmarks".into(),
        );
    }
    let _work = WorkDirectory(root.clone());
    let address: SocketAddr = std::env::var("WECOM_ARCHIVE_SERVER_LISTEN")?.parse()?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("benchmark requires loopback with a nonzero port".into());
    }
    eprintln!("search_benchmark_phase=synthetic_ingest;messages={count}");
    ingest_fixture(&root, count)?;
    eprintln!("search_benchmark_phase=server_start");
    let server =
        wecom_archive_server::start_desktop_server_with_local_collector(|_, _, _, _, _, _| {
            Err("synthetic host has no source collector".into())
        })?;
    let (_, token) = server
        .page_url()
        .split_once("/#token=")
        .ok_or("invalid synthetic host URL")?;
    let mut metrics = BTreeMap::new();
    for name in [
        "rare",
        "common",
        "cjk_partial",
        "filename",
        "missing",
        "combined",
    ] {
        let term = match name {
            "rare" => "uniqueneedle",
            "cjk_partial" => "目归",
            "filename" => "附件专项",
            "missing" => "absentneedle",
            _ => "commonword",
        };
        let matching = (0..count)
            .filter(|index| match name {
                "rare" => index % 1000 == 0,
                "filename" => index % 20 == 0,
                "missing" => false,
                "combined" => index % 20 == 0 && *index >= 120 && *index < count - 120,
                _ => true,
            })
            .collect::<Vec<_>>();
        for ascending in [false, true] {
            let sort = if ascending { "asc" } else { "desc" };
            let expected = if ascending {
                matching.clone()
            } else {
                matching.iter().rev().copied().collect()
            }
            .into_iter()
            .take(50)
            .map(|index| format!("synthetic-{index:09}"))
            .collect::<Vec<_>>();
            for locate in [false, true] {
                if locate && !matches!(name, "rare" | "common") {
                    continue;
                }
                let mut path = format!(
                    "/api/v1/search/messages?q={}&limit=50&sort={sort}&locate={locate}",
                    encode(term)
                );
                if name == "combined" {
                    path.push_str(&format!("&conversation_id={}&participant_id=person-00&message_type=file&media_only=true&starts_at={}&ends_at={}",
                        encode("R:synthetic-hot"), encode(&time(120).to_rfc3339()), encode(&(time(count - 120) - TimeDelta::seconds(1)).to_rfc3339())));
                }
                let key = format!("{name}_{sort}_locate_{locate}");
                eprintln!("search_benchmark_phase={key}");
                metrics.insert(
                    key,
                    measure(samples, || {
                        let response = request(address, token, &path)?;
                        let rows = response.as_array().ok_or("expected search result array")?;
                        let ids = rows
                            .iter()
                            .map(|row| row["stable_message_id"].as_str().ok_or("missing search ID"))
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        assert_eq!(ids, expected, "search ordering or matching changed");
                        for row in rows {
                            assert_eq!(row["offset_in_conversation"].is_number(), locate);
                            if locate {
                                let index: usize =
                                    row["stable_message_id"].as_str().unwrap()[10..].parse()?;
                                let (rank, total) = if index.is_multiple_of(2) {
                                    (index / 2, count.div_ceil(2))
                                } else {
                                    (index / 1000, (count - 1 - index % 1000) / 1000 + 1)
                                };
                                let position = if ascending { rank } else { total - 1 - rank };
                                assert_eq!(row["offset_in_conversation"], position);
                            }
                        }
                        Ok(())
                    })?,
                );
            }
        }
    }
    let index = ((count - 1) / 2) * 2;
    for ascending in [false, true] {
        let sort = if ascending { "asc" } else { "desc" };
        metrics.insert(
            format!("selected_position_{sort}"),
            measure(samples, || {
                let response = request(
                    address,
                    token,
                    &format!("/api/v1/messages/synthetic-{index:09}/position?sort={sort}"),
                )?;
                assert_eq!(
                    response["offset_in_conversation"],
                    if ascending { index / 2 } else { 0 }
                );
                Ok(())
            })?,
        );
    }
    println!(
        "{}",
        serde_json::to_string(
            &json!({ "product_version": env!("CARGO_PKG_VERSION"), "messages": count,
        "query_samples": samples, "transport": "authenticated loopback HTTP/1.1; fresh connection per request",
        "scope": "request through complete JSON response and correctness assertions; fixture/startup excluded",
        "fixture": "production ingest; half in one group; equal timestamps; 501 conversations; 20 participants; 5% attachment references; no media bytes",
        "metrics": metrics })
        )?
    );
    server.shutdown();
    Ok(())
}

fn main() {
    if run().is_err() {
        eprintln!(
            "synthetic search API benchmark failed; no source data or credentials were logged"
        );
        std::process::exit(1);
    }
}
