use crate::{
    ACCESS_TOKEN_FILE, ALLOW_REMOTE_ENV, AppState, DATA_ROOT_ENV, DEFAULT_SERVER_PORT, LISTEN_ENV,
    LocalCollectionMediaSink, LocalCollectionProgress, LocalCollectionProgressReporter,
    LocalCollector, archive_summary, archive_updates, cancel_export, collect_local,
    collected_users, collector_heartbeat, configured_private_listen_address, count_messages,
    create_export_job, create_local_collection_plan, create_local_export, default_data_root,
    delete_local_collection_plan, diagnostics, embedded_web, export_status,
    find_collector_template, generate_collector, get_collection_schedules, get_collection_targets,
    get_collector, get_enterprise_config, get_media, health, import_enterprise, import_json,
    list_collectors, list_conversation_participants, list_conversations, list_messages,
    load_enterprise_config, load_or_generate_access_token, local_collection_progress,
    message_position, open_collector_directory, open_export_directory, open_media,
    regenerate_access_token, request_collector_collection, revoke_collector, rotate_enterprise_key,
    run_local_collection_scheduler, search_messages, start_export_job, update_access_token,
    update_collector, update_enterprise_config, update_local_collection_plan, update_super_admin,
};
use archive_domain::ClientExportV1;
use archive_store::ArchiveStore;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::post;
use sha2::Digest;
use sha2::Sha256;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::watch;
use zeroize::Zeroizing;

struct PreparedServer {
    address: SocketAddr,
    token: Zeroizing<String>,
    router: Router,
    state: AppState,
}

pub struct DesktopServer {
    page_url: String,
    shutdown_sender: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    worker: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

impl DesktopServer {
    pub fn page_url(&self) -> &str {
        &self.page_url
    }

    pub fn is_running(&self) -> bool {
        self.worker
            .lock()
            .ok()
            .and_then(|worker| worker.as_ref().map(|worker| !worker.is_finished()))
            .unwrap_or(false)
    }

