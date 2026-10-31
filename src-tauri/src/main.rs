#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod auto;
// Shared with the backend; the host uses only the preview decoder.
#[allow(dead_code)]
#[path = "../../native-shared/jpeg.rs"]
mod jpeg;
mod magnet;
mod medius_device;
mod monitor_window;
mod movement;
mod native_app;
mod native_monitor;
mod serial_queue;
mod single_instance;
#[cfg_attr(not(windows), allow(dead_code))]
mod supervisor;
#[path = "../../native-shared/timer_resolution.rs"]
mod timer_resolution;

fn main() -> std::process::ExitCode {
    // The launched SysX.exe only supervises: it runs the app as a child and
    // starts it again when it ends without the user closing it.
    #[cfg(windows)]
    if !supervisor::is_child() && supervisor::run() {
        return std::process::ExitCode::SUCCESS;
    }
    timer_resolution::request_fine_timer();
    let restart =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(native_app::run)).unwrap_or(true);
    std::process::ExitCode::from(if restart {
        supervisor::EXIT_RESTART
    } else {
        supervisor::EXIT_DONE
    } as u8)
}

#[path = "../../native-shared/motion_capture.rs"]
mod motion_capture;
mod mouse_capture;
