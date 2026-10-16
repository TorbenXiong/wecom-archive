use crate::{
    ApiError, CollectionPlanResponse, CollectionSchedule, CollectionScheduleMode,
    CollectionSchedulesResponse, CollectionTargetResponse, CollectionTargetsResponse,
    CollectorControlEnvelopeResponse, CollectorDesiredConfigResponse, CollectorPlanResponse,
    EnterpriseCollectorRecord, EnterpriseConfig, LocalCollectionPlanResponse, decode_hex,
    encode_hex,
};
use chrono::Local;
use chrono::TimeDelta;
use chrono::Utc;
use source_windows::enterprise_crypto;
use source_windows::secrets as dpapi_protect;
use std::path::Path;

pub(crate) fn collection_schedules_response(
    config: &EnterpriseConfig,
    data_root: &Path,
) -> CollectionSchedulesResponse {
    let now = Local::now();
    let local_plans = config
        .local_collection_plans
        .iter()
        .cloned()
        .map(|plan| LocalCollectionPlanResponse {
            next_run_at: estimated_next_run(
                &plan.schedule,
                plan.last_run_at
                    .as_deref()
                    .or(Some(plan.created_at.as_str())),
                now,
            ),
            plan,
        })
        .collect();
    CollectionSchedulesResponse {
        local_plans,
        collector_plans: collector_plan_responses(config, Some(&data_root.join("collectors")), now),
    }
}

pub(crate) fn estimated_next_run(
    schedule: &CollectionSchedule,
    reference: Option<&str>,
    now: chrono::DateTime<Local>,
) -> Option<String> {
    let next = match schedule.mode {
        CollectionScheduleMode::Disabled => None,
        CollectionScheduleMode::Daily => schedule.next_after(now),
        CollectionScheduleMode::Interval => {
            let from_reference = reference
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| {
                    value.with_timezone(&Local)
                        + TimeDelta::minutes(i64::from(schedule.interval_minutes))
                });
            Some(
                from_reference
                    .filter(|value| *value > now)
                    .unwrap_or_else(|| {
                        now + TimeDelta::minutes(i64::from(schedule.interval_minutes))
                    }),
            )
        }
    }?;
    Some(next.to_rfc3339())
}

pub(crate) fn collector_plan_responses(
    config: &EnterpriseConfig,
    collector_root: Option<&Path>,
    now: chrono::DateTime<Local>,
) -> Vec<CollectorPlanResponse> {
    let mut plans = config
        .collectors
        .iter()
        .map(|collector| CollectorPlanResponse {
            collector_id: collector.collector_id.clone(),
            file_name: collector.file_name.clone(),
            organization_name: collector.organization_name.clone(),
            upload_url: collector.upload_url.clone(),
            include_media: collector.include_media,
            data_redaction: collector.data_redaction,
            offline_export_enabled: collector.offline_export_enabled,
            hidden_mode_enabled: collector.hidden_mode_enabled,
            collector_log_level: collector.collector_log_level.clone(),
            schedule: collector.schedule.clone(),
            created_at: collector.created_at.clone(),
            last_upload_at: collector.last_upload_at.clone(),
            last_run_at: collector.last_run_at.clone(),
            next_run_at: estimated_next_run(
                &collector.schedule,
                collector
                    .last_upload_at
                    .as_deref()
                    .or(Some(collector.created_at.as_str())),
                now,
            ),
            executable_available: collector_root
                .filter(|_| !collector.file_name.is_empty())
                .is_some_and(|root| root.join(&collector.file_name).is_file()),
        })
        .collect::<Vec<_>>();
    plans.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    plans
}

pub(crate) fn collector_status(
    collector: &EnterpriseCollectorRecord,
    now: chrono::DateTime<Utc>,
) -> String {
    if collector.revoked_at.is_some() {
        return "revoked".into();
    }
    if !collector.enabled {
        return "disabled".into();
    }
    let online = collector
        .last_seen_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| now.signed_duration_since(value.with_timezone(&Utc)) <= TimeDelta::minutes(5))
        .unwrap_or(false);
    if online {
        "online".into()
    } else {
        "offline".into()
    }
}

pub(crate) fn collector_display_name(collector: &EnterpriseCollectorRecord) -> String {
    let name = collector.display_name.trim();
    let generated_default = name.is_empty() || name.starts_with("采集端 ");
    if generated_default {
        collector
            .last_client_ip
            .clone()
            .unwrap_or_else(|| "采集端".into())
    } else {
        name.into()
    }
}

pub(crate) fn collector_has_connected(collector: &EnterpriseCollectorRecord) -> bool {
    collector.last_client_ip.is_some()
        || collector.client_version.is_some()
        || collector.last_applied_revision > 0
        || collector.last_run_at.is_some()
        || collector.last_upload_at.is_some()
}

pub(crate) fn collector_desired_config(
    config: &EnterpriseConfig,
    collector: &EnterpriseCollectorRecord,
) -> CollectorDesiredConfigResponse {
    CollectorDesiredConfigResponse {
        collector_id: collector.collector_id.clone(),
        display_name: collector_display_name(collector),
        enabled: collector.enabled && collector.revoked_at.is_none(),
        config_revision: collector.config_revision,
        organization_id: config.organization_id.clone(),
        organization_name: collector.organization_name.clone(),
        collection_notice: collector.collection_notice.clone(),
        upload_url: collector.upload_url.clone(),
        key_id: collector.key_id.clone(),
        public_key_hex: config.public_key_hex.clone(),
        include_media: collector.include_media,
        data_redaction: collector.data_redaction,
        offline_export_enabled: collector.offline_export_enabled,
        hidden_mode_enabled: collector.hidden_mode_enabled,
        collector_log_level: collector.collector_log_level.clone(),
        schedule: collector.schedule.clone(),
        manual_collection_request_id: collector.manual_collection_request_id.clone(),
    }
}

