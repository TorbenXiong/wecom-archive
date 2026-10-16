use crate::{ApiError, CollectionSchedule, default_collector_log_level, default_log_level};
use archive_domain::MessageType;
use archive_export::ExportFormat;
use archive_export::ExportScope;
use chrono::NaiveDate;
use chrono::TimeDelta;
use chrono::TimeZone;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessTokenRequest {
    pub(crate) access_token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessTokenResponse {
    pub(crate) updated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegeneratedAccessTokenResponse {
    pub(crate) access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuperAdminRequest {
    pub(crate) enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuperAdminResponse {
    pub(crate) super_admin_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnterpriseConfigRequest {
    pub(crate) organization_name: String,
    #[serde(default)]
    pub(crate) collection_notice: String,
    pub(crate) upload_url: String,
    pub(crate) key_id: Option<String>,
    #[serde(default)]
    pub(crate) include_media: bool,
    #[serde(default)]
    pub(crate) data_redaction: bool,
    #[serde(default)]
    pub(crate) offline_export_enabled: bool,
    #[serde(default)]
    pub(crate) hidden_mode_enabled: bool,
    #[serde(default = "default_log_level")]
    pub(crate) server_log_level: String,
    #[serde(default = "default_collector_log_level")]
    pub(crate) collector_log_level: String,
    #[serde(default)]
    pub(crate) super_admin_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) server_schedule: Option<CollectionSchedule>,
    #[serde(default)]
    pub(crate) collector_schedule: Option<CollectionSchedule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnterpriseConfigResponse {
    pub(crate) configured: bool,
    pub(crate) organization_id: String,
    pub(crate) organization_name: String,
    pub(crate) collection_notice: String,
    pub(crate) upload_url: String,
    pub(crate) key_id: String,
    pub(crate) include_media: bool,
    pub(crate) data_redaction: bool,
    pub(crate) offline_export_enabled: bool,
    #[serde(default)]
    pub(crate) hidden_mode_enabled: bool,
    pub(crate) server_log_level: String,
    pub(crate) collector_log_level: String,
    pub(crate) super_admin_enabled: bool,
    pub(crate) server_schedule: CollectionSchedule,
    pub(crate) collector_schedule: CollectionSchedule,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalCollectionPlanRequest {
    pub(crate) name: String,
    pub(crate) schedule: CollectionSchedule,
    pub(crate) include_media: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalCollectionPlan {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) include_media: bool,
    pub(crate) schedule: CollectionSchedule,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) last_run_at: Option<String>,
    #[serde(default)]
    pub(crate) last_status: Option<String>,
    #[serde(default)]
    pub(crate) last_detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalCollectionPlanResponse {
    #[serde(flatten)]
    pub(crate) plan: LocalCollectionPlan,
    pub(crate) next_run_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionSchedulesResponse {
    pub(crate) local_plans: Vec<LocalCollectionPlanResponse>,
    pub(crate) collector_plans: Vec<CollectorPlanResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionTargetsResponse {
    pub(crate) targets: Vec<CollectionTargetResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionTargetResponse {
    pub(crate) update_capable: bool,
    pub(crate) update_available: bool,
    pub(crate) update_version: Option<String>,
    pub(crate) update_pending: bool,
    pub(crate) update_error: Option<String>,
    pub(crate) target_id: String,
    pub(crate) kind: String,
    pub(crate) display_name: String,
    pub(crate) status: String,
    pub(crate) last_seen_at: Option<String>,
    pub(crate) client_ip: Option<String>,
    pub(crate) client_version: Option<String>,
    pub(crate) config_revision: Option<u64>,
    pub(crate) last_applied_revision: Option<u64>,
    pub(crate) last_error: Option<String>,
    pub(crate) include_media: Option<bool>,
    pub(crate) data_redaction: Option<bool>,
    pub(crate) offline_export_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) hidden_mode_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) collector_log_level: Option<String>,
    #[serde(default)]
    pub(crate) log_directory: Option<String>,
    pub(crate) plans: Vec<CollectionPlanResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectionPlanResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) include_media: bool,
    pub(crate) schedule: CollectionSchedule,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) last_run_at: Option<String>,
    pub(crate) last_upload_at: Option<String>,
    pub(crate) last_status: Option<String>,
    pub(crate) last_detail: Option<String>,
    pub(crate) next_run_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorDesiredConfigResponse {
    pub(crate) collector_id: String,
    pub(crate) display_name: String,
    pub(crate) enabled: bool,
    pub(crate) config_revision: u64,
    pub(crate) organization_id: String,
    pub(crate) organization_name: String,
    pub(crate) collection_notice: String,
    pub(crate) upload_url: String,
    pub(crate) key_id: String,
    pub(crate) public_key_hex: String,
    pub(crate) include_media: bool,
    pub(crate) data_redaction: bool,
    pub(crate) offline_export_enabled: bool,
    pub(crate) hidden_mode_enabled: bool,
    pub(crate) collector_log_level: String,
    pub(crate) schedule: CollectionSchedule,
    pub(crate) manual_collection_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorControlEnvelopeResponse {
    pub(crate) schema_version: &'static str,
    pub(crate) config_revision: u64,
    pub(crate) expires_at: String,
    pub(crate) config: CollectorDesiredConfigResponse,
    pub(crate) signature_hex: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorHeartbeatRequest {
    #[serde(default)]
    pub(crate) update_capable: bool,
    #[serde(default)]
    pub(crate) software_update_error: Option<String>,
    pub(crate) client_version: Option<String>,
    pub(crate) current_config_revision: Option<u64>,
    pub(crate) last_applied_revision: Option<u64>,
    pub(crate) last_run_at: Option<String>,
    pub(crate) last_success_at: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) last_manual_collection_request_id: Option<String>,
    #[serde(default)]
    pub(crate) diagnostics: Vec<CollectorDiagnosticEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorDiagnosticEvent {
    pub(crate) timestamp: u64,
    pub(crate) event: String,
    pub(crate) details: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorHeartbeatResponse {
    pub(crate) software_update: Option<archive_transfer::software_update::SignedSoftwareUpdate>,
    pub(crate) status: String,
    pub(crate) config: Option<CollectorDesiredConfigResponse>,
    pub(crate) control: Option<CollectorControlEnvelopeResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorUpdateRequest {
    pub(crate) display_name: String,
    pub(crate) enabled: bool,
    pub(crate) include_media: bool,
    pub(crate) data_redaction: bool,
    pub(crate) offline_export_enabled: bool,
    #[serde(default)]
    pub(crate) hidden_mode_enabled: bool,
    #[serde(default = "default_collector_log_level")]
    pub(crate) collector_log_level: String,
    #[serde(default)]
    pub(crate) collection_notice: String,
    pub(crate) schedule: CollectionSchedule,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorPlanResponse {
    pub(crate) collector_id: String,
    pub(crate) file_name: String,
    pub(crate) organization_name: String,
    pub(crate) upload_url: String,
    pub(crate) include_media: bool,
    pub(crate) data_redaction: bool,
    pub(crate) offline_export_enabled: bool,
    pub(crate) hidden_mode_enabled: bool,
    pub(crate) collector_log_level: String,
    pub(crate) schedule: CollectionSchedule,
    pub(crate) created_at: String,
    pub(crate) last_upload_at: Option<String>,
    pub(crate) last_run_at: Option<String>,
    pub(crate) next_run_at: Option<String>,
    pub(crate) executable_available: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorListResponse {
    pub(crate) collectors: Vec<CollectorPlanResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CollectorResponse {
    pub(crate) file_name: String,
    pub(crate) directory: String,
    pub(crate) organization_id: String,
    pub(crate) key_id: String,
    pub(crate) collector_id: String,
    pub(crate) artifact: String,
    pub(crate) executable_generated: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenDirectoryResponse {
    pub(crate) opened: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthResponse {
    pub(crate) status: &'static str,
    pub(crate) schema: &'static str,
    pub(crate) network_mode: &'static str,
    pub(crate) listen_address: String,
    pub(crate) version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportResponse {
    pub(crate) export_id: String,
    pub(crate) batch_count: usize,
    pub(crate) inserted: u64,
    pub(crate) unchanged: u64,
    pub(crate) revised: u64,
    pub(crate) media_count: u64,
    pub(crate) missing_media_count: u64,
    pub(crate) completed_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArchiveSummaryResponse {
    #[serde(flatten)]
    pub(crate) summary: archive_store::ArchiveSummary,
    pub(crate) local_collection_available: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ArchiveUpdateQuery {
    pub(crate) since: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveUpdateResponse {
    pub(crate) revision: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorResponse {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) recoverable: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageQuery {
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LocalCollectionQuery {
    pub(crate) include_media: Option<bool>,
    pub(crate) source_root: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenMediaQuery {
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesQuery {
    pub(crate) conversation_id: String,
    pub(crate) participant_id: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) message_type: Option<String>,
    pub(crate) media_only: Option<bool>,
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u64>,
    pub(crate) sort: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct GlobalSearchQuery {
    pub(crate) q: String,
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u64>,
    pub(crate) sort: Option<String>,
    pub(crate) conversation_id: Option<String>,
    pub(crate) participant_id: Option<String>,
    pub(crate) message_type: Option<String>,
    pub(crate) media_only: Option<bool>,
    pub(crate) starts_at: Option<String>,
    pub(crate) ends_at: Option<String>,
    pub(crate) locate: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PositionQuery {
    pub(crate) sort: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessageResponse {
    #[serde(flatten)]
    pub(crate) message: archive_domain::MessageV1,
    pub(crate) sender_name: Option<String>,
    pub(crate) sender_kind: Option<String>,
    pub(crate) quoted_message: Option<QuotedMessageResponse>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GlobalSearchResponse {
    #[serde(flatten)]
    pub(crate) message: MessageResponse,
    pub(crate) conversation_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) offset_in_conversation: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessagePositionResponse {
    pub(crate) conversation_id: String,
    pub(crate) offset_in_conversation: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessageCountResponse {
    pub(crate) total: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct QuotedMessageResponse {
    pub(crate) stable_message_id: String,
    pub(crate) sender_id: Option<String>,
    pub(crate) sender_name: Option<String>,
    pub(crate) sent_at: chrono::DateTime<Utc>,
    pub(crate) body_text: Option<String>,
    pub(crate) message_type: archive_domain::MessageType,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportRequest {
    pub(crate) scope: ExportScope,
    pub(crate) format: ExportFormat,
    pub(crate) conversation_id: Option<String>,
    #[serde(default)]
    pub(crate) conversation_ids: Vec<String>,
    pub(crate) start_date: Option<String>,
    pub(crate) end_date: Option<String>,
    pub(crate) participant_id: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) message_type: Option<String>,
    pub(crate) media_only: Option<bool>,
    #[serde(default)]
    pub(crate) data_redaction: bool,
    #[serde(default)]
    pub(crate) simplify: bool,
    #[serde(default)]
    pub(crate) pretty: bool,
    #[serde(default)]
    pub(crate) conversation_order: ExportOrder,
    #[serde(default)]
    pub(crate) message_order: ExportOrder,
}

pub(crate) fn parse_export_date(
    value: Option<&str>,
    end_of_day: bool,
) -> Result<Option<chrono::DateTime<Utc>>, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let date =
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| ApiError::invalid_query())?;
    let date_time = if end_of_day {
        date.checked_add_signed(TimeDelta::days(1))
            .ok_or_else(ApiError::invalid_query)?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(ApiError::invalid_query)?
    } else {
        date.and_hms_opt(0, 0, 0)
            .ok_or_else(ApiError::invalid_query)?
    };
    Ok(Some(Utc.from_utc_datetime(&date_time)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalExportRequest {
    pub(crate) format: ExportFormat,
    #[serde(default)]
    pub(crate) source_root: Option<String>,
    #[serde(default)]
    pub(crate) data_redaction: bool,
    #[serde(default)]
    pub(crate) simplify: bool,
    #[serde(default)]
    pub(crate) pretty: bool,
    #[serde(default)]
    pub(crate) conversation_order: ExportOrder,
    #[serde(default)]
    pub(crate) message_order: ExportOrder,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExportOrder {
    Ascending,
    #[default]
    Descending,
}

impl ExportOrder {
    pub(crate) fn is_ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    pub(crate) export_id: String,
    pub(crate) file_name: String,
    pub(crate) directory: String,
    pub(crate) message_count: u64,
    pub(crate) media_count: u64,
    pub(crate) missing_media_count: u64,
    pub(crate) manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportStartResponse {
    pub(crate) task_id: String,
    pub(crate) status: String,
}

pub(crate) fn parse_message_type(value: &str) -> Result<MessageType, ApiError> {
    match value {
        "text" => Ok(MessageType::Text),
        "image" => Ok(MessageType::Image),
        "audio" => Ok(MessageType::Audio),
        "video" => Ok(MessageType::Video),
        "file" => Ok(MessageType::File),
        "link" => Ok(MessageType::Link),
        "reply" => Ok(MessageType::Reply),
        "system" => Ok(MessageType::System),
        "unsupported" => Ok(MessageType::Unsupported),
        _ => Err(ApiError::invalid_query()),
    }
}

pub(crate) fn parse_sort_ascending(value: Option<&str>) -> Result<bool, ApiError> {
    match value.unwrap_or("desc") {
        "asc" => Ok(true),
        "desc" => Ok(false),
        _ => Err(ApiError::invalid_query()),
    }
}
