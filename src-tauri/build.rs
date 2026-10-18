use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;

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
        // Desktop releases embed the backend; never package a host that cannot
        // start its receiver on the target platform.
        if env::var("PROFILE").as_deref() == Ok("release") {
            panic!(
                "release host is missing the Rust backend sidecar: {}",
                sidecar.display()
            );
        }
        println!("cargo:warning=Rust backend was not built; run pnpm build:desktop before a runnable release build");
        "&[]".into()
    };

    let mut generated =
        format!("#[allow(dead_code)]\npub const EMBEDDED_BACKEND: &[u8] = {embedded_backend};\n");

    // This manifest is embedded in the host, never trusted from an editable
    // runtime JSON file. Hash the final (possibly VMProtected) build inputs.
    generated.push_str(
        "#[allow(dead_code)]\npub const EMBEDDED_RUNTIME_HASHES: &[(&str, u64, [u8; 32])] = &[\n",
    );
    if sidecar.is_file() {
        let bytes = fs::read(&sidecar).expect("read backend for integrity manifest");
        let name = format!("receiver-backend{extension}");
        generated.push_str(&format!(
            "    ({name:?}, {}, {:?}),\n",
            bytes.len(),
            <[u8; 32]>::from(Sha256::digest(&bytes))
        ));
    }
    generated.push_str("];\n");

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_runtime.rs");
    fs::write(output, generated).expect("write embedded runtime index");
}
