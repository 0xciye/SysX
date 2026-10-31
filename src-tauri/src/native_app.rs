//! Native SysX host: embedded backend, Medius I/O, movement and a Slint monitor.
//! Settings and Preview run in the native Slint application.
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{connect, Message};

include!(concat!(env!("OUT_DIR"), "/embedded_runtime.rs"));
use crate::auto::AutoTargetEvent;

/// Grace period for native-window reloads before the host shuts down.
const MONITOR_CLOSE_GRACE: Duration = Duration::from_secs(3);
/// How long to wait for the monitor window to connect at all.
const MONITOR_FIRST_CONNECT: Duration = Duration::from_secs(60);

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) enum HostEvent {
    ClientCount(usize),
    CommandResponse(serde_json::Value),
    Disconnected,
}

struct BackendRuntime {
    child: Arc<Mutex<Child>>,
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

/// Runs SysX until it ends; true when that was not the user closing it, so
/// the supervisor should start it again.
pub fn run() -> bool {
    let Some(_instance) = crate::single_instance::acquire() else {
        // Another SysX owns the backend and UDP port: show its monitor, exit.
        if !crate::single_instance::focus_monitor_window() {
            crate::single_instance::request_monitor();
        }
        return false;
    };
    let movement_state = Arc::new(crate::movement::MovementState::default());
    let auto_target = Arc::new(Mutex::new(AutoTargetEvent::default()));
    let device_state = Arc::new(Mutex::new(DeviceStatus::missing()));
    let capture = Arc::new(Mutex::new(crate::mouse_capture::Bridge::default()));
    let (runtime, port, token) =
        match start_backend(Arc::clone(&movement_state), Arc::clone(&auto_target)) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("SysX could not start: {error}");
                return true;
            }
        };
    if start_device_monitor(
        Arc::clone(&movement_state),
        Arc::clone(&auto_target),
        Arc::clone(&device_state),
        Arc::clone(&capture),
    )
    .is_err()
    {
        set_device_status(&device_state, DeviceStatus::error());
    }
    #[cfg(target_os = "macos")]
    terminate::hold(runtime);
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::sync_channel(8);
    start_host_client(
        port,
        token.clone(),
        Arc::clone(&movement_state),
        Arc::clone(&device_state),
        Arc::clone(&capture),
        event_tx,
        command_rx,
    );
    let monitor_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::monitor_window::run(port, token, event_rx, command_tx)
    }));
    let restart = !matches!(monitor_result, Ok(Ok(())));
    #[cfg(target_os = "macos")]
    drop(terminate::release());
    #[cfg(not(target_os = "macos"))]
    drop(runtime);
    restart
}

/// Quit (Cmd+Q, the Dock menu, logout) makes AppKit call `exit()` from inside
/// the event loop, so the code after it in `run` never runs; `atexit` still
/// does. It stops the owned backend and removes its runtime there instead.
#[cfg(target_os = "macos")]
mod terminate {
    use super::BackendRuntime;
    use std::sync::Mutex;

    static RUNTIME: Mutex<Option<BackendRuntime>> = Mutex::new(None);

    extern "C" {
        fn atexit(callback: extern "C" fn()) -> i32;
    }

    extern "C" fn on_exit() {
        let _ = std::panic::catch_unwind(|| {
            if let Some(runtime) = release() {
                drop(runtime);
            }
        });
    }

    pub(super) fn hold(runtime: BackendRuntime) {
        *RUNTIME.lock().unwrap_or_else(|error| error.into_inner()) = Some(runtime);
        // SAFETY: registers a plain function; it only runs at process exit.
        unsafe {
            atexit(on_exit);
        }
    }

