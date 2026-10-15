#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod monitor_window;
mod movement;
mod native_app;
mod single_instance;

fn main() {
    native_app::run();
}
