use crate::collection::{
    CollectionAuthorization, CollectionReadOptions, MediaCollectionOptions, collect_candidate,
    collect_candidate_with_options, collect_candidate_with_options_since, collect_local_export,
    collect_local_export_internal, notice_evidence_hash,
};
use crate::collector_config::{
    CollectionSchedule, CollectionScheduleMode, EnterpriseCollectorConfig,
    collector_control_payload, verify_collector_control_envelope,
};
use crate::collector_control::collector_heartbeat_failure;
use crate::collector_schedule::{CollectorScheduleTimer, scheduled_collect_and_upload};
use crate::collector_transfer::{
    has_upload_changes, record_upload_success, redact_sensitive_export, redact_sensitive_json,
    redact_sensitive_text,
};
use crate::commands::{UploadResult, persist_offline_package};
use crate::paths::{
    MAX_REMEMBERED_SOURCE_ROOTS, REMEMBERED_SOURCE_ROOTS_SCHEMA, RememberedSourceRoots,
    collection_roots, create_work_root, load_remembered_source_roots, local_data_root,
    remember_source_root,
};
use crate::source_directory::{
    DisplayNameIndex, collect_display_names, collect_group_announcements, conversation_type,
    decode_participant_profile, direct_conversation_name, document_conversation_metadata,
};
use crate::source_keys::{
    SourceKey, SourceKeyKind, choose_collectable_source, key_opens_candidate, open_source_database,
    run_isolated_key_probe, validation_page_prefix,
};
use crate::source_media::{
    MediaResolver, local_media_paths, resolve_media_metadata_with_local_paths,
};
use crate::source_messages::readable_message_payload;
use crate::source_protobuf::{decode_wecom_body, decode_wecom_metadata_body};
use crate::source_sql::source_timestamp_cutoff;
use crate::state::{
    AppState, CommandError, begin_collector_attempt, finish_collector_attempt, internal_error,
};
use crate::{collector_http, sqlite3mc};
use archive_domain::{
    MediaIntegrity, MessageType, SourceAdapter, SourceCandidate, SourceCapability,
};
use chrono::{Local, TimeDelta, TimeZone, Utc};
use message_parser::{RawMessageRow, normalize};
use source_windows::WindowsSourceAdapter;
use source_windows::roots::normalize_source_root;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[test]
fn expired_schedule_waits_for_config_refresh_without_accessing_sources() {
    let config: EnterpriseCollectorConfig = serde_json::from_value(serde_json::json!({
        "schemaVersion": "enterprise-collector.v1",
        "organizationId": "synthetic", "organizationName": "synthetic",
        "collectionNotice": "synthetic", "keyId": "synthetic",
        "publicKeyHex": "", "signingPublicKeyHex": "", "signatureHex": "",
        "uploadUrl": "http://127.0.0.1:1/", "uploadToken": "synthetic",
        "configExpiresAt": (Utc::now() - TimeDelta::minutes(1)).to_rfc3339(),
    }))
    .unwrap();
    let state = AppState::default();
    assert!(matches!(
        scheduled_collect_and_upload(&state, &config, false, false),
        Err(CommandError {
            code: "ENTERPRISE_CONFIG_EXPIRED",
            ..
        })
    ));
    assert!(!state.scheduler_shutdown.load(Ordering::Acquire));
    assert!(state.latest_export.lock().unwrap().is_none());
}

#[test]
fn transient_heartbeat_failures_keep_the_collector_alive() {
    let state = AppState::default();
    for (status, body) in [
        (503, br#"{"code":"UNAUTHORIZED"}"#.as_slice()),
        (502, b"upstream unavailable".as_slice()),
        (401, b"proxy authentication".as_slice()),
    ] {
        let error = collector_heartbeat_failure(
            &collector_http::Response {
                status,
                body: body.to_vec(),
            },
            Some(&state.schedule_status),
            Some(&state),
        );
        assert!(error.recoverable);
        assert!(!state.scheduler_shutdown.load(Ordering::Acquire));
        assert!(
            !state
                .schedule_status
                .lock()
                .unwrap()
                .server_authorization_denied
        );
    }
    let error = collector_heartbeat_failure(
        &collector_http::Response {
            status: 401,
            body: br#"{"code":"UNAUTHORIZED"}"#.to_vec(),
        },
        Some(&state.schedule_status),
        Some(&state),
    );
    assert!(!error.recoverable);
    assert!(state.scheduler_shutdown.load(Ordering::Acquire));
    assert!(
        state
            .schedule_status
            .lock()
            .unwrap()
            .server_authorization_denied
    );
}

#[test]
fn collector_schedule_enabling_runs_now_before_waiting_for_the_interval() {
    let now = Local.with_ymd_and_hms(2026, 9, 24, 9, 0, 0).unwrap();
    let disabled = CollectionSchedule {
        mode: CollectionScheduleMode::Disabled,
        interval_minutes: 60,
        daily_time: "02:00".into(),
    };
    let enabled = CollectionSchedule {
        mode: CollectionScheduleMode::Interval,
        ..disabled.clone()
    };
    let mut timer = CollectorScheduleTimer::new(disabled.clone(), now);
    assert_eq!(timer.next_run, None);
    assert!(timer.update(&enabled, now));
    assert_eq!(timer.next_run, Some(now));
    assert!(timer.activation_pending);

    // A busy collector receives the same settings on the next heartbeat.
    // Its first due time must remain pending rather than skipping one hour.
    let later = now + TimeDelta::seconds(15);
    assert!(!timer.update(&enabled, later));
    assert_eq!(timer.next_run, Some(now));
    assert!(timer.activation_pending);
    let changed = CollectionSchedule {
        interval_minutes: 30,
        ..enabled.clone()
    };
    assert!(timer.update(&changed, later));
    assert!(timer.activation_pending);
    assert_eq!(timer.next_run, Some(later));

    timer.completed(later);
    assert!(!timer.activation_pending);
    assert_eq!(timer.next_run, Some(later + TimeDelta::minutes(30)));
    assert!(!timer.update(&changed, later + TimeDelta::seconds(15)));
    assert_eq!(timer.next_run, Some(later + TimeDelta::minutes(30)));
    assert!(timer.update(&disabled, later));
    assert_eq!(timer.next_run, None);
    assert!(timer.update(&enabled, later));
    assert_eq!(timer.next_run, Some(later));
}

#[test]
fn collector_schedule_daily_enable_runs_now_then_returns_to_the_daily_time() {
    let now = Local.with_ymd_and_hms(2026, 9, 24, 9, 0, 0).unwrap();
    let disabled = CollectionSchedule {
        mode: CollectionScheduleMode::Disabled,
        interval_minutes: 60,
        daily_time: "20:00".into(),
    };
    let enabled = CollectionSchedule {
        mode: CollectionScheduleMode::Daily,
        ..disabled.clone()
    };
    let mut timer = CollectorScheduleTimer::new(disabled, now);
    assert!(timer.update(&enabled, now));
    assert_eq!(timer.next_run, Some(now));
    timer.completed(now + TimeDelta::minutes(1));
    assert_eq!(
        timer.next_run,
        Some(Local.with_ymd_and_hms(2026, 9, 24, 20, 0, 0).unwrap())
    );
    let already_enabled = CollectorScheduleTimer::new(enabled, now);
    assert!(!already_enabled.activation_pending);
    assert_eq!(already_enabled.next_run, timer.next_run);
}

#[test]
fn collector_initial_empty_export_is_uploaded_before_deduplication() {
    let state = AppState::default();
    let export = archive_transfer::create_export("test-client", vec![], vec![], vec![]).unwrap();
    assert!(has_upload_changes(&state, &export));
    record_upload_success(&state, &export);
    state.schedule_status.lock().unwrap().last_success_at = Some("2026-09-24T01:00:00Z".into());
    assert!(!has_upload_changes(&state, &export));
}

#[test]
fn collector_attempt_tracks_first_run_failures_and_no_change_retries() {
    let state = AppState::default();
    assert!(
        state
            .schedule_status
            .lock()
            .unwrap()
            .last_attempt_at
            .is_none()
    );

    begin_collector_attempt(&state);
    let first_attempt = {
        let status = state.schedule_status.lock().unwrap();
        assert!(status.running);
        let attempt = status.last_attempt_at.clone().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(&attempt).is_ok());
        attempt
    };
    finish_collector_attempt::<()>(&state, &Err(internal_error()));
    {
        let mut status = state.schedule_status.lock().unwrap();
        assert!(!status.running);
        assert!(status.last_error.is_some());
        assert_eq!(
            status.last_attempt_at.as_deref(),
            Some(first_attempt.as_str())
        );
        status.last_success_at = Some("2026-09-24T10:00:00Z".into());
    }

    begin_collector_attempt(&state);
    // A scan without new messages still counts as an execution, while the
    // successful-upload timestamp continues to describe the last upload.
    finish_collector_attempt(&state, &Ok(UploadResult { message_count: 0 }));
    let status = state.schedule_status.lock().unwrap();
    assert!(!status.running);
    assert!(status.last_attempt_at.is_some());
    assert!(status.last_error.is_none());
    assert_eq!(
        status.last_success_at.as_deref(),
        Some("2026-09-24T10:00:00Z")
    );
}

#[test]
fn custom_source_root_rejects_drive_roots_and_normalizes_explorer_paste() {
    assert_eq!(normalize_source_root("   "), None);
    assert_eq!(normalize_source_root("WXWork"), None);
    assert_eq!(normalize_source_root("D:\\"), None);
    assert_eq!(
        normalize_source_root("  \"D:/WXWork/\"  "),
        Some(PathBuf::from(r"D:\WXWork"))
    );
}

