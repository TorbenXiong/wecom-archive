use crate::collector_config::{EnterpriseCollectorConfig, load_enterprise_collector_config};
use crate::collector_schedule::{
    collector_scheduler, describe_collector_schedule, format_status_time,
};
use crate::paths::collector_data_root;
use crate::state::AppState;
use crate::{commands, diagnostics, update};
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    diagnostics::init();
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            let collector_config = load_enterprise_collector_config().ok();
            if let Some(config) = collector_config.as_ref() {
                diagnostics::set_level(&config.collector_log_level);
            }
            let hidden_mode_enabled = collector_config
                .as_ref()
                .is_some_and(|config| config.hidden_mode_enabled);
            diagnostics::write(
                "window_setup",
                &format!("config_loaded={};hidden_mode={hidden_mode_enabled}", collector_config.is_some()),
            );
            // The workspace and generated collectors share one Tauri identifier, but
            // serve different pages. WebView2 must not reuse the workspace browser
            // process (or another collector's process) for the collector window.
            let collector_profile = collector_config
                .as_ref()
                .map(|config| hex::encode(Sha256::digest(config.collector_id.as_bytes())))
                .unwrap_or_else(|| "unconfigured".into());
            let webview_data_dir = collector_data_root()
                .map_err(|_| std::io::Error::other("collectorData path unavailable"))?
                .join("webview")
                .join(collector_profile);
            let _collector_window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .data_directory(webview_data_dir)
            .initialization_script(
                r#"window.addEventListener('DOMContentLoaded', () => {
                    window.setTimeout(() => {
                        const root = document.getElementById('root');
                        if (root && !root.hasChildNodes()) {
                            root.textContent = '采集端界面加载失败，请关闭后重新打开采集端。';
                            root.style.cssText = 'padding:24px;font:14px/1.6 sans-serif;color:#9b2c2c';
                        }
                    }, 6000);
                });"#,
            )
            .title("企业微信采集端")
            .inner_size(440.0, 360.0)
            .min_inner_size(420.0, 340.0)
            .resizable(true)
            .center()
            // Create hidden-mode collectors without ever presenting the native
            // window. The page-load callback below keeps the window hidden as
            // well, while tray actions can still show it when applicable.
            .visible(!hidden_mode_enabled)
            .skip_taskbar(hidden_mode_enabled)
            .on_page_load(move |window, payload| {
                diagnostics::verbose(
                    "page_load",
                    &format!("event={:?};hidden_mode={hidden_mode_enabled}", payload.event()),
                );
                if hidden_mode_enabled
                    && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                {
                    diagnostics::write("page_load_hide", "hidden_mode=true");
                    let _ = window.set_skip_taskbar(true);
                    let _ = window.hide();
                }
            })
            .icon(collector_tray_icon())?
            .build()?;
            let app_state = app.state::<AppState>().inner().clone();
            if let Ok(mut tray_app) = app_state.tray_app.lock() {
                *tray_app = Some(app.handle().clone());
            }
            if !hidden_mode_enabled
                && let Some(config) = collector_config.as_ref()
            {
                create_collector_tray(app.handle(), config)?;
            }
            let tray_app = app.handle().clone();
            let tray_state = app_state.clone();
            std::thread::spawn(move || {
                while !tray_state.scheduler_shutdown.load(Ordering::Acquire) {
                    let tip = tray_state.schedule_status.lock().ok().map(|status| {
                        let last = status
                            .last_success_at
                            .as_deref()
                            .map(format_status_time)
                            .unwrap_or_else(|| "尚未成功".into());
                        let next = status.next_run_at.as_deref().unwrap_or("计算中");
                        let state = if status.running {
                            "上传中"
                        } else if status.last_error.is_some() {
                            "最近失败"
                        } else {
                            "运行正常"
                        };
                        let schedule_tip = load_enterprise_collector_config()
                            .map(|config| describe_collector_schedule(&config.collector_schedule))
                            .unwrap_or_else(|_| "采集计划加载中".into());
                        format!("{}\n状态：{}\n最近成功：{}\n下次执行：{}\n已上传：{} 条", schedule_tip, state, last, next, status.uploaded_count)
                    });
                    if let (Some(tray), Some(tip)) = (tray_app.tray_by_id("collector-tray"), tip) {
                        let _ = tray.set_tooltip(Some(tip));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
            let scheduler_state = app.state::<AppState>().inner().clone();
            std::thread::Builder::new()
                .name("wecom-archive-collector-scheduler".into())
                .spawn(move || collector_scheduler(scheduler_state))
                .map_err(|_| "无法创建采集端后台任务。")?;
            update::acknowledge_startup();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::sync_collector_config,
            commands::get_collector_schedule_status,
            commands::hide_collector_window,
            commands::discover_sources,
            commands::collect_source,
            commands::upload_latest_enterprise,
            commands::export_latest_enterprise,
            commands::open_offline_export_directory,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                diagnostics::write("window_close_requested", "hide_for_tray=true");
                api.prevent_close();
                let _ = window.hide();
            }
            if matches!(event, tauri::WindowEvent::Destroyed)
                && let Some(state) = window.try_state::<AppState>()
            {
                state.scheduler_shutdown.store(true, Ordering::Release);
            }
        })
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}

