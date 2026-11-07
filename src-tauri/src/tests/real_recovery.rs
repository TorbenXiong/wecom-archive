//! Explicit real-data drill. Never log payloads, credentials, identifiers or paths.
use archive_domain::{
    ArchiveBatchV1, ClientExportV1, ConversationV1, MediaBlobV1, MessageV1, ParticipantV1,
};
use chrono::{Local, TimeDelta, Utc};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;
use wecom_archive_server::maintenance;
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, &'static str>;

#[test]
fn batch_factory_reads_plain_database_consent_enums() {
    use archive_domain::ExternalContactConsent;
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE collection_batches(batch_id TEXT, source_adapter TEXT, source_instance_id TEXT, collected_at TEXT, employee_notice_json TEXT, external_consent TEXT, collection_scope_json TEXT, cursor_before TEXT, cursor_after TEXT, retention_json TEXT);").unwrap();
    let id = Uuid::new_v4();
    let message: MessageV1 = serde_json::from_value(json!({
        "schema_version": archive_domain::MESSAGE_SCHEMA_VERSION, "source_kind":"synthetic", "source_instance_id":"synthetic-source", "source_message_id":"synthetic-message", "stable_message_id":"synthetic-stable", "conversation_id":"synthetic-conversation", "sender_id":null, "sent_at":"2026-01-02T03:04:05Z", "direction":"incoming", "message_type":"text", "body_text":"synthetic", "quoted_message_id":null, "lifecycle":"active", "media":[], "raw_type":"text", "raw_payload":{}, "parser_version":"synthetic", "collection_batch_id":id
    })).unwrap();
    for (text, expected) in [
        ("granted", ExternalContactConsent::Granted),
        ("denied", ExternalContactConsent::Denied),
        ("unknown", ExternalContactConsent::Unknown),
        ("not_applicable", ExternalContactConsent::NotApplicable),
    ] {
        db.execute("DELETE FROM collection_batches", []).unwrap();
        db.execute("INSERT INTO collection_batches VALUES (?1,'synthetic','synthetic-source','2026-01-02T03:04:05Z',?2,?3,?4,NULL,NULL,?5)", rusqlite::params![id.to_string(), json!({"notice_version":"synthetic","displayed_at":"2026-01-02T03:04:05Z","acknowledged_at":"2026-01-02T03:04:05Z","evidence_hash":"synthetic"}).to_string(), text, json!({"source_ids":["synthetic-source"],"conversation_ids":["synthetic-conversation"],"starts_at":null,"ends_at":null,"message_types":["text"]}).to_string(), json!({"policy_id":null,"expires_at":null,"legal_hold":false}).to_string()]).unwrap();
        assert_eq!(
            batch_for(&db, message.clone(), true)
                .unwrap()
                .external_contact_consent,
            expected
        );
    }
}

#[test]
#[ignore = "requires explicit authorization to collect real data and retain a local verified backup"]
fn authorized_real_backup_restore_and_import_failures() {
    if std::env::var("WECOM_ARCHIVE_REAL_RECOVERY").as_deref() != Ok("authorized") {
        panic!("EXPLICIT_REAL_RECOVERY_AUTHORIZATION_REQUIRED");
    }
    let result = run();
    if let Err(code) = result {
        panic!("{code}");
    }
}

fn check(condition: bool, label: &'static str) -> Result<()> {
    if !condition {
        return Err(label);
    }
    println!("PASS {label}");
    Ok(())
}

