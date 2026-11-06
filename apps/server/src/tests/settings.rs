use super::*;

const DEVICE_TOKEN: &str = "synthetic-device-token-for-settings-tests";
const PLAN_ID: &str = "synthetic-plan";
const COLLECTOR_ID: &str = "synthetic-collector";

fn configured_state(directory: &TestDirectory) -> AppState {
    let mut app_state = state(directory);
    let template = directory.0.join("synthetic-template.exe");
    std::fs::write(&template, b"synthetic executable fixture").unwrap();
    app_state.collector_template = Some(Arc::new(template));
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        config.organization_name = "synthetic organization".into();
        config.local_collection_plans.push(LocalCollectionPlan {
            id: PLAN_ID.into(),
            name: "synthetic plan".into(),
            include_media: false,
            schedule: CollectionSchedule::default(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_run_at: None,
            last_status: None,
            last_detail: None,
        });
        config
            .local_collection_cursors
            .insert(PLAN_ID.into(), "2026-01-01T00:00:00Z".into());
        let collector = serde_json::from_value(json!({
            "collector_id": COLLECTOR_ID,
            "key_id": config.key_id,
            "created_at": "2026-01-01T00:00:00Z",
            "public_key_hex": config.public_key_hex,
            "private_key_protected_hex": config.private_key_protected_hex,
            "upload_token_sha256": encode_hex(&Sha256::digest(DEVICE_TOKEN.as_bytes())),
            "display_name": "synthetic collector",
            "config_revision": 3,
            "update_capable": true,
            "client_version": "0.0.1",
            "manual_collection_request_id": "synthetic-manual-request"
        }))
        .unwrap();
        config.collectors.push(collector);
        persist_enterprise_config(&directory.0, &config).unwrap();
    }
    app_state
}

fn config_bytes(state: &AppState) -> Vec<u8> {
    serde_json::to_vec(&*state.enterprise_config.lock().unwrap()).unwrap()
}

fn plan_request() -> LocalCollectionPlanRequest {
    LocalCollectionPlanRequest {
        name: "synthetic changed plan".into(),
        include_media: true,
        schedule: CollectionSchedule::default(),
    }
}

fn heartbeat_request() -> CollectorHeartbeatRequest {
    serde_json::from_value(json!({
        "updateCapable": true,
        "clientVersion": "0.0.1",
        "currentConfigRevision": 0,
        "lastAppliedRevision": 2,
        "lastRunAt": "2026-01-02T00:00:00Z",
        "lastSuccessAt": "2026-01-02T00:01:00Z",
        "lastManualCollectionRequestId": "synthetic-manual-request"
    }))
    .unwrap()
}

#[cfg(windows)]
fn lock_config(directory: &TestDirectory) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(directory.0.join(ENTERPRISE_CONFIG_FILE))
        .unwrap()
}

