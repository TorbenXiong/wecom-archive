use crate::{ApiError, EnterpriseConfig, encode_hex};
use axum::http::HeaderMap;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::sync::Mutex;

pub(super) fn authorize(
    headers: &HeaderMap,
    expected: &Arc<Mutex<[u8; 32]>>,
) -> Result<(), ApiError> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(ApiError::unauthorized());
    };
    let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    let expected = expected.lock().map_err(|_| ApiError::store())?;
    let difference = actual
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |state, (left, right)| state | (left ^ right));
    if difference == 0 {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

pub(super) fn authorize_enterprise_upload(
    headers: &HeaderMap,
    server_token_sha256: &Arc<Mutex<[u8; 32]>>,
    enterprise_config: &Arc<Mutex<EnterpriseConfig>>,
) -> Result<Option<String>, ApiError> {
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(ApiError::unauthorized());
    };
    let actual: [u8; 32] = Sha256::digest(value.as_bytes()).into();
    let server_matches = {
        let expected = server_token_sha256.lock().map_err(|_| ApiError::store())?;
        constant_time_equal(&actual, expected.as_slice())
    };
    if server_matches {
        return Ok(None);
    }
    let actual_hex = encode_hex(&actual);
    let config = enterprise_config.lock().map_err(|_| ApiError::store())?;
    config
        .collectors
        .iter()
        .find(|collector| {
            constant_time_equal(
                actual_hex.as_bytes(),
                collector.upload_token_sha256.as_bytes(),
            ) && collector.revoked_at.is_none()
        })
        .map(|collector| {
            if !collector.enabled {
                return Err(ApiError::unauthorized());
            }
            Ok(Some(collector.collector_id.clone()))
        })
        .transpose()?
        .ok_or_else(ApiError::unauthorized)
}

pub(super) fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}
