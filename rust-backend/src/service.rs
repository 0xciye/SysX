use crate::config::{
    load_settings, save_settings, AppSettings, ModelDefinition, ModelRegistry, RecoilSettings,
};
use crate::movement::{
    CalibrationStore, MovementBatcher, MovementRuntimeSettings, MovementTargetSample,
    MovementTicker,
};
use crate::pipeline::{LatestFramePipeline, PipelineConfig, ProcessedFrame};
use crate::pixel_seek::PixelTargetColor;
use crate::receiver::UdpDecodedFrameSource;
use crate::results::DetectionMode;
use crate::storage::DetectionDataStore;
use crate::udp::ReassemblyStats;
use crate::web_config;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use image::codecs::jpeg::JpegEncoder;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle as TokioJoinHandle;

const UDP_RETRY: Duration = Duration::from_secs(3);
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
struct SourceState {
    active: Option<UdpDecodedFrameSource>,
    active_port: u16,
    pending_port: Option<u16>,
    retry_at: Instant,
    last_error: Option<String>,
}

/// What `state()` reports about the UDP source, readable without the source lock.
#[derive(Clone, Default)]
struct SourceView {
    port: u16,
    transport: ReassemblyStats,
    error: Option<String>,
}

impl SourceView {
    fn of(state: &SourceState) -> Self {
        Self {
            port: state.active_port,
            transport: state
                .active
                .as_ref()
                .map(UdpDecodedFrameSource::stats)
                .unwrap_or_default(),
            error: state.last_error.clone(),
        }
    }
}

pub struct ReceiverService {
    settings: Arc<Mutex<AppSettings>>,
    registry: Arc<Mutex<ModelRegistry>>,
    pipeline: Arc<LatestFramePipeline>,
    store: Arc<DetectionDataStore>,
    movement_batcher: Arc<MovementBatcher>,
    movement_ticker: Arc<MovementTicker>,
    source: Arc<Mutex<SourceState>>,
    source_times: Arc<Mutex<VecDeque<u64>>>,
    errors: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    session_id: String,
    /// Latest Medius/Ferrum status reported by the native host
    /// (`device_status` command); forwarded to the monitor page in `state`.
    device: Arc<Mutex<Value>>,
    /// Open `role=monitor` WebSocket clients. The native host exits when the
    /// monitor window has been closed.
    monitor_clients: AtomicUsize,
    /// Published-at timestamp of the newest processed frame; changes once per
    /// frame so each preview socket forwards every input frame.
    preview_rx: tokio::sync::watch::Receiver<u64>,
    source_view: Mutex<SourceView>,
}

