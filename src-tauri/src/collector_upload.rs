//! Results are confirmed against one authenticated product endpoint. Pending
//! metadata contains no package bytes, credentials, source paths or messages.
use crate::{
    collector_http,
    collector_http::Response,
    state::{AppState, CommandError, internal_error},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub(super) struct PackageIdentity {
    export_id: Uuid,
    #[serde(rename = "plaintext_sha256")]
    content_sha256: String,
}

#[derive(Clone)]
pub(super) struct PendingUpload {
    upload_id: Uuid,
    identity: PackageIdentity,
    scope: Vec<u8>,
    tracked: bool,
}

#[derive(Clone)]
pub(super) struct UploadReceipt {
    identity: PackageIdentity,
    scope: Vec<u8>,
    result: ConfirmedUpload,
}

#[derive(Clone, Debug)]
pub(super) struct ConfirmedUpload {
    pub(super) completed_at: String,
    pub(super) message_count: u64,
    pub(super) media_count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadStatus {
    schema: String,
    upload_id: Uuid,
    export_id: Uuid,
    content_sha256: String,
    status: String,
    result: Option<CompletedUpload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletedUpload {
    export_id: Uuid,
    completed_at: String,
    inserted: u64,
    unchanged: u64,
    revised: u64,
    media_count: u64,
}

impl CompletedUpload {
    fn confirmed(&self) -> Option<ConfirmedUpload> {
        Some(ConfirmedUpload {
            completed_at: self.completed_at.clone(),
            message_count: self
                .inserted
                .checked_add(self.unchanged)?
                .checked_add(self.revised)?,
            media_count: self.media_count,
        })
    }
}

#[derive(Clone, Copy)]
struct Policy {
    receive_timeout: u32,
    status_timeout: u32,
    confirm_budget: Duration,
    poll_interval: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            receive_timeout: 32 * 60,
            status_timeout: 30,
            confirm_budget: Duration::from_secs(5 * 60),
            poll_interval: Duration::from_secs(1),
        }
    }
}

struct UploadClient<'a> {
    state: &'a AppState,
    endpoint: &'a str,
    token: &'a str,
    scope: Vec<u8>,
    policy: Policy,
}

fn unknown(message: &str) -> CommandError {
    CommandError {
        code: "ENTERPRISE_UPLOAD_RESULT_UNKNOWN",
        message: message.into(),
        recoverable: true,
    }
}

fn unresolved() -> CommandError {
    unknown("上传结果暂未确认，已保留原上传标识；不会重复发送文件或推进增量进度，请稍后继续确认。")
}

fn failed() -> CommandError {
    CommandError {
        code: "ENTERPRISE_UPLOAD_FAILED",
        message: "上传未完成，请检查本机临时空间、授权、归档空间及连接后重试。".into(),
        recoverable: true,
    }
}

fn code(response: &Response) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()?
        .get("code")?
        .as_str()
        .map(str::to_owned)
}

fn valid_time(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

fn stage(state: &AppState, value: &str) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.upload_stage = Some(value.into());
    }
}

fn record_confirmed_result(state: &AppState, result: &ConfirmedUpload) {
    // The remote archive is confirmed even if saving a local checkpoint later
    // fails. Keep that fact separate from local fingerprints/checkpoint writes.
    if let Ok(mut status) = state.schedule_status.lock() {
        status.last_success_at = Some(result.completed_at.clone());
        status.last_message_count = Some(result.message_count);
        status.last_media_count = Some(result.media_count);
        status.last_upload_message_count = Some(result.message_count);
    }
}

impl<'a> UploadClient<'a> {
    fn new(state: &'a AppState, endpoint: &'a str, token: &'a str, policy: Policy) -> Self {
        let mut scope = Sha256::new();
        scope.update((endpoint.len() as u64).to_le_bytes());
        scope.update(endpoint.as_bytes());
        scope.update(token.as_bytes());
        Self {
            state,
            endpoint,
            token,
            scope: scope.finalize().to_vec(),
            policy,
        }
    }

    fn result_url(&self, id: Uuid) -> Option<String> {
        let mut url = tauri::Url::parse(self.endpoint).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        let base = url
            .path()
            .trim_end_matches('/')
            .strip_suffix("/api/v1/imports/enterprise")?;
        let path = format!("{base}/api/v1/collector-uploads/{id}");
        url.set_path(&path);
        Some(url.to_string())
    }