#[cfg(windows)]
#[tokio::test]
async fn failed_config_operations_preserve_all_settings_keys_and_control_state() {
    let directory = TestDirectory::new();
    let app_state = configured_state(&directory);
    let before = config_bytes(&app_state);
    let path = directory.0.join(ENTERPRISE_CONFIG_FILE);
    let disk_before = std::fs::read(&path).unwrap();
    let locked = lock_config(&directory);
    for operation in [
        "create-plan",
        "update-plan",
        "delete-plan",
        "enterprise",
        "rotate-key",
        "generate-collector",
        "update-collector",
        "revoke-collector",
        "collect-now",
        "software-update",
        "heartbeat",
    ] {
        let result = match operation {
            "create-plan" => create_local_collection_plan(State(app_state.clone()), authorized_headers(), Json(plan_request())).await.map(|_| ()),
            "update-plan" => update_local_collection_plan(State(app_state.clone()), AxumPath(PLAN_ID.into()), authorized_headers(), Json(plan_request())).await.map(|_| ()),
            "delete-plan" => delete_local_collection_plan(State(app_state.clone()), AxumPath(PLAN_ID.into()), authorized_headers()).await.map(|_| ()),
            "enterprise" => update_enterprise_config(State(app_state.clone()), authorized_headers(), Json(serde_json::from_value(json!({
                "organizationName": "synthetic changed organization", "uploadUrl": "http://127.0.0.1:19812", "keyId": "key-synthetic-replacement-configuration", "serverLogLevel": "off", "superAdminEnabled": false
            })).unwrap())).await.map(|_| ()),
            "rotate-key" => rotate_enterprise_key(State(app_state.clone()), authorized_headers()).await.map(|_| ()),
            "generate-collector" => generate_collector(State(app_state.clone()), authorized_headers()).await.map(|_| ()),
            "update-collector" => update_collector(State(app_state.clone()), AxumPath(COLLECTOR_ID.into()), authorized_headers(), Json(serde_json::from_value(json!({
                "displayName": "synthetic changed collector", "enabled": false, "includeMedia": true, "dataRedaction": false, "offlineExportEnabled": true, "hiddenModeEnabled": true, "schedule": CollectionSchedule::default()
            })).unwrap())).await.map(|_| ()),
            "revoke-collector" => revoke_collector(State(app_state.clone()), AxumPath(COLLECTOR_ID.into()), authorized_headers()).await.map(|_| ()),
            "collect-now" => request_collector_collection(State(app_state.clone()), AxumPath(COLLECTOR_ID.into()), authorized_headers()).await.map(|_| ()),
            "software-update" => request_collector_update(State(app_state.clone()), AxumPath(COLLECTOR_ID.into()), authorized_headers()).await.map(|_| ()),
            "heartbeat" => collector_heartbeat(State(app_state.clone()), AxumPath(COLLECTOR_ID.into()), ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 12345))), device_headers(DEVICE_TOKEN), Json(heartbeat_request())).await.map(|_| ()),
            _ => unreachable!(),
        };
        assert_eq!(
            result.unwrap_err().status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{operation}"
        );
        assert!(
            config_bytes(&app_state) == before,
            "failed {operation} changed active configuration"
        );
        assert!(
            std::fs::read(&path).unwrap() == disk_before,
            "failed {operation} changed the configuration file"
        );
    }
    assert_eq!(
        std::fs::read_dir(directory.0.join("collectors"))
            .unwrap()
            .count(),
        0
    );
    drop(locked);
    let response = request_collector_collection(
        State(app_state.clone()),
        AxumPath(COLLECTOR_ID.into()),
        authorized_headers(),
    )
    .await
    .unwrap();
    assert_eq!(response["queued"], true);
    let saved = load_enterprise_config(&directory.0).unwrap();
    assert!(serde_json::to_vec(&saved).unwrap() == config_bytes(&app_state));
}

#[cfg(windows)]
#[tokio::test]
async fn failed_heartbeat_signing_preserves_runtime_and_disk_state() {
    let directory = TestDirectory::new();
    let app_state = configured_state(&directory);
    {
        let mut config = app_state.enterprise_config.lock().unwrap();
        config.signing_private_key_protected_hex = "00".into();
        persist_enterprise_config(&directory.0, &config).unwrap();
    }
    let before = config_bytes(&app_state);
    let path = directory.0.join(ENTERPRISE_CONFIG_FILE);
    let disk_before = std::fs::read(&path).unwrap();
    let error = collector_heartbeat(
        State(app_state.clone()),
        AxumPath(COLLECTOR_ID.into()),
        ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 12345))),
        device_headers(DEVICE_TOKEN),
        Json(heartbeat_request()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(config_bytes(&app_state) == before);
    assert!(std::fs::read(&path).unwrap() == disk_before);
}

#[cfg(windows)]
#[tokio::test]
async fn failed_plan_result_save_keeps_the_durable_cursor_and_allows_retry() {
    let directory = TestDirectory::new();
    let app_state = configured_state(&directory);
    let before = config_bytes(&app_state);
    let started = Utc::now();
    let export = serde_json::from_slice(&export_bytes(
        "synthetic plan result",
        Uuid::new_v4(),
        "settings-plan",
    ))
    .unwrap();
    let success = Ok(ingest_export(&app_state, export).await.unwrap());
    let locked = lock_config(&directory);
    update_local_plan_result(&app_state, PLAN_ID, started, &success);
    assert!(config_bytes(&app_state) == before);
    drop(locked);
    update_local_plan_result(&app_state, PLAN_ID, started, &success);
    let saved = load_enterprise_config(&directory.0).unwrap();
    assert_eq!(
        saved.local_collection_cursors[PLAN_ID],
        started.to_rfc3339()
    );
    assert_eq!(
        saved.local_collection_plans[0].last_status.as_deref(),
        Some("success")
    );
    assert!(serde_json::to_vec(&saved).unwrap() == config_bytes(&app_state));
}