    pub(super) fn release() -> Option<BackendRuntime> {
        RUNTIME
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Decides when the native monitor client is gone, tolerating its short
/// reconnect gap before exiting the owned backend.
pub(crate) struct MonitorLifecycle {
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

    pub(crate) fn after_monitor_closed(now: Instant) -> Self {
        Self {
            seen: true,
            zero_since: Some(now),
            ..Self::new(now)
        }
    }

    pub(crate) fn should_exit(&mut self, counts: Option<usize>, now: Instant) -> bool {
        if let Some(count) = counts {
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

/// Latest Magnet sample from the backend (one per processed frame while
/// Magnet is enabled).
static MAGNET_TARGET: Mutex<Option<crate::magnet::MagnetTargetEvent>> = Mutex::new(None);

fn start_backend(
    movement_state: Arc<crate::movement::MovementState>,
    auto_target: Arc<Mutex<AutoTargetEvent>>,
) -> Result<(BackendRuntime, u16, String), String> {
    let token = make_token()?;
    let data_dir = app_data_dir();
    fs::create_dir_all(&data_dir).map_err(|error| format!("Cannot create app data: {error}"))?;
    let (executable, runtime_dir) = prepare_runtime(&data_dir)?;
    let mut command = Command::new(&executable);
    command
        .args(["serve", "--host", "127.0.0.1", "--port", "0", "--data-dir"])
        .arg(&data_dir)
        .env("RECEIVER_IPC_TOKEN", &token)
        .env("SYSX_LIBRARY_DIR", library_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
    let runtime = BackendRuntime {
        child: Arc::new(Mutex::new(child)),
        runtime_dir,
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
                if value.get("event").and_then(|value| value.as_str()) == Some("magnet_target") {
                    if let Ok(event) =
                        serde_json::from_value::<crate::magnet::MagnetTargetEvent>(value)
                    {
                        *lock_recover(&MAGNET_TARGET) = Some(event);
                    }
                    continue;
                }
                if matches!(
                    value.get("event").and_then(|value| value.as_str()),
                    Some("auto_target" | "auto_config")
                ) {
                    if let Ok(event) = serde_json::from_value::<AutoTargetEvent>(value) {
                        *lock_recover(&auto_target) = event;
                    }
                    continue;
                }
                if value.get("event").and_then(|value| value.as_str()) == Some("ready") {
                    if let Some(port) = value.get("port").and_then(|value| value.as_u64()) {
                        let _ = ready_tx.send(port as u16);
                    }
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let port = ready_rx
        .recv_timeout(Duration::from_secs(12))
        .map_err(|_| "Backend did not become ready within 12 seconds".to_string())?;
    Ok((runtime, port, token))
}

/// Backend connection with `role=host`: no preview frames, pushes device
/// status changes, and reports how many native monitor windows are connected.
fn start_host_client(
    port: u16,
    token: String,
    movement_state: Arc<crate::movement::MovementState>,
    device_state: Arc<Mutex<DeviceStatus>>,
    capture: Arc<Mutex<crate::mouse_capture::Bridge>>,
    events: Sender<HostEvent>,
    commands: mpsc::Receiver<serde_json::Value>,
) {
    let failure_capture = Arc::clone(&capture);
    let failure_events = events.clone();
    let spawn = thread::Builder::new()
        .name("sysx-host-client".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let url = format!("ws://127.0.0.1:{port}/receiver?token={token}&role=host");
                let Ok((mut socket, _)) = connect(url) else {
                    return;
                };
                if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
                    // Button changes are sent between reads; the backend's trigger
                    // state (calibration samples only while it is held) should
                    // trail the device by about a movement tick, not 20 ms.
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(2)));
                }
                let mut sent: Option<DeviceStatus> = None;
                loop {
                    while let Ok(command) = commands.try_recv() {
                        if socket
                            .send(Message::Text(command.to_string().into()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    let device = lock_recover(&device_state).clone();
                    if sent.as_ref() != Some(&device) {
                        let command = device_command(&device).to_string();
                        if socket.send(Message::Text(command.into())).is_err() {
                            break;
                        }
                        sent = Some(device);
                    }
                    let capture_commands = lock_recover(&capture).commands(Instant::now());
                    let mut capture_send_failed = false;
                    for command in capture_commands {
                        if socket
                            .send(Message::Text(command.to_string().into()))
                            .is_err()
                        {
                            capture_send_failed = true;
                            break;
                        }
                    }
                    if capture_send_failed {
                        break;
                    }
                    match socket.read() {
                        Ok(Message::Text(text)) => {
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                                if value.get("event").and_then(|event| event.as_str())
                                    != Some("state")
                                {
                                    let _ = events.send(HostEvent::CommandResponse(value.clone()));
                                }
                                if let Some(settings) = recoil_settings_from_message(&value) {
                                    crate::movement::set_recoil_settings(&movement_state, settings);
                                }
                                lock_recover(&capture).observe_state(&value);
                            }
                            if let Some(counts) = client_counts(&text) {
                                let _ = events.send(HostEvent::ClientCount(counts));
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
            }));
            let _ = result;
            lock_recover(&capture).configure(None);
            let _ = events.send(HostEvent::Disconnected);
        });
    if spawn.is_err() {
        lock_recover(&failure_capture).configure(None);
        let _ = failure_events.send(HostEvent::Disconnected);
    }
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

fn client_counts(text: &str) -> Option<usize> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if value.get("event")?.as_str()? != "state" {
        return None;
    }
    usize::try_from(value.get("monitor_clients")?.as_u64()?).ok()
}

fn recoil_settings_from_message(
    message: &serde_json::Value,
) -> Option<crate::movement::RecoilSettings> {
    let state = message.get("state").unwrap_or(message);
    serde_json::from_value(state.get("recoil")?.clone()).ok()
}

fn set_device_status(state: &Arc<Mutex<DeviceStatus>>, status: DeviceStatus) {
    *state.lock().unwrap_or_else(|error| error.into_inner()) = status;
}

fn auto_trigger_held(input: &AutoTargetEvent, held: impl FnOnce(u16) -> bool) -> bool {
    crate::movement::trigger_button_index(&input.trigger_button).is_some_and(held)
}

fn start_device_monitor(
    movement_state: Arc<crate::movement::MovementState>,
    auto_target: Arc<Mutex<AutoTargetEvent>>,
    device_state: Arc<Mutex<DeviceStatus>>,
    capture: Arc<Mutex<crate::mouse_capture::Bridge>>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("sysx-device-monitor".into())
        .spawn(move || loop {
            let retry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                inspect_devices(&movement_state, &auto_target, &device_state, &capture)
            }))
            .unwrap_or_else(|_| {
                set_device_status(&device_state, DeviceStatus::error());
                true
            });
            if retry {
                thread::sleep(Duration::from_millis(250));
            } else {
                thread::sleep(Duration::from_millis(500));
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn find_medius_ports() -> Result<Vec<medius::PortInfo>, String> {
    serialport::available_ports()
        .map(|ports| {
            ports
                .into_iter()
                .filter_map(|port| match port.port_type {
                    serialport::SerialPortType::UsbPort(usb)
                        if usb.vid == 0x1A86
                            && usb.pid == 0x55D3
                            && is_control_port(&port.port_name) =>
                    {
                        Some(medius::PortInfo {
                            path: port.port_name,
                            vid: usb.vid,
                            pid: usb.pid,
                            serial: usb.serial_number,
                        })
                    }
                    _ => None,
                })
                .collect()
        })
        .map_err(|error| format!("serial-port scan failed: {error}"))
}

/// macOS lists each USB serial device twice, as `/dev/cu.*` (call-out) and
/// `/dev/tty.*` (dial-in, which waits for carrier). Open only the call-out
/// node so one box is not tried twice.
fn is_control_port(path: &str) -> bool {
    !cfg!(target_os = "macos") || !path.starts_with("/dev/tty.")
}

fn inspect_devices(
    movement_state: &crate::movement::MovementState,
    auto_target: &Arc<Mutex<AutoTargetEvent>>,
    device_state: &Arc<Mutex<DeviceStatus>>,
    capture: &Arc<Mutex<crate::mouse_capture::Bridge>>,
) -> bool {
    // Scan errors must remain distinguishable from an empty port list.
    let ports = match find_medius_ports() {
        Ok(ports) => ports,
        Err(_) => {
            set_device_status(device_state, DeviceStatus::error());
            return true;
        }
    };
    if ports.is_empty() {
        set_device_status(device_state, DeviceStatus::missing());
        return false;
    }

    let mut failures = Vec::new();
    for port in ports {
        match medius::Device::open(&port.path) {
            Ok(device) => {
                match monitor_medius(movement_state, auto_target, device_state, capture, device) {
                    Ok(retry_scan) => return retry_scan,
                    Err(error) => failures.push(format!("{}: {error}", port.path)),
                }
            }
            Err(error) => failures.push(format!("{}: open failed: {error}", port.path)),
        }
    }
    set_device_status(device_state, DeviceStatus::error());
    true
}

fn injection_ready(health: medius::Health) -> bool {
    health.link_up && health.mouse_attached && health.clone_configured
}

/// A box may report before its link and physical mouse are ready. Wait briefly
/// for the conditions SysX needs before subscribing to its input.
fn wait_for_injection_ready(device: &medius::Device) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let result = device.query_health();
        if result.as_ref().is_ok_and(|health| injection_ready(*health)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(match result {
                Ok(health) => format!(
                    "mouse not ready after 3s (link_up={}, mouse_attached={}, clone_configured={})",
                    health.link_up, health.mouse_attached, health.clone_configured
                ),
                Err(error) => format!("health query failed for 3s ({error})"),
            });
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn monitor_medius(
    movement_state: &crate::movement::MovementState,
    auto_target: &Arc<Mutex<AutoTargetEvent>>,
    device_state: &Arc<Mutex<DeviceStatus>>,
    capture: &Arc<Mutex<crate::mouse_capture::Bridge>>,
    device: medius::Device,
) -> Result<bool, String> {
    use medius::{Button, CatchFilter, Class, Input};

    let version = device
        .query_version()
        .map_err(|error| format!("version query failed: {error}"))?;
    wait_for_injection_ready(&device)?;
    let filters = [
        Button::LEFT,
        Button::RIGHT,
        Button::MIDDLE,
        Button::SIDE1,
        Button::SIDE2,
    ]
    .into_iter()
    .map(CatchFilter::watch);
    let events = device
        .input_events(filters)
        .map_err(|error| format!("mouse input subscription failed: {error}"))?;
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
    // Capture owns a different SDK queue and never reads/writes from this movement loop.
    let _capture_worker =
        crate::mouse_capture::Worker::start(device.clone(), Arc::clone(capture)).ok();
    let move_device = device.clone();
    let writer = crate::serial_queue::ThreadedSerialWriter::spawn(move |bytes, _| {
        crate::medius_device::dispatch(&move_device, bytes)
    })
    .map_err(|error| format!("mouse output worker failed: {error}"))?;
    let mut serial = crate::serial_queue::SerialCommandQueue::new(writer);
    let mut move_completion: Option<std::sync::mpsc::Receiver<Result<(), String>>> = None;
    let mut movement_error = None;
    let mut vertical_adjustment = crate::movement::VerticalAdjustmentClock::default();
    // Recoil pulls come once per bullet; they wait here while a command is in
    // flight instead of being lost.
    let mut pending_recoil: i64 = 0;
    let mut auto_state = crate::auto::AutoState::new();
    let mut magnet = crate::magnet::MagnetState::default();
    let mut next_health_check = Instant::now() + Duration::from_millis(250);
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
                lock_recover(&device_state).buttons = buttons;
            }
        }
        if !events.is_connected() {
            break;
        }
        let held = |button: u16| {
            events
                .held(Class::Button)
                .iter()
                .any(|usage| usage.id == button)
        };
        let magnet_event = lock_recover(&MAGNET_TARGET).clone();
        let magnet_held = magnet_event.as_ref().is_some_and(|event| {
            crate::movement::trigger_button_index(&event.settings.trigger_button)
                .is_some_and(|button| held(button))
        });
        magnet.update(
            magnet_event.as_ref(),
            magnet_held,
            Instant::now(),
            crate::magnet::unix_ms(),
        );
        // A burst runs while the weapon fires: the physical left button or a
        // Magnet press.
        let recoil_settings = crate::movement::recoil_settings(movement_state);
        let firing = held(u16::from(Button::LEFT.as_id())) || magnet.holding();
        pending_recoil = pending_recoil.saturating_add(vertical_adjustment.sample(
            firing,
            &recoil_settings,
            Instant::now(),
        ));
        if !firing {
            pending_recoil = 0;
        }
        if Instant::now() >= next_health_check {
            next_health_check = Instant::now() + Duration::from_millis(250);
            let health = device.query_health();
            if !health.as_ref().is_ok_and(|health| injection_ready(*health)) {
                movement_error = Some(match health {
                    Ok(health) => format!(
                        "mouse health lost (link_up={}, mouse_attached={}, clone_configured={})",
                        health.link_up, health.mouse_attached, health.clone_configured
                    ),
                    Err(error) => format!("mouse health query failed: {error}"),
                });
                break;
            }
        }
        serial.pump();
        if let Some(completion) = &move_completion {
            match completion.try_recv() {
                Ok(Ok(())) => move_completion = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Ok(Err(error)) => {
                    movement_error = Some(format!("mouse output failed: {error}"));
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    movement_error = Some("mouse output worker disconnected".into());
                    break;
                }
            }
        }
        // While a move is in flight, new targets keep summing in MovementState
        // (and expire after 100 ms) instead of queueing up behind it.
        if !serial.is_idle() {
            continue;
        }
        if let Some(op) = magnet.next_op() {
            let submitted = match op {
                crate::magnet::MagnetOp::Move(dx, dy) => serial.submit_medius_move(dx, dy),
                crate::magnet::MagnetOp::Press => serial.submit_medius_left_press(true),
                crate::magnet::MagnetOp::Release => serial.submit_medius_left_press(false),
            };
            match submitted {
                Ok(completion) => {
                    move_completion = Some(completion);
                    serial.pump();
                    continue;
                }
                Err(error) => {
                    movement_error = Some(format!("Magnet output failed: {error}"));
                    break;
                }
            }
        }
        let auto_input = lock_recover(auto_target).clone();
        let auto_held = auto_trigger_held(&auto_input, held);
        let auto_action = auto_input.action(&mut auto_state, auto_held, Instant::now());
        if let Some(crate::auto::AutoAction::LeftClick) = auto_action.filter(|_| !magnet.holding())
        {
            match serial.submit_medius_left_click() {
                Ok(completion) => {
                    move_completion = Some(completion);
                    serial.pump();
                    continue;
                }
                Err(error) => {
                    movement_error = Some(format!("left click queue failed: {error}"));
                    break;
                }
            }
        }
        let recoil = pending_recoil.clamp(-32, 32);
        pending_recoil -= recoil;
        let target = crate::movement::take_target(movement_state);
        let mut remainder = None;
        let (dx, dy) = if let Some(target) = target {
            let held =
                crate::movement::trigger_button_index(&target.trigger_button).is_some_and(held);
            // Control events advance the shared routing revision before new
            // frames arrive. Never apply a batch queued by the previous mode.
            let motion_active = target.config_revision == auto_input.config_revision()
                && !auto_input.blocks_motion(auto_held)
                && crate::movement::should_apply_target(&target, held);
            if motion_active {
                let (movement, rest) = target.device_move(recoil);
                remainder = Some(rest);
                movement
            } else {
                crate::movement::recoil_move(recoil)
            }
        } else {
            crate::movement::recoil_move(recoil)
        };
        if dx != 0 || dy != 0 {
            match serial.submit_medius_move(dx, dy) {
                Ok(completion) => {
                    move_completion = Some(completion);
                    if let Some(remainder) = remainder {
                        crate::movement::restore_remainder(movement_state, remainder);
                    }
                    serial.pump();
                }
                Err(error) => {
                    movement_error = Some(format!("mouse movement queue failed: {error}"));
                    break;
                }
            }
        }
    }
    // Never leave the left button held by Magnet (best effort: the device
    // may already be gone).
    if magnet.holding() {
        let _ = crate::medius_device::dispatch(
            &device,
            &crate::serial_queue::encode_medius_left_press(false),
        );
    }
    if movement_error.is_some() {
        set_device_status(device_state, DeviceStatus::error());
    } else {
        set_device_status(device_state, DeviceStatus::missing());
    }
    Ok(true)
}

fn prepare_runtime(_data_dir: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
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
        return Ok((executable, None));
    }
    #[cfg(not(debug_assertions))]
    {
        if EMBEDDED_BACKEND.is_empty() {
            return Err("Rust backend was not embedded in the SysX application".into());
        }
        // A fixed path (safe: only one SysX runs) keeps the backend exe at the
        // same location every launch, so a Windows Firewall allow rule for LAN
        // UDP is granted once instead of re-prompting per run. Older
        // per-process `runtime-*` folders are removed.
        if let Ok(entries) = fs::read_dir(_data_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("runtime-") {
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
        // The backend (with its models) is large: it stays between launches
        // and is written again only when this build carries another one
        // (startup was slow with a write per launch, user report 2026-10-31).
        let stamp = runtime_dir.join("receiver-backend.stamp");
        let expected = format!("{EMBEDDED_BACKEND_HASH:x} {}", EMBEDDED_BACKEND.len());
        let current = fs::read_to_string(&stamp).ok().as_deref() == Some(expected.as_str())
            && fs::metadata(&executable)
                .is_ok_and(|meta| meta.len() == EMBEDDED_BACKEND.len() as u64);
        if !current {
            let _ = fs::remove_file(&stamp);
            fs::write(&executable, EMBEDDED_BACKEND).map_err(|error| error.to_string())?;
            fs::write(&stamp, expected).map_err(|error| error.to_string())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;
        }
        // No folder to clean on exit: the next launch reuses it.
        Ok((executable, None))
    }
}

/// Where onnxruntime.dll, DirectML.dll and the other runtime libraries
/// ship: next to SysX.exe (in development, next to the backend sidecar).
fn library_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries")
    }
    #[cfg(not(debug_assertions))]
    {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Ok(root) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(root).join("SysX");
    }
    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("SysX");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_opens_only_the_call_out_node_of_a_serial_device() {
        assert!(is_control_port("/dev/cu.usbmodem1101"));
        assert!(is_control_port("COM3"));
        assert_eq!(
            is_control_port("/dev/tty.usbmodem1101"),
            !cfg!(target_os = "macos")
        );
    }

    #[test]
    fn medius_health_requires_link_mouse_and_clone_configuration() {
        assert!(!injection_ready(medius::Health::from_flags(0)));
        assert!(!injection_ready(medius::Health::from_flags(0x01)));
        assert!(!injection_ready(medius::Health::from_flags(0x03)));
        assert!(injection_ready(medius::Health::from_flags(0x07)));
    }

    #[test]
    fn device_status_recovers_after_a_poisoned_monitor_lock() {
        let status = Arc::new(Mutex::new(DeviceStatus::missing()));
        let poisoned = Arc::clone(&status);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.lock().unwrap();
            panic!("simulated device-monitor panic");
        }));

        set_device_status(&status, DeviceStatus::error());
        assert_eq!(
            status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .state,
            DeviceState::Error
        );
    }

    #[test]
    fn linked_mouse_without_clone_configuration_is_rejected() {
        assert!(!injection_ready(medius::Health::from_flags(0x03)));
        assert!(injection_ready(medius::Health::from_flags(0x07)));
    }

    #[test]
    fn host_exits_after_monitor_closes_but_survives_reloads() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(None, start + Duration::from_secs(30)));
        assert!(!lifecycle.should_exit(Some(1), start + Duration::from_secs(31)));
        // Native monitor reload: it reconnects inside the grace period.
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
    fn settings_window_keeps_backend_alive_after_monitor_closes() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::new(start);
        assert!(!lifecycle.should_exit(Some(2), start + Duration::from_secs(1)));
        assert!(!lifecycle.should_exit(Some(1), start + Duration::from_secs(120)));
        assert!(!lifecycle.should_exit(Some(0), start + Duration::from_secs(124)));
        assert!(lifecycle.should_exit(None, start + Duration::from_secs(127)));
    }

