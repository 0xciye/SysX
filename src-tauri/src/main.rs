#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod auto;
#[path = "../../native-shared/diagnostics.rs"]
mod diagnostics;
#[cfg_attr(debug_assertions, allow(dead_code))]
mod integrity;
#[path = "../../native-shared/jpeg.rs"]
mod jpeg;
mod medius_device;
mod monitor_window;
mod movement;
mod native_app;
mod native_monitor;
mod security;
mod serial_queue;
mod single_instance;
#[path = "../../native-shared/timer_resolution.rs"]
mod timer_resolution;

fn main() {
    if let Err(error) = diagnostics::install("host") {
        eprintln!("SysX diagnostics unavailable: {error}");
    }
    security::start_release_guard();
    timer_resolution::request_fine_timer();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(native_app::run)) {
        Ok(()) => diagnostics::finish_session("native application returned normally"),
        Err(_) => diagnostics::finish_session(
            "native host caught an unwinding panic; see the crash report for details",
        ),
    }
}

#[path = "../../native-shared/motion_capture.rs"]
mod motion_capture;
mod mouse_capture;