#[test]
fn custom_source_root_limits_collection_to_the_explicit_directory() {
    let roots = collection_roots(
        Some("D:/WXWork/"),
        &[PathBuf::from(r"E:\WXWork"), PathBuf::from(r"D:\WXWork")],
        &[
            PathBuf::from(r"C:\Users\mia\Documents\WXWork"),
            PathBuf::from(r"D:\WXWork"),
        ],
    );
    assert_eq!(roots, vec![PathBuf::from(r"D:\WXWork")]);
    assert!(collection_roots(Some(r"D:\"), &roots, &roots).is_empty());
}

#[test]
fn invalid_explicit_roots_return_a_retry_error_before_automatic_discovery() {
    for raw in [r"C:\", "relative", r"\\server\share\WXWork"] {
        assert_eq!(
            collect_local_export(false, false, Some(raw.to_owned())).unwrap_err(),
            "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。"
        );
    }
}
use rusqlite::Connection;

struct TestDirectory(PathBuf);

#[test]
fn offline_package_preserves_existing_files_and_cleans_own_partial_before_retry() {
    let directory = TestDirectory::new();
    let partial = directory.0.join(".synthetic.wca.partial");
    let target = directory.0.join("synthetic.wca");
    fs::write(&partial, b"previous synthetic export").unwrap();
    assert!(persist_offline_package(b"new synthetic export", &partial, &target).is_err());
    assert_eq!(fs::read(&partial).unwrap(), b"previous synthetic export");
    assert!(!target.exists());
    fs::remove_file(&partial).unwrap();

    fs::write(&target, b"published synthetic export").unwrap();
    assert!(persist_offline_package(b"new synthetic export", &partial, &target).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"published synthetic export");
    assert!(!partial.exists());
    fs::remove_file(&target).unwrap();

    persist_offline_package(b"new synthetic export", &partial, &target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new synthetic export");
    assert!(!partial.exists());
}

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("archive-client-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn local_work_uses_server_data_without_creating_collector_data() {
    let directory = TestDirectory::new();
    let data_root = local_data_root(&directory.0, None);
    assert_eq!(data_root, directory.0.join("serverData"));
    assert!(!data_root.exists());
    let work = create_work_root(&data_root, "local").unwrap();
    assert_eq!(work.parent(), Some(data_root.join("work").as_path()));
    assert!(work.is_dir());
    assert!(!directory.0.join("collectorData").exists());
}

#[test]
fn local_work_respects_the_configured_server_data_directory() {
    let directory = TestDirectory::new();
    let configured = directory.0.join("custom-server-data");
    let data_root = local_data_root(&directory.0, Some(configured.clone()));
    let work = create_work_root(&data_root, "local").unwrap();
    assert!(work.starts_with(configured.join("work")));
    assert!(!directory.0.join("serverData").exists());
    assert!(!directory.0.join("collectorData").exists());
}

#[cfg(windows)]
#[test]
fn local_probe_cleans_up_failed_snapshots_in_server_data() {
    let directory = TestDirectory::new();
    let data_root = local_data_root(&directory.0, None);
    let candidate = SourceCandidate {
        source_id: "synthetic-source".into(),
        display_path: "synthetic-source".into(),
        root_path: directory.0.clone(),
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: directory.0.join("message.db"),
            wal_path: None,
            shm_path: None,
            encrypted: true,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![],
        client_version: None,
        capability: SourceCapability::ProbeRequired,
    };
    // Missing and truncated files cover snapshot failure and prefix-read failure.
    for truncated in [false, true] {
        if truncated {
            fs::write(&candidate.databases[0].path, b"short").unwrap();
        }
        assert!(validation_page_prefix(&candidate, &data_root).is_none());
        assert_eq!(fs::read_dir(data_root.join("work")).unwrap().count(), 0);
        assert!(!directory.0.join("collectorData").exists());
    }
}

#[test]
fn remembered_source_roots_survive_reload_and_discover_non_default_data() {
    let directory = TestDirectory::new();
    let config = directory.0.join("source-roots.dpapi");
    let roots = (0..4)
        .map(|index| directory.0.join(format!("custom-{index}")))
        .collect::<Vec<_>>();
    for root in &roots {
        fs::create_dir_all(root).unwrap();
        remember_source_root(&config, root).unwrap();
    }
    remember_source_root(&config, &roots[2]).unwrap();
    let reloaded = load_remembered_source_roots(&config);
    assert_eq!(
        reloaded,
        vec![roots[2].clone(), roots[3].clone(), roots[1].clone()]
    );
    for name in ["message.db", "session.db", "user.db"] {
        let mut header = [0_u8; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        fs::write(roots[2].join(name), header).unwrap();
    }
    let candidates = source_windows::discover_roots(&collection_roots(None, &reloaded, &[]));
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].root_path, roots[2]);
    fs::remove_dir(&roots[1]).unwrap();
    assert_eq!(
        load_remembered_source_roots(&config),
        vec![roots[2].clone(), roots[3].clone()]
    );
}

#[test]
fn invalid_remembered_source_roots_fall_back_to_detected_roots() {
    let directory = TestDirectory::new();
    let config = directory.0.join("source-roots.dpapi");
    let detected = vec![directory.0.join("automatic")];
    assert!(load_remembered_source_roots(&config).is_empty());
    fs::write(&config, b"damaged DPAPI fixture").unwrap();
    assert_eq!(
        collection_roots(None, &load_remembered_source_roots(&config), &detected),
        detected
    );
    source_windows::dpapi::store_current_user(&config, b"invalid JSON fixture").unwrap();
    assert!(load_remembered_source_roots(&config).is_empty());
    let payload = serde_json::to_vec(&RememberedSourceRoots {
        schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA + 1,
        roots: vec![directory.0.clone()],
    })
    .unwrap();
    source_windows::dpapi::store_current_user(&config, &payload).unwrap();
    assert_eq!(
        collection_roots(None, &load_remembered_source_roots(&config), &detected),
        detected
    );
}

#[test]
fn concurrent_source_root_saves_keep_a_readable_bounded_config() {
    let directory = TestDirectory::new();
    let config = directory.0.join("source-roots.dpapi");
    let handles = (0..4)
        .map(|index| {
            let root = directory.0.join(format!("fixture-{index}"));
            fs::create_dir_all(&root).unwrap();
            let path = config.clone();
            std::thread::spawn(move || remember_source_root(&path, &root))
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(
        load_remembered_source_roots(&config).len(),
        MAX_REMEMBERED_SOURCE_ROOTS
    );
    assert!(!config.with_extension("dpapi.partial").exists());
}

#[test]
fn remembered_roots_revalidate_paths_before_directory_access() {
    let directory = TestDirectory::new();
    let config = directory.0.join("source-roots.dpapi");
    let payload = serde_json::to_vec(&RememberedSourceRoots {
        schema_version: REMEMBERED_SOURCE_ROOTS_SCHEMA,
        roots: vec![
            PathBuf::from(r"C:\"),
            PathBuf::from(r"\\server\share\WXWork"),
            PathBuf::from("relative"),
            directory.0.clone(),
        ],
    })
    .unwrap();
    source_windows::dpapi::store_current_user(&config, &payload).unwrap();
    assert_eq!(
        load_remembered_source_roots(&config),
        vec![directory.0.clone()]
    );
}

fn authorization() -> CollectionAuthorization {
    CollectionAuthorization {
        notice_version: "test-notice.v1".into(),
        notice_displayed_at: Utc::now(),
    }
}

#[test]
fn decodes_labeled_profile_attributes_without_importing_unrelated_payload_fields() {
    fn text_field(field: u64, text: &[u8], out: &mut Vec<u8>) {
        write_test_varint((field << 3) | 2, out);
        write_test_varint(text.len() as u64, out);
        out.extend_from_slice(text);
    }
    let mut raw = vec![8, 42];
    text_field(3, b"person@example.invalid", &mut raw);
    text_field(5, b"13800000000", &mut raw);
    text_field(6, b"010-12345678", &mut raw);
    text_field(50, "及时交付".as_bytes(), &mut raw);
    text_field(55, "项目经理".as_bytes(), &mut raw);
    text_field(11, b"https://example.invalid/private-avatar", &mut raw);
    let mut attributes = Vec::new();
    for (label, value) in [
        ("工号", "TEST-001"),
        ("座机", "1001"),
        ("办公地点", "测试楼A栋"),
    ] {
        let mut entry = Vec::new();
        text_field(1, label.as_bytes(), &mut entry);
        text_field(2, value.as_bytes(), &mut entry);
        text_field(1, &entry, &mut attributes);
    }
    text_field(33, &attributes, &mut raw);
    let profile = decode_participant_profile("42", &raw);
    assert_eq!(profile["email"], "person@example.invalid");
    assert_eq!(profile["mobile"], "13800000000");
    assert_eq!(profile["phone"], "010-12345678");
    assert_eq!(profile["signature"], "及时交付");
    assert_eq!(profile["position"], "项目经理");
    assert_eq!(profile["employee_no"], "TEST-001");
    assert_eq!(profile["office_phone"], "1001");
    assert_eq!(profile["custom:办公地点"], "测试楼A栋");
    assert!(
        !serde_json::to_string(&profile)
            .unwrap()
            .contains("private-avatar")
    );
    assert!(decode_participant_profile("43", &raw).is_empty());
    assert!(decode_participant_profile("42", &raw[..raw.len() - 1]).is_empty());
}

#[test]
fn collects_configured_profiles_and_resolves_department_hierarchy() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(
            "CREATE TABLE user_table (id TEXT, name TEXT, mobile TEXT, office_phone TEXT, email TEXT, number TEXT, corp_id TEXT, position TEXT, external_job TEXT, external_corp_name TEXT, company_name TEXT);
             INSERT INTO user_table VALUES ('member-a', '测试成员甲', '13800000000', '010-00000000', 'member@example.invalid', 'TEST-001', 'corp-a', '', '工程师', '', '');
             INSERT INTO user_table VALUES ('member-b', '测试成员乙', '', '', '', '', 'corp-a', '', '', '', '');
             CREATE TABLE company_group4 (id TEXT, root_department_id TEXT);
             INSERT INTO company_group4 VALUES ('corp-a', 'root');
             CREATE TABLE department_tableV2 (id TEXT, name TEXT, parent_id TEXT, corpany_id TEXT);
             INSERT INTO department_tableV2 VALUES ('root', '测试企业', '0', 'corp-a'), ('sales', '销售部', 'root', 'corp-a'), ('team', '业务组', 'sales', 'corp-a');
             CREATE TABLE user_dept_tableV2 (user_id TEXT, department_id TEXT, job TEXT, is_main_job INTEGER, sort INTEGER);
             INSERT INTO user_dept_tableV2 VALUES ('member-a', 'team', '经理', 1, 0), ('member-b', 'missing', '', 1, 0);"
        ).unwrap();
    let mut index = DisplayNameIndex::default();
    collect_display_names(&connection, &mut index);
    let profile = &index.profiles["member-a"];
    assert_eq!(profile["mobile"], "13800000000");
    assert_eq!(profile["office_phone"], "010-00000000");
    assert_eq!(profile["email"], "member@example.invalid");
    assert_eq!(profile["employee_no"], "TEST-001");
    assert_eq!(profile["department"], "销售部/业务组");
    assert_eq!(profile["organization"], "测试企业");
    assert_eq!(profile["position"], "工程师");
    assert!(!index.profiles["member-b"].contains_key("mobile"));
    assert!(!index.profiles["member-b"].contains_key("department"));
    connection
        .execute("DELETE FROM company_group4", [])
        .unwrap();
    let mut fallback = DisplayNameIndex::default();
    collect_display_names(&connection, &mut fallback);
    assert_eq!(fallback.profiles["member-a"]["organization"], "测试企业");
    assert_eq!(fallback.profiles["member-a"]["department"], "销售部/业务组");
    assert_eq!(conversation_type("T:opaque", 9), "temporary");
    assert_eq!(conversation_type("Y:10024", 1), "system");
}

#[test]
fn collected_contact_numbers_survive_profile_projection_and_redaction() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("source");
    fs::create_dir_all(&source_root).unwrap();
    let message_path = source_root.join("message.db");
    let connection = Connection::open(&message_path).unwrap();
    connection.execute_batch(
            "CREATE TABLE messages (source_message_id TEXT, conversation_id TEXT, sender_id TEXT, sent_at_unix_ms INTEGER, outgoing INTEGER, raw_type TEXT, payload_json TEXT, recalled INTEGER);
             INSERT INTO messages VALUES ('1', 'conversation', '42', 1700000000000, 0, 'text', '{\"text\":\"测试消息\"}', 0);"
        ).unwrap();
    drop(connection);
    let user_path = source_root.join("user.db");
    let connection = Connection::open(&user_path).unwrap();
    connection
        .execute_batch("CREATE TABLE user_table (id TEXT, name TEXT, pb_content BLOB);")
        .unwrap();
    let attribute = test_protobuf_bytes(&[(1, "座机".as_bytes()), (2, b"010-00000000")]);
    let attributes = test_protobuf_bytes(&[(1, &attribute)]);
    let mut raw = vec![8, 42];
    raw.extend(test_protobuf_bytes(&[
        (5, b"13800000000"),
        (6, b"010-12345678"),
        (33, &attributes),
    ]));
    connection
        .execute(
            "INSERT INTO user_table VALUES ('42', '测试成员', ?1)",
            [raw],
        )
        .unwrap();
    drop(connection);
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source_root,
        databases: [("message", message_path), ("user", user_path)]
            .into_iter()
            .map(|(kind, path)| archive_domain::SourceDatabase {
                kind: kind.into(),
                path,
                wal_path: None,
                shm_path: None,
                encrypted: false,
                page_size_hint: Some(4096),
            })
            .collect(),
        media_roots: vec![],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let export = collect_candidate_with_options(
        candidate,
        directory.0.join("work"),
        None,
        authorization(),
        MediaCollectionOptions::default(),
        None,
        None,
    )
    .unwrap();
    let profile = &export.batches[0].messages[0].raw_payload["participant_profiles"]["42"];
    assert_eq!(profile["mobile"], "13800000000");
    assert_eq!(profile["phone"], "010-12345678");
    assert_eq!(profile["office_phone"], "010-00000000");
    let redacted = redact_sensitive_export(export.clone()).unwrap();
    let profile = &redacted.batches[0].messages[0].raw_payload["participant_profiles"]["42"];
    for key in ["mobile", "phone", "office_phone"] {
        assert_eq!(profile[key], "[敏感数据已脱敏]");
    }
}

#[test]
fn maps_known_windows_contact_and_session_tables() {
    let connection = Connection::open_in_memory().unwrap();
    connection
            .execute_batch(
                "CREATE TABLE user_table (id TEXT, name TEXT);
                 INSERT INTO user_table VALUES ('0000000000000000', '联系人甲');
                 CREATE TABLE wechat_contactV1 (wxid TEXT, name TEXT);
                 INSERT INTO wechat_contactV1 VALUES ('wxid_1', '微信联系人');
                 CREATE TABLE conversation_table (id TEXT, name TEXT);
                 INSERT INTO conversation_table VALUES ('S:1_2', '项目会话');
                 CREATE TABLE conversation_user_table (
                    conversation_id TEXT, user_id TEXT, nick_name TEXT
                 );
                 INSERT INTO conversation_user_table VALUES ('R:1', 'group-user', '群成员昵称');
                 INSERT INTO conversation_user_table VALUES ('R:1', '0000000000000000', '群内别名');",
            )
            .unwrap();

    let mut names = DisplayNameIndex::default();
    collect_display_names(&connection, &mut names);

    assert_eq!(
        names
            .participants
            .get("0000000000000000")
            .map(String::as_str),
        Some("联系人甲")
    );
    assert_eq!(
        names
            .conversation_members
            .get("R:1")
            .and_then(|members| members.get("0000000000000000"))
            .map(String::as_str),
        Some("群内别名")
    );
    assert_eq!(
        names.participants.get("wxid_1").map(String::as_str),
        Some("微信联系人")
    );
    assert!(!names.participants.contains_key("group-user"));
    assert_eq!(
        names
            .conversation_members
            .get("R:1")
            .and_then(|members| members.get("group-user"))
            .map(String::as_str),
        Some("群成员昵称")
    );
    assert_eq!(
        names.conversations.get("S:1_2").map(String::as_str),
        Some("项目会话")
    );
    assert!(!names.conversations.contains_key("R:1"));
    names.set_participant("self", "当前员工".into(), 100);
    assert_eq!(
        direct_conversation_name(
            "S:self_0000000000000000",
            &names.participants,
            &names.conversation_members,
        )
        .as_deref(),
        Some("当前员工、联系人甲")
    );
    assert_eq!(conversation_type("S:self_0000000000000000", 4), "direct");
    assert_eq!(conversation_type("R:group", 2), "group");

    let session = Connection::open_in_memory().unwrap();
    session
        .execute_batch(
            "CREATE TABLE \"USER\" (RID TEXT, name TEXT);
                 INSERT INTO \"USER\" VALUES ('0000000000000000', '会话别名');",
        )
        .unwrap();
    collect_display_names(&session, &mut names);
    assert_eq!(
        names
            .participants
            .get("0000000000000000")
            .map(String::as_str),
        Some("联系人甲")
    );
}

#[test]
fn extracts_text_from_protobuf_and_rejects_binary_noise() {
    let text = "你好，世界";
    let mut encoded = vec![0x0a, text.len() as u8];
    encoded.extend_from_slice(text.as_bytes());
    assert_eq!(decode_wecom_body(&encoded).as_deref(), Some(text));
    assert_eq!(
        decode_wecom_body(&[0x1e, 0x08, 0x00, 0x12, 0x1a, 0x0a, 0x18]),
        None
    );

    let announcement = "各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称";
    let mut encoded = vec![0x0a, 16];
    encoded.extend_from_slice(b"1688856080881486");
    encoded.push(0x12);
    write_test_varint(announcement.len() as u64, &mut encoded);
    encoded.extend_from_slice(announcement.as_bytes());
    assert_eq!(
        decode_wecom_metadata_body(&encoded).as_deref(),
        Some(announcement)
    );

    let locator = "2026/09/photo-0123456789abcdef.png";
    let mut image = vec![0x0a];
    write_test_varint(locator.len() as u64, &mut image);
    image.extend_from_slice(locator.as_bytes());
    let payload = readable_message_payload("3", &image);
    assert_eq!(payload["media_path"], locator);
    assert_eq!(payload["name"], "photo-0123456789abcdef.png");

    let file_payload = readable_message_payload("49", br#"{"name":"report.pdf"}"#);
    assert_eq!(file_payload["media_path"], "report.pdf");
    assert_eq!(file_payload["kind"], "file");
    let installer = "Clash.Verge_2.5.8_x64-setup.exe";
    let file_payload = readable_message_payload(
        "49",
        format!("{{\"name\":\"{installer}\",\"id\":\"1234567890\"}}").as_bytes(),
    );
    assert_eq!(file_payload["media_path"], installer);
    assert_eq!(file_payload["name"], installer);
}

#[test]
fn preserves_rich_text_emoji_numbers_spaces_and_repeated_fragments() {
    // Protobuf writers may omit a zero-valued fragment kind.
    let content = test_protobuf_bytes(&[(1, b"99345")]);
    let fragment = test_protobuf_bytes(&[(2, &content)]);
    let raw = test_protobuf_bytes(&[(1, &fragment)]);
    assert_eq!(decode_wecom_body(&raw).as_deref(), Some("99345"));
    assert_eq!(
        readable_message_payload("2", &raw)["rich_segments"][0]["kind"],
        "text"
    );
    fn rich(parts: &[(u8, &str)]) -> Vec<u8> {
        let mut result = Vec::new();
        for (kind, text) in parts {
            let mut content = vec![0x0a];
            write_test_varint(text.len() as u64, &mut content);
            content.extend_from_slice(text.as_bytes());
            let mut fragment = vec![0x08, *kind, 0x12];
            write_test_varint(content.len() as u64, &mut fragment);
            fragment.extend(content);
            result.push(0x0a);
            write_test_varint(fragment.len() as u64, &mut result);
            result.extend(fragment);
        }
        result
    }
    let bytes = rich(&[(3, "[便便]"), (0, "99345")]);
    let payload = readable_message_payload("2", &bytes);
    assert_eq!(payload["text"], "[便便]99345");
    assert_eq!(payload["rich_segments"][0]["kind"], "emoji");
    assert_eq!(payload["rich_segments"][1]["kind"], "text");
    assert_eq!(payload["rich_segments"][1]["text"], "99345");
    let bytes = rich(&[
        (0, "99345"),
        (0, " "),
        (3, "[便便]"),
        (3, "[便便]"),
        (0, "99345"),
    ]);
    assert_eq!(
        decode_wecom_body(&bytes).as_deref(),
        Some("99345 [便便][便便]99345")
    );
    assert_eq!(decode_wecom_body(b"99345").as_deref(), Some("99345"));
    assert_eq!(
        decode_wecom_body(&rich(&[(0, "1234567890123456")])).as_deref(),
        Some("1234567890123456")
    );
    assert_eq!(
        decode_wecom_body(&rich(&[(0, "a"); 20])).as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaa")
    );
}

#[test]
fn extracts_type_20_file_metadata_without_storing_remote_access_material() {
    let name = "synthetic-setup.exe";
    let md5 = "0123456789abcdef0123456789abcdef";
    let mut raw = Vec::new();
    for (field, value) in [
        (1, "remote-access-material-not-for-archive"),
        (2, name),
        (10, md5),
    ] {
        write_test_varint((field << 3) | 2, &mut raw);
        write_test_varint(value.len() as u64, &mut raw);
        raw.extend_from_slice(value.as_bytes());
    }
    raw.push(0x20);
    write_test_varint(37, &mut raw);
    let payload = readable_message_payload("20", &raw);
    assert_eq!(payload["name"], name);
    assert_eq!(payload["media_path"], name);
    assert_eq!(payload["media_md5"], md5);
    assert_eq!(payload["size"], 37);
    assert!(!payload.to_string().contains("remote-access-material"));
    let mut message = normalize(
        RawMessageRow {
            source_instance_id: "fixture".into(),
            source_message_id: "1".into(),
            conversation_id: "conversation".into(),
            sender_id: None,
            sent_at_unix_ms: 1700000000000,
            outgoing: None,
            raw_type: "20".into(),
            payload,
            recalled: false,
        },
        uuid::Uuid::new_v4(),
    )
    .unwrap();
    assert_eq!(message.message_type, MessageType::File);
    assert_eq!(message.media[0].original_name.as_deref(), Some(name));
    let directory = TestDirectory::new();
    fs::write(directory.0.join(md5), [1_u8; 37]).unwrap();
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: directory.0.clone(),
        databases: Vec::new(),
        media_roots: vec![directory.0.clone()],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: true,
    };
    let resolver = MediaResolver::new(&candidate, options);
    let blobs = resolve_media_metadata_with_local_paths(
        &candidate,
        &resolver,
        &mut message,
        options,
        None,
        &[],
    )
    .unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(message.media[0].integrity, MediaIntegrity::Verified);
    assert_eq!(message.media[0].size_bytes, Some(37));
    assert_eq!(blobs[0].original_name.as_deref(), Some(name));
}

fn test_protobuf_bytes(fields: &[(u64, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (field, bytes) in fields {
        write_test_varint((field << 3) | 2, &mut output);
        write_test_varint(bytes.len() as u64, &mut output);
        output.extend_from_slice(bytes);
    }
    output
}

#[test]
fn native_images_use_message_cache_paths_without_archiving_private_metadata() {
    let directory = TestDirectory::new();
    let account = directory.0.join("private-account");
    let source_root = account.join("Data");
    let cache = account.join("Cache").join("Image");
    let storage = account.join("FileStorage");
    let emotions = account.join("Emotion");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&cache).unwrap();
    fs::create_dir_all(&storage).unwrap();
    fs::create_dir_all(&emotions).unwrap();
    let sticker = emotions.join("downloaded-sticker");
    fs::write(&sticker, b"GIF89a synthetic sticker").unwrap();
    let original = cache.join("downloaded-cache-object");
    let preview = cache.join("cached-preview");
    fs::write(&original, b"synthetic original").unwrap();
    fs::write(&preview, b"synthetic preview").unwrap();
    let remote = b"remote-access-material-not-for-archive";
    let content = test_protobuf_bytes(&[
        (1, remote),
        (2, b"photo.png"),
        (10, b"0123456789abcdef0123456789abcdef"),
    ]);
    // Previews can precede the primary resource in the local envelope.
    let paths = test_protobuf_bytes(&[
        (1, b"preview-remote-material"),
        (1, preview.to_str().unwrap().as_bytes()),
        (1, remote),
        (1, original.to_str().unwrap().as_bytes()),
    ]);
    let extra = test_protobuf_bytes(&[(8, &paths), (22, b"private OCR must not be archived")]);
    assert_eq!(
        local_media_paths("14", &content, &extra),
        vec![original.clone(), preview.clone()]
    );
    let database = source_root.join("message.db");
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch("CREATE TABLE message_table (send_time INTEGER, conversation_id TEXT, sender_id TEXT, content_type INTEGER, content BLOB, local_extra_content BLOB);").unwrap();
    for raw_type in [14, 29] {
        let content = if raw_type == 14 {
            content.clone()
        } else {
            test_protobuf_bytes(&[(1, remote), (6, b"0123456789abcdef0123456789abcdef")])
        };
        let extra = if raw_type == 14 {
            extra.clone()
        } else {
            test_protobuf_bytes(&[(
                8,
                &test_protobuf_bytes(&[(1, remote), (1, sticker.to_str().unwrap().as_bytes())]),
            )])
        };
        connection.execute("INSERT INTO message_table VALUES (1700000000, 'conversation', 'sender', ?1, ?2, ?3)", rusqlite::params![raw_type, content, extra]).unwrap();
    }
    drop(connection);
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source_root,
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: database,
            wal_path: None,
            shm_path: None,
            encrypted: false,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![storage, cache, emotions],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: true,
    };
    let export = collect_candidate_with_options(
        candidate.clone(),
        directory.0.join("work"),
        None,
        authorization(),
        options,
        None,
        None,
    )
    .unwrap();
    assert_eq!(export.message_count, 2);
    assert_eq!(export.missing_media_count, 0);
    assert_eq!(export.media_blobs.len(), 2);
    for expected in [
        b"synthetic original".as_slice(),
        b"GIF89a synthetic sticker".as_slice(),
    ] {
        assert!(
            export
                .media_blobs
                .iter()
                .any(|blob| hex::decode(&blob.content_hex).unwrap() == expected)
        );
    }
    let serialized = serde_json::to_string(&export).unwrap();
    let image = export.batches[0]
        .messages
        .iter()
        .find(|message| message.raw_type == "14")
        .unwrap();
    assert!(!serialized.contains("remote-access-material"));
    assert!(!serialized.contains("private-account"));
    assert!(!serialized.contains("private OCR"));
    // A deleted original still leaves an authorized preview for this message.
    let missing = original.with_file_name("missing-original");
    let resolver = MediaResolver::new(&candidate, options);
    let mut message = image.clone();
    let blobs = resolve_media_metadata_with_local_paths(
        &candidate,
        &resolver,
        &mut message,
        options,
        None,
        &[missing, preview],
    )
    .unwrap();
    assert_eq!(
        hex::decode(&blobs[0].content_hex).unwrap(),
        b"synthetic preview"
    );
    // Existing but unauthorized files must not be accepted as local hints.
    let outside = directory.0.join("outside.png");
    fs::write(&outside, b"outside").unwrap();
    let mut message = image.clone();
    let blobs = resolve_media_metadata_with_local_paths(
        &candidate,
        &resolver,
        &mut message,
        options,
        None,
        &[outside],
    )
    .unwrap();
    assert!(blobs.is_empty());
    assert_eq!(message.media[0].integrity, MediaIntegrity::Missing);
}

#[test]
fn native_voice_messages_collect_timestamped_silk_by_content_digest() {
    let directory = TestDirectory::new();
    let account = directory.0.join("account");
    let source_root = account.join("Data");
    let voice_root = account.join("Cache/Voice");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&voice_root).unwrap();
    let voice_file = voice_root.join("2026_11_04_12_00_00_001.silk");
    let bytes = b"\x02#!SILK_V3 synthetic voice bytes";
    let digest = "D47D1E2BA230C47EAC9B5AE2B72A4ACB";
    fs::write(&voice_file, bytes).unwrap();
    let mut voice_content = test_protobuf_bytes(&[
        (1, b"remote-access-material-not-for-archive"),
        (8, b"00000000000000000000000000000000"),
        (10, digest.as_bytes()),
    ]);
    write_test_varint(4 << 3, &mut voice_content);
    write_test_varint(bytes.len() as u64, &mut voice_content);
    // Observed voice envelopes contain status integers, not a cache path.
    let voice_extra = vec![8_u8, 1, 112, 1];
    let database = source_root.join("message.db");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE message_table (
                    send_time INTEGER, server_id TEXT, conversation_id TEXT,
                    sender_id TEXT, content_type INTEGER, content BLOB,
                    local_extra_content BLOB
                );
                CREATE TABLE msg_voice2text (message_id TEXT, voice_id TEXT, text TEXT,
                    collapse INTEGER, voice_silent_transfer INTEGER);
                INSERT INTO msg_voice2text VALUES
                    ('server-voice-1', 'voice-1', '来自独立转写表', 0, 0);
                INSERT INTO msg_voice2text VALUES
                    ('orphan-server-id', 'voice-orphan', '孤儿转写不应导入', 0, 0);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message_table VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                1700000000_i64,
                "server-voice-1",
                "conversation",
                "sender",
                16_i64,
                voice_content,
                voice_extra,
            ],
        )
        .unwrap();
    drop(connection);

    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source_root.clone(),
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: database,
            wal_path: None,
            shm_path: None,
            encrypted: false,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![voice_root],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let export = collect_candidate_with_options(
        candidate.clone(),
        directory.0.join("work"),
        None,
        authorization(),
        MediaCollectionOptions {
            include_files: true,
            include_images: false,
        },
        None,
        None,
    )
    .unwrap();
    assert_eq!(export.message_count, 1);
    let message = &export.batches[0].messages[0];
    assert_eq!(message.message_type, MessageType::Audio);
    assert_eq!(message.body_text.as_deref(), Some("来自独立转写表"));
    assert_eq!(message.media.len(), 1);
    assert_eq!(message.media[0].integrity, MediaIntegrity::Verified);
    assert!(message.media[0].content_hash.is_some());
    assert_eq!(message.media[0].mime_type.as_deref(), Some("audio/silk"));
    assert_eq!(message.media[0].size_bytes, Some(bytes.len() as u64));
    assert!(
        message.media[0]
            .original_name
            .as_deref()
            .unwrap()
            .ends_with(".silk")
    );
    assert_eq!(export.media_blobs.len(), 1);
    assert_eq!(
        hex::decode(&export.media_blobs[0].content_hex).unwrap(),
        bytes
    );
    let serialized = serde_json::to_string(&export).unwrap();
    assert!(!serialized.contains("remote-access-material"));
    assert!(!serialized.contains("00000000000000000000000000000000"));
    assert!(!serialized.contains("2026_11_04_12_00_00_001"));
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink_received = Arc::clone(&received);
    let sink: wecom_archive_server::LocalCollectionMediaSink = Arc::new(move |source, _, _| {
        *sink_received.lock().unwrap() =
            fs::read(source).map_err(|_| "fixture read failed".to_owned())?;
        Ok(())
    });
    let streamed = collect_candidate_with_options(
        candidate.clone(),
        directory.0.join("streamed-work"),
        None,
        authorization(),
        MediaCollectionOptions {
            include_files: true,
            include_images: false,
        },
        Some(&sink),
        None,
    )
    .unwrap();
    assert!(streamed.media_blobs.is_empty());
    assert_eq!(streamed.missing_media_count, 0);
    assert_eq!(*received.lock().unwrap(), bytes);
    let text_only = collect_candidate_with_options(
        candidate,
        directory.0.join("text-work"),
        None,
        authorization(),
        MediaCollectionOptions::default(),
        None,
        None,
    )
    .unwrap();
    assert!(text_only.media_blobs.is_empty());
    assert!(
        text_only.batches[0].messages[0].media[0]
            .content_hash
            .is_none()
    );
    assert!(
        !serde_json::to_string(&export)
            .unwrap()
            .contains("孤儿转写不应导入")
    );
}

#[test]
fn voice_digest_lookup_rejects_wrong_content_size_ambiguity_and_changed_files() {
    let directory = TestDirectory::new();
    let source = directory.0.join("Data");
    let voice = directory.0.join("Cache/Voice");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&voice).unwrap();
    let bytes = b"\x02#!SILK_V3 synthetic voice bytes";
    let digest = "d47d1e2ba230c47eac9b5ae2b72a4acb";
    let timestamped = voice.join("2026_11_04_12_00_00_001.silk");
    // Even a convincing filename is insufficient if its content differs.
    fs::write(
        voice.join(format!("{digest}.silk")),
        b"\x02#!SILK_V3 wrong content",
    )
    .unwrap();
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source,
        databases: vec![],
        media_roots: vec![voice.clone(), directory.0.join("Cache")],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: false,
    };
    let resolver = MediaResolver::new(&candidate, options);
    assert!(resolver.resolve_voice_by_md5(digest, None).is_none());
    assert!(resolver.files_by_key.get().is_none());
    assert!(
        resolver
            .resolve_voice_by_md5("not a digest", None)
            .is_none()
    );
    fs::write(&timestamped, bytes).unwrap();
    let resolver = MediaResolver::new(&candidate, options);
    assert_eq!(
        resolver.resolve_voice_by_md5(digest, Some(bytes.len() as u64)),
        Some(timestamped.canonicalize().unwrap())
    );
    assert!(resolver.files_by_key.get().is_none());
    assert!(
        resolver
            .resolve_voice_by_md5(digest, Some(bytes.len() as u64 + 1))
            .is_none()
    );
    fs::write(&timestamped, b"\x02#!SILK_V3 replaced cache").unwrap();
    assert!(resolver.resolve_voice_by_md5(digest, None).is_none());
    fs::write(&timestamped, bytes).unwrap();
    fs::write(voice.join("second.silk"), bytes).unwrap();
    let resolver = MediaResolver::new(&candidate, options);
    assert!(resolver.resolve_voice_by_md5(digest, None).is_none());
    let resolver = MediaResolver::new(&candidate, MediaCollectionOptions::default());
    assert!(resolver.resolve_voice_by_md5(digest, None).is_none());
}

