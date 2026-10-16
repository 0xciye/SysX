use crate::client::{
    command_channel, get_local_settings, save_local_settings, send_command, start_backend_client,
};
use crate::protocol::{BackendEvent, BackendModel, BackendState, ModelArtifacts};
use crate::settings::{format_recoil_levels, parse_recoil_levels, AppSettings};
use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{File, HtmlInputElement};

#[component]
pub fn ConfigApp() -> impl IntoView {
    let settings = RwSignal::new(AppSettings::default());
    let models = RwSignal::new(Vec::<BackendModel>::new());
    let backend_connected = RwSignal::new(false);
    let backend_error = RwSignal::new(String::new());
    let apply_message = RwSignal::new(String::new());
    let port_draft = RwSignal::new("4200".to_string());
    let crop_draft = RwSignal::new("640".to_string());
    let offset_x_draft = RwSignal::new("0".to_string());
    let offset_y_draft = RwSignal::new("0".to_string());
    let sens_draft = RwSignal::new("0.25".to_string());
    let dpi_draft = RwSignal::new("1600".to_string());
    let recoil_levels_draft = RwSignal::new(String::new());
    let command_sender = RwSignal::new(None);
    let last_applied = RwSignal::new(String::new());
    let apply_pending_id = RwSignal::new(String::new());
    let apply_success_message = RwSignal::new(String::new());
    let apply_epoch = RwSignal::new(0_u64);
    let loaded_settings = Rc::new(Cell::new(false));
    let (command_tx, command_rx) = command_channel();
    let command_receiver = Rc::new(RefCell::new(Some(command_rx)));
    let command_tx_for_mount = command_tx.clone();

    Effect::new(move |_| {
        command_sender.set(Some(command_tx_for_mount.clone()));
        if let Some(command_rx) = command_receiver.borrow_mut().take() {
            let loaded_state_for_events = Rc::clone(&loaded_settings);
            start_backend_client("config", command_rx, move |event| match event {
                BackendEvent::Ready => {
                    backend_connected.set(true);
                    backend_error.set(String::new());
                }
                BackendEvent::Disconnected => {
                    backend_connected.set(false);
                    if !apply_pending_id.get_untracked().is_empty() {
                        apply_message
                            .set("Settings saved. Waiting for the backend to reconnect.".into());
                    }
                }
                BackendEvent::State(state) => {
                    backend_connected.set(true);
                    backend_error.set(state.last_error.clone().unwrap_or_default());
                    if !loaded_state_for_events.get() {
                        copy_state_to_settings(
                            &state,
                            settings,
                            port_draft,
                            crop_draft,
                            offset_x_draft,
                            offset_y_draft,
                            sens_draft,
                            dpi_draft,
                            recoil_levels_draft,
                        );
                    }
                }
                BackendEvent::Models(next, active) => {
                    models.set(next.clone());
                    settings.update(|value| value.active_model_id = active.clone());
                    if let Some(model) = next
                        .iter()
                        .find(|model| Some(model.id.as_str()) == active.as_deref())
                    {
                        settings.update(|value| {
                            value.model_width = model.input_width.to_string();
                            value.model_height = model.input_height.to_string();
                            value.model_confidence = model.confidence.to_string();
                            value.model_iou = model.iou.to_string();
                        });
                    }
                }
                // The backend sends no frames to the config page.
                BackendEvent::Frame(..) => {}
                BackendEvent::CommandResponse {
                    request_id,
                    command_type,
                    error,
                } => {
                    if command_type == "apply_settings"
                        && apply_pending_id.get_untracked() == request_id
                    {
                        apply_pending_id.set(String::new());
                        if let Some(error) = error {
                            apply_success_message.set(String::new());
                            backend_error.set(error.clone());
                            apply_message.set(format!(
                                "Settings saved, but the backend could not apply them: {error}"
                            ));
                        } else {
                            backend_error.set(String::new());
                            let message = apply_success_message.get_untracked();
                            apply_message.set(message.clone());
                            let epoch = apply_epoch.get_untracked();
                            spawn_local(async move {
                                TimeoutFuture::new(3000).await;
                                if apply_epoch.get_untracked() == epoch
                                    && apply_message.get_untracked() == message
                                {
                                    apply_message.set(String::new());
                                }
                            });
                        }
                    } else if let Some(error) = error {
                        backend_error.set(error);
                    }
                }
                BackendEvent::Error(message) => backend_error.set(message),
            });
        }
        let loaded_state_for_load = Rc::clone(&loaded_settings);
        spawn_local(async move {
            match get_local_settings::<AppSettings>().await {
                Ok(value) => {
                    port_draft.set(value.port.clone());
                    crop_draft.set(value.crop_resolution.to_string());
                    offset_x_draft.set(format_number(value.offset_x));
                    offset_y_draft.set(format_number(value.offset_y));
                    sens_draft.set(format_number(value.sens));
                    dpi_draft.set(value.dpi.to_string());
                    recoil_levels_draft.set(format_recoil_levels(&value.recoil.recoil_levels));
                    last_applied.set(serde_json::to_string(&value).unwrap_or_default());
                    settings.set(value);
                    loaded_state_for_load.set(true);
                }
                Err(error) => {
                    backend_error.set(error);
                    loaded_state_for_load.set(true);
                }
            }
        });
    });

    let settings_changed = move || {
        let current = settings.get();
        serde_json::to_string(&current).unwrap_or_default() != last_applied.get()
            || port_draft.get() != current.port
            || crop_draft.get() != current.crop_resolution.to_string()
            || offset_x_draft.get() != format_number(current.offset_x)
            || offset_y_draft.get() != format_number(current.offset_y)
            || sens_draft.get() != format_number(current.sens)
            || dpi_draft.get() != current.dpi.to_string()
            || recoil_levels_draft.get() != format_recoil_levels(&current.recoil.recoil_levels)
    };

    let apply_changes = move |_| {
        let mut next = settings.get();
        let port = port_draft.get().parse::<u16>();
        let crop = crop_draft.get().trim().parse::<u32>();
        let offset_x = offset_x_draft.get().trim().parse::<i64>();
        let offset_y = offset_y_draft.get().trim().parse::<i64>();
        let sens = sens_draft.get().trim().parse::<f64>();
        let dpi = dpi_draft.get().trim().parse::<u32>();
        let recoil_levels = match parse_recoil_levels(&recoil_levels_draft.get()) {
            Ok(value) => value,
            Err(error) => {
                apply_message.set(format!("Check movement settings: {error}"));
                return;
            }
        };
        let validation_error = match (port, crop, offset_x, offset_y, sens, dpi) {
            (Ok(port), Ok(crop), Ok(offset_x), Ok(offset_y), Ok(sens), Ok(dpi)) => {
                next.port = port.to_string();
                next.crop_resolution = crop;
                next.offset_x = offset_x as f64;
                next.offset_y = offset_y as f64;
                next.sens = sens;
                next.dpi = dpi;
                next.recoil.recoil_levels = recoil_levels;
                next.validate().err().map(str::to_owned)
            }
            _ if port_draft.get().parse::<u16>().is_err() => {
                Some("Enter a UDP port from 1 to 65535.".into())
            }
            _ if crop_draft.get().trim().parse::<u32>().is_err() => {
                Some("Enter a whole-number crop resolution from 1 to 8192 pixels.".into())
            }
            _ if offset_x_draft.get().trim().parse::<i64>().is_err()
                || offset_y_draft.get().trim().parse::<i64>().is_err() =>
            {
                Some("Enter a whole number for both target offsets.".into())
            }
            _ if sens_draft.get().trim().parse::<f64>().is_err() => {
                Some("Enter a sensitivity from 0.01 to 10.".into())
            }
            _ => Some("Enter an integer DPI from 100 to 100000.".into()),
        };
        if let Some(message) = validation_error {
            apply_message.set(format!("Check settings: {message}"));
            return;
        }
        let has_model = next
            .active_model_id
            .as_ref()
            .is_some_and(|id| models.get().iter().any(|model| &model.id == id));
        if let Err(error) = next.validate() {
            apply_message.set(format!("Check settings: {error}"));
            return;
        }
        let command_sender = command_sender.get();
        let connected = backend_connected.get();
        let attempt_epoch = apply_epoch.get_untracked().wrapping_add(1);
        apply_epoch.set(attempt_epoch);
        spawn_local(async move {
            match save_local_settings(&next).await {
                Ok(()) => {
                    if let Some(sender) = command_sender {
                        let request_id = format!("apply-{attempt_epoch}");
                        apply_success_message.set(
                            if next.strategy == "object_detection" && !has_model {
                                "Settings saved. Import a model to activate Object Detection."
                                    .into()
                            } else {
                                "Changes applied successfully.".into()
                            },
                        );
                        apply_pending_id.set(request_id.clone());
                        apply_message.set(if connected {
                            "Applying changes…".into()
                        } else {
                            "Settings saved. Waiting for the backend to reconnect.".into()
                        });
                        if !send_settings_commands(sender, &next, has_model, &request_id) {
                            apply_pending_id.set(String::new());
                            apply_success_message.set(String::new());
                            apply_message.set(
                                "Settings saved. Backend will apply them when connected.".into(),
                            );
                        }
                    } else {
                        apply_message
                            .set("Settings saved. Backend will apply them when connected.".into());
                    }
                    settings.set(next.clone());
                    port_draft.set(next.port.clone());
                    crop_draft.set(next.crop_resolution.to_string());
                    offset_x_draft.set(format_number(next.offset_x));
                    offset_y_draft.set(format_number(next.offset_y));
                    sens_draft.set(format_number(next.sens));
                    dpi_draft.set(next.dpi.to_string());
                    recoil_levels_draft.set(format_recoil_levels(&next.recoil.recoil_levels));
                    last_applied.set(serde_json::to_string(&next).unwrap_or_default());
                }
                Err(error) => apply_message.set(format!("Could not save settings: {error}")),
            }
        });
    };

    let status_label = move || {
        if !backend_error.get().is_empty() {
            if backend_connected.get() {
                "Runtime error"
            } else {
                "Backend unavailable"
            }
        } else if backend_connected.get() {
            "Connected"
        } else {
            "Connecting"
        }
    };
    let active_model = move || {
        let id = settings.get().active_model_id;
        models
            .get()
            .into_iter()
            .find(|model| Some(model.id.as_str()) == id.as_deref())
    };
    let marker_x =
        move || (parse_float(&offset_x_draft.get()).unwrap_or(0.0) * 2.25).clamp(-320.0, 320.0);
    let marker_y =
        move || (parse_float(&offset_y_draft.get()).unwrap_or(0.0) * 2.25).clamp(-32.0, 32.0);

    view! {
        <main class="app-window config-window">
            <header class="app-header">
                <div class="nav-brand"><img class="brand-mark" src="/receiver-mark.svg" alt="" aria-hidden="true"/><span>"SysX Configuration"</span></div>
                <span class=move || format!("status-chip {}", if !backend_error.get().is_empty() { "error" } else if backend_connected.get() { "ready" } else { "waiting" })><i/>{status_label}</span>
            </header>
            <div class="settings-workspace">
                <section class="settings-view" aria-label="Settings">
                    <div class="page-heading">
                        <div class="page-title">
                            <h1>"Settings"</h1>
                            <p class="page-subtitle">"Tune capture, detection, targeting, and movement for this receiver."</p>
                        </div>
                        <div class="apply-controls">
                            <button type="button" class="apply-button" on:click=apply_changes disabled=move || !apply_pending_id.get().is_empty() || (!settings_changed() && apply_message.get().is_empty())>"Apply Changes"</button>
                            <Show when=move || !apply_message.get().is_empty()>
                                <span class=move || format!("apply-message{}", if apply_message.get().starts_with("Enter") || apply_message.get().starts_with("Could not") || apply_message.get().starts_with("Check") { " error" } else { "" }) role="status">{apply_message}</span>
                            </Show>
                        </div>
                    </div>
                    <div class="general-page config-general">
                        <div class="settings-panel general-settings">
                            <div class="settings-row preview-options-row">
                                <div class="setting-description"><h2>"Preview"</h2></div>
                                <div class="settings-fields">
                                    <label class="field settings-wide"><span class="field-label">"Scaling"</span>
                                        <select prop:value=move || settings.get().scale on:change=move |event| settings.update(|value| value.scale = event_target_value(&event))>
                                            <option value="Smooth">"Smooth"</option><option value="Pixel perfect">"Pixel perfect"</option>
                                        </select>
                                    </label>
                                </div>
                            </div>
                            <div class="settings-row">
                                <div class="setting-description"><h2>"Input"</h2></div>
                                <div class="settings-fields">
                                    <label class="field"><span class="field-label">"UDP port"</span>
                                        <input class="text-input" inputmode="numeric" prop:value=port_draft on:input=move |event| {
                                            let digits = event_target_value(&event).chars().filter(char::is_ascii_digit).take(5).collect::<String>();
                                            port_draft.set(digits);
                                            apply_message.set(String::new());
                                        } on:blur=move |_| change_udp_port(command_sender, port_draft)/>
                                    </label>
                                    <label class="field"><span class="field-label">"Crop resolution"</span>
                                        <input class="text-input" type="number" min="1" max="8192" step="1" prop:value=crop_draft on:input=move |event| { crop_draft.set(event_target_value(&event)); apply_message.set(String::new()); }/>
                                    </label>
                                    <label class="field settings-wide"><span class="field-label">"Trigger button"</span>
                                        <select prop:value=move || settings.get().trigger_button on:change=move |event| settings.update(|value| value.trigger_button = event_target_value(&event))>
                                            <option value="right">"Right"</option><option value="middle">"Middle"</option><option value="m4">"M4"</option><option value="m5">"M5"</option>
                                        </select>
                                    </label>
                                </div>
                            </div>
                            <div class="settings-row">
                                <div class="setting-description"><h2>"Detection Engine"</h2></div>
                                <div class="settings-fields">
                                    <div class="strategy-picker settings-wide" role="radiogroup" aria-label="Detection engine">
                                        <label class=move || format!("strategy-option{}", if settings.get().strategy == "pixel_seek" { " selected" } else { "" })>
                                            <input type="radio" name="detection-engine" value="pixel_seek" prop:checked=move || settings.get().strategy == "pixel_seek" on:change=move |_| settings.update(|value| value.strategy = "pixel_seek".into())/>
                                            <span class="strategy-radio" aria-hidden="true"/><span class="strategy-copy"><strong>"Pixel Seek"</strong></span>
                                        </label>
                                        <label class=move || format!("strategy-option{}", if settings.get().strategy == "object_detection" { " selected" } else { "" })>
                                            <input type="radio" name="detection-engine" value="object_detection" prop:checked=move || settings.get().strategy == "object_detection" on:change=move |_| settings.update(|value| value.strategy = "object_detection".into())/>
                                            <span class="strategy-radio" aria-hidden="true"/><span class="strategy-copy"><strong>"Object Detection"</strong></span>
                                        </label>
                                    </div>
                                    <Show when=move || settings.get().strategy == "pixel_seek">
                                        <div class="pixel-color-picker settings-wide">
                                            <div class="color-options" role="group" aria-label="Pixel Seek target color">
                                                {["yellow", "red", "purple"].into_iter().map(|color| {
                                                    let selected = move || settings.get().pixel_color == color;
                                                    let label = title_case(color);
                                                    view! {
                                                        <button type="button" aria-pressed=selected class=move || format!("color-option color-{color}{}", if selected() { " selected" } else { "" }) on:click=move |_| {
                                                            settings.update(|value| value.pixel_color = color.into());
                                                            send_command_from(command_sender, "set_pixel_color", serde_json::json!({"color":color}));
                                                        }><i aria-hidden="true"/>{label}</button>
                                                    }
                                                }).collect_view()}
                                            </div>
                                        </div>
                                    </Show>
                                    <Show when=move || settings.get().strategy == "object_detection">
                                        <>
                                            <label class="field settings-wide model-confidence">
                                                <span class="range-heading"><span class="field-label">"Min confidence"</span><output>{move || format!("{:.2}", parse_float(&settings.get().model_confidence).unwrap_or(0.5))}</output></span>
                                                <input type="range" min="0" max="1" step="0.01" prop:value=move || settings.get().model_confidence on:input=move |event| settings.update(|value| value.model_confidence = event_target_value(&event)) aria-label="Minimum confidence" on:change=move |_| update_active_model(command_sender, settings, "confidence")/>
                                            </label>
                                            <label class="field"><span class="field-label">"Active model"</span>
                                                <select prop:value=move || settings.get().active_model_id.unwrap_or_default() on:change=move |event| {
                                                    let id = event_target_value(&event);
                                                    if !id.is_empty() { send_command_from(command_sender, "activate_model", serde_json::json!({"id":id})); }
                                                }>
                                                    <option value="">{move || if models.get().is_empty() { "No model imported" } else { "Select a model" }}</option>
                                                    {move || models.get().into_iter().map(|model| view! { <option value=model.id>{model.name}</option> }).collect_view()}
                                                </select>
                                            </label>
                                            <label class="text-button model-upload">"Import model bundle"<input type="file" multiple accept=".onnx,.engine,.plan,.xml,.bin" on:change=move |event| {
                                                let input = event.target().and_then(|target| target.dyn_into::<HtmlInputElement>().ok());
                                                if let Some(input) = input {
                                                    if let Some(files) = input.files() {
                                                        let selected = (0..files.length()).filter_map(|index| files.get(index)).collect::<Vec<_>>();
                                                        if !selected.is_empty() {
                                                            import_model(selected, settings, command_sender, apply_message);
                                                        }
                                                    }
                                                    input.set_value("");
                                                }
                                            }/></label>
                                            <label class="field"><span class="field-label">"Backend"</span>
                                                <select prop:value=move || settings.get().model_backend on:change=move |event| settings.update(|value| value.model_backend = event_target_value(&event))>
                                                    <option value="onnxruntime">"ONNX Runtime"</option><option value="tensorrt">"TensorRT"</option><option value="openvino">"OpenVINO"</option>
                                                </select>
                                            </label>
                                            <label class="field"><span class="field-label">"Precision"</span>
                                                <select prop:value=move || settings.get().model_precision on:change=move |event| settings.update(|value| value.model_precision = event_target_value(&event))>
                                                    <option value="fp16">"FP16"</option><option value="int8">"INT8"</option>
                                                </select>
                                            </label>
                                            <label class="field settings-wide"><span class="field-label">"Compute"</span>
                                                <select prop:value=move || settings.get().compute_target on:change=move |event| settings.update(|value| value.compute_target = event_target_value(&event))>
                                                    <option value="cpu">"CPU"</option><option value="gpu">"GPU"</option><option value="npu">"NPU"</option>
                                                </select>
                                            </label>
                                        </>
                                    </Show>
                                    <Show when=move || active_model().is_some() && settings.get().strategy == "object_detection">
                                        <div class="model-config settings-wide">
                                            <label class="field"><span class="field-label">"Input width"</span><input class="text-input" inputmode="numeric" prop:value=move || settings.get().model_width on:input=move |event| settings.update(|value| value.model_width = event_target_value(&event)) on:blur=move |_| update_active_model(command_sender, settings, "input_width")/></label>
                                            <label class="field"><span class="field-label">"Input height"</span><input class="text-input" inputmode="numeric" prop:value=move || settings.get().model_height on:input=move |event| settings.update(|value| value.model_height = event_target_value(&event)) on:blur=move |_| update_active_model(command_sender, settings, "input_height")/></label>
                                            <label class="field"><span class="field-label">"IoU"</span><input class="text-input" type="number" min="0" max="1" step="0.01" prop:value=move || settings.get().model_iou on:input=move |event| settings.update(|value| value.model_iou = event_target_value(&event)) on:blur=move |_| update_active_model(command_sender, settings, "iou")/></label>
                                            <span class="readout settings-wide model-path">{move || active_model().map(|model| selected_artifact_path(&model, &settings.get().model_backend)).unwrap_or_default()}</span>
                                            <button type="button" class="text-button remove-model settings-wide" on:click=move |_| remove_active_model(command_sender, settings)>"Remove model from registry"</button>
                                        </div>
                                    </Show>
                                </div>
                            </div>
                            <div class="settings-row target-offset-row">
                                <div class="setting-description"><h2>"Target Offset"</h2></div>
                                <div class="settings-fields target-offset-fields">
                                    <label class="field"><span class="field-label">"X Offset (px)"</span><input class="text-input" type="number" step="1" prop:value=offset_x_draft on:input=move |event| { offset_x_draft.set(event_target_value(&event)); apply_message.set(String::new()); }/></label>
                                    <label class="field"><span class="field-label">"Y Offset (px)"</span><input class="text-input" type="number" step="1" prop:value=offset_y_draft on:input=move |event| { offset_y_draft.set(event_target_value(&event)); apply_message.set(String::new()); }/></label>
                                    <div class="target-offset-map settings-wide" role="img" aria-label=move || format!("Target offset preview: X {}, Y {} pixels", offset_x_draft.get(), offset_y_draft.get())>
                                        <span class="offset-origin" aria-hidden="true"><i/></span>
                                        <span class="offset-point" aria-hidden="true" style:left=move || format!("clamp(8px, calc(50% + {}px), calc(100% - 8px))", marker_x()) style:top=move || format!("clamp(8px, calc(50% + {}px), calc(100% - 8px))", marker_y())/>
                                        <output class="offset-readout">{move || format!("{:+}, {:+}", parse_integer(&offset_x_draft.get()).unwrap_or(0), parse_integer(&offset_y_draft.get()).unwrap_or(0))}</output>
                                    </div>
                                </div>
                            </div>
                            <div class="settings-row smoothing-settings-row">
                                <div class="setting-description"><h2>"Exponential Smoothing"</h2></div>
                                <div class="settings-fields smoothing-fields">
                                    <label class="field smooth-control"><span class="range-heading"><span class="field-label">"X Smoothing (α)"</span><output>{move || format!("{:.2}", settings.get().smoothing_alpha_x)}</output></span>
                                        <input type="range" min="0.01" max="1" step="0.01" prop:value=move || settings.get().smoothing_alpha_x.to_string() on:input=move |event| if let Ok(value) = event_target_value(&event).parse() { settings.update(|settings| settings.smoothing_alpha_x = value) } aria-label="X smoothing alpha"/>
                                    </label>
                                    <label class="field smooth-control"><span class="range-heading"><span class="field-label">"Y Smoothing (α)"</span><output>{move || format!("{:.2}", settings.get().smoothing_alpha_y)}</output></span>
                                        <input type="range" min="0.01" max="1" step="0.01" prop:value=move || settings.get().smoothing_alpha_y.to_string() on:input=move |event| if let Ok(value) = event_target_value(&event).parse() { settings.update(|settings| settings.smoothing_alpha_y = value) } aria-label="Y smoothing alpha"/>
                                    </label>
                                    <p class="smoothing-hint settings-wide">"Lower alpha smooths more; higher alpha follows the target faster."</p>
                                </div>
                            </div>
                            <div class="settings-row">
                                <div class="setting-description"><h2>"Movement Calibration"</h2></div>
                                <div class="settings-fields">
                                    <label class="field"><span class="field-label">"Sensitivity"</span><input class="text-input" type="number" min="0.01" max="10" step="0.001" prop:value=sens_draft on:input=move |event| sens_draft.set(event_target_value(&event)) aria-label="Mouse sensitivity"/></label>
                                    <label class="field"><span class="field-label">"DPI"</span><input class="text-input" type="number" min="100" max="100000" step="1" prop:value=dpi_draft on:input=move |event| dpi_draft.set(event_target_value(&event)) aria-label="Mouse DPI"/></label>
                                    <p class="smoothing-hint settings-wide">"Used in the sensitivity/DPI movement conversion formula."</p>
                                </div>
                            </div>
                            <div class="settings-row">
                                <div class="setting-description"><h2>"Vertical Adjustment"</h2></div>
                                <div class="settings-fields">
                                    <label class="motion-toggle settings-wide"><span>"Enable sampled Y adjustment"</span><input type="checkbox" prop:checked=move || settings.get().recoil.recoil_enabled on:change=move |_| settings.update(|value| value.recoil.recoil_enabled = !value.recoil.recoil_enabled)/></label>
                                    <label class="field settings-wide"><span class="field-label">"Levels (comma-separated)"</span><input class="text-input" type="text" prop:value=recoil_levels_draft on:input=move |event| recoil_levels_draft.set(event_target_value(&event)) aria-label="Vertical adjustment levels"/></label>
                                    <label class="field"><span class="field-label">"Start delay (ms)"</span><input class="text-input" type="number" min="0" max="60000" step="1" prop:value=move || settings.get().recoil.recoil_start_delay_ms.to_string() on:input=move |event| { if let Ok(value) = event_target_value(&event).parse() { settings.update(|settings| settings.recoil.recoil_start_delay_ms = value) } }/></label>
                                    <label class="field"><span class="field-label">"Time per level (ms)"</span><input class="text-input" type="number" min="0" max="5000" step="1" prop:value=move || settings.get().recoil.recoil_level_duration_ms.to_string() on:input=move |event| { if let Ok(value) = event_target_value(&event).parse() { settings.update(|settings| settings.recoil.recoil_level_duration_ms = value) } }/></label>
                                    <label class="field smooth-control"><span class="range-heading"><span class="field-label">"Strength (%)"</span><output>{move || settings.get().recoil.recoil_percentage}</output></span>
                                        <input type="range" min="0" max="100" step="1" prop:value=move || settings.get().recoil.recoil_percentage.to_string() on:input=move |event| { if let Ok(value) = event_target_value(&event).parse::<u8>() { settings.update(|settings| settings.recoil.recoil_percentage = value) } } aria-label="Vertical adjustment strength"/>
                                    </label>
                                    <label class="motion-toggle"><span>"Allow early resampling"</span><input type="checkbox" prop:checked=move || settings.get().recoil.recoil_allow_early_resample on:change=move |_| settings.update(|value| value.recoil.recoil_allow_early_resample = !value.recoil.recoil_allow_early_resample)/></label>
                                    <p class="smoothing-hint settings-wide">"The selected physical trigger starts the elapsed-time profile. Values are sampled by level and added to the Y delta."</p>
                                </div>
                            </div>
                            <div class="settings-row fov-settings-row">
                                <div class="setting-description"><h2>"FOV"</h2></div>
                                <div class="settings-fields">
                                    <label class="field fov-size settings-wide"><span class="range-heading"><span class="field-label">"Size"</span><output>{move || format!("{}°", settings.get().fov_degrees)}</output></span>
                                        <input type="range" min="1" max="360" step="1" prop:value=move || settings.get().fov_degrees.to_string() on:input=move |event| if let Ok(value) = event_target_value(&event).parse() { settings.update(|settings| settings.fov_degrees = value) } aria-label="FOV size in degrees"/>
                                    </label>
                                </div>
                            </div>
                        </div>
                    </div>
                    <Show when=move || !backend_error.get().is_empty()>
                        <p class="inline-error" role="status">{move || format!("{}{}", backend_error.get(), if is_udp_bind_conflict(&backend_error.get()) { " · Retrying every 3 seconds" } else { "" })}</p>
                    </Show>
                </section>
            </div>
        </main>
    }
}

