//! Native SysX host. It owns everything that must stay native â€” the embedded
//! backend process, Medius device I/O and movement output â€” and shows
//! the receiver UI as the web monitor page (`/monitor.html`) in a chromeless
//! browser app window. The host exits once the monitor and configuration
//! clients have both closed.
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{connect, Message};

include!(concat!(env!("OUT_DIR"), "/embedded_runtime.rs"));
use crate::auto::AutoTargetEvent;

/// Grace period for monitor/configuration page reloads before the host shuts down.
const MONITOR_CLOSE_GRACE: Duration = Duration::from_secs(3);
/// How long to wait for the monitor window to connect at all.
const MONITOR_FIRST_CONNECT: Duration = Duration::from_secs(60);

enum HostEvent {
    ClientCounts { monitor: usize, config: usize },
    Disconnected,
}

struct BackendRuntime {
    child: Arc<Mutex<Child>>,
    runtime_dir: Option<PathBuf>,
    integrity: Option<crate::integrity::IntegrityGuard>,
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
        drop(self.integrity.take());
        // Child::kill uses the owned OS process handle on Windows, avoiding a
        // PATH lookup and the possibility of targeting a recycled PID.
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
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
    let Some(_instance) = crate::single_instance::acquire() else {
        // Another SysX owns the backend and UDP port: show its monitor, exit.
        if !crate::single_instance::focus_monitor_window() {
            let data_dir = app_data_dir();
            if let Some(config_port) = crate::single_instance::running_config_port(&data_dir) {
                open_monitor_window(config_port, &data_dir);
            }
        }
        return;
    };
    let movement_state = Arc::new(crate::movement::MovementState::default());
    let auto_target = Arc::new(Mutex::new(AutoTargetEvent::default()));
    let device_state = Arc::new(Mutex::new(DeviceStatus::missing()));
    let (runtime, port, token, data_dir, config_port) =
        match start_backend(Arc::clone(&movement_state), Arc::clone(&auto_target)) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("SysX could not start: {error}");
                return;
            }
        };
    start_device_monitor(
        Arc::clone(&movement_state),
        Arc::clone(&auto_target),
        Arc::clone(&device_state),
    );
    let (event_tx, event_rx) = mpsc::channel();
    start_host_client(port, token, Arc::clone(&device_state), event_tx);
    let Some(config_port) = config_port else {
        eprintln!("SysX local web server did not start; no receiver window can be shown");
        return;
    };
    crate::single_instance::record_config_port(&data_dir, config_port);
    // The opt-in Windows smoke owns only the monitor window. Do not open a
    // config tab in the user's browser that would keep the host alive.
    if !std::env::args().any(|arg| arg == "--smoke-test") {
        open_config_page(&format!("http://127.0.0.1:{config_port}/config.html"));
    }

    // Preferred: the receiver as a native SysX window (WebView2). Closing it
    // leaves the backend alive while the browser config page is open. Without
    // the WebView2 runtime, fall back to an Edge app window.
    if wry::webview_version().is_ok() {
        let ui_counts = Arc::new(Mutex::new((0_usize, 0_usize)));
        let backend_disconnected = Arc::new(AtomicBool::new(false));
        let counts_for_events = Arc::clone(&ui_counts);
        let disconnected_for_events = Arc::clone(&backend_disconnected);
        let (gone_tx, gone_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(event) = event_rx.recv() {
                match event {
                    HostEvent::ClientCounts { monitor, config } => {
                        if let Ok(mut counts) = counts_for_events.lock() {
                            *counts = (monitor, config);
                        }
                    }
                    HostEvent::Disconnected => {
                        disconnected_for_events.store(true, Ordering::Release);
                        break;
                    }
                }
            }
            let _ = gone_tx.send(());
        });
        let native_closed = match crate::monitor_window::run(config_port, &data_dir, gone_rx) {
            Ok(()) => true,
            Err(_gone_rx) => {
                open_monitor_window(config_port, &data_dir);
                false
            }
        };
        if !backend_disconnected.load(Ordering::Acquire) {
            // A native window that was explicitly closed must not restart the
            // 60-second first-client wait. A failed WebView creation still
            // allows the Edge fallback time to connect, without blocking on
            // backend exit (which itself depends on this host shutting down).
            let now = Instant::now();
            let mut lifecycle = if native_closed {
                MonitorLifecycle::after_monitor_closed(now)
            } else {
                MonitorLifecycle::new(now)
            };
            while !backend_disconnected.load(Ordering::Acquire) {
                let counts = ui_counts.lock().map(|value| *value).ok();
                if lifecycle.should_exit(counts, Instant::now()) {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
        crate::single_instance::clear_config_port(&data_dir);
        drop(runtime);
        return;
    }
    open_monitor_window(config_port, &data_dir);
    let mut lifecycle = MonitorLifecycle::new(Instant::now());
    loop {
        let count = match event_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(HostEvent::ClientCounts { monitor, config }) => Some((monitor, config)),
            Ok(HostEvent::Disconnected) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => None,
        };
        if lifecycle.should_exit(count, Instant::now()) {
            break;
        }
    }
    crate::single_instance::clear_config_port(&data_dir);
    // Dropping the runtime stops the backend and removes the extracted files.
    drop(runtime);
}

