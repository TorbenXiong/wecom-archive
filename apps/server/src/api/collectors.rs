use crate::{
    ApiError, AppState, CollectionSchedulesResponse, CollectionTargetsResponse,
    CollectorHeartbeatRequest, CollectorHeartbeatResponse, CollectorListResponse,
    CollectorResponse, CollectorUpdateRequest, EnterpriseCollectorRecord, EnterpriseConfig,
    EnterpriseConfigRequest, EnterpriseConfigResponse, LocalCollectionPlan,
    LocalCollectionPlanRequest, MAX_ENTERPRISE_KEY_ID_LEN, MIN_ENTERPRISE_KEY_ID_LEN,
    OpenDirectoryResponse, SuperAdminRequest, SuperAdminResponse, authorize,
    collection_schedules_response, collection_target_response, collector_desired_config,
    collector_plan_responses, collector_status, diagnostics, encode_hex,
    enterprise_config_response, normalize_log_level, normalize_upload_url,
    persist_enterprise_config, preserve_current_key_if_used, random_access_token,
    sanitize_file_component, signed_collector_control, upload_endpoint,
};
use archive_transfer::append_enterprise_collector_config;
use axum::Json;
use axum::extract::ConnectInfo;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::Local;
use chrono::Utc;
use sha2::Digest;
use sha2::Sha256;
use source_windows::enterprise_crypto;
use source_windows::secrets as dpapi_protect;
use std::net::SocketAddr;
use std::path::PathBuf;
use uuid::Uuid;

pub(crate) async fn get_enterprise_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    Ok(Json(enterprise_config_response(&config)))
}

pub(crate) async fn get_collection_schedules(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

pub(crate) async fn update_super_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SuperAdminRequest>,
) -> Result<Json<SuperAdminResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    config.super_admin_enabled = request.enabled;
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(SuperAdminResponse {
        super_admin_enabled: config.super_admin_enabled,
    }))
}

pub(crate) async fn create_local_collection_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LocalCollectionPlanRequest>,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    request.schedule.validate()?;
    let name = validate_plan_name(&request.name)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let now = Utc::now().to_rfc3339();
    config.local_collection_plans.push(LocalCollectionPlan {
        id: Uuid::new_v4().to_string(),
        name,
        include_media: request.include_media,
        schedule: request.schedule,
        created_at: now.clone(),
        updated_at: now,
        last_run_at: None,
        last_status: None,
        last_detail: None,
    });
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