struct OwnedRun {
    root: PathBuf,
    base: PathBuf,
}
impl OwnedRun {
    fn resume(workspace: &Path, path: &Path) -> Result<Self> {
        let base = workspace
            .join("target/acceptance")
            .canonicalize()
            .map_err(|_| "RESUME_RUN_UNAVAILABLE")?;
        if source_windows::atomic_file::is_link(
            &fs::symlink_metadata(path).map_err(|_| "RESUME_RUN_UNAVAILABLE")?,
        ) {
            return Err("UNSAFE_RESUME_RUN");
        }
        let root = path.canonicalize().map_err(|_| "RESUME_RUN_UNAVAILABLE")?;
        let source = root.join("source-data");
        if source_windows::atomic_file::is_link(
            &fs::symlink_metadata(&source).map_err(|_| "RESUME_RUN_UNAVAILABLE")?,
        ) || source
            .canonicalize()
            .map_err(|_| "RESUME_RUN_UNAVAILABLE")?
            .parent()
            != Some(root.as_path())
            || source_windows::atomic_file::is_link(
                &fs::symlink_metadata(source.join("archive.db"))
                    .map_err(|_| "RESUME_RUN_UNAVAILABLE")?,
            )
        {
            return Err("UNSAFE_RESUME_RUN");
        }
        if root.parent() != Some(base.as_path())
            || root
                .file_name()
                .and_then(|v| v.to_str())
                .and_then(|v| v.strip_prefix("real-recovery-"))
                .and_then(|v| Uuid::parse_str(v).ok())
                .is_none()
            || fs::read(root.join(".authorized-real-recovery"))
                .ok()
                .as_deref()
                != Some(b"real-recovery.v1")
            || !root.join("source-data/archive.db").is_file()
        {
            return Err("UNSAFE_RESUME_RUN");
        }
        Ok(Self { root, base })
    }
    fn new(workspace: &Path) -> Result<Self> {
        let base = workspace.join("target/acceptance");
        fs::create_dir_all(&base).map_err(|_| "RUN_ROOT_UNAVAILABLE")?;
        let base = base.canonicalize().map_err(|_| "RUN_ROOT_UNAVAILABLE")?;
        if !base.starts_with(workspace)
            || source_windows::atomic_file::is_link(
                &fs::symlink_metadata(&base).map_err(|_| "RUN_ROOT_UNAVAILABLE")?,
            )
        {
            return Err("UNSAFE_RUN_ROOT");
        }
        let root = base.join(format!("real-recovery-{}", Uuid::new_v4()));
        fs::create_dir(&root).map_err(|_| "RUN_ROOT_UNAVAILABLE")?;
        fs::write(root.join(".authorized-real-recovery"), b"real-recovery.v1")
            .map_err(|_| "RUN_MARKER_FAILED")?;
        Ok(Self { root, base })
    }
    fn cleanup(&self) -> Result<()> {
        if source_windows::atomic_file::is_link(
            &fs::symlink_metadata(&self.root).map_err(|_| "UNSAFE_RUN_CLEANUP")?,
        ) {
            return Err("UNSAFE_RUN_CLEANUP");
        }
        if self
            .root
            .canonicalize()
            .ok()
            .as_deref()
            .and_then(Path::parent)
            != Some(self.base.as_path())
            || fs::read(self.root.join(".authorized-real-recovery"))
                .ok()
                .as_deref()
                != Some(b"real-recovery.v1")
        {
            return Err("UNSAFE_RUN_CLEANUP");
        }
        for _ in 0..10 {
            if fs::remove_dir_all(&self.root).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("REAL_RECOVERY_CLEANUP_FAILED")
    }
}

struct Instance {
    child: Child,
    address: SocketAddr,
    root: PathBuf,
}
impl Instance {
    fn start(executable: &Path, root: &Path, profile: &Path, passive: bool) -> Result<Self> {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| "LOCAL_PORT_UNAVAILABLE")?;
        let address = listener
            .local_addr()
            .map_err(|_| "LOCAL_PORT_UNAVAILABLE")?;
        drop(listener);
        let mut command = Command::new(executable);
        command
            .current_dir(executable.parent().ok_or("EXECUTABLE_UNAVAILABLE")?)
            .env("WECOM_ARCHIVE_SERVER_DATA", root)
            .env("WECOM_ARCHIVE_SERVER_LISTEN", address.to_string())
            .env("WECOM_ARCHIVE_SERVER_ALLOW_REMOTE", "false")
            .env("WECOM_ARCHIVE_SERVER_LOG", "off")
            .env("WECOM_ARCHIVE_COLLECTOR_LOG", "off")
            .env("WECOM_ARCHIVE_PROBE_EXE", executable)
            .env("WEBVIEW2_USER_DATA_FOLDER", profile)
            .env(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--disable-background-networking",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        source_windows::updater::hide(&mut command);
        let child = command
            .spawn()
            .map_err(|_| "ISOLATED_INSTANCE_START_FAILED")?;
        let mut instance = Self {
            child,
            address,
            root: root.into(),
        };
        if passive {
            std::thread::sleep(Duration::from_secs(3));
            check(
                instance
                    .child
                    .try_wait()
                    .map_err(|_| "INSTANCE_STATUS_FAILED")?
                    .is_none()
                    && TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_err(),
                "passive-instance-does-not-listen",
            )?;
        } else {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                if instance
                    .child
                    .try_wait()
                    .map_err(|_| "INSTANCE_STATUS_FAILED")?
                    .is_some()
                {
                    return Err("ISOLATED_INSTANCE_EXITED");
                }
                if request(address, None, "GET", "/api/v1/health", b"{}").is_ok_and(|r| r.0 == 200)
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("ISOLATED_INSTANCE_START_TIMEOUT");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        Ok(instance)
    }
    fn api(&self, method: &str, path: &str, body: &Value) -> Result<(u16, Value)> {
        let bytes = Zeroizing::new(serde_json::to_vec(body).map_err(|_| "REQUEST_ENCODE_FAILED")?);
        self.raw(method, path, &bytes)
    }
    fn raw(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Value)> {
        let protected = fs::read(self.root.join("access-token.dpapi"))
            .map_err(|_| "ISOLATED_TOKEN_UNAVAILABLE")?;
        let token = Zeroizing::new(
            source_windows::secrets::unprotect(&protected)
                .map_err(|_| "ISOLATED_TOKEN_UNAVAILABLE")?,
        );
        let token = std::str::from_utf8(&token).map_err(|_| "ISOLATED_TOKEN_INVALID")?;
        request(self.address, Some(token), method, path, body)
    }
    fn stop(&mut self) -> Result<()> {
        // Only the child created by this drill is stopped. Collection/import
        // responses have completed before intentionally exercising WAL recovery.
        if self
            .child
            .try_wait()
            .map_err(|_| "INSTANCE_STATUS_FAILED")?
            .is_none()
        {
            self.child
                .kill()
                .map_err(|_| "ISOLATED_INSTANCE_STOP_FAILED")?;
        }
        self.child
            .wait()
            .map_err(|_| "ISOLATED_INSTANCE_STOP_FAILED")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect_timeout(&self.address, Duration::from_millis(200)).is_ok() {
            if Instant::now() > deadline {
                return Err("ISOLATED_PORT_NOT_CLOSED");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn request(
    address: SocketAddr,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<(u16, Value)> {
    if !address.ip().is_loopback()
        || !path.starts_with("/api/v1/")
        || path.chars().any(char::is_control)
        || !matches!(method, "GET" | "POST" | "PUT")
        || token.is_some_and(|t| t.chars().any(char::is_control))
    {
        return Err("UNSAFE_LOCAL_REQUEST");
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(|_| "LOCAL_CONNECT_FAILED")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15 * 60)))
        .map_err(|_| "LOCAL_TIMEOUT_FAILED")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|_| "LOCAL_TIMEOUT_FAILED")?;
    let auth = Zeroizing::new(
        token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default(),
    );
    let header = Zeroizing::new(format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        auth.as_str(),
        body.len()
    ));
    stream
        .write_all(header.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|_| "LOCAL_REQUEST_FAILED")?;
    let mut response = Zeroizing::new(Vec::new());
    stream
        .take(128 * 1024)
        .read_to_end(&mut response)
        .map_err(|_| "LOCAL_RESPONSE_FAILED")?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("LOCAL_RESPONSE_INVALID")?;
    let header = std::str::from_utf8(&response[..split]).map_err(|_| "LOCAL_RESPONSE_INVALID")?;
    let status = header
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("LOCAL_RESPONSE_INVALID")?;
    let value =
        serde_json::from_slice(&response[split + 4..]).map_err(|_| "LOCAL_RESPONSE_INVALID")?;
    Ok((status, value))
}

fn connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "DRILL_DB_UNAVAILABLE")?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|_| "DRILL_DB_UNAVAILABLE")?;
    Ok(connection)
}