impl ReceiverService {
    fn new(token: String, data_dir: PathBuf, requested_udp_port: u16) -> Result<Arc<Self>, String> {
        if token.len() < 32 {
            return Err("IPC token must contain at least 32 characters".into());
        }
        std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        crate::pixel_seek::warm_profile_luts();
        let settings_path = data_dir.join("app-settings.json");
        let had_settings = settings_path.is_file();
        let mut settings_value = load_settings(&settings_path);
        if !had_settings {
            settings_value.port = requested_udp_port.to_string();
            save_settings(&settings_path, &settings_value)
                .map_err(|error| format!("could not create the settings file: {error}"))?;
        }
        let settings = Arc::new(Mutex::new(settings_value));
        let registry = Arc::new(Mutex::new(
            ModelRegistry::load(data_dir.join("models.yaml")).map_err(|error| error.to_string())?,
        ));
        {
            let mut app = settings.lock().map_err(|_| "settings lock poisoned")?;
            let models = registry
                .lock()
                .map_err(|_| "model registry lock poisoned")?;
            if app.strategy == "object_detection"
                && models
                    .active_model_id
                    .as_ref()
                    .and_then(|id| models.models.get(id))
                    .is_none()
            {
                app.strategy = "pixel_seek".into();
            }
        }
        let session_id = format!("{}", unix_ms());
        let store = Arc::new(
            DetectionDataStore::new(data_dir.join("sessions").join(&session_id), 4096)
                .map_err(|error| error.to_string())?,
        );
        let app = settings
            .lock()
            .map_err(|_| "settings lock poisoned")?
            .clone();
        let model = {
            let registry = registry
                .lock()
                .map_err(|_| "model registry lock poisoned")?;
            registry
                .active_model_id
                .as_ref()
                .and_then(|id| registry.models.get(id).cloned())
        };
        let mode = if app.strategy == "object_detection" {
            DetectionMode::ObjectDetection
        } else {
            DetectionMode::PixelSeek
        };
        let pixel_color =
            PixelTargetColor::parse(&app.pixel_color).unwrap_or(PixelTargetColor::Yellow);
        let config = PipelineConfig {
            strategy: mode,
            pixel_color,
            crop_resolution: Some(app.crop_resolution),
            fov_degrees: app.fov_degrees,
            smoothing_alpha_x: app.smoothing_alpha_x,
            smoothing_alpha_y: app.smoothing_alpha_y,
            model,
            model_backend: app.model_backend.clone(),
            compute_target: app.compute_target.clone(),
        };
        let movement_batcher = Arc::new(MovementBatcher::new(Arc::new(|event| {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
        })));
        let device = Arc::new(Mutex::new(missing_device()));
        let ticker_settings = Arc::clone(&settings);
        let ticker_device = Arc::clone(&device);
        let ticker_batcher = Arc::clone(&movement_batcher);
        let movement_ticker = Arc::new(MovementTicker::new(
            app.sens,
            f64::from(app.dpi),
            Arc::new(CalibrationStore::load(
                data_dir.join("motion-calibration.json"),
            )),
            Arc::new(move || {
                let settings = ticker_settings.lock().ok()?.clone();
                let trigger_held = ticker_device
                    .lock()
                    .ok()
                    .is_some_and(|device| trigger_held(&device, &settings.trigger_button));
                Some(MovementRuntimeSettings {
                    trigger_held,
                    sens: settings.sens,
                    dpi: f64::from(settings.dpi),
                    offset_x: settings.offset_x,
                    offset_y: settings.offset_y,
                    trigger_button: settings.trigger_button,
                    recoil: settings.recoil,
                })
            }),
            Arc::new(move |event| ticker_batcher.submit(event)),
        ));
        let callback_ticker = Arc::clone(&movement_ticker);
        let (preview_tx, preview_rx) = tokio::sync::watch::channel(0_u64);
        let callback: Arc<dyn Fn(&ProcessedFrame) + Send + Sync> = Arc::new(move |processed| {
            // Wake every preview socket for this frame (no timer polling:
            // Windows timers tick at ~15.6 ms, which capped Preview near 60 FPS).
            preview_tx.send_replace(processed.published_ns);
            let target = processed
                .result
                .smoothed_target_xy
                .or(processed.result.target_xy);
            let point = |point: [f32; 2]| (f64::from(point[0]), f64::from(point[1]));
            callback_ticker.update(MovementTargetSample {
                frame_id: processed.frame.frame_id,
                target: target.map(point),
                raw_target: processed.result.target_xy.map(point),
                crop_origin: (processed.crop_x, processed.crop_y),
                processed_size: (
                    processed.result.processed_width,
                    processed.result.processed_height,
                ),
                received_s: 0.0,
            });
        });
        let pipeline = Arc::new(LatestFramePipeline::new(
            config,
            Some(Arc::clone(&store)),
            Some(callback),
        ));
        let mut initial_source = UdpDecodedFrameSource::new(
            "0.0.0.0",
            app.port.parse::<u16>().unwrap_or(requested_udp_port),
        )
        .map_err(|error| error.to_string())?;
        initial_source.set_crop_resolution(app.crop_resolution);
        let mut source_state = SourceState {
            active: None,
            active_port: app.port.parse().unwrap_or(requested_udp_port),
            pending_port: None,
            retry_at: Instant::now(),
            last_error: None,
        };
        match initial_source.open() {
            Ok(()) => source_state.active = Some(initial_source),
            Err(error) => {
                source_state.last_error = Some(error.to_string());
                source_state.pending_port = Some(source_state.active_port);
                source_state.retry_at = Instant::now() + UDP_RETRY;
            }
        }
        let service = Arc::new(Self {
            settings,
            registry,
            pipeline,
            store,
            movement_batcher,
            movement_ticker,
            source_view: Mutex::new(SourceView::of(&source_state)),
            source: Arc::new(Mutex::new(source_state)),
            source_times: Arc::new(Mutex::new(VecDeque::with_capacity(720))),
            errors: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            reader: Mutex::new(None),
            session_id,
            device,
            monitor_clients: AtomicUsize::new(0),
            preview_rx,
        });
        Self::start_reader(&service);
        Ok(service)
    }

    fn start_reader(service: &Arc<Self>) {
        let weak = Arc::downgrade(service);
        let thread = thread::Builder::new()
            .name("sysx-udp-receiver".into())
            .spawn(move || receiver_loop(weak))
            .expect("UDP receiver thread");
        if let Ok(mut slot) = service.reader.lock() {
            *slot = Some(thread);
        }
    }

    pub fn state(&self) -> Value {
        let settings = self
            .settings
            .lock()
            .ok()
            .map(|value| value.clone())
            .unwrap_or_default();
        let models = self.registry.lock().ok();
        let latest = self.pipeline.latest();
        let stats = self.pipeline.stats();
        let source_name;
        let source_port;
        let transport;
        let source_error;
        // Never wait on the source lock: the UDP thread holds it while blocked
        // in a read, which used to stall every state()/preview send by up to a
        // frame interval. Fall back to the snapshot that thread keeps current.
        let view = match self.source.try_lock() {
            Ok(source) => SourceView::of(&source),
            Err(_) => self
                .source_view
                .lock()
                .map(|view| view.clone())
                .unwrap_or_default(),
        };
        source_port = view.port;
        source_name = format!("UDP · 0.0.0.0:{}", view.port);
        transport = view.transport;
        source_error = view.error;
        let is_live = latest
            .as_ref()
            .is_some_and(|frame| preview_is_fresh(frame.published_ns, monotonic_ns()));
        let source_fps = self.source_fps();
        let active_model_id = models
            .as_ref()
            .and_then(|registry| registry.active_model_id.clone());
        // The OBS plugin crops before transmission.  This is the resolution
        // the receiver advertises to the sender and expects in each packet.
        let expected_source_resolution =
            Some(vec![settings.crop_resolution, settings.crop_resolution]);
        let last_error = stats
            .last_error
            .clone()
            .or(source_error)
            .or_else(|| self.errors.lock().ok()?.last().cloned());
        json!({
            "event":"state", "version":1, "status":if is_live { "streaming" } else { "waiting" }, "input":"udp", "source":source_name,
            "mode":settings.strategy, "fov_degrees":settings.fov_degrees, "target_offset_x":settings.offset_x, "target_offset_y":settings.offset_y,
            "trigger_button":settings.trigger_button, "smoothing_alpha_x":settings.smoothing_alpha_x, "smoothing_alpha_y":settings.smoothing_alpha_y,
            "sens":settings.sens, "dpi":settings.dpi, "preview_scale":settings.scale, "crop_resolution":settings.crop_resolution,
            "recoil":settings.recoil,
            "pixel_color":settings.pixel_color, "pixel_profile_configured":true,
            "active_model_id":active_model_id,
            "expected_source_resolution":expected_source_resolution,
            "session_id":self.session_id, "processed_frames":stats.processed_frames, "dropped_input_frames":stats.dropped_input_frames,
            "rejected_frames":stats.rejected_frames, "resolution_mismatches":stats.resolution_mismatches,
            "transport_dropped_frames":transport.dropped_frames + transport.expired_frames, "invalid_packets":transport.invalid_packets,
            "processing_fps":stats.processing_fps, "source_fps":source_fps, "port":source_port, "last_error":last_error,
            "device":self.device.lock().map(|device| device.clone()).unwrap_or_else(|_| missing_device()),
            "monitor_clients":self.monitor_clients.load(Ordering::Relaxed)
        })
    }

