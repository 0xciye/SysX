#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod movement;
mod native_app;

fn main() {
    native_app::run();
}