pub(super) fn create_collector_tray(
    app: &tauri::AppHandle,
    config: &EnterpriseCollectorConfig,
) -> tauri::Result<()> {
    if app.tray_by_id("collector-tray").is_some() {
        return Ok(());
    }
    let open = MenuItem::with_id(app, "open", "打开采集端", true, None::<&str>)?;
    let export = MenuItem::with_id(app, "export", "离线导出…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出采集端", true, None::<&str>)?;
    let offline_export_enabled = config.offline_export_enabled;
    let menu = if offline_export_enabled {
        MenuBuilder::new(app)
            .items(&[&open, &export, &quit])
            .build()?
    } else {
        MenuBuilder::new(app).items(&[&open, &quit]).build()?
    };
    let schedule_tip = describe_collector_schedule(&config.collector_schedule);
    let mut tray = TrayIconBuilder::with_id("collector-tray")
        .menu(&menu)
        .tooltip(schedule_tip)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => open_collector_window(app, false),
            "export" if offline_export_enabled => open_collector_window(app, true),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                open_collector_window(tray.app_handle(), offline_export_enabled);
            }
        });
    tray = tray.icon(collector_tray_icon());
    tray.build(app)?;
    Ok(())
}

pub(super) fn apply_collector_visibility(
    app: &tauri::AppHandle,
    config: &EnterpriseCollectorConfig,
) -> tauri::Result<()> {
    if config.hidden_mode_enabled {
        let _ = app.remove_tray_by_id("collector-tray");
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_skip_taskbar(true);
            let _ = window.hide();
        }
    } else {
        create_collector_tray(app, config)?;
    }
    Ok(())
}

pub(super) fn open_collector_window(app: &tauri::AppHandle, request_export: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_skip_taskbar(false);
        let _ = window.show();
        let _ = window.set_focus();
        if request_export {
            let _ = window.emit("collector:open-offline-export", ());
        }
    }
}

pub(super) fn collector_tray_icon() -> tauri::image::Image<'static> {
    let mut pixels = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32_u32 {
        for x in 0..32_u32 {
            let paper = (8..=23).contains(&x) && (6..=26).contains(&y);
            let line = paper && (y == 13 || y == 17 || y == 21) && (11..=21).contains(&x);
            let (r, g, b, a) = if paper && !line {
                (255, 255, 255, 255)
            } else {
                (22, 128, 60, 255)
            };
            pixels.extend([r, g, b, a]);
        }
    }
    tauri::image::Image::new_owned(pixels, 32, 32)
}
