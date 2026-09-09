use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let web_root = manifest.join("..").join("..").join("dist").join("server");
    println!("cargo:rerun-if-changed={}", web_root.display());
    if !web_root.join("index.html").is_file() {
        println!("cargo:warning=server web assets are missing; embedding the build hint page");
        let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("embedded_web.rs");
        fs::write(
            output,
            "pub fn embedded_asset(path: &str) -> Option<(&'static [u8], &'static str)> { if path == \"/index.html\" { Some((include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/fallback.html\")), \"text/html; charset=utf-8\")) } else { None } }\n",
        )
        .unwrap();
        return;
    }

    let mut files = Vec::new();
    collect_files(&web_root, &web_root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut generated = String::from(
        "pub fn embedded_asset(path: &str) -> Option<(&'static [u8], &'static str)> {\n    match path {\n",
    );
    for (route, file) in files {
        let absolute = file.canonicalize().unwrap();
        generated.push_str(&format!(
            "        {:?} => Some((include_bytes!({:?}), {:?})),\n",
            route,
            absolute.to_string_lossy(),
            mime_type(&route),
        ));
    }
    generated.push_str("        _ => None,\n    }\n}\n");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("embedded_web.rs");
    fs::write(output, generated).unwrap();
}

fn collect_files(root: &Path, directory: &Path, output: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, output);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            output.push((format!("/{relative}"), path));
        }
    }
}

fn mime_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