    fn source_fps(&self) -> Option<f64> {
        let now = monotonic_ns();
        let times = self.source_times.lock().ok()?;
        let recent: Vec<_> = times
            .iter()
            .copied()
            .filter(|stamp| now.saturating_sub(*stamp) <= 1_000_000_000)
            .collect();
        if recent.len() < 2 || recent.last()? <= recent.first()? {
            return None;
        }
        Some((recent.len() - 1) as f64 * 1e9 / (recent.last()? - recent.first()?) as f64)
    }

    fn record_error(&self, message: String) {
        if let Ok(mut errors) = self.errors.lock() {
            errors.push(message);
            if errors.len() > 20 {
                let excess = errors.len() - 20;
                errors.drain(..excess);
            }
        }
    }

    fn set_udp_port(&self, port: u16) -> Result<(), String> {
        if port == 0 {
            return Err("UDP port must be between 1 and 65535".into());
        }
        if self
            .source
            .lock()
            .ok()
            .is_some_and(|source| source.active_port == port && source.active.is_some())
        {
            if let Ok(mut source) = self.source.lock() {
                source.pending_port = None;
                source.last_error = None;
            }
            if let Ok(mut settings) = self.settings.lock() {
                settings.port = port.to_string();
            }
            return Ok(());
        }
        let mut candidate =
            UdpDecodedFrameSource::new("0.0.0.0", port).map_err(|error| error.to_string())?;
        let crop_resolution = self
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned")?
            .crop_resolution;
        candidate.set_crop_resolution(crop_resolution);
        if let Err(error) = candidate.open() {
            let message = error.to_string();
            if let Ok(mut source) = self.source.lock() {
                source.pending_port = Some(port);
                source.retry_at = Instant::now() + UDP_RETRY;
                source.last_error = Some(message.clone());
            }
            self.record_error(message.clone());
            return Err(message);
        }
        let mut source = self.source.lock().map_err(|_| "source lock poisoned")?;
        source.active = Some(candidate);
        source.active_port = port;
        source.pending_port = None;
        source.last_error = None;
        // state() may read the snapshot instead of the source; refresh it now
        // so the reply to this command already reports the new port.
        if let Ok(mut view) = self.source_view.lock() {
            *view = SourceView::of(&source);
        }
        if let Ok(mut settings) = self.settings.lock() {
            settings.port = port.to_string();
        }
        Ok(())
    }

