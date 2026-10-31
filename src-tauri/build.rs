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
    let mut backend_hash = 0u64;
    let embedded_backend = if sidecar.is_file() {
        backend_hash = content_hash(
            &fs::read(&sidecar).expect("read backend sidecar"),
            0xcbf2_9ce4_8422_2325,
        );
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

    let generated = format!(
        "#[allow(dead_code)]\npub const EMBEDDED_BACKEND: &[u8] = {embedded_backend};\n\
         #[allow(dead_code)]\npub const EMBEDDED_BACKEND_HASH: u64 = {backend_hash:#x};\n"
    );

    // Runtime libraries (onnxruntime.dll, DirectML.dll, provider DLLs) are
    // not embedded: they ship next to SysX.exe (user request, 2026-10-31).

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_runtime.rs");
    fs::write(output, generated).expect("write embedded runtime index");
}

/// Hash of the embedded backend: a launch whose extracted copy carries the
/// same stamp reuses it instead of writing it again.
fn content_hash(data: &[u8], mut hash: u64) -> u64 {
    for chunk in data.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(0x0000_0100_0000_01b3);
        hash ^= hash >> 32;
    }
    hash ^ data.len() as u64
}
