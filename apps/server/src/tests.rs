use archive_domain::ClientExportV1;
use archive_export::{ExportFormat, ExportScope};
use archive_transfer::read_json;
use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{Local, NaiveTime, TimeDelta, TimeZone};
use sha2::{Digest, Sha256};
use source_windows::enterprise_crypto;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use uuid::Uuid;

use super::*;

#[test]
fn only_source_discovery_failures_offer_a_custom_root_retry() {
    assert!(is_local_source_not_found(
        "未发现可支持的本机企业微信数据，请确认客户端已登录。"
    ));
    assert!(is_local_source_not_found(
        "这个文件夹里没找到企业微信数据库，请填企业微信实际存放聊天记录的那一层目录。"
    ));
    assert!(!is_local_source_not_found("读取授权失败。"));
    assert_eq!(
        local_collection_error(
            "数据目录无效或无法访问，请填写本机磁盘上的企业微信存储目录（不能是盘符根目录）。"
                .to_owned()
        )
        .body
        .code,
        "LOCAL_SOURCE_NOT_FOUND"
    );
}
use archive_domain::{
    ArchiveBatchV1, BATCH_SCHEMA_VERSION, CollectionScope, ConversationV1, EmployeeNoticeEvidence,
    ExternalContactConsent, LifecycleState, MESSAGE_SCHEMA_VERSION, MediaBlobV1, MessageDirection,
    MessageType, MessageV1, ParticipantV1, RetentionDirective,
};
use archive_transfer::create_export;
use chrono::{DateTime, Utc};
use serde_json::json;

const TEST_TOKEN: &str = "0123456789abcdefghijklmnop";

#[test]
fn private_collector_url_selects_listener_address() {
    assert_eq!(
        configured_private_listen_address("http://10.10.1.202:9812"),
        Some("10.10.1.202:9812".parse().unwrap())
    );
    assert_eq!(
        configured_private_listen_address("http://192.168.1.20:9812/api/v1/imports/enterprise"),
        Some("192.168.1.20:9812".parse().unwrap())
    );
    assert_eq!(
        configured_private_listen_address("http://[fd12:3456::20]:9812/api"),
        Some("[fd12:3456::20]:9812".parse().unwrap())
    );
    assert_eq!(
        configured_private_listen_address("http://127.0.0.1:9812"),
        None
    );
    assert_eq!(
        configured_private_listen_address("http://8.8.8.8:9812"),
        None
    );
    assert_eq!(
        configured_private_listen_address("http://archive.example:9812"),
        None
    );
    assert_eq!(
        configured_private_listen_address("http://10.10.1.202"),
        None
    );
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("archive-server-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(directory: &TestDirectory) -> AppState {
    AppState {
        data_root: Arc::new(directory.0.clone()),
        archive_path: Arc::new(directory.0.join("archive.db")),
        token_sha256: Arc::new(Mutex::new(Sha256::digest(TEST_TOKEN.as_bytes()).into())),
        access_token_path: Arc::new(directory.0.join(ACCESS_TOKEN_FILE)),
        enterprise_config: Arc::new(Mutex::new(new_enterprise_config())),
        collector_template: None,
        local_collector: None,
        local_collection_progress: Arc::new(Mutex::new(LocalCollectionProgress::default())),
        local_collection_running: Arc::new(AtomicBool::new(false)),
        archive_update: watch::channel(0).0,
        network_mode: "loopback_only",
    }
}

fn authorized_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {TEST_TOKEN}").parse().unwrap(),
    );
    headers
}

fn enable_super_admin(state: &AppState) {
    state.enterprise_config.lock().unwrap().super_admin_enabled = true;
}

