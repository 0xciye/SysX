use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            files.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let extension = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let sidecar = manifest_dir
        .join("binaries")
        .join(format!("receiver-backend-{target}{extension}"));
    println!("cargo:rerun-if-changed={}", sidecar.display());
    let embedded_backend = if sidecar.is_file() {
        format!("include_bytes!({sidecar:?})")
    } else {
        println!("cargo:warning=Rust backend was not built; run pnpm build:desktop before a runnable release build");
        "&[]".into()
    };

    let dist = manifest_dir.join("../dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files = Vec::new();
    if dist.is_dir() {
        collect_files(&dist, &dist, &mut files).expect("read built UI assets");
    }
    files.sort();

    let mut generated =
        format!("#[allow(dead_code)]\npub const EMBEDDED_BACKEND: &[u8] = {embedded_backend};\n");
    generated
        .push_str("#[allow(dead_code)]\npub const EMBEDDED_CONFIG_ASSETS: &[(&str, &[u8])] = &[\n");
    for relative in &files {
        let source = dist.join(relative);
        let name = relative.to_string_lossy().replace('\\', "/");
        generated.push_str(&format!("    ({name:?}, include_bytes!({source:?})),\n"));
    }
    generated.push_str("];\n");

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_runtime.rs");
    fs::write(output, generated).expect("write embedded runtime index");
}
