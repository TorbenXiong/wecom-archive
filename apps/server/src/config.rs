use crate::{
    ApiError, DEFAULT_COLLECTION_NOTICE, DEFAULT_SERVER_URL, ENTERPRISE_CONFIG_FILE,
    EnterpriseConfigResponse, LEGACY_COLLECTION_NOTICE, LocalCollectionPlan, encode_hex,
};
use chrono::Local;
use chrono::NaiveTime;
use chrono::TimeDelta;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use source_windows::enterprise_crypto;
use source_windows::secrets as dpapi_protect;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EnterpriseConfig {
    pub(super) organization_id: String,
    pub(super) organization_name: String,
    pub(super) collection_notice: String,
    pub(super) upload_url: String,
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
    #[serde(default = "default_log_level")]
    pub(super) server_log_level: String,
    #[serde(default = "default_collector_log_level")]
    pub(super) collector_log_level: String,
    #[serde(default = "default_super_admin_enabled")]
    pub(super) super_admin_enabled: bool,
    #[serde(default)]
    pub(super) local_collection_plans: Vec<LocalCollectionPlan>,
    #[serde(default)]
    pub(super) local_collection_cursors: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing)]
    pub(super) server_include_media: bool,
    #[serde(default)]
    #[serde(skip_serializing)]
    pub(super) server_schedule: CollectionSchedule,
    #[serde(default)]
    pub(super) collector_schedule: CollectionSchedule,
    pub(super) key_id: String,
    pub(super) public_key_hex: String,
    pub(super) private_key_protected_hex: String,
    pub(super) signing_public_key_hex: String,
    pub(super) signing_private_key_protected_hex: String,
    #[serde(default)]
    pub(super) key_history: Vec<EnterpriseKeyRecord>,
    #[serde(default)]
    pub(super) collectors: Vec<EnterpriseCollectorRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CollectionSchedule {
    #[serde(default)]
    pub(super) mode: CollectionScheduleMode,
    #[serde(default = "default_schedule_interval_minutes")]
    pub(super) interval_minutes: u32,
    #[serde(default = "default_schedule_daily_time")]
    pub(super) daily_time: String,
}

impl Default for CollectionSchedule {
    fn default() -> Self {
        Self {
            mode: CollectionScheduleMode::Disabled,
            interval_minutes: default_schedule_interval_minutes(),
            daily_time: default_schedule_daily_time(),
        }
    }
}

impl CollectionSchedule {
    pub(super) fn validate(&self) -> Result<(), ApiError> {
        if !(1..=10_080).contains(&self.interval_minutes)
            || NaiveTime::parse_from_str(&self.daily_time, "%H:%M").is_err()
        {
            return Err(ApiError::invalid_query());
        }
        Ok(())
    }

