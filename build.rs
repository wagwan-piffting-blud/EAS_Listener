//! Bakes the dashboard's files into the binary, so a release is one executable and a release
//! build does not depend on `web_server/` being next to it at runtime. A copy on disk still wins
//! when there is one -- see `src/web_assets.rs`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Written by the listener itself or by tooling, and gitignored, so they are not part of the
/// dashboard as the repository ships it. `web_config.json` also carries the local configuration,
/// which has no business inside a published binary.
const NOT_SHIPPED: &[&str] = &["web_config.json", "image_info.json"];

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let web_root = manifest_dir.join("web_server");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=web_server");

    let mut files = Vec::new();
    collect(&web_root, &web_root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut generated = String::new();
    generated.push_str(
        "/// Every dashboard file, sorted by path so a lookup can binary search.\n\
         pub static EMBEDDED: &[(&str, &str, &[u8])] = &[\n",
    );
    for (route, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(
            generated,
            "    ({:?}, {:?}, include_bytes!({:?})),",
            route,
            content_type(route),
            path.display().to_string().replace('\\', "/")
        )
        .expect("write generated table");
    }
    generated.push_str("];\n");

    let out_path = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    fs::write(&out_path, generated).expect("write web_assets.rs");
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if NOT_SHIPPED.contains(&name) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        // Not canonicalized: on Windows that returns a \\?\ path, which `include_bytes!` will not
        // take. `web_server` is reached from CARGO_MANIFEST_DIR, so this is already absolute.
        files.push((relative.to_string_lossy().replace('\\', "/"), path));
    }
}

fn content_type(route: &str) -> &'static str {
    match route.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("webmanifest") => "application/manifest+json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
