pub mod cli;
pub mod config;
pub mod crop;
pub mod inference;
pub mod movement;
pub mod pipeline;
pub mod pixel_seek;
pub mod receiver;
pub mod results;
pub mod service;
pub mod storage;
pub mod targeting;
pub mod udp;
pub mod web_config;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    cli::run(args)
}
