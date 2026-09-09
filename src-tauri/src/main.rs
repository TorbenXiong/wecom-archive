#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|argument| argument == "--key-probe") {
        std::process::exit(wecom_archive_desktop::run_key_probe());
    }
    wecom_archive_desktop::run();
}