fn copy_state_to_settings(
    state: &BackendState,
    settings: RwSignal<AppSettings>,
    port: RwSignal<String>,
    crop: RwSignal<String>,
    offset_x: RwSignal<String>,
    offset_y: RwSignal<String>,
    sens: RwSignal<String>,
    dpi: RwSignal<String>,
    recoil_levels: RwSignal<String>,
) {
    settings.update(|value| {
        value.strategy = state.mode.clone();
        value.fov_degrees = state.fov_degrees.clamp(1, 360);
        value.offset_x = state.target_offset_x;
        value.offset_y = state.target_offset_y;
        value.trigger_button = state.trigger_button.clone();
        value.smoothing_alpha_x = state.smoothing_alpha_x;
        value.smoothing_alpha_y = state.smoothing_alpha_y;
        value.sens = state.sens;
        value.dpi = state.dpi;
        value.recoil = state.recoil.clone();
        value.scale = state.preview_scale.clone();
        value.crop_resolution = state.crop_resolution;
        value.pixel_color = state.pixel_color.clone();
        if let Some(port) = state.port {
            value.port = port.to_string();
        }
        value.active_model_id = state.active_model_id.clone();
    });
    if let Some(value) = state.port {
        port.set(value.to_string());
    }
    crop.set(state.crop_resolution.to_string());
    offset_x.set(format_number(state.target_offset_x));
    offset_y.set(format_number(state.target_offset_y));
    sens.set(format_number(state.sens));
    dpi.set(state.dpi.to_string());
    recoil_levels.set(format_recoil_levels(&state.recoil.recoil_levels));
}

