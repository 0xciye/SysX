use crate::client::is_tauri_window;
use crate::config::ConfigApp;
use crate::monitor::MonitorApp;
use leptos::prelude::*;

#[component]
pub fn App() -> impl IntoView {
    if is_tauri_window() {
        view! { <MonitorApp/> }.into_any()
    } else {
        view! { <ConfigApp/> }.into_any()
    }
}