#[test]
fn native_voice_projection_omits_access_metadata_and_requires_the_verified_digest() {
    let mut raw = test_protobuf_bytes(&[
        (1, b"remote-access-material"),
        (8, b"00000000000000000000000000000000"),
        (10, b"D47D1E2BA230C47EAC9B5AE2B72A4ACB"),
    ]);
    write_test_varint(4 << 3, &mut raw);
    write_test_varint(29, &mut raw);
    let payload = readable_message_payload("16", &raw);
    assert_eq!(payload["media_md5"], "d47d1e2ba230c47eac9b5ae2b72a4acb");
    assert_eq!(payload["size"], 29);
    assert!(!payload.to_string().contains("remote-access-material"));
    assert!(
        !payload
            .to_string()
            .contains("00000000000000000000000000000000")
    );
    let no_digest = test_protobuf_bytes(&[(8, b"00000000000000000000000000000000")]);
    assert!(
        readable_message_payload("16", &no_digest)
            .get("media_md5")
            .is_none()
    );
    assert!(
        readable_message_payload("16", &[0x0a, 0xff])
            .get("media_md5")
            .is_none()
    );
}

#[test]
fn voice_digest_lookup_stays_within_the_authorized_account() {
    let directory = TestDirectory::new();
    let source = directory.0.join("first/Data");
    let voice = directory.0.join("first/Cache/Voice");
    let other = directory.0.join("second/Cache/Voice");
    for path in [&source, &voice, &other] {
        fs::create_dir_all(path).unwrap();
    }
    let path = other.join("2026_11_04_12_00_00_001.silk");
    fs::write(&path, b"\x02#!SILK_V3 synthetic voice bytes").unwrap();
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source,
        databases: vec![],
        media_roots: vec![voice],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: false,
    };
    let resolver = MediaResolver::new(&candidate, options);
    assert!(resolver.authorized_file(path).is_none());
    assert!(
        resolver
            .resolve_voice_by_md5("d47d1e2ba230c47eac9b5ae2b72a4acb", None)
            .is_none()
    );
}