fn send_settings_commands(
    sender: futures_channel::mpsc::UnboundedSender<String>,
    settings: &AppSettings,
    has_model: bool,
    request_id: &str,
) -> bool {
    let command = |kind: &str, fields: serde_json::Value| {
        let mut value = fields.as_object().cloned().unwrap_or_default();
        value.insert("version".into(), serde_json::json!(1));
        value.insert("type".into(), serde_json::json!(kind));
        serde_json::Value::Object(value)
    };
    let mut commands = vec![
        command(
            "set_udp_port",
            serde_json::json!({"port":settings.port.parse::<u16>().unwrap_or(4200)}),
        ),
        command(
            "set_pixel_color",
            serde_json::json!({"color":settings.pixel_color}),
        ),
        command(
            "set_display_config",
            serde_json::json!({"fov_degrees":settings.fov_degrees,"preview_scale":settings.scale}),
        ),
        command(
            "set_target_offset",
            serde_json::json!({"x":settings.offset_x,"y":settings.offset_y}),
        ),
        command(
            "set_trigger_button",
            serde_json::json!({"button":settings.trigger_button}),
        ),
        command(
            "set_smoothing_config",
            serde_json::json!({"alpha_x":settings.smoothing_alpha_x,"alpha_y":settings.smoothing_alpha_y}),
        ),
        command(
            "set_movement_calibration",
            serde_json::json!({"sens":settings.sens,"dpi":settings.dpi}),
        ),
        command(
            "set_recoil",
            serde_json::json!({"settings":settings.recoil}),
        ),
        command(
            "set_crop_resolution",
            serde_json::json!({"resolution":settings.crop_resolution}),
        ),
        command(
            "set_inference_config",
            serde_json::json!({"backend":settings.model_backend,"compute_target":settings.compute_target}),
        ),
    ];
    if settings.strategy == "pixel_seek" || has_model {
        commands.push(command(
            "set_strategy",
            serde_json::json!({"mode":settings.strategy}),
        ));
    }
    if has_model {
        if let Some(id) = settings.active_model_id.as_deref() {
            commands.push(command(
                "update_model",
                serde_json::json!({
                    "id":id,
                    "input_width":settings.model_width.parse::<u32>().unwrap_or(640),
                    "input_height":settings.model_height.parse::<u32>().unwrap_or(640),
                    "confidence":parse_float(&settings.model_confidence).unwrap_or(0.5),
                    "iou":parse_float(&settings.model_iou).unwrap_or(0.45)
                }),
            ));
            commands.push(command("activate_model", serde_json::json!({"id":id})));
        }
    }
    send_command(
        &sender,
        "apply_settings",
        serde_json::json!({"commands":commands,"request_id":request_id}),
    )
}

