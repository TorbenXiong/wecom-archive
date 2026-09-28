#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

use libsqlite3_sys as _;

fn main() {
    if std::env::args().any(|argument| argument == "--key-probe") {
        std::process::exit(wecom_archive_desktop::run_key_probe());
    }
    if is_collector() {
        #[cfg(windows)]
        let _instance_guard = match collector_instance::acquire() {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                collector_instance::show_already_running();
                return;
            }
            Err(_) => exit_with_error("无法确认采集端是否已启动。"),
        };
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
            .inner_size(1480.0, 900.0)
            .min_inner_size(1120.0, 700.0)
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

#[cfg(windows)]
mod collector_instance {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(
            attributes: *const c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct Guard(*mut c_void);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn acquire() -> Result<Option<Guard>, ()> {
        let executable = std::env::current_exe().map_err(|_| ())?;
        let bytes =
            archive_transfer::read_enterprise_collector_config(&executable).map_err(|_| ())?;
        let config: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let id = config
            .get("collectorId")
            .and_then(|value| value.as_str())
            .ok_or(())?;
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(());
        }
        let name: Vec<u16> = OsStr::new(&format!("Local\\WeComArchiveCollector-{id}"))
            .encode_wide()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(());
        }
        if unsafe { GetLastError() } == 183 {
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Guard(handle)))
    }

    pub fn show_already_running() {
        super::show_notice("采集已启动，请从系统托盘打开采集端。")
    }
}

#[cfg(windows)]
fn show_notice(message: &str) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    let text: Vec<u16> = std::ffi::OsStr::new(message)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = std::ffi::OsStr::new("企业微信采集端")
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, title: *const u16, kind: u32) -> i32;
    }
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x40);
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}
