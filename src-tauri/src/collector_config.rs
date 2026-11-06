use crate::paths::{collector_runtime_config_path, legacy_collector_runtime_config_path};
use crate::state::{CommandError, internal_error};
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EnterpriseCollectorConfig {
    pub(super) schema_version: String,
    pub(super) organization_id: String,
    pub(super) organization_name: String,
    pub(super) collection_notice: String,
    pub(super) key_id: String,
    pub(super) public_key_hex: String,
    pub(super) signing_public_key_hex: String,
    pub(super) signature_hex: String,
    pub(super) upload_url: String,
    pub(super) upload_token: String,
    #[serde(default)]
    pub(super) collector_id: String,
    #[serde(default)]
    pub(super) display_name: String,
    #[serde(default)]
    pub(super) config_revision: u64,
    #[serde(default)]
    pub(super) config_expires_at: Option<String>,
    #[serde(default = "default_collector_enabled")]
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) include_files: bool,
    #[serde(default)]
    pub(super) include_images: bool,
    #[serde(default = "default_data_redaction")]
    pub(super) data_redaction: bool,
    #[serde(default)]
    pub(super) offline_export_enabled: bool,
    #[serde(default)]
    pub(super) hidden_mode_enabled: bool,
    #[serde(default = "default_collector_log_level")]
    pub(super) collector_log_level: String,
    #[serde(default)]
    pub(super) manual_collection_request_id: Option<String>,
    #[serde(default)]
    pub(super) collector_schedule: CollectionSchedule,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum CollectionScheduleMode {
    #[default]
    Disabled,
    Interval,
    Daily,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectionSchedule {
    #[serde(default)]
    pub(super) mode: CollectionScheduleMode,
    #[serde(default = "default_schedule_interval_minutes")]
    pub(super) interval_minutes: u32,
    #[serde(default = "default_schedule_daily_time")]
    pub(super) daily_time: String,
}

pub(super) fn default_schedule_interval_minutes() -> u32 {
    60
}

pub(super) fn default_data_redaction() -> bool {
    true
}

pub(super) fn default_collector_log_level() -> String {
    "off".into()
}

pub(super) fn default_schedule_daily_time() -> String {
    "02:00".into()
}

pub(super) fn default_collector_enabled() -> bool {
    true
}

pub(super) fn collector_control_payload(
    config: &EnterpriseCollectorConfig,
    expires_at: &str,
) -> Result<Vec<u8>, CommandError> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": config.config_revision,
        "expiresAt": expires_at,
        "config": {
            "collectorId": config.collector_id,
            "displayName": config.display_name,
            "enabled": config.enabled,
            "configRevision": config.config_revision,
            "organizationId": config.organization_id,
            "organizationName": config.organization_name,
            "collectionNotice": config.collection_notice,
            "uploadUrl": config.upload_url,
            "keyId": config.key_id,
            "publicKeyHex": config.public_key_hex,
            "includeMedia": config.include_files || config.include_images,
            "dataRedaction": config.data_redaction,
            "offlineExportEnabled": config.offline_export_enabled,
            "hiddenModeEnabled": config.hidden_mode_enabled,
            "collectorLogLevel": config.collector_log_level,
            "manualCollectionRequestId": config.manual_collection_request_id,
            "schedule": config.collector_schedule,
        },
    }))
    .map_err(|_| internal_error())
}

pub(super) fn load_enterprise_collector_config() -> Result<EnterpriseCollectorConfig, CommandError>
{
    let executable = std::env::current_exe().map_err(|_| internal_error())?;
    let embedded_bytes =
        archive_transfer::read_enterprise_collector_config(&executable).map_err(|_| {
            CommandError {
                code: "ENTERPRISE_CONFIG_MISSING",
                message: "采集端内未找到企业配置。".into(),
                recoverable: false,
            }
        })?;
    let embedded: EnterpriseCollectorConfig =
        serde_json::from_slice(&embedded_bytes).map_err(|_| CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置无效。".into(),
            recoverable: false,
        })?;
    let runtime = collector_runtime_config_path(&embedded.collector_id)
        .ok()
        .and_then(|path| source_windows::dpapi::load_current_user(&path).ok())
        .or_else(|| {
            legacy_collector_runtime_config_path()
                .ok()
                .and_then(|path| source_windows::dpapi::load_current_user(&path).ok())
        })
        .and_then(|bytes| serde_json::from_slice::<EnterpriseCollectorConfig>(&bytes).ok())
        .filter(|config| config.collector_id == embedded.collector_id);
    let config = runtime.unwrap_or(embedded);
    let key = hex::decode(&config.public_key_hex).map_err(|_| CommandError {
        code: "ENTERPRISE_CONFIG_INVALID",
        message: "企业采集配置中的加密密钥无效。".into(),
        recoverable: false,
    })?;
    let signing_public_key =
        hex::decode(&config.signing_public_key_hex).map_err(|_| CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置中的签名公钥无效。".into(),
            recoverable: false,
        })?;
    let signature = hex::decode(&config.signature_hex).map_err(|_| CommandError {
        code: "ENTERPRISE_CONFIG_INVALID",
        message: "企业采集配置中的签名无效。".into(),
        recoverable: false,
    })?;
    let signature_valid = if let Some(expires_at) = config.config_expires_at.as_deref() {
        collector_control_payload(&config, expires_at)
            .ok()
            .is_some_and(|payload| {
                source_windows::enterprise_crypto::verify(&signing_public_key, &payload, &signature)
                    .is_ok()
            })
    } else {
        let signing_payload = collector_signing_payload(&config);
        source_windows::enterprise_crypto::verify(
            &signing_public_key,
            signing_payload.as_bytes(),
            &signature,
        )
        .is_ok()
            || verify_legacy_collector_signature(&config, &signing_public_key, &signature, true)
            || (!config.offline_export_enabled
                && verify_legacy_collector_signature(
                    &config,
                    &signing_public_key,
                    &signature,
                    false,
                ))
    };
    if !signature_valid {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_SIGNATURE_INVALID",
            message: "企业采集配置签名校验失败。".into(),
            recoverable: false,
        });
    }
    if config.schema_version != "enterprise-collector.v1"
        || config.organization_id.trim().is_empty()
        || config.organization_name.trim().is_empty()
        || config.collection_notice.trim().is_empty()
        || config.key_id.trim().is_empty()
        || config.upload_url.trim().is_empty()
        || config.upload_token.trim().is_empty()
        || key.len() < 64
    {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_INVALID",
            message: "企业采集配置不完整或不允许离线运行。".into(),
            recoverable: false,
        });
    }
    Ok(config)
}

