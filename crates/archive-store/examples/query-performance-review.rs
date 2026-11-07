//! Offline SQL diagnostics; fixtures are synthetic and bypass production ingestion.
//! This measures query costs, not import throughput or full HTTP request latency.
use archive_domain::{
    LifecycleState, MESSAGE_SCHEMA_VERSION, MessageDirection, MessageType, MessageV1,
};
use archive_store::{ArchiveStore, MessageQuery};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn sample<T>(mut action: impl FnMut() -> Result<T>) -> Result<Value> {
    let start = Instant::now();
    let _ = action()?;
    let first_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = action()?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    Ok(json!({"first_ms": first_ms, "warm_p50_ms": times[2], "warm_p95_ms": times[4]}))
}

fn plan(connection: &Connection, sql: &str, parameters: impl rusqlite::Params) -> Result<Value> {
    let rows = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
        .query_map(parameters, |row| row.get::<_, String>(3))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows))
}

fn ids(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<String>> {
    Ok(connection
        .prepare(sql)?
        .query_map(parameters, |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn fixture(connection: &mut Connection, count: usize) -> Result<()> {
    let time = DateTime::<Utc>::from_timestamp(1_735_689_600, 0).unwrap();
    let template = MessageV1 {
        schema_version: MESSAGE_SCHEMA_VERSION.into(),
        source_kind: "synthetic".into(),
        source_instance_id: "synthetic-source".into(),
        source_message_id: "synthetic".into(),
        stable_message_id: "synthetic".into(),
        conversation_id: "R:synthetic-hot".into(),
        sender_id: None,
        sent_at: time,
        direction: MessageDirection::Incoming,
        message_type: MessageType::Text,
        body_text: Some("synthetic archive message".into()),
        quoted_message_id: None,
        lifecycle: LifecycleState::Active,
        media: Vec::new(),
        raw_type: "synthetic".into(),
        raw_payload: json!({"synthetic": true}),
        parser_version: "review.v1".into(),
        collection_batch_id: Uuid::nil(),
    };
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<999)
         INSERT INTO conversations(conversation_id, last_message_at)
         SELECT 'R:synthetic-' || printf('%04d', i), '2025-01-01T00:00:00+00:00' FROM n;
         INSERT INTO conversations(conversation_id, last_message_at)
         VALUES ('R:synthetic-hot', '2025-01-01T00:00:00+00:00');",
    )?;
    transaction.execute(
        "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i+1<?1),
         fixture AS (
            SELECT 'stable-' || printf('%09d', i) AS id,
                   'source-' || printf('%09d', i) AS source_id,
                   CASE WHEN i%2=0 THEN 'R:synthetic-hot'
                        ELSE 'R:synthetic-' || printf('%04d', (i/2)%1000) END AS conversation,
                   strftime('%Y-%m-%dT%H:%M:%S', 1735689600+i/3, 'unixepoch') || '+00:00' AS ts,
                   'synthetic archive message ' || i || ' ' || ?3 ||
                   CASE WHEN i%10000=0 THEN ' benchmarkneedle' ELSE '' END AS body
            FROM n
         )
         INSERT INTO messages(stable_message_id, source_kind, source_instance_id, source_message_id,
            conversation_id, sent_at, direction, message_type, body_text, lifecycle, raw_type,
            parser_version, first_batch_id, last_batch_id, content_sha256, payload_json)
         SELECT id, 'synthetic', 'synthetic-source', source_id, conversation, ts, 'incoming',
            'text', body, 'active', 'synthetic', 'review.v1', 'synthetic', 'synthetic', 'synthetic',
            json_set(?2, '$.stable_message_id', id, '$.source_message_id', source_id,
                '$.conversation_id', conversation, '$.sent_at', ts, '$.body_text', body)
         FROM fixture",
        params![
            count as i64,
            serde_json::to_string(&template)?,
            "x".repeat(200)
        ],
    )?;
    transaction.execute_batch(
        "INSERT INTO message_fts(stable_message_id, body_text)
         SELECT stable_message_id, body_text FROM messages;
         INSERT INTO archive_fts_rows(fts_rowid, stable_message_id)
         SELECT rowid, stable_message_id FROM message_fts;
         INSERT INTO media(media_id, original_name, source_locator, integrity, first_batch_id)
         SELECT 'missing:' || stable_message_id, 'synthetic-file.txt', 'synthetic', 'missing', 'synthetic'
         FROM messages WHERE cast(substr(stable_message_id, 8) AS INTEGER)%20=0;
         INSERT INTO message_media(stable_message_id, media_id, ordinal)
         SELECT substr(media_id, 9), media_id, 0 FROM media;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn run() -> Result<()> {
    let count: usize = std::env::args()
        .nth(1)
        .unwrap_or("1000000".into())
        .parse()?;
    if !(10_000..=1_000_000).contains(&count) || !count.is_multiple_of(10_000) {
        return Err("invalid synthetic count".into());
    }
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/benchmarks");
    std::fs::create_dir_all(&root)?;
    let directory = tempfile::Builder::new()
        .prefix("query-review-")
        .tempdir_in(root)?;
    let path = directory.path().join("synthetic.db");
    drop(ArchiveStore::open(&path)?);
    let mut connection = Connection::open(&path)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    eprintln!("review_phase=synthetic_fixture");
    fixture(&mut connection, count)?;
    let store = ArchiveStore::open_read_only(&path)?;
    let mut metrics = BTreeMap::new();
    let mut plans = BTreeMap::new();
    if std::env::args().nth(2).as_deref() == Some("anchor-probe") {
        let mut metrics = BTreeMap::new();
        let revision = store.summary()?.revision;
        let conversation = "R:synthetic-hot";
        let total = store.count_messages(&MessageQuery {
            conversation_id: Some(conversation.into()),
            ..Default::default()
        })?;
        for ascending in [false, true] {
            let order = if ascending { "asc" } else { "desc" };
            let query = MessageQuery {
                conversation_id: Some(conversation.into()),
                limit: 100,
                offset: total * 4 / 5,
                sort_ascending: ascending,
                ..Default::default()
            };
            let expected = store.query_messages(&query)?;
            let anchor = store
                .query_messages(&MessageQuery {
                    limit: 1,
                    offset: query.offset - 1,
                    ..query.clone()
                })?
                .remove(0)
                .stable_message_id;
            metrics.insert(
                format!("offset_page_{order}"),
                sample(|| {
                    let _snapshot = store.read_snapshot()?;
                    let page = store.query_messages(&query)?;
                    assert_eq!(page, expected);
                    Ok(page)
                })?,
            );
            metrics.insert(
                format!("anchor_page_{order}"),
                sample(|| {
                    let _snapshot = store.read_snapshot()?;
                    let page = store
                        .query_messages_at_anchor(&query, &anchor, 1, revision)?
                        .ok_or("anchor unexpectedly expired")?;
                    assert_eq!(page, expected);
                    Ok(page)
                })?,
            );
            let target = &expected[37].stable_message_id;
            metrics.insert(
                format!("located_page_{order}"),
                sample(|| {
                    let _snapshot = store.read_snapshot()?;
                    let page = store
                        .query_messages_at_anchor(&query, target, -37, revision)?
                        .ok_or("target unexpectedly expired")?;
                    assert_eq!(page, expected);
                    Ok(page)
                })?,
            );
            for rank in [10, total * 4 / 5] {
                let id = store
                    .query_messages(&MessageQuery {
                        limit: 1,
                        offset: rank,
                        ..query.clone()
                    })?
                    .remove(0)
                    .stable_message_id;
                metrics.insert(
                    format!("position_original_{order}_{rank}"),
                    sample(|| {
                        let value = original_offset(&connection, &id, ascending)?;
                        assert_eq!(value, rank);
                        Ok(value)
                    })?,
                );
                metrics.insert(
                    format!("position_tuple_{order}_{rank}"),
                    sample(|| {
                        let value = store
                            .message_offset_in_conversation(&id, ascending)?
                            .ok_or("target missing")?;
                        assert_eq!(value, rank);
                        Ok(value)
                    })?,
                );
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "scope": "store_pages_and_exact_position_not_http", "message_count": count,
                "conversation_messages": total, "anchor_acquisition_included": false,
                "metrics": metrics
            }))?
        );
        return Ok(());
    }
    if std::env::args().nth(2).as_deref() == Some("ordered-probe") {
        let mut metrics = BTreeMap::new();
        let mut plans = BTreeMap::new();
        // Restrict the ordered prefix before testing text, but fall back when
        // it cannot fill the page. No candidate outside the prefix is discarded.
        let probe_sql = "WITH window AS MATERIALIZED (
            SELECT rowid AS message_rowid, sent_at, stable_message_id FROM messages
            ORDER BY sent_at DESC, stable_message_id DESC LIMIT 4096
         ) SELECT m.payload_json FROM window w JOIN messages m ON m.rowid=w.message_rowid
         WHERE CASE WHEN m.body_text LIKE ?2 ESCAPE '\\' THEN 1
           WHEN m.stable_message_id IN (
             SELECT stable_message_id FROM message_fts WHERE message_fts MATCH ?1
           ) THEN 1 ELSE m.stable_message_id IN (
             SELECT mm.stable_message_id FROM message_media mm
             WHERE mm.media_id IN (SELECT media_id FROM media WHERE original_name LIKE ?2 ESCAPE '\\')
           ) END
         ORDER BY w.sent_at DESC, w.stable_message_id DESC LIMIT 50";
        for term in [
            "synthetic",
            "thetic",
            "synthetic-file",
            "benchmarkneedle",
            "absentneedle",
        ] {
            let parameters = [format!("\"{term}\""), format!("%{term}%")];
            let query = MessageQuery {
                text: Some(term.into()),
                limit: 50,
                ..Default::default()
            };
            let expected = store.query_messages(&query)?;
            let probe = || -> Result<Vec<MessageV1>> {
                let page = ids(
                    &connection,
                    probe_sql,
                    params![parameters[0], parameters[1]],
                )?
                .into_iter()
                .map(|text| serde_json::from_str(&text))
                .collect::<std::result::Result<Vec<MessageV1>, _>>()?;
                if page.len() == 50 {
                    Ok(page)
                } else {
                    Ok(store.query_messages(&query)?)
                }
            };
            assert_eq!(probe()?, expected);
            plans.insert(
                term,
                plan(
                    &connection,
                    probe_sql,
                    params![parameters[0], parameters[1]],
                )?,
            );
            metrics.insert(
                format!("{term}_current"),
                sample(|| Ok(store.query_messages(&query)?))?,
            );
            metrics.insert(format!("{term}_probe_with_fallback"), sample(probe)?);
        }
        println!(
            "{}",
            json!({"messages": count, "metrics": metrics, "plans": plans,
            "notes": "synthetic SQL fixture; includes deserialization and insufficient-prefix fallback; no HTTP"})
        );
        return Ok(());
    }
    let fts_sql = "SELECT stable_message_id FROM message_fts WHERE message_fts MATCH ?1";
    let media_sql = "SELECT mm.stable_message_id FROM media media_search
        JOIN message_media mm ON mm.media_id = media_search.media_id
        WHERE media_search.original_name LIKE ?1 ESCAPE '\\'";
    let query = MessageQuery {
        text: Some("benchmarkneedle".into()),
        limit: 50,
        ..Default::default()
    };
    eprintln!("review_phase=search_and_result_positions");
    plans.insert(
        "attachment_before",
        plan(&connection, media_sql, ["%benchmarkneedle%"])?,
    );
    metrics.insert(
        "fts_candidates",
        sample(|| ids(&connection, fts_sql, ["\"benchmarkneedle\""]))?,
    );
    metrics.insert(
        "attachment_candidates_before",
        sample(|| ids(&connection, media_sql, ["%benchmarkneedle%"]))?,
    );
    metrics.insert(
        "query_messages_before",
        sample(|| Ok(store.query_messages(&query)?))?,
    );
    for ascending in [false, true] {
        let search = MessageQuery {
            sort_ascending: ascending,
            ..query.clone()
        };
        let messages = store.query_messages(&search)?;
        metrics.insert(
            if ascending {
                "positions_for_search_results_asc"
            } else {
                "positions_for_search_results_desc"
            },
            sample(|| {
                for message in &messages {
                    let actual = store
                        .message_offset_in_conversation(&message.stable_message_id, ascending)?;
                    let index = message.stable_message_id[7..].parse::<usize>()?;
                    let expected = if ascending {
                        index / 2
                    } else {
                        (count - 2 - index) / 2
                    };
                    assert_eq!(actual, Some(expected as u64));
                }
                Ok(())
            })?,
        );
    }
    let first = store.query_messages(&query)?.remove(0);
    let ts = first.sent_at.to_rfc3339();
    let rank_sql = "SELECT count(*) FROM messages WHERE conversation_id=?1
        AND (sent_at>?2 OR (sent_at=?2 AND stable_message_id>?3))";
    plans.insert(
        "result_position",
        plan(
            &connection,
            rank_sql,
            params![first.conversation_id, ts, first.stable_message_id],
        )?,
    );
    metrics.insert(
        "plain_conversation_count",
        sample(|| {
            Ok(store.count_messages(&MessageQuery {
                conversation_id: Some("R:synthetic-hot".into()),
                ..Default::default()
            })?)
        })?,
    );
    metrics.insert(
        "derived_conversation_count",
        sample(|| {
            let actual: i64 = connection.query_row(
                "SELECT message_count FROM archive_conversation_stats WHERE conversation_id=?1",
                ["R:synthetic-hot"],
                |row| row.get(0),
            )?;
            assert_eq!(actual, count as i64 / 2);
            Ok(actual)
        })?,
    );
    eprintln!("review_phase=offset_and_cursor");
    let offset = count as i64 * 4 / 5;
    let offset_sql = "SELECT stable_message_id FROM messages ORDER BY sent_at DESC, stable_message_id DESC LIMIT 100 OFFSET ?1";
    let anchor: (String, String) = connection.query_row(
        "SELECT sent_at, stable_message_id FROM messages ORDER BY sent_at DESC, stable_message_id DESC LIMIT 1 OFFSET ?1",
        [offset - 1], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let cursor_sql =
        "SELECT stable_message_id FROM messages WHERE (sent_at, stable_message_id)<(?1, ?2)
        ORDER BY sent_at DESC, stable_message_id DESC LIMIT 100";
    assert_eq!(
        ids(&connection, offset_sql, [offset])?,
        ids(&connection, cursor_sql, params![anchor.0, anchor.1])?
    );
    plans.insert("deep_offset", plan(&connection, offset_sql, [offset])?);
    plans.insert(
        "deep_cursor",
        plan(&connection, cursor_sql, params![anchor.0, anchor.1])?,
    );
    metrics.insert(
        "deep_offset_keys",
        sample(|| ids(&connection, offset_sql, [offset]))?,
    );
    // The cursor must already be known by the caller; finding it using OFFSET is not timed.
    metrics.insert(
        "deep_cursor_keys",
        sample(|| ids(&connection, cursor_sql, params![anchor.0, anchor.1]))?,
    );
    eprintln!("review_phase=reverse_media_index_experiment");
    connection.execute_batch(
        "CREATE INDEX review_media_reverse ON message_media(media_id, stable_message_id)",
    )?;
    plans.insert(
        "attachment_after",
        plan(&connection, media_sql, ["%benchmarkneedle%"])?,
    );
    metrics.insert(
        "attachment_candidates_after",
        sample(|| ids(&connection, media_sql, ["%benchmarkneedle%"]))?,
    );
    metrics.insert(
        "query_messages_after",
        sample(|| Ok(store.query_messages(&query)?))?,
    );
    // A reverse index alone need not change join order. Test names before
    // resolving references, rather than looking up a name for every reference.
    let media_first_sql = "SELECT mm.stable_message_id FROM media media_search
        CROSS JOIN message_media mm ON mm.media_id=media_search.media_id
        WHERE media_search.original_name LIKE ?1 ESCAPE '\\'";
    plans.insert(
        "attachment_media_first",
        plan(&connection, media_first_sql, ["%benchmarkneedle%"])?,
    );
    assert_eq!(
        ids(&connection, media_sql, ["%synthetic-file%"])?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        ids(&connection, media_first_sql, ["%synthetic-file%"])?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );
    metrics.insert(
        "attachment_candidates_media_first",
        sample(|| ids(&connection, media_first_sql, ["%benchmarkneedle%"]))?,
    );
    eprintln!("review_phase=common_word_search");
    let common = MessageQuery {
        text: Some("synthetic".into()),
        ..query.clone()
    };
    metrics.insert(
        "common_word_query_messages",
        sample(|| {
            let page = store.query_messages(&common)?;
            assert_eq!(page.len(), 50);
            Ok(page)
        })?,
    );
    assert_eq!(store.count_messages(&query)?, (count / 10_000) as u64);
    let report = json!({
        "schema_version": "archive-query-review.v1", "dataset": "synthetic.sql-review.v1",
        "messages": count, "hot_conversation_messages": count / 2, "media_refs": count / 20,
        "position_result_count": (count / 10_000).min(50),
        "sqlite_version": rusqlite::version(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "warm_samples": 5, "metrics": metrics, "plans": plans,
        "notes": ["Fixture bypasses ingestion; no import/export throughput claim.",
                  "Positions timed separately, excluding query and HTTP response construction.",
                  "Cursor comparison reads keys only; anchor lookup excluded.",
                  "Experimental index exists only in this process's synthetic database.",
                  "Media refs exercise SQL filtering; payloads intentionally contain no media metadata."]
    });
    println!("{report}");
    Ok(())
}

fn original_offset(connection: &Connection, id: &str, ascending: bool) -> Result<u64> {
    let (conversation, time): (String, String) = connection.query_row(
        "SELECT conversation_id, sent_at FROM messages WHERE stable_message_id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let comparison = if ascending {
        "sent_at < ?2 OR (sent_at = ?2 AND stable_message_id < ?3)"
    } else {
        "sent_at > ?2 OR (sent_at = ?2 AND stable_message_id > ?3)"
    };
    Ok(connection.query_row(
        &format!("SELECT count(*) FROM messages WHERE conversation_id = ?1 AND ({comparison})"),
        params![conversation, time, id],
        |row| row.get::<_, i64>(0),
    )? as u64)
}

fn main() {
    if run().is_err() {
        eprintln!("query_review_failed");
        std::process::exit(1);
    }
}