pub(crate) async fn update_local_collection_plan(
    State(state): State<AppState>,
    AxumPath(plan_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<LocalCollectionPlanRequest>,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    request.schedule.validate()?;
    let name = validate_plan_name(&request.name)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let plan = config
        .local_collection_plans
        .iter_mut()
        .find(|plan| plan.id == plan_id)
        .ok_or_else(ApiError::invalid_query)?;
    plan.name = name;
    plan.include_media = request.include_media;
    plan.schedule = request.schedule;
    plan.updated_at = Utc::now().to_rfc3339();
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

pub(crate) async fn delete_local_collection_plan(
    State(state): State<AppState>,
    AxumPath(plan_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<CollectionSchedulesResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let previous_len = config.local_collection_plans.len();
    config
        .local_collection_plans
        .retain(|plan| plan.id != plan_id);
    if config.local_collection_plans.len() == previous_len {
        return Err(ApiError::invalid_query());
    }
    config.local_collection_cursors.remove(&plan_id);
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(collection_schedules_response(
        &config,
        &state.data_root,
    )))
}

pub(crate) fn validate_plan_name(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 80 {
        return Err(ApiError::invalid_query());
    }
    Ok(value.to_owned())
}

pub(crate) async fn update_enterprise_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<EnterpriseConfigRequest>,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    if request.organization_name.trim().is_empty() {
        return Err(ApiError::invalid_query());
    }
    let upload_url = normalize_upload_url(&request.upload_url)?;
    if let Some(schedule) = &request.server_schedule {
        schedule.validate()?;
    }
    if let Some(schedule) = &request.collector_schedule {
        schedule.validate()?;
    }
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    if let Some(key_id) = request
        .key_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        if !(MIN_ENTERPRISE_KEY_ID_LEN..=MAX_ENTERPRISE_KEY_ID_LEN).contains(&key_id.len()) {
            return Err(ApiError::invalid_query());
        }
        if key_id != config.key_id {
            preserve_current_key_if_used(&mut config);
            config.key_id = key_id;
            let (public_key, private_key) =
                enterprise_crypto::generate_rsa_key_pair().map_err(|_| ApiError::store())?;
            config.public_key_hex = encode_hex(&public_key);
            config.private_key_protected_hex =
                encode_hex(&dpapi_protect::protect(&private_key).map_err(|_| ApiError::store())?);
        }
    }
    config.organization_name = request.organization_name.trim().to_owned();
    if !request.collection_notice.trim().is_empty() {
        config.collection_notice = request.collection_notice.trim().to_owned();
    }
    config.upload_url = upload_url;
    config.include_files = request.include_media;
    config.include_images = request.include_media;
    config.data_redaction = request.data_redaction;
    config.offline_export_enabled = request.offline_export_enabled;
    config.hidden_mode_enabled = request.hidden_mode_enabled;
    config.server_log_level = normalize_log_level(&request.server_log_level);
    config.collector_log_level = normalize_log_level(&request.collector_log_level);
    diagnostics::set_level(&config.server_log_level);
    diagnostics::write(
        "enterprise_config_update",
        &format!(
            "hidden_mode={};include_media={}",
            config.hidden_mode_enabled, request.include_media
        ),
    );
    if let Some(enabled) = request.super_admin_enabled {
        config.super_admin_enabled = enabled;
    }
    if let Some(schedule) = request.server_schedule {
        config.server_schedule = schedule;
    }
    if let Some(schedule) = request.collector_schedule {
        config.collector_schedule = schedule;
    }
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(enterprise_config_response(&config)))
}

pub(crate) async fn rotate_enterprise_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EnterpriseConfigResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    preserve_current_key_if_used(&mut config);
    let (public_key, private_key) =
        enterprise_crypto::generate_rsa_key_pair().map_err(|_| ApiError::store())?;
    config.key_id = format!("key-{}", Uuid::new_v4().simple());
    config.public_key_hex = encode_hex(&public_key);
    config.private_key_protected_hex =
        encode_hex(&dpapi_protect::protect(&private_key).map_err(|_| ApiError::store())?);
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(enterprise_config_response(&config)))
}

