use crate::{
    ApiError, AppState, CollectionSchedule, CollectionScheduleMode, ImportResponse,
    LocalCollectionMediaSink, LocalCollectionProgress, LocalCollectionProgressReporter,
    LocalCollectionQuery, authorize, ingest_export, local_collection_error,
    persist_enterprise_config, persist_local_media_file,
};
use axum::Json;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use chrono::Local;
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) async fn collect_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LocalCollectionQuery>,
) -> Result<Json<ImportResponse>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    if state.local_collector.is_none() {
        return Err(ApiError::local_collection_unavailable());
    }
    if state
        .local_collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(ApiError::local_collection_running());
    }
    let _run_guard = LocalCollectionRunGuard(Arc::clone(&state.local_collection_running));
    let (configured_include_media, configured_data_redaction) = {
        let config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        (config.server_include_media, config.data_redaction)
    };
    let include_media = query.include_media.unwrap_or(configured_include_media);
    let data_redaction = query.data_redaction.unwrap_or(configured_data_redaction);
    run_local_collection(
        &state,
        include_media,
        data_redaction,
        None,
        query.source_root,
    )
    .await
}

pub(crate) struct LocalCollectionRunGuard(Arc<AtomicBool>);

impl Drop for LocalCollectionRunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) async fn run_local_collection_scheduler(state: AppState) {
    let mut next_runs: BTreeMap<String, (CollectionSchedule, chrono::DateTime<Local>)> =
        BTreeMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let plans = match state.enterprise_config.lock() {
            Ok(config) => config.local_collection_plans.clone(),
            Err(_) => continue,
        };
        let active_ids = plans
            .iter()
            .map(|plan| plan.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        next_runs.retain(|id, _| active_ids.contains(id.as_str()));
        let now = Local::now();
        for plan in &plans {
            if plan.schedule.mode == CollectionScheduleMode::Disabled {
                next_runs.remove(&plan.id);
                continue;
            }
            let should_reset = next_runs
                .get(&plan.id)
                .is_none_or(|(schedule, _)| schedule != &plan.schedule);
            if should_reset && let Some(next) = plan.schedule.next_after(now) {
                next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
            }
        }
        for plan in plans {
            let Some((_, due_at)) = next_runs.get(&plan.id).cloned() else {
                continue;
            };
            if now < due_at {
                continue;
            }
            if state
                .local_collection_running
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                if let Some(next) = plan.schedule.next_after(now) {
                    next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
                }
                continue;
            }
            let guard = LocalCollectionRunGuard(Arc::clone(&state.local_collection_running));
            let result = run_local_collection_plan(&state, &plan.id, plan.include_media).await;
            drop(guard);
            if let Err(error) = result {
                set_local_collection_progress(
                    &state,
                    false,
                    0,
                    "定时采集失败",
                    &error.body.message,
                );
            }
            if let Some(next) = plan.schedule.next_after(Local::now()) {
                next_runs.insert(plan.id.clone(), (plan.schedule.clone(), next));
            }
        }
    }
}

pub(crate) async fn run_local_collection_plan(
    state: &AppState,
    plan_id: &str,
    include_media: bool,
) -> Result<Json<ImportResponse>, ApiError> {
    let since = state
        .enterprise_config
        .lock()
        .map_err(|_| ApiError::store())?
        .local_collection_cursors
        .get(plan_id)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis().saturating_sub(5 * 60 * 1000));
    // A snapshot predates task completion, sometimes by many minutes. Advance
    // only to its conservative lower bound after a successful ingest. Old configs
    // without this cursor get one full scan instead of trusting a completion time.
    let started_at = Utc::now();
    let result = run_local_collection_once(state, include_media, since).await;
    update_local_plan_result(state, plan_id, started_at, &result);
    result
}

