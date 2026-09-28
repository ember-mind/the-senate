//! Embeds every file under `world/` into the binary at compile time as a
//! URL-path -> (content-type, bytes) table, so `senate world` serves the
//! browser app without ever reading the filesystem at runtime. Kept
//! dependency-free: it only uses `std`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let world_dir = Path::new(&manifest_dir).join("world");
    println!("cargo:rerun-if-changed=world");

    let mut entries = Vec::new();
    if world_dir.is_dir() {
        collect_assets(&world_dir, &world_dir, &mut entries);
    }
    entries.sort();

    let mut generated = String::from("pub static ASSETS: &[(&str, &str, &[u8])] = &[\n");
    for (url_path, content_type, absolute_path) in &entries {
        writeln!(
            generated,
            "    ({url_path:?}, {content_type:?}, include_bytes!({absolute_path:?})),"
        )
        .expect("String writes never fail");
    }
    generated.push_str("];\n");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let destination = PathBuf::from(out_dir).join("world_assets.rs");
    fs::write(&destination, generated).expect("write generated asset table");
}

/// Recursively lists files under `dir` (a subtree of `root`) as
/// `(url_path, content_type, absolute_path)` triples.
fn collect_assets(root: &Path, dir: &Path, out: &mut Vec<(String, String, String)>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            // Blender sources that build the assets are not served.
            if entry_path
                .file_name()
                .is_some_and(|name| name == "assets-src")
            {
                continue;
            }
            collect_assets(root, &entry_path, out);
            continue;
        }
        let Ok(relative) = entry_path.strip_prefix(root) else {
            continue;
        };
        let mut url_path = String::from("/");
        url_path.push_str(&relative.to_string_lossy().replace('\\', "/"));
        let content_type = content_type_for(&entry_path).to_owned();
        out.push((
            url_path,
            content_type,
            entry_path.to_string_lossy().into_owned(),
        ));
    }
}

/// Content type for one embedded asset, chosen from its extension only.
fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("glb") => "model/gltf-binary",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