#[test]
fn native_recall_uses_explicit_flags_without_matching_arbitrary_event_values() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("WXWork");
    fs::create_dir_all(&source_root).unwrap();
    let database = source_root.join("message.db");
    let connection = Connection::open(&database).unwrap();
    let recall_content = {
        let mut bytes = Vec::new();
        write_test_varint(8, &mut bytes);
        write_test_varint(12345, &mut bytes);
        bytes
    };
    connection
        .execute_batch(
            "CREATE TABLE message_table (
                    send_time INTEGER, server_id TEXT, conversation_id TEXT,
                    sender_id TEXT, content_type INTEGER, content BLOB,
                    local_extra_content BLOB, is_revoke INTEGER, is_deleted INTEGER
                );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message_table VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1)",
            rusqlite::params![
                1700000000_i64,
                "12345",
                "conversation",
                "sender",
                20_i64,
                br#"{"name":"withdrawn.bin"}"#.as_slice(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message_table VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 0)",
            rusqlite::params![
                1700000010_i64,
                "recall-event",
                "conversation",
                "sender",
                2063_i64,
                recall_content,
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO message_table VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 1, 0)",
            rusqlite::params![
                1700000020_i64,
                "explicitly-recalled",
                "conversation",
                "sender",
                20_i64,
                br#"{"name":"recalled.bin"}"#.as_slice(),
            ],
        )
        .unwrap();
    drop(connection);

    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source_root.clone(),
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: database,
            wal_path: None,
            shm_path: None,
            encrypted: false,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![source_root],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let export = collect_candidate_with_options(
        candidate,
        directory.0.join("work"),
        None,
        authorization(),
        MediaCollectionOptions::default(),
        None,
        None,
    )
    .unwrap();
    let original = export.batches[0]
        .messages
        .iter()
        .find(|message| message.source_message_id == "1")
        .unwrap();
    assert_eq!(original.lifecycle, archive_domain::LifecycleState::Active);
    let recalled = &export.batches[0].messages[2];
    assert_eq!(recalled.lifecycle, archive_domain::LifecycleState::Recalled);
    let notice = &export.batches[0].messages[1];
    assert_eq!(notice.message_type, MessageType::System);
    assert_eq!(notice.body_text.as_deref(), Some("消息已撤回"));
}

