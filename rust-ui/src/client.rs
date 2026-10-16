use crate::protocol::{BackendEvent, BackendModel, BackendState, ConnectionInfo};
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::{select, FutureExt, SinkExt, StreamExt};
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use gloo_timers::future::TimeoutFuture;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::VecDeque;
use wasm_bindgen_futures::spawn_local;
use web_sys::UrlSearchParams;

const SETTINGS_CACHE_KEY: &str = "sysx.settings.v1";

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
                    Some(Ok(Message::Bytes(_))) => {} // Config never renders preview frames.
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

pub async fn get_local_settings<T: DeserializeOwned>() -> Result<T, String> {
    let api_error = match Request::get("/api/settings").send().await {
        Ok(response) if response.ok() => {
            return response
                .json::<T>()
                .await
                .map_err(|error| error.to_string());
        }
        Ok(_) => "Could not load saved settings".to_string(),
        Err(error) => error.to_string(),
    };
    read_cached_settings().ok_or_else(|| {
        format!(
            "Could not load saved settings: {api_error}. Start SysX once to create the local settings service."
        )
    })
}

pub async fn save_local_settings<T: Serialize>(settings: &T) -> Result<(), String> {
    let payload = serde_json::to_string(settings).map_err(|error| error.to_string())?;
    let api_result = Request::put("/api/settings")
        .header("content-type", "application/json")
        .body(payload.clone())
        .map_err(|error| error.to_string())?
        .send()
        .await;
    let response = api_result.map_err(|error| {
        format!("Settings were not saved. The local configuration service is unavailable: {error}")
    })?;
    let status = response.status();
    let message = if response.ok() {
        String::new()
    } else {
        response.text().await.unwrap_or_default()
    };
    crate::settings::require_settings_saved(status, &message)?;
    // Cache only a confirmed disk save. A browser draft is not a durable save
    // and would be overwritten by the next successful GET of the old file.
    let _ = write_cached_settings(&payload);
    Ok(())
}

fn settings_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn read_cached_settings<T: DeserializeOwned>() -> Option<T> {
    let payload = settings_storage()?.get_item(SETTINGS_CACHE_KEY).ok()??;
    serde_json::from_str(&payload).ok()
}

fn write_cached_settings(payload: &str) -> Result<(), String> {
    settings_storage()
        .ok_or_else(|| "browser storage is unavailable".to_string())?
        .set_item(SETTINGS_CACHE_KEY, payload)
        .map_err(|_| "browser storage rejected the settings".to_string())
}

/// Configuration consumes backend state and acknowledgements only.
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