#[cfg(windows)]
#[tokio::test]
async fn failed_upload_status_save_does_not_publish_an_unpersisted_timestamp() {
    let directory = TestDirectory::new();
    let app_state = configured_state(&directory);
    let before = config_bytes(&app_state);
    let bytes = export_bytes(
        "synthetic settings upload",
        Uuid::new_v4(),
        "settings-upload",
    );
    let export: ClientExportV1 = serde_json::from_slice(&bytes).unwrap();
    let config = app_state.enterprise_config.lock().unwrap().clone();
    let encrypted =
        enterprise_crypto::encrypt(&decode_hex(&config.public_key_hex).unwrap(), &bytes).unwrap();
    let package = archive_transfer::create_enterprise_package(
        &export,
        &config.organization_id,
        &config.key_id,
        &encrypted.wrapped_dek,
        &encrypted.nonce,
        &encrypted.ciphertext,
        &encrypted.tag,
    )
    .unwrap();
    let locked = lock_config(&directory);
    let imported = import_enterprise(
        State(app_state.clone()),
        device_headers(DEVICE_TOKEN),
        Body::from(package.clone()),
    )
    .await
    .unwrap();
    assert_eq!(imported.inserted, 1);
    assert!(config_bytes(&app_state) == before);
    drop(locked);
    let retried = import_enterprise(
        State(app_state.clone()),
        device_headers(DEVICE_TOKEN),
        Body::from(package),
    )
    .await
    .unwrap();
    assert_eq!(retried.inserted, 0);
    assert_eq!(retried.unchanged, 1);
    let saved = load_enterprise_config(&directory.0).unwrap();
    assert!(saved.collectors[0].last_upload_at.is_some());
    assert!(serde_json::to_vec(&saved).unwrap() == config_bytes(&app_state));
}

#[cfg(windows)]
#[tokio::test]
async fn failed_token_changes_preserve_the_active_and_persisted_token() {
    use std::os::windows::fs::OpenOptionsExt;
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    persist_access_token(&app_state.access_token_path, TEST_TOKEN.as_bytes()).unwrap();
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&*app_state.access_token_path)
        .unwrap();
    let set = update_access_token(
        State(app_state.clone()),
        authorized_headers(),
        Json(AccessTokenRequest {
            access_token: "synthetic-replacement-token-settings".into(),
        }),
    )
    .await
    .unwrap_err();
    let regenerated = regenerate_access_token(State(app_state.clone()), authorized_headers())
        .await
        .unwrap_err();
    assert_eq!(set.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(regenerated.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(authorize(&authorized_headers(), &app_state.token_sha256).is_ok());
    assert!(
        load_or_generate_access_token(&app_state.access_token_path)
            .unwrap()
            .as_str()
            == TEST_TOKEN
    );
    drop(locked);
}

#[test]
fn concurrent_token_changes_have_one_authorized_winner_and_matching_disk_state() {
    let directory = TestDirectory::new();
    let app_state = state(&directory);
    persist_access_token(&app_state.access_token_path, TEST_TOKEN.as_bytes()).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let results = std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|index| {
                let state = app_state.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    barrier.wait();
                    runtime.block_on(async {
                        if index % 2 == 0 {
                            regenerate_access_token(State(state), authorized_headers())
                                .await
                                .map(|response| response.access_token.clone())
                        } else {
                            let token = format!("synthetic-settings-replacement-token-{index}");
                            update_access_token(
                                State(state),
                                authorized_headers(),
                                Json(AccessTokenRequest {
                                    access_token: token.clone(),
                                }),
                            )
                            .await
                            .map(|_| token)
                        }
                    })
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.status == StatusCode::UNAUTHORIZED)
    );
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert!(
        load_or_generate_access_token(&app_state.access_token_path)
            .unwrap()
            .as_str()
            == winner
    );
    assert!(
        *app_state.token_sha256.lock().unwrap()
            == <[u8; 32]>::from(Sha256::digest(winner.as_bytes()))
    );
    assert!(authorize(&device_headers(&winner), &app_state.token_sha256).is_ok());
    assert!(authorize(&authorized_headers(), &app_state.token_sha256).is_err());
}

#[test]
fn concurrent_plan_creation_preserves_every_successful_update() {
    let directory = TestDirectory::new();
    let app_state = configured_state(&directory);
    let barrier = Arc::new(std::sync::Barrier::new(6));
    std::thread::scope(|scope| {
        let workers = (0..6)
            .map(|index| {
                let state = app_state.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    barrier.wait();
                    runtime
                        .block_on(create_local_collection_plan(
                            State(state),
                            authorized_headers(),
                            Json(LocalCollectionPlanRequest {
                                name: format!("synthetic concurrent plan {index}"),
                                ..plan_request()
                            }),
                        ))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.join().unwrap();
        }
    });
    let saved = load_enterprise_config(&directory.0).unwrap();
    assert_eq!(saved.local_collection_plans.len(), 7);
    let names = saved
        .local_collection_plans
        .iter()
        .map(|plan| &plan.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), 7);
    assert!(serde_json::to_vec(&saved).unwrap() == config_bytes(&app_state));
}
