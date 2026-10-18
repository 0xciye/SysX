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

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_runtime.rs");
    fs::write(output, generated).expect("write embedded runtime index");
}