pub(crate) async fn generate_collector(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectorResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    if config.organization_name.trim().is_empty()
        || config.public_key_hex.is_empty()
        || config.private_key_protected_hex.is_empty()
        || config.signing_public_key_hex.is_empty()
        || config.signing_private_key_protected_hex.is_empty()
    {
        return Err(ApiError::invalid_query());
    }
    let upload_url = upload_endpoint(&config.upload_url);
    let collector_id = Uuid::new_v4().to_string();
    diagnostics::write(
        "collector_generate_started",
        &format!(
            "hidden_mode={};schedule={:?}",
            config.hidden_mode_enabled, config.collector_schedule.mode
        ),
    );
    let upload_token = random_access_token();
    let upload_token_sha256 = encode_hex(&Sha256::digest(upload_token.as_bytes()));
    let key_id = config.key_id.clone();
    let now = Utc::now();
    let record = EnterpriseCollectorRecord {
        update_capable: false,
        software_update_request: None,
        software_update_version: None,
        software_update_error: None,
        collector_id: collector_id.clone(),
        key_id: key_id.clone(),
        created_at: now.to_rfc3339(),
        file_name: String::new(),
        organization_name: config.organization_name.clone(),
        collection_notice: config.collection_notice.clone(),
        upload_url: upload_url.clone(),
        include_media: config.include_files || config.include_images,
        data_redaction: config.data_redaction,
        offline_export_enabled: config.offline_export_enabled,
        hidden_mode_enabled: config.hidden_mode_enabled,
        collector_log_level: config.collector_log_level.clone(),
        schedule: config.collector_schedule.clone(),
        last_upload_at: None,
        last_run_at: None,
        public_key_hex: String::new(),
        private_key_protected_hex: String::new(),
        upload_token_sha256,
        display_name: format!("采集端 {}", &collector_id[..8]),
        protocol_version: "collector-control.v1".into(),
        client_version: None,
        registered_at: Some(now.to_rfc3339()),
        last_seen_at: None,
        last_client_ip: None,
        enabled: true,
        revoked_at: None,
        config_revision: 1,
        last_applied_revision: 0,
        last_error: None,
        manual_collection_request_id: None,
        last_manual_collection_request_id: None,
    };
    let desired = collector_desired_config(&config, &record);
    let control = signed_collector_control(&config, desired.clone())?;
    let artifact = serde_json::json!({
        "schemaVersion": "enterprise-collector.v1",
        "collectorId": collector_id.clone(),
        "configRevision": desired.config_revision,
        "configExpiresAt": control.expires_at,
        "displayName": desired.display_name,
        "organizationId": config.organization_id,
        "organizationName": config.organization_name,
        "collectionNotice": config.collection_notice,
        "keyId": key_id.clone(),
        "encryption": "aes-256-gcm+rsa-oaep-sha256",
        "publicKeyHex": config.public_key_hex,
        "signingPublicKeyHex": config.signing_public_key_hex,
        "signatureHex": control.signature_hex.clone(),
        "uploadUrl": upload_url.clone(),
        "uploadToken": upload_token.clone(),
        "includeFiles": config.include_files,
        "includeImages": config.include_images,
        "includeMedia": config.include_files || config.include_images,
        "dataRedaction": config.data_redaction,
        "offlineExportEnabled": config.offline_export_enabled,
        "hiddenModeEnabled": config.hidden_mode_enabled,
        "collectorLogLevel": config.collector_log_level,
        "enabled": desired.enabled,
        "manualCollectionRequestId": desired.manual_collection_request_id,
        "collectorSchedule": &config.collector_schedule,
        "formats": ["enterprise-package.v1"],
    });
    let artifact_text = serde_json::to_string_pretty(&artifact).map_err(|_| ApiError::store())?;
    let file_name = format!(
        "{}-{}.exe",
        sanitize_file_component(&key_id),
        &collector_id[..8]
    );
    let collector_root = state.data_root.join("collectors");
    std::fs::create_dir_all(&collector_root).map_err(|_| ApiError::store())?;
    let executable_path = collector_root.join(&file_name);
    let partial_executable = collector_root.join(format!(".{file_name}.{collector_id}.partial"));
    let executable_generated = state
        .collector_template
        .as_deref()
        .filter(|template| template.is_file())
        .cloned()
        .or_else(find_collector_template)
        .is_some_and(|template| {
            let generated = (|| -> Result<(), std::io::Error> {
                std::fs::copy(template, &partial_executable)?;
                append_enterprise_collector_config(&partial_executable, artifact_text.as_bytes())
                    .map_err(std::io::Error::other)?;
                if executable_path.exists() {
                    std::fs::remove_file(&executable_path)?;
                }
                std::fs::rename(&partial_executable, &executable_path)?;
                Ok(())
            })()
            .is_ok();
            if !generated {
                let _ = std::fs::remove_file(&partial_executable);
            }
            generated
        });
    if executable_generated {
        let mut record = record;
        record.file_name = file_name.clone();
        config.collectors.push(record);
        if let Err(error) = persist_enterprise_config(&state.data_root, &config) {
            config
                .collectors
                .retain(|collector| collector.collector_id != collector_id);
            let _ = std::fs::remove_file(&executable_path);
            return Err(error);
        }
        diagnostics::write("collector_generate_completed", "executable=true");
    } else {
        diagnostics::write("collector_generate_completed", "executable=false");
    }
    Ok(Json(CollectorResponse {
        file_name,
        directory: collector_root.display().to_string(),
        organization_id: config.organization_id.clone(),
        key_id,
        collector_id,
        artifact: artifact_text,
        executable_generated,
    }))
}

