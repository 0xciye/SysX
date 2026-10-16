//! Release-only process hardening.
//!
//! Optional VMProtect processing is managed separately by the user. This small guard is disabled in debug/test builds and
//! does not change application logic.

/// Refuse to run under a user-mode Windows debugger in release builds.
pub fn start_release_guard() {
    #[cfg(all(not(debug_assertions), not(test), target_os = "windows"))]
    {
        enable_process_mitigations();
        if debugger_present() {
            terminate_process_tree();
        }
        let watchdog = std::thread::Builder::new()
            .name("sysx-release-guard".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if debugger_present() {
                    terminate_process_tree();
                }
            });
        if watchdog.is_err() {
            terminate_process_tree();
        }
    }
}

/// Reduce common DLL-hijacking and extension-point injection paths. Windows
/// may reject a policy on older systems; that is intentionally non-fatal so
/// the supported receiver runtime keeps its normal startup behavior.
#[cfg(all(not(debug_assertions), not(test), target_os = "windows"))]
fn enable_process_mitigations() {
    const PROCESS_IMAGE_LOAD_POLICY: u32 = 10;
    const PROCESS_EXTENSION_POINT_DISABLE_POLICY: u32 = 6;
    const NO_REMOTE_IMAGES: u32 = 1 << 0;
    const NO_LOW_MANDATORY_LABEL_IMAGES: u32 = 1 << 1;
    const DISABLE_EXTENSION_POINTS: u32 = 1;

    let image_policy = NO_REMOTE_IMAGES | NO_LOW_MANDATORY_LABEL_IMAGES;
    let extension_policy = DISABLE_EXTENSION_POINTS;
    unsafe {
        let _ = SetProcessMitigationPolicy(
            PROCESS_IMAGE_LOAD_POLICY,
            (&image_policy as *const u32).cast(),
            std::mem::size_of_val(&image_policy),
        );
        let _ = SetProcessMitigationPolicy(
            PROCESS_EXTENSION_POINT_DISABLE_POLICY,
            (&extension_policy as *const u32).cast(),
            std::mem::size_of_val(&extension_policy),
        );
    }
}

/// Kill the host and every child it owns. A plain `process::exit` would skip
/// `BackendRuntime::Drop` and could leave the extracted receiver alive.
#[cfg(all(not(debug_assertions), not(test), target_os = "windows"))]
fn terminate_process_tree() -> ! {
    crate::diagnostics::record("security guard terminated the process tree");
    let pid = unsafe { GetCurrentProcessId() };
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
    std::process::exit(1)
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn debugger_present() -> bool {
    let mut remote = 0_i32;
    unsafe {
        IsDebuggerPresent() != 0
            || (CheckRemoteDebuggerPresent(GetCurrentProcess(), &mut remote) != 0 && remote != 0)
            || native_debug_flags_present()
    }
}

/// Check the process debug port/object/flags as a second line of defense.
/// These values are intentionally only consulted by the release watchdog;
/// development and test binaries never call this function.
#[cfg(target_os = "windows")]
unsafe fn native_debug_flags_present() -> bool {
    const PROCESS_DEBUG_PORT: u32 = 7;
    const PROCESS_DEBUG_OBJECT_HANDLE: u32 = 30;
    const PROCESS_DEBUG_FLAGS: u32 = 31;
    let process = GetCurrentProcess();

    let mut debug_port: *mut std::ffi::c_void = std::ptr::null_mut();
    let port_status = NtQueryInformationProcess(
        process,
        PROCESS_DEBUG_PORT,
        &mut debug_port as *mut _ as *mut std::ffi::c_void,
        std::mem::size_of_val(&debug_port) as u32,
        std::ptr::null_mut(),
    );
    if port_status == 0 && debug_port as isize == -1 {
        return true;
    }

    let mut debug_object: *mut std::ffi::c_void = std::ptr::null_mut();
    let object_status = NtQueryInformationProcess(
        process,
        PROCESS_DEBUG_OBJECT_HANDLE,
        &mut debug_object as *mut _ as *mut std::ffi::c_void,
        std::mem::size_of_val(&debug_object) as u32,
        std::ptr::null_mut(),
    );
    if object_status == 0 && !debug_object.is_null() {
        return true;
    }

    let mut debug_flags = 1_u32;
    let flags_status = NtQueryInformationProcess(
        process,
        PROCESS_DEBUG_FLAGS,
        &mut debug_flags as *mut _ as *mut std::ffi::c_void,
        std::mem::size_of_val(&debug_flags) as u32,
        std::ptr::null_mut(),
    );
    flags_status == 0 && debug_flags == 0
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn debugger_present() -> bool {
    false
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CheckRemoteDebuggerPresent(
        process: *mut std::ffi::c_void,
        debugger_present: *mut i32,
    ) -> i32;
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    #[allow(dead_code)]
    fn GetCurrentProcessId() -> u32;
    fn IsDebuggerPresent() -> i32;
    #[allow(dead_code)]
    fn SetProcessMitigationPolicy(
        policy: u32,
        mitigation_policy: *const std::ffi::c_void,
        mitigation_policy_size: usize,
    ) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: *mut std::ffi::c_void,
        process_information_class: u32,
        process_information: *mut std::ffi::c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}