    pub fn shutdown(&self) {
        if let Ok(mut sender) = self.shutdown_sender.lock()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(());
        }
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for DesktopServer {
    fn drop(&mut self) {
        if let Ok(sender) = self.shutdown_sender.get_mut()
            && let Some(sender) = sender.take()
        {
            let _ = sender.send(());
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

pub fn start_desktop_server_with_local_collector<F>(collector: F) -> Result<DesktopServer, String>
where
    F: Fn(
            bool,
            bool,
            Option<i64>,
            Option<String>,
            LocalCollectionProgressReporter,
            LocalCollectionMediaSink,
        ) -> Result<ClientExportV1, String>
        + Send
        + Sync
        + 'static,
{
    start_desktop_server_inner(Some(Arc::new(collector)))
}

fn start_desktop_server_inner(
    local_collector: Option<LocalCollector>,
) -> Result<DesktopServer, String> {
    let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
    let collector_template = find_collector_template();
    let prepared = prepare_server(collector_template, local_collector)?;
    let page_url = format!(
        "http://{}/#token={}",
        prepared.address,
        prepared.token.as_str()
    );
    let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("wecom-archive-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "无法初始化本机服务运行时。".to_owned())?;
            runtime.block_on(serve(prepared, shutdown_receiver, Some(ready_sender)))
        })
        .map_err(|_| "无法创建本机服务线程。".to_owned())?;

    match ready_receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(DesktopServer {
            page_url,
            shutdown_sender: Mutex::new(Some(shutdown_sender)),
            worker: Mutex::new(Some(worker)),
        }),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => {
            let _ = shutdown_sender.send(());
            let _ = worker.join();
            Err("本机归档服务启动超时。".into())
        }
    }
}

fn prepare_server(
    collector_template: Option<PathBuf>,
    local_collector: Option<LocalCollector>,
) -> Result<PreparedServer, String> {
    let data_root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(default_data_root);
    std::fs::create_dir_all(&data_root)
        .map_err(|error| format!("服务端数据目录不可用（{}），请检查目录权限。", error.kind()))?;
    diagnostics::init(&data_root);
    diagnostics::write("server_prepare_started", "");
    let access_token_path = data_root.join(ACCESS_TOKEN_FILE);
    let token = load_or_generate_access_token(&access_token_path)
        .map_err(|_| "服务端访问令牌初始化失败，请检查 serverData 目录权限。".to_owned())?;
    let enterprise_config = load_enterprise_config(&data_root);
    diagnostics::set_level(&enterprise_config.server_log_level);
    let (address, automatic_remote) = std::env::var(LISTEN_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(|address| (address, false))
        .or_else(|| {
            configured_private_listen_address(&enterprise_config.upload_url)
                .map(|address| (address, true))
        })
        .unwrap_or((
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_SERVER_PORT),
            false,
        ));
    diagnostics::write(
        "server_config_ready",
        &format!(
            "network_mode={};port={}",
            if address.ip().is_loopback() {
                "loopback_only"
            } else {
                "configured_remote"
            },
            address.port()
        ),
    );
    if !address.ip().is_loopback()
        && !automatic_remote
        && !std::env::var(ALLOW_REMOTE_ENV)
            .ok()
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return Err(format!(
            "远程采集端需要显式启用 {ALLOW_REMOTE_ENV}=true 后才能监听非回环地址。"
        ));
    }

    let archive_path = Arc::new(data_root.join("archive.db"));
    let initial_revision = open_archive_for_startup(&archive_path)?;
    let (archive_update, _) = watch::channel(initial_revision);
    let state = AppState {
        data_root: Arc::new(data_root.clone()),
        archive_path,
        token_sha256: Arc::new(Mutex::new(Sha256::digest(token.as_bytes()).into())),
        access_token_path: Arc::new(access_token_path),
        enterprise_config: Arc::new(Mutex::new(enterprise_config)),
        collector_template: collector_template.map(Arc::new),
        local_collector,
        local_collection_progress: Arc::new(Mutex::new(LocalCollectionProgress::default())),
        local_collection_running: Arc::new(AtomicBool::new(false)),
        archive_update,
        network_mode: if address.ip().is_loopback() {
            "loopback_only"
        } else {
            "configured_remote"
        },
        listen_address: address.to_string(),
        export_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };

    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route(
            "/api/v1/collectors/{collector_id}/software-update",
            post(crate::request_collector_update),
        )
        .route(
            "/api/v1/collectors/{collector_id}/software-update/{request_id}",
            get(crate::download_collector_update),
        )
        .route("/api/v1/imports/json", post(import_json))
        .route("/api/v1/imports/enterprise", post(import_enterprise))
        .route("/api/v1/collections/local", post(collect_local))
        .route(
            "/api/v1/collections/local/progress",
            get(local_collection_progress),
        )
        .route(
            "/api/v1/collection-schedules",
            get(get_collection_schedules).post(create_local_collection_plan),
        )
        .route(
            "/api/v1/collection-schedules/{plan_id}",
            axum::routing::put(update_local_collection_plan).delete(delete_local_collection_plan),
        )
        .route("/api/v1/server/access-token", post(update_access_token))
        .route(
            "/api/v1/server/access-token/regenerate",
            post(regenerate_access_token),
        )
        .route(
            "/api/v1/enterprise/config",
            get(get_enterprise_config).put(update_enterprise_config),
        )
        .route(
            "/api/v1/settings/super-admin",
            axum::routing::put(update_super_admin),
        )
        .route("/api/v1/enterprise/key/rotate", post(rotate_enterprise_key))
        .route(
            "/api/v1/enterprise/collectors",
            get(list_collectors).post(generate_collector),
        )
        .route("/api/v1/collection-targets", get(get_collection_targets))
        .route(
            "/api/v1/collectors/{collector_id}",
            get(get_collector).put(update_collector),
        )
        .route(
            "/api/v1/collectors/{collector_id}/heartbeat",
            post(collector_heartbeat).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/collectors/{collector_id}/revoke",
            post(revoke_collector),
        )
        .route(
            "/api/v1/collectors/{collector_id}/collect",
            post(request_collector_collection),
        )
        .route(
            "/api/v1/enterprise/collectors/open-directory",
            post(open_collector_directory),
        )
        .route("/api/v1/archive/summary", get(archive_summary))
        .route("/api/v1/archive/updates", get(archive_updates))
        .route("/api/v1/archive/collected-users", get(collected_users))
        .route("/api/v1/conversations", get(list_conversations))
        .route(
            "/api/v1/conversations/{conversation_id}/participants",
            get(list_conversation_participants),
        )
        .route("/api/v1/media/{content_hash}", get(get_media))
        .route("/api/v1/media/{content_hash}/open", post(open_media))
        .route("/api/v1/messages/count", get(count_messages))
        .route("/api/v1/messages", get(list_messages))
        .route("/api/v1/search/messages", get(search_messages))
        .route(
            "/api/v1/messages/{stable_message_id}/position",
            get(message_position),
        )
        .route(
            "/api/v1/exports",
            post(create_export_job).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/exports/start",
            post(start_export_job).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/exports/local",
            post(create_local_export).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/exports/open-directory",
            post(open_export_directory),
        )
        .route("/api/v1/exports/{export_id}", get(export_status))
        .route("/api/v1/exports/{export_id}/cancel", post(cancel_export))
        .layer(DefaultBodyLimit::disable())
        .with_state(state.clone())
        .fallback(embedded_web);

    Ok(PreparedServer {
        address,
        token,
        router: app,
        state,
    })
}

fn open_archive_for_startup(path: &Path) -> Result<u64, String> {
    let mut last_error = None;
    for attempt in 0..45 {
        match ArchiveStore::open_with_busy_timeout(path, Duration::from_secs(1))
            .and_then(|store| store.summary())
        {
            Ok(summary) => {
                return Ok(summary.revision);
            }
            Err(error) => {
                let detail = error.to_string();
                let locked = detail.contains("database is locked")
                    || detail.contains("database table is locked");
                last_error = Some(detail);
                if !locked || attempt == 44 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(format!(
        "归档数据库初始化失败：{}。如果刚刚中断了采集，请等待上一次采集完全退出后重试。",
        last_error.unwrap_or_else(|| "未知数据库错误".into())
    ))
}

async fn serve(
    prepared: PreparedServer,
    shutdown_receiver: std::sync::mpsc::Receiver<()>,
    ready_sender: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
) -> Result<(), String> {
    let listener = match tokio::net::TcpListener::bind(prepared.address).await {
        Ok(listener) => listener,
        Err(error) => {
            diagnostics::write("server_bind_failed", &format!("kind={}", error.kind()));
            let message = format!(
                "服务端启动失败（{}）。请确认 {} 端口未被占用。",
                error.kind(),
                prepared.address.port()
            );
            if let Some(sender) = ready_sender {
                let _ = sender.send(Err(message.clone()));
            }
            return Err(message);
        }
    };
    if let Some(sender) = ready_sender {
        let _ = sender.send(Ok(()));
    }
    diagnostics::write("server_ready", &format!("port={}", prepared.address.port()));
    let scheduler = tokio::spawn(run_local_collection_scheduler(prepared.state.clone()));
    axum::serve(
        listener,
        prepared
            .router
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::task::spawn_blocking(move || shutdown_receiver.recv()).await;
    })
    .await
    .map_err(|_| "本机归档服务异常退出。".to_owned())?;
    scheduler.abort();
    diagnostics::write("server_shutdown", "");
    Ok(())
}