fn database_fingerprint(path: &Path) -> Result<BTreeMap<String, (u64, Vec<u8>)>> {
    let db = connection(path)?;
    let mut tables = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_stat%' ORDER BY name").map_err(|_| "DB_FINGERPRINT_FAILED")?;
    let names = tables
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| "DB_FINGERPRINT_FAILED")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| "DB_FINGERPRINT_FAILED")?;
    let mut result = BTreeMap::new();
    for name in names {
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let columns = db
            .prepare(&format!("SELECT * FROM {quoted} LIMIT 0"))
            .map_err(|_| "DB_FINGERPRINT_FAILED")?
            .column_count();
        let order = (1..=columns)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut query = db
            .prepare(&format!("SELECT * FROM {quoted} ORDER BY {order}"))
            .map_err(|_| "DB_FINGERPRINT_FAILED")?;
        let mut rows = query.query([]).map_err(|_| "DB_FINGERPRINT_FAILED")?;
        let mut hash = Sha256::new();
        let mut count = 0_u64;
        while let Some(row) = rows.next().map_err(|_| "DB_FINGERPRINT_FAILED")? {
            for column in 0..columns {
                match row.get_ref(column).map_err(|_| "DB_FINGERPRINT_FAILED")? {
                    ValueRef::Null => hash.update([0]),
                    ValueRef::Integer(v) => {
                        hash.update([1]);
                        hash.update(v.to_le_bytes());
                    }
                    ValueRef::Real(v) => {
                        hash.update([2]);
                        hash.update(v.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(v) => {
                        hash.update([3]);
                        hash.update((v.len() as u64).to_le_bytes());
                        hash.update(v);
                    }
                    ValueRef::Blob(v) => {
                        hash.update([4]);
                        hash.update((v.len() as u64).to_le_bytes());
                        hash.update(v);
                    }
                }
            }
            count += 1;
        }
        result.insert(name, (count, hash.finalize().to_vec()));
    }
    Ok(result)
}

fn file_hash(path: &Path) -> Result<Vec<u8>> {
    let mut input = File::open(path).map_err(|_| "FILE_FINGERPRINT_FAILED")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| "FILE_FINGERPRINT_FAILED")?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_vec())
}

