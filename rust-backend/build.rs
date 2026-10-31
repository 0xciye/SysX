use std::path::PathBuf;

/// Content hash written next to extracted files: a launch whose stamp
/// matches skips rewriting (and re-reading) them.
fn content_hash(data: &[u8], mut hash: u64) -> u64 {
    for chunk in data.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(0x0000_0100_0000_01b3);
        hash ^= hash >> 32;
    }
    hash ^ data.len() as u64
}

/// Embeds the built-in Object Detection models from `models/` (or the folder
/// in `SYSX_EMBEDDED_MODELS`), searched together with its immediate
/// subfolders (`onnx/`, `tensorrt/`, `openvino/`). Every `.onnx`, `.engine`,
/// `.plan`, `.xml` and `.bin` file becomes one artifact; `builtin_models.rs`
/// groups them by name and precision (`<name>_fp16`, `<name>_int8`, ...).
fn embed_models() {
    println!("cargo:rerun-if-env-changed=SYSX_EMBEDDED_MODELS");
    let folder = std::env::var_os("SYSX_EMBEDDED_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models"));
    println!("cargo:rerun-if-changed={}", folder.display());
    let mut folders = vec![folder.clone()];
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
            folders.push(entry.path());
        }
    }
    let mut files = Vec::new();
    for dir in &folders {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if path.is_file()
                && extension
                    .as_deref()
                    .is_some_and(|e| ["onnx", "engine", "plan", "xml", "bin"].contains(&e))
            {
                files.push((path, extension.unwrap()));
            }
        }
    }
    files.sort();
    let mut generated = String::from(
        "static EMBEDDED_ARTIFACTS: &[EmbeddedArtifact] = &[
",
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (path, extension) in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        if !seen.insert((stem.clone(), extension.clone())) {
            panic!("two built-in model files are named {stem}.{extension}");
        }
        println!("cargo:rerun-if-changed={}", path.display());
        let full = std::fs::canonicalize(path).unwrap();
        hash = content_hash(stem.as_bytes(), hash);
        hash = content_hash(extension.as_bytes(), hash);
        hash = content_hash(&std::fs::read(&full).expect("read model"), hash);
        generated.push_str(&format!(
            "    EmbeddedArtifact {{ stem: {stem:?}, extension: {extension:?}, bytes: include_bytes!({full:?}) }},
"
        ));
    }
    generated.push_str(&format!(
        "];
/// Hash of every embedded artifact (name and content).
static EMBEDDED_MODELS_HASH: u64 = {hash:#x};
"
    ));
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("embedded_models.rs");
    std::fs::write(output, generated).expect("write embedded model index");
}

fn main() {
    embed_models();
    let root = "../third-party/abcurves";
    println!("cargo:rerun-if-changed={root}");
    println!("cargo:rerun-if-changed=src/movement/abcurves_bridge.c");
    let mut build = cc::Build::new();
    build
        .include(format!("{root}/include"))
        .include(format!("{root}/src"))
        .file(format!("{root}/src/abc_online.c"))
        .file(format!("{root}/src/abc_fixed.c"))
        .file(format!("{root}/src/abc_w5_stream.c"))
        .file("src/movement/abcurves_bridge.c");
    if build.get_compiler().is_like_msvc() {
        build.flag("/fp:strict").flag_if_supported("/std:c11");
    } else {
        build
            .flag_if_supported("-std=c99")
            .flag_if_supported("-ffp-contract=off");
        println!("cargo:rustc-link-lib=m");
    }
    build.compile("sysx_abcurves");
}