    fn pipeline_config(&self, settings: &AppSettings) -> Result<PipelineConfig, String> {
        let strategy = match settings.strategy.as_str() {
            "pixel_seek" => DetectionMode::PixelSeek,
            "object_detection" => DetectionMode::ObjectDetection,
            _ => return Err("invalid detection strategy".into()),
        };
        let pixel_color =
            PixelTargetColor::parse(&settings.pixel_color).ok_or("invalid Pixel Seek color")?;
        let model = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| "model registry lock poisoned")?;
            registry
                .active_model_id
                .as_ref()
                .and_then(|id| registry.models.get(id).cloned())
        };
        Ok(PipelineConfig {
            strategy,
            pixel_color,
            crop_resolution: Some(settings.crop_resolution),
            fov_degrees: settings.fov_degrees,
            smoothing_alpha_x: settings.smoothing_alpha_x,
            smoothing_alpha_y: settings.smoothing_alpha_y,
            model,
            model_backend: settings.model_backend.clone(),
            compute_target: settings.compute_target.clone(),
        })
    }

    fn reconfigure(&self) -> Result<(), String> {
        let settings = self
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned")?
            .clone();
        self.pipeline
            .configure(self.pipeline_config(&settings)?)
            .map_err(str::to_owned)
    }

    fn dispatch(&self, command: Value) -> Result<Value, String> {
        if command.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("command must be a version 1 object".into());
        }
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .ok_or("command type is required")?;
        match kind {
            "state" => Ok(self.state()),
            "device_status" => {
                let device = parse_device_status(&command)?;
                *self.device.lock().map_err(|_| "device lock poisoned")? = device;
                Ok(json!({"event":"ack","type":kind}))
            }
            "apply_settings" => {
                let commands = command
                    .get("commands")
                    .and_then(Value::as_array)
                    .filter(|commands| !commands.is_empty() && commands.len() <= 16)
                    .ok_or("settings command batch must contain 1..16 commands")?;
                for item in commands {
                    if item.get("type").and_then(Value::as_str) == Some("apply_settings") {
                        return Err("nested settings command batches are not supported".into());
                    }
                    self.dispatch(item.clone())?;
                }
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_udp_port" => {
                let port = command
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|v| u16::try_from(v).ok())
                    .ok_or("UDP port must be between 1 and 65535")?;
                self.set_udp_port(port)?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_pixel_color" => {
                let color = command
                    .get("color")
                    .and_then(Value::as_str)
                    .ok_or("pixel color is required")?;
                if PixelTargetColor::parse(color).is_none() {
                    return Err("pixel color must be yellow, red, or purple".into());
                }
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .pixel_color = color.into();
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_display_config" => {
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                if let Some(value) = command.get("fov_degrees").and_then(Value::as_u64) {
                    settings.fov_degrees =
                        u16::try_from(value).map_err(|_| "FOV must be 1..360")?;
                }
                if !(1..=360).contains(&settings.fov_degrees) {
                    return Err("FOV must be 1..360".into());
                }
                if let Some(scale) = command.get("preview_scale").and_then(Value::as_str) {
                    if !["Smooth", "Pixel perfect"].contains(&scale) {
                        return Err("preview scale must be Smooth or Pixel perfect".into());
                    }
                    settings.scale = scale.into();
                }
                drop(settings);
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_target_offset" => {
                let x = finite_number(command.get("x"), "target X offset must be finite")?;
                let y = finite_number(command.get("y"), "target Y offset must be finite")?;
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.offset_x = x;
                settings.offset_y = y;
                drop(settings);
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_trigger_button" => {
                let value = command
                    .get("button")
                    .and_then(Value::as_str)
                    .ok_or("trigger button is required")?;
                if !["right", "middle", "m4", "m5"].contains(&value) {
                    return Err("trigger button must be right, middle, m4, or m5".into());
                }
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .trigger_button = value.into();
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_recoil" => {
                let recoil: RecoilSettings = serde_json::from_value(
                    command
                        .get("settings")
                        .cloned()
                        .ok_or("recoil settings are required")?,
                )
                .map_err(|_| "invalid recoil settings")?;
                recoil.validate()?;
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .recoil = recoil;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_movement_calibration" => {
                let sens = finite_number(command.get("sens"), "sens must be 0.01..10")?;
                let dpi = command
                    .get("dpi")
                    .and_then(Value::as_u64)
                    .ok_or("DPI must be an integer from 100 to 100000")?;
                if !(0.01..=10.0).contains(&sens) || !(100..=100_000).contains(&dpi) {
                    return Err("sens must be 0.01..10 and DPI 100..100000".into());
                }
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.sens = sens;
                settings.dpi = dpi as u32;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_smoothing_config" => {
                let x = finite_number(command.get("alpha_x"), "smoothing alpha must be 0.01..1")?
                    as f32;
                let y = finite_number(command.get("alpha_y"), "smoothing alpha must be 0.01..1")?
                    as f32;
                if !(0.01..=1.0).contains(&x) || !(0.01..=1.0).contains(&y) {
                    return Err("smoothing alpha must be 0.01..1".into());
                }
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.smoothing_alpha_x = x;
                settings.smoothing_alpha_y = y;
                drop(settings);
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_crop_resolution" => {
                let resolution = command
                    .get("resolution")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| (1..=8192).contains(v))
                    .ok_or("crop resolution must be 1..8192")?;
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .crop_resolution = resolution;
                if let Ok(mut source) = self.source.lock() {
                    if let Some(active) = source.active.as_mut() {
                        active.set_crop_resolution(resolution);
                    }
                }
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_strategy" => {
                let mode = command
                    .get("mode")
                    .and_then(Value::as_str)
                    .ok_or("detection strategy is required")?;
                if !["pixel_seek", "object_detection"].contains(&mode) {
                    return Err("invalid detection strategy".into());
                }
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .strategy = mode.into();
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_inference_config" => {
                let backend = command
                    .get("backend")
                    .and_then(Value::as_str)
                    .ok_or("inference backend is required")?;
                let compute = command
                    .get("compute_target")
                    .and_then(Value::as_str)
                    .ok_or("compute target is required")?;
                let mut next = self
                    .settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .clone();
                next.model_backend = backend.into();
                next.compute_target = compute.into();
                next.validate().map_err(|error| error.to_string())?;
                let config = self.pipeline_config(&next)?;
                self.pipeline.configure(config).map_err(str::to_owned)?;
                *self.settings.lock().map_err(|_| "settings lock poisoned")? = next;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "list_models" => Ok(self.models_event()),
            "add_model" => {
                let model: ModelDefinition =
                    serde_json::from_value(command.clone()).map_err(|error| error.to_string())?;
                self.registry
                    .lock()
                    .map_err(|_| "registry lock poisoned")?
                    .add(model)
                    .map_err(|error| error.to_string())?;
                Ok(self.models_event())
            }
            "update_model" => self.update_model(&command),
            "activate_model" => {
                let id = command
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("model ID is required")?;
                let mut registry = self.registry.lock().map_err(|_| "registry lock poisoned")?;
                let model = registry.models.get(id).ok_or("selected model is missing")?;
                if !model.enabled {
                    return Err("selected model is disabled".into());
                }
                registry
                    .activate(Some(id))
                    .map_err(|error| error.to_string())?;
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .active_model_id = Some(id.into());
                drop(registry);
                self.reconfigure()?;
                Ok(self.models_event())
            }
            "remove_model" => {
                let id = command
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("model ID is required")?;
                let mut registry = self.registry.lock().map_err(|_| "registry lock poisoned")?;
                let active = registry.active_model_id.as_deref() == Some(id);
                registry.remove(id).map_err(|error| error.to_string())?;
                if active {
                    let mut settings =
                        self.settings.lock().map_err(|_| "settings lock poisoned")?;
                    settings.active_model_id = None;
                    if settings.strategy == "object_detection" {
                        settings.strategy = "pixel_seek".into();
                    }
                }
                drop(registry);
                self.reconfigure()?;
                Ok(self.models_event())
            }
            _ => Err(format!("unsupported command type: {kind}")),
        }
    }

    fn update_model(&self, command: &Value) -> Result<Value, String> {
        let id = command
            .get("id")
            .and_then(Value::as_str)
            .ok_or("model ID is required")?;
        let mut registry = self.registry.lock().map_err(|_| "registry lock poisoned")?;
        let mut model = registry
            .models
            .get(id)
            .cloned()
            .ok_or("model does not exist")?;
        if let Some(value) = command.get("name").and_then(Value::as_str) {
            model.name = value.into();
        }
        if let Some(value) = command.get("input_width").and_then(Value::as_u64) {
            model.input_width = value as u32;
        }
        if let Some(value) = command.get("input_height").and_then(Value::as_u64) {
            model.input_height = value as u32;
        }
        if let Some(value) = command.get("confidence").and_then(Value::as_f64) {
            model.confidence = value as f32;
        }
        if let Some(value) = command.get("iou").and_then(Value::as_f64) {
            model.iou = value as f32;
        }
        if let Some(value) = command.get("device").and_then(Value::as_str) {
            model.device = value.into();
        }
        if let Some(value) = command.get("enabled").and_then(Value::as_bool) {
            model.enabled = value;
        }
        registry.update(model).map_err(|error| error.to_string())?;
        drop(registry);
        self.reconfigure()?;
        Ok(self.models_event())
    }

    fn models_event(&self) -> Value {
        let Ok(registry) = self.registry.lock() else {
            return json!({"event":"models","models":[],"active_model_id":null});
        };
        let models: Vec<_> = registry.models.values().collect();
        json!({"event":"models","models":models,"active_model_id":registry.active_model_id})
    }

    fn close(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut slot) = self.reader.lock() {
            if let Some(thread) = slot.take() {
                let _ = thread.join();
            }
        }
        if let Ok(mut source) = self.source.lock() {
            if let Some(active) = source.active.as_mut() {
                active.close();
            }
        }
        if !self.pipeline.close(Duration::from_secs(3)) {
            self.record_error("frame pipeline did not stop within three seconds".into());
        }
        let _ = self.movement_ticker.close(Duration::from_secs(1));
        let _ = self.movement_batcher.close(Duration::from_secs(1));
        let _ = self.store.close(Duration::from_secs(3));
    }
}