    pub(super) fn next_after(
        &self,
        now: chrono::DateTime<Local>,
    ) -> Option<chrono::DateTime<Local>> {
        match self.mode {
            CollectionScheduleMode::Disabled => None,
            CollectionScheduleMode::Interval => {
                Some(now + TimeDelta::minutes(i64::from(self.interval_minutes)))
            }
            CollectionScheduleMode::Daily => {
                let time = NaiveTime::parse_from_str(&self.daily_time, "%H:%M").ok()?;
                let today = now.date_naive().and_time(time);
                let candidate = today.and_local_timezone(Local).earliest()?;
                if candidate > now {
                    Some(candidate)
                } else {
                    (today + TimeDelta::days(1))
                        .and_local_timezone(Local)
                        .earliest()
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CollectionScheduleMode {
    #[default]
    Disabled,
    Interval,
    Daily,
}

pub(super) fn default_schedule_interval_minutes() -> u32 {
    60
}

pub(super) fn default_super_admin_enabled() -> bool {
    true
}

pub(super) fn default_data_redaction() -> bool {
    true
}

pub(super) fn default_log_level() -> String {
    "normal".into()
}

pub(super) fn default_collector_log_level() -> String {
    "off".into()
}

pub(super) fn normalize_log_level(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "verbose" => value.trim().to_ascii_lowercase(),
        _ => "normal".into(),
    }
}

pub(super) fn default_schedule_daily_time() -> String {
    "02:00".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct EnterpriseKeyRecord {
    pub(super) key_id: String,
    pub(super) public_key_hex: String,
    pub(super) private_key_protected_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct EnterpriseCollectorRecord {
    #[serde(default)]
    pub(super) update_capable: bool,
    #[serde(default)]
    pub(super) software_update_request: Option<String>,
    #[serde(default)]
    pub(super) software_update_version: Option<String>,
    #[serde(default)]
    pub(super) software_update_error: Option<String>,
    pub(super) collector_id: String,
    pub(super) key_id: String,
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) file_name: String,
    #[serde(default)]
    pub(super) organization_name: String,
    #[serde(default)]
    pub(super) collection_notice: String,
    #[serde(default)]
    pub(super) upload_url: String,
    #[serde(default)]
    pub(super) include_media: bool,
    #[serde(default = "default_data_redaction")]
    pub(super) data_redaction: bool,
    #[serde(default)]
    pub(super) offline_export_enabled: bool,
    #[serde(default)]
    pub(super) hidden_mode_enabled: bool,
    #[serde(default = "default_collector_log_level")]
    pub(super) collector_log_level: String,
    #[serde(default)]
    pub(super) schedule: CollectionSchedule,
    #[serde(default)]
    pub(super) last_upload_at: Option<String>,
    #[serde(default)]
    pub(super) last_run_at: Option<String>,
    pub(super) public_key_hex: String,
    pub(super) private_key_protected_hex: String,
    pub(super) upload_token_sha256: String,
    #[serde(default)]
    pub(super) display_name: String,
    #[serde(default = "default_collector_protocol_version")]
    pub(super) protocol_version: String,
    #[serde(default)]
    pub(super) client_version: Option<String>,
    #[serde(default)]
    pub(super) registered_at: Option<String>,
    #[serde(default)]
    pub(super) last_seen_at: Option<String>,
    #[serde(default)]
    pub(super) last_client_ip: Option<String>,
    #[serde(default = "default_collector_enabled")]
    pub(super) enabled: bool,
    #[serde(default)]
    pub(super) revoked_at: Option<String>,
    #[serde(default)]
    pub(super) config_revision: u64,
    #[serde(default)]
    pub(super) last_applied_revision: u64,
    #[serde(default)]
    pub(super) last_error: Option<String>,
    #[serde(default)]
    pub(super) manual_collection_request_id: Option<String>,
    #[serde(default)]
    pub(super) last_manual_collection_request_id: Option<String>,
}

pub(super) fn default_collector_protocol_version() -> String {
    "collector-control.v1".into()
}
pub(super) fn default_collector_enabled() -> bool {
    true
}

pub(super) fn load_enterprise_config(root: &Path) -> EnterpriseConfig {
    let path = root.join(ENTERPRISE_CONFIG_FILE);
    let Some(mut config) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    else {
        return new_enterprise_config();
    };
    let mut migrated = discard_unused_historical_keys(&mut config);
    if config.collection_notice == LEGACY_COLLECTION_NOTICE {
        config.collection_notice = DEFAULT_COLLECTION_NOTICE.into();
        migrated = true;
    }
    if config.local_collection_plans.is_empty()
        && config.server_schedule.mode != CollectionScheduleMode::Disabled
    {
        let now = Utc::now().to_rfc3339();
        config.local_collection_plans.push(LocalCollectionPlan {
            id: Uuid::new_v4().to_string(),
            name: "本机采集计划".into(),
            include_media: config.server_include_media,
            schedule: config.server_schedule.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_run_at: None,
            last_status: None,
            last_detail: None,
        });
        config.server_schedule = CollectionSchedule::default();
        config.server_include_media = false;
        migrated = true;
    }
    if migrated {
        let _ = persist_enterprise_config(root, &config);
    }
    config
}

pub(super) fn new_enterprise_config() -> EnterpriseConfig {
    let (
        public_key_hex,
        private_key_protected_hex,
        signing_public_key_hex,
        signing_private_key_protected_hex,
    ) = enterprise_crypto::generate_rsa_key_pair()
        .and_then(|(public_key, private_key)| {
            dpapi_protect::protect(&private_key).and_then(|protected| {
                enterprise_crypto::generate_rsa_key_pair().and_then(
                    |(signing_public, signing_private)| {
                        dpapi_protect::protect(&signing_private).map(|signing_protected| {
                            (
                                encode_hex(&public_key),
                                encode_hex(&protected),
                                encode_hex(&signing_public),
                                encode_hex(&signing_protected),
                            )
                        })
                    },
                )
            })
        })
        .unwrap_or_default();
    EnterpriseConfig {
        organization_id: Uuid::new_v4().to_string(),
        organization_name: String::new(),
        collection_notice: DEFAULT_COLLECTION_NOTICE.into(),
        upload_url: default_upload_url(),
        include_files: false,
        include_images: false,
        data_redaction: true,
        offline_export_enabled: false,
        hidden_mode_enabled: false,
        server_log_level: default_log_level(),
        collector_log_level: default_collector_log_level(),
        super_admin_enabled: default_super_admin_enabled(),
        local_collection_plans: Vec::new(),
        local_collection_cursors: std::collections::BTreeMap::new(),
        server_include_media: false,
        server_schedule: CollectionSchedule::default(),
        collector_schedule: CollectionSchedule::default(),
        key_id: format!("key-{}", Uuid::new_v4().simple()),
        public_key_hex,
        private_key_protected_hex,
        signing_public_key_hex,
        signing_private_key_protected_hex,
        key_history: Vec::new(),
        collectors: Vec::new(),
    }
}

pub(super) fn enterprise_config_response(config: &EnterpriseConfig) -> EnterpriseConfigResponse {
    EnterpriseConfigResponse {
        configured: !config.organization_name.trim().is_empty(),
        organization_id: config.organization_id.clone(),
        organization_name: config.organization_name.clone(),
        collection_notice: config.collection_notice.clone(),
        upload_url: config.upload_url.clone(),
        key_id: config.key_id.clone(),
        include_media: config.include_files || config.include_images,
        data_redaction: config.data_redaction,
        offline_export_enabled: config.offline_export_enabled,
        hidden_mode_enabled: config.hidden_mode_enabled,
        server_log_level: config.server_log_level.clone(),
        collector_log_level: config.collector_log_level.clone(),
        super_admin_enabled: config.super_admin_enabled,
        server_schedule: config.server_schedule.clone(),
        collector_schedule: config.collector_schedule.clone(),
    }
}

pub(super) fn default_upload_url() -> String {
    DEFAULT_SERVER_URL.into()
}

pub(super) fn normalize_upload_url(value: &str) -> Result<String, ApiError> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty()
        || value.len() > 2048
        || value.chars().any(char::is_whitespace)
        || !(value.starts_with("http://") || value.starts_with("https://"))
    {
        return Err(ApiError::invalid_query());
    }
    Ok(value.to_owned())
}

/// Derive a listener from a saved collector URL only when it explicitly points
/// at a private network address. Public addresses and host names must never
/// cause the desktop service to bind broadly without an explicit override.
pub(super) fn configured_private_listen_address(upload_url: &str) -> Option<SocketAddr> {
    let trimmed = upload_url.trim();
    let authority = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))?;
    let authority = authority.split(['/', '?', '#']).next()?.trim();
    let authority = authority.rsplit('@').next()?.trim();
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = suffix.strip_prefix(':')?.parse::<u16>().ok()?;
        (host, port)
    } else {
        let (host, port) = authority.rsplit_once(':')?;
        if host.is_empty() || host.contains(':') {
            return None;
        }
        (host, port.parse::<u16>().ok()?)
    };
    let ip = host.parse::<IpAddr>().ok()?;
    let private = match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    };
    if !private || port == 0 {
        return None;
    }
    Some(SocketAddr::new(ip, port))
}