/// Decides when the UI clients are gone: the host keeps running while at least
/// one monitor or configuration page is connected, tolerates a short reload
/// gap, and gives up if neither client ever connects.
struct MonitorLifecycle {
    started: Instant,
    seen: bool,
    count: usize,
    config_count: usize,
    zero_since: Option<Instant>,
}

impl MonitorLifecycle {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            seen: false,
            count: 0,
            config_count: 0,
            zero_since: None,
        }
    }

    fn after_monitor_closed(now: Instant) -> Self {
        Self {
            seen: true,
            zero_since: Some(now),
            ..Self::new(now)
        }
    }

    fn should_exit(&mut self, counts: Option<(usize, usize)>, now: Instant) -> bool {
        if let Some((count, config_count)) = counts {
            self.count = count;
            self.config_count = config_count;
        }
        if self.config_count > 0 || self.count > 0 {
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
    auto_target: Arc<Mutex<AutoTargetEvent>>,
) -> Result<(BackendRuntime, u16, String, PathBuf, Option<u16>), String> {
    let token = make_token()?;
    let data_dir = app_data_dir();
    fs::create_dir_all(&data_dir).map_err(|error| format!("Cannot create app data: {error}"))?;
    let (executable, config_root, runtime_dir) = prepare_runtime(&data_dir, &token)?;
    let verified = verify_release_runtime(runtime_dir.as_deref())?;
    let mut command = Command::new(&executable);
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
    // Any subsequent startup error now runs the ordinary owned-child cleanup.
    let mut runtime = BackendRuntime {
        child: Arc::new(Mutex::new(child)),
        runtime_dir,
        integrity: None,
    };
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
                if matches!(
                    value.get("event").and_then(|value| value.as_str()),
                    Some("auto_target" | "auto_config")
                ) {
                    if let Ok(event) = serde_json::from_value::<AutoTargetEvent>(value) {
                        if let Ok(mut current) = auto_target.lock() {
                            *current = event;
                        }
                    }
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
    if let Some(verified) = verified {
        let owned_child = Arc::clone(&runtime.child);
        runtime.integrity = Some(
            verified
                .watch_backend(owned_child)
                .map_err(|error| format!("Could not start runtime integrity watchdog: {error}"))?,
        );
    }
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
                        if let Some(counts) = client_counts(&text) {
                            let _ = events.send(HostEvent::ClientCounts {
                                monitor: counts.0,
                                config: counts.1,
                            });
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

fn client_counts(text: &str) -> Option<(usize, usize)> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("event")?.as_str()? != "state" {
        return None;
    }
    let monitor = usize::try_from(value.get("monitor_clients")?.as_u64()?).ok()?;
    let config = usize::try_from(value.get("config_clients")?.as_u64()?).ok()?;
    Some((monitor, config))
}

fn set_device_status(state: &Arc<Mutex<DeviceStatus>>, status: DeviceStatus) {
    if let Ok(mut current) = state.lock() {
        *current = status;
    }
}

fn auto_trigger_held(input: &AutoTargetEvent, held: impl FnOnce(u16) -> bool) -> bool {
    crate::movement::trigger_button_index(&input.trigger_button).is_some_and(held)
}

fn start_device_monitor(
    movement_state: Arc<crate::movement::MovementState>,
    auto_target: Arc<Mutex<AutoTargetEvent>>,
    device_state: Arc<Mutex<DeviceStatus>>,
) {
    let _ = thread::Builder::new()
        .name("sysx-device-monitor".into())
        .spawn(move || loop {
            if inspect_devices(&movement_state, &auto_target, &device_state) {
                thread::sleep(Duration::from_millis(250));
            } else {
                thread::sleep(Duration::from_secs(2));
            }
        });
}

fn inspect_devices(
    movement_state: &crate::movement::MovementState,
    auto_target: &Arc<Mutex<AutoTargetEvent>>,
    device_state: &Arc<Mutex<DeviceStatus>>,
) -> bool {
    let medius_ports = medius::find_medius();
    for port in &medius_ports {
        if monitor_medius(movement_state, auto_target, device_state, &port.path) {
            return true;
        }
    }

    set_device_status(
        device_state,
        if !medius_ports.is_empty() {
            DeviceStatus::error()
        } else {
            DeviceStatus::missing()
        },
    );
    false
}

fn monitor_medius(
    movement_state: &crate::movement::MovementState,
    auto_target: &Arc<Mutex<AutoTargetEvent>>,
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

    // Moves run on the queue's writer thread through a clone of the device handle
    // (the crate shares one link), so a slow move never delays button events.
    let move_device = device.clone();
    let Ok(writer) = crate::serial_queue::ThreadedSerialWriter::spawn(move |bytes, _| {
        crate::medius_device::dispatch(&move_device, bytes)
    }) else {
        return false;
    };
    let mut serial = crate::serial_queue::SerialCommandQueue::new(writer);
    let mut move_completion: Option<std::sync::mpsc::Receiver<Result<(), String>>> = None;
    let mut movement_error = false;
    let mut vertical_adjustment = crate::movement::VerticalAdjustmentClock::default();
    let mut auto_state = crate::auto::AutoState::new();
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
        if !events.is_connected() {
            break;
        }
        serial.pump();
        if let Some(completion) = &move_completion {
            match completion.try_recv() {
                Ok(Ok(())) => move_completion = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    movement_error = true;
                    break;
                }
            }
        }
        // While a move is in flight, new targets keep summing in MovementState
        // (and expire after 100 ms) instead of queueing up behind it.
        if !serial.is_idle() {
            continue;
        }
        let auto_input = auto_target
            .lock()
            .map(|input| input.clone())
            .unwrap_or_default();
        let auto_held = auto_trigger_held(&auto_input, |button| {
            events
                .held(Class::Button)
                .iter()
                .any(|usage| usage.id == button)
        });
        if let Some(crate::auto::AutoAction::LeftClick) =
            auto_input.action(&mut auto_state, auto_held, Instant::now())
        {
            match serial.submit_medius_left_click() {
                Ok(completion) => {
                    move_completion = Some(completion);
                    serial.pump();
                    continue;
                }
                Err(_) => {
                    movement_error = true;
                    break;
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
            let motion_active = crate::movement::should_apply_target(&target, held);
            let recoil = vertical_adjustment.sample(
                motion_active,
                &target.recoil,
                std::time::Instant::now(),
            );
            if motion_active {
                let (dx, dy) = target.device_delta(recoil);
                if dx != 0 || dy != 0 {
                    match serial.submit_medius_move(dx, dy) {
                        Ok(completion) => {
                            move_completion = Some(completion);
                            serial.pump();
                        }
                        Err(_) => {
                            movement_error = true;
                            break;
                        }
                    }
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
        // A fixed path (safe: only one SysX runs) keeps the backend exe at the
        // same location every launch, so a Windows Firewall allow rule for LAN
        // UDP is granted once instead of re-prompting per run. Leftovers from
        // crashed runs and older per-process `runtime-*` folders are removed.
        if let Ok(entries) = fs::read_dir(_data_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == "runtime" || name.starts_with("runtime-") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let runtime_dir = _data_dir.join("runtime");
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

/// Hash every immutable runtime file before starting any backend/UI code.
/// Debug and test launches retain the ordinary editable development runtime.
fn verify_release_runtime(
    root: Option<&Path>,
) -> Result<Option<crate::integrity::VerifiedRuntime>, String> {
    #[cfg(all(not(debug_assertions), not(test)))]
    {
        let root = root.ok_or("Release runtime was not extracted")?;
        crate::integrity::VerifiedRuntime::open(root, EMBEDDED_RUNTIME_HASHES)
            .map(Some)
            .map_err(|error| format!("Runtime integrity verification failed: {error}"))
    }
    #[cfg(any(debug_assertions, test))]
    {
        let _ = root;
        Ok(None)
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

/// Web config opens in Google Chrome when it is installed, otherwise in the
/// default browser.
fn open_config_page(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let chrome = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|root| PathBuf::from(root).join(r"Google\Chrome\Application\chrome.exe"))
            .find(|path| path.is_file());
        if let Some(chrome) = chrome {
            if Command::new(chrome).arg(url).spawn().is_ok() {
                return;
            }
        }
    }
    open_url(url);
}

fn open_url(url: &str) {
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = url;
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
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
        assert!(!lifecycle.should_exit(Some((1, 0)), start + Duration::from_secs(31)));
        // Reload: the page disconnects and reconnects inside the grace period.
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(40)));
        assert!(!lifecycle.should_exit(Some((1, 0)), start + Duration::from_secs(41)));
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(50)));
        assert!(!lifecycle.should_exit(None, start + Duration::from_millis(52_900)));
        assert!(lifecycle.should_exit(None, start + Duration::from_secs(53)));
    }

    #[test]
    fn host_gives_up_when_no_monitor_ever_connects() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(59)));
        assert!(lifecycle.should_exit(None, start + MONITOR_FIRST_CONNECT));
    }

    #[test]
    fn config_client_keeps_backend_alive_after_monitor_closes() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(Some((1, 1)), start + Duration::from_secs(1)));
        assert!(!lifecycle.should_exit(Some((0, 1)), start + Duration::from_secs(120)));
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(124)));
        assert!(lifecycle.should_exit(None, start + Duration::from_secs(127)));
    }

    #[test]
    fn native_window_close_uses_close_grace_without_waiting_for_a_first_client() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::after_monitor_closed(start);
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(2)));
        assert!(lifecycle.should_exit(None, start + MONITOR_CLOSE_GRACE));
    }

    #[test]
    fn config_only_client_gets_a_reload_grace_after_its_last_disconnect() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::after_monitor_closed(start);
        assert!(!lifecycle.should_exit(Some((0, 1)), start + Duration::from_secs(10)));
        assert!(!lifecycle.should_exit(Some((0, 0)), start + Duration::from_secs(120)));
        assert!(!lifecycle.should_exit(None, start + Duration::from_secs(122)));
        assert!(lifecycle.should_exit(None, start + Duration::from_secs(123)));
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
            client_counts(r#"{"event":"state","monitor_clients":2,"config_clients":1}"#),
            Some((2, 1))
        );
        assert_eq!(
            client_counts(r#"{"event":"ack","monitor_clients":2,"config_clients":1}"#),
            None
        );
        assert_eq!(client_counts("not json"), None);
    }
}