    fn pending(&self) -> Result<Option<PendingUpload>, CommandError> {
        Ok(self
            .state
            .pending_upload
            .lock()
            .map_err(|_| internal_error())?
            .clone())
    }

    fn clear(&self, id: Uuid) -> Result<(), CommandError> {
        let mut pending = self
            .state
            .pending_upload
            .lock()
            .map_err(|_| internal_error())?;
        if pending.as_ref().is_some_and(|p| p.upload_id == id) {
            *pending = None;
        }
        Ok(())
    }

    fn save(&self, pending: PendingUpload) -> Result<(), CommandError> {
        let mut current = self
            .state
            .pending_upload
            .lock()
            .map_err(|_| internal_error())?;
        if current.is_some() {
            return Err(unresolved());
        }
        *current = Some(pending);
        Ok(())
    }

    fn confirm_pending(&self) -> Result<Option<ConfirmedUpload>, CommandError> {
        if let Some(pending) = self.pending()? {
            if pending.scope != self.scope {
                return Err(unknown(
                    "存在结果未确认的旧上传，但服务端配置已变化。请先核对旧归档，重启采集端后再使用新配置。",
                ));
            }
            if !pending.tracked {
                return Err(unknown(
                    "旧工作台不支持上传结果查询，无法确认上一项上传。请先核对归档，重启采集端后再决定是否重试。",
                ));
            }
            return self.confirm(&pending).map(Some);
        }
        Ok(None)
    }

    fn confirm(&self, pending: &PendingUpload) -> Result<ConfirmedUpload, CommandError> {
        let url = self.result_url(pending.upload_id).ok_or_else(unresolved)?;
        stage(self.state, "正在确认上传结果");
        let started = Instant::now();
        let mut failures = 0;
        while started.elapsed() < self.policy.confirm_budget {
            let remaining = self
                .policy
                .confirm_budget
                .saturating_sub(started.elapsed())
                .as_secs()
                .max(1);
            let timeout = self
                .policy
                .status_timeout
                .min(u32::try_from(remaining).unwrap_or(u32::MAX));
            match collector_http::get_json(&url, self.token, 10, timeout) {
                Ok(response) if response.is_success() => {
                    let progress = serde_json::from_slice::<UploadStatus>(&response.body)
                        .map_err(|_| unresolved())?;
                    if progress.schema != "collector-upload.v1"
                        || progress.upload_id != pending.upload_id
                        || progress.export_id != pending.identity.export_id
                        || progress.content_sha256 != pending.identity.content_sha256
                    {
                        return Err(unresolved());
                    }
                    match progress.status.as_str() {
                        "completed" => {
                            let result = progress
                                .result
                                .filter(|r| {
                                    r.export_id == pending.identity.export_id
                                        && valid_time(&r.completed_at)
                                })
                                .ok_or_else(unresolved)?;
                            let result = result.confirmed().ok_or_else(unresolved)?;
                            self.clear(pending.upload_id)?;
                            *self
                                .state
                                .confirmed_upload
                                .lock()
                                .map_err(|_| internal_error())? = Some(UploadReceipt {
                                identity: pending.identity.clone(),
                                scope: self.scope.clone(),
                                result: result.clone(),
                            });
                            record_confirmed_result(self.state, &result);
                            return Ok(result);
                        }
                        "failed" => {
                            self.clear(pending.upload_id)?;
                            return Err(failed());
                        }
                        "queued" | "receiving" | "running" => {
                            failures = 0;
                            stage(
                                self.state,
                                if progress.status == "running" {
                                    "工作台正在校验并归档"
                                } else {
                                    "等待工作台确认上传"
                                },
                            );
                        }
                        _ => return Err(unresolved()),
                    }
                }
                Ok(response) if matches!(response.status, 401 | 403) => return Err(unresolved()),
                _ => {
                    failures += 1;
                    if failures >= 3 {
                        return Err(unresolved());
                    }
                }
            }
            std::thread::sleep(
                self.policy
                    .poll_interval
                    .min(self.policy.confirm_budget.saturating_sub(started.elapsed())),
            );
        }
        Err(unresolved())
    }