#[tokio::test]
async fn super_admin_defaults_to_enabled_and_can_be_disabled_for_server_export() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    assert!(
        app_state
            .enterprise_config
            .lock()
            .unwrap()
            .super_admin_enabled
    );
    app_state
        .enterprise_config
        .lock()
        .unwrap()
        .super_admin_enabled = false;

    let message_error = list_messages(
        State(app_state.clone()),
        authorized_headers(),
        Query(MessagesQuery {
            conversation_id: "conversation".into(),
            participant_id: None,
            text: None,
            message_type: None,
            media_only: None,
            limit: None,
            offset: None,
            sort: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(message_error.status, StatusCode::FORBIDDEN);

    let error = create_export_job(
        State(app_state),
        authorized_headers(),
        Json(ExportRequest {
            scope: ExportScope::EntireArchive,
            format: ExportFormat::Json,
            conversation_id: None,
            conversation_ids: Vec::new(),
            start_date: None,
            end_date: None,
            participant_id: None,
            text: None,
            message_type: None,
            media_only: None,
            data_redaction: false,
            simplify: false,
            pretty: false,
            conversation_order: ExportOrder::Descending,
            message_order: ExportOrder::Descending,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
    assert_eq!(error.body.code, "SUPER_ADMIN_REQUIRED");
}

#[tokio::test]
async fn super_admin_setting_is_persisted() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let Json(response) = update_super_admin(
        State(app_state),
        authorized_headers(),
        Json(SuperAdminRequest { enabled: true }),
    )
    .await
    .unwrap();
    assert!(response.super_admin_enabled);
    assert!(load_enterprise_config(&directory.0).super_admin_enabled);
}

#[test]
fn collection_schedule_calculates_interval_and_daily_runs() {
    let now = Local
        .with_ymd_and_hms(2026, 1, 15, 1, 30, 0)
        .single()
        .unwrap();
    let interval = CollectionSchedule {
        mode: CollectionScheduleMode::Interval,
        interval_minutes: 45,
        daily_time: "02:00".into(),
    };
    assert_eq!(
        interval.next_after(now).unwrap() - now,
        TimeDelta::minutes(45)
    );

    let daily = CollectionSchedule {
        mode: CollectionScheduleMode::Daily,
        interval_minutes: 60,
        daily_time: "02:00".into(),
    };
    let next = daily.next_after(now).unwrap();
    assert_eq!(next.date_naive(), now.date_naive());
    assert_eq!(next.time(), NaiveTime::from_hms_opt(2, 0, 0).unwrap());
    assert!(
        CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 1,
            daily_time: "02:00".into(),
        }
        .validate()
        .is_ok()
    );
    assert!(
        CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 0,
            daily_time: "02:00".into(),
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn local_collection_plans_are_independent_and_persisted() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let first_schedule = CollectionSchedule {
        mode: CollectionScheduleMode::Daily,
        interval_minutes: 60,
        daily_time: "03:15".into(),
    };
    let second_schedule = CollectionSchedule {
        mode: CollectionScheduleMode::Interval,
        interval_minutes: 30,
        daily_time: "02:00".into(),
    };

    let Json(first_response) = create_local_collection_plan(
        State(app_state.clone()),
        authorized_headers(),
        Json(LocalCollectionPlanRequest {
            name: "早班采集".into(),
            schedule: first_schedule.clone(),
            include_media: true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(first_response.local_plans.len(), 1);
    assert_eq!(first_response.local_plans[0].plan.schedule, first_schedule);

    let Json(second_response) = create_local_collection_plan(
        State(app_state.clone()),
        authorized_headers(),
        Json(LocalCollectionPlanRequest {
            name: "晚班采集".into(),
            schedule: second_schedule.clone(),
            include_media: false,
        }),
    )
    .await
    .unwrap();
    assert_eq!(second_response.local_plans.len(), 2);

    let persisted = load_enterprise_config(&directory.0);
    assert_eq!(persisted.local_collection_plans.len(), 2);
    assert_eq!(
        persisted.local_collection_plans[1].schedule,
        second_schedule
    );
}

#[tokio::test]
async fn enterprise_config_update_without_schedules_preserves_existing_plans() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        config.local_collection_plans.push(LocalCollectionPlan {
            id: "local-plan".into(),
            name: "已有计划".into(),
            include_media: true,
            schedule: CollectionSchedule::default(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            last_status: None,
            last_detail: None,
        });
        config.collector_schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 90,
            daily_time: "02:00".into(),
        };
    }

    let _ = update_enterprise_config(
        State(app_state.clone()),
        authorized_headers(),
        Json(EnterpriseConfigRequest {
            organization_name: "测试企业".into(),
            collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
            upload_url: default_upload_url(),
            key_id: None,
            include_media: true,
            data_redaction: false,
            offline_export_enabled: false,
            hidden_mode_enabled: false,
            server_log_level: default_log_level(),
            collector_log_level: default_collector_log_level(),
            super_admin_enabled: None,
            server_schedule: None,
            collector_schedule: None,
        }),
    )
    .await
    .unwrap();

    let config = app_state.enterprise_config.lock().unwrap();
    assert_eq!(config.local_collection_plans.len(), 1);
    assert_eq!(config.collector_schedule.interval_minutes, 90);
}

#[tokio::test]
async fn local_collection_plan_rejects_an_invalid_interval() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);

    let error = create_local_collection_plan(
        State(app_state),
        authorized_headers(),
        Json(LocalCollectionPlanRequest {
            name: "无效计划".into(),
            include_media: false,
            schedule: CollectionSchedule {
                mode: CollectionScheduleMode::Interval,
                interval_minutes: 0,
                daily_time: "02:00".into(),
            },
        }),
    )
    .await
    .unwrap_err();

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
}

#[test]
fn streams_local_media_to_the_content_addressed_store() {
    let directory = TestDirectory::new();
    let source = directory.0.join("source.bin");
    let bytes = b"streamed local media";
    std::fs::write(&source, bytes).unwrap();
    let hash = encode_hex(&Sha256::digest(bytes));

    persist_local_media_file(&directory.0, &source, &hash, bytes.len() as u64).unwrap();

    assert_eq!(
        std::fs::read(directory.0.join("media").join(hash)).unwrap(),
        bytes
    );
    assert!(
        std::fs::read_dir(directory.0.join("media"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".partial"))
    );
}

fn export_bytes(body: &str, batch_id: Uuid, batch_hash: &str) -> Vec<u8> {
    let sent_at = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .unwrap()
        .with_timezone(&Utc);
    let batch = ArchiveBatchV1 {
        schema_version: BATCH_SCHEMA_VERSION.into(),
        batch_id,
        source_adapter: "synthetic_fixture".into(),
        source_instance_id: "source-fixture".into(),
        collected_at: sent_at,
        employee_notice: EmployeeNoticeEvidence {
            notice_version: "test-notice".into(),
            displayed_at: sent_at,
            acknowledged_at: Some(sent_at),
            evidence_hash: "test-evidence".into(),
        },
        external_contact_consent: ExternalContactConsent::NotApplicable,
        collection_scope: CollectionScope {
            source_ids: vec!["source-fixture".into()],
            conversation_ids: vec!["conversation-fixture".into()],
            starts_at: None,
            ends_at: None,
            message_types: vec![MessageType::Text],
        },
        cursor_before: None,
        cursor_after: Some("cursor-1".into()),
        messages: vec![MessageV1 {
            schema_version: MESSAGE_SCHEMA_VERSION.into(),
            source_kind: "synthetic_fixture".into(),
            source_instance_id: "source-fixture".into(),
            source_message_id: "message-1".into(),
            stable_message_id: "stable-message-1".into(),
            conversation_id: "conversation-fixture".into(),
            sender_id: Some("participant-fixture".into()),
            sent_at,
            direction: MessageDirection::Incoming,
            message_type: MessageType::Text,
            body_text: Some(body.into()),
            quoted_message_id: None,
            lifecycle: LifecycleState::Active,
            media: vec![],
            raw_type: "text".into(),
            raw_payload: json!({"text": body}),
            parser_version: "synthetic-fixture.v1".into(),
            collection_batch_id: batch_id,
        }],
        content_sha256: batch_hash.into(),
        retention: RetentionDirective {
            policy_id: None,
            expires_at: None,
            legal_hold: false,
        },
    };
    let export = create_export(
        "test-client",
        vec![batch],
        vec![ConversationV1 {
            conversation_id: "conversation-fixture".into(),
            display_name: Some("测试会话".into()),
            conversation_type: Some("direct".into()),
            participant_ids: vec!["participant-fixture".into()],
        }],
        vec![ParticipantV1 {
            participant_id: "participant-fixture".into(),
            display_name: Some("测试成员".into()),
            participant_kind: Some("employee".into()),
        }],
    )
    .unwrap();
    serde_json::to_vec(&export).unwrap()
}

#[tokio::test]
async fn global_search_pages_cjk_matches_and_keeps_message_locations() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "我们讨论项目进度和验收结果",
        Uuid::new_v4(),
        "synthetic",
    ))
    .unwrap();
    let mut batch = export.batches[0].clone();
    let template = batch.messages[0].clone();
    batch.messages = (0..55)
        .map(|index| {
            let mut message = template.clone();
            message.stable_message_id = format!("search-{index:03}");
            message.source_message_id = format!("search-source-{index:03}");
            message.sent_at += TimeDelta::seconds(index / 3);
            message
        })
        .collect();
    let mut store = archive_store::ArchiveStore::open(&app_state.archive_path).unwrap();
    store.ingest_batch(&batch).unwrap();
    drop(store);
    for ascending in [false, true] {
        let mut ids = Vec::new();
        for offset in [0, 50] {
            let response = search_messages(
                State(app_state.clone()),
                authorized_headers(),
                Query(GlobalSearchQuery {
                    q: "项目进度".into(),
                    limit: Some(50),
                    offset: Some(offset),
                    sort: Some(if ascending { "asc" } else { "desc" }.into()),
                }),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(response.len(), if offset == 0 { 50 } else { 5 });
            for (index, result) in response.into_iter().enumerate() {
                assert_eq!(result.offset_in_conversation, offset + index as u64);
                ids.push(result.message.message.stable_message_id);
            }
        }
        let mut expected: Vec<_> = (0..55).map(|index| format!("search-{index:03}")).collect();
        if !ascending {
            expected.reverse();
        }
        assert_eq!(ids, expected);
    }
}

#[tokio::test]
async fn rejects_missing_and_incorrect_tokens() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let missing = import_json(State(app_state.clone()), HeaderMap::new(), Body::from("{}"))
        .await
        .unwrap_err();
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);

    let mut wrong_headers = HeaderMap::new();
    wrong_headers.insert(
        "authorization",
        "Bearer 000000000000000000000000".parse().unwrap(),
    );
    let wrong = import_json(State(app_state), wrong_headers, Body::from("{}"))
        .await
        .unwrap_err();
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn local_collection_query_overrides_the_configured_media_mode() {
    let directory = TestDirectory::new();
    let mut app_state = state(&directory);
    app_state.enterprise_config.lock().unwrap().include_files = false;
    app_state.enterprise_config.lock().unwrap().include_images = false;
    let selected_modes = Arc::new(Mutex::new(Vec::new()));
    let captured_modes = Arc::clone(&selected_modes);
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "本机消息",
        Uuid::new_v4(),
        "local-collection",
    ))
    .unwrap();
    app_state.local_collector = Some(Arc::new(
        move |include_media, _, _, source_root, reporter, _| {
            captured_modes
                .lock()
                .unwrap()
                .push((include_media, source_root));
            reporter(55, "解析消息", "正在解析测试消息。");
            Ok(export.clone())
        },
    ));

    let _ = collect_local(
        State(app_state.clone()),
        authorized_headers(),
        Query(LocalCollectionQuery {
            include_media: Some(true),
            source_root: Some(r"D:\FixtureWXWork".to_owned()),
        }),
    )
    .await
    .unwrap();

    assert_eq!(
        *selected_modes.lock().unwrap(),
        vec![(true, Some(r"D:\FixtureWXWork".to_owned()))]
    );
    let progress = app_state.local_collection_progress.lock().unwrap().clone();
    assert!(!progress.running);
    assert_eq!(progress.percent, 100);
    assert_eq!(progress.stage, "采集完成");
}