pub(super) fn ensure_collector_config_fresh(
    config: &EnterpriseCollectorConfig,
) -> Result<(), CommandError> {
    if config
        .config_expires_at
        .as_deref()
        .is_some_and(|expires_at| {
            chrono::DateTime::parse_from_rfc3339(expires_at)
                .ok()
                .is_none_or(|value| value.with_timezone(&Utc) <= Utc::now())
        })
    {
        return Err(CommandError {
            code: "ENTERPRISE_CONFIG_EXPIRED",
            message: "企业采集配置已过期，请等待服务端同步。".into(),
            recoverable: true,
        });
    }
    Ok(())
}

#[tauri::command]
pub(super) fn verify_collector_control_envelope(
    control: &serde_json::Value,
    signing_public_key_hex: &str,
    current_revision: u64,
) -> Result<(), CommandError> {
    if control
        .get("schemaVersion")
        .and_then(serde_json::Value::as_str)
        != Some("collector-control.v1")
    {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置版本无效。".into(),
            recoverable: true,
        });
    }
    let revision = control
        .get("configRevision")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置修订号无效。".into(),
            recoverable: true,
        })?;
    if revision < current_revision {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_ROLLBACK",
            message: "服务端返回了较旧的采集配置，已拒绝应用。".into(),
            recoverable: true,
        });
    }
    let expires_at = control
        .get("expiresAt")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置缺少有效期。".into(),
            recoverable: true,
        })?;
    if chrono::DateTime::parse_from_rfc3339(expires_at)
        .ok()
        .is_none_or(|value| value.with_timezone(&Utc) <= Utc::now())
    {
        return Err(CommandError {
            code: "COLLECTOR_CONFIG_EXPIRED",
            message: "服务端控制配置已过期。".into(),
            recoverable: true,
        });
    }
    let signature = control
        .get("signatureHex")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CommandError {
            code: "COLLECTOR_CONFIG_INVALID",
            message: "服务端控制配置缺少签名。".into(),
            recoverable: true,
        })?;
    let payload = serde_json::json!({
        "schemaVersion": "collector-control.v1",
        "configRevision": revision,
        "expiresAt": expires_at,
        "config": control.get("config").ok_or_else(internal_error)?,
    });
    let public_key = hex::decode(signing_public_key_hex).map_err(|_| CommandError {
        code: "COLLECTOR_CONFIG_INVALID",
        message: "服务端签名公钥无效。".into(),
        recoverable: true,
    })?;
    let signature = hex::decode(signature).map_err(|_| CommandError {
        code: "COLLECTOR_CONFIG_INVALID",
        message: "服务端控制配置签名无效。".into(),
        recoverable: true,
    })?;
    let bytes = serde_json::to_vec(&payload).map_err(|_| internal_error())?;
    source_windows::enterprise_crypto::verify(&public_key, &bytes, &signature).map_err(|_| {
        CommandError {
            code: "COLLECTOR_CONFIG_SIGNATURE_INVALID",
            message: "服务端控制配置签名校验失败。".into(),
            recoverable: true,
        }
    })
}

pub(super) fn collector_signing_payload(config: &EnterpriseCollectorConfig) -> String {
    format!(
        "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        config.organization_id,
        config.organization_name,
        config.collection_notice,
        config.key_id,
        config.public_key_hex,
        "aes-256-gcm+rsa-oaep-sha256",
        config.upload_url,
        config.upload_token,
        config.include_files,
        config.include_images,
        config.data_redaction,
        config.offline_export_enabled,
        config.hidden_mode_enabled,
        match config.collector_schedule.mode {
            CollectionScheduleMode::Disabled => "disabled",
            CollectionScheduleMode::Interval => "interval",
            CollectionScheduleMode::Daily => "daily",
        },
        config.collector_schedule.interval_minutes,
        config.collector_schedule.daily_time,
    )
}

pub(super) fn verify_legacy_collector_signature(
    config: &EnterpriseCollectorConfig,
    signing_public_key: &[u8],
    signature: &[u8],
    include_offline_export: bool,
) -> bool {
    let payload = if include_offline_export {
        format!(
            "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            config.organization_id,
            config.organization_name,
            config.collection_notice,
            config.key_id,
            config.public_key_hex,
            "aes-256-gcm+rsa-oaep-sha256",
            config.upload_url,
            config.upload_token,
            config.include_files,
            config.include_images,
            config.data_redaction,
            config.offline_export_enabled,
        )
    } else {
        format!(
            "enterprise-collector.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            config.organization_id,
            config.organization_name,
            config.collection_notice,
            config.key_id,
            config.public_key_hex,
            "aes-256-gcm+rsa-oaep-sha256",
            config.upload_url,
            config.upload_token,
            config.include_files,
            config.include_images,
            config.data_redaction,
        )
    };
    source_windows::enterprise_crypto::verify(signing_public_key, payload.as_bytes(), signature)
        .is_ok()
}
