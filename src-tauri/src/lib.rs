use serde::Serialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Manager, State};

struct BackendState {
    token: String,
    port: Mutex<Option<u16>>,
    startup_error: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
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

    loop {
        if let Some(event) = events.recv_timeout(Duration::from_millis(250)) {
            let usage = match event.input {
                Input::Press(usage) | Input::Release(usage) => usage,
                Input::Motion { .. } => continue,
            };
            if usage.class == Class::Button && usage.id < 5 {
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
        if !events.is_connected() {
            break;
        }
    }
    set_device_status(app, empty_device_status("missing"));
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

    let Ok(mut connection) = serialport::new(port_name, 115200)
        .timeout(Duration::from_millis(150))
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
    loop {
        match connection.read(&mut chunk) {
            Ok(count) => {
                buffered.extend_from_slice(&chunk[..count]);
                if let Some(mask) = parse_ferrum_button_mask(&buffered) {
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
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
    }
    set_device_status(app, empty_device_status("missing"));
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

fn backend_executable() -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        #[cfg(target_os = "windows")]
        let candidate = root.join(".venv/Scripts/receiver-backend.exe");
        #[cfg(not(target_os = "windows"))]
        let candidate = root.join(".venv/bin/receiver-backend");
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(format!(
                "Backend is not installed at {}",
                candidate.display()
            ))
        };
    }

    let executable_dir = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Cannot locate application executable directory".to_string())?;
    find_sidecar(&executable_dir)
}

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
    let executable = match backend_executable() {
        Ok(path) => path,
        Err(error) => {
            set_startup_error(app, error);
            return;
        }
    };
    let mut backend_command = Command::new(executable);
    backend_command
        .args(["serve", "--port", "0", "--data-dir"])
        .arg(&data_dir)
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
            set_startup_error(app, format!("Could not start the vision backend: {error}"));
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
                                    == Some("ready") =>
                            {
                                if let Some(port) = value
                                    .get("port")
                                    .and_then(|value| value.as_u64())
                                    .and_then(|value| u16::try_from(value).ok())
                                {
                                    if let Some(state) = app_handle.try_state::<BackendState>() {
                                        if let Ok(mut current) = state.port.lock() {
                                            *current = Some(port);
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
                    if let Ok(mut port) = state.port.lock() {
                        *port = None;
                    }
                    if let Ok(mut error) = state.startup_error.lock() {
                        if error.is_none() {
                            *error = Some("Vision backend exited".to_string());
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
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            launch_backend(app.handle());
            start_device_monitor(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend_connection_info,
            device_status
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Receiver desktop application");
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            stop_backend(handle);
        }
    });
}