#[test]
fn local_media_hints_ignore_urls_network_paths_parent_traversal_and_broken_envelopes() {
    let paths = test_protobuf_bytes(&[
        (1, b"https://invalid.example/photo.png"),
        (1, br"\\server\share\image.png"),
        (1, br"C:\cache\..\private.png"),
        (1, b"relative.png"),
    ]);
    let extra = test_protobuf_bytes(&[(8, &paths)]);
    assert!(local_media_paths("14", &[], &extra).is_empty());
    assert!(local_media_paths("14", &[], &[0x42, 0xff]).is_empty());
    assert!(local_media_paths("text", &[], &extra).is_empty());
}

#[test]
fn media_lookup_defers_directory_index_until_fallback_and_rebuilds_next_collection() {
    let directory = TestDirectory::new();
    let source = directory.0.join("Data");
    let media = directory.0.join("Media");
    let nested = media.join("nested");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&nested).unwrap();
    let direct = media.join("direct.png");
    let fallback = nested.join("fallback.dat");
    fs::write(&direct, b"synthetic direct image").unwrap();
    fs::write(&fallback, b"synthetic fallback image").unwrap();
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: source.clone(),
        databases: vec![],
        media_roots: vec![media],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: true,
    };
    let resolver = MediaResolver::new(&candidate, options);
    // Empty/text-only incremental results never pay for recursive traversal.
    assert!(resolver.files_by_key.get().is_none());
    assert_eq!(
        resolver.resolve(&source, "direct.png"),
        Some(direct.canonicalize().unwrap())
    );
    assert!(resolver.files_by_key.get().is_none());
    assert_eq!(
        resolver.resolve(&source, "fallback.dat"),
        Some(fallback.canonicalize().unwrap())
    );
    let index = resolver.files_by_key.get().unwrap();
    assert_eq!(
        resolver.resolve(&source, "fallback"),
        Some(fallback.canonicalize().unwrap())
    );
    assert!(std::ptr::eq(index, resolver.files_by_key.get().unwrap()));
    assert!(resolver.resolve(&source, "downloaded-later.dat").is_none());
    let downloaded = nested.join("downloaded-later.dat");
    fs::write(&downloaded, b"synthetic later download").unwrap();
    let next = MediaResolver::new(&candidate, options);
    assert!(next.files_by_key.get().is_none());
    assert_eq!(
        next.resolve(&source, "downloaded-later.dat"),
        Some(downloaded.canonicalize().unwrap())
    );
    fs::remove_file(&fallback).unwrap();
    assert!(resolver.resolve(&source, "fallback.dat").is_none());
    let disabled = MediaResolver::new(&candidate, MediaCollectionOptions::default());
    assert!(disabled.resolve(&source, "downloaded-later.dat").is_none());
}

#[test]
fn media_lookup_prefers_exact_names_rejects_ambiguity_and_deduplicates_overlapping_roots() {
    let directory = TestDirectory::new();
    let first = directory.0.join("first");
    let second = directory.0.join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join("photo.png"), b"first").unwrap();
    fs::write(first.join("photo.jpg"), b"second").unwrap();
    let mut candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: directory.0.clone(),
        databases: vec![],
        media_roots: vec![directory.0.clone(), first.clone()],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: true,
        include_images: true,
    };
    let resolver = MediaResolver::new(&candidate, options);
    assert_eq!(
        resolver.resolve(&directory.0, "photo.png"),
        Some(first.join("photo.png").canonicalize().unwrap())
    );
    assert!(resolver.resolve(&directory.0, "photo").is_none());
    fs::write(second.join("photo.png"), b"duplicate").unwrap();
    candidate.media_roots = vec![directory.0.clone()];
    let resolver = MediaResolver::new(&candidate, options);
    assert!(resolver.resolve(&directory.0, "photo.png").is_none());
    candidate.media_roots = vec![first.clone(), second.clone()];
    let resolver = MediaResolver::new(&candidate, options);
    assert!(resolver.resolve(&directory.0, "photo.png").is_none());
    assert_eq!(
        resolver.resolve(&directory.0, first.join("photo.png").to_str().unwrap()),
        Some(first.join("photo.png").canonicalize().unwrap())
    );
}

