fn main() {
    sysx_backend::timer_resolution::request_fine_timer();
    use_shipped_libraries();
    if let Err(error) = sysx_backend::run(std::env::args().skip(1).collect()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

/// SysX ships onnxruntime.dll, DirectML.dll and the other runtime libraries
/// next to SysX.exe; the host passes that folder in `SYSX_LIBRARY_DIR`. Load
/// ONNX Runtime from there and let its own dependencies resolve there before
/// System32 (Windows 10's System32 DirectML.dll is too old: `887A0004`).
fn use_shipped_libraries() {
    let Some(dir) = std::env::var_os("SYSX_LIBRARY_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated wide path that outlives the call.
        unsafe {
            windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(wide.as_ptr());
        }
    }
    let library = dir.join(if cfg!(windows) {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    });
    // Single-threaded here: no other thread reads the environment yet.
    if std::env::var_os("ORT_DYLIB_PATH").is_none() && library.is_file() {
        std::env::set_var("ORT_DYLIB_PATH", library);
    }
    // The OpenVINO crate finds openvino_c only through its environment
    // variables and the library path, never beside the executable.
    let library_path = if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let mut paths = vec![dir];
    paths.extend(
        std::env::var_os(library_path)
            .iter()
            .flat_map(std::env::split_paths),
    );
    if let Ok(joined) = std::env::join_paths(paths) {
        std::env::set_var(library_path, joined);
    }
}
