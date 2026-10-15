use crate::client::is_monitor_page;
use crate::config::ConfigApp;
use crate::monitor::MonitorApp;
use leptos::prelude::*;

#[component]
pub fn App() -> impl IntoView {
    if is_monitor_page() {
        view! { <MonitorApp/> }.into_any()
    } else {
        view! { <ConfigApp/> }.into_any()
    }
}
