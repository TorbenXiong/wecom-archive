use crate::collection::{
    CollectionAuthorization, CollectionReadOptions, LOCAL_NOTICE_VERSION, MediaCollectionOptions,
    collect_candidate_with_options_since,
};
use crate::collector_config::{
    CollectionSchedule, CollectionScheduleMode, EnterpriseCollectorConfig,
    ensure_collector_config_fresh, load_enterprise_collector_config,
};
use crate::collector_control::sync_collector_config_with_status;
use crate::collector_transfer::{
    encrypt_enterprise_export, post_enterprise_package, redact_sensitive_export,
};
use crate::paths::{collector_data_root, collector_work_root, discover_local_sources};
use crate::source_keys::{choose_collectable_source, resolve_collection_key};
use crate::state::{
    AppState, CollectionRunGuard, CollectionSummary, CommandError, begin_collector_attempt,
    finish_collector_attempt, internal_error,
};
use crate::{collector_incremental, collector_update};
use chrono::{Local, NaiveTime, TimeDelta, Utc};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(super) struct CollectorScheduleTimer {
    pub(super) schedule: CollectionSchedule,
    pub(super) next_run: Option<chrono::DateTime<Local>>,
    pub(super) activation_pending: bool,
}

impl CollectorScheduleTimer {
    pub(super) fn new(schedule: CollectionSchedule, now: chrono::DateTime<Local>) -> Self {
        Self {
            next_run: next_schedule_at(&schedule, now),
            schedule,
            activation_pending: false,
        }
    }

    pub(super) fn update(
        &mut self,
        schedule: &CollectionSchedule,
        now: chrono::DateTime<Local>,
    ) -> bool {
        if self.schedule == *schedule {
            return false;
        }
        if schedule.mode == CollectionScheduleMode::Disabled {
            self.activation_pending = false;
        } else if self.schedule.mode == CollectionScheduleMode::Disabled {
            self.activation_pending = true;
        }
        self.schedule = schedule.clone();
        // Keep the first run pending while another collection holds the lock.
        // Repeated heartbeats must not move it into the next interval.
        self.next_run = if self.activation_pending {
            Some(now)
        } else {
            next_schedule_at(schedule, now)
        };
        true
    }

    pub(super) fn completed(&mut self, now: chrono::DateTime<Local>) {
        self.activation_pending = false;
        self.next_run = next_schedule_at(&self.schedule, now);
    }
}

pub(super) fn collector_scheduler(state: AppState) {
    let config = match load_enterprise_collector_config() {
        Ok(config) => config,
        Err(_) => return,
    };
    let mut timer = CollectorScheduleTimer::new(config.collector_schedule.clone(), Local::now());
    set_next_run(&state, timer.next_run);
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let _ = sync_collector_config_with_status(Some(&state.schedule_status), Some(&state));
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        let latest = match load_enterprise_collector_config() {
            Ok(config) => config,
            Err(_) => continue,
        };
        collector_update::run_pending(&state, &latest);
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        if state
            .schedule_status
            .lock()
            .is_ok_and(|status| status.server_authorization_denied)
        {
            set_next_run(&state, None);
            continue;
        }
        if !latest.enabled {
            timer.update(
                &CollectionSchedule {
                    mode: CollectionScheduleMode::Disabled,
                    ..latest.collector_schedule.clone()
                },
                Local::now(),
            );
            set_next_run(&state, None);
            continue;
        }
        if let Some(request_id) = latest.manual_collection_request_id.clone() {
            let should_run = state
                .schedule_status
                .lock()
                .map(|status| {
                    status.last_manual_collection_request_id.as_deref() != Some(request_id.as_str())
                        && status.manual_collection_attempted_id.as_deref()
                            != Some(request_id.as_str())
                })
                .unwrap_or(false);
            if should_run {
                if let Ok(mut status) = state.schedule_status.lock() {
                    status.manual_collection_attempted_id = Some(request_id.clone());
                }
                if let Some(result) = run_collection_and_record(&state, &latest, false, true) {
                    if result.is_ok()
                        && let Ok(mut status) = state.schedule_status.lock()
                    {
                        status.last_manual_collection_request_id = Some(request_id);
                    } else if let Ok(mut status) = state.schedule_status.lock() {
                        status.manual_collection_attempted_id = None;
                    }
                    timer.update(&latest.collector_schedule, Local::now());
                    timer.completed(Local::now());
                    set_next_run(&state, timer.next_run);
                    continue;
                } else if let Ok(mut status) = state.schedule_status.lock() {
                    status.manual_collection_attempted_id = None;
                }
            }
        }
        if timer.update(&latest.collector_schedule, Local::now()) {
            set_next_run(&state, timer.next_run);
        }
        if !timer.activation_pending
            && timer.schedule.mode == CollectionScheduleMode::Interval
            && let Ok(status) = state.schedule_status.lock()
            && let Some(value) = status.next_run_at.as_deref()
            && let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        {
            timer.next_run = parsed.and_local_timezone(Local).earliest();
        }
        let Some(due_at) = timer.next_run else {
            continue;
        };
        if Local::now() < due_at {
            continue;
        }
        if run_collection_and_record(&state, &latest, timer.activation_pending, false).is_none() {
            continue;
        }
        if state.scheduler_shutdown.load(Ordering::Acquire) {
            return;
        }
        timer.completed(Local::now());
        set_next_run(&state, timer.next_run);
    }
}