    fn upload(&self, package: &[u8]) -> Result<String, CommandError> {
        let identity: PackageIdentity =
            serde_json::from_slice(package).map_err(|_| internal_error())?;
        if identity.export_id.is_nil()
            || identity.content_sha256.len() != 64
            || !identity
                .content_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(internal_error());
        }
        self.confirm_pending()?;
        if let Some(receipt) = self
            .state
            .confirmed_upload
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            && receipt.scope == self.scope
            && receipt.identity == identity
        {
            return Ok(receipt.result.completed_at.clone());
        }
        let pending = PendingUpload {
            upload_id: Uuid::new_v4(),
            identity,
            scope: self.scope.clone(),
            tracked: false,
        };
        let Some(url) = self.result_url(pending.upload_id) else {
            return self.legacy(package, pending);
        };
        stage(self.state, "检查工作台上传能力");
        let probe = collector_http::get_json(&url, self.token, 10, self.policy.status_timeout)
            .map_err(|_| failed())?;
        match (probe.status, code(&probe).as_deref()) {
            (404, Some("COLLECTOR_UPLOAD_NOT_FOUND")) => {}
            (404, _) | (403, Some("COLLECTOR_IDENTITY_REQUIRED")) => {
                return self.legacy(package, pending);
            }
            (200, _) if serde_json::from_slice::<UploadStatus>(&probe.body).is_err() => {
                return self.legacy(package, pending);
            }
            _ => return Err(failed()),
        }
        let pending = PendingUpload {
            tracked: true,
            ..pending
        };
        self.save(pending.clone())?;
        stage(self.state, "正在上传加密文件");
        let export_id = pending.identity.export_id.to_string();
        let response = collector_http::post_upload(
            &url,
            self.token,
            package,
            collector_http::UploadIdentity {
                export_id: &export_id,
                content_sha256: &pending.identity.content_sha256,
            },
            15,
            self.policy.receive_timeout,
        );
        if let Err(error) = &response
            && !error.outcome_unknown
        {
            self.clear(pending.upload_id)?;
            return Err(failed());
        }
        if let Ok(response) = &response
            // Only the product's exact registration-refusal replies establish
            // that no upload was accepted. A gateway may reuse a business code
            // with another status after forwarding the original request.
            && matches!(
                (response.status, code(response).as_deref()),
                (400, Some("INVALID_QUERY"))
                    | (401, Some("UNAUTHORIZED"))
                    | (403, Some("COLLECTOR_IDENTITY_REQUIRED"))
                    | (503, Some("COLLECTOR_UPLOAD_BUSY"))
            )
        {
            self.clear(pending.upload_id)?;
            return Err(failed());
        }
        // An accepted upload may outlive its HTTP response. Never re-POST here.
        self.confirm(&pending).map(|result| result.completed_at)
    }

    fn legacy(&self, package: &[u8], pending: PendingUpload) -> Result<String, CommandError> {
        self.save(pending.clone())?;
        stage(self.state, "正在上传加密文件");
        let response = collector_http::post_json_upload(
            self.endpoint,
            self.token,
            package,
            "application/octet-stream",
            15,
            300,
        );
        let response = match response {
            Ok(response) => response,
            Err(error) if !error.outcome_unknown => {
                self.clear(pending.upload_id)?;
                return Err(failed());
            }
            Err(_) => return Err(unresolved()),
        };
        if !response.is_success() {
            if response.status < 500 && !matches!(response.status, 408 | 409 | 429) {
                self.clear(pending.upload_id)?;
                return Err(failed());
            }
            return Err(unresolved());
        }
        let result: CompletedUpload =
            serde_json::from_slice(&response.body).map_err(|_| unresolved())?;
        if result.export_id != pending.identity.export_id || !valid_time(&result.completed_at) {
            return Err(unresolved());
        }
        let result = result.confirmed().ok_or_else(unresolved)?;
        self.clear(pending.upload_id)?;
        *self
            .state
            .confirmed_upload
            .lock()
            .map_err(|_| internal_error())? = Some(UploadReceipt {
            identity: pending.identity,
            scope: self.scope.clone(),
            result: result.clone(),
        });
        record_confirmed_result(self.state, &result);
        Ok(result.completed_at)
    }
}

pub(super) fn prepare_upload(
    state: &AppState,
    endpoint: &str,
    token: &[u8],
) -> Result<Option<ConfirmedUpload>, CommandError> {
    let token = std::str::from_utf8(token).map_err(|_| internal_error())?;
    UploadClient::new(state, endpoint, token, Policy::default()).confirm_pending()
}

pub(super) fn upload(
    state: &AppState,
    endpoint: &str,
    token: &[u8],
    package: &[u8],
) -> Result<String, CommandError> {
    let token = std::str::from_utf8(token).map_err(|_| internal_error())?;
    UploadClient::new(state, endpoint, token, Policy::default()).upload(package)
}

#[cfg(test)]
mod tests;
