use crate::protocol::{BackendEvent, BackendModel, BackendState, ConnectionInfo, LiveFrame};
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::{select, FutureExt, SinkExt, StreamExt};
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, Uint8Array};
use std::collections::VecDeque;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Blob, UrlSearchParams};

pub fn command_channel() -> (UnboundedSender<String>, UnboundedReceiver<String>) {
    unbounded()
}

pub fn send_command(
    sender: &UnboundedSender<String>,
    kind: &str,
    fields: serde_json::Value,
) -> bool {
    let mut command = fields.as_object().cloned().unwrap_or_default();
    command.insert("version".into(), serde_json::json!(1));
    command.insert("type".into(), serde_json::json!(kind));
    sender
        .unbounded_send(serde_json::Value::Object(command).to_string())
        .is_ok()
}

/// `role` is sent to the backend: `"monitor"` for the receiver window (counted
/// so the native host can exit when it closes), `"config"` for the settings page.
pub fn start_backend_client(
    role: &'static str,
    commands: UnboundedReceiver<String>,
    on_event: impl Fn(BackendEvent) + 'static,
) {
    spawn_local(run_backend_client(role, commands, Box::new(on_event)));
}

async fn run_backend_client(
    role: &'static str,
    mut commands: UnboundedReceiver<String>,
    on_event: Box<dyn Fn(BackendEvent)>,
) {
    let mut recent_preview_times = VecDeque::with_capacity(128);
    let mut pending_commands = VecDeque::new();
    loop {
        let connection = match connection_info().await {
            Ok(value) => value,
            Err(error) => {
                on_event(BackendEvent::Error(error));
                if wait_before_reconnect(&mut commands, &mut pending_commands).await {
                    break;
                }
                continue;
            }
        };
        let token = js_sys::encode_uri_component(&connection.token)
            .as_string()
            .unwrap_or_default();
        let address = format!(
            "ws://{}:{}/receiver?token={token}&role={role}",
            connection.host, connection.port
        );
        let socket = match WebSocket::open(&address) {
            Ok(value) => value,
            Err(error) => {
                on_event(BackendEvent::Error(error.to_string()));
                if wait_before_reconnect(&mut commands, &mut pending_commands).await {
                    break;
                }
                continue;
            }
        };
        let (mut writer, mut reader) = socket.split();
        on_event(BackendEvent::Ready);
        let _ = writer
            .send(Message::Text(
                serde_json::json!({"version":1,"type":"state"}).to_string(),
            ))
            .await;
        let _ = writer
            .send(Message::Text(
                serde_json::json!({"version":1,"type":"list_models"}).to_string(),
            ))
            .await;
        while let Some(command) = pending_commands.pop_front() {
            if writer.send(Message::Text(command)).await.is_err() {
                break;
            }
        }
        let mut connected = true;
        while connected {
            let incoming = reader.next().fuse();
            let outgoing = commands.next().fuse();
            futures_util::pin_mut!(incoming, outgoing);
            select! {
                message = incoming => match message {
                    Some(Ok(Message::Text(text))) => parse_text_event(&text, &*on_event),
                    Some(Ok(Message::Bytes(bytes))) => {
                        if let Some((frame, image, fps)) = decode_frame(&bytes, &mut recent_preview_times) {
                            on_event(BackendEvent::Frame(frame, image, fps));
                        }
                    }
                    Some(Err(error)) => {
                        on_event(BackendEvent::Error(error.to_string()));
                        connected = false;
                    }
                    None => connected = false,
                },
                outgoing_command = outgoing => match outgoing_command {
                    Some(command) if command == "\0" => return,
                    Some(command) => {
                        if writer.send(Message::Text(command)).await.is_err() {
                            connected = false;
                        }
                    }
                    None => return,
                },
            }
        }
        on_event(BackendEvent::Disconnected);
        if wait_before_reconnect(&mut commands, &mut pending_commands).await {
            break;
        }
    }
}

async fn wait_before_reconnect(
    commands: &mut UnboundedReceiver<String>,
    pending: &mut VecDeque<String>,
) -> bool {
    let next = commands.next().fuse();
    let delay = TimeoutFuture::new(1000).fuse();
    futures_util::pin_mut!(next, delay);
    select! {
        command = next => match command {
            None => true,
            Some(value) if value == "\0" => true,
            Some(value) => {
                pending.push_back(value);
                false
            }
        },
        _ = delay => false,
    }
}

