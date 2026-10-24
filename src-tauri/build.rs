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

    // Runtime libraries the backend loads (DirectML.dll, ONNX Runtime
    // provider DLLs), copied next to the sidecar by build-rust-backend.mjs.
    let binaries = manifest_dir.join("binaries");
    println!("cargo:rerun-if-changed={}", binaries.display());
    let mut libraries = fs::read_dir(&binaries)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        ["dll", "dylib", "so"].contains(&extension.to_ascii_lowercase().as_str())
                    })
        })
        .collect::<Vec<_>>();
    libraries.sort();
    generated
        .push_str("#[allow(dead_code)]\npub const EMBEDDED_LIBRARIES: &[(&str, &[u8])] = &[\n");
    for library in &libraries {
        println!("cargo:rerun-if-changed={}", library.display());
        let name = library.file_name().unwrap().to_string_lossy();
        generated.push_str(&format!("    ({name:?}, include_bytes!({library:?})),\n"));
    }
    generated.push_str("];\n");

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_runtime.rs");
    fs::write(output, generated).expect("write embedded runtime index");
}
