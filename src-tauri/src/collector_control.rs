use crate::collector_config::{
    ensure_collector_config_fresh, load_enterprise_collector_config,
    verify_collector_control_envelope,
};
use crate::collector_runtime::apply_collector_visibility;
use crate::paths::collector_runtime_config_path;
use crate::state::{AppState, CollectorScheduleStatus, CommandError, internal_error};
use crate::{collector_http, collector_update, diagnostics};
use serde::Serialize;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::Emitter;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectorDiagnosticEvent {
    pub(super) timestamp: u64,
    pub(super) event: String,
    pub(super) details: String,
}

pub(super) fn server_base_url(upload_url: &str) -> &str {
    upload_url
        .trim_end_matches('/')
        .strip_suffix("/api/v1/imports/enterprise")
        .unwrap_or(upload_url.trim_end_matches('/'))
}

pub(super) fn collector_heartbeat_failure(
    response: &collector_http::Response,
    status_state: Option<&Arc<Mutex<CollectorScheduleStatus>>>,
    app_state: Option<&AppState>,
) -> CommandError {
    let unauthorized = response.status == 401
        && serde_json::from_slice::<serde_json::Value>(&response.body)
            .ok()
            .is_some_and(|value| {
                value.get("code").and_then(serde_json::Value::as_str) == Some("UNAUTHORIZED")
            });
    if unauthorized {
        if let Some(state) = status_state
            && let Ok(mut status) = state.lock()
        {
            status.server_authorization_denied = true;
            status.last_error = Some("采集端授权已失效，后台采集已停止。".into());
        }
        if let Some(state) = app_state {
            state.scheduler_shutdown.store(true, Ordering::Release);
            if let Ok(Some(app)) = state.tray_app.lock().map(|handle| handle.clone()) {
                app.exit(0);
            }
        }
    }
    CommandError {
        code: if unauthorized {
            "COLLECTOR_AUTHORIZATION_DENIED"
        } else {
            "COLLECTOR_HEARTBEAT_FAILED"
        },
        message: if unauthorized {
            "采集端授权已失效，后台采集已停止。"
        } else {
            "服务端暂时不可用，将稍后重试连接。"
        }
        .into(),
        recoverable: !unauthorized,
    }
}