fn receiver_loop(service: Weak<ReceiverService>) {
    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        if service.stop.load(Ordering::Acquire) {
            return;
        }
        let retry = service.source.lock().ok().and_then(|state| {
            state
                .pending_port
                .filter(|_| Instant::now() >= state.retry_at)
        });
        if let Some(port) = retry {
            let crop_resolution = service
                .settings
                .lock()
                .ok()
                .map(|settings| settings.crop_resolution)
                .unwrap_or(640);
            let candidate = UdpDecodedFrameSource::new("0.0.0.0", port).and_then(|mut source| {
                source.set_crop_resolution(crop_resolution);
                source.open()?;
                Ok(source)
            });
            match candidate {
                Ok(candidate) => {
                    if let Ok(mut state) = service.source.lock() {
                        if state.pending_port == Some(port) && !service.stop.load(Ordering::Acquire)
                        {
                            state.active = Some(candidate);
                            state.active_port = port;
                            state.pending_port = None;
                            state.last_error = None;
                            if let Ok(mut settings) = service.settings.lock() {
                                settings.port = port.to_string();
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Ok(mut state) = service.source.lock() {
                        state.last_error = Some(error.to_string());
                        state.retry_at = Instant::now() + UDP_RETRY;
                    }
                }
            }
        }
        let read = service.source.lock().ok().map(|mut state| {
            let result = if let Some(source) = state.active.as_mut() {
                source.read(Duration::from_millis(100))
            } else {
                Ok(None)
            };
            // Readers of state() use this snapshot instead of waiting on the
            // source lock, which is held for the whole blocking read above.
            if let Ok(mut view) = service.source_view.lock() {
                *view = SourceView::of(&state);
            }
            result
        });
        match read {
            Some(Ok(Some(frame))) => {
                if let Ok(mut times) = service.source_times.lock() {
                    times.push_back(monotonic_ns());
                    if times.len() > 720 {
                        times.pop_front();
                    }
                }
                let _ = service.pipeline.submit(frame);
            }
            Some(Ok(None)) => thread::sleep(Duration::from_millis(5)),
            Some(Err(error)) => {
                let message = error.to_string();
                if let Ok(mut source) = service.source.lock() {
                    source.active = None;
                    source.pending_port = Some(source.active_port);
                    source.retry_at = Instant::now() + UDP_RETRY;
                    source.last_error = Some(message.clone());
                    if let Ok(mut view) = service.source_view.lock() {
                        *view = SourceView::of(&source);
                    }
                }
                service.record_error(message);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[derive(Clone)]
struct WebSocketState {
    service: Arc<ReceiverService>,
    token: String,
    config_port: Option<u16>,
}

#[derive(serde::Deserialize)]
struct WsQuery {
    token: String,
    /// `monitor` (counted, receives preview frames), `host` (native host: state
    /// only, no preview frames), or absent (config page: frames + state).
    #[serde(default)]
    role: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientRole {
    Monitor,
    Host,
    Config,
}

impl ClientRole {
    fn parse(role: Option<&str>) -> Self {
        match role {
            Some("monitor") => Self::Monitor,
            Some("host") => Self::Host,
            _ => Self::Config,
        }
    }
}

/// Decrements the monitor count however the socket loop ends.
struct MonitorGuard(Arc<ReceiverService>);

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.0.monitor_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn websocket_route(
    State(state): State<WebSocketState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let token_ok = token_matches(&state.token, &query.token);
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let allowed_origin = origin_allowed(origin, state.config_port);
    if !token_ok || !allowed_origin {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let role = ClientRole::parse(query.role.as_deref());
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            socket_loop(socket, state.service, role).await;
            // The native host keeps one role=host connection for its whole
            // life. If it closes, the host is gone (even if it was killed and
            // never ran its cleanup), so exit instead of lingering as an
            // orphan that keeps the UDP port and blocks the next launch.
            if role == ClientRole::Host {
                std::process::exit(0);
            }
        })
}

fn token_matches(expected: &str, supplied: &str) -> bool {
    expected.as_bytes().ct_eq(supplied.as_bytes()).unwrap_u8() == 1
}

fn origin_allowed(origin: Option<&str>, config_port: Option<u16>) -> bool {
    origin.is_none_or(|origin| {
        [
            "tauri://localhost",
            "http://tauri.localhost",
            "http://127.0.0.1:1420",
        ]
        .contains(&origin)
            || config_port.is_some_and(|port| {
                origin == format!("http://127.0.0.1:{port}")
                    || origin == format!("http://localhost:{port}")
            })
    })
}

async fn socket_loop(mut socket: WebSocket, service: Arc<ReceiverService>, role: ClientRole) {
    let _monitor = (role == ClientRole::Monitor).then(|| {
        service.monitor_clients.fetch_add(1, Ordering::Relaxed);
        MonitorGuard(Arc::clone(&service))
    });
    let initial_state = service.state();
    if send_text(&mut socket, initial_state.clone()).await.is_err() {
        return;
    }
    // Preview forwards every processed frame (no display-rate cap).
    let mut preview_rx = service.preview_rx.clone();
    // 100 ms keeps the monitor's Mouse State responsive; unchanged states are not resent.
    let mut state_ticker = tokio::time::interval(Duration::from_millis(100));
    let mut last_frame = None;
    let mut last_state = initial_state;
    loop {
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(Message::Text(text))) = message else { break; };
                if text.len() > MAX_MESSAGE_BYTES {
                    if send_text(&mut socket, json!({"event":"error","message":"commands must be under 64 KiB"})).await.is_err() { break; }
                    continue;
                }
                let parsed = serde_json::from_str::<Value>(&text);
                let request_id = parsed
                    .as_ref()
                    .ok()
                    .and_then(|command| command.get("request_id"))
                    .cloned();
                let command_type = parsed
                    .as_ref()
                    .ok()
                    .and_then(|command| command.get("type"))
                    .cloned();
                let response = match parsed {
                    Ok(command) => {
                        let service = Arc::clone(&service);
                        tokio::task::spawn_blocking(move || service.dispatch(command)).await
                            .unwrap_or_else(|error| Err(error.to_string()))
                    }
                    Err(error) => Err(error.to_string()),
                };
                let mut event = response.unwrap_or_else(|error| {
                    service.record_error(error.clone());
                    json!({"event":"error","type":command_type,"message":error})
                });
                attach_request_id(&mut event, request_id);
                if send_text(&mut socket, event).await.is_err() { break; }
            }
            // Only the monitor shows Preview; the config page just edits and
            // saves settings, so it gets state but never frames.
            changed = preview_rx.changed(), if role == ClientRole::Monitor => {
                if changed.is_err() { break; }
                if let Some(processed) = service.pipeline.latest() {
                    // A client that connects after the sender stopped must not
                    // receive the stale last frame (it would read as "Receiving").
                    if last_frame != Some(processed.published_ns) && preview_is_fresh(processed.published_ns, monotonic_ns()) {
                        match encode_frame(&service, &processed) {
                            Ok(bytes) => { if socket.send(Message::Binary(bytes.into())).await.is_err() { break; } last_frame = Some(processed.published_ns); }
                            Err(error) => service.record_error(error),
                        }
                    }
                }
            }
            // Settings are applied by another authenticated WebSocket client (the
            // browser config page). Keep every connected receiver client in sync
            // even while the frame stream is busy sending binary preview data.
            _ = state_ticker.tick() => {
                let state = service.state();
                if state != last_state {
                    if send_text(&mut socket, state.clone()).await.is_err() { break; }
                    last_state = state;
                }
            }
        }
    }
}

/// Same one-second window that `state()` uses for `streaming` vs `waiting`.
fn preview_is_fresh(published_ns: u64, now_ns: u64) -> bool {
    now_ns.saturating_sub(published_ns) < 1_000_000_000
}

/// Whether the device status reports the configured trigger button as held.
fn trigger_held(device: &Value, trigger_button: &str) -> bool {
    let index = match trigger_button {
        "right" => 1,
        "middle" => 2,
        "m4" => 3,
        "m5" => 4,
        _ => return false,
    };
    device["state"] == "connected" && device["buttons"][index] == true
}

fn missing_device() -> Value {
    json!({"state":"missing","name":null,"buttons":[null,null,null,null,null]})
}

/// Validates the native host's device report before it is broadcast.
fn parse_device_status(command: &Value) -> Result<Value, String> {
    let state = command
        .get("state")
        .and_then(Value::as_str)
        .filter(|state| ["connected", "missing", "error"].contains(state))
        .ok_or("device state must be connected, missing, or error")?;
    let name = match command.get("name") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(name)) if name.chars().count() <= 64 => json!(name),
        _ => return Err("device name must be a string of at most 64 characters".into()),
    };
    let buttons = command
        .get("buttons")
        .and_then(Value::as_array)
        .filter(|buttons| {
            buttons.len() == 5 && buttons.iter().all(|b| b.is_boolean() || b.is_null())
        })
        .ok_or("device buttons must be five booleans or nulls")?;
    Ok(json!({"state":state,"name":name,"buttons":buttons}))
}

fn attach_request_id(event: &mut Value, request_id: Option<Value>) {
    if let (Some(request_id), Some(object)) = (request_id, event.as_object_mut()) {
        object.insert("request_id".into(), request_id);
    }
}

fn encode_frame(service: &ReceiverService, processed: &ProcessedFrame) -> Result<Vec<u8>, String> {
    // The receiver processes the sender's frame as-is, so the original JPEG is
    // exactly what Preview shows; re-encode only when it is unavailable.
    let encoded;
    let jpeg: &[u8] = if processed.frame.jpeg.is_empty() {
        let mut buffer = Vec::new();
        JpegEncoder::new_with_quality(&mut buffer, 88)
            .encode_image(&image::DynamicImage::ImageRgb8(
                processed.frame.image.clone(),
            ))
            .map_err(|error| error.to_string())?;
        encoded = buffer;
        &encoded
    } else {
        &processed.frame.jpeg
    };
    let stats = service.pipeline.stats();
    let state = service.state();
    let metadata = json!({
        "event":"frame", "frame_id":processed.frame.frame_id, "source":processed.frame.source,
        "source_width":processed.result.source_width, "source_height":processed.result.source_height,
        "crop_x":processed.crop_x, "crop_y":processed.crop_y, "mode":processed.result.mode,
        "model_id":processed.result.model_id, "processing_fps":stats.processing_fps, "source_fps":state["source_fps"],
        "dropped_input_frames":stats.dropped_input_frames, "rejected_frames":stats.rejected_frames,
        "resolution_mismatches":stats.resolution_mismatches, "transport_dropped_frames":state["transport_dropped_frames"],
        "detection":processed.result,
    });
    let bytes = serde_json::to_vec(&metadata).map_err(|error| error.to_string())?;
    let size = u32::try_from(bytes.len()).map_err(|_| "frame metadata too large")?;
    let mut output = Vec::with_capacity(4 + bytes.len() + jpeg.len());
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&bytes);
    output.extend_from_slice(jpeg);
    Ok(output)
}