#[tokio::test]
async fn generated_collector_copies_the_configured_single_executable_template() {
    let directory = TestDirectory::new();
    let template = directory.0.join("enterprise-template.exe");
    std::fs::write(&template, b"single executable template").unwrap();
    let mut app_state = state(&directory);
    app_state.collector_template = Some(Arc::new(template));
    app_state
        .enterprise_config
        .lock()
        .unwrap()
        .organization_name = "测试企业".into();
    app_state
        .enterprise_config
        .lock()
        .unwrap()
        .collector_schedule = CollectionSchedule {
        mode: CollectionScheduleMode::Interval,
        interval_minutes: 45,
        daily_time: "02:00".into(),
    };

    let before = app_state.enterprise_config.lock().unwrap().clone();
    let Json(response) = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();

    assert!(response.executable_generated);
    assert_eq!(response.key_id, before.key_id);
    assert!(
        response
            .file_name
            .starts_with(&format!("{}-", before.key_id))
    );
    assert!(response.file_name.ends_with(".exe"));
    assert_eq!(
        response.directory,
        directory.0.join("collectors").display().to_string()
    );
    let artifact: serde_json::Value = serde_json::from_str(&response.artifact).unwrap();
    assert_eq!(artifact["keyId"], before.key_id);
    assert_eq!(artifact["publicKeyHex"], before.public_key_hex);
    assert_eq!(artifact["collectorLogLevel"], before.collector_log_level);
    assert_eq!(artifact["collectorSchedule"]["mode"], "interval");
    assert_eq!(artifact["collectorSchedule"]["intervalMinutes"], 45);
    let after = app_state.enterprise_config.lock().unwrap().clone();
    assert!(after.collectors.iter().any(|collector| {
        collector.collector_id == response.collector_id
            && collector.key_id == before.key_id
            && collector.file_name == response.file_name
            && collector.schedule.mode == CollectionScheduleMode::Interval
            && collector.private_key_protected_hex.is_empty()
    }));
    let executable = directory.0.join("collectors").join(&response.file_name);
    let executable_bytes = std::fs::read(&executable).unwrap();
    assert!(executable_bytes.starts_with(b"single executable template"));
    let embedded_config = archive_transfer::read_enterprise_collector_config(&executable).unwrap();
    assert_eq!(embedded_config, response.artifact.as_bytes());
    assert!(
        !directory
            .0
            .join("collectors")
            .join(format!("{}.wca-collector", response.file_name))
            .exists()
    );

    let Json(second) = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();
    assert_ne!(second.file_name, response.file_name);
    let Json(list) = list_collectors(State(app_state), authorized_headers())
        .await
        .unwrap();
    assert_eq!(list.collectors.len(), 2);
    assert!(list.collectors.iter().all(|collector| {
        collector.schedule.mode == CollectionScheduleMode::Interval
            && collector.executable_available
    }));
}