#[test]
fn native_image_hash_hints_resolve_cache_without_remote_material() {
    let directory = TestDirectory::new();
    let hint = "0123456789abcdef0123456789abcdef";
    fs::write(directory.0.join(format!("{hint}.dat")), b"synthetic image").unwrap();
    let raw = test_protobuf_bytes(&[
        (1, b"remote-secret"),
        (2, b"original.png"),
        (8, b"invalid hint"),
        (30, hint.as_bytes()),
    ]);
    let payload = readable_message_payload("14", &raw);
    assert_eq!(payload["media_lookup_hints"], serde_json::json!([hint]));
    assert!(!payload.to_string().contains("remote-secret"));
    let mut message = normalize(
        RawMessageRow {
            source_instance_id: "fixture".into(),
            source_message_id: "1".into(),
            conversation_id: "conversation".into(),
            sender_id: None,
            sent_at_unix_ms: 1700000000000,
            outgoing: None,
            raw_type: "14".into(),
            payload,
            recalled: false,
        },
        uuid::Uuid::new_v4(),
    )
    .unwrap();
    let candidate = SourceCandidate {
        source_id: "fixture".into(),
        display_path: "fixture".into(),
        root_path: directory.0.clone(),
        databases: vec![],
        media_roots: vec![directory.0.clone()],
        client_version: None,
        capability: SourceCapability::Supported,
    };
    let options = MediaCollectionOptions {
        include_files: false,
        include_images: true,
    };
    let resolver = MediaResolver::new(&candidate, options);
    let blobs = resolve_media_metadata_with_local_paths(
        &candidate,
        &resolver,
        &mut message,
        options,
        None,
        &[],
    )
    .unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(message.media[0].integrity, MediaIntegrity::Verified);
}

fn write_test_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        output.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

#[test]
fn reads_document_identity_without_treating_notification_entity_as_a_person() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE conversation_table (id TEXT, name TEXT, type_content BLOB);")
        .unwrap();
    let mut content = vec![0x08];
    write_test_varint(12345, &mut content);
    for (tag, text) in [
        (
            0x22,
            "https://doc.weixin.qq.com/journal/create?docid=synthetic",
        ),
        (0x2a, "测试工作周报"),
    ] {
        content.push(tag);
        write_test_varint(text.len() as u64, &mut content);
        content.extend_from_slice(text.as_bytes());
    }
    connection
        .execute(
            "INSERT INTO conversation_table VALUES (?1,'',?2)",
            rusqlite::params!["M:12345", content],
        )
        .unwrap();
    let mut names = DisplayNameIndex::default();
    collect_display_names(&connection, &mut names);
    assert_eq!(
        names.conversations.get("M:12345").map(String::as_str),
        Some("测试工作周报")
    );
    assert_eq!(
        names.conversation_types.get("M:12345").map(String::as_str),
        Some("document")
    );
    assert_eq!(
        names.participants.get("12345").map(String::as_str),
        Some("汇报提醒")
    );
    assert_eq!(
        names.participant_kinds.get("12345").map(String::as_str),
        Some("bot")
    );
    assert_eq!(conversation_type("M:unclassified", 8), "application");
    assert!(document_conversation_metadata("M:999", &content).is_none());
    assert!(document_conversation_metadata("S:12345", &content).is_none());
    assert!(document_conversation_metadata("M:12345", &content[..content.len() - 1]).is_none());
}

#[test]
fn reads_group_announcement_from_session_metadata() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE conversation_table (
                    id TEXT,
                    notice_content BLOB,
                    notice_time INTEGER,
                    modify_time INTEGER
                );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conversation_table VALUES (?1, ?2, ?3, 0)",
            rusqlite::params![
                "R:group",
                "各位同事请及时查看坐车注意事项",
                1_789_430_400_i64
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conversation_table VALUES (?1, ?2, ?3, 0)",
            rusqlite::params!["R:id-only", "1688856080881486", 1_789_430_400_i64],
        )
        .unwrap();

    let mut announcements = Vec::new();
    collect_group_announcements(&connection, &mut announcements);

    assert_eq!(announcements.len(), 1);
    assert_eq!(announcements[0].0, "R:group");
    assert!(announcements[0].1.contains("坐车注意事项"));
}

#[test]
fn redacts_sensitive_message_text_before_enterprise_upload() {
    let redacted = redact_sensitive_text("账号 admin，密码 secret-123");
    assert_eq!(redacted, "账号 admin，密码 [敏感数据已脱敏]");
    assert!(redacted.contains("admin") && !redacted.contains("secret-123"));

    let mut payload = serde_json::json!({
        "password": "secret-456",
        "profile": { "email": "person@example.com" },
        "participant_profiles": {"synthetic-member": {"mobile": "13800000000", "office_phone": "010-00000000", "email": "member@example.invalid", "department": "研发部", "employee_no": "TEST-001", "custom:办公地点": "测试楼A栋"}},
        "message": "普通内容"
    });
    redact_sensitive_json(&mut payload);
    let serialized = serde_json::to_string(&payload).unwrap();
    assert!(!serialized.contains("secret-456"));
    assert!(!serialized.contains("person@example.com"));
    assert!(!serialized.contains("13800000000"));
    assert!(!serialized.contains("010-00000000"));
    assert!(!serialized.contains("member@example.invalid"));
    assert!(!serialized.contains("TEST-001"));
    assert!(!serialized.contains("测试楼A栋"));
    assert!(serialized.contains("研发部"));
    assert!(serialized.contains("普通内容"));
}

#[test]
fn converts_incremental_cutoffs_to_source_timestamp_units() {
    let cutoff = 1_700_000_000_500_i64;
    assert_eq!(
        source_timestamp_cutoff(cutoff, 1_700_000_000),
        1_700_000_000
    );
    assert_eq!(source_timestamp_cutoff(cutoff, cutoff), cutoff);
    assert_eq!(
        source_timestamp_cutoff(cutoff, 1_700_000_000_000_000),
        cutoff.saturating_mul(1_000)
    );
}

