use crate::EnterpriseConfig;
use archive_domain::ClientExportV1;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct AppState {
    pub(super) data_root: Arc<PathBuf>,
    pub(super) archive_path: Arc<PathBuf>,
    pub(super) token_sha256: Arc<Mutex<[u8; 32]>>,
    pub(super) access_token_path: Arc<PathBuf>,
    pub(super) enterprise_config: Arc<Mutex<EnterpriseConfig>>,
    pub(super) collector_template: Option<Arc<PathBuf>>,
    pub(super) local_collector: Option<LocalCollector>,
    pub(super) local_collection_progress: Arc<Mutex<LocalCollectionProgress>>,
    pub(super) local_collection_running: Arc<AtomicBool>,
    pub(super) archive_update: watch::Sender<u64>,
    pub(super) network_mode: &'static str,
}

pub type LocalCollectionProgressReporter = Arc<dyn Fn(u8, &str, &str) + Send + Sync>;
pub type LocalCollectionMediaSink =
    Arc<dyn Fn(&Path, &str, u64) -> Result<(), String> + Send + Sync>;
pub(super) type LocalCollector = Arc<
    dyn Fn(
            bool,
            bool,
            Option<i64>,
            Option<String>,
            LocalCollectionProgressReporter,
            LocalCollectionMediaSink,
        ) -> Result<ClientExportV1, String>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCollectionProgress {
    pub(super) running: bool,
    pub(super) percent: u8,
    pub(super) stage: String,
    pub(super) detail: String,
}

impl Default for LocalCollectionProgress {
    fn default() -> Self {
        Self {
            running: false,
            percent: 0,
            stage: "等待开始".into(),
            detail: "选择采集范围后开始处理。".into(),
        }
    }
}