async fn send_text(socket: &mut WebSocket, value: Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(value.to_string().into())).await
}

fn finite_number(value: Option<&Value>, message: &str) -> Result<f64, String> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| message.into())
}

pub async fn serve(
    token: String,
    data_dir: PathBuf,
    host: String,
    ws_port: u16,
    config_root: Option<PathBuf>,
    config_port: u16,
    udp_port: u16,
) -> Result<(), String> {
    if !["127.0.0.1", "localhost", "::1"].contains(&host.as_str()) {
        return Err("desktop IPC server must bind to loopback only".into());
    }
    let service = ReceiverService::new(token.clone(), data_dir.clone(), udp_port)?;
    let listener = TcpListener::bind((host.as_str(), ws_port))
        .await
        .map_err(|error| error.to_string())?;
    let actual_port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let (config_actual_port, config_task, config_error) = if let Some(root) = config_root {
        match web_config::start(root, data_dir, token.clone(), actual_port, config_port).await {
            Ok((port, task)) => (Some(port), Some(task), None),
            Err(error) => (
                None,
                None,
                Some(format!("Local configuration page could not start: {error}")),
            ),
        }
    } else {
        (None, None, None)
    };
    let web_state = WebSocketState {
        service: Arc::clone(&service),
        token,
        config_port: config_actual_port,
    };
    let app = Router::new()
        .route("/receiver", get(websocket_route))
        .with_state(web_state);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let ws_task: TokioJoinHandle<Result<(), std::io::Error>> = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    println!(
        "{}",
        json!({"event":"ready","port":actual_port,"config_port":config_actual_port,"config_error":config_error})
    );
    let _ = tokio::io::stdout().flush().await;
    wait_for_shutdown().await;
    let _ = shutdown_tx.send(());
    let _ = ws_task.await;
    if let Some(task) = config_task {
        task.abort();
    }
    service.close();
    Ok(())
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => return,
            };
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn monotonic_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{save_settings, AppSettings};
    use crate::udp::encode_jpeg_frame;
    use std::net::UdpSocket;

    struct ServiceGuard(Arc<ReceiverService>);

    impl Drop for ServiceGuard {
        fn drop(&mut self) {
            self.0.close();
        }
    }

    fn reserve_udp_socket() -> UdpSocket {
        UdpSocket::bind(("127.0.0.1", 0)).expect("reserve ephemeral UDP port")
    }

    fn test_service(label: &str) -> (Arc<ReceiverService>, PathBuf) {
        let reservation = reserve_udp_socket();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let data_dir =
            std::env::temp_dir().join(format!("sysx-{label}-{}-{}", std::process::id(), unix_ms()));
        let service = ReceiverService::new("b".repeat(64), data_dir.clone(), port)
            .expect("start test receiver service");
        (service, data_dir)
    }

    #[test]
    fn first_start_creates_the_persistent_settings_file() {
        let (service, data_dir) = test_service("initial-settings");
        let _guard = ServiceGuard(Arc::clone(&service));
        let path = data_dir.join("app-settings.json");
        assert!(path.is_file());
        assert_eq!(
            load_settings(path).port,
            service.state()["port"].to_string()
        );
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn device_status_is_validated_before_broadcast() {
        let valid =
            json!({"state":"connected","name":"Medius","buttons":[true,false,null,false,false]});
        assert_eq!(super::parse_device_status(&valid).unwrap(), valid);
        let unnamed = super::parse_device_status(
            &json!({"state":"missing","buttons":[null,null,null,null,null]}),
        )
        .unwrap();
        assert_eq!(unnamed["name"], Value::Null);
        for invalid in [
            json!({"state":"hacked","buttons":[null,null,null,null,null]}),
            json!({"state":"connected","buttons":[true,false]}),
            json!({"state":"connected","buttons":[1,0,0,0,0]}),
            json!({"state":"connected","name":"x".repeat(65),"buttons":[null,null,null,null,null]}),
        ] {
            assert!(super::parse_device_status(&invalid).is_err());
        }
        assert!(matches!(
            super::ClientRole::parse(Some("monitor")),
            super::ClientRole::Monitor
        ));
        assert!(matches!(
            super::ClientRole::parse(Some("host")),
            super::ClientRole::Host
        ));
        assert!(matches!(
            super::ClientRole::parse(None),
            super::ClientRole::Config
        ));
    }

    #[test]
    fn stale_preview_frames_are_not_sent_to_new_clients() {
        let published = 5_000_000_000;
        assert!(super::preview_is_fresh(published, published + 999_999_999));
        assert!(!super::preview_is_fresh(
            published,
            published + 1_000_000_000
        ));
        assert!(super::preview_is_fresh(published, published - 1));
    }

    #[test]
    fn websocket_origin_and_token_gates_reject_non_local_or_wrong_credentials() {
        assert!(super::origin_allowed(Some("tauri://localhost"), None));
        assert!(super::origin_allowed(
            Some("http://127.0.0.1:4201"),
            Some(4201)
        ));
        assert!(!super::origin_allowed(
            Some("https://example.invalid"),
            Some(4201)
        ));
        assert!(super::token_matches("same secret", "same secret"));
        assert!(!super::token_matches("same secret", "other secret"));
    }

    #[test]
    fn apply_settings_acknowledges_recoil_and_echoes_request_ids() {
        let (service, data_dir) = test_service("apply-settings");
        let _guard = ServiceGuard(Arc::clone(&service));
        let recoil = RecoilSettings {
            recoil_enabled: true,
            recoil_levels: vec![2, 4],
            recoil_level_duration_ms: 50,
            recoil_percentage: 75,
            ..RecoilSettings::default()
        };
        let response = service
            .dispatch(json!({
                "version":1,
                "type":"apply_settings",
                "request_id":"apply-7",
                "commands":[{
                    "version":1,
                    "type":"set_recoil",
                    "settings":recoil
                }]
            }))
            .expect("apply recoil settings");
        assert_eq!(response["event"], "ack");
        assert_eq!(response["type"], "apply_settings");
        assert_eq!(response["state"]["recoil"]["recoilEnabled"], true);
        assert_eq!(response["state"]["recoil"]["recoilLevels"][1], 4);

        let mut event = response;
        attach_request_id(&mut event, Some(json!("apply-7")));
        assert_eq!(event["request_id"], "apply-7");

        let error = service.dispatch(json!({
            "version":1,
            "type":"apply_settings",
            "commands":[{
                "version":1,
                "type":"set_recoil",
                "settings":{"recoilEnabled":true,"recoilLevels":[1],"recoilLevelDurationMs":0}
            }]
        }));
        assert!(error.is_err());
        let mut error_event =
            json!({"event":"error","type":"apply_settings","message":"invalid recoil settings"});
        attach_request_id(&mut error_event, Some(json!("apply-8")));
        assert_eq!(error_event["request_id"], "apply-8");
        assert_eq!(service.state()["recoil"]["recoilEnabled"], true);

        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn changing_udp_port_binds_new_listener_and_processes_a_jpeg_frame() {
        let initial_reservation = reserve_udp_socket();
        let initial_port = initial_reservation
            .local_addr()
            .expect("read initial test port")
            .port();
        let target_reservation = loop {
            let candidate = reserve_udp_socket();
            if candidate
                .local_addr()
                .expect("read target test port")
                .port()
                != initial_port
            {
                break candidate;
            }
        };
        let target_port = target_reservation
            .local_addr()
            .expect("read target test port")
            .port();
        drop((initial_reservation, target_reservation));
        let data_dir = std::env::temp_dir().join(format!(
            "sysx-port-switch-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let mut settings = AppSettings::default();
        settings.port = initial_port.to_string();
        settings.crop_resolution = 192;
        save_settings(data_dir.join("app-settings.json"), &settings)
            .expect("write integration settings");
        let service = ReceiverService::new("a".repeat(64), data_dir.clone(), initial_port)
            .expect("start UDP service on initial port");
        let _guard = ServiceGuard(Arc::clone(&service));

        let response = service
            .dispatch(json!({"version":1,"type":"set_udp_port","port":target_port}))
            .expect("switch UDP port");
        assert_eq!(response["state"]["port"], target_port);

        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/assets/red-cube-192.jpg");
        let jpeg = std::fs::read(fixture).expect("read actual JPEG smoke fixture");
        let packets = encode_jpeg_frame(1, 192, 192, 1, &jpeg).expect("packetize JPEG frame");
        let sender = UdpSocket::bind(("127.0.0.1", 0)).expect("bind local test sender");
        for packet in packets {
            sender
                .send_to(&packet, ("127.0.0.1", target_port))
                .expect("send JPEG packet to updated port");
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while service.pipeline.stats().processed_frames == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(service.state()["port"], target_port);
        assert!(
            service.pipeline.stats().processed_frames > 0,
            "the JPEG sent to the updated port must reach the frame pipeline"
        );

        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