#[test]
fn synthetic_plain_source_runs_through_snapshot_and_export_contract() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("source");
    fs::create_dir_all(&source_root).unwrap();
    let message_path = source_root.join("message.db");
    let connection = Connection::open(&message_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "message-1",
                "conversation-1",
                "participant-1",
                1_700_000_000_000_i64,
                0_i64,
                "text",
                r#"{"text":"你好","local_path":"C:\\private\\account"}"#,
                0_i64,
            ],
        )
        .unwrap();
    drop(connection);
    let user = Connection::open(source_root.join("user.db")).unwrap();
    user.execute_batch(
        "CREATE TABLE contacts (user_id TEXT PRIMARY KEY, display_name TEXT NOT NULL);
             INSERT INTO contacts VALUES ('participant-1', '测试成员');",
    )
    .unwrap();
    drop(user);
    let session = Connection::open(source_root.join("session.db")).unwrap();
    session
        .execute_batch(
            "CREATE TABLE sessions (
                    conversation_id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL
                 );
                 INSERT INTO sessions VALUES ('conversation-1', '与测试成员的会话');",
        )
        .unwrap();
    drop(session);

    let candidate = WindowsSourceAdapter
        .discover(Some(source_root))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let work_root = directory.0.join("work");
    let export =
        collect_candidate(candidate.clone(), work_root.clone(), None, authorization()).unwrap();

    assert_eq!(export.schema_version, "client-export.v1");
    assert_eq!(export.message_count, 1);
    assert_eq!(export.batches[0].employee_notice.acknowledged_at, None);
    assert_eq!(
        export.batches[0].employee_notice.evidence_hash,
        notice_evidence_hash("test-notice.v1")
    );
    assert_eq!(
        export.batches[0].messages[0].body_text.as_deref(),
        Some("你好")
    );
    assert_eq!(
        export.participants[0].display_name.as_deref(),
        Some("测试成员")
    );
    assert_eq!(
        export.conversations[0].display_name.as_deref(),
        Some("与测试成员的会话")
    );
    let connection = Connection::open(&message_path).unwrap();
    connection
        .execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "message-2",
                "conversation-1",
                "participant-1",
                1_700_000_001_000_i64,
                0_i64,
                "text",
                r#"{"text":"增量消息"}"#,
                0_i64,
            ],
        )
        .unwrap();
    drop(connection);
    let snapshot_cache_root = directory.0.join("snapshot-cache");
    let incremental_work = directory.0.join("work-incremental");
    let incremental = collect_candidate_with_options_since(
        candidate.clone(),
        incremental_work.clone(),
        None,
        authorization(),
        CollectionReadOptions {
            since_unix_ms: Some(1_700_000_000_500),
            snapshot_cache_root: Some(&snapshot_cache_root),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(incremental.message_count, 1);
    assert_eq!(
        incremental.batches[0].messages[0].body_text.as_deref(),
        Some("增量消息")
    );
    for damaged in [false, true, false] {
        let original = fs::read(&message_path).unwrap();
        if damaged {
            fs::write(&message_path, b"synthetic invalid database").unwrap();
        }
        let repeated = collect_candidate_with_options_since(
            candidate.clone(),
            incremental_work.clone(),
            None,
            authorization(),
            CollectionReadOptions {
                since_unix_ms: Some(1_700_000_000_500),
                snapshot_cache_root: Some(&snapshot_cache_root),
                ..Default::default()
            },
        );
        if damaged {
            assert!(repeated.is_err());
            fs::write(&message_path, original).unwrap();
        } else {
            let repeated = repeated.unwrap();
            assert_eq!(repeated.message_count, incremental.message_count);
            assert_eq!(
                repeated.batches[0].messages[0].stable_message_id,
                incremental.batches[0].messages[0].stable_message_id
            );
        }
        assert_eq!(fs::read_dir(&incremental_work).unwrap().count(), 0);
    }
    let serialized = serde_json::to_string(&export).unwrap();
    assert!(!serialized.contains("private"));
    assert_eq!(fs::read_dir(work_root).unwrap().count(), 0);
}

#[test]
fn incremental_snapshot_keeps_employee_direction_from_historical_conversation_keys() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("source");
    fs::create_dir(&source_root).unwrap();
    let message_path = source_root.join("message.db");
    let connection = Connection::open(&message_path).unwrap();
    connection.execute_batch(
            "CREATE TABLE messages (source_message_id TEXT, conversation_id TEXT, sender_id TEXT,
             sent_at_unix_ms INTEGER, outgoing INTEGER, raw_type TEXT, payload_json TEXT, recalled INTEGER);
             INSERT INTO messages VALUES ('old-a', 'S:self_a', 'self', 1700000000000, NULL, 'text', '{\"text\":\"old a\"}', 0);
             INSERT INTO messages VALUES ('old-b', 'S:self_b', 'self', 1700000000000, NULL, 'text', '{\"text\":\"old b\"}', 0);
             INSERT INTO messages VALUES ('recent', 'S:self_a', 'a', 1700003600000, NULL, 'text', '{\"text\":\"recent\"}', 0);"
        ).unwrap();
    drop(connection);
    let candidate = WindowsSourceAdapter
        .discover(Some(source_root))
        .unwrap()
        .remove(0);
    let full = collect_candidate(
        candidate.clone(),
        directory.0.join("full"),
        None,
        authorization(),
    )
    .unwrap();
    let incremental = collect_candidate_with_options_since(
        candidate,
        directory.0.join("incremental"),
        None,
        authorization(),
        CollectionReadOptions {
            since_unix_ms: Some(1_700_003_599_000),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(incremental.message_count, 1);
    let message = &incremental.batches[0].messages[0];
    assert_eq!(message.source_message_id, "recent");
    assert_eq!(
        message.direction,
        archive_domain::MessageDirection::Incoming
    );
    assert_eq!(
        message.direction,
        full.batches[0]
            .messages
            .iter()
            .find(|row| row.source_message_id == "recent")
            .unwrap()
            .direction
    );
    assert_eq!(
        fs::read_dir(directory.0.join("incremental"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn media_mode_resolves_nested_files_and_text_mode_keeps_media_relations() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("source");
    let media_root = source_root.join("FileStorage");
    let nested_media = media_root.join("2026").join("09");
    fs::create_dir_all(&nested_media).unwrap();
    let media_bytes = b"synthetic image bytes";
    fs::write(nested_media.join("photo-fixture.png"), media_bytes).unwrap();
    let message_path = source_root.join("message.db");
    let connection = Connection::open(&message_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );
                INSERT INTO messages VALUES (
                    'text-1', 'conversation-1', 'participant-1', 1700000000000, 0,
                    'text', '{\"text\":\"保留的文本\"}', 0
                );
                INSERT INTO messages VALUES (
                    'image-1', 'conversation-1', 'participant-1', 1700000001000, 0,
                    'image', '{\"name\":\"photo-fixture.png\"}', 0
                );",
        )
        .unwrap();
    drop(connection);
    let candidate = SourceCandidate {
        source_id: "source".into(),
        display_path: "source".into(),
        root_path: source_root,
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: message_path,
            wal_path: None,
            shm_path: None,
            encrypted: false,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![media_root],
        client_version: None,
        capability: SourceCapability::ProbeRequired,
    };

    let with_media = collect_candidate_with_options(
        candidate.clone(),
        directory.0.join("work-media"),
        None,
        authorization(),
        MediaCollectionOptions {
            include_files: true,
            include_images: true,
        },
        None,
        None,
    )
    .unwrap();
    assert_eq!(with_media.message_count, 2);
    assert_eq!(with_media.media_count, 1);
    assert_eq!(with_media.media_blobs.len(), 1);
    assert_eq!(
        hex::decode(&with_media.media_blobs[0].content_hex).unwrap(),
        media_bytes
    );

    let streamed_files = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_files = std::sync::Arc::clone(&streamed_files);
    let media_sink: wecom_archive_server::LocalCollectionMediaSink =
        std::sync::Arc::new(move |path, hash, size| {
            captured_files.lock().unwrap().push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                hash.to_owned(),
                size,
            ));
            Ok(())
        });
    let streamed_media = collect_candidate_with_options(
        candidate.clone(),
        directory.0.join("work-streamed-media"),
        None,
        authorization(),
        MediaCollectionOptions {
            include_files: true,
            include_images: true,
        },
        Some(&media_sink),
        None,
    )
    .unwrap();
    assert_eq!(streamed_media.media_count, 1);
    assert!(streamed_media.media_blobs.is_empty());
    assert_eq!(streamed_media.media_transport, "metadata_only");
    assert_eq!(streamed_files.lock().unwrap().len(), 1);

    let text_only = collect_candidate_with_options(
        candidate,
        directory.0.join("work-text"),
        None,
        authorization(),
        MediaCollectionOptions::default(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(text_only.message_count, 2);
    assert_eq!(
        text_only.batches[0].messages[0].message_type,
        MessageType::Text
    );
    assert_eq!(
        text_only.batches[0].messages[1].message_type,
        MessageType::Image
    );
    assert_eq!(text_only.batches[0].messages[1].media.len(), 1);
    assert!(text_only.media_blobs.is_empty());
}

#[test]
fn collectable_source_skips_accounts_whose_key_cannot_open_them() {
    let directory = TestDirectory::new();
    let candidate = |name: &str| SourceCandidate {
        source_id: name.into(),
        display_path: format!("…\\{name}"),
        root_path: directory.0.join(name),
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: directory.0.join(name).join("message.db"),
            wal_path: None,
            shm_path: None,
            encrypted: true,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![],
        client_version: None,
        capability: SourceCapability::ProbeRequired,
    };

    let mut tried: Vec<String> = Vec::new();
    let (chosen, key) = choose_collectable_source(
        vec![candidate("stale-largest"), candidate("logged-in")],
        &mut |source| {
            tried.push(source.source_id.clone());
            if source.source_id == "logged-in" {
                Ok(Some(SourceKey::passphrase(b"verified-key")))
            } else {
                Err(CommandError {
                    code: "SOURCE_KEY_NOT_FOUND",
                    message: "自动解析未找到可验证的数据库密钥。".into(),
                    recoverable: true,
                })
            }
        },
    )
    .expect("the account whose key opens its database should be the one collected");

    assert_eq!(chosen.source_id, "logged-in");
    assert!(key.is_some());
    assert_eq!(
        tried,
        vec!["stale-largest".to_string(), "logged-in".to_string()]
    );
}

#[test]
fn encrypted_source_requires_a_key_before_snapshot() {
    let directory = TestDirectory::new();
    let candidate = SourceCandidate {
        source_id: "source".into(),
        display_path: "…\\source".into(),
        root_path: directory.0.clone(),
        databases: vec![archive_domain::SourceDatabase {
            kind: "message".into(),
            path: directory.0.join("message.db"),
            wal_path: None,
            shm_path: None,
            encrypted: true,
            page_size_hint: Some(4096),
        }],
        media_roots: vec![],
        client_version: None,
        capability: SourceCapability::ProbeRequired,
    };
    let error =
        collect_candidate(candidate, directory.0.join("work"), None, authorization()).unwrap_err();
    assert_eq!(error.code, "SOURCE_KEY_REQUIRED");
    assert!(!directory.0.join("work").exists());
}

#[test]
fn aes128cbc_fixture_opens_with_the_right_key_and_rejects_the_wrong_key() {
    let directory = TestDirectory::new();
    let source_root = directory.0.join("encrypted-source");
    fs::create_dir_all(&source_root).unwrap();
    let message_path = source_root.join("message.db");
    let correct_key = b"fixture passphrase";
    let connection = sqlite3mc::create_encrypted_fixture(&message_path, correct_key).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE messages (
                    source_message_id TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    sender_id TEXT,
                    sent_at_unix_ms INTEGER NOT NULL,
                    outgoing INTEGER,
                    raw_type TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    recalled INTEGER NOT NULL
                );
                INSERT INTO messages VALUES (
                    'encrypted-1', 'conversation-1', 'participant-1',
                    1700000000000, 0, 'text', '{\"text\":\"加密夹具\"}', 0
                );",
        )
        .unwrap();
    drop(connection);
    assert_ne!(
        &fs::read(&message_path).unwrap()[..16],
        b"SQLite format 3\0"
    );
    for name in ["session.db", "user.db"] {
        let auxiliary = Connection::open(source_root.join(name)).unwrap();
        auxiliary
            .execute_batch("CREATE TABLE fixture (id INTEGER PRIMARY KEY);")
            .unwrap();
    }
    let candidate = WindowsSourceAdapter
        .discover(Some(source_root))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    let wrong = collect_candidate(
        candidate.clone(),
        directory.0.join("wrong-work"),
        Some(&SourceKey::passphrase(b"wrong passphrase")),
        authorization(),
    )
    .unwrap_err();
    assert_eq!(wrong.code, "SOURCE_KEY_INVALID");
    assert_eq!(
        fs::read_dir(directory.0.join("wrong-work"))
            .unwrap()
            .count(),
        0
    );

    let export = collect_candidate(
        candidate,
        directory.0.join("correct-work"),
        Some(&SourceKey::passphrase(correct_key)),
        authorization(),
    )
    .unwrap();
    assert_eq!(export.message_count, 1);
    assert_eq!(
        export.batches[0].messages[0].body_text.as_deref(),
        Some("加密夹具")
    );
}

#[test]
fn raw_wxsqlite3_decryption_merges_committed_wal_pages() {
    let directory = TestDirectory::new();
    let source = directory.0.join("message.db");
    let snapshot = directory.0.join("snapshot");
    fs::create_dir_all(&snapshot).unwrap();
    let key = b"0123456789abcdef";
    let connection = sqlite3mc::create_encrypted_derived_fixture(&source, key).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
                 CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                 INSERT INTO messages VALUES (1, '旧消息');
                 PRAGMA wal_checkpoint(TRUNCATE);
                 INSERT INTO messages VALUES (2, '最新消息');",
        )
        .unwrap();
    let snapshot_database = snapshot.join("message.db");
    fs::copy(&source, &snapshot_database).unwrap();
    let source_wal = PathBuf::from(format!("{}-wal", source.display()));
    let snapshot_wal = PathBuf::from(format!("{}-wal", snapshot_database.display()));
    assert!(source_wal.is_file(), "fixture did not leave a WAL sidecar");
    fs::copy(source_wal, snapshot_wal).unwrap();

    let plain = sqlite3mc::decrypt_raw_wxsqlite3_database(&snapshot_database, key).unwrap();
    let opened =
        Connection::open_with_flags(plain, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let bodies = opened
        .prepare("SELECT body FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(bodies, vec!["旧消息", "最新消息"]);
}

#[test]
fn derived_aes128_fixture_opens_only_as_a_derived_key() {
    let directory = TestDirectory::new();
    let path = directory.0.join("derived.db");
    let derived_key = b"0123456789abcdef";
    let connection = sqlite3mc::create_encrypted_derived_fixture(&path, derived_key).unwrap();
    connection
        .execute_batch("CREATE TABLE fixture (value TEXT); INSERT INTO fixture VALUES ('ok');")
        .unwrap();
    drop(connection);

    let key = SourceKey::derived_aes128_with_config(derived_key, false, 0).unwrap();
    let opened = open_source_database(&path, Some(&key)).unwrap();
    let value = opened
        .query_row("SELECT value FROM fixture", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    assert_eq!(value, "ok");
    assert!(open_source_database(&path, Some(&SourceKey::passphrase(derived_key))).is_err());
    assert!(sqlite3mc::quick_verify_raw_wxsqlite3_key(
        &path,
        derived_key
    ));
    assert!(!sqlite3mc::quick_verify_raw_wxsqlite3_key(
        &path,
        b"xxxxxxxxxxxxxxxx"
    ));
}

#[test]
fn derived_aes256_fixture_opens_with_the_matching_cipher() {
    let directory = TestDirectory::new();
    let path = directory.0.join("derived-aes256.db");
    let derived_key = b"0123456789abcdef0123456789abcdef";
    let connection =
        sqlite3mc::create_encrypted_derived_aes256_fixture(&path, derived_key).unwrap();
    connection
        .execute_batch("CREATE TABLE fixture (value TEXT); INSERT INTO fixture VALUES ('ok');")
        .unwrap();
    drop(connection);

    let key = SourceKey::derived_aes256_with_config(derived_key, false, 0).unwrap();
    let opened = open_source_database(&path, Some(&key)).unwrap();
    let value = opened
        .query_row("SELECT value FROM fixture", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    assert_eq!(value, "ok");
}

#[cfg(windows)]
#[test]
#[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
fn authorized_real_probe_finds_a_key_without_exposing_it() {
    let sources = WindowsSourceAdapter.discover(None).unwrap();
    assert!(!sources.is_empty(), "no default data source was discovered");
    let directory = TestDirectory::new();
    let keys = run_isolated_key_probe(&sources[0], &directory.0).unwrap();
    let derived_count = keys
        .iter()
        .filter(|key| key.kind != SourceKeyKind::Passphrase)
        .count();
    let passphrase_count = keys.len().saturating_sub(derived_count);
    let direct_match = sources
        .iter()
        .any(|source| keys.iter().any(|key| key_opens_candidate(source, key)));
    assert!(
        direct_match,
        "probe candidates did not validate against any discovered source (derived candidates: {derived_count}, passphrase candidates: {passphrase_count})"
    );
    let (source, key) = sources
        .iter()
        .flat_map(|source| keys.iter().map(move |key| (source, key)))
        .find(|(source, key)| key_opens_candidate(source, key))
        .expect("validated source/key pair disappeared");
    let validation = TestDirectory::new();
    let export = collect_candidate(
        source.clone(),
        validation.0.join("authorized-collection"),
        Some(key),
        authorization(),
    )
    .expect("validated raw key did not complete a collection");
    assert!(
        export.message_count > 0,
        "authorized source contained no messages"
    );
}

#[test]
#[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
fn authorized_real_collection_includes_group_metadata_without_exposing_content() {
    let export = collect_local_export(false, false, None).unwrap();
    let message_count = export
        .batches
        .iter()
        .map(|batch| batch.messages.len())
        .sum::<usize>();
    let announcement_count = export
        .batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .filter(|message| message.raw_type == "group_announcement")
        .count();
    assert!(message_count > 0, "authorized source contained no messages");
    assert!(
        announcement_count > 0,
        "authorized source contained no readable group announcements"
    );
    eprintln!(
        "authorized collection verified: {message_count} messages, {announcement_count} group announcements"
    );
}

#[test]
#[ignore = "requires the user's explicit authorization and a running trusted WXWork.exe"]
fn authorized_real_collection_includes_media_without_exposing_content() {
    let streamed_files = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let streamed_count = std::sync::Arc::clone(&streamed_files);
    let media_sink: wecom_archive_server::LocalCollectionMediaSink =
        std::sync::Arc::new(move |_, _, _| {
            streamed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
    let export = collect_local_export_internal(
        true,
        false,
        None,
        None,
        std::sync::Arc::new(|_, _, _| {}),
        Some(&media_sink),
    )
    .unwrap();
    assert!(
        export.message_count > 0,
        "authorized source contained no messages"
    );
    assert!(
        export.media_count > 0,
        "authorized source contained no media references"
    );
    assert!(export.media_blobs.is_empty());
    eprintln!(
        "authorized media collection verified: {} media references, {} streamed files, {} missing files",
        export.media_count,
        streamed_files.load(std::sync::atomic::Ordering::Relaxed),
        export.missing_media_count
    );
    let images = export
        .batches
        .iter()
        .flat_map(|batch| &batch.messages)
        .filter(|message| message.message_type == MessageType::Image)
        .flat_map(|message| &message.media)
        .collect::<Vec<_>>();
    eprintln!(
        "authorized image collection verified: {} image references, {} verified images",
        images.len(),
        images
            .iter()
            .filter(|media| media.integrity == MediaIntegrity::Verified)
            .count()
    );
}

#[test]
fn collector_config_without_log_field_uses_signed_off_default() {
    let (public_key, private_key) =
        source_windows::enterprise_crypto::generate_rsa_key_pair().unwrap();
    let expires_at = (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
    let mut artifact = serde_json::json!({
        "schemaVersion": "enterprise-collector.v1",
        "collectorId": "collector-test",
        "displayName": "测试采集端",
        "configRevision": 1,
        "configExpiresAt": expires_at,
        "organizationId": "org-test",
        "organizationName": "测试组织",
        "collectionNotice": "测试告知",
        "keyId": "key-test",
        "publicKeyHex": "aa",
        "signingPublicKeyHex": hex::encode(&public_key),
        "signatureHex": "",
        "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
        "uploadToken": "test-token",
        "includeFiles": false,
        "includeImages": false,
        "dataRedaction": true,
        "offlineExportEnabled": false,
        "hiddenModeEnabled": false,
        "enabled": true,
        "manualCollectionRequestId": null,
        "collectorSchedule": { "mode": "disabled", "intervalMinutes": 60, "dailyTime": "02:00" },
    });
    let config: EnterpriseCollectorConfig = serde_json::from_value(artifact.clone()).unwrap();
    assert_eq!(config.collector_log_level, "off");
    let signed_payload = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": 1,
        "expiresAt": expires_at,
        "config": {
            "collectorId": "collector-test",
            "displayName": "测试采集端",
            "enabled": true,
            "configRevision": 1,
            "organizationId": "org-test",
            "organizationName": "测试组织",
            "collectionNotice": "测试告知",
            "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
            "keyId": "key-test",
            "publicKeyHex": "aa",
            "includeMedia": false,
            "dataRedaction": true,
            "offlineExportEnabled": false,
            "hiddenModeEnabled": false,
            "collectorLogLevel": "off",
            "manualCollectionRequestId": null,
            "schedule": { "mode": "disabled", "intervalMinutes": 60, "dailyTime": "02:00" },
        },
    }))
    .unwrap();
    assert_eq!(
        collector_control_payload(&config, &expires_at).unwrap(),
        signed_payload
    );
    let signature = source_windows::enterprise_crypto::sign(&private_key, &signed_payload).unwrap();
    artifact["signatureHex"] = hex::encode(signature).into();
    let loaded: EnterpriseCollectorConfig = serde_json::from_value(artifact).unwrap();
    let signature = hex::decode(&loaded.signature_hex).unwrap();
    source_windows::enterprise_crypto::verify(
        &public_key,
        &collector_control_payload(&loaded, &expires_at).unwrap(),
        &signature,
    )
    .unwrap();
}

#[test]
fn collector_control_envelope_rejects_rollback_expiry_and_bad_signature() {
    let (public_key, private_key) =
        source_windows::enterprise_crypto::generate_rsa_key_pair().unwrap();
    let expires_at = (Utc::now() + TimeDelta::minutes(5)).to_rfc3339();
    let control_config = serde_json::json!({
        "collectorId": "collector-test",
        "displayName": "测试采集端",
        "enabled": true,
        "configRevision": 3,
        "organizationId": "org-test",
        "organizationName": "测试组织",
        "collectionNotice": "测试告知",
        "uploadUrl": "http://127.0.0.1:9812/api/v1/imports/enterprise",
        "keyId": "key-test",
        "publicKeyHex": "aa",
        "includeMedia": false,
        "dataRedaction": true,
        "offlineExportEnabled": false,
        "schedule": { "mode": "interval", "intervalMinutes": 15, "dailyTime": "02:00" }
    });
    let payload = serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": 3,
        "expiresAt": expires_at,
        "config": control_config,
    });
    let signature = source_windows::enterprise_crypto::sign(
        &private_key,
        &serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    let control = serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": 3,
        "expiresAt": payload["expiresAt"],
        "config": payload["config"],
        "signatureHex": hex::encode(signature),
    });
    let public_hex = hex::encode(public_key);
    verify_collector_control_envelope(&control, &public_hex, 2).unwrap();
    assert_eq!(
        verify_collector_control_envelope(&control, &public_hex, 4)
            .unwrap_err()
            .code,
        "COLLECTOR_CONFIG_ROLLBACK"
    );

    let mut expired = control.clone();
    expired["expiresAt"] = (Utc::now() - TimeDelta::minutes(1)).to_rfc3339().into();
    assert_eq!(
        verify_collector_control_envelope(&expired, &public_hex, 0)
            .unwrap_err()
            .code,
        "COLLECTOR_CONFIG_EXPIRED"
    );

    let mut invalid = control;
    invalid["signatureHex"] = "00".into();
    assert_eq!(
        verify_collector_control_envelope(&invalid, &public_hex, 0)
            .unwrap_err()
            .code,
        "COLLECTOR_CONFIG_SIGNATURE_INVALID"
    );
}
