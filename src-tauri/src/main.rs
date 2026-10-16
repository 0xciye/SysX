#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auto;
#[cfg_attr(debug_assertions, allow(dead_code))]
mod integrity;
mod medius_device;
mod monitor_window;
mod movement;
mod native_app;
mod security;
mod serial_queue;
mod single_instance;

fn main() {
    security::start_release_guard();
    native_app::run();
}
