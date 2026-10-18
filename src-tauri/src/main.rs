#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod auto;
#[path = "../../native-shared/diagnostics.rs"]
mod diagnostics;
#[path = "../../native-shared/jpeg.rs"]
mod jpeg;
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
    if let Err(error) = diagnostics::install("host") {
        eprintln!("SysX diagnostics unavailable: {error}");
    }
    timer_resolution::request_fine_timer();
    let restart = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(native_app::run)) {
        Ok(restart) => {
            diagnostics::finish_session("native application returned normally");
            restart
        }
        Err(_) => {
            diagnostics::finish_session(
                "native host caught an unwinding panic; see the crash report for details",
            );
            true
        }
    };
    std::process::ExitCode::from(if restart {
        supervisor::EXIT_RESTART
    } else {
        supervisor::EXIT_DONE
    } as u8)
}

#[path = "../../native-shared/motion_capture.rs"]
mod motion_capture;
mod mouse_capture;