pub(super) fn sync_collector_config_with_status(
    status_state: Option<&Arc<Mutex<CollectorScheduleStatus>>>,
    app_state: Option<&AppState>,
) -> Result<(), CommandError> {
    let current = load_enterprise_collector_config()?;
    diagnostics::write(
        "heartbeat_started",
        &format!(
            "revision={};hidden_mode={}",
            current.config_revision, current.hidden_mode_enabled
        ),
    );
    if current.collector_id.is_empty() {
        return Ok(());
    }
    let endpoint = format!(
        "{}/api/v1/collectors/{}/heartbeat",
        server_base_url(&current.upload_url),
        current.collector_id
    );
    let status = status_state.and_then(|state| state.lock().ok().map(|value| value.clone()));
    let current_revision = if ensure_collector_config_fresh(&current).is_ok() {
        current.config_revision
    } else {
        0
    };
    let pending_diagnostics = diagnostics::take_events();
    let body = serde_json::to_vec(&serde_json::json!({ "updateCapable": true, "softwareUpdateError": app_state.and_then(|state| state.software_update_error.lock().ok().and_then(|value| value.clone())), "clientVersion": env!("CARGO_PKG_VERSION"), "currentConfigRevision": current_revision, "lastAppliedRevision": current.config_revision, "lastRunAt": status.as_ref().and_then(|value| value.last_attempt_at.clone()), "lastSuccessAt": status.as_ref().and_then(|value| value.last_success_at.clone()), "lastError": status.as_ref().and_then(|value| value.last_error.clone()), "lastManualCollectionRequestId": status.as_ref().and_then(|value| value.last_manual_collection_request_id.clone()), "diagnostics": pending_diagnostics.iter().map(|event| CollectorDiagnosticEvent { timestamp: event.timestamp, event: event.event.clone(), details: event.details.clone() }).collect::<Vec<_>>() })).map_err(|_| internal_error())?;
    let response = match collector_http::post_json(
        &endpoint,
        &current.upload_token,
        &body,
        "application/json",
        10,
        30,
    ) {
        Ok(response) => response,
        Err(_) => {
            diagnostics::restore_events(pending_diagnostics);
            diagnostics::write("heartbeat_failed", "transport_failed");
            return Err(CommandError {
                code: "COLLECTOR_HEARTBEAT_FAILED",
                message: "无法连接服务端，将稍后重试。".into(),
                recoverable: true,
            });
        }
    };
    if !response.is_success() {
        diagnostics::restore_events(pending_diagnostics);
        diagnostics::write("heartbeat_failed", "http_request_failed");
        return Err(collector_heartbeat_failure(
            &response,
            status_state,
            app_state,
        ));
    }
    if let Some(state) = status_state
        && let Ok(mut status) = state.lock()
    {
        status.server_authorization_denied = false;
    }
    let response: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| internal_error())?;
    if let (Some(value), Some(state)) = (response.get("softwareUpdate"), app_state) {
        collector_update::receive(value, &current, state);
    }
    let Some(control) = response.get("control").filter(|value| !value.is_null()) else {
        diagnostics::write("heartbeat_ok", "control=none");
        return Ok(());
    };
    let control_hidden_mode = control
        .get("config")
        .and_then(|value| value.get("hiddenModeEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    diagnostics::write(
        "heartbeat_control",
        &format!("hidden_mode={control_hidden_mode}"),
    );
    verify_collector_control_envelope(
        control,
        &current.signing_public_key_hex,
        current.config_revision,
    )?;
    let config = control.get("config").ok_or_else(internal_error)?;
    let mut artifact = serde_json::to_value(&current).map_err(|_| internal_error())?;
    let object = artifact.as_object_mut().ok_or_else(internal_error)?;
    for (from, to) in [
        ("displayName", "displayName"),
        ("organizationName", "organizationName"),
        ("collectionNotice", "collectionNotice"),
        ("uploadUrl", "uploadUrl"),
        ("keyId", "keyId"),
        ("publicKeyHex", "publicKeyHex"),
        ("dataRedaction", "dataRedaction"),
        ("offlineExportEnabled", "offlineExportEnabled"),
        ("hiddenModeEnabled", "hiddenModeEnabled"),
        ("collectorLogLevel", "collectorLogLevel"),
        ("manualCollectionRequestId", "manualCollectionRequestId"),
        ("schedule", "collectorSchedule"),
        ("configRevision", "configRevision"),
    ] {
        if let Some(value) = config.get(from) {
            object.insert(to.into(), value.clone());
        }
    }
    if let Some(include_media) = config.get("includeMedia") {
        object.insert("includeFiles".into(), include_media.clone());
        object.insert("includeImages".into(), include_media.clone());
    }
    if let Some(value) = control.get("expiresAt") {
        object.insert("configExpiresAt".into(), value.clone());
    }
    if let Some(value) = config.get("displayName") {
        object.insert("displayName".into(), value.clone());
    }
    if let Some(value) = control.get("signatureHex") {
        object.insert("signatureHex".into(), value.clone());
    }
    if let Some(enabled) = config.get("enabled").and_then(|value| value.as_bool()) {
        object.insert("enabled".into(), enabled.into());
    }
    let path = collector_runtime_config_path(&current.collector_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| internal_error())?;
    }
    source_windows::dpapi::store_current_user(
        &path,
        &serde_json::to_vec_pretty(&artifact).map_err(|_| internal_error())?,
    )
    .map_err(|_| internal_error())?;
    if let Some(app_state) = app_state
        && let Ok(Some(app)) = app_state.tray_app.lock().map(|handle| handle.clone())
    {
        let latest = load_enterprise_collector_config()?;
        diagnostics::set_level(&latest.collector_log_level);
        diagnostics::write(
            "apply_visibility",
            &format!("hidden_mode={}", latest.hidden_mode_enabled),
        );
        apply_collector_visibility(&app, &latest).map_err(|_| internal_error())?;
        let _ = app.emit("collector:schedule-updated", &latest.collector_schedule);
    }
    Ok(())
}