pub(crate) async fn list_collectors(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectorListResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    Ok(Json(CollectorListResponse {
        collectors: collector_plan_responses(
            &config,
            Some(&state.data_root.join("collectors")),
            Local::now(),
        ),
    }))
}

pub(crate) async fn get_collection_targets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollectionTargetsResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    Ok(Json(collection_target_response(
        &config,
        &state.data_root,
        Local::now(),
    )))
}

pub(crate) fn collector_by_device_token<'a>(
    config: &'a mut EnterpriseConfig,
    collector_id: &str,
    headers: &HeaderMap,
) -> Result<&'a mut EnterpriseCollectorRecord, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let token_hash = encode_hex(&Sha256::digest(token.as_bytes()));
    config
        .collectors
        .iter_mut()
        .find(|collector| {
            collector.collector_id == collector_id
                && collector.upload_token_sha256 == token_hash
                && collector.revoked_at.is_none()
        })
        .ok_or_else(ApiError::unauthorized)
}

pub(crate) async fn collector_heartbeat(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<CollectorHeartbeatRequest>,
) -> Result<Json<CollectorHeartbeatResponse>, ApiError> {
    let client_ip = remote_addr.ip().to_string();
    diagnostics::verbose(
        "collector_heartbeat",
        &format!(
            "revision={};client_revision={}",
            request.last_applied_revision.unwrap_or_default(),
            request.current_config_revision.unwrap_or_default()
        ),
    );
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    // Multiple devices can share one NAT or proxy address. Only the authenticated
    // collector identity may be updated; retiring other identities is explicit.
    let snapshot = {
        let collector = collector_by_device_token(&mut config, &collector_id, &headers)?;
        collector.last_seen_at = Some(Utc::now().to_rfc3339());
        collector.last_client_ip = Some(client_ip.clone());
        collector.client_version = request.client_version;
        collector.update_capable = request.update_capable;
        collector.software_update_error = request
            .software_update_error
            .filter(|value| value == "UPDATE_FAILED")
            .map(|_| "升级失败；原程序和绑定保留，可重试升级。".into());
        if collector.software_update_version.as_deref() == collector.client_version.as_deref() {
            collector.software_update_request = None;
            collector.software_update_error = None;
        }
        collector.last_applied_revision = request
            .last_applied_revision
            .unwrap_or(collector.last_applied_revision);
        collector.last_error = request.last_error;
        if let Some(last_run) = request.last_run_at {
            collector.last_run_at = Some(last_run);
        }
        if let Some(last_success) = request.last_success_at {
            collector.last_upload_at = Some(last_success);
        }
        if let Some(completed_id) = request.last_manual_collection_request_id
            && collector.manual_collection_request_id.as_deref() == Some(completed_id.as_str())
        {
            collector.last_manual_collection_request_id = Some(completed_id);
            collector.manual_collection_request_id = None;
            collector.config_revision = collector.config_revision.saturating_add(1);
        }
        collector.clone()
    };
    let collector_diagnostics = request.diagnostics;
    for event in collector_diagnostics.into_iter().take(100) {
        let event_name = event.event.trim();
        if event_name.is_empty() {
            continue;
        }
        let details = format!(
            "timestamp={};display_name={};client_ip={};details={}",
            event.timestamp, snapshot.display_name, client_ip, event.details
        );
        diagnostics::collector(&snapshot.collector_id, event_name, &details);
    }
    persist_enterprise_config(&state.data_root, &config)?;
    let should_send = request.current_config_revision.unwrap_or(0) < snapshot.config_revision;
    let control = if should_send {
        Some(signed_collector_control(
            &config,
            collector_desired_config(&config, &snapshot),
        )?)
    } else {
        None
    };
    diagnostics::write(
        "collector_heartbeat_response",
        &format!(
            "should_send_control={};hidden_mode={};revision={}",
            should_send, snapshot.hidden_mode_enabled, snapshot.config_revision
        ),
    );
    Ok(Json(CollectorHeartbeatResponse {
        software_update: crate::signed_software_update(&config, &snapshot)?,
        status: if snapshot.enabled {
            "ok".into()
        } else {
            "disabled".into()
        },
        config: control.as_ref().map(|value| value.config.clone()),
        control,
    }))
}

