fn main() {
    let frontend_dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist")
        .join("client");
    println!("cargo:rerun-if-changed={}", frontend_dist.display());
    if frontend_dist.is_dir() {
        watch_frontend_files(&frontend_dist);
    }
    tauri_build::build()
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