pub(crate) fn collector_control_payload(
    config: &CollectorDesiredConfigResponse,
    config_revision: u64,
    expires_at: &str,
) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": config_revision,
        "expiresAt": expires_at,
        "config": config,
    }))
    .map_err(|_| ApiError::store())
}

pub(crate) fn signed_collector_control(
    config: &EnterpriseConfig,
    desired: CollectorDesiredConfigResponse,
) -> Result<CollectorControlEnvelopeResponse, ApiError> {
    let expires_at = (Utc::now() + TimeDelta::minutes(15)).to_rfc3339();
    let payload = collector_control_payload(&desired, desired.config_revision, &expires_at)?;
    let signing_private = dpapi_protect::unprotect(
        &decode_hex(&config.signing_private_key_protected_hex).map_err(|_| ApiError::store())?,
    )
    .map_err(|_| ApiError::store())?;
    let signature =
        enterprise_crypto::sign(&signing_private, &payload).map_err(|_| ApiError::store())?;
    Ok(CollectorControlEnvelopeResponse {
        schema_version: "collector-control.v1",
        config_revision: desired.config_revision,
        expires_at,
        config: desired,
        signature_hex: encode_hex(&signature),
    })
}

pub(crate) fn collection_target_response(
    config: &EnterpriseConfig,
    data_root: &Path,
    now: chrono::DateTime<Local>,
) -> CollectionTargetsResponse {
    let server_plans = config
        .local_collection_plans
        .iter()
        .map(|plan| CollectionPlanResponse {
            id: plan.id.clone(),
            name: plan.name.clone(),
            include_media: plan.include_media,
            schedule: plan.schedule.clone(),
            created_at: plan.created_at.clone(),
            updated_at: plan.updated_at.clone(),
            last_run_at: plan.last_run_at.clone(),
            last_upload_at: None,
            last_status: plan.last_status.clone(),
            last_detail: plan.last_detail.clone(),
            next_run_at: estimated_next_run(
                &plan.schedule,
                plan.last_run_at
                    .as_deref()
                    .or(Some(plan.created_at.as_str())),
                now,
            ),
        })
        .collect();
    let mut targets = vec![CollectionTargetResponse {
        update_capable: false,
        update_available: false,
        update_version: None,
        update_pending: false,
        update_error: None,
        target_id: "server".into(),
        kind: "server".into(),
        display_name: if config.organization_name.is_empty() {
            "服务端".into()
        } else {
            config.organization_name.clone()
        },
        status: "online".into(),
        last_seen_at: None,
        client_ip: None,
        client_version: None,
        config_revision: None,
        last_applied_revision: None,
        last_error: None,
        include_media: Some(config.include_files || config.include_images),
        data_redaction: Some(config.data_redaction),
        offline_export_enabled: Some(config.offline_export_enabled),
        hidden_mode_enabled: Some(config.hidden_mode_enabled),
        collector_log_level: None,
        log_directory: None,
        plans: server_plans,
    }];
    for collector in config
        .collectors
        .iter()
        .filter(|collector| collector_has_connected(collector))
    {
        let plan = CollectionPlanResponse {
            id: collector.collector_id.clone(),
            name: if collector.display_name.is_empty() {
                collector.file_name.clone()
            } else {
                collector.display_name.clone()
            },
            include_media: collector.include_media,
            schedule: collector.schedule.clone(),
            created_at: collector.created_at.clone(),
            updated_at: collector
                .last_seen_at
                .clone()
                .unwrap_or_else(|| collector.created_at.clone()),
            last_run_at: collector.last_run_at.clone(),
            last_upload_at: collector.last_upload_at.clone(),
            last_status: collector
                .last_error
                .as_ref()
                .map(|_| "error".into())
                .or_else(|| Some("success".into())),
            last_detail: collector.last_error.clone(),
            next_run_at: estimated_next_run(
                &collector.schedule,
                collector
                    .last_upload_at
                    .as_deref()
                    .or(Some(collector.created_at.as_str())),
                now,
            ),
        };
        targets.push(CollectionTargetResponse {
            update_capable: collector.update_capable,
            update_available: collector.update_capable
                && collector.client_version.as_deref().is_some_and(|current| {
                    archive_transfer::software_update::newer(current, env!("CARGO_PKG_VERSION"))
                }),
            update_version: Some(env!("CARGO_PKG_VERSION").into()),
            update_pending: collector.software_update_request.is_some(),
            update_error: collector.software_update_error.clone(),
            target_id: collector.collector_id.clone(),
            kind: "collector".into(),
            display_name: collector_display_name(collector),
            status: collector_status(collector, Utc::now()),
            last_seen_at: collector.last_seen_at.clone(),
            client_ip: collector.last_client_ip.clone(),
            client_version: collector.client_version.clone(),
            config_revision: Some(collector.config_revision),
            last_applied_revision: Some(collector.last_applied_revision),
            last_error: collector.last_error.clone(),
            include_media: Some(collector.include_media),
            data_redaction: Some(collector.data_redaction),
            offline_export_enabled: Some(collector.offline_export_enabled),
            hidden_mode_enabled: Some(collector.hidden_mode_enabled),
            collector_log_level: Some(collector.collector_log_level.clone()),
            log_directory: Some(format!("collector/{}", collector.collector_id)),
            plans: vec![plan],
        });
    }
    let _ = data_root;
    CollectionTargetsResponse { targets }
}