pub(crate) async fn get_collector(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let collector = config
        .collectors
        .iter()
        .find(|item| item.collector_id == collector_id)
        .ok_or_else(ApiError::invalid_query)?;
    Ok(Json(serde_json::json!({
        "collector": collector_plan_responses(&config, Some(&state.data_root.join("collectors")), Local::now()).into_iter().find(|item| item.collector_id == collector_id),
        "status": collector_status(collector, Utc::now()),
        "config": collector_desired_config(&config, collector),
    })))
}

pub(crate) async fn update_collector(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<CollectorUpdateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let display_name = validate_plan_name(&request.display_name)?;
    request.schedule.validate()?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let collector = config
        .collectors
        .iter_mut()
        .find(|item| item.collector_id == collector_id)
        .ok_or_else(ApiError::invalid_query)?;
    collector.display_name = display_name;
    collector.enabled = request.enabled;
    collector.include_media = request.include_media;
    collector.data_redaction = request.data_redaction;
    collector.offline_export_enabled = request.offline_export_enabled;
    collector.hidden_mode_enabled = request.hidden_mode_enabled;
    collector.collector_log_level = normalize_log_level(&request.collector_log_level);
    if !request.collection_notice.trim().is_empty() {
        collector.collection_notice = request.collection_notice.trim().to_owned();
    }
    collector.schedule = request.schedule;
    collector.config_revision = collector.config_revision.saturating_add(1);
    persist_enterprise_config(&state.data_root, &config)?;
    let snapshot = config
        .collectors
        .iter()
        .find(|item| item.collector_id == collector_id)
        .ok_or_else(ApiError::invalid_query)?;
    Ok(Json(
        serde_json::json!({ "config": collector_desired_config(&config, snapshot), "status": "updated" }),
    ))
}

pub(crate) async fn revoke_collector(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let collector = config
        .collectors
        .iter_mut()
        .find(|item| item.collector_id == collector_id)
        .ok_or_else(ApiError::invalid_query)?;
    collector.enabled = false;
    collector.revoked_at = Some(Utc::now().to_rfc3339());
    collector.config_revision = collector.config_revision.saturating_add(1);
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

pub(crate) async fn request_collector_collection(
    State(state): State<AppState>,
    AxumPath(collector_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let mut config = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?;
    let collector = config
        .collectors
        .iter_mut()
        .find(|item| item.collector_id == collector_id)
        .ok_or_else(ApiError::invalid_query)?;
    if collector.revoked_at.is_some() || !collector.enabled {
        return Err(ApiError::invalid_query());
    }
    let request_id = Uuid::new_v4().to_string();
    collector.manual_collection_request_id = Some(request_id.clone());
    collector.config_revision = collector.config_revision.saturating_add(1);
    persist_enterprise_config(&state.data_root, &config)?;
    Ok(Json(
        serde_json::json!({ "queued": true, "requestId": request_id }),
    ))
}

pub(crate) async fn open_collector_directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OpenDirectoryResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let directory = state.data_root.join("collectors");
    if !directory.is_dir() {
        return Err(ApiError::collector_directory_missing());
    }
    #[cfg(windows)]
    {
        source_windows::desktop::open_directory(&directory).map_err(|_| ApiError::store())?;
        Ok(Json(OpenDirectoryResponse { opened: true }))
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(ApiError::invalid_query())
    }
}

pub(crate) fn find_collector_template() -> Option<PathBuf> {
    std::env::current_exe().ok().filter(|path| path.is_file())
}