fn send_command_from(
    sender: RwSignal<Option<futures_channel::mpsc::UnboundedSender<String>>>,
    kind: &str,
    fields: serde_json::Value,
) {
    if let Some(sender) = sender.get_untracked() {
        send_command(&sender, kind, fields);
    }
}

fn change_udp_port(
    sender: RwSignal<Option<futures_channel::mpsc::UnboundedSender<String>>>,
    draft: RwSignal<String>,
) {
    if let Ok(port) = draft.get_untracked().parse::<u16>() {
        if port > 0 {
            send_command_from(sender, "set_udp_port", serde_json::json!({"port":port}));
        }
    }
}

fn update_active_model(
    sender: RwSignal<Option<futures_channel::mpsc::UnboundedSender<String>>>,
    settings: RwSignal<AppSettings>,
    field: &str,
) {
    let current = settings.get_untracked();
    let Some(id) = current.active_model_id else {
        return;
    };
    let (key, value) = match field {
        "confidence" => (
            "confidence",
            serde_json::json!(parse_float(&current.model_confidence).unwrap_or(0.5)),
        ),
        "iou" => (
            "iou",
            serde_json::json!(parse_float(&current.model_iou).unwrap_or(0.45)),
        ),
        "input_width" => (
            "input_width",
            serde_json::json!(current.model_width.parse::<u32>().unwrap_or(640)),
        ),
        "input_height" => (
            "input_height",
            serde_json::json!(current.model_height.parse::<u32>().unwrap_or(640)),
        ),
        _ => return,
    };
    send_command_from(
        sender,
        "update_model",
        serde_json::json!({"id":id,key:value}),
    );
}

