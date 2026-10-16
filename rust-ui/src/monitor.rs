use crate::client::{command_channel, start_backend_client};
use crate::layout::{fps_tone, latency_tone, preview_detections, LATENCY_HIGH_MS};
use crate::preview::{PreviewCanvas, PreviewFrame};
use crate::protocol::{BackendEvent, BackendState, DeviceStatus};
use leptos::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Receiver window: one square live feed with an in-feed HUD, connection and
/// device status in a thin bar, and the five mouse-button states below the feed.
#[component]
pub fn MonitorApp() -> impl IntoView {
    let backend_state = RwSignal::new(None::<BackendState>);
    let backend_connected = RwSignal::new(false);
    let backend_error = RwSignal::new(String::new());
    let frame = RwSignal::new(None::<PreviewFrame>);
    let detections = RwSignal::new(Vec::new());
    let target = RwSignal::new(None::<[f32; 2]>);
    let fov = RwSignal::new(36_u16);
    let scale = RwSignal::new("Smooth".to_string());
    let detection_color = RwSignal::new("#e3bf4d".to_string());
    let fov_color = RwSignal::new("#43cfa0".to_string());
    let model_confidence = RwSignal::new(0.5_f32);
    let (command_sender, command_receiver) = command_channel();
    let command_receiver = Rc::new(RefCell::new(Some(command_receiver)));

    Effect::new({
        let command_receiver = Rc::clone(&command_receiver);
        move || {
            let _keep_sender = command_sender.clone();
            if let Some(command_receiver) = command_receiver.borrow_mut().take() {
                start_backend_client("monitor", command_receiver, move |event| match event {
                    BackendEvent::Ready => {
                        backend_connected.set(true);
                        backend_error.set(String::new());
                    }
                    BackendEvent::Disconnected => {
                        backend_connected.set(false);
                        frame.set(None);
                    }
                    BackendEvent::State(state) => {
                        // State arrives every 100 ms; unchanged values must not
                        // wake the preview redraw.
                        set_changed(backend_connected, true);
                        set_changed(backend_error, state.last_error.clone().unwrap_or_default());
                        let effective_fov = if state.effective_fov_degrees == 0 {
                            state.fov_degrees
                        } else {
                            state.effective_fov_degrees
                        };
                        set_changed(fov, effective_fov.clamp(1, 360));
                        set_changed(
                            scale,
                            if state.preview_scale == "Pixel perfect" {
                                "Pixel perfect".into()
                            } else {
                                "Smooth".into()
                            },
                        );
                        let effective_color = if state.effective_pixel_color.is_empty() {
                            &state.pixel_color
                        } else {
                            &state.effective_pixel_color
                        };
                        set_changed(detection_color, pixel_color_hex(effective_color).into());
                        set_changed(
                            fov_color,
                            if state.auto_trigger_held {
                                "#4da7e8".into()
                            } else {
                                "#43cfa0".into()
                            },
                        );
                        // "waiting" = no frame for a second: show the empty
                        // state instead of a frozen image.
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
                    BackendEvent::Frame(next, image) => {
                        let visible = preview_detections(
                            &next.detection.objects,
                            &next.mode,
                            model_confidence.get_untracked(),
                            |object| object.confidence,
                        );
                        // Detections and target ride along with the frame:
                        // only `frame` notifies, so each frame is drawn once.
                        target.update_untracked(|value| {
                            *value = if next.detection.auto_target_valid {
                                // Auto is a presence gate: a matching pixel
                                // is enough, so never draw Motion's target
                                // line/ring to an arbitrary pixel.
                                None
                            } else {
                                next.detection
                                    .smoothed_target_xy
                                    .or(next.detection.target_xy)
                            }
                        });
                        detections.update_untracked(|value| *value = visible);
                        frame.set(Some(PreviewFrame { frame: next, image }));
                        set_changed(backend_error, String::new());
                    }
                    BackendEvent::Error(message) => backend_error.set(message),
                });
            }
        }
    });

    let status = Memo::new(move |_| {
        monitor_status(
            !backend_error.get().is_empty(),
            backend_connected.get(),
            frame.with(Option::is_some),
        )
    });
    let device = Memo::new(move |_| {
        backend_state
            .get()
            .and_then(|state| state.device)
            .filter(|_| backend_connected.get())
            .unwrap_or_else(|| DeviceStatus {
                state: "missing".into(),
                name: None,
                buttons: vec![None; 5],
            })
    });
    let device_label = move || {
        let device = device.get();
        match device.state.as_str() {
            "connected" => device.name.unwrap_or_else(|| "Device connected".into()),
            "error" => "Device error".into(),
            _ => "No device".into(),
        }
    };
    let profile_label = move || {
        backend_state
            .get()
            .map(|state| {
                state
                    .profiles
                    .iter()
                    .find(|profile| profile.index == usize::from(state.profile_index))
                    .map(|profile| profile.name.clone())
                    .unwrap_or_else(|| {
                        if state.profile_index == 0 {
                            "Default".into()
                        } else {
                            format!("Profile {}", state.profile_index + 1)
                        }
                    })
            })
            .unwrap_or_else(|| "Default".into())
    };
    let device_tone = move || match device.get().state.as_str() {
        "connected" => "live",
        "error" => "error",
        _ => "idle",
    };
    let hud = move || {
        frame.with(|frame| match frame {
            Some(value) => (
                // The processed crop is always square, so one side is enough.
                value.frame.detection.processed_width.to_string(),
                value
                    .frame
                    .processing_fps
                    .map(|fps| format!("{fps:.0}"))
                    .unwrap_or_else(|| "—".into()),
                format!("{:.2}", value.frame.detection.total_processing_ms),
            ),
            None => ("—".into(), "—".into(), "—".into()),
        })
    };
    let fps_level = move || {
        frame.with(|frame| {
            frame
                .as_ref()
                .and_then(|value| fps_tone(value.frame.processing_fps, value.frame.source_fps))
                .unwrap_or("none")
        })
    };
    let latency_level = move || {
        frame.with(|frame| {
            frame
                .as_ref()
                .and_then(|value| latency_tone(value.frame.detection.total_processing_ms))
                .unwrap_or("none")
        })
    };
    let latency_high = move || {
        frame.with(|frame| {
            frame
                .as_ref()
                .is_some_and(|value| value.frame.detection.total_processing_ms > LATENCY_HIGH_MS)
        })
    };
    // Only errors get a detail line under the empty-state headline.
    let empty_detail = move || match status.get() {
        MonitorStatus::RuntimeError | MonitorStatus::ConnectionError => backend_error.get(),
        _ => String::new(),
    };

    view! {
        <main class="monitor">
            <header class="monitor-bar">
                <div class="monitor-brand">
                    <img src="receiver-mark.svg" alt="" aria-hidden="true"/>
                    <span>"SysX"</span>
                </div>
                <div class="monitor-status">
                    <span class="monitor-profile" aria-live="polite">
                        <span class="monitor-profile-label">"Profile"</span>
                        <strong>{profile_label}</strong>
                    </span>
                    <span class="status" data-tone=device_tone aria-live="polite">
                        <i aria-hidden="true"/>{device_label}
                    </span>
                </div>
            </header>
            <section class="monitor-stage" aria-label="Live preview">
                <div class="monitor-column">
                    <div class="feed">
                        <PreviewCanvas frame=frame scale=scale detections=detections target_point=target detection_color=detection_color fov_degrees=fov fov_color=fov_color/>
                        <div class="feed-hud" aria-label="Processing resolution, FPS and latency">
                            <span><i>"Res:"</i><b>{move || hud().0}</b></span>
                            <span data-level=fps_level><b>{move || hud().1}</b><i>"FPS"</i></span>
                            <span data-level=latency_level><b>{move || hud().2}</b><i>"ms"</i>
                                <Show when=latency_high>
                                    <em class="hud-high">"(HIGH)"</em>
                                </Show>
                            </span>
                        </div>
                        <Show when=move || frame.with(Option::is_none)>
                            <div class="feed-empty" role="status" aria-live="polite">
                                <span class="feed-empty-tag">"NO FRAME"</span>
                                <strong data-tone=move || status.get().tone()><i aria-hidden="true"/>{move || status.get().label()}</strong>
                                <Show when=move || !empty_detail().is_empty()>
                                    <p>{empty_detail}</p>
                                </Show>
                            </div>
                        </Show>
                    </div>
                    <div class="mouse" role="group" aria-label="Physical mouse buttons" data-device=move || if device.get().state == "connected" { "on" } else { "off" }>
                        {["Left", "Right", "Middle", "M4", "M5"].into_iter().enumerate().map(|(index, label)| {
                            let state = move || {
                                let device = device.get();
                                match device.buttons.get(index).copied().flatten() {
                                    Some(true) if device.state == "connected" => "down",
                                    Some(false) if device.state == "connected" => "up",
                                    _ => "unknown",
                                }
                            };
                            view! {
                                <span class="mouse-key" data-state=state aria-label=move || format!("{label}: {}", state())>{label}</span>
                            }
                        }).collect_view()}
                    </div>
                </div>
            </section>
        </main>
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorStatus {
    Starting,
    Waiting,
    Receiving,
    RuntimeError,
    ConnectionError,
}

impl MonitorStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting receiver",
            Self::Waiting => "Waiting for sender",
            Self::Receiving => "Receiving frames",
            Self::RuntimeError => "Runtime error",
            Self::ConnectionError => "Connection error",
        }
    }

    fn tone(self) -> &'static str {
        match self {
            Self::Starting | Self::Waiting => "wait",
            Self::Receiving => "live",
            Self::RuntimeError | Self::ConnectionError => "error",
        }
    }
}

fn monitor_status(has_error: bool, connected: bool, has_frame: bool) -> MonitorStatus {
    match (has_error, connected, has_frame) {
        (true, true, _) => MonitorStatus::RuntimeError,
        (true, false, _) => MonitorStatus::ConnectionError,
        (false, _, true) => MonitorStatus::Receiving,
        (false, true, false) => MonitorStatus::Waiting,
        (false, false, false) => MonitorStatus::Starting,
    }
}

fn set_changed<T: PartialEq + Send + Sync + 'static>(signal: RwSignal<T>, value: T) {
    if signal.with_untracked(|current| *current != value) {
        signal.set(value);
    }
}

fn pixel_color_hex(color: &str) -> &'static str {
    match color {
        "red" => "#e26868",
        "purple" => "#a682dc",
        _ => "#e3bf4d",
    }
}
