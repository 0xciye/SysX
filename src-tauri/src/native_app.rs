//! Native SysX host. It owns everything that must stay native â€” the embedded
//! backend process, Medius/Ferrum device I/O and movement output â€” and shows
//! the receiver UI as the web monitor page (`/monitor.html`) in a chromeless
//! browser app window. The host exits once that window has been closed.
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{connect, Message};

include!(concat!(env!("OUT_DIR"), "/embedded_runtime.rs"));

/// Grace period for a monitor page reload before the host shuts down.
const MONITOR_CLOSE_GRACE: Duration = Duration::from_secs(3);
/// How long to wait for the monitor window to connect at all.
const MONITOR_FIRST_CONNECT: Duration = Duration::from_secs(60);

enum HostEvent {
    MonitorClients(usize),
    Disconnected,
}

struct BackendRuntime {
    child: Child,
    runtime_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceState {
    Missing,
    Connected,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
struct DeviceStatus {
    state: DeviceState,
    name: Option<String>,
    buttons: [Option<bool>; 5],
}

impl DeviceStatus {
    fn missing() -> Self {
        Self {
            state: DeviceState::Missing,
            name: None,
            buttons: [None; 5],
        }
    }

    fn error() -> Self {
        Self {
            state: DeviceState::Error,
            name: None,
            buttons: [None; 5],
        }
    }
}

impl Drop for BackendRuntime {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                .status();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(path) = self.runtime_dir.take() {
            // Windows can hold the just-exited backend image for a moment,
            // leaving an empty directory behind; retry briefly.
            for _ in 0..20 {
                if fs::remove_dir_all(&path).is_ok() || !path.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub fn run() {
    let movement_state = Arc::new(crate::movement::MovementState::default());
    let device_state = Arc::new(Mutex::new(DeviceStatus::missing()));
    let (runtime, port, token, data_dir, config_port) =
        match start_backend(Arc::clone(&movement_state)) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("SysX could not start: {error}");
                return;
            }
        };
    start_device_monitor(Arc::clone(&movement_state), Arc::clone(&device_state));
    let (event_tx, event_rx) = mpsc::channel();
    start_host_client(port, token, Arc::clone(&device_state), event_tx);
    let Some(config_port) = config_port else {
        eprintln!("SysX local web server did not start; no receiver window can be shown");
        return;
    };
    open_monitor_window(config_port, &data_dir);
    open_url(&format!("http://127.0.0.1:{config_port}/config.html"));

    let mut lifecycle = MonitorLifecycle::new(Instant::now());
    loop {
        let count = match event_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(HostEvent::MonitorClients(count)) => Some(count),
            Ok(HostEvent::Disconnected) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => None,
        };
        if lifecycle.should_exit(count, Instant::now()) {
            break;
        }
    }
    // Dropping the runtime stops the backend and removes the extracted files.
    drop(runtime);
}

/// Decides when the receiver window is gone: the host keeps running while at
/// least one monitor page is connected, tolerates a short reload gap, and
/// gives up if no monitor ever connects.
struct MonitorLifecycle {
    started: Instant,
    seen: bool,
    count: usize,
    zero_since: Option<Instant>,
}

impl MonitorLifecycle {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            seen: false,
            count: 0,
            zero_since: None,
        }
    }

    fn should_exit(&mut self, count: Option<usize>, now: Instant) -> bool {
        if let Some(count) = count {
            self.count = count;
        }
        if self.count > 0 {
            self.seen = true;
            self.zero_since = None;
            return false;
        }
        if !self.seen {
            return now.duration_since(self.started) >= MONITOR_FIRST_CONNECT;
        }
        let since = *self.zero_since.get_or_insert(now);
        now.duration_since(since) >= MONITOR_CLOSE_GRACE
    }
}

