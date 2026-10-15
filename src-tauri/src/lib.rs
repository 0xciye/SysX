use serde::Serialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Manager, State};

include!(concat!(env!("OUT_DIR"), "/embedded_runtime.rs"));

mod movement;
#[allow(dead_code)]
mod serial_queue;

const FERRUM_LEGACY_BAUD_RATE: u32 = 115_200;

struct BackendState {
    token: String,
    port: Mutex<Option<u16>>,
    startup_error: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    runtime_dir: Mutex<Option<PathBuf>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStatus {
    state: &'static str,
    name: Option<String>,
    buttons: [Option<bool>; 5],
}

struct DeviceStatusState(Mutex<DeviceStatus>);

#[tauri::command]
fn device_status(state: State<'_, DeviceStatusState>) -> Result<DeviceStatus, String> {
    state
        .0
        .lock()
        .map(|status| status.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn ui_wasm_asset() -> Result<String, String> {
    use base64::Engine;
    let bytes = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../dist/sysx-ui_bg.wasm"));
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn empty_device_status(state: &'static str) -> DeviceStatus {
    DeviceStatus {
        state,
        name: None,
        buttons: [None; 5],
    }
}

fn set_device_status(app: &tauri::AppHandle, status: DeviceStatus) {
    if let Some(state) = app.try_state::<DeviceStatusState>() {
        if let Ok(mut current) = state.0.lock() {
            *current = status;
        }
    }
}

fn monitor_medius(app: &tauri::AppHandle, path: &str) -> bool {
    use medius::{Button, CatchFilter, Class, Device, Input};

    let Ok(device) = Device::open(path) else {
        return false;
    };
    let Ok(version) = device.query_version() else {
        return false;
    };
    let filters = [
        Button::LEFT,
        Button::RIGHT,
        Button::MIDDLE,
        Button::SIDE1,
        Button::SIDE2,
    ]
    .into_iter()
    .map(CatchFilter::watch);
    let Ok(mut events) = device.input_events(filters) else {
        return false;
    };
    let name = if version.name.trim().is_empty() {
        "Medius".to_string()
    } else {
        version.name
    };
    set_device_status(
        app,
        DeviceStatus {
            state: "connected",
            name: Some(name),
            buttons: [None; 5],
        },
    );

    let mut movement_error = false;
    let mut vertical_adjustment = movement::VerticalAdjustmentClock::default();
    loop {
        if let Some(event) = events.recv_timeout(Duration::from_millis(4)) {
            let usage = match event.input {
                Input::Press(usage) | Input::Release(usage) => Some(usage),
                Input::Motion { .. } => None,
            };
            if usage
                .as_ref()
                .is_some_and(|usage| usage.class == Class::Button && usage.id < 5)
            {
                let mut buttons = [Some(false); 5];
                for held in events.held(Class::Button) {
                    if held.id < 5 {
                        buttons[held.id as usize] = Some(true);
                    }
                }
                if let Some(state) = app.try_state::<DeviceStatusState>() {
                    if let Ok(mut current) = state.0.lock() {
                        current.buttons = buttons;
                    }
                }
            }
        }
        if let Some(target) = movement::take_target(app) {
            let held =
                movement::trigger_button_index(&target.trigger_button).is_some_and(|button| {
                    events
                        .held(Class::Button)
                        .iter()
                        .any(|usage| usage.id == button)
                });
            let now = std::time::Instant::now();
            let recoil =
                vertical_adjustment.sample(held && target.target_valid, &target.recoil, now);
            if held && target.target_valid && events.is_connected() {
                let (dx, dy) = target.device_delta(recoil);
                if (dx != 0 || dy != 0) && device.move_rel(dx, dy).is_err() {
                    movement_error = true;
                    break;
                }
            }
        }
        if !events.is_connected() {
            break;
        }
    }
    set_device_status(
        app,
        empty_device_status(if movement_error { "error" } else { "missing" }),
    );
    true
}

fn parse_ferrum_button_mask(bytes: &[u8]) -> Option<u8> {
    let prefix = b"km.";
    for found in bytes
        .windows(prefix.len())
        .enumerate()
        .filter_map(|(index, window)| (window == prefix).then_some(index))
    {
        let start = found + prefix.len();
        let Some(&mask) = bytes.get(start) else {
            continue;
        };
        if mask <= 0b1_1111 && bytes.get(start + 1..start + 3) == Some(b"\r\n") {
            return Some(mask);
        }
    }
    None
}

fn monitor_ferrum(app: &tauri::AppHandle, port_name: &str) -> bool {
    use std::io::{Read, Write};

    let Ok(mut connection) = serialport::new(port_name, FERRUM_LEGACY_BAUD_RATE)
        .timeout(Duration::from_millis(2))
        .open()
    else {
        return false;
    };
    if connection.write_all(b"km.version()\n").is_err() {
        return false;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut response = Vec::with_capacity(256);
    let mut chunk = [0_u8; 128];
    while std::time::Instant::now() < deadline && response.len() < 1024 {
        match connection.read(&mut chunk) {
            Ok(count) => {
                response.extend_from_slice(&chunk[..count]);
                if ferrum_device_name(&String::from_utf8_lossy(&response)).is_some() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return false,
        }
    }
    let Some(name) = ferrum_device_name(&String::from_utf8_lossy(&response)) else {
        return false;
    };
    if connection.write_all(b"km.buttons(1)\n").is_err() {
        return false;
    }
    set_device_status(
        app,
        DeviceStatus {
            state: "connected",
            name: Some(name),
            buttons: [None; 5],
        },
    );
    let mut buffered = Vec::with_capacity(128);
    let mut last_mask = 0_u8;
    let mut movement_error = false;
    let mut vertical_adjustment = movement::VerticalAdjustmentClock::default();
    loop {
        match connection.read(&mut chunk) {
            Ok(count) => {
                buffered.extend_from_slice(&chunk[..count]);
                if let Some(mask) = parse_ferrum_button_mask(&buffered) {
                    last_mask = mask;
                    let buttons = std::array::from_fn(|index| Some(mask & (1 << index) != 0));
                    if let Some(state) = app.try_state::<DeviceStatusState>() {
                        if let Ok(mut current) = state.0.lock() {
                            current.buttons = buttons;
                        }
                    }
                    buffered.clear();
                } else if buffered.len() > 256 {
                    let keep = buffered.split_off(buffered.len() - 8);
                    buffered = keep;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        if let Some(target) = movement::take_target(app) {
            let held = movement::trigger_button_index(&target.trigger_button)
                .is_some_and(|button| last_mask & (1 << button) != 0);
            let now = std::time::Instant::now();
            let recoil =
                vertical_adjustment.sample(held && target.target_valid, &target.recoil, now);
            if held && target.target_valid {
                let (dx, dy) = target.device_delta(recoil);
                if (dx != 0 || dy != 0)
                    && connection
                        .write_all(format!("km.move({dx},{dy})\n").as_bytes())
                        .is_err()
                {
                    movement_error = true;
                    break;
                }
            }
        }
    }
    set_device_status(
        app,
        empty_device_status(if movement_error { "error" } else { "missing" }),
    );
    true
}

fn inspect_devices(app: &tauri::AppHandle) -> bool {
    let medius_ports = medius::find_medius();
    for port in &medius_ports {
        if monitor_medius(app, &port.path) {
            return true;
        }
    }

    let mut ferrum_candidate = false;
    for port in serialport::available_ports().unwrap_or_default() {
        let serialport::SerialPortType::UsbPort(usb) = &port.port_type else {
            continue;
        };
        let is_cp210x = usb.vid == 0x10c4
            && usb
                .product
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("cp210");
        if !is_cp210x {
            continue;
        }
        ferrum_candidate = true;
        if monitor_ferrum(app, &port.port_name) {
            return true;
        }
    }

    set_device_status(
        app,
        empty_device_status(if ferrum_candidate || !medius_ports.is_empty() {
            "error"
        } else {
            "missing"
        }),
    );
    false
}

fn ferrum_device_name(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|line| line.eq_ignore_ascii_case("kmbox: ferrum"))
        .map(|_| "Ferrum".to_string())
}

fn start_device_monitor(app: &tauri::AppHandle) {
    app.manage(DeviceStatusState(Mutex::new(DeviceStatus {
        state: "missing",
        name: None,
        buttons: [None; 5],
    })));
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("receiver-device-monitor".into())
        .spawn(move || loop {
            if inspect_devices(&app) {
                std::thread::sleep(Duration::from_millis(250));
            } else {
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

#[cfg(test)]
mod device_tests {
    use super::{ferrum_device_name, parse_ferrum_button_mask};

    #[test]
    fn identifies_only_a_ferrum_version_reply() {
        assert_eq!(
            ferrum_device_name("km.version()\nkmbox: Ferrum\n>>>\n"),
            Some("Ferrum".into())
        );
        assert_eq!(
            ferrum_device_name("km.version()\nkmbox: Other\n>>>\n"),
            None
        );
        assert_eq!(ferrum_device_name("serial port is open"), None);
    }

    #[test]
    fn parses_ferrum_live_button_bitmap_without_confusing_command_echo() {
        assert_eq!(
            parse_ferrum_button_mask(b"km.buttons(1)\r\n>>> km.\x09\r\n>>> "),
            Some(9)
        );
        assert_eq!(parse_ferrum_button_mask(b"km.buttons(1)\r\n>>> "), None);
        assert_eq!(parse_ferrum_button_mask(b"km. \r\n>>> "), None);
    }
}

#[derive(Serialize)]
struct BackendConnectionInfo {
    host: &'static str,
    port: u16,
    token: String,
}

#[tauri::command]
fn backend_connection_info(
    state: State<'_, BackendState>,
) -> Result<BackendConnectionInfo, String> {
    if let Some(message) = state
        .startup_error
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
    {
        return Err(message);
    }
    let port = state
        .port
        .lock()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Backend is starting".to_string())?;
    Ok(BackendConnectionInfo {
        host: "127.0.0.1",
        port,
        token: state.token.clone(),
    })
}

fn make_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Unable to create local IPC token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(debug_assertions)]
fn backend_executable() -> Result<PathBuf, String> {
    find_sidecar(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries"))
}

#[cfg(not(debug_assertions))]
fn extract_runtime(data_dir: &Path, token: &str) -> Result<(PathBuf, PathBuf), String> {
    if EMBEDDED_BACKEND.is_empty() {
        return Err("The Rust backend was not embedded in this executable".into());
    }
    if !EMBEDDED_CONFIG_ASSETS
        .iter()
        .any(|(name, _)| *name == "index.html")
    {
        return Err("The local configuration UI was not embedded in this executable".into());
    }

    let runtime_dir = data_dir.join(format!("runtime-{}-{}", std::process::id(), &token[..8]));
    fs::create_dir_all(&runtime_dir).map_err(|error| error.to_string())?;
    let extraction = (|| {
        #[cfg(target_os = "windows")]
        let executable = runtime_dir.join("receiver-backend.exe");
        #[cfg(not(target_os = "windows"))]
        let executable = runtime_dir.join("receiver-backend");
        fs::write(&executable, EMBEDDED_BACKEND).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;
        }
        let config_root = runtime_dir.join("web-config");
        for (relative, bytes) in EMBEDDED_CONFIG_ASSETS {
            let destination = config_root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(destination, bytes).map_err(|error| error.to_string())?;
        }
        Ok::<_, String>((executable, runtime_dir.clone()))
    })();
    if extraction.is_err() {
        let _ = fs::remove_dir_all(&runtime_dir);
    }
    extraction
}

fn open_config_browser(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/config.html");
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url.as_str()])
        .spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(&url).spawn();
    result
        .map(|_| ())
        .map_err(|error| format!("Could not open SysX local configuration in the browser: {error}"))
}

#[cfg(any(debug_assertions, test))]
fn find_sidecar(directory: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let candidate = directory.join("receiver-backend-x86_64-pc-windows-msvc.exe");
    #[cfg(target_os = "macos")]
    let candidate = directory.join(format!(
        "receiver-backend-{}-apple-darwin",
        std::env::consts::ARCH
    ));
    #[cfg(target_os = "linux")]
    let candidate = directory.join(format!(
        "receiver-backend-{}-unknown-linux-gnu",
        std::env::consts::ARCH
    ));
    let bundled_candidate = directory.join("receiver-backend");
    let windows_candidate = directory.join("receiver-backend.exe");
    [candidate, bundled_candidate, windows_candidate]
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "Packaged receiver-backend sidecar is missing from {}",
                directory.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::find_sidecar;
    use std::fs;

    #[test]
    fn finds_tauri_bundled_sidecar_name() {
        let directory =
            std::env::temp_dir().join(format!("receiver-sidecar-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let sidecar = directory.join("receiver-backend");
        fs::write(&sidecar, b"sidecar").unwrap();

        assert_eq!(find_sidecar(&directory).unwrap(), sidecar);
        fs::remove_dir_all(directory).unwrap();
    }
}

fn launch_backend(app: &tauri::AppHandle) {
    let token = match make_token() {
        Ok(token) => token,
        Err(message) => {
            if let Some(state) = app.try_state::<BackendState>() {
                *state.startup_error.lock().unwrap() = Some(message);
            }
            return;
        }
    };
    let state = BackendState {
        token: token.clone(),
        port: Mutex::new(None),
        startup_error: Mutex::new(None),
        child: Mutex::new(None),
        runtime_dir: Mutex::new(None),
    };
    app.manage(state);

    let data_dir = match app.path().app_data_dir() {
        Ok(path) => path,
        Err(error) => {
            set_startup_error(app, format!("Cannot find application data folder: {error}"));
            return;
        }
    };
    if let Err(error) = fs::create_dir_all(&data_dir) {
        set_startup_error(
            app,
            format!("Cannot create application data folder: {error}"),
        );
        return;
    }
    #[cfg(debug_assertions)]
    let executable = match backend_executable() {
        Ok(path) => path,
        Err(error) => {
            set_startup_error(app, error);
            return;
        }
    };
    #[cfg(debug_assertions)]
    let config_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist");
    #[cfg(not(debug_assertions))]
    let (executable, config_root) = match extract_runtime(&data_dir, &token) {
        Ok((executable, runtime_dir)) => {
            if let Ok(mut current) = app.state::<BackendState>().runtime_dir.lock() {
                *current = Some(runtime_dir.clone());
            }
            (executable, runtime_dir.join("web-config"))
        }
        Err(error) => {
            set_startup_error(app, format!("Cannot prepare embedded runtime: {error}"));
            return;
        }
    };
    let mut backend_command = Command::new(executable);
    backend_command
        .args(["serve", "--port", "0", "--data-dir"])
        .arg(&data_dir)
        .args(["--config-port", "4201", "--config-root"])
        .arg(&config_root)
        .env("RECEIVER_IPC_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        backend_command.creation_flags(0x08000000); // CREATE_NO_WINDOW; stdout remains piped for the readiness event.
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        backend_command.process_group(0);
    }
    let mut child = match backend_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_startup_error(app, format!("Could not start the Rust backend: {error}"));
            return;
        }
    };
    let stdout = child.stdout.take();
    if let Ok(mut child_state) = app.state::<BackendState>().child.lock() {
        *child_state = Some(child);
    }
    if let Some(stdout) = stdout {
        let app_handle = app.clone();
        std::thread::Builder::new()
            .name("receiver-backend-status".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => match serde_json::from_str::<serde_json::Value>(&line) {
                                Ok(value)
                                    if value.get("event").and_then(|event| event.as_str())
                                        == Some("movement_target") =>
                                {
                                    movement::set_target(&app_handle, &value);
                                }
                                Ok(value)
                                    if value.get("event").and_then(|event| event.as_str())
                                        == Some("ready") =>
                                {
                                    if let Some(port) = value
                                        .get("port")
                                        .and_then(|value| value.as_u64())
                                        .and_then(|value| u16::try_from(value).ok())
                                    {
                                        if let Some(state) = app_handle.try_state::<BackendState>()
                                        {
                                            if let Ok(mut current) = state.port.lock() {
                                                *current = Some(port);
                                            }
                                            if let Ok(mut error) = state.startup_error.lock() {
                                                *error = None;
                                            }
                                            if let Some(config_port) = value
                                                .get("config_port")
                                                .and_then(|value| value.as_u64())
                                                .and_then(|value| u16::try_from(value).ok())
                                            {
                                                if let Err(error) = open_config_browser(config_port)
                                                {
                                                    set_startup_error(&app_handle, error);
                                                }
                                            } else if let Some(error) = value
                                                .get("config_error")
                                                .and_then(|value| value.as_str())
                                            {
                                                set_startup_error(&app_handle, error.to_string());
                                            }
                                        }
                                    }
                                }
                                Ok(value)
                                    if value.get("event").and_then(|event| event.as_str())
                                        == Some("error") =>
                                {
                                    set_startup_error(
                                        &app_handle,
                                        value
                                            .get("message")
                                            .and_then(|m| m.as_str())
                                            .unwrap_or("Backend startup failed")
                                            .to_string(),
                                    );
                                }
                            _ => {}
                        },
                        Err(error) => {
                            set_startup_error(
                                &app_handle,
                                format!("Could not read backend status: {error}"),
                            );
                            break;
                        }
                    }
                }
                if let Some(state) = app_handle.try_state::<BackendState>() {
                    let exited = state
                        .child
                        .lock()
                        .ok()
                        .and_then(|mut child| child.as_mut()?.try_wait().ok())
                        .flatten()
                        .is_some();
                    if exited {
                        if let Ok(mut port) = state.port.lock() {
                            *port = None;
                        }
                        if let Ok(mut error) = state.startup_error.lock() {
                            if error.is_none() {
                                *error = Some("Receiver backend exited".to_string());
                            }
                        }
                    }
                }
            })
            .ok();
    }
}

fn set_startup_error(app: &tauri::AppHandle, message: String) {
    if let Some(state) = app.try_state::<BackendState>() {
        if let Ok(mut error) = state.startup_error.lock() {
            *error = Some(message);
        }
    }
}

fn stop_backend(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<BackendState>() {
        if let Ok(mut child) = state.child.lock() {
            if let Some(child) = child.as_mut() {
                #[cfg(unix)]
                {
                    let process_group = -(child.id() as i32);
                    unsafe {
                        libc::kill(process_group, libc::SIGTERM);
                    }
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while std::time::Instant::now() < deadline {
                        let group_exists = unsafe { libc::kill(process_group, 0) == 0 };
                        if !group_exists {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    unsafe {
                        libc::kill(process_group, libc::SIGKILL);
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = Command::new("taskkill")
                        .args(["/PID", &child.id().to_string(), "/T", "/F"])
                        .status();
                }
                let _ = child.wait();
            }
        }
        if let Ok(mut runtime_dir) = state.runtime_dir.lock() {
            if let Some(path) = runtime_dir.take() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(movement::MovementState::default());
            launch_backend(app.handle());
            start_device_monitor(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend_connection_info,
            device_status,
            ui_wasm_asset
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Receiver desktop application");
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            stop_backend(handle);
        }
    });
}
