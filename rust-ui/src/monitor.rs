use crate::client::{command_channel, device_status, start_backend_client};
use crate::layout::preview_detections;
use crate::preview::{PreviewCanvas, PreviewFrame};
use crate::protocol::{BackendEvent, BackendState, DeviceStatus};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn MonitorApp() -> impl IntoView {
    let backend_state = RwSignal::new(None::<BackendState>);
    let backend_connected = RwSignal::new(false);
    let backend_error = RwSignal::new(String::new());
    let device = RwSignal::new(DeviceStatus {
        state: "missing".into(),
        name: None,
        buttons: vec![None; 5],
    });
    let frame = RwSignal::new(None::<PreviewFrame>);
    let detections = RwSignal::new(Vec::new());
    let target = RwSignal::new(None::<[f32; 2]>);
    let fov = RwSignal::new(36_u16);
    let scale = RwSignal::new("Smooth".to_string());
    let detection_color = RwSignal::new("#e3bf4d".to_string());
    let model_confidence = RwSignal::new(0.5_f32);
    let (command_sender, command_receiver) = command_channel();
    let command_receiver = Rc::new(RefCell::new(Some(command_receiver)));

    Effect::new({
        let command_receiver = Rc::clone(&command_receiver);
        move || {
            if let Some(command_receiver) = command_receiver.borrow_mut().take() {
                start_backend_client(false, command_receiver, move |event| match event {
                    BackendEvent::Ready => {
                        backend_connected.set(true);
                        backend_error.set(String::new());
                    }
                    BackendEvent::Disconnected => backend_connected.set(false),
                    BackendEvent::State(state) => {
                        backend_connected.set(true);
                        backend_error.set(state.last_error.clone().unwrap_or_default());
                        fov.set(state.fov_degrees.clamp(1, 360));
                        scale.set(if state.preview_scale == "Pixel perfect" {
                            "Pixel perfect".into()
                        } else {
                            "Smooth".into()
                        });
                        detection_color.set(pixel_color_hex(&state.pixel_color).into());
                        if state.status == "waiting" {
                            frame.set(None);
                            detections.set(Vec::new());
                            target.set(None);
                        }
                        backend_state.set(Some(state));
                    }
                    BackendEvent::CommandResponse {
                        error: Some(error), ..
                    } => backend_error.set(error),
                    BackendEvent::CommandResponse { error: None, .. } => {}
                    BackendEvent::Models(models, active) => {
                        if let Some(model) = models
                            .iter()
                            .find(|model| Some(model.id.as_str()) == active.as_deref())
                        {
                            model_confidence.set(model.confidence);
                        }
                    }
                    BackendEvent::Frame(next, image_url, preview_fps) => {
                        let strategy = next.mode.as_str();
                        let visible = preview_detections(
                            &next.detection.objects,
                            strategy,
                            model_confidence.get_untracked(),
                            |object| object.confidence,
                        );
                        target.set(
                            next.detection
                                .smoothed_target_xy
                                .or(next.detection.target_xy),
                        );
                        detections.set(visible);
                        frame.set(Some(PreviewFrame {
                            frame: next,
                            image_url,
                            preview_fps,
                        }));
                        backend_error.set(String::new());
                    }
                    BackendEvent::Error(message) => backend_error.set(message),
                });
            }
            let command_sender = command_sender.clone();
            spawn_local(async move {
                let _command_sender = command_sender;
                loop {
                    match device_status().await {
                        Ok(value) => device.set(value),
                        Err(_) => {
                            device.set(DeviceStatus {
                                state: "error".into(),
                                name: None,
                                buttons: vec![None; 5],
                            });
                        }
                    }
                    TimeoutFuture::new(100).await;
                }
            });
        }
    });

    let receiving = move || {
        frame.get().is_some()
            && backend_state
                .get()
                .is_some_and(|state| state.status == "streaming")
    };
    let status_label = move || {
        let error = !backend_error.get().is_empty();
        if error {
            if backend_connected.get() {
                "Runtime error"
            } else {
                "Connection error"
            }
        } else if receiving()
            || backend_state
                .get()
                .is_some_and(|state| state.status == "streaming")
        {
            "Receiving frames"
        } else if backend_connected.get() {
            "Waiting for sender"
        } else {
            "Starting receiver"
        }
    };
    view! {
        <main class="app-window monitor-window">
            <header class="app-header">
                <div class="nav-brand"><img class="brand-mark" src="/receiver-mark.svg" alt="" aria-hidden="true"/><span>"SysX"</span></div>
                <div class="header-status-group monitor-status-group">
                    <span class=move || format!("device-chip {}", if device.get().state == "connected" { "connected" } else { "error" }) aria-live="polite">
                        <i/>
                        {move || match device.get().state.as_str() {
                            "connected" => device.get().name.unwrap_or_else(|| "Connected".into()),
                            "error" => "Device error".into(),
                            _ => "No device".into(),
                        }}
                    </span>
                    <div class="mouse-state" role="group" aria-label="Physical mouse button state">
                        <span class="mouse-state-title">"Mouse"</span>
                        {[
                            ("Left", "Left"),
                            ("Right", "Right"),
                            ("Middle", "Middle"),
                            ("Mouse4", "M4"),
                            ("Mouse5", "M5"),
                        ].into_iter().enumerate().map(|(index, (label, display))| {
                            let button_state = move || match device.get().buttons.get(index).copied().flatten() {
                                Some(true) if device.get().state == "connected" => "pressed",
                                Some(false) if device.get().state == "connected" => "released",
                                _ => "unknown",
                            };
                            view! {
                                <span class=move || format!("mouse-state-key {}", button_state()) aria-label=move || format!("{label}: {}", button_state()) title=move || format!("{label}: {}", button_state())>
                                    <i aria-hidden="true"/>{display}
                                </span>
                            }
                        }).collect_view()}
                    </div>
                    <span class=move || format!("status-chip {}", if !backend_error.get().is_empty() { "error" } else if receiving() { "ready" } else { "waiting" }) aria-live="polite"><i/>{status_label}</span>
                </div>
            </header>
            <section class="monitor-content" aria-label="Live preview">
                <section class="preview-panel monitor-preview" aria-label="Preview">
                    <div class="preview-canvas">
                        <PreviewCanvas frame=frame scale=scale detections=detections target_point=target detection_color=detection_color fov_degrees=fov/>
                        <Show when=move || frame.get().is_none()>
                            <div class="waiting-message" role="status" aria-live="polite">
                                <span class="waiting-indicator" aria-hidden="true"/>
                                <span class="frame-empty-label">"NO FRAME"</span>
                                <strong>{move || if !backend_error.get().is_empty() { if backend_connected.get() { "Receiver error" } else { "Receiver unavailable" } } else if backend_connected.get() { "Waiting for sender" } else { "Starting receiver" }}</strong>
                                <Show when=move || !backend_error.get().is_empty()>
                                    <span>{backend_error}</span>
                                </Show>
                            </div>
                        </Show>
                    </div>
                    <footer class="monitor-hud" aria-label="Performance">
                        <span><i>"Processing resolution"</i><b>{move || if receiving() { frame.get().map(|value| format!("{} × {}", value.frame.detection.processed_width, value.frame.detection.processed_height)).unwrap_or_else(|| "—".into()) } else { "—".into() }}</b></span>
                        <span><i>"FPS"</i><b>{move || if receiving() { frame.get().and_then(|value| value.frame.processing_fps).map(|fps| format!("{fps:.1}")).unwrap_or_else(|| "—".into()) } else { "—".into() }}</b></span>
                        <span><i>"Latency"</i><b>{move || if receiving() { frame.get().map(|value| format!("{:.2} ms", value.frame.detection.total_processing_ms)).unwrap_or_else(|| "—".into()) } else { "—".into() }}</b></span>
                    </footer>
                </section>
            </section>
        </main>
    }
}

fn pixel_color_hex(color: &str) -> &'static str {
    match color {
        "red" => "#e26868",
        "purple" => "#a682dc",
        _ => "#e3bf4d",
    }
}