#[tokio::test]
async fn regenerated_access_token_replaces_the_persisted_token_and_authentication_hash() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);

    let Json(response) = regenerate_access_token(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();

    assert_eq!(response.access_token.len(), 64);
    assert!(authorize(&authorized_headers(), &app_state.token_sha256).is_err());
    let mut new_headers = HeaderMap::new();
    new_headers.insert(
        "authorization",
        format!("Bearer {}", response.access_token).parse().unwrap(),
    );
    assert!(authorize(&new_headers, &app_state.token_sha256).is_ok());
    assert!(app_state.access_token_path.is_file());
}

#[tokio::test]
async fn rotating_unused_enterprise_key_does_not_preserve_it() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let before = app_state.enterprise_config.lock().unwrap().clone();

    let Json(response) = rotate_enterprise_key(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();
    let after = app_state.enterprise_config.lock().unwrap().clone();

    assert_ne!(response.key_id, before.key_id);
    assert_ne!(
        after.private_key_protected_hex,
        before.private_key_protected_hex
    );
    assert!(
        !after
            .key_history
            .iter()
            .any(|record| record.key_id == before.key_id)
    );
}

#[tokio::test]
async fn custom_enterprise_key_id_replaces_the_key_pair() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let before = app_state.enterprise_config.lock().unwrap().clone();
    let custom_key_id = "custom-enterprise-key-20260914".to_owned();

    let Json(response) = update_enterprise_config(
        State(app_state.clone()),
        authorized_headers(),
        Json(EnterpriseConfigRequest {
            organization_name: "测试企业".into(),
            collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
            upload_url: default_upload_url(),
            key_id: Some(custom_key_id.clone()),
            include_media: false,
            data_redaction: false,
            offline_export_enabled: false,
            hidden_mode_enabled: false,
            server_log_level: default_log_level(),
            collector_log_level: default_collector_log_level(),
            super_admin_enabled: None,
            server_schedule: None,
            collector_schedule: None,
        }),
    )
    .await
    .unwrap();
    let after = app_state.enterprise_config.lock().unwrap().clone();

    assert_eq!(response.key_id, custom_key_id);
    assert_eq!(after.key_id, custom_key_id);
    assert_ne!(after.public_key_hex, before.public_key_hex);
    assert_ne!(
        after.private_key_protected_hex,
        before.private_key_protected_hex
    );
}

#[tokio::test]
async fn rotating_used_enterprise_key_preserves_it_for_existing_collectors() {
    let directory = TestDirectory::new();
    let template = directory.0.join("enterprise-template.exe");
    std::fs::write(&template, b"single executable template").unwrap();
    let mut app_state = state(&directory);
    app_state.collector_template = Some(Arc::new(template));
    app_state
        .enterprise_config
        .lock()
        .unwrap()
        .organization_name = "测试企业".into();
    let before = app_state.enterprise_config.lock().unwrap().clone();

    let Json(collector) = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();
    let Json(response) = rotate_enterprise_key(State(app_state.clone()), authorized_headers())
        .await
        .unwrap();
    let after = app_state.enterprise_config.lock().unwrap().clone();

    assert_eq!(collector.key_id, before.key_id);
    assert_ne!(response.key_id, before.key_id);
    assert!(after.key_history.iter().any(|record| {
        record.key_id == before.key_id
            && record.private_key_protected_hex == before.private_key_protected_hex
    }));

    let plaintext = export_bytes("轮换后的历史密钥仍可解密", Uuid::new_v4(), "batch-history");
    let export: archive_domain::ClientExportV1 = serde_json::from_slice(&plaintext).unwrap();
    let encrypted =
        enterprise_crypto::encrypt(&decode_hex(&before.public_key_hex).unwrap(), &plaintext)
            .unwrap();
    let package = archive_transfer::create_enterprise_package(
        &export,
        &before.organization_id,
        &before.key_id,
        &encrypted.wrapped_dek,
        &encrypted.nonce,
        &encrypted.ciphertext,
        &encrypted.tag,
    )
    .unwrap();
    let Json(imported) =
        import_enterprise(State(app_state), authorized_headers(), Body::from(package))
            .await
            .unwrap();
    assert_eq!(imported.inserted, 1);
}

#[tokio::test]
async fn rejects_invalid_schema_and_modified_hash() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let bytes = export_bytes("原始内容", Uuid::new_v4(), "batch-a");

    let mut invalid_schema: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    invalid_schema["schema_version"] = json!("client-export.invalid");
    let schema_error = import_json(
        State(app_state.clone()),
        authorized_headers(),
        Body::from(serde_json::to_vec(&invalid_schema).unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(schema_error.status, StatusCode::UNPROCESSABLE_ENTITY);

    let mut modified_hash: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    modified_hash["content_sha256"] = json!("00");
    let hash_error = import_json(
        State(app_state),
        authorized_headers(),
        Body::from(serde_json::to_vec(&modified_hash).unwrap()),
    )
    .await
    .unwrap_err();
    assert_eq!(hash_error.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn repeated_import_is_idempotent() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let bytes = export_bytes("同一内容", Uuid::new_v4(), "batch-a");

    let first = import_json(
        State(app_state.clone()),
        authorized_headers(),
        Body::from(bytes.clone()),
    )
    .await
    .unwrap();
    assert_eq!(first.inserted, 1);
    let repeated = import_json(State(app_state), authorized_headers(), Body::from(bytes))
        .await
        .unwrap();
    assert_eq!(repeated.inserted, 0);
    assert_eq!(repeated.unchanged, 1);
    assert_eq!(repeated.revised, 0);
}

#[tokio::test]
async fn rejects_conflicting_batch_identifier() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    let batch_id = Uuid::new_v4();
    let first = export_bytes("第一份内容", batch_id, "batch-a");
    let conflict = export_bytes("冲突内容", batch_id, "batch-b");
    let _ = import_json(
        State(app_state.clone()),
        authorized_headers(),
        Body::from(first),
    )
    .await
    .unwrap();
    let error = import_json(State(app_state), authorized_headers(), Body::from(conflict))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.body.code, "BATCH_CONFLICT");
}

