//! Desktop entry points; collector internals remain private to this crate.

mod collection;
mod collector_config;
mod collector_control;
mod collector_http;
mod collector_incremental;
mod collector_runtime;
mod collector_schedule;
mod collector_transfer;
mod collector_update;
mod commands;
mod diagnostics;
mod paths;
mod source_directory;
mod source_keys;
mod source_media;
mod source_messages;
mod source_protobuf;
mod source_sql;
mod sqlite3mc;
mod state;
mod update;

pub use collection::{collect_local_export, collect_local_export_with_progress};
pub use collector_runtime::run;
pub use source_keys::run_key_probe;
pub use update::{UpdateInfo, acknowledge_startup, run_helper as run_update_helper};

pub fn check_for_update() -> Result<Option<UpdateInfo>, String> {
    update::check()
}

pub fn install_update(version: &str) -> Result<(), String> {
    update::install(version)
}

#[cfg(test)]
mod tests;
