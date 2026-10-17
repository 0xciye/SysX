#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auto;
#[path = "../../native-shared/diagnostics.rs"]
mod diagnostics;
#[cfg_attr(debug_assertions, allow(dead_code))]
mod integrity;
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
    native_app::run();
    diagnostics::record("host stopped");
}
