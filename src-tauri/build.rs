fn main() {
    let frontend_dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist")
        .join("client");
    println!("cargo:rerun-if-changed={}", frontend_dist.display());
    if frontend_dist.is_dir() {
        watch_frontend_files(&frontend_dist);
    }
    tauri_build::build();
    // Cargo test executables lack Tauri's application resources. The linker
    // generates a sidecar manifest so Wry can load Common Controls v6 in tests.
    // Keep it external to avoid duplicating the application's embedded manifest.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/MANIFEST");
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}

fn watch_frontend_files(directory: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                watch_frontend_files(&path);
            } else if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