fn tree_fingerprint(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut folders = vec![root.to_path_buf()];
    let mut result = BTreeMap::new();
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(folder).map_err(|_| "TREE_FINGERPRINT_FAILED")? {
            let path = entry.map_err(|_| "TREE_FINGERPRINT_FAILED")?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|_| "TREE_FINGERPRINT_FAILED")?;
            if source_windows::atomic_file::is_link(&metadata) {
                return Err("UNEXPECTED_DRILL_LINK");
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "TREE_FINGERPRINT_FAILED")?;
            if relative.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("workspace.lock" | "archive.db-wal" | "archive.db-shm" | "work")
                )
            }) {
                continue;
            }
            if metadata.is_dir() {
                folders.push(path);
            } else {
                result.insert(relative.into(), file_hash(&path)?);
            }
        }
    }
    Ok(result)
}

fn batch_for(db: &Connection, message: MessageV1, new_identity: bool) -> Result<ArchiveBatchV1> {
    let mut batch = db.query_row("SELECT source_adapter, source_instance_id, collected_at, employee_notice_json, external_consent, collection_scope_json, cursor_before, cursor_after, retention_json FROM collection_batches WHERE batch_id=?1", [message.collection_batch_id.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, String>(8)?))
    }).map_err(|_| "REAL_BATCH_TEMPLATE_UNAVAILABLE")?;
    let original_id = message.collection_batch_id;
    let batch_id = if new_identity {
        Uuid::new_v4()
    } else {
        original_id
    };
    let mut message = message;
    message.collection_batch_id = batch_id;
    archive_transfer::sanitize_export_message(&mut message);
    let messages = vec![message];
    Ok(ArchiveBatchV1 {
        schema_version: archive_domain::BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: std::mem::take(&mut batch.0),
        source_instance_id: batch.1,
        collected_at: if new_identity {
            Utc::now()
        } else {
            batch.2.parse().map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?
        },
        employee_notice: serde_json::from_str(&batch.3)
            .map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?,
        external_contact_consent: serde_json::from_value(Value::String(batch.4))
            .map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?,
        collection_scope: serde_json::from_str(&batch.5)
            .map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?,
        cursor_before: batch.6,
        cursor_after: batch.7,
        content_sha256: archive_transfer::json_sha256(&messages)
            .map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?,
        messages,
        retention: serde_json::from_str(&batch.8).map_err(|_| "REAL_BATCH_TEMPLATE_INVALID")?,
    })
}