fn start_backend(
    movement_state: Arc<crate::movement::MovementState>,
) -> Result<(BackendRuntime, u16, String, PathBuf, Option<u16>), String> {
    let token = make_token()?;
    let data_dir = app_data_dir();
    fs::create_dir_all(&data_dir).map_err(|error| format!("Cannot create app data: {error}"))?;
    let (executable, config_root, runtime_dir) = prepare_runtime(&data_dir, &token)?;
    let mut command = Command::new(executable);
    command
        .args(["serve", "--host", "127.0.0.1", "--port", "0", "--data-dir"])
        .arg(&data_dir)
        .args(["--config-port", "4201", "--config-root"])
        .arg(&config_root)
        .env("RECEIVER_IPC_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start Rust backend: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Backend stdout was not available".to_string())?;
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("sysx-backend-status".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if value.get("event").and_then(|value| value.as_str()) == Some("movement_target") {
                    crate::movement::set_target(&movement_state, &value);
                    continue;
                }
                if value.get("event").and_then(|value| value.as_str()) == Some("ready") {
                    if let Some(port) = value.get("port").and_then(|value| value.as_u64()) {
                        let config_port = value
                            .get("config_port")
                            .and_then(|value| value.as_u64())
                            .and_then(|value| u16::try_from(value).ok());
                        let _ = ready_tx.send((port as u16, config_port));
                    }
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let runtime = BackendRuntime { child, runtime_dir };
    let (port, config_port) = ready_rx
        .recv_timeout(Duration::from_secs(12))
        .map_err(|_| "Backend did not become ready within 12 seconds".to_string())?;
    Ok((runtime, port, token, data_dir, config_port))
}

/// Backend connection with `role=host`: no preview frames, pushes device
/// status changes, and reports how many monitor pages are connected.
fn start_host_client(
    port: u16,
    token: String,
    device_state: Arc<Mutex<DeviceStatus>>,
    events: Sender<HostEvent>,
) {
    let _ = thread::Builder::new()
        .name("sysx-host-client".into())
        .spawn(move || {
            let url = format!("ws://127.0.0.1:{port}/receiver?token={token}&role=host");
            let Ok((mut socket, _)) = connect(url) else {
                let _ = events.send(HostEvent::Disconnected);
                return;
            };
            if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
            }
            let mut sent: Option<DeviceStatus> = None;
            loop {
                let device = device_state
                    .lock()
                    .map(|status| status.clone())
                    .unwrap_or_else(|_| DeviceStatus::missing());
                if sent.as_ref() != Some(&device) {
                    let command = device_command(&device).to_string();
                    if socket.send(Message::Text(command.into())).is_err() {
                        break;
                    }
                    sent = Some(device);
                }
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        if let Some(count) = monitor_clients(&text) {
                            let _ = events.send(HostEvent::MonitorClients(count));
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            let _ = events.send(HostEvent::Disconnected);
        });
}

fn device_command(device: &DeviceStatus) -> serde_json::Value {
    let state = match device.state {
        DeviceState::Connected => "connected",
        DeviceState::Missing => "missing",
        DeviceState::Error => "error",
    };
    serde_json::json!({
        "version": 1,
        "type": "device_status",
        "state": state,
        "name": device.name,
        "buttons": device.buttons,
    })
}

fn monitor_clients(text: &str) -> Option<usize> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("event")?.as_str()? != "state" {
        return None;
    }
    usize::try_from(value.get("monitor_clients")?.as_u64()?).ok()
}

const FERRUM_LEGACY_BAUD_RATE: u32 = 115_200;

fn set_device_status(state: &Arc<Mutex<DeviceStatus>>, status: DeviceStatus) {
    if let Ok(mut current) = state.lock() {
        *current = status;
    }
}

fn start_device_monitor(
    movement_state: Arc<crate::movement::MovementState>,
    device_state: Arc<Mutex<DeviceStatus>>,
) {
    let _ = thread::Builder::new()
        .name("sysx-device-monitor".into())
        .spawn(move || loop {
            if inspect_devices(&movement_state, &device_state) {
                thread::sleep(Duration::from_millis(250));
            } else {
                thread::sleep(Duration::from_secs(2));
            }
        });
}

fn inspect_devices(
    movement_state: &crate::movement::MovementState,
    device_state: &Arc<Mutex<DeviceStatus>>,
) -> bool {
    let medius_ports = medius::find_medius();
    for port in &medius_ports {
        if monitor_medius(movement_state, device_state, &port.path) {
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
        if monitor_ferrum(movement_state, device_state, &port.port_name) {
            return true;
        }
    }

    set_device_status(
        device_state,
        if ferrum_candidate || !medius_ports.is_empty() {
            DeviceStatus::error()
        } else {
            DeviceStatus::missing()
        },
    );
    false
}

fn monitor_medius(
    movement_state: &crate::movement::MovementState,
    device_state: &Arc<Mutex<DeviceStatus>>,
    path: &str,
) -> bool {
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
        device_state,
        DeviceStatus {
            state: DeviceState::Connected,
            name: Some(name),
            buttons: [None; 5],
        },
    );

    let mut movement_error = false;
    let mut vertical_adjustment = crate::movement::VerticalAdjustmentClock::default();
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
                if let Ok(mut current) = device_state.lock() {
                    current.buttons = buttons;
                }
            }
        }
        if let Some(target) = crate::movement::take_target(movement_state) {
            let held = crate::movement::trigger_button_index(&target.trigger_button).is_some_and(
                |button| {
                    events
                        .held(Class::Button)
                        .iter()
                        .any(|usage| usage.id == button)
                },
            );
            let recoil = vertical_adjustment.sample(
                held && target.target_valid,
                &target.recoil,
                std::time::Instant::now(),
            );
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
        device_state,
        if movement_error {
            DeviceStatus::error()
        } else {
            DeviceStatus::missing()
        },
    );
    true
}

fn ferrum_device_name(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|line| line.eq_ignore_ascii_case("kmbox: ferrum"))
        .map(|_| "Ferrum".to_string())
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

