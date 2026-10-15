#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;

#[cfg(target_arch = "wasm32")]
fn main() {
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        if let Some(fallback) = document.get_element_by_id("sysx-boot-fallback") {
            fallback.remove();
        }
    }
    mount_to_body(sysx_ui::app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