fn export_for(
    batches: Vec<ArchiveBatchV1>,
    media: Vec<MediaBlobV1>,
    reject_directory: bool,
) -> Result<ClientExportV1> {
    let mut conversations = BTreeMap::new();
    for message in batches.iter().flat_map(|b| &b.messages) {
        conversations
            .entry(message.conversation_id.clone())
            .or_insert_with(|| ConversationV1 {
                conversation_id: message.conversation_id.clone(),
                display_name: None,
                conversation_type: None,
                participant_ids: vec![],
            });
    }
    let participants = if reject_directory {
        vec![ParticipantV1 {
            participant_id: "__authorized_recovery_failure__".into(),
            display_name: None,
            participant_kind: None,
        }]
    } else {
        vec![]
    };
    archive_transfer::create_export_with_media(
        "authorized-local-recovery",
        batches,
        conversations.into_values().collect(),
        participants,
        media,
    )
    .map_err(|_| "REAL_EXPORT_TEMPLATE_INVALID")
}

fn import(instance: &Instance, export: &ClientExportV1) -> Result<(u16, Value)> {
    instance.raw(
        "POST",
        "/api/v1/imports/json",
        &Zeroizing::new(serde_json::to_vec(export).map_err(|_| "REAL_EXPORT_ENCODING_FAILED")?),
    )
}

fn preflight_input(source: &Path, run: &OwnedRun) -> Result<()> {
    let db = connection(&source.join("archive.db"))?;
    let payload: String = db.query_row("SELECT payload_json FROM messages WHERE message_type='text' AND length(body_text)>0 LIMIT 1", [], |r| r.get(0)).map_err(|_| "REAL_TEXT_SAMPLE_UNAVAILABLE")?;
    let message = serde_json::from_str(&payload).map_err(|_| "REAL_TEXT_SAMPLE_INVALID")?;
    let export = export_for(vec![batch_for(&db, message, true)?], vec![], false)?;
    let path = run.root.join("input-preflight.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "INPUT_PREFLIGHT_UNAVAILABLE")?;
    serde_json::to_writer(&mut file, &export).map_err(|_| "INPUT_PREFLIGHT_ENCODING_FAILED")?;
    file.sync_all()
        .map_err(|_| "INPUT_PREFLIGHT_ENCODING_FAILED")?;
    drop(file);
    let valid = archive_transfer::read_json(&path).is_ok();
    fs::remove_file(path).map_err(|_| "INPUT_PREFLIGHT_CLEANUP_FAILED")?;
    check(valid, "real-import-fixture-passes-production-validation")
}

struct CorruptedByte {
    path: PathBuf,
    original: u8,
}
impl CorruptedByte {
    fn new(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| "DRILL_MEDIA_UNAVAILABLE")?;
        let mut byte = [0];
        file.read_exact(&mut byte)
            .map_err(|_| "DRILL_MEDIA_UNAVAILABLE")?;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&[byte[0] ^ 1]))
            .and_then(|_| file.sync_all())
            .map_err(|_| "DRILL_MEDIA_CORRUPTION_FAILED")?;
        Ok(Self {
            path: path.into(),
            original: byte[0],
        })
    }
    fn repair(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|_| "DRILL_MEDIA_REPAIR_FAILED")?;
        file.write_all(&[self.original])
            .and_then(|_| file.sync_all())
            .map_err(|_| "DRILL_MEDIA_REPAIR_FAILED")
    }
}
impl Drop for CorruptedByte {
    fn drop(&mut self) {
        let _ = self.repair();
    }
}

