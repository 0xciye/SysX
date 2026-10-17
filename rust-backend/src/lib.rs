#![recursion_limit = "256"]

pub mod auto_target;
pub mod cli;
pub mod config;
pub mod crop;
#[path = "../../native-shared/diagnostics.rs"]
pub mod diagnostics;
pub mod inference;
pub mod movement;
pub mod pipeline;
pub mod pixel_seek;
pub mod receiver;
pub mod results;
pub mod security;
pub mod service;
pub mod targeting;
#[path = "../../native-shared/timer_resolution.rs"]
pub mod timer_resolution;
pub mod udp;
pub mod web_config;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    cli::run(args)
}
