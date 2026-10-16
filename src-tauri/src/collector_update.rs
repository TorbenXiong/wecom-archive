use super::*;
use archive_transfer::software_update::{
    MAX_EXECUTABLE_BYTES, SignedSoftwareUpdate, newer, signing_payload, valid_hash,
};

pub(super) fn verify(
    envelope: &SignedSoftwareUpdate,
    config: &EnterpriseCollectorConfig,
) -> Result<(), String> {
    let update = &envelope.update;
    if update.collector_id != config.collector_id
        || uuid::Uuid::parse_str(&update.request_id).is_err()
        || !newer(env!("CARGO_PKG_VERSION"), &update.version)
        || !valid_hash(&update.sha256)
        || update.size == 0
        || update.size > MAX_EXECUTABLE_BYTES
        || chrono::DateTime::parse_from_rfc3339(&update.expires_at)
            .ok()
            .is_none_or(|time| time <= Utc::now())
    {
        return Err("升级指令已失效或不适用于当前采集端。".into());
    }
    let key = hex::decode(&config.signing_public_key_hex).map_err(|_| "升级签名公钥无效。")?;
    let signature = hex::decode(&envelope.signature_hex).map_err(|_| "升级签名无效。")?;
    source_windows::enterprise_crypto::verify(
        &key,
        &signing_payload(update).map_err(|_| "升级指令无效。")?,
        &signature,
    )
    .map_err(|_| "升级签名校验失败。".into())
}

pub(super) fn receive(
    value: &serde_json::Value,
    config: &EnterpriseCollectorConfig,
    state: &AppState,
) {
    let Ok(envelope) = serde_json::from_value::<SignedSoftwareUpdate>(value.clone()) else {
        return;
    };
    if verify(&envelope, config).is_err() {
        return;
    }
    if let Ok(mut pending) = state.pending_update.lock() {
        *pending = Some(envelope);
    }
}

pub(super) fn run_pending(state: &AppState, config: &EnterpriseCollectorConfig) {
    let pending = state
        .pending_update
        .lock()
        .ok()
        .and_then(|value| value.clone());
    let Some(envelope) = pending else {
        return;
    };
    if state
        .update_attempt
        .lock()
        .is_ok_and(|value| value.as_deref() == Some(&envelope.update.request_id))
    {
        return;
    }
    // Serialize with manual collection, scheduled collection and offline export.
    if state
        .collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    if let Ok(mut value) = state.update_attempt.lock() {
        *value = Some(envelope.update.request_id.clone());
    }
    let result = (|| {
        verify(&envelope, config)?;
        let directory = update::staging_directory()?;
        let result = (|| {
            let url = format!(
                "{}/api/v1/collectors/{}/software-update/{}",
                server_base_url(&config.upload_url),
                config.collector_id,
                envelope.update.request_id
            );
            let next = directory.join("next.exe");
            update::download(
                &url,
                &next,
                envelope.update.size,
                Some(&config.upload_token),
            )?;
            if fs::metadata(&next).map_err(|_| "更新文件不可用。")?.len() != envelope.update.size
            {
                return Err("更新文件长度不匹配。".into());
            }
            update::validate_executable(&next, &envelope.update.sha256)?;
            if archive_transfer::read_enterprise_collector_config(&next).is_ok() {
                return Err("更新模板含有其他绑定信息。".into());
            }
            // Keep enrollment identity; the latest signed config remains in DPAPI.
            let current = std::env::current_exe().map_err(|_| "无法定位采集端。")?;
            let binding = archive_transfer::read_enterprise_collector_config(&current)
                .map_err(|_| "采集端绑定缺失。")?;
            archive_transfer::append_enterprise_collector_config(&next, &binding)
                .map_err(|_| "无法保留采集端绑定。")?;
            update::schedule(&directory)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&directory);
        }
        result
    })();
    if result.is_ok() {
        state.scheduler_shutdown.store(true, Ordering::Release);
        if let Ok(Some(app)) = state.tray_app.lock().map(|value| value.clone()) {
            app.exit(0);
        }
        // Hold the task gate through process shutdown.
        std::mem::forget(guard);
    } else if let Ok(mut error) = state.software_update_error.lock() {
        *error = Some("UPDATE_FAILED".into());
    }
}