pub(super) fn run_collection_and_record(
    state: &AppState,
    config: &EnterpriseCollectorConfig,
    force_upload: bool,
    force_full_scan: bool,
) -> Option<Result<ScheduledCollectionOutcome, CommandError>> {
    if state
        .collection_running
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return None;
    }
    let _guard = CollectionRunGuard(Arc::clone(&state.collection_running));
    begin_collector_attempt(state);
    let result = scheduled_collect_and_upload(state, config, force_upload, force_full_scan);
    finish_collector_attempt(state, &result);
    if let Ok(outcome) = &result
        && let Ok(mut status) = state.schedule_status.lock()
    {
        status.last_message_count = Some(outcome.summary.message_count);
        status.last_media_count = Some(outcome.summary.media_count);
        if let Some(completed_at) = outcome.completed_at.as_ref() {
            status.last_success_at = Some(completed_at.clone());
        }
    }
    Some(result)
}

pub(super) fn set_next_run(state: &AppState, next: Option<chrono::DateTime<Local>>) {
    if let Ok(mut status) = state.schedule_status.lock() {
        status.next_run_at = next.map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string());
    }
}

pub(super) fn set_next_run_from_completion(state: &AppState, schedule: &CollectionSchedule) {
    let next = state
        .schedule_status
        .lock()
        .ok()
        .and_then(|status| status.last_success_at.clone())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Local))
        .and_then(|value| next_schedule_at(schedule, value));
    set_next_run(state, next);
}

pub(super) fn format_status_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

pub(super) fn describe_collector_schedule(schedule: &CollectionSchedule) -> String {
    match schedule.mode {
        CollectionScheduleMode::Disabled => "采集计划未启用".into(),
        CollectionScheduleMode::Interval => {
            format!(
                "采集计划：每 {} 分钟自动采集并上传",
                schedule.interval_minutes.max(1)
            )
        }
        CollectionScheduleMode::Daily => {
            format!("采集计划：每天 {} 自动采集并上传", schedule.daily_time)
        }
    }
}

pub(super) fn next_schedule_at(
    schedule: &CollectionSchedule,
    now: chrono::DateTime<Local>,
) -> Option<chrono::DateTime<Local>> {
    match schedule.mode {
        CollectionScheduleMode::Disabled => None,
        CollectionScheduleMode::Interval => {
            Some(now + TimeDelta::minutes(i64::from(schedule.interval_minutes.max(1))))
        }
        CollectionScheduleMode::Daily => {
            let time = NaiveTime::parse_from_str(&schedule.daily_time, "%H:%M").ok()?;
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

pub(super) struct ScheduledCollectionOutcome {
    pub(super) summary: CollectionSummary,
    pub(super) completed_at: Option<String>,
}

pub(super) fn scheduled_collect_and_upload(
    state: &AppState,
    config: &EnterpriseCollectorConfig,
    force_upload: bool,
    force_full_scan: bool,
) -> Result<ScheduledCollectionOutcome, CommandError> {
    ensure_collector_config_fresh(config)?;
    let candidates = discover_local_sources();
    let media_options = MediaCollectionOptions {
        include_files: config.include_files,
        include_images: config.include_images,
    };
    let (candidate, key) = choose_collectable_source(candidates, &mut |source| {
        resolve_collection_key(
            source,
            source.databases.iter().any(|database| database.encrypted),
            &collector_data_root()?,
        )
    })?;
    let started_ms = Utc::now().timestamp_millis();
    let progress_path = collector_incremental::progress_path(
        &collector_data_root()?,
        &config.collector_id,
        &candidate.source_id,
    );
    let mut incremental = collector_incremental::IncrementalCollector::open(
        &progress_path,
        collector_incremental::scope_hash(config)?,
    )?;
    // Offline exports retain the complete local result. Their existing meaning
    // must not change merely because scheduled uploads become incremental.
    let plan = incremental.plan(
        started_ms,
        force_full_scan || config.offline_export_enabled,
        config.data_redaction,
    );
    let root = collector_work_root("scheduled")?;
    let result = (|| {
        let export = collect_candidate_with_options_since(
            candidate,
            root.join("work"),
            key.as_ref(),
            CollectionAuthorization {
                notice_version: LOCAL_NOTICE_VERSION.into(),
                notice_displayed_at: Utc::now(),
            },
            CollectionReadOptions {
                media_options,
                since_unix_ms: plan.read_since_unix_ms(),
                ..Default::default()
            },
        )?;
        // Preserve redaction context before removing unchanged or older messages.
        let upload_export = if config.data_redaction {
            redact_sensitive_export(export.clone())?
        } else {
            export.clone()
        };
        let outcome =
            incremental.upload(upload_export, &plan, started_ms, force_upload, |delta| {
                let package = encrypt_enterprise_export(delta, config)?;
                post_enterprise_package(
                    &config.upload_url,
                    config.upload_token.as_bytes(),
                    &package,
                )
            })?;
        if let Ok(mut status) = state.schedule_status.lock() {
            status.uploaded_count = outcome.uploaded_count;
            status.last_success_at = Some(outcome.last_upload_at);
        }
        let summary = CollectionSummary::from(&export);
        *state.latest_export.lock().map_err(|_| internal_error())? = Some(Arc::new(export));
        Ok::<_, CommandError>(ScheduledCollectionOutcome {
            summary,
            completed_at: outcome.completed_at,
        })
    })();
    let _ = fs::remove_dir_all(root);
    result
}
