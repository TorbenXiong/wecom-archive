use std::sync::Mutex;

static EXPORT_FILE_LOCK: Mutex<()> = Mutex::new(());

mod diagnostics;

const DATA_ROOT_ENV: &str = "WECOM_ARCHIVE_SERVER_DATA";
const LISTEN_ENV: &str = "WECOM_ARCHIVE_SERVER_LISTEN";
const ALLOW_REMOTE_ENV: &str = "WECOM_ARCHIVE_SERVER_ALLOW_REMOTE";
const DEFAULT_SERVER_PORT: u16 = 9812;
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:9812";
const MAX_CLIENT_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ENTERPRISE_CONFIG_FILE: &str = "enterprise-config.json";
const ACCESS_TOKEN_FILE: &str = "access-token.dpapi";
const DEFAULT_COLLECTION_NOTICE: &str = "加密传输本机企业微信聊天记录到服务端";
const LEGACY_COLLECTION_NOTICE: &str = "仅处理您有权归档的数据。";
const MIN_ENTERPRISE_KEY_ID_LEN: usize = 24;
const MAX_ENTERPRISE_KEY_ID_LEN: usize = 128;

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

mod api;
mod auth;
mod collector;
mod config;
mod error;
mod files;
mod runtime;
mod service;
mod state;

use api::*;
use auth::*;
use collector::*;
use config::*;
use error::*;
use files::*;
pub use runtime::{DesktopServer, start_desktop_server_with_local_collector};
use service::*;
pub use service::{ArchiveExportOptions, ArchiveExportSource};
use state::*;
pub use state::{
    LocalCollectionMediaSink, LocalCollectionProgress, LocalCollectionProgressReporter,
};

#[cfg(test)]
mod tests;