pub(super) fn upload_endpoint(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/api/v1/imports/enterprise") {
        base_url.to_owned()
    } else {
        format!("{base_url}/api/v1/imports/enterprise")
    }
}

pub(super) fn persist_enterprise_config(
    root: &Path,
    config: &EnterpriseConfig,
) -> Result<(), ApiError> {
    let path = root.join(ENTERPRISE_CONFIG_FILE);
    let partial = path.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(config).map_err(|_| ApiError::store())?;
    std::fs::write(&partial, bytes).map_err(|_| ApiError::store())?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| ApiError::store())?;
    }
    std::fs::rename(&partial, &path).map_err(|_| ApiError::store())
}

pub(super) fn discard_unused_historical_keys(config: &mut EnterpriseConfig) -> bool {
    let used_key_ids: Vec<String> = config
        .collectors
        .iter()
        .map(|collector| collector.key_id.clone())
        .collect();
    let previous_count = config.key_history.len();
    config
        .key_history
        .retain(|record| used_key_ids.contains(&record.key_id));
    config.key_history.len() != previous_count
}

pub(super) fn preserve_current_key_if_used(config: &mut EnterpriseConfig) {
    let key_was_used = config
        .collectors
        .iter()
        .any(|collector| collector.key_id == config.key_id);
    let key_is_preserved = config
        .key_history
        .iter()
        .any(|record| record.key_id == config.key_id);
    if key_was_used && !key_is_preserved && !config.private_key_protected_hex.is_empty() {
        config.key_history.push(EnterpriseKeyRecord {
            key_id: config.key_id.clone(),
            public_key_hex: config.public_key_hex.clone(),
            private_key_protected_hex: config.private_key_protected_hex.clone(),
        });
    }
}

pub(super) fn load_or_generate_access_token(path: &Path) -> Result<Zeroizing<String>, String> {
    if path.is_file() {
        let protected = std::fs::read(path).map_err(|_| "access token file is unavailable")?;
        let token = dpapi_protect::unprotect(&protected)?;
        let token = String::from_utf8(token).map_err(|_| "access token file is invalid")?;
        if token.len() < 24 {
            return Err("access token is too short".into());
        }
        return Ok(Zeroizing::new(token));
    }
    let token = random_access_token();
    persist_access_token(path, token.as_bytes())?;
    Ok(Zeroizing::new(token))
}

pub(super) fn random_access_token() -> String {
    let bytes = enterprise_crypto::random_bytes::<32>().unwrap_or_else(|_| {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(first.as_bytes());
        bytes[16..].copy_from_slice(second.as_bytes());
        bytes
    });
    encode_hex(&bytes)
}

pub(super) fn persist_access_token(path: &Path, token: &[u8]) -> Result<(), String> {
    if token.len() < 24 {
        return Err("access token is too short".into());
    }
    let protected = dpapi_protect::protect(token)?;
    let partial = path.with_extension("dpapi.partial");
    std::fs::write(&partial, protected).map_err(|_| "access token write failed")?;
    if path.exists() {
        std::fs::remove_file(path).map_err(|_| "access token replacement failed")?;
    }
    std::fs::rename(partial, path).map_err(|_| "access token publish failed".to_owned())
}

pub(super) fn default_data_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("serverData")
}
