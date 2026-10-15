pub mod layout;
pub mod protocol;
pub mod settings;

#[cfg(target_arch = "wasm32")]
pub mod client;

#[cfg(target_arch = "wasm32")]
pub mod app;

#[cfg(target_arch = "wasm32")]
pub mod config;

#[cfg(target_arch = "wasm32")]
pub mod monitor;

#[cfg(target_arch = "wasm32")]
pub mod preview;