fn remove_active_model(
    sender: RwSignal<Option<futures_channel::mpsc::UnboundedSender<String>>>,
    settings: RwSignal<AppSettings>,
) {
    if let Some(id) = settings.get_untracked().active_model_id {
        send_command_from(sender, "remove_model", serde_json::json!({"id":id}));
        send_command_from(sender, "state", serde_json::json!({}));
    }
}

fn import_model(
    files: Vec<File>,
    settings: RwSignal<AppSettings>,
    sender: RwSignal<Option<futures_channel::mpsc::UnboundedSender<String>>>,
    message: RwSignal<String>,
) {
    spawn_local(async move {
        let selected = files
            .iter()
            .map(|file| (file.clone(), file_extension(&file.name())))
            .collect::<Vec<_>>();
        let onnx_count = selected.iter().filter(|(_, ext)| ext == "onnx").count();
        let tensorrt_count = selected
            .iter()
            .filter(|(_, ext)| ext == "engine" || ext == "plan")
            .count();
        let xml_count = selected.iter().filter(|(_, ext)| ext == "xml").count();
        let bin_count = selected.iter().filter(|(_, ext)| ext == "bin").count();
        if (onnx_count, tensorrt_count, xml_count, bin_count) != (1, 1, 1, 1) || selected.len() != 4
        {
            message.set("Select one .onnx, one TensorRT .engine or .plan, and the matching OpenVINO .xml and .bin files.".into());
            return;
        }
        let xml_name = selected
            .iter()
            .find(|(_, ext)| ext == "xml")
            .map(|(file, _)| file.name());
        let bin_name = selected
            .iter()
            .find(|(_, ext)| ext == "bin")
            .map(|(file, _)| file.name());
        if xml_name.as_deref().and_then(file_stem) != bin_name.as_deref().and_then(file_stem) {
            message.set("The OpenVINO .xml and .bin files must share the same base name.".into());
            return;
        }
        let bundle_id = web_sys::window()
            .and_then(|window| window.crypto().ok())
            .map(|crypto| crypto.random_uuid())
            .unwrap_or_else(|| {
                format!(
                    "00000000-0000-4000-8000-{:012x}",
                    (js_sys::Date::now() as u64) & 0xffff_ffff_ffff
                )
            });
        for (file, _) in &selected {
            let encoded_name = js_sys::encode_uri_component(&file.name())
                .as_string()
                .unwrap_or_else(|| file.name());
            let request = Request::post("/api/upload-model")
                .header("X-Filename", &encoded_name)
                .header("X-Bundle-Id", &bundle_id)
                .body(file.clone());
            let response = match request {
                Ok(request) => request.send().await,
                Err(error) => {
                    cleanup_model_bundle(&bundle_id).await;
                    message.set(format!("Model upload failed: {error}"));
                    return;
                }
            };
            match response {
                Ok(response) if response.ok() => {}
                Ok(response) => {
                    cleanup_model_bundle(&bundle_id).await;
                    message.set(format!("Model upload failed (HTTP {}). Check the local config connection and try again.", response.status()));
                    return;
                }
                Err(error) => {
                    cleanup_model_bundle(&bundle_id).await;
                    message.set(format!("Model upload failed: {error}"));
                    return;
                }
            }
        }
        let commit = Request::post("/api/commit-model-bundle")
            .header("X-Bundle-Id", &bundle_id)
            .body("");
        let response = match commit {
            Ok(request) => request.send().await,
            Err(error) => {
                cleanup_model_bundle(&bundle_id).await;
                message.set(format!("Model bundle could not be committed: {error}"));
                return;
            }
        };
        let response = match response {
            Ok(response) if response.ok() => response,
            Ok(response) => {
                cleanup_model_bundle(&bundle_id).await;
                message.set(format!(
                    "Model bundle could not be committed (HTTP {}).",
                    response.status()
                ));
                return;
            }
            Err(error) => {
                cleanup_model_bundle(&bundle_id).await;
                message.set(format!("Model bundle could not be committed: {error}"));
                return;
            }
        };
        let artifacts = match response.json::<UploadResponse>().await {
            Ok(value) => value.artifacts,
            Err(error) => {
                cleanup_model_bundle(&bundle_id).await;
                message.set(format!("Model bundle response was invalid: {error}"));
                return;
            }
        };
        let mut value = settings.get_untracked();
        let id = web_sys::window()
            .and_then(|window| window.crypto().ok())
            .map(|crypto| crypto.random_uuid())
            .unwrap_or_else(|| format!("sysx-{}", js_sys::Date::now() as u64));
        let model_name = selected
            .iter()
            .find(|(_, extension)| extension == "onnx")
            .and_then(|(file, _)| file_stem(&file.name()).map(str::to_owned))
            .unwrap_or_else(|| "Model".into());
        let width = value.model_width.parse::<u32>().unwrap_or(640);
        let height = value.model_height.parse::<u32>().unwrap_or(640);
        let confidence = parse_float(&value.model_confidence).unwrap_or(0.5);
        let iou = parse_float(&value.model_iou).unwrap_or(0.45);
        send_command_from(
            sender,
            "add_model",
            serde_json::json!({
                "id":id,"name":model_name,"artifacts":artifacts,"input_width":width,"input_height":height,
                "confidence":confidence,"iou":iou,"device":"auto","enabled":true
            }),
        );
        send_command_from(sender, "activate_model", serde_json::json!({"id":id}));
        value.active_model_id = Some(id);
        settings.set(value);
        message.set("Model bundle imported. Apply Changes to use the selected backend.".into());
    });
}

#[derive(Deserialize)]
struct UploadResponse {
    artifacts: ModelArtifacts,
}

fn file_extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

fn file_stem(filename: &str) -> Option<&str> {
    let (stem, _) = filename.rsplit_once('.')?;
    (!stem.is_empty()).then_some(stem)
}

fn selected_artifact_path(model: &BackendModel, backend: &str) -> String {
    match backend {
        "onnxruntime" => model.artifacts.onnx.clone(),
        "tensorrt" => model.artifacts.tensorrt.clone(),
        "openvino" => model.artifacts.openvino_xml.clone(),
        _ => String::new(),
    }
}

async fn cleanup_model_bundle(bundle_id: &str) {
    let _ = Request::delete(&format!("/api/model-bundles/{bundle_id}"))
        .send()
        .await;
}

fn parse_integer(value: &str) -> Option<i64> {
    value.trim().parse().ok()
}

fn parse_float(value: &str) -> Option<f64> {
    let value = value.trim().parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}

fn is_udp_bind_conflict(message: &str) -> bool {
    let normalized = message.to_lowercase();
    normalized.contains("address already in use")
        || normalized.contains("only one usage of each socket address")
}
