use tauri::Manager;
use wecom_archive_server::maintenance as archive;

pub fn capability() -> tauri::ipc::CapabilityBuilder {
    tauri::ipc::CapabilityBuilder::new("maintenance-window")
        .local(true)
        .window("maintenance")
        .permission("core:default")
        .permission("allow-maintenance-status")
        .permission("allow-maintenance-create-backup")
        .permission("allow-maintenance-verify-backup")
        .permission("allow-maintenance-restore-backup")
        .permission("allow-maintenance-activate")
}

pub fn run(backup_location: bool) {
    tauri::Builder::default()
        .setup(move |app| {
            app.add_capability(capability())?;
            tauri::WebviewWindowBuilder::new(
                app,
                "maintenance",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .initialization_script(format!("window.__WECOM_MAINTENANCE__ = true; window.__WECOM_BACKUP_LOCATION__ = {backup_location};"))
            .title("企业微信记录归档 · 备份与恢复")
            .inner_size(850.0, 720.0)
            .min_inner_size(680.0, 600.0)
            .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            maintenance_status,
            maintenance_create_backup,
            maintenance_verify_backup,
            maintenance_restore_backup,
            maintenance_activate
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|_| super::exit_with_error("备份与恢复窗口初始化失败。"));
}

fn pick() -> Result<Option<std::path::PathBuf>, String> {
    source_windows::dialog::pick_folder(None).map_err(|_| "无法选择本机目录，请重试。".into())
}

#[tauri::command]
async fn maintenance_status() -> Result<bool, String> {
    archive::restore_pending()
}

#[tauri::command]
async fn maintenance_create_backup() -> Result<Option<archive::MaintenanceResult>, String> {
    if archive::backup_location()? {
        return Err("当前程序位于备份目录，请先恢复到新目录。".into());
    }
    tauri::async_runtime::spawn_blocking(|| {
        let Some(parent) = pick()? else {
            return Ok(None);
        };
        let executable = std::env::current_exe().map_err(|_| "无法定位当前程序。")?;
        archive::create_backup(&archive::data_root(), &executable, &parent).map(Some)
    })
    .await
    .map_err(|_| "备份任务未完成，原数据已保留。".to_owned())?
}

#[tauri::command]
async fn maintenance_verify_backup() -> Result<Option<archive::MaintenanceResult>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(backup) = pick()? else {
            return Ok(None);
        };
        archive::verify_backup(&backup).map(Some)
    })
    .await
    .map_err(|_| "备份校验任务未完成。".to_owned())?
}

#[tauri::command]
async fn maintenance_restore_backup() -> Result<Option<archive::MaintenanceResult>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(backup) = pick()? else {
            return Ok(None);
        };
        // Validate before asking for a destination; an invalid backup must not
        // create even a staging directory there.
        archive::verify_backup(&backup)?;
        let Some(parent) = pick()? else {
            return Ok(None);
        };
        archive::restore_backup(&backup, &parent).map(Some)
    })
    .await
    .map_err(|_| "恢复任务未完成，原数据已保留。".to_owned())?
}

#[tauri::command]
async fn maintenance_activate(app: tauri::AppHandle) -> Result<(), String> {
    if archive::backup_location()? {
        return Err("当前程序位于备份目录，请先恢复到新目录。".into());
    }
    let root = archive::data_root();
    let activate_root = root.clone();
    tauri::async_runtime::spawn_blocking(move || archive::activate_restored(&activate_root))
        .await
        .map_err(|_| "恢复确认任务未完成。".to_owned())??;
    let mut command =
        std::process::Command::new(std::env::current_exe().map_err(|_| "无法定位当前程序。")?);
    command
        .env("WECOM_ARCHIVE_SERVER_DATA", root)
        .env("WECOM_ARCHIVE_SERVER_LISTEN", "127.0.0.1:9812")
        .env("WECOM_ARCHIVE_SERVER_ALLOW_REMOTE", "false");
    source_windows::updater::hide(&mut command);
    command
        .spawn()
        .map_err(|_| "已暂停计划和采集端，请手动重新打开恢复后的程序。")?;
    app.exit(0);
    Ok(())
}
