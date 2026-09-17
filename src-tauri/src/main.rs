#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

use libsqlite3_sys as _;

fn main() {
    if std::env::args().any(|argument| argument == "--key-probe") {
        std::process::exit(wecom_archive_desktop::run_key_probe());
    }
    if is_collector() {
        wecom_archive_desktop::run();
        return;
    }

    let server = Arc::new(
        wecom_archive_server::start_desktop_server_with_local_collector(
            wecom_archive_desktop::collect_local_export_with_progress,
        )
        .unwrap_or_else(|message| exit_with_error(&message)),
    );
    let page_url = tauri::Url::parse(server.page_url())
        .unwrap_or_else(|_| exit_with_error("工作台页面地址无效。"));
    let setup_server = Arc::clone(&server);
    let exit_server = Arc::clone(&server);

    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(page_url.clone()),
            )
            .title("企业微信记录归档")
            .inner_size(1380.0, 820.0)
            .min_inner_size(980.0, 640.0)
            .resizable(true)
            .center()
            .build()?;

            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                while setup_server.is_running() {
                    std::thread::sleep(Duration::from_millis(300));
                }
                app_handle.exit(0);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![exit_workspace])
        .build(tauri::generate_context!())
        .unwrap_or_else(|_| exit_with_error("桌面工作台初始化失败。"))
        .run(move |_app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                exit_server.shutdown();
            }
        });
}

#[tauri::command]
fn exit_workspace(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        app.exit(0);
    });
}

fn is_collector() -> bool {
    std::env::current_exe().ok().is_some_and(|executable| {
        archive_transfer::read_enterprise_collector_config(&executable).is_ok()
    })
}

fn exit_with_error(message: &str) -> ! {
    show_error(message);
    std::process::exit(1)
}

#[cfg(windows)]
fn show_error(message: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    let text: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = std::ffi::OsStr::new("企业微信记录归档")
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, title: *const u16, kind: u32) -> i32;
    }
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}