fn exercise_imports(instance: &Instance) -> Result<()> {
    println!("STAGE real-import-failure-drills");
    let path = instance.root.join("archive.db");
    let baseline = database_fingerprint(&path)?;
    let db = connection(&path)?;
    let payload: String = db.query_row("SELECT payload_json FROM messages WHERE message_type='text' AND length(body_text)>0 LIMIT 1", [], |r| r.get(0)).map_err(|_| "REAL_TEXT_SAMPLE_UNAVAILABLE")?;
    let mut sample: MessageV1 =
        serde_json::from_str(&payload).map_err(|_| "REAL_TEXT_SAMPLE_INVALID")?;
    sample.body_text = Some(format!(
        "{} [authorized rollback drill]",
        sample.body_text.as_deref().unwrap_or_default()
    ));
    let first = batch_for(&db, sample.clone(), true)?;
    let conflict = batch_for(&db, sample, false)?;
    let export = export_for(vec![first.clone(), conflict], vec![], false)?;
    let response = import(instance, &export)?;
    check(
        response.0 == 409
            && response.1["code"] == "BATCH_CONFLICT"
            && database_fingerprint(&path)? == baseline,
        "real-late-batch-conflict-rolls-back-all-tables",
    )?;

    let writer = Connection::open(&path).map_err(|_| "DRILL_DB_UNAVAILABLE")?;
    writer.execute_batch("CREATE TRIGGER authorized_recovery_reject_directory BEFORE INSERT ON participants WHEN new.participant_id='__authorized_recovery_failure__' BEGIN SELECT RAISE(ABORT, 'AUTHORIZED_RECOVERY_DRILL'); END;").map_err(|_| "DRILL_TRIGGER_FAILED")?;
    let response = import(instance, &export_for(vec![first.clone()], vec![], true)?)?;
    writer
        .execute_batch("DROP TRIGGER authorized_recovery_reject_directory;")
        .map_err(|_| "DRILL_TRIGGER_CLEANUP_FAILED")?;
    check(
        response.0 == 500
            && response.1["code"] == "ARCHIVE_STORE_FAILED"
            && database_fingerprint(&path)? == baseline,
        "real-directory-write-failure-rolls-back-all-tables",
    )?;

    let mut invalid = export_for(vec![first.clone()], vec![], false)?;
    invalid.content_sha256 = "0".repeat(64);
    let response = import(instance, &invalid)?;
    check(
        response.0 == 422 && database_fingerprint(&path)? == baseline,
        "real-invalid-checksum-preserves-archive",
    )?;

    let (hash, payload): (String, String) = db.query_row("SELECT m.content_sha256, msg.payload_json FROM media m JOIN message_media mm ON mm.media_id=m.media_id JOIN messages msg ON msg.stable_message_id=mm.stable_message_id WHERE m.integrity='verified' AND m.size_bytes>0 AND m.size_bytes<=1048576 LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|_| "REAL_MEDIA_SAMPLE_UNAVAILABLE")?;
    let media_path = instance.root.join("media").join(&hash);
    let bytes = Zeroizing::new(fs::read(&media_path).map_err(|_| "REAL_MEDIA_SAMPLE_UNAVAILABLE")?);
    let message: MessageV1 =
        serde_json::from_str(&payload).map_err(|_| "REAL_MEDIA_SAMPLE_INVALID")?;
    let media_export = export_for(
        vec![batch_for(&db, message, true)?],
        vec![MediaBlobV1 {
            content_hash: hash,
            original_name: None,
            mime_type: None,
            size_bytes: bytes.len() as u64,
            content_hex: hex::encode(bytes.as_slice()),
        }],
        false,
    )?;
    let original_hash = file_hash(&media_path)?;
    let corrupt = CorruptedByte::new(&media_path)?;
    let damaged_hash = file_hash(&media_path)?;
    let response = import(instance, &media_export)?;
    check(
        response.0 == 500
            && response.1["code"] == "ARCHIVE_STORE_FAILED"
            && file_hash(&media_path)? == damaged_hash
            && database_fingerprint(&path)? == baseline,
        "real-corrupt-existing-media-is-rejected-without-overwrite",
    )?;
    corrupt.repair()?;
    check(
        file_hash(&media_path)? == original_hash,
        "real-drill-media-repaired",
    )?;
    drop(corrupt);
    drop(writer);
    drop(db);

    let retry = export_for(vec![first], vec![], false)?;
    let response = import(instance, &retry)?;
    check(
        response.0 == 200 && response.1["revised"] == 1,
        "real-failed-import-retry-succeeds",
    )?;
    let after = database_fingerprint(&path)?;
    let response = import(instance, &retry)?;
    let repeated = database_fingerprint(&path)?;
    check(
        response.0 == 200
            && response.1["unchanged"] == 1
            && after.get("messages") == repeated.get("messages")
            && after.get("message_revisions") == repeated.get("message_revisions"),
        "real-import-repeat-is-idempotent",
    )?;
    let db = connection(&path)?;
    check(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .map_err(|_| "REAL_DB_INTEGRITY_FAILED")?
            == "ok",
        "real-post-failure-database-integrity",
    )?;
    Ok(())
}

fn run_inner(workspace: &Path, run: &OwnedRun, resume: Option<&OwnedRun>) -> Result<()> {
    let app = run.root.join("app");
    fs::create_dir(&app).map_err(|_| "RUN_APP_UNAVAILABLE")?;
    let executable = app.join("WeComArchive.exe");
    fs::copy(
        workspace.join("target/release/WeComArchive.exe"),
        &executable,
    )
    .map_err(|_| "RELEASE_EXE_UNAVAILABLE")?;
    let source_root = resume.unwrap_or(run).root.join("source-data");
    if resume.is_some() {
        check(true, "authorized-existing-isolated-source-reused")?;
    } else {
        let mut source = Instance::start(
            &executable,
            &source_root,
            &run.root.join("webview-source"),
            false,
        )?;
        let origin = format!("http://{}", source.address);
        let response = source.api("PUT", "/api/v1/enterprise/config", &json!({"organizationName":"Authorized offline recovery drill", "uploadUrl":origin, "superAdminEnabled":true, "dataRedaction":true, "serverLogLevel":"off", "collectorLogLevel":"off", "collectorSchedule":{"mode":"disabled","intervalMinutes":60,"dailyTime":"02:00"}}))?;
        check(response.0 == 200, "isolated-real-archive-configured")?;
        println!("STAGE collecting-real-data-and-media-readonly");
        let response = source.api(
            "POST",
            "/api/v1/collections/local?include_media=true&data_redaction=true",
            &json!({}),
        )?;
        check(
            response.0 == 200
                && response.1["inserted"].as_u64().is_some_and(|n| n > 0)
                && response.1["mediaCount"].as_u64().is_some_and(|n| n > 0),
            "real-data-and-media-collected",
        )?;
        check(
            source
                .api("POST", "/api/v1/enterprise/collectors", &json!({}))?
                .0
                == 200,
            "isolated-disabled-schedule-collector-generated",
        )?;
        let daily = (Local::now() + TimeDelta::hours(12))
            .format("%H:%M")
            .to_string();
        check(source.api("POST", "/api/v1/collection-schedules", &json!({"name":"Authorized recovery drill", "includeMedia":false, "schedule":{"mode":"daily","intervalMinutes":60,"dailyTime":daily}}))?.0 == 200, "isolated-plan-created-for-recovery-pause-check")?;
        source.stop()?;
    }
    println!("STAGE backup-real-archive");
    preflight_input(&source_root, run)?;
    let before = database_fingerprint(&source_root.join("archive.db"))?;
    let source_files = tree_fingerprint(&source_root)?;
    let backup_parent = workspace.join("userData/verified-backups");
    fs::create_dir_all(&backup_parent).map_err(|_| "BACKUP_PARENT_UNAVAILABLE")?;
    let backup = if let Some(existing) = std::env::var_os("WECOM_ARCHIVE_REAL_RECOVERY_BACKUP") {
        let existing = PathBuf::from(existing)
            .canonicalize()
            .map_err(|_| "REUSED_BACKUP_UNAVAILABLE")?;
        if existing.parent()
            != Some(
                backup_parent
                    .canonicalize()
                    .map_err(|_| "BACKUP_PARENT_UNAVAILABLE")?
                    .as_path(),
            )
            || !existing
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with("WeComArchive-backup-"))
        {
            return Err("UNSAFE_REUSED_BACKUP");
        }
        let result = maintenance::verify_backup(&existing).map_err(|_| "REUSED_BACKUP_INVALID")?;
        check(
            database_fingerprint(&existing.join("serverData/archive.db"))? == before,
            "retained-backup-matches-isolated-source",
        )?;
        result
    } else {
        maintenance::create_backup(&source_root, &executable, &backup_parent)
            .map_err(|_| "REAL_BACKUP_FAILED")?
    };
    let backup_root = PathBuf::from(&backup.directory);
    println!(
        "BACKUP {}",
        backup_root
            .strip_prefix(workspace)
            .map_err(|_| "UNSAFE_BACKUP_LOCATION")?
            .display()
    );
    check(
        maintenance::verify_backup(&backup_root).is_ok(),
        "real-complete-backup-verified",
    )?;
    check(
        database_fingerprint(&source_root.join("archive.db"))? == before
            && tree_fingerprint(&source_root)? == source_files,
        "real-source-archive-and-media-unchanged",
    )?;
    let immutable = tree_fingerprint(&backup_root)?;
    let mut backup_view = Instance::start(
        &backup_root.join("WeComArchive.exe"),
        &backup_root.join("serverData"),
        &run.root.join("webview-backup"),
        true,
    )?;
    backup_view.stop()?;
    check(
        tree_fingerprint(&backup_root)? == immutable,
        "opening-real-backup-does-not-modify-backup",
    )?;

    println!("STAGE restore-real-archive-to-independent-directory");
    let restores = run.root.join("restores");
    fs::create_dir(&restores).map_err(|_| "RESTORE_PARENT_UNAVAILABLE")?;
    let restored =
        maintenance::restore_backup(&backup_root, &restores).map_err(|_| "REAL_RESTORE_FAILED")?;
    let restored = PathBuf::from(restored.directory);
    let data = restored.join("serverData");
    check(
        database_fingerprint(&data.join("archive.db"))? == before,
        "real-restored-all-table-content-matches",
    )?;
    check(
        file_hash(&data.join("enterprise-config.json"))?
            == file_hash(&source_root.join("enterprise-config.json"))?
            && file_hash(&data.join("access-token.dpapi"))?
                == file_hash(&source_root.join("access-token.dpapi"))?,
        "real-restored-identity-and-config-match",
    )?;
    let mut pending = Instance::start(
        &restored.join("WeComArchive.exe"),
        &data,
        &run.root.join("webview-pending"),
        true,
    )?;
    pending.stop()?;
    check(
        data.join("restore-pending.json").is_file(),
        "real-restore-requires-confirmation",
    )?;
    maintenance::activate_restored(&data).map_err(|_| "REAL_RESTORE_CONFIRMATION_FAILED")?;
    let config: Value = serde_json::from_slice(
        &fs::read(data.join("enterprise-config.json"))
            .map_err(|_| "RESTORED_CONFIG_UNAVAILABLE")?,
    )
    .map_err(|_| "RESTORED_CONFIG_INVALID")?;
    check(
        config["localCollectionPlans"].as_array().is_some_and(|p| {
            !p.is_empty() && p.iter().all(|v| v["schedule"]["mode"] == "disabled")
        }) && config["collectors"]
            .as_array()
            .is_some_and(|c| !c.is_empty() && c.iter().all(|v| v["enabled"] == false))
            && config["uploadUrl"] == "http://127.0.0.1:9812",
        "real-restored-plans-and-devices-paused",
    )?;
    let mut restored_instance = Instance::start(
        &restored.join("WeComArchive.exe"),
        &data,
        &run.root.join("webview-restored"),
        false,
    )?;
    exercise_imports(&restored_instance)?;
    restored_instance.stop()?;
    check(
        maintenance::verify_backup(&backup_root).is_ok()
            && tree_fingerprint(&backup_root)? == immutable,
        "real-retained-backup-still-valid-after-failure-drills",
    )?;
    check(
        database_fingerprint(&source_root.join("archive.db"))? == before
            && tree_fingerprint(&source_root)? == source_files,
        "real-source-still-unchanged-after-failure-drills",
    )?;
    println!(
        "COUNTS messages={} media={} backup_files={} backup_bytes={}",
        before.get("messages").map_or(0, |v| v.0),
        before.get("media").map_or(0, |v| v.0),
        backup.file_count,
        backup.bytes
    );
    Ok(())
}

fn run() -> Result<()> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("WORKSPACE_UNAVAILABLE")?
        .canonicalize()
        .map_err(|_| "WORKSPACE_UNAVAILABLE")?;
    let run = OwnedRun::new(&workspace)?;
    let resume = std::env::var_os("WECOM_ARCHIVE_REAL_RECOVERY_SOURCE_RUN")
        .map(|path| OwnedRun::resume(&workspace, &PathBuf::from(path)))
        .transpose()?;
    let outcome = run_inner(&workspace, &run, resume.as_ref());
    let cleanup = run.cleanup();
    check(cleanup.is_ok(), "real-temporary-data-and-profiles-cleaned")?;
    if outcome.is_ok()
        && let Some(resume) = resume
    {
        check(
            resume.cleanup().is_ok(),
            "reused-real-source-temporary-data-cleaned",
        )?;
    }
    outcome
}