    #[test]
    fn native_window_close_uses_close_grace_without_waiting_for_a_first_client() {
        let start = Instant::now();
        let mut lifecycle = MonitorLifecycle::after_monitor_closed(start);
        assert!(!lifecycle.should_exit(Some(0), start + Duration::from_secs(2)));
        assert!(lifecycle.should_exit(None, start + MONITOR_CLOSE_GRACE));
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
            client_counts(r#"{"event":"state","monitor_clients":2}"#),
            Some(2)
        );
        assert_eq!(
            client_counts(r#"{"event":"ack","monitor_clients":2}"#),
            None
        );
        assert_eq!(client_counts("not json"), None);
        let recoil = recoil_settings_from_message(&serde_json::json!({
            "event": "state",
            "recoil": {
                "recoilEnabled": true,
                "recoilLevels": [2, 5],
                "recoilStartDelayMs": 75,
                "recoilLevelDurationMs": 35,
                "recoilPercentage": 60
            }
        }))
        .unwrap();
        assert!(recoil.recoil_enabled);
        assert_eq!(recoil.recoil_levels, vec![2, 5]);
        assert_eq!(recoil.recoil_start_delay_ms, 75);
        assert_eq!(recoil.recoil_level_duration_ms, 35);
        assert_eq!(recoil.recoil_percentage, 60);
    }
}