fn monitor_ferrum(
    movement_state: &crate::movement::MovementState,
    device_state: &Arc<Mutex<DeviceStatus>>,
    port_name: &str,
) -> bool {
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
        device_state,
        DeviceStatus {
            state: DeviceState::Connected,
            name: Some(name),
            buttons: [None; 5],
        },
    );
    let mut buffered = Vec::with_capacity(128);
    let mut last_mask = 0_u8;
    let mut movement_error = false;
    let mut vertical_adjustment = crate::movement::VerticalAdjustmentClock::default();
    loop {
        match connection.read(&mut chunk) {
            Ok(count) => {
                buffered.extend_from_slice(&chunk[..count]);
                if let Some(mask) = parse_ferrum_button_mask(&buffered) {
                    last_mask = mask;
                    let buttons = std::array::from_fn(|index| Some(mask & (1 << index) != 0));
                    if let Ok(mut current) = device_state.lock() {
                        current.buttons = buttons;
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
        if let Some(target) = crate::movement::take_target(movement_state) {
            let held = crate::movement::trigger_button_index(&target.trigger_button)
                .is_some_and(|button| last_mask & (1 << button) != 0);
            let recoil = vertical_adjustment.sample(
                held && target.target_valid,
                &target.recoil,
                std::time::Instant::now(),
            );
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
        device_state,
        if movement_error {
            DeviceStatus::error()
        } else {
            DeviceStatus::missing()
        },
    );
    true
}

fn prepare_runtime(
    _data_dir: &Path,
    _token: &str,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), String> {
    #[cfg(debug_assertions)]
    {
        let executable = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join(if cfg!(target_os = "windows") {
                "receiver-backend-x86_64-pc-windows-msvc.exe"
            } else {
                "receiver-backend"
            });
        if !executable.is_file() {
            return Err(format!(
                "Debug backend sidecar is missing: {}",
                executable.display()
            ));
        }
        return Ok((
            executable,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dist"),
            None,
        ));
    }
    #[cfg(not(debug_assertions))]
    {
        if EMBEDDED_BACKEND.is_empty() {
            return Err("Rust backend was not embedded in SysX.exe".into());
        }
        let runtime_dir =
            _data_dir.join(format!("runtime-{}-{}", std::process::id(), &_token[..8]));
        fs::create_dir_all(&runtime_dir).map_err(|error| error.to_string())?;
        let executable = runtime_dir.join(if cfg!(target_os = "windows") {
            "receiver-backend.exe"
        } else {
            "receiver-backend"
        });
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
        Ok((executable, config_root, Some(runtime_dir)))
    }
}

fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Ok(root) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(root).join("SysX");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("SysX-data")
}

fn make_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Opens the receiver monitor as a chromeless Edge app window with its own
/// profile under app data (window size/position persist there). Falls back
/// to the default browser.
fn open_monitor_window(port: u16, data_dir: &Path) {
    let url = format!("http://127.0.0.1:{port}/monitor.html");
    #[cfg(target_os = "windows")]
    {
        let edge = ["ProgramFiles(x86)", "ProgramFiles"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|root| PathBuf::from(root).join(r"Microsoft\Edge\Application\msedge.exe"))
            .find(|path| path.is_file());
        if let Some(edge) = edge {
            let profile = data_dir.join("monitor-window");
            if Command::new(edge)
                .arg(format!("--app={url}"))
                .arg(format!("--user-data-dir={}", profile.display()))
                .args([
                    // Compact default; Edge remembers later resizes in this profile.
                    "--window-size=550,620",
                    "--no-first-run",
                    "--no-default-browser-check",
                ])
                .spawn()
                .is_ok()
            {
                return;
            }
        }
    }
    let _ = data_dir;
    open_url(&url);
}

fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_exits_after_monitor_closes_but_survives_reloads() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(None, start + Duration::from_secs(30)));
        assert!(!lifecycle.should_exit(Some(1), start + Duration::from_secs(31)));
        // Reload: the page disconnects and reconnects inside the grace period.
        assert!(!lifecycle.should_exit(Some(0), start + Duration::from_secs(40)));
        assert!(!lifecycle.should_exit(Some(1), start + Duration::from_secs(41)));
        assert!(!lifecycle.should_exit(Some(0), start + Duration::from_secs(50)));
        assert!(!lifecycle.should_exit(None, start + Duration::from_millis(52_900)));
        assert!(lifecycle.should_exit(None, start + Duration::from_secs(53)));
    }

    #[test]
    fn host_gives_up_when_no_monitor_ever_connects() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(Some(0), start + Duration::from_secs(59)));
        assert!(lifecycle.should_exit(None, start + MONITOR_FIRST_CONNECT));
    }

    #[test]
    fn device_report_and_state_parsing() {
        let device = DeviceStatus {
            state: DeviceState::Connected,
            name: Some("Medius".into()),
            buttons: [Some(true), Some(false), None, Some(false), Some(false)],
        };
        let command = device_command(&device);
        assert_eq!(command["type"], "device_status");
        assert_eq!(command["state"], "connected");
        assert_eq!(
            command["buttons"],
            serde_json::json!([true, false, null, false, false])
        );
        assert_eq!(
            monitor_clients(r#"{"event":"state","monitor_clients":2}"#),
            Some(2)
        );
        assert_eq!(
            monitor_clients(r#"{"event":"ack","monitor_clients":2}"#),
            None
        );
        assert_eq!(monitor_clients("not json"), None);
    }
}