async fn connection_info() -> Result<ConnectionInfo, String> {
    let search = web_sys::window()
        .and_then(|window| window.location().search().ok())
        .unwrap_or_default();
    if let Ok(params) = UrlSearchParams::new_with_str(&search) {
        if let (Some(port), Some(token)) = (params.get("backendPort"), params.get("token")) {
            if let Ok(port) = port.parse::<u16>() {
                if port > 0 {
                    return Ok(ConnectionInfo {
                        host: "127.0.0.1".into(),
                        port,
                        token,
                    });
                }
            }
        }
    }
    let response = Request::get("/api/session")
        .send()
        .await
        .map_err(|error| format!("Local configuration service is unavailable: {error}"))?;
    if !response.ok() {
        return Err("Local configuration service is unavailable".into());
    }
    response
        .json::<ConnectionInfo>()
        .await
        .map_err(|error| error.to_string())
}

pub async fn get_local_settings<T: serde::de::DeserializeOwned>() -> Result<T, String> {
    let response = Request::get("/api/settings")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err("Could not load saved settings".into());
    }
    response
        .json::<T>()
        .await
        .map_err(|error| error.to_string())
}

pub async fn save_local_settings<T: serde::Serialize>(settings: &T) -> Result<(), String> {
    let response = Request::put("/api/settings")
        .json(settings)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.ok() {
        Ok(())
    } else {
        Err("Local settings could not be saved".into())
    }
}

/// The native host opens the receiver window at `/monitor.html`; every other
/// path is the configuration page.
pub fn is_monitor_page() -> bool {
    web_sys::window()
        .and_then(|window| window.location().pathname().ok())
        .is_some_and(|path| path.ends_with("/monitor.html"))
}

fn parse_text_event(text: &str, on_event: &dyn Fn(BackendEvent)) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        on_event(BackendEvent::Error("Backend sent invalid JSON".into()));
        return;
    };
    match value.get("event").and_then(serde_json::Value::as_str) {
        Some("state") => match serde_json::from_value::<BackendState>(value) {
            Ok(state) => on_event(BackendEvent::State(state)),
            Err(error) => on_event(BackendEvent::Error(error.to_string())),
        },
        Some("models") => {
            let models = serde_json::from_value::<Vec<BackendModel>>(value["models"].clone());
            match models {
                Ok(models) => on_event(BackendEvent::Models(
                    models,
                    value["active_model_id"].as_str().map(str::to_owned),
                )),
                Err(error) => on_event(BackendEvent::Error(error.to_string())),
            }
        }
        Some("error") => {
            let message = value["message"]
                .as_str()
                .unwrap_or("Backend error")
                .to_string();
            if let (Some(request_id), Some(command_type)) =
                (value["request_id"].as_str(), value["type"].as_str())
            {
                on_event(BackendEvent::CommandResponse {
                    request_id: request_id.into(),
                    command_type: command_type.into(),
                    error: Some(message),
                });
            } else {
                on_event(BackendEvent::Error(message));
            }
        }
        Some("ack") => {
            if let (Some(request_id), Some(command_type)) =
                (value["request_id"].as_str(), value["type"].as_str())
            {
                on_event(BackendEvent::CommandResponse {
                    request_id: request_id.into(),
                    command_type: command_type.into(),
                    error: None,
                });
            }
            if let Ok(state) = serde_json::from_value::<BackendState>(value["state"].clone()) {
                on_event(BackendEvent::State(state));
            }
        }
        _ => {}
    }
}

fn decode_frame(bytes: &[u8], times: &mut VecDeque<f64>) -> Option<(LiveFrame, Blob, Option<f64>)> {
    if bytes.len() < 5 {
        return None;
    }
    let metadata_len = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    let metadata_end = 4_usize.checked_add(metadata_len)?;
    if metadata_end >= bytes.len() {
        return None;
    }
    // Binary messages only carry frames; parse the metadata once, directly.
    let frame: LiveFrame = serde_json::from_slice(bytes.get(4..metadata_end)?).ok()?;
    let image = Uint8Array::from(bytes.get(metadata_end..)?);
    let parts = Array::new();
    parts.push(image.as_ref());
    let blob = Blob::new_with_u8_array_sequence(&parts).ok()?;
    let now = web_sys::window()
        .and_then(|window| window.performance())
        .map(|performance| performance.now())
        .unwrap_or_else(js_sys::Date::now);
    times.push_back(now);
    while times.front().is_some_and(|time| now - *time > 1000.0) {
        times.pop_front();
    }
    let preview_fps = times
        .front()
        .zip(times.back())
        .filter(|(first, last)| times.len() > 1 && **last > **first)
        .map(|(first, last)| (times.len() - 1) as f64 * 1000.0 / (*last - *first));
    Some((frame, blob, preview_fps))
}