pub(crate) fn update_local_plan_result(
    state: &AppState,
    plan_id: &str,
    started_at: chrono::DateTime<Utc>,
    result: &Result<Json<ImportResponse>, ApiError>,
) {
    if let Ok(mut config) = state.enterprise_config.lock()
        && let Some(plan) = config
            .local_collection_plans
            .iter_mut()
            .find(|plan| plan.id == plan_id)
    {
        if result.is_ok() {
            plan.last_run_at = Some(Utc::now().to_rfc3339());
        }
        plan.last_status = Some(if result.is_ok() { "success" } else { "error" }.into());
        plan.last_detail = result
            .as_ref()
            .err()
            .map(|error| error.body.message.clone());
        plan.updated_at = Utc::now().to_rfc3339();
        if result.is_ok() {
            config
                .local_collection_cursors
                .insert(plan_id.to_owned(), started_at.to_rfc3339());
        }
        let _ = persist_enterprise_config(&state.data_root, &config);
    }
}

pub(crate) async fn run_local_collection_once(
    state: &AppState,
    include_media: bool,
    since_unix_ms: Option<i64>,
) -> Result<Json<ImportResponse>, ApiError> {
    let data_redaction = {
        let config = state
            .enterprise_config
            .lock()
            .map_err(|_| ApiError::store())?;
        config.data_redaction
    };
    run_local_collection(state, include_media, data_redaction, since_unix_ms, None).await
}

async fn run_local_collection(
    state: &AppState,
    include_media: bool,
    data_redaction: bool,
    since_unix_ms: Option<i64>,
    source_root: Option<String>,
) -> Result<Json<ImportResponse>, ApiError> {
    let collector = state
        .local_collector
        .clone()
        .ok_or_else(ApiError::local_collection_unavailable)?;
    let progress = Arc::clone(&state.local_collection_progress);
    let reporter: LocalCollectionProgressReporter = Arc::new(move |percent, stage, detail| {
        if let Ok(mut current) = progress.lock() {
            *current = LocalCollectionProgress {
                running: true,
                percent: percent.min(95),
                stage: stage.to_owned(),
                detail: detail.to_owned(),
            };
        }
    });
    let media_root = Arc::clone(&state.data_root);
    let media_sink: LocalCollectionMediaSink =
        Arc::new(move |source, expected_hash, expected_size| {
            persist_local_media_file(&media_root, source, expected_hash, expected_size)
                .map_err(|_| "媒体文件写入归档失败。".to_owned())
        });
    let result = async {
        set_local_collection_progress(
            state,
            true,
            4,
            if include_media {
                "完整采集"
            } else {
                "采集文字"
            },
            if include_media {
                "正在一次性采集文字、图片和附件。"
            } else {
                "正在采集文字消息和会话关系。"
            },
        );
        let export = tokio::task::spawn_blocking(move || {
            collector(
                include_media,
                data_redaction,
                since_unix_ms,
                source_root,
                reporter,
                media_sink,
            )
        })
        .await
        .map_err(|_| ApiError::local_collection_failed("本机采集进程异常结束，请重试。".into()))?
        .map_err(local_collection_error)?;
        set_local_collection_progress(
            state,
            true,
            96,
            "写入归档",
            "正在写入本次采集的文字、图片和附件。",
        );
        ingest_export(state, export).await
    }
    .await;
    if result.is_ok() {
        set_local_collection_progress(
            state,
            false,
            100,
            "采集完成",
            "本机数据已经合并到当前归档。",
        );
    } else if let Err(error) = &result {
        set_local_collection_progress(state, false, 0, "采集失败", &error.body.message);
    }
    result
}

pub(crate) async fn local_collection_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LocalCollectionProgress>, ApiError> {
    authorize(&headers, &state.token_sha256)?;
    let progress = state
        .local_collection_progress
        .lock()
        .map_err(|_| ApiError::store())?
        .clone();
    Ok(Json(progress))
}

pub(crate) fn set_local_collection_progress(
    state: &AppState,
    running: bool,
    percent: u8,
    stage: &str,
    detail: &str,
) {
    if let Ok(mut progress) = state.local_collection_progress.lock() {
        *progress = LocalCollectionProgress {
            running,
            percent: percent.min(100),
            stage: stage.to_owned(),
            detail: detail.to_owned(),
        };
    }
}