#[tokio::test]
async fn rejects_empty_and_oversized_bodies_without_partial_files() {
    let directory = TestDirectory::new();
    let empty = receive_json_with_limit(&directory.0, Body::empty(), 8)
        .await
        .unwrap_err();
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    let oversized = receive_json_with_limit(&directory.0, Body::from("123456789"), 8)
        .await
        .unwrap_err();
    assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
    let import_root = directory.0.join("imports");
    assert_eq!(std::fs::read_dir(import_root).unwrap().count(), 0);
}

#[tokio::test]
async fn creates_authenticated_server_export_file() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    enable_super_admin(&app_state);
    let bytes = export_bytes("服务端导出内容", Uuid::new_v4(), "batch-a");
    let _ = import_json(
        State(app_state.clone()),
        authorized_headers(),
        Body::from(bytes),
    )
    .await
    .unwrap();
    let result = create_export_job(
        State(app_state),
        authorized_headers(),
        Json(ExportRequest {
            scope: ExportScope::EntireArchive,
            format: ExportFormat::Json,
            conversation_id: None,
            conversation_ids: Vec::new(),
            start_date: None,
            end_date: None,
            participant_id: None,
            text: None,
            message_type: None,
            media_only: None,
            data_redaction: false,
            simplify: false,
            pretty: false,
            conversation_order: ExportOrder::Descending,
            message_order: ExportOrder::Descending,
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.message_count, 1);
    assert!(
        directory
            .0
            .join("exports")
            .join(&result.file_name)
            .is_file()
    );
    assert_eq!(
        std::path::Path::new(&result.file_name)
            .extension()
            .and_then(|value| value.to_str()),
        Some("json")
    );
    assert!(
        !directory
            .0
            .join("exports")
            .join("checksums.sha256")
            .exists()
    );
    assert!(!result.manifest_sha256.is_empty());
}

#[test]
fn concurrent_media_persistence_is_idempotent() {
    let directory = TestDirectory::new();
    let bytes = b"shared media";
    let blob = MediaBlobV1 {
        content_hash: encode_hex(&Sha256::digest(bytes)),
        original_name: Some("shared.bin".into()),
        mime_type: Some("application/octet-stream".into()),
        size_bytes: bytes.len() as u64,
        content_hex: encode_hex(bytes),
    };
    let root = Arc::new(directory.0.clone());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let workers = (0..8)
        .map(|_| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            let blob = blob.clone();
            std::thread::spawn(move || {
                barrier.wait();
                persist_media_blobs(&root, &[blob])
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    assert_eq!(
        std::fs::read(directory.0.join("media").join(&blob.content_hash)).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read_dir(directory.0.join("media"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn export_options_reach_both_writers_and_keep_full_local_json_importable() {
    let directory = TestDirectory::new();
    let mut app_state = state(&directory);
    enable_super_admin(&app_state);
    let bytes = export_bytes("普通消息\n第二行", Uuid::new_v4(), "fixture");
    let export: ClientExportV1 = serde_json::from_slice(&bytes).unwrap();
    let selected_roots = Arc::new(Mutex::new(Vec::new()));
    let captured_roots = Arc::clone(&selected_roots);
    app_state.local_collector = Some(Arc::new(move |_, _, _, source_root, _, _| {
        captured_roots.lock().unwrap().push(source_root);
        let mut snapshot = export.clone();
        snapshot.export_id = Uuid::new_v4();
        Ok(snapshot)
    }));
    let _ = import_json(
        State(app_state.clone()),
        authorized_headers(),
        Body::from(bytes),
    )
    .await
    .unwrap();
    for simplify in [false, true] {
        for pretty in [false, true] {
            for local in [false, true] {
                let mut payload = json!({"scope": "entire_archive", "format": "json", "simplify": simplify, "pretty": pretty});
                if local {
                    payload["sourceRoot"] = json!(r"D:\FixtureWXWork");
                }
                let response = if local {
                    create_local_export(
                        State(app_state.clone()),
                        authorized_headers(),
                        Json(serde_json::from_value(payload).unwrap()),
                    )
                    .await
                    .unwrap()
                } else {
                    create_export_job(
                        State(app_state.clone()),
                        authorized_headers(),
                        Json(serde_json::from_value(payload).unwrap()),
                    )
                    .await
                    .unwrap()
                };
                let path = directory.0.join("exports").join(&response.file_name);
                let text = std::fs::read_to_string(&path).unwrap();
                assert_eq!(text.contains('\n'), pretty);
                assert_eq!(response.message_count, 1);
                let document: serde_json::Value = serde_json::from_str(&text).unwrap();
                if simplify {
                    let conversation = &document["conversations"][0];
                    assert_eq!(conversation["conversation"], "测试会话");
                    assert_eq!(conversation["messages"][0]["sender"], "测试成员");
                    assert_eq!(conversation["messages"][0]["content"], "普通消息\n第二行");
                    assert!(!text.contains("participant-fixture"));
                    assert!(!text.contains("raw_payload"));
                    assert!(!text.contains("participants"));
                } else if local {
                    assert_eq!(read_json(&path).unwrap().message_count, 1);
                } else {
                    assert_eq!(
                        document["conversations"][0]["conversation_id"],
                        "conversation-fixture"
                    );
                    assert_eq!(
                        document["conversations"][0]["messages"][0]["stable_message_id"],
                        "stable-message-1"
                    );
                }
            }
        }
    }
    assert_eq!(
        *selected_roots.lock().unwrap(),
        vec![Some(r"D:\FixtureWXWork".to_owned()); 4]
    );
}

#[test]
fn legacy_export_requests_default_to_full_compact_output() {
    let local: LocalExportRequest = serde_json::from_value(json!({"format":"json"})).unwrap();
    let server: ExportRequest =
        serde_json::from_value(json!({"scope":"entire_archive", "format":"json"})).unwrap();
    assert!(!local.simplify && !local.pretty && !server.simplify && !server.pretty);
}

#[test]
fn archive_streaming_preserves_filtered_formats_orders_redaction_and_snapshot() {
    use archive_domain::{MediaIntegrity, MediaRefV1};
    use archive_export::{ExportPackage, ExportSource, export_package, export_streamed};
    use archive_store::{ArchiveStore, MessageQuery};
    let directory = TestDirectory::new();
    let path = directory.0.join("stream.db");
    let mut writer = ArchiveStore::open(&path).unwrap();
    let export: ClientExportV1 = serde_json::from_slice(&export_bytes(
        "needle 密码 secret-456",
        Uuid::new_v4(),
        "fixture",
    ))
    .unwrap();
    let mut batch = export.batches[0].clone();
    let template = batch.messages[0].clone();
    batch.messages = (0..9)
        .map(|index| {
            let mut message = template.clone();
            message.stable_message_id = format!("stable-{index:02}");
            message.source_message_id = format!("source-{index:02}");
            message.conversation_id = match index {
                0..=3 => "a",
                4..=7 => "b",
                _ => "announcement-only",
            }
            .into();
            message.sent_at += TimeDelta::seconds(index / 2);
            if index == 3 || index >= 7 {
                message.raw_type = "group_announcement".into();
            }
            if index % 2 == 0 {
                message.media.push(MediaRefV1 {
                    content_hash: None,
                    original_name: Some("@<attachment>.txt".into()),
                    mime_type: None,
                    size_bytes: None,
                    source_locator: "synthetic".into(),
                    integrity: MediaIntegrity::Missing,
                    missing_reason: Some("synthetic missing media".into()),
                });
            }
            message
        })
        .collect();
    writer.ingest_batch(&batch).unwrap();
    {
        let mut moved = batch.clone();
        moved.batch_id = Uuid::new_v4();
        moved.messages.truncate(1);
        moved.messages[0].collection_batch_id = moved.batch_id;
        moved.messages[0].conversation_id = "b".into();
        writer.ingest_batch(&moved).unwrap();
        let reader = ArchiveStore::open_read_only(&path).unwrap();
        let source = ArchiveExportSource::new(
            &reader,
            ArchiveExportOptions {
                query: MessageQuery {
                    conversation_id: Some("a".into()),
                    ..Default::default()
                },
                conversation_ids: vec!["a".into(), "b".into(), "announcement-only".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let mut count = 0;
        source
            .visit_conversations(&mut |id| {
                source.visit_messages(id, &mut |message| {
                    assert_eq!(message.conversation_id, id);
                    count += 1;
                    Ok(())
                })
            })
            .unwrap();
        assert_eq!(count, 9);
    }
    // Older revisions can update payloads without changing indexed columns.
    // Export must preserve decoded grouping and nanosecond ordering anyway.
    let mut revised = batch.clone();
    revised.batch_id = Uuid::new_v4();
    revised.messages.truncate(4);
    for message in &mut revised.messages {
        message.collection_batch_id = revised.batch_id;
    }
    revised.messages[0].conversation_id = "b".into();
    revised.messages[0].sent_at += TimeDelta::seconds(2) + TimeDelta::nanoseconds(7);
    revised.messages[1].sent_at = "2016-12-31T23:59:60Z".parse().unwrap();
    revised.messages[2].sent_at = template.sent_at + TimeDelta::nanoseconds(1);
    revised.messages[3].sent_at = "2016-12-31T23:59:59.999999999Z".parse().unwrap();
    writer.ingest_batch(&revised).unwrap();
    let names: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::from([
        ("a".into(), "=会话 A".into()),
        ("b".into(), "会话 B".into()),
    ]);
    let conversations = names
        .iter()
        .map(|(id, name)| {
            let mut conversation = export.conversations[0].clone();
            conversation.conversation_id = id.clone();
            conversation.display_name = Some(name.clone());
            conversation
        })
        .collect::<Vec<_>>();
    writer
        .upsert_directory(&conversations, &export.participants)
        .unwrap();
    let reader = ArchiveStore::open_read_only(&path).unwrap();
    for format in [
        ExportFormat::Json,
        ExportFormat::Csv,
        ExportFormat::Html,
        ExportFormat::Md,
    ] {
        for simplify in [false, true] {
            for pretty in [false, true] {
                for conversation_ascending in [false, true] {
                    for message_ascending in [false, true] {
                        let query = MessageQuery {
                            text: Some("needle".into()),
                            participant_id: template.sender_id.clone(),
                            starts_at: Some(template.sent_at),
                            ends_at: Some(template.sent_at + TimeDelta::seconds(3)),
                            media_only: pretty,
                            limit: 50_000,
                            sort_ascending: message_ascending,
                            ..Default::default()
                        };
                        let source = ArchiveExportSource::new(
                            &reader,
                            ArchiveExportOptions {
                                query: query.clone(),
                                conversation_ids: vec!["a".into(), "b".into(), "a".into()],
                                simplify,
                                pretty,
                                data_redaction: true,
                                conversation_order_ascending: conversation_ascending,
                            },
                        )
                        .unwrap();
                        let mut messages = reader.query_messages(&query).unwrap();
                        messages.retain(|message| {
                            message.conversation_id == "a" || message.conversation_id == "b"
                        });
                        redact_sensitive_messages(&mut messages);
                        let media = messages
                            .iter()
                            .flat_map(|message| &message.media)
                            .map(|item| archive_export::ExportMedia {
                                content_sha256: item.content_hash.clone(),
                                original_name: item.original_name.clone(),
                                mime_type: item.mime_type.clone(),
                                size_bytes: item.size_bytes,
                                source_path: None,
                                integrity: item.integrity.clone(),
                                missing_reason: item.missing_reason.clone(),
                            })
                            .collect();
                        let participants = export
                            .participants
                            .iter()
                            .map(|item| archive_export::ExportParticipant {
                                participant_id: item.participant_id.clone(),
                                display_name: if simplify || pretty {
                                    item.display_name.clone()
                                } else {
                                    None
                                },
                                participant_kind: None,
                            })
                            .collect();
                        let package = ExportPackage {
                            archive_id: "synthetic".into(),
                            scope: ExportScope::SelectedConversations,
                            format,
                            messages,
                            participants,
                            media,
                            conversation_names: names.clone(),
                            simplify,
                            pretty,
                            generated_at: template.sent_at,
                            conversation_order_ascending: conversation_ascending,
                            message_order_ascending: message_ascending,
                        };
                        let reference = directory.0.join("reference");
                        let stream = directory.0.join("stream");
                        let original =
                            export_package(&package, &reference, &AtomicBool::new(false)).unwrap();
                        let actual =
                            export_streamed(&package, &source, &stream, &AtomicBool::new(false))
                                .unwrap();
                        assert_eq!(actual.message_count, original.message_count);
                        assert_eq!(actual.manifest_sha256, original.manifest_sha256);
                        assert_eq!(
                            std::fs::read(&reference).unwrap(),
                            std::fs::read(&stream).unwrap()
                        );
                        assert!(
                            !std::fs::read_to_string(&stream)
                                .unwrap()
                                .contains("secret-456")
                        );
                        std::fs::remove_file(reference).unwrap();
                        std::fs::remove_file(stream).unwrap();
                    }
                }
            }
        }
    }
    let source = ArchiveExportSource::new(&reader, ArchiveExportOptions::default()).unwrap();
    let initial_count = source.statistics().message_count;
    let mut added = export.batches[0].clone();
    added.batch_id = Uuid::new_v4();
    added.messages[0].collection_batch_id = added.batch_id;
    added.messages[0].stable_message_id = "added".into();
    added.messages[0].source_message_id = "added".into();
    writer.ingest_batch(&added).unwrap();
    let mut count = 0;
    source
        .visit_conversations(&mut |id| {
            source.visit_messages(id, &mut |_| {
                count += 1;
                Ok(())
            })
        })
        .unwrap();
    assert_eq!(count, initial_count);
    drop(source);
    assert_eq!(reader.message_count().unwrap(), initial_count + 1);
    let empty_source = ArchiveExportSource::new(
        &reader,
        ArchiveExportOptions {
            conversation_ids: vec!["not-present".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(empty_source.statistics().message_count, 0);
}

#[test]
fn data_redaction_removes_plaintext_values() {
    let export: archive_domain::ClientExportV1 = serde_json::from_slice(&export_bytes(
        "账号 admin 密码 secret",
        Uuid::new_v4(),
        "fixture",
    ))
    .unwrap();
    let mut messages = export.batches[0].messages.clone();
    messages[0].raw_payload = serde_json::json!({"password": "secret-456"});
    redact_sensitive_messages(&mut messages);

    assert_eq!(
        messages[0].body_text.as_deref(),
        Some("账号 admin 密码 [敏感数据已脱敏]")
    );
    assert!(!serde_json::to_string(&messages).unwrap().contains("secret"));
}

#[test]
fn streamed_redaction_keeps_previous_message_context_even_when_hidden() {
    use archive_export::{ExportSource, ExportStatistics};
    use archive_store::{ArchiveStore, MessageQuery};
    let directory = TestDirectory::new();
    let mut store = ArchiveStore::open(&directory.0.join("redaction.db")).unwrap();
    let export: ClientExportV1 =
        serde_json::from_slice(&export_bytes("账号密码如下", Uuid::new_v4(), "fixture")).unwrap();
    let mut batch = export.batches[0].clone();
    batch.messages[0].raw_type = "group_announcement".into();
    let mut next = batch.messages[0].clone();
    next.stable_message_id = "credentials".into();
    next.source_message_id = "credentials".into();
    next.raw_type = "text".into();
    next.sent_at += TimeDelta::seconds(7);
    next.body_text = Some("adminuser跟SafePass-123".into());
    next.raw_payload = json!({"content": "adminuser跟SafePass-123", "value": "adminuser"});
    batch.messages.push(next);
    store.ingest_batch(&batch).unwrap();
    let mut expected = batch.messages.clone();
    redact_sensitive_messages(&mut expected);
    assert!(
        !expected[1]
            .body_text
            .as_ref()
            .unwrap()
            .contains("adminuser")
    );
    for ascending in [false, true] {
        let source = ArchiveExportSource::new(
            &store,
            ArchiveExportOptions {
                query: MessageQuery {
                    sort_ascending: ascending,
                    ..Default::default()
                },
                simplify: true,
                data_redaction: true,
                ..Default::default()
            },
        )
        .unwrap();
        let ExportStatistics { message_count, .. } = source.statistics();
        assert_eq!(message_count, 1);
        let mut actual = Vec::new();
        source
            .visit_conversations(&mut |id| {
                source.visit_messages(id, &mut |message| {
                    actual.push(message.clone());
                    Ok(())
                })
            })
            .unwrap();
        assert_eq!(actual, vec![expected[1].clone()]);
    }
}

fn device_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

#[tokio::test]
async fn generated_collector_heartbeat_update_and_revoke_flow() {
    let directory = TestDirectory::new();
    let template = directory.0.join("collector-template.exe");
    std::fs::write(&template, b"collector").unwrap();
    let mut app_state = state(&directory);
    app_state.collector_template = Some(Arc::new(template));
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        config.organization_name = "测试组织".into();
        config.upload_url = "http://127.0.0.1:9812".into();
        config.collector_schedule = CollectionSchedule {
            mode: CollectionScheduleMode::Interval,
            interval_minutes: 30,
            daily_time: "02:00".into(),
        };
    }

    let generated = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap()
        .0;
    let enrollment: serde_json::Value = serde_json::from_str(&generated.artifact).unwrap();
    let collector_id = enrollment["collectorId"].as_str().unwrap().to_owned();
    let device_token = enrollment["uploadToken"].as_str().unwrap().to_owned();
    assert_eq!(enrollment["configRevision"], 1);
    let before_connection = collection_target_response(
        &app_state.enterprise_config.lock().unwrap(),
        &directory.0,
        Local::now(),
    );
    assert_eq!(before_connection.targets.len(), 1);

    let signature = decode_hex(enrollment["signatureHex"].as_str().unwrap()).unwrap();
    let signing_public = decode_hex(
        &app_state
            .enterprise_config
            .lock()
            .unwrap()
            .signing_public_key_hex,
    )
    .unwrap();
    let record = app_state
        .enterprise_config
        .lock()
        .unwrap()
        .collectors
        .iter()
        .find(|collector| collector.collector_id == collector_id)
        .unwrap()
        .clone();
    let desired = collector_desired_config(&app_state.enterprise_config.lock().unwrap(), &record);
    let signed_payload = serde_json::to_vec(&json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": enrollment["configRevision"],
        "expiresAt": enrollment["configExpiresAt"],
        "config": desired,
    }))
    .unwrap();
    enterprise_crypto::verify(&signing_public, &signed_payload, &signature).unwrap();

    let heartbeat = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42310))),
        device_headers(&device_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("1.2.4".into()),
            current_config_revision: Some(0),
            last_applied_revision: Some(0),
            last_run_at: Some("2026-09-23T01:00:00Z".into()),
            last_success_at: Some("2026-09-23T01:01:00Z".into()),
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(heartbeat.status, "ok");
    assert_eq!(heartbeat.control.unwrap().config_revision, 1);
    let after_connection = collection_target_response(
        &app_state.enterprise_config.lock().unwrap(),
        &directory.0,
        Local::now(),
    );
    assert_eq!(after_connection.targets.len(), 2);
    assert_eq!(
        after_connection.targets[1].plans[0].last_run_at.as_deref(),
        Some("2026-09-23T01:00:00Z")
    );
    assert_eq!(
        after_connection.targets[1].plans[0]
            .last_upload_at
            .as_deref(),
        Some("2026-09-23T01:01:00Z")
    );
    assert_eq!(
        after_connection.targets[1].client_ip.as_deref(),
        Some("192.0.2.10")
    );

    let update = update_collector(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        authorized_headers(),
        Json(CollectorUpdateRequest {
            display_name: "已停用采集端".into(),
            enabled: false,
            include_media: true,
            data_redaction: true,
            offline_export_enabled: true,
            hidden_mode_enabled: false,
            collector_log_level: default_collector_log_level(),
            collection_notice: "测试告知".into(),
            schedule: CollectionSchedule {
                mode: CollectionScheduleMode::Daily,
                interval_minutes: 60,
                daily_time: "03:30".into(),
            },
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(update["config"]["configRevision"], 2);
    assert_eq!(update["config"]["enabled"], false);

    let disabled_heartbeat = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42311))),
        device_headers(&device_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("1.2.4".into()),
            current_config_revision: Some(1),
            last_applied_revision: Some(1),
            last_run_at: None,
            last_success_at: None,
            last_error: Some("test_error".into()),
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(disabled_heartbeat.status, "disabled");
    assert_eq!(disabled_heartbeat.control.unwrap().config_revision, 2);
    assert!(
        authorize_enterprise_upload(
            &device_headers(&device_token),
            &app_state.token_sha256,
            &app_state.enterprise_config,
        )
        .is_err()
    );

    let reenabled = update_collector(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        authorized_headers(),
        Json(CollectorUpdateRequest {
            display_name: "重新启用采集端".into(),
            enabled: true,
            include_media: false,
            data_redaction: true,
            offline_export_enabled: false,
            hidden_mode_enabled: false,
            collector_log_level: default_collector_log_level(),
            collection_notice: "测试告知".into(),
            schedule: CollectionSchedule {
                mode: CollectionScheduleMode::Interval,
                interval_minutes: 15,
                daily_time: "03:30".into(),
            },
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(reenabled["config"]["configRevision"], 3);
    assert!(
        authorize_enterprise_upload(
            &device_headers(&device_token),
            &app_state.token_sha256,
            &app_state.enterprise_config,
        )
        .is_ok()
    );

    let queued = request_collector_collection(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        authorized_headers(),
    )
    .await
    .unwrap()
    .0;
    let request_id = queued["requestId"].as_str().unwrap().to_owned();
    assert_eq!(queued["queued"], true);
    let requested = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42312))),
        device_headers(&device_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("0.0.5".into()),
            current_config_revision: Some(3),
            last_applied_revision: Some(3),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        requested
            .control
            .unwrap()
            .config
            .manual_collection_request_id,
        Some(request_id.clone())
    );
    let completed = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42313))),
        device_headers(&device_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("0.0.5".into()),
            current_config_revision: Some(4),
            last_applied_revision: Some(4),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: Some(request_id),
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        completed
            .control
            .unwrap()
            .config
            .manual_collection_request_id,
        None
    );

    let _ = revoke_collector(
        State(app_state.clone()),
        AxumPath(collector_id.clone()),
        authorized_headers(),
    )
    .await
    .unwrap();
    let revoked = collector_heartbeat(
        State(app_state),
        AxumPath(collector_id),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42314))),
        device_headers(&device_token),
        Json(CollectorHeartbeatRequest {
            client_version: None,
            current_config_revision: Some(3),
            last_applied_revision: Some(3),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(revoked.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn new_collector_replaces_online_collector_on_same_device() {
    let directory = TestDirectory::new();
    let template = directory.0.join("collector-template.exe");
    std::fs::write(&template, b"collector").unwrap();
    let mut app_state = state(&directory);
    app_state.collector_template = Some(Arc::new(template));
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        config.organization_name = "测试组织".into();
        config.upload_url = "http://127.0.0.1:9812".into();
    }

    let first = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap()
        .0;
    let first_artifact: serde_json::Value = serde_json::from_str(&first.artifact).unwrap();
    let first_id = first.collector_id.clone();
    let first_token = first_artifact["uploadToken"].as_str().unwrap().to_owned();
    let first_path = directory.0.join("collectors").join(&first.file_name);

    let _ = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(first_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42310))),
        device_headers(&first_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("1.2.4".into()),
            current_config_revision: Some(0),
            last_applied_revision: Some(0),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap();
    assert!(first_path.is_file());

    // Restarting the same collector keeps its registration and executable.
    let _ = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(first_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42311))),
        device_headers(&first_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("1.2.4".into()),
            current_config_revision: Some(1),
            last_applied_revision: Some(1),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap();
    assert!(first_path.is_file());

    let second = generate_collector(State(app_state.clone()), authorized_headers())
        .await
        .unwrap()
        .0;
    let second_artifact: serde_json::Value = serde_json::from_str(&second.artifact).unwrap();
    let second_id = second.collector_id.clone();
    let second_token = second_artifact["uploadToken"].as_str().unwrap().to_owned();
    let replaced = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(second_id.clone()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 42312))),
        device_headers(&second_token),
        Json(CollectorHeartbeatRequest {
            client_version: Some("1.2.4".into()),
            current_config_revision: Some(1),
            last_applied_revision: Some(0),
            last_run_at: None,
            last_success_at: None,
            last_error: None,
            last_manual_collection_request_id: None,
            diagnostics: Vec::new(),
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(replaced.status, "ok");

    let config = app_state.enterprise_config.lock().unwrap();
    assert!(
        !config
            .collectors
            .iter()
            .any(|collector| collector.collector_id == first_id)
    );
    assert!(
        config
            .collectors
            .iter()
            .any(|collector| collector.collector_id == second_id)
    );
    assert!(!first_path.exists());
}
