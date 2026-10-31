#![recursion_limit = "256"]

pub mod auto_target;
pub mod builtin_models;
pub mod cli;
pub mod config;
pub mod crop;
pub mod decode_pool;
pub mod inference;
#[path = "../../native-shared/jpeg.rs"]
pub mod jpeg;
#[path = "../../native-shared/motion_capture.rs"]
pub mod motion_capture;
pub mod movement;
pub mod onnx_info;
pub mod pipeline;
pub mod pixel_seek;
pub mod receiver;
pub mod results;
pub mod service;
pub mod target_manager;
pub mod targeting;
#[path = "../../native-shared/timer_resolution.rs"]
pub mod timer_resolution;
pub mod udp;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    cli::run(args)
}
