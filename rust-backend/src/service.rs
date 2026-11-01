use crate::config::{
    load_settings, save_settings, AppSettings, AutoSettings, ModelDefinition, ModelRegistry,
    RecoilSettings,
};
use crate::decode_pool::DecodePool;
use crate::movement::{
    MovementBatcher, MovementRuntimeSettings, MovementTargetSample, MovementTicker,
};
use crate::pipeline::{LatestFramePipeline, PipelineConfig, ProcessedFrame};
use crate::pixel_seek::PixelTargetColor;
use crate::receiver::UdpDecodedFrameSource;
use crate::results::DetectionMode;
use crate::targeting::SmoothingRange;
use crate::udp::ReassemblyStats;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use image::codecs::jpeg::JpegEncoder;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinHandle as TokioJoinHandle;

const UDP_RETRY: Duration = Duration::from_secs(3);
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// WebSocket limit above the command limit, so an oversized command gets an
/// error reply instead of closing the socket (closing the host's socket
/// stops the backend).
const MAX_SOCKET_MESSAGE_BYTES: usize = 1024 * 1024;

fn emit_stdout_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Serialize control changes and target samples on the same stdout channel.
/// Controls have no fresh target; a revision also cancels pending actions when
/// several changes occur between two device polls (for example off then on).
struct AutoEvents {
    settings: AutoSettings,
    revision: u64,
    emit: Arc<dyn Fn(Value) + Send + Sync>,
}

impl AutoEvents {
    fn configure(&mut self, settings: &AutoSettings) {
        if self.settings != *settings {
            self.settings = settings.clone();
            self.revision = self.revision.wrapping_add(1);
            (self.emit)(self.event("auto_config", false, 0));
        }
    }

    /// Every routing/configuration boundary invalidates old frame callbacks and
    /// queued Motion samples, including trigger release/press with unchanged settings.
    fn reconfigure(&mut self, settings: &AutoSettings) {
        let previous = self.revision;
        self.configure(settings);
        if self.revision == previous {
            self.revision = self.revision.wrapping_add(1);
            (self.emit)(self.event("auto_config", false, 0));
        }
    }

    fn event(&self, kind: &str, target_valid: bool, timestamp_ms: u64) -> Value {
        json!({
            "event": kind,
            "config_revision": self.revision,
            "enabled": self.settings.enabled,
            "trigger_button": self.settings.trigger_button,
            "delay_ms": self.settings.delay_ms,
            "cooldown_ms": self.settings.cooldown_ms,
            "sleep_ms": self.settings.sleep_ms,
            "target_valid": target_valid,
            "timestamp_ms": timestamp_ms,
        })
    }

    fn target(&self, revision: u64, valid: bool) {
        // A processing callback can be delayed after releasing the pipeline
        // lock. It must not rearm Auto using a pre-edit target.
        if revision == self.revision {
            (self.emit)(self.event("auto_target", valid, unix_ms()));
        }
    }
}

#[derive(Default)]
struct CaptureState {
    sequence: u64,
    current: Option<(crate::motion_capture::CaptureRequest, Value)>,
    count: usize,
    message: String,
}
impl CaptureState {
    fn invalidate(&mut self) {
        self.current = None;
        self.count = 0;
        self.message.clear();
    }
    fn request(
        &mut self,
        settings: &AppSettings,
        connected: bool,
        session: &str,
    ) -> Option<crate::motion_capture::CaptureRequest> {
        if !settings.motion_renderer.enabled || !settings.motion_renderer.reports.is_empty() {
            self.invalidate();
            return None;
        }
        if !connected {
            self.invalidate();
            self.message =
                "Connect the Medius mouse to collect a profile. Normal remains active.".into();
            return None;
        }
        let identity = crate::config::capture_identity(settings);
        if self
            .current
            .as_ref()
            .is_none_or(|(_, old)| *old != identity)
        {
            self.sequence = self.sequence.wrapping_add(1);
            self.current = Some((
                crate::motion_capture::CaptureRequest {
                    id: format!("{session}:{}", self.sequence),
                },
                identity,
            ));
            self.count = 0;
            self.message = "Move the physical mouse continuously at 1,000 Hz to collect 256 reports. Normal remains active.".into();
        }
        self.current.as_ref().map(|(request, _)| request.clone())
    }
    fn matches(&self, id: &str, settings: &AppSettings) -> bool {
        self.current.as_ref().is_some_and(|(request, identity)| {
            request.id == id && *identity == crate::config::capture_identity(settings)
        })
    }
}

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
    settings_path: PathBuf,
    registry: Arc<Mutex<ModelRegistry>>,
    /// Built-in models by name, with their FP16/INT8 variants.
    builtin: Mutex<Vec<crate::builtin_models::BuiltinModel>>,
    pipeline: Arc<LatestFramePipeline>,
    movement_batcher: Arc<MovementBatcher>,
    movement_ticker: Arc<MovementTicker>,
    motion_renderer: Arc<Mutex<crate::movement::abcurves::ProfileCache>>,
    motion_capture: Mutex<CaptureState>,
    source: Arc<Mutex<SourceState>>,
    source_times: Arc<Mutex<VecDeque<u64>>>,
    /// Decodes the frames the receiver thread reassembles.
    decoder: DecodePool,
    errors: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    session_id: String,
    /// Latest Medius status reported by the native host
    /// (`device_status` command); forwarded to the native monitor in `state`.
    device: Arc<Mutex<Value>>,
    auto_events: Arc<Mutex<AutoEvents>>,
    /// Open `role=monitor` WebSocket clients.
    monitor_clients: AtomicUsize,
    /// Published-at timestamp of the newest processed frame; changes once per
    /// frame so each preview socket forwards every input frame.
    preview_rx: tokio::sync::watch::Receiver<u64>,
    source_view: Mutex<SourceView>,
    /// The request last handed to the UDP source; compared before locking the
    /// source, which the receiver thread holds during its blocking reads.
    /// Crop last sent to the plugin with SYSCFG.
    frame_request: Mutex<u32>,
}

impl ReceiverService {
    fn new(token: String, data_dir: PathBuf, requested_udp_port: u16) -> Result<Arc<Self>, String> {
        if token.len() < 32 {
            return Err("IPC token must contain at least 32 characters".into());
        }
        std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
        let lut_warmer = match thread::Builder::new()
            .name("sysx-pixel-lut-warmup".into())
            .spawn(crate::pixel_seek::warm_profile_luts)
        {
            Ok(worker) => Some(worker),
            Err(_) => {
                crate::pixel_seek::warm_profile_luts();
                None
            }
        };
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
        let builtin = {
            let mut app = settings.lock().map_err(|_| "settings lock poisoned")?;
            let mut models = registry
                .lock()
                .map_err(|_| "model registry lock poisoned")?;
            let builtin =
                crate::builtin_models::install_embedded(&data_dir, &mut models, &mut app)?;
            if app.strategy == "object_detection"
                && models
                    .active_model_id
                    .as_ref()
                    .and_then(|id| models.models.get(id))
                    .is_none()
            {
                app.strategy = "pixel_seek".into();
            }
            builtin
        };
        let session_id = format!("{}", unix_ms());
        // Per-frame detection logs were written here (about 290 MB per hour
        // at 240 FPS, never read back); reclaim what older versions left.
        let _ = std::fs::remove_dir_all(data_dir.join("sessions"));
        // Motion no longer learns; older versions saved what they learned here.
        let _ = std::fs::remove_file(data_dir.join("motion-calibration.json"));
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
        let model_size = model.as_ref().map(|model| model.input_width);
        let config = PipelineConfig {
            strategy: mode,
            pixel_color,
            crop_resolution: Some(capture_size(&app, model_size)),
            fov_degrees: app.fov_degrees,
            auto_fov_degrees: app.magnet.fov_degrees,
            auto_target_active: false,
            target_lock_active: app.motion_enabled && app.motion_always_on,
            auto_config_revision: 0,
            color_filter: app.color_filter,
            color_swap: app.color_swap(),
            ability_confidence: app.ability_confidence as f32,
            model,
            model_backend: app.model_backend.clone(),
            compute_target: app.compute_target.clone(),
        };
        let movement_batcher = Arc::new(
            MovementBatcher::new(Arc::new(|event| {
                if let Ok(line) = serde_json::to_string(&event) {
                    emit_stdout_line(&line);
                }
            }))
            .map_err(|error| format!("could not start movement batcher: {error}"))?,
        );
        let device = Arc::new(Mutex::new(missing_device()));
        let auto_events = Arc::new(Mutex::new(AutoEvents {
            settings: routed_auto_settings(&app, &missing_device()),
            revision: 0,
            emit: Arc::new(|event| {
                if let Ok(line) = serde_json::to_string(&event) {
                    emit_stdout_line(&line);
                }
            }),
        }));
        let motion_renderer = Arc::new(Mutex::new(crate::movement::abcurves::ProfileCache::new(
            &app.motion_renderer,
        )?));
        let ticker_renderer = Arc::clone(&motion_renderer);
        let ticker_settings = Arc::clone(&settings);
        let ticker_device = Arc::clone(&device);
        let ticker_auto = Arc::clone(&auto_events);
        let ticker_batcher = Arc::clone(&movement_batcher);
        let movement_ticker = Arc::new(
            MovementTicker::new(
                app.sens,
                Arc::new(move || {
                    let settings = ticker_settings.lock().ok()?;
                    let device = ticker_device.lock().ok()?;
                    let renderer_profile = ticker_renderer.lock().ok()?.profile.clone();
                    let config_revision = ticker_auto.lock().ok()?.revision;
                    Some(movement_runtime(
                        &settings,
                        &device,
                        renderer_profile,
                        config_revision,
                    ))
                }),
                Arc::new(move |event| ticker_batcher.submit(event)),
            )
            .map_err(|error| format!("could not start movement ticker: {error}"))?,
        );
        let callback_ticker = Arc::clone(&movement_ticker);
        let callback_settings = Arc::clone(&settings);
        let callback_device = Arc::clone(&device);
        let callback_auto = Arc::clone(&auto_events);
        let (preview_tx, preview_rx) = tokio::sync::watch::channel(0_u64);
        let callback: Arc<dyn Fn(&ProcessedFrame) + Send + Sync> = Arc::new(move |processed| {
            // Wake every preview socket for this frame (no timer polling:
            // Windows timers tick at ~15.6 ms, which capped Preview near 60 FPS).
            preview_tx.send_replace(processed.published_ns);
            // Hold the same settings lock used by reconfigure until publication.
            // The pipeline drops its lock before callbacks, so generation checks
            // alone cannot reject a callback delayed across a mode change.
            if let (Ok(settings), Ok(device)) = (callback_settings.lock(), callback_device.lock()) {
                if let Ok(events) = callback_auto.lock() {
                    if processed.auto_config_revision != events.revision {
                        return;
                    }
                    let point = |point: [f32; 2]| (f64::from(point[0]), f64::from(point[1]));
                    callback_ticker.update(MovementTargetSample {
                        config_revision: processed.auto_config_revision,
                        frame_id: processed.frame.frame_id,
                        target: if processed.result.auto_active {
                            None
                        } else {
                            processed.result.target_xy.map(point)
                        },
                        target_box: processed.target_box.map(|bbox| bbox.map(f64::from)),
                        crop_origin: (processed.crop_x, processed.crop_y),
                        processed_size: (
                            processed.result.processed_width,
                            processed.result.processed_height,
                        ),
                        received_s: 0.0,
                        age_s: processed.frame.received.elapsed().as_secs_f64(),
                    });
                    let auto_active = effective_detection_settings(&settings, &device).2;
                    let target_valid = auto_active && processed.result.auto_target_valid;
                    events.target(processed.auto_config_revision, target_valid);
                    // Every mode, Auto included: each trigger press snaps.
                    if settings.magnet.enabled {
                        (events.emit)(magnet_event(
                            &settings,
                            processed,
                            processed.auto_config_revision,
                        ));
                    }
                }
            }
        });
        let pipeline = Arc::new(
            LatestFramePipeline::new(config, Some(callback))
                .map_err(|error| format!("could not start frame processing worker: {error}"))?,
        );
        let source_times = Arc::new(Mutex::new(VecDeque::with_capacity(720)));
        let decoder = {
            let (times, pipeline) = (Arc::clone(&source_times), Arc::clone(&pipeline));
            DecodePool::new(
                crate::decode_pool::default_workers(),
                Arc::new(move |frame| {
                    if let Ok(mut times) = times.lock() {
                        times.push_back(monotonic_ns());
                        if times.len() > 720 {
                            times.pop_front();
                        }
                    }
                    let _ = pipeline.submit(frame);
                }),
            )
            .map_err(|error| format!("could not start JPEG decoder pool: {error}"))?
        };
        let mut initial_source = UdpDecodedFrameSource::new(
            "0.0.0.0",
            app.port.parse::<u16>().unwrap_or(requested_udp_port),
        )
        .map_err(|error| error.to_string())?;
        initial_source.set_crop_resolution(frame_request(&app, model_size));
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
            settings_path: settings_path.clone(),
            registry,
            builtin: Mutex::new(builtin),
            pipeline,
            movement_batcher,
            movement_ticker,
            motion_renderer,
            motion_capture: Mutex::new(CaptureState::default()),
            source_view: Mutex::new(SourceView::of(&source_state)),
            frame_request: Mutex::new(frame_request(&app, model_size)),
            source: Arc::new(Mutex::new(source_state)),
            source_times,
            decoder,
            errors: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            reader: Mutex::new(None),
            session_id,
            device,
            auto_events,
            monitor_clients: AtomicUsize::new(0),
            preview_rx,
        });
        if let Some(worker) = lut_warmer {
            worker
                .join()
                .map_err(|_| "Pixel Seek lookup warmup thread panicked".to_owned())?;
        }
        Self::start_reader(&service).map_err(|error| error.to_string())?;
        Ok(service)
    }

    fn start_reader(service: &Arc<Self>) -> std::io::Result<()> {
        let weak = Arc::downgrade(service);
        let thread = thread::Builder::new()
            .name("sysx-udp-receiver".into())
            .spawn(move || receiver_supervisor(weak))?;
        match service.reader.lock() {
            Ok(mut slot) => *slot = Some(thread),
            Err(_) => {
                service.stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(std::io::Error::other(
                    "receiver thread handle lock poisoned",
                ));
            }
        }
        Ok(())
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
        let decoded = self.decoder.stats();
        let source_name;
        let source_port;
        let transport;
        let source_error;
        // Never wait on the source lock: the UDP thread holds it while blocked
        // in a read, which used to stall every state()/preview send by up to a
        // frame interval. Fall back to the snapshot that thread keeps current.
        let view = self.source_view_now();
        source_port = view.port;
        source_name = format!("UDP Â· 0.0.0.0:{}", view.port);
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
        let model_size = models.as_ref().and_then(|registry| {
            let id = registry.active_model_id.as_ref()?;
            registry.models.get(id).map(|model| model.input_width)
        });
        let capture = capture_size(&settings, model_size);
        let expected_source_resolution = Some(vec![capture, capture]);
        // History remains available for diagnostics, but a recovered error
        // must not masquerade as a current operational failure.
        let renderer_error = self
            .motion_renderer
            .lock()
            .ok()
            .and_then(|cache| cache.profile.as_ref().and_then(|profile| profile.error()));
        let last_error = stats.last_error.clone().or(source_error).or(renderer_error);
        let device = self
            .device
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| missing_device());
        let (effective_fov_degrees, effective_pixel_color, auto_trigger_held) =
            effective_detection_settings(&settings, &device);
        let (capture_request, capture_count, capture_message) = self
            .motion_capture
            .lock()
            .map(|mut capture| {
                let request =
                    capture.request(&settings, device["state"] == "connected", &self.session_id);
                (request, capture.count, capture.message.clone())
            })
            .unwrap_or_default();
        let profiles: Vec<Value> = settings
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| json!({"index":index,"name":profile.name}))
            .collect();
        json!({
            "event":"state", "version":1, "status":if is_live { "streaming" } else { "waiting" }, "input":"udp", "source":source_name,
            "mode":settings.strategy, "fov_degrees":settings.fov_degrees, "target_offset_x":settings.offset_x, "target_offset_y":settings.offset_y,
            "motion_capture":capture_request,"motion_capture_count":capture_count,"motion_capture_message":capture_message,
            "motion_renderer":settings.motion_renderer, "motion_enabled":settings.motion_enabled, "motion_always_on":settings.motion_always_on, "motion_toggle":settings.motion_toggle, "motion_toggled":motion_toggled(&settings, &device), "color_filter":settings.color_filter, "trigger_button":settings.trigger_button, "smoothing_x_min":settings.smoothing_x_min, "smoothing_x_max":settings.smoothing_x_max, "smoothing_y_min":settings.smoothing_y_min, "smoothing_y_max":settings.smoothing_y_max,
            "sens":settings.sens, "speed_booster":settings.speed_booster, "speed_near":settings.speed_near, "speed_mid":settings.speed_mid, "speed_far":settings.speed_far,
            "continue_inside_box":settings.continue_inside_box, "motion_speed_x":settings.motion_speed_x, "motion_speed_y":settings.motion_speed_y, "smooth_fov_degrees":settings.smooth_fov_degrees, "preview_scale":settings.scale, "crop_resolution":settings.crop_resolution,
            "recoil":settings.recoil,
            "auto":settings.auto,
            "magnet":settings.magnet,
            "pixel_color":settings.pixel_color, "pixel_profile_configured":true,
            "effective_fov_degrees":effective_fov_degrees,
            "effective_pixel_color":effective_pixel_color,
            "auto_trigger_held":auto_trigger_held,
            "profile_index":settings.active_profile,
            "profile_count":profiles.len(),
            "profiles":profiles,
            "active_model_id":active_model_id,
            "expected_source_resolution":expected_source_resolution,
            "session_id":self.session_id, "processed_frames":stats.processed_frames, "dropped_input_frames":stats.dropped_input_frames,
            "rejected_frames":stats.rejected_frames, "resolution_mismatches":stats.resolution_mismatches,
            "transport_dropped_frames":transport.dropped_frames + transport.expired_frames + decoded.superseded, "invalid_packets":transport.invalid_packets,
            "invalid_frames":transport.invalid_frames + decoded.failed,
            "processing_fps":stats.processing_fps, "source_fps":source_fps, "port":source_port, "last_error":last_error,
            "device":device,
            "monitor_clients":self.monitor_clients.load(Ordering::Relaxed),
        })
    }

    fn source_view_now(&self) -> SourceView {
        match self.source.try_lock() {
            Ok(source) => SourceView::of(&source),
            Err(_) => self
                .source_view
                .lock()
                .map(|view| view.clone())
                .unwrap_or_default(),
        }
    }

    fn source_fps(&self) -> Option<f64> {
        let now = monotonic_ns();
        let times = self.source_times.lock().ok()?;
        let (mut first, mut last, mut count) = (None, None, 0usize);
        for stamp in times
            .iter()
            .copied()
            .filter(|stamp| now.saturating_sub(*stamp) <= 1_000_000_000)
        {
            first.get_or_insert(stamp);
            last = Some(stamp);
            count += 1;
        }
        let (Some(first), Some(last)) = (first, last) else {
            return None;
        };
        if count < 2 || last <= first {
            return None;
        }
        Some((count - 1) as f64 * 1e9 / (last - first) as f64)
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
        let model_size = self.active_model_size();
        let request = frame_request(
            &*self.settings.lock().map_err(|_| "settings lock poisoned")?,
            model_size,
        );
        candidate.set_crop_resolution(request);
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

    fn prepare_udp_source(
        &self,
        port: u16,
        request: u32,
    ) -> Result<Option<UdpDecodedFrameSource>, String> {
        let source = lock_source(&self.source);
        if source.active_port == port && source.active.is_some() {
            return Ok(None);
        }
        drop(source);
        let mut candidate =
            UdpDecodedFrameSource::new("0.0.0.0", port).map_err(|error| error.to_string())?;
        candidate.set_crop_resolution(request);
        candidate.open().map_err(|error| error.to_string())?;
        Ok(Some(candidate))
    }

    fn pipeline_config(&self, settings: &AppSettings) -> Result<PipelineConfig, String> {
        let strategy = match settings.strategy.as_str() {
            "pixel_seek" => DetectionMode::PixelSeek,
            "object_detection" => DetectionMode::ObjectDetection,
            _ => return Err("invalid detection strategy".into()),
        };
        let device = self
            .device
            .lock()
            .map_err(|_| "device lock poisoned")?
            .clone();
        let (_, pixel_color_name, auto_target_active) =
            effective_detection_settings(settings, &device);
        // Auto uses `auto_fov_degrees`; Motion keeps its own FOV meanwhile.
        let fov_degrees = if magnet_active(settings, &device) {
            magnet_fov(settings, &device)
        } else {
            motion_fov(settings, &device)
        };
        let target_lock_active = auto_target_active
            || motion_target_active(settings, &device)
            || magnet_active(settings, &device);
        let pixel_color =
            PixelTargetColor::parse(&pixel_color_name).ok_or("invalid Pixel Seek color")?;
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
            crop_resolution: Some(capture_size(
                settings,
                model.as_ref().map(|model| model.input_width),
            )),
            fov_degrees,
            auto_fov_degrees: magnet_fov(settings, &device),
            auto_target_active,
            target_lock_active,
            auto_config_revision: 0, // filled atomically by reconfigure()
            color_filter: settings.color_filter,
            color_swap: settings.color_swap(),
            ability_confidence: settings.ability_confidence as f32,
            model,
            model_backend: settings.model_backend.clone(),
            compute_target: settings.compute_target.clone(),
        })
    }

    fn reconfigure(&self) -> Result<(), String> {
        // Crop, engine, color and mask settings decide what the plugin sends.
        // Settings and source are never locked together.
        let model_size = self.active_model_size();
        let request = frame_request(
            &*self.settings.lock().map_err(|_| "settings lock poisoned")?,
            model_size,
        );
        let changed = self
            .frame_request
            .lock()
            .map(|mut last| std::mem::replace(&mut *last, request) != request)
            .unwrap_or(true);
        if changed {
            if let Some(active) = lock_source(&self.source).active.as_mut() {
                active.set_crop_resolution(request);
            }
        }
        let settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        // Publish controls even if no frame arrives or pipeline configuration
        // fails. In particular, disabling Auto must never depend on video.
        let device = self
            .device
            .lock()
            .map_err(|_| "device lock poisoned")?
            .clone();
        let mut events = self
            .auto_events
            .lock()
            .map_err(|_| "Auto event lock poisoned")?;
        events.reconfigure(&routed_auto_settings(&settings, &device));
        let revision = events.revision;
        drop(events);
        self.motion_capture
            .lock()
            .map_err(|_| "Capture lock poisoned")?
            .invalidate();
        // A renderer preparation error must not suppress Auto control events.
        self.motion_renderer
            .lock()
            .map_err(|_| "Motion renderer lock poisoned")?
            .configure(&settings.motion_renderer)?;
        let mut config = self.pipeline_config(&settings)?;
        config.auto_config_revision = revision;
        self.pipeline.configure(config).map_err(str::to_owned)
    }

    fn reconfigure_pipeline(&self) -> Result<(), String> {
        let settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        let device = self
            .device
            .lock()
            .map_err(|_| "device lock poisoned")?
            .clone();
        let mut events = self
            .auto_events
            .lock()
            .map_err(|_| "Auto event lock poisoned")?;
        events.reconfigure(&routed_auto_settings(&settings, &device));
        let revision = events.revision;
        drop(events);
        let mut config = self.pipeline_config(&settings)?;
        config.auto_config_revision = revision;
        self.pipeline.configure(config).map_err(str::to_owned)
    }

    fn complete_motion_capture(&self, command: &Value) -> Result<Value, String> {
        let captured: crate::motion_capture::CapturedProfile = serde_json::from_value(
            command
                .get("capture")
                .cloned()
                .ok_or("Physical capture is required")?,
        )
        .map_err(|_| "Invalid physical capture")?;
        captured.validate()?;
        {
            let settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
            let capture = self
                .motion_capture
                .lock()
                .map_err(|_| "Capture lock poisoned")?;
            if !capture.matches(&captured.capture_id, &settings) {
                return Err("Capture was cancelled or belongs to a previous profile".into());
            }
        }
        let renderer = crate::config::MotionRendererSettings {
            enabled: true,
            sample_interval_us: 1000,
            reports: captured.reports,
        };
        let prepared = crate::movement::abcurves::ProfileCache::new(&renderer)?;
        let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        let mut capture = self
            .motion_capture
            .lock()
            .map_err(|_| "Capture lock poisoned")?;
        if !capture.matches(&captured.capture_id, &settings)
            || self.device.lock().map_err(|_| "device lock poisoned")?["state"] != "connected"
        {
            return Err("Capture was cancelled or device changed".into());
        }
        let saved =
            match crate::config::save_captured_renderer(&self.settings_path, &settings, renderer) {
                Ok(saved) => saved,
                Err(error) => {
                    capture.message = format!(
                        "Could not save mouse profile: {error}. Normal remains active; retrying."
                    );
                    return Err(error.to_string());
                }
            };
        *self
            .motion_renderer
            .lock()
            .map_err(|_| "Motion renderer lock poisoned")? = prepared;
        *settings = saved;
        capture.invalidate();
        drop(capture);
        drop(settings);
        // Only the renderer profile changed. Do not reset detection/routing or
        // Auto's control revision: a background capture must not cancel a click.
        Ok(json!({"event":"ack","type":"complete_motion_capture","state":self.state()}))
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
            "get_app_settings" => Ok(json!({
                "event":"app_settings",
                "settings":*self.settings.lock().map_err(|_| "settings lock poisoned")?,
                "builtin_models":*self.builtin.lock().map_err(|_| "model catalog lock poisoned")?
            })),
            "apply_app_settings" => {
                let mut settings: AppSettings = serde_json::from_value(
                    command
                        .get("settings")
                        .cloned()
                        .ok_or("app settings are required")?,
                )
                .map_err(|_| "Invalid app settings")?;
                if settings.profiles.is_empty() {
                    settings.profiles = self
                        .settings
                        .lock()
                        .map_err(|_| "settings lock poisoned")?
                        .profiles
                        .clone();
                }
                settings.sync_active_profile();
                // Settings changes profiles only through activate_profile; a
                // draft whose profile differs was taken before a middle-click
                // cycle. Save its edits into that profile but keep the current
                // one active instead of switching back.
                let current_active = self
                    .settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .active_profile;
                if settings.active_profile != current_active
                    && usize::from(current_active) < settings.profiles.len()
                {
                    settings
                        .activate_profile(usize::from(current_active))
                        .map_err(|error| error.to_string())?;
                }
                settings.validate().map_err(|error| error.to_string())?;
                self.apply_model_selection(&mut settings)?;
                let port = settings
                    .port
                    .parse::<u16>()
                    .map_err(|_| "UDP port must be 1..65535")?;
                let candidate =
                    self.prepare_udp_source(port, frame_request(&settings, self.active_model_size()))?;
                let renderer =
                    crate::movement::abcurves::ProfileCache::new(&settings.motion_renderer)?;
                {
                    // Keep the source stable from validation through persistence and install.
                    // This uses the same source -> settings lock order as set_udp_port.
                    let mut source = lock_source(&self.source);
                    let mut current = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                    if candidate.is_none()
                        && (source.active_port != port || source.active.is_none())
                    {
                        return Err(
                            "UDP source changed while applying settings; retry Apply Changes."
                                .into(),
                        );
                    }
                    let mut current_renderer = self
                        .motion_renderer
                        .lock()
                        .map_err(|_| "Motion renderer lock poisoned")?;
                    save_settings(&self.settings_path, &settings)
                        .map_err(|error| error.to_string())?;
                    *current_renderer = renderer;
                    *current = settings;
                    if let Some(candidate) = candidate {
                        source.active = Some(candidate);
                        source.active_port = port;
                    }
                    source.pending_port = None;
                    source.last_error = None;
                    if let Ok(mut view) = self.source_view.lock() {
                        *view = SourceView::of(&source);
                    }
                }
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "device_status" => {
                let mut device = parse_device_status(&command)?;
                let settings = self
                    .settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .clone();
                let previous = self
                    .device
                    .lock()
                    .map_err(|_| "device lock poisoned")?
                    .clone();
                if previous["state"] != device["state"] || previous["name"] != device["name"] {
                    self.motion_capture
                        .lock()
                        .map_err(|_| "Capture lock poisoned")?
                        .invalidate();
                }
                let previous_owner = trigger_owner(&settings, &previous);
                let next_owner = next_trigger_owner(&settings, previous_owner, &device);
                device[TriggerOwner::KEY] = json!(next_owner.name());
                device[MOTION_TOGGLED_KEY] =
                    json!(next_motion_toggled(&settings, &previous, &device));
                let previous_auto = previous_owner == TriggerOwner::Auto;
                let next_auto = next_owner == TriggerOwner::Auto;
                let previous_motion = motion_target_active(&settings, &previous)
                    || magnet_active(&settings, &previous);
                let next_motion =
                    motion_target_active(&settings, &device) || magnet_active(&settings, &device);
                // Dynamic FOV keys change the target FOV.
                let fov_changed = effective_detection_settings(&settings, &previous).0
                    != effective_detection_settings(&settings, &device).0
                    || magnet_fov(&settings, &previous) != magnet_fov(&settings, &device);
                let middle_pressed =
                    !trigger_held(&previous, "middle") && trigger_held(&device, "middle");
                *self.device.lock().map_err(|_| "device lock poisoned")? = device;
                if middle_pressed {
                    self.cycle_profile()?;
                }
                if previous_auto != next_auto {
                    self.reconfigure()?;
                } else if previous_owner != next_owner
                    || previous_motion != next_motion
                    || fov_changed
                {
                    // Advance the shared routing revision so queued Motion
                    // deltas from before the physical trigger edge are dropped,
                    // and publish Auto disabled while a Motion hold owns routing.
                    self.reconfigure_pipeline()?;
                }
                Ok(json!({"event":"ack","type":kind}))
            }
            "apply_settings" => {
                let commands = command
                    .get("commands")
                    .and_then(Value::as_array)
                    .filter(|commands| !commands.is_empty() && commands.len() <= 17)
                    .ok_or("settings command batch must contain 1..17 commands")?;
                if commands
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("apply_settings"))
                {
                    return Err("nested settings command batches are not supported".into());
                }
                // Apply every command even when one fails (for example a busy
                // UDP port), so the rest of the user's settings still take effect.
                let failures: Vec<String> = commands
                    .iter()
                    .filter_map(|item| {
                        let command_type = item.get("type").and_then(Value::as_str).unwrap_or("?");
                        self.dispatch(item.clone())
                            .err()
                            .map(|error| format!("{command_type}: {error}"))
                    })
                    .collect();
                if !failures.is_empty() {
                    return Err(failures.join("; ").into());
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
            "activate_profile" => {
                let index = command
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or("profile index is required")?;
                self.activate_profile(index)?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "create_profile" => {
                let name = command.get("name").and_then(Value::as_str);
                let duplicate = command
                    .get("duplicate")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.create_profile(name, duplicate)?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "rename_profile" => {
                let name = command
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("profile name is required")?;
                self.rename_profile(name)?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "delete_profile" => {
                self.delete_profile()?;
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
                // Validate everything first: a rejected command must not
                // leave half of it in the settings for the next reconfigure.
                let fov = command
                    .get("fov_degrees")
                    .and_then(Value::as_u64)
                    .map(|value| {
                        u16::try_from(value)
                            .ok()
                            .filter(|fov| (1..=360).contains(fov))
                            .ok_or("FOV must be 1..360")
                    })
                    .transpose()?;
                let scale = command.get("preview_scale").and_then(Value::as_str);
                if scale.is_some_and(|scale| !["Smooth", "Pixel perfect"].contains(&scale)) {
                    return Err("preview scale must be Smooth or Pixel perfect".into());
                }
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                if let Some(fov) = fov {
                    settings.fov_degrees = fov;
                }
                if !(1..=360).contains(&settings.fov_degrees) {
                    return Err("FOV must be 1..360".into());
                }
                if let Some(scale) = scale {
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
                if !crate::config::TRIGGER_BUTTONS.contains(&value) {
                    return Err("trigger button must be right, m4, or m5".into());
                }
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .trigger_button = value.into();
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_color_filter" => {
                let enabled = command
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or("color filter enabled flag is required")?;
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .color_filter = enabled;
                self.reconfigure()?;
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
            "set_auto" => {
                let auto: crate::config::AutoSettings = serde_json::from_value(
                    command
                        .get("settings")
                        .cloned()
                        .ok_or("Auto settings are required")?,
                )
                .map_err(|_| "invalid Auto settings")?;
                auto.validate()?;
                {
                    // Auto is the Magnet mode "auto".
                    let mut settings =
                        self.settings.lock().map_err(|_| "settings lock poisoned")?;
                    let magnet = &mut settings.magnet;
                    if auto.enabled {
                        magnet.enabled = true;
                        magnet.mode = "auto".into();
                    } else if magnet.mode == "auto" {
                        magnet.enabled = false;
                    }
                    magnet.trigger_button = auto.trigger_button;
                    magnet.fov_degrees = auto.fov_degrees;
                    magnet.delay_ms = auto.delay_ms;
                    magnet.cooldown_ms = auto.cooldown_ms;
                    magnet.sleep_ms = auto.sleep_ms;
                    settings.auto = settings.magnet.auto();
                }
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_movement_calibration" => {
                // In-game sensitivity only: DPI does not change injected counts.
                let sens = finite_number(command.get("sens"), "sens must be 0.01..10")?;
                if !(0.01..=10.0).contains(&sens) {
                    return Err("sens must be 0.01..10".into());
                }
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.sens = sens;
                if let Some(enabled) = command.get("motion_enabled").and_then(Value::as_bool) {
                    settings.motion_enabled = enabled;
                }
                if let Some(enabled) = command.get("motion_always_on").and_then(Value::as_bool) {
                    settings.motion_always_on = enabled;
                }
                if let Some(enabled) = command.get("motion_toggle").and_then(Value::as_bool) {
                    settings.motion_toggle = enabled;
                }
                drop(settings);
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "motion_capture_progress" => {
                let id = command
                    .get("capture_id")
                    .and_then(Value::as_str)
                    .ok_or("Capture id is required")?;
                let count = command
                    .get("count")
                    .and_then(Value::as_u64)
                    .filter(|count| *count <= 256)
                    .ok_or("Invalid capture progress")? as usize;
                let settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                let mut capture = self
                    .motion_capture
                    .lock()
                    .map_err(|_| "Capture lock poisoned")?;
                if capture.matches(id, &settings) {
                    capture.count = count;
                    capture.message = if count == 256 {
                        "Saving mouse profile...".into()
                    } else if command["status"] == "restart" {
                        "Capture restarted: reports must be consecutive at 1,000 Hz. Keep moving the physical mouse; Normal remains active.".into()
                    } else if command["status"] == "sdk_error" {
                        "Mouse capture subscription failed. Normal remains active; reconnect the device to retry.".into()
                    } else {
                        format!("Collecting mouse profile: {count}/256. Move continuously at 1,000 Hz; Normal remains active.")
                    };
                }
                Ok(json!({"event":"ack","type":kind}))
            }
            "complete_motion_capture" => self.complete_motion_capture(&command),
            "set_motion_renderer" => {
                let renderer: crate::config::MotionRendererSettings = serde_json::from_value(
                    command
                        .get("settings")
                        .cloned()
                        .ok_or("Motion renderer settings are required")?,
                )
                .map_err(|_| "Invalid Motion renderer settings")?;
                renderer.validate()?;
                // Prepare before changing live settings. A rejected profile leaves
                // the current output path and settings intact.
                let prepared = crate::movement::abcurves::ProfileCache::new(&renderer)?;
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                let mut current = self
                    .motion_renderer
                    .lock()
                    .map_err(|_| "Motion renderer lock poisoned")?;
                *current = prepared;
                settings.motion_renderer = renderer;
                drop(current);
                drop(settings);
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_motion_config" => {
                const INVALID: &str = "booster speeds must be 0..10";
                let speed = |key: &str| {
                    finite_number(command.get(key), INVALID).and_then(|v| {
                        (0.0..=10.0)
                            .contains(&v)
                            .then_some(v)
                            .ok_or_else(|| INVALID.to_string())
                    })
                };
                let (near, mid, far) = (
                    speed("speed_near")?,
                    speed("speed_mid")?,
                    speed("speed_far")?,
                );
                let flag = |key: &str| {
                    command
                        .get(key)
                        .and_then(Value::as_bool)
                        .ok_or_else(|| format!("{key} must be true or false"))
                };
                let (booster, inside) = (flag("speed_booster")?, flag("continue_inside_box")?);
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.speed_booster = booster;
                settings.speed_near = near;
                settings.speed_mid = mid;
                settings.speed_far = far;
                settings.continue_inside_box = inside;
                drop(settings);
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_smoothing_config" => {
                const INVALID: &str = "smoothing must be 1..50 with min <= max";
                let value = |key: &str| finite_number(command.get(key), INVALID).map(|v| v as f32);
                let (x_min, x_max) = (value("x_min")?, value("x_max")?);
                let (y_min, y_max) = (value("y_min")?, value("y_max")?);
                if SmoothingRange::new(x_min, x_max).is_none()
                    || SmoothingRange::new(y_min, y_max).is_none()
                {
                    return Err(INVALID.into());
                }
                let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
                settings.smoothing_x_min = x_min;
                settings.smoothing_x_max = x_max;
                settings.smoothing_y_min = y_min;
                settings.smoothing_y_max = y_max;
                drop(settings);
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_crop_resolution" => {
                let resolution = command
                    .get("resolution")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| (1..=sysx_ui::protocol::MAX_CROP_RESOLUTION).contains(v))
                    .ok_or("crop resolution exceeds the receiver's frame limit")?;
                self.settings
                    .lock()
                    .map_err(|_| "settings lock poisoned")?
                    .crop_resolution = resolution;
                self.reconfigure()?;
                Ok(json!({"event":"ack","type":kind,"state":self.state()}))
            }
            "set_strategy" => {
                let mode = command
                    .get("mode")
                    .and_then(Value::as_str)
                    .ok_or("detection strategy is required")?;
                if mode == "object_detection" {
                    let registry = self.registry.lock().map_err(|_| "registry lock poisoned")?;
                    if registry
                        .active_model_id
                        .as_ref()
                        .and_then(|id| registry.models.get(id))
                        .is_none()
                    {
                        return Err("This build has no built-in Object Detection model".into());
                    }
                } else if mode != "pixel_seek" {
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
            _ => Err(format!("unsupported command type: {kind}")),
        }
    }

    fn cycle_profile(&self) -> Result<(), String> {
        let next = {
            let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
            settings.normalize_profiles();
            (usize::from(settings.active_profile) + 1) % settings.profiles.len()
        };
        self.activate_profile(next)
    }

    fn activate_profile(&self, index: usize) -> Result<(), String> {
        let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        settings
            .activate_profile(index)
            .map_err(|error| error.to_string())?;
        let port = settings
            .port
            .parse::<u16>()
            .map_err(|_| "profile UDP port is invalid".to_string())?;
        save_settings(&self.settings_path, &settings).map_err(|error| error.to_string())?;
        drop(settings);
        // Keep the current listener if the profile's port is temporarily
        // occupied; the source retry loop will adopt it when it is free.
        let _ = self.set_udp_port(port);
        self.reconfigure()
    }

    fn create_profile(&self, name: Option<&str>, duplicate: bool) -> Result<(), String> {
        let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        settings
            .create_profile(name, duplicate)
            .map_err(|error| error.to_string())?;
        let port = settings
            .port
            .parse::<u16>()
            .map_err(|_| "profile UDP port is invalid")?;
        save_settings(&self.settings_path, &settings).map_err(|error| error.to_string())?;
        drop(settings);
        let _ = self.set_udp_port(port);
        self.reconfigure()
    }

    fn rename_profile(&self, name: &str) -> Result<(), String> {
        let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        let index = usize::from(settings.active_profile);
        settings
            .rename_profile(index, name)
            .map_err(|error| error.to_string())?;
        save_settings(&self.settings_path, &settings).map_err(|error| error.to_string())
    }

    fn delete_profile(&self) -> Result<(), String> {
        let mut settings = self.settings.lock().map_err(|_| "settings lock poisoned")?;
        let index = usize::from(settings.active_profile);
        settings
            .delete_profile(index)
            .map_err(|error| error.to_string())?;
        let port = settings
            .port
            .parse::<u16>()
            .map_err(|_| "profile UDP port is invalid")?;
        save_settings(&self.settings_path, &settings).map_err(|error| error.to_string())?;
        drop(settings);
        let _ = self.set_udp_port(port);
        self.reconfigure()
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

    /// Makes the draft's model the active one, applies its Min confidence
    /// and, in Object Detection, sets Crop Resolution to the model's input
    /// size so the sender delivers frames the model accepts.
    fn apply_model_selection(&self, settings: &mut AppSettings) -> Result<(), String> {
        let builtin = self
            .builtin
            .lock()
            .map_err(|_| "model catalog lock poisoned")?
            .clone();
        let model = settings
            .active_model_id
            .clone()
            .filter(|id| builtin.iter().any(|model| model.id == *id))
            .or_else(|| builtin.first().map(|model| model.id.clone()));
        if settings.strategy == "object_detection" && model.is_none() {
            return Err("This build has no built-in Object Detection model".into());
        }
        let Some(model) = model else {
            return Ok(());
        };
        settings.active_model_id = Some(model.clone());
        // Runtime + Compute pick the variant; in Object Detection a missing
        // one is an error instead of a silent substitute.
        let resolved = crate::builtin_models::resolve(
            &builtin,
            &model,
            &settings.model_precision,
            &settings.model_backend,
        );
        let id = match resolved {
            Ok(id) => id,
            Err(error) if settings.strategy == "object_detection" => return Err(error),
            Err(_) => return Ok(()),
        };
        let mut registry = self.registry.lock().map_err(|_| "registry lock poisoned")?;
        if registry.active_model_id.as_deref() != Some(id.as_str()) {
            registry
                .activate(Some(&id))
                .map_err(|error| error.to_string())?;
        }
        let confidence = settings
            .model_confidence
            .parse::<f32>()
            .map_err(|_| "Min confidence must be from 0 to 1")?;
        let mut model = registry.models[&id].clone();
        if settings.strategy == "object_detection" {
            if model.input_width != model.input_height {
                return Err(format!(
                    "Model input {}x{} is not square; the sender only crops squares",
                    model.input_width, model.input_height
                ));
            }
        }
        if model.confidence != confidence {
            model.confidence = confidence;
            registry.update(model).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Input side of the active model (Object Detection's capture size).
    fn active_model_size(&self) -> Option<u32> {
        let registry = self.registry.lock().ok()?;
        let id = registry.active_model_id.as_ref()?;
        registry.models.get(id).map(|model| model.input_width)
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
        let mut source = lock_source(&self.source);
        if let Some(active) = source.active.as_mut() {
            active.close();
        }
        drop(source);
        if !self.pipeline.close(Duration::from_secs(3)) {
            self.record_error("frame pipeline did not stop within three seconds".into());
        }
        let _ = self.movement_ticker.close(Duration::from_secs(1));
        let _ = self.movement_batcher.close(Duration::from_secs(1));
    }
}

fn receiver_supervisor(service: Weak<ReceiverService>) {
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            receiver_loop(service.clone())
        }));
        let Err(panic) = result else { return };
        let Some(receiver) = service.upgrade() else {
            return;
        };
        if receiver.stop.load(Ordering::Acquire) {
            return;
        }
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        let message = format!("UDP receive worker recovered from panic: {detail}");
        {
            let mut source = lock_source(&receiver.source);
            reset_source_after_failure(&mut source, Instant::now(), message.clone());
            if let Ok(mut view) = receiver.source_view.lock() {
                *view = SourceView::of(&source);
            }
        }
        receiver.record_error(message);
        drop(receiver);
        thread::sleep(Duration::from_millis(250));
    }
}

fn lock_source(source: &Mutex<SourceState>) -> MutexGuard<'_, SourceState> {
    match source.lock() {
        Ok(guard) => guard,
        Err(error) => {
            let guard = error.into_inner();
            source.clear_poison();
            guard
        }
    }
}

fn reset_source_after_failure(source: &mut SourceState, now: Instant, message: String) {
    source.active = None;
    source.pending_port = Some(source.active_port);
    source.retry_at = now + UDP_RETRY;
    source.last_error = Some(message);
}

fn receiver_loop(service: Weak<ReceiverService>) {
    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        if service.stop.load(Ordering::Acquire) {
            return;
        }
        let retry = {
            let state = lock_source(&service.source);
            state
                .pending_port
                .filter(|_| Instant::now() >= state.retry_at)
        };
        if let Some(port) = retry {
            let model_size = service.active_model_size();
            let request = service
                .settings
                .lock()
                .ok()
                .map(|settings| frame_request(&settings, model_size))
                .unwrap_or_default();
            let candidate = UdpDecodedFrameSource::new("0.0.0.0", port).and_then(|mut source| {
                source.set_crop_resolution(request);
                source.open()?;
                Ok(source)
            });
            match candidate {
                Ok(candidate) => {
                    let mut state = lock_source(&service.source);
                    if state.pending_port == Some(port) && !service.stop.load(Ordering::Acquire) {
                        state.active = Some(candidate);
                        state.active_port = port;
                        state.pending_port = None;
                        state.last_error = None;
                        if let Ok(mut view) = service.source_view.lock() {
                            *view = SourceView::of(&state);
                        }
                        if let Ok(mut settings) = service.settings.lock() {
                            settings.port = port.to_string();
                        }
                    }
                }
                Err(error) => {
                    let mut state = lock_source(&service.source);
                    state.last_error = Some(error.to_string());
                    state.retry_at = Instant::now() + UDP_RETRY;
                }
            }
        }
        let read = {
            let mut state = lock_source(&service.source);
            let result = if let Some(source) = state.active.as_mut() {
                catch_receiver_panic(|| source.read_encoded(Duration::from_millis(100)))
            } else {
                Ok(None)
            };
            // Readers of state() use this snapshot instead of waiting on the
            // source lock, which is held for the whole blocking read above.
            if let Ok(mut view) = service.source_view.lock() {
                *view = SourceView::of(&state);
            }
            result
        };
        match read {
            Ok(Some(frame)) => service.decoder.submit(frame, Instant::now()),
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let message = error.to_string();
                let mut source = lock_source(&service.source);
                reset_source_after_failure(&mut source, Instant::now(), message.clone());
                if let Ok(mut view) = service.source_view.lock() {
                    *view = SourceView::of(&source);
                }
                service.record_error(message);
            }
        }
    }
}

fn catch_receiver_panic<T>(
    run: impl FnOnce() -> Result<T, crate::receiver::ReceiverError>,
) -> Result<T, crate::receiver::ReceiverError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        Err(crate::receiver::ReceiverError::Io(std::io::Error::other(
            format!("UDP receive worker recovered from panic: {message}"),
        )))
    })
}

#[derive(Clone)]
struct WebSocketState {
    service: Arc<ReceiverService>,
    token: String,
}

#[derive(serde::Deserialize)]
struct WsQuery {
    token: String,
    /// `monitor` receives preview frames; `host` is the native app client.
    #[serde(default)]
    role: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientRole {
    Monitor,
    Host,
}

impl ClientRole {
    fn parse(role: Option<&str>) -> Option<Self> {
        match role {
            Some("monitor") => Some(Self::Monitor),
            Some("host") => Some(Self::Host),
            _ => None,
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
    let allowed_origin = origin_allowed(origin);
    if !token_ok || !allowed_origin {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(role) = ClientRole::parse(query.role.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "unsupported client role").into_response();
    };
    ws.max_message_size(MAX_SOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_SOCKET_MESSAGE_BYTES)
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

fn origin_allowed(origin: Option<&str>) -> bool {
    origin.is_none_or(|origin| {
        [
            "tauri://localhost",
            "http://tauri.localhost",
            "http://127.0.0.1:1420",
        ]
        .contains(&origin)
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
                // Only a closed or failed socket ends the loop: for the host
                // that exits the backend, so a ping or other frame must not.
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => continue,
                    _ => break,
                };
                if text.len() > MAX_MESSAGE_BYTES {
                    let event = oversized_command_error(&text);
                    service.record_error("commands must be under 64 KiB".into());
                    if send_text(&mut socket, event).await.is_err() { break; }
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
                    Ok(command) if matches!(command.get("type").and_then(Value::as_str),
                        Some("complete_motion_capture" | "motion_capture_progress")) && role != ClientRole::Host => {
                        Err("Physical capture commands require the native host connection".into())
                    }
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
            // Only the native monitor receives Preview; the host receives state only.
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
            // Settings are applied by the authenticated native host client.
            // Keep connected receiver clients in sync even while the frame
            // stream is busy sending binary preview data.
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

/// What the OBS plugin is asked for: the Crop Resolution as JPEG, or in
/// Pixel Seek with Plugin mask on, the centered mask of the selected color.
/// The square the OBS plugin crops: the model's input size in Object
/// Detection, Crop Resolution in Pixel Seek (user request: Crop Resolution
/// applies to Pixel Seek only).
fn capture_size(settings: &AppSettings, model_size: Option<u32>) -> u32 {
    match model_size {
        Some(size) if settings.strategy == "object_detection" => size,
        _ => settings.crop_resolution,
    }
}

/// What SYSCFG asks the plugin to crop.
fn frame_request(settings: &AppSettings, model_size: Option<u32>) -> u32 {
    capture_size(settings, model_size)
}

/// Same one-second window that `state()` uses for `streaming` vs `waiting`.
fn preview_is_fresh(published_ns: u64, now_ns: u64) -> bool {
    now_ns.saturating_sub(published_ns) < 1_000_000_000
}

/// What the movement ticker runs with. While Auto owns the triggers Motion
/// is off, Always-on included.
fn movement_runtime(
    settings: &AppSettings,
    device: &Value,
    renderer_profile: Option<Arc<crate::movement::abcurves::Profile>>,
    config_revision: u64,
) -> MovementRuntimeSettings {
    let auto_active = trigger_owner(settings, device) == TriggerOwner::Auto;
    MovementRuntimeSettings {
        renderer_profile,
        config_revision,
        motion_enabled: settings.motion_enabled && !auto_active,
        motion_always_on: motion_latched(settings, device),
        motion_toggle: settings.motion_toggle,
        trigger_held: trigger_held(device, &settings.trigger_button),
        sens: settings.sens,
        offset_x: settings.offset_x,
        offset_y: settings.offset_y,
        speed_x: settings.motion_speed_x,
        speed_y: settings.motion_speed_y,
        smoothing_x: settings.smoothing_x(),
        smoothing_y: settings.smoothing_y(),
        smooth_fov_degrees: settings.smooth_fov_degrees,
        booster: settings.speed_booster(),
        no_deadzone: settings.continue_inside_box,
        trigger_button: settings.trigger_button.clone(),
        recoil: settings.recoil.clone(),
    }
}

/// Which feature's held trigger owns routing (user request, 2026-10-25):
/// while the Motion trigger is held every Auto command is cut, and while the
/// Auto trigger is held Motion (Always-on included) is cut. With both held the
/// one pressed first keeps it; pressed together (or the same button), Auto.
/// With neither held Motion runs only through Always-on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TriggerOwner {
    #[default]
    None,
    Motion,
    Auto,
}

impl TriggerOwner {
    const KEY: &'static str = "trigger_owner";

    /// The owner stored with the device status by the previous update.
    fn stored(device: &Value) -> Self {
        match device[Self::KEY].as_str() {
            Some("motion") => Self::Motion,
            Some("auto") => Self::Auto,
            _ => Self::None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Motion => "motion",
            Self::Auto => "auto",
        }
    }
}

fn next_trigger_owner(
    settings: &AppSettings,
    previous: TriggerOwner,
    device: &Value,
) -> TriggerOwner {
    // In toggle mode the Motion button is a switch, not a hold.
    let motion = settings.motion_enabled
        && !settings.motion_toggle
        && trigger_held(device, &settings.trigger_button);
    let auto = settings.auto.enabled && trigger_held(device, &settings.auto.trigger_button);
    match (motion, auto) {
        (true, true) if previous == TriggerOwner::Motion => TriggerOwner::Motion,
        (_, true) => TriggerOwner::Auto,
        (true, false) => TriggerOwner::Motion,
        (false, false) => TriggerOwner::None,
    }
}

/// Current owner; also correct after a settings change during a hold.
fn trigger_owner(settings: &AppSettings, device: &Value) -> TriggerOwner {
    next_trigger_owner(settings, TriggerOwner::stored(device), device)
}

/// Magnet runs while its own trigger is held.
fn magnet_active(settings: &AppSettings, device: &Value) -> bool {
    settings.magnet.enabled
        && settings.magnet.mode != "auto"
        && trigger_held(device, &settings.magnet.trigger_button)
}

/// One Magnet sample per processed frame for the host: the target error in
/// crop px (offsets applied) and the px → count conversion. Sent only while
/// Magnet is enabled; the host treats a sample older than 100 ms as no target.
fn magnet_event(settings: &AppSettings, processed: &ProcessedFrame, revision: u64) -> Value {
    let error = processed
        .snap_target
        .map(|target| {
            let (x, y) = crate::movement::crop_target_error(
                (f64::from(target[0]), f64::from(target[1])),
                (processed.crop_x, processed.crop_y),
                (
                    processed.result.processed_width,
                    processed.result.processed_height,
                ),
                (settings.magnet.offset_x, settings.magnet.offset_y),
            );
            [x, y]
        });
    json!({
        "event": "magnet_target",
        "config_revision": revision,
        "frame_id": processed.frame.frame_id,
        "error": error,
        "counts_per_px": crate::movement::motion::counts_per_px(settings.sens),
        "timestamp_ms": unix_ms(),
        "settings": settings.magnet,
    })
}

fn motion_target_active(settings: &AppSettings, device: &Value) -> bool {
    settings.motion_enabled
        && match trigger_owner(settings, device) {
            TriggerOwner::Motion => true,
            TriggerOwner::Auto => false,
            TriggerOwner::None => motion_latched(settings, device),
        }
}

/// Toggle mode (user request, 2026-10-25): each press of the Motion trigger
/// button turns Motion on or off. The state is kept with the device status;
/// it is off after a disconnect and while Motion or toggle mode is off.
const MOTION_TOGGLED_KEY: &str = "motion_toggled";

fn motion_toggled(settings: &AppSettings, device: &Value) -> bool {
    settings.motion_enabled && settings.motion_toggle && device[MOTION_TOGGLED_KEY] == true
}

fn next_motion_toggled(settings: &AppSettings, previous: &Value, device: &Value) -> bool {
    let pressed = !trigger_held(previous, &settings.trigger_button)
        && trigger_held(device, &settings.trigger_button);
    let flip = settings.motion_enabled && settings.motion_toggle && pressed;
    device["state"] == "connected" && (motion_toggled(settings, previous) != flip)
}

/// Motion runs without a held trigger: Always-on, or toggled on.
fn motion_latched(settings: &AppSettings, device: &Value) -> bool {
    settings.motion_always_on || motion_toggled(settings, device)
}

// A held Motion trigger publishes Auto disabled so the host cancels a delayed
// click and the cooldown at once, without waiting for another video frame.
fn routed_auto_settings(settings: &AppSettings, device: &Value) -> AutoSettings {
    let mut auto = settings.auto.clone();
    auto.enabled &= trigger_owner(settings, device) != TriggerOwner::Motion;
    auto
}

/// `base`, or `dynamic` while the Dynamic FOV key is held.
fn dynamic_fov(base: u16, dynamic: u16, key: &str, device: &Value) -> u16 {
    if key != "off" && trigger_held(device, key) {
        dynamic
    } else {
        base
    }
}

fn motion_fov(settings: &AppSettings, device: &Value) -> u16 {
    dynamic_fov(
        settings.fov_degrees,
        settings.dynamic_fov_degrees,
        &settings.dynamic_fov_key,
        device,
    )
}

fn magnet_fov(settings: &AppSettings, device: &Value) -> u16 {
    let magnet = &settings.magnet;
    dynamic_fov(
        magnet.fov_degrees,
        magnet.dynamic_fov_degrees,
        &magnet.dynamic_fov_key,
        device,
    )
}

/// Target FOV, color and whether Auto owns this frame. A held Magnet
/// trigger selects targets inside Magnet's FOV.
fn effective_detection_settings(settings: &AppSettings, device: &Value) -> (u16, String, bool) {
    if trigger_owner(settings, device) == TriggerOwner::Auto {
        (
            magnet_fov(settings, device),
            settings.pixel_color.clone(),
            true,
        )
    } else if magnet_active(settings, device) {
        (
            magnet_fov(settings, device),
            settings.pixel_color.clone(),
            false,
        )
    } else {
        (
            motion_fov(settings, device),
            settings.pixel_color.clone(),
            false,
        )
    }
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

#[derive(serde::Deserialize)]
struct CommandHeader {
    #[serde(rename = "type")]
    kind: Option<Value>,
    request_id: Option<Value>,
}

/// The error for a command too large to run, with its type and request id
/// so the client that sent it is told it failed.
fn oversized_command_error(text: &str) -> Value {
    let header = serde_json::from_str::<CommandHeader>(text).ok();
    let (kind, request_id) = header.map_or((None, None), |header| (header.kind, header.request_id));
    let mut event = json!({
        "event":"error",
        "type":kind,
        "message":"commands must be under 64 KiB"
    });
    attach_request_id(&mut event, request_id);
    event
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
    // Per frame and per monitor: read only the two counters needed instead of
    // building the whole state (settings and profile clones, registry lock).
    let transport = service.source_view_now().transport;
    // Color filter: the mask applied before processing, one bit per pixel,
    // sent between the metadata and the original JPEG; Preview blacks out the
    // same pixels, so it shows exactly the image Motion/Auto processed.
    let color_mask = processed.mask.as_ref().map(|bits| {
        json!({"width":processed.result.processed_width, "height":processed.result.processed_height, "bytes":bits.len()})
    });
    let metadata = json!({
        "event":"frame", "frame_id":processed.frame.frame_id, "source":processed.frame.source,
        "source_width":processed.result.source_width, "source_height":processed.result.source_height,
        "crop_x":processed.crop_x, "crop_y":processed.crop_y, "mode":processed.result.mode,
        "model_id":processed.result.model_id, "processing_fps":stats.processing_fps, "source_fps":service.source_fps(),
        "dropped_input_frames":stats.dropped_input_frames, "rejected_frames":stats.rejected_frames,
        "resolution_mismatches":stats.resolution_mismatches, "transport_dropped_frames":transport.dropped_frames + transport.expired_frames + service.decoder.stats().superseded,
        "detection":processed.result, "color_mask":color_mask,
    });
    let bytes = serde_json::to_vec(&metadata).map_err(|error| error.to_string())?;
    let size = u32::try_from(bytes.len()).map_err(|_| "frame metadata too large")?;
    let mask = processed.mask.as_deref().unwrap_or_default();
    let mut output = Vec::with_capacity(4 + bytes.len() + mask.len() + jpeg.len());
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(&bytes);
    output.extend_from_slice(&mask);
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
    udp_port: u16,
) -> Result<(), String> {
    if !["127.0.0.1", "localhost", "::1"].contains(&host.as_str()) {
        return Err("desktop IPC server must bind to loopback only".into());
    }
    let service = ReceiverService::new(token.clone(), data_dir, udp_port)?;
    let listener = TcpListener::bind((host.as_str(), ws_port))
        .await
        .map_err(|error| error.to_string())?;
    let actual_port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let web_state = WebSocketState {
        service: Arc::clone(&service),
        token,
    };
    let app = Router::new()
        .route("/receiver", get(websocket_route))
        .with_state(web_state);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut ws_task: TokioJoinHandle<Result<(), std::io::Error>> = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    if let Ok(line) = serde_json::to_string(&json!({"event":"ready","port":actual_port})) {
        emit_stdout_line(&line);
    }
    let server_result = tokio::select! {
        _ = wait_for_shutdown() => {
            let _ = shutdown_tx.send(());
            ws_task.await.map_err(|error| error.to_string())?
        }
        result = &mut ws_task => result.map_err(|error| error.to_string())?,
    };
    service.close();
    server_result.map_err(|error| error.to_string())
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

    #[test]
    fn udp_worker_panics_become_recoverable_receiver_errors() {
        let error =
            super::catch_receiver_panic(|| -> Result<(), crate::receiver::ReceiverError> {
                panic!("simulated UDP receiver panic");
            })
            .unwrap_err();
        assert!(error.to_string().contains("UDP receive worker recovered"));
        assert!(error.to_string().contains("simulated UDP receiver panic"));
    }

    #[test]
    fn receiver_source_recovers_after_a_poisoned_lock_and_retries_its_port() {
        let now = Instant::now();
        let source = Mutex::new(SourceState {
            active: None,
            active_port: 4200,
            pending_port: None,
            retry_at: now,
            last_error: None,
        });
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = source.lock().unwrap();
            panic!("simulate receiver panic while source state is locked");
        }));

        let mut state = lock_source(&source);
        assert!(!source.is_poisoned());
        reset_source_after_failure(&mut state, now, "worker panic".into());
        assert_eq!(state.pending_port, Some(4200));
        assert_eq!(state.retry_at, now + UDP_RETRY);
        assert_eq!(state.last_error.as_deref(), Some("worker panic"));
    }

    fn arm_physical_capture(service: &ReceiverService) -> String {
        {
            let mut settings = service.settings.lock().unwrap();
            settings.motion_renderer.enabled = true;
            settings.motion_renderer.reports.clear();
            save_settings(&service.settings_path, &settings).unwrap();
        }
        service
            .dispatch(
                json!({"version":1,"type":"device_status","state":"connected",
            "name":"Mock Medius","buttons":[false,false,false,false,false]}),
            )
            .unwrap();
        service.reconfigure().unwrap();
        service.state()["motion_capture"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn captured_command(id: &str) -> Value {
        // Synthetic capture only for validation/persistence tests.
        json!({"version":1,"type":"complete_motion_capture","capture":{
            "capture_id":id,"reports":(0..256).map(|i|[(i%9) as i16-4,(i%5) as i16-2]).collect::<Vec<_>>(),
            "timestamps_us":(0..256).map(|i|1000+i*1000).collect::<Vec<u32>>()
        }})
    }
    #[test]
    fn physical_capture_saves_and_activates_without_resetting_auto_or_needing_frames() {
        let (service, dir) = test_service("physical-profile-save");
        let guard = ServiceGuard(Arc::clone(&service));
        let id = arm_physical_capture(&service);
        assert!(
            service.motion_renderer.lock().unwrap().profile.is_none(),
            "Normal while waiting"
        );
        let revision = service.auto_events.lock().unwrap().revision;
        service.dispatch(captured_command(&id)).unwrap();
        assert_eq!(
            service.auto_events.lock().unwrap().revision,
            revision,
            "capture must not cancel pending Auto"
        );
        assert!(service.motion_renderer.lock().unwrap().profile.is_some());
        assert!(service.state()["motion_capture"].is_null());
        let saved = load_settings(&service.settings_path);
        assert!(saved.motion_renderer.enabled);
        assert_eq!(saved.motion_renderer.reports.len(), 256);
        assert!(
            crate::movement::abcurves::ProfileCache::new(&saved.motion_renderer)
                .unwrap()
                .profile
                .is_some()
        );
        assert_eq!(service.pipeline.stats().processed_frames, 0);
        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn stale_invalid_cancelled_and_changed_profile_captures_do_not_write() {
        let (service, dir) = test_service("physical-profile-cancel");
        let guard = ServiceGuard(Arc::clone(&service));
        let old = arm_physical_capture(&service);
        let original = std::fs::read(&service.settings_path).unwrap();
        let mut invalid = captured_command(&old);
        invalid["capture"]["timestamps_us"][120] = json!(1);
        assert!(service.dispatch(invalid).is_err());
        service.reconfigure().unwrap();
        let current = service.state()["motion_capture"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(old, current);
        assert!(service.dispatch(captured_command(&old)).is_err());
        service.create_profile(Some("Other"), false).unwrap();
        let after_switch = std::fs::read(&service.settings_path).unwrap();
        assert!(service.dispatch(captured_command(&current)).is_err());
        assert_eq!(std::fs::read(&service.settings_path).unwrap(), after_switch);
        assert_ne!(original, after_switch);
        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn capture_never_overwrites_a_newer_http_save_or_activates_on_write_error() {
        let (service, dir) = test_service("physical-profile-newer-save");
        let guard = ServiceGuard(Arc::clone(&service));
        let id = arm_physical_capture(&service);
        let mut newer = load_settings(&service.settings_path);
        newer.offset_x = 13.0;
        save_settings(&service.settings_path, &newer).unwrap();
        let bytes = std::fs::read(&service.settings_path).unwrap();
        assert!(service.dispatch(captured_command(&id)).is_err());
        assert_eq!(std::fs::read(&service.settings_path).unwrap(), bytes);
        assert!(service.motion_renderer.lock().unwrap().profile.is_none());
        // Simulate a filesystem failure after restoring matching settings.
        save_settings(&service.settings_path, &service.settings.lock().unwrap()).unwrap();
        let original = std::fs::read(&service.settings_path).unwrap();
        let blocked_temp = service.settings_path.parent().unwrap().join(format!(
            ".{}.{}.tmp",
            service.settings_path.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        std::fs::create_dir(&blocked_temp).unwrap();
        let error = service.dispatch(captured_command(&id)).unwrap_err();
        assert!(
            error.contains("exist") || error.to_ascii_lowercase().contains("access is denied"),
            "expected atomic-write failure: {error}"
        );
        assert_eq!(std::fs::read(&service.settings_path).unwrap(), original);
        assert!(service.motion_renderer.lock().unwrap().profile.is_none());
        drop(guard);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn motion_renderer_profile_is_opt_in_validated_and_saved_per_profile() {
        let (service, data_dir) = test_service("motion-renderer-settings");
        let guard = ServiceGuard(Arc::clone(&service));
        assert!(service.motion_renderer.lock().unwrap().profile.is_none());
        assert!(!service.settings.lock().unwrap().motion_renderer.enabled);
        let bad = service.dispatch(json!({"version":1,"type":"set_motion_renderer",
            "settings":{"enabled":true,"reports":[[1,2]]}}));
        assert!(bad.is_err());
        assert!(service.motion_renderer.lock().unwrap().profile.is_none());
        // Synthetic profile for API/persistence testing only, never a shipped preset.
        let reports = (0..256)
            .map(|i| [(i % 9) as i16 - 4, (i % 5) as i16 - 2])
            .collect::<Vec<_>>();
        service
            .dispatch(json!({"version":1,"type":"apply_settings","commands":[{
            "version":1,"type":"set_motion_renderer","settings":{
                "enabled":true,"sampleIntervalUs":1000,"reports":reports
            }}]}))
            .unwrap();
        assert!(service.motion_renderer.lock().unwrap().profile.is_some());
        // Runtime apply is separate from durable settings persistence.
        save_settings(
            data_dir.join("app-settings.json"),
            &service.settings.lock().unwrap(),
        )
        .unwrap();
        let saved = load_settings(data_dir.join("app-settings.json"));
        assert!(saved.motion_renderer.enabled);
        assert_eq!(saved.motion_renderer.reports, reports);
        let snapshot: AppSettings =
            serde_json::from_value(saved.profiles[0].settings.clone()).unwrap();
        assert_eq!(snapshot.motion_renderer, saved.motion_renderer);
        // Auto trigger/reconfigure must reuse the prepared immutable profile.
        let before = service
            .motion_renderer
            .lock()
            .unwrap()
            .profile
            .clone()
            .unwrap();
        let mut output = crate::movement::abcurves::MotionOutput::default();
        assert!(output
            .filter(Some(&before), (i64::MAX, 0), Instant::now())
            .is_err());
        assert!(service.state()["last_error"]
            .as_str()
            .unwrap()
            .contains("ABCurves output stopped"));
        output.reset();
        service.reconfigure().unwrap();
        assert!(Arc::ptr_eq(
            &before,
            service
                .motion_renderer
                .lock()
                .unwrap()
                .profile
                .as_ref()
                .unwrap()
        ));
        service
            .dispatch(json!({"version":1,"type":"set_motion_renderer",
            "settings":{"enabled":false,"sampleIntervalUs":1000,"reports":reports}}))
            .unwrap();
        assert!(service.motion_renderer.lock().unwrap().profile.is_none());
        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn trigger_route_changes_invalidate_callbacks_even_with_unchanged_auto_settings() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let settings = AutoSettings {
            enabled: true,
            ..AutoSettings::default()
        };
        let mut events = AutoEvents {
            settings: settings.clone(),
            revision: 0,
            emit: Arc::new(move |event| captured.lock().unwrap().push(event)),
        };
        events.reconfigure(&settings); // release
        events.reconfigure(&settings); // press
        events.target(0, true); // delayed callback from the earlier hold
        assert_eq!(output.lock().unwrap().len(), 2);
        events.target(2, true);
        let output = output.lock().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[2]["config_revision"], 2);
    }

    #[test]
    fn a_held_motion_trigger_publishes_auto_disabled_without_waiting_for_video() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let mut settings = AppSettings::default();
        settings.auto.enabled = true;
        settings.auto.trigger_button = "m4".into();
        settings.motion_enabled = true;
        settings.motion_always_on = true;
        let mut events = AutoEvents {
            settings: settings.auto.clone(),
            revision: 0,
            emit: Arc::new(move |event| captured.lock().unwrap().push(event)),
        };
        let released = json!({"state":"connected","buttons":[false,false,false,false,false]});
        assert!(
            routed_auto_settings(&settings, &released).enabled,
            "Always-on no longer disables Auto"
        );
        let motion_held = json!({"state":"connected","buttons":[false,true,false,false,false]});
        events.reconfigure(&routed_auto_settings(&settings, &motion_held));
        let output = output.lock().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["event"], "auto_config");
        assert_eq!(output[0]["enabled"], false);
        assert_eq!(output[0]["timestamp_ms"], 0);
    }

    #[test]
    fn the_trigger_pressed_first_owns_routing_until_released() {
        use super::{next_trigger_owner, TriggerOwner};
        let mut settings = AppSettings::default();
        settings.motion_enabled = true;
        settings.auto.enabled = true;
        settings.auto.trigger_button = "m4".into();
        let buttons = |motion: bool, auto: bool| json!({"state":"connected","buttons":[false,motion,false,auto,false]});
        let mut owner = TriggerOwner::None;
        for (motion, auto, expected) in [
            (true, false, TriggerOwner::Motion),
            (true, true, TriggerOwner::Motion), // Auto pressed while Motion held
            (false, true, TriggerOwner::Auto),  // Motion released
            (true, true, TriggerOwner::Auto),   // Motion pressed while Auto held
            (true, false, TriggerOwner::Motion),
            (false, false, TriggerOwner::None),
            (true, true, TriggerOwner::Auto), // pressed together
        ] {
            owner = next_trigger_owner(&settings, owner, &buttons(motion, auto));
            assert_eq!(owner, expected, "motion={motion} auto={auto}");
        }
        // The same button for both features: Auto owns it.
        settings.auto.trigger_button = "right".into();
        assert_eq!(
            next_trigger_owner(&settings, TriggerOwner::None, &buttons(true, false)),
            TriggerOwner::Auto
        );
        // A disabled feature never owns its held trigger.
        settings.auto.enabled = false;
        assert_eq!(
            next_trigger_owner(&settings, TriggerOwner::None, &buttons(true, false)),
            TriggerOwner::Motion
        );
    }

    #[test]
    fn toggle_mode_switches_motion_on_each_press_of_its_trigger() {
        let (service, data_dir) = test_service("motion-toggle");
        let guard = ServiceGuard(Arc::clone(&service));
        {
            let mut settings = service.settings.lock().unwrap();
            settings.motion_enabled = true;
            settings.motion_toggle = true;
            settings.auto.enabled = true;
            settings.auto.trigger_button = "m4".into();
        }
        let status = |state: &str, motion: bool, auto: bool| {
            json!({"version":1,"type":"device_status","state":state,"name":"test",
                "buttons":[false,motion,false,auto,false]})
        };
        let runtime = || {
            let settings = service.settings.lock().unwrap().clone();
            let device = service.device.lock().unwrap().clone();
            let runtime = super::movement_runtime(&settings, &device, None, 0);
            (
                runtime.motion_enabled && runtime.motion_always_on,
                runtime.motion_toggle,
                super::effective_detection_settings(&settings, &device).2,
            )
        };

        service.dispatch(status("connected", false, false)).unwrap();
        assert_eq!(runtime(), (false, true, false));
        service.dispatch(status("connected", true, false)).unwrap();
        assert_eq!(runtime(), (true, true, false), "a press turns Motion on");
        service.dispatch(status("connected", false, false)).unwrap();
        assert_eq!(
            runtime(),
            (true, true, false),
            "and it stays on after release"
        );
        service.dispatch(status("connected", false, true)).unwrap();
        assert_eq!(
            runtime(),
            (false, true, true),
            "an Auto hold still cuts Motion"
        );
        service.dispatch(status("connected", false, false)).unwrap();
        assert_eq!(runtime(), (true, true, false), "toggled Motion resumes");
        service.dispatch(status("connected", true, false)).unwrap();
        service.dispatch(status("connected", false, false)).unwrap();
        assert_eq!(
            runtime(),
            (false, true, false),
            "a second press turns it off"
        );
        service.dispatch(status("connected", true, false)).unwrap();
        service.dispatch(status("missing", false, false)).unwrap();
        service.dispatch(status("connected", false, false)).unwrap();
        assert_eq!(runtime(), (false, true, false), "a disconnect turns it off");

        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn device_status_routes_auto_and_motion_by_the_held_trigger() {
        let (service, data_dir) = test_service("trigger-owner-routing");
        let guard = ServiceGuard(Arc::clone(&service));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        service.auto_events.lock().unwrap().emit = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });
        {
            let mut settings = service.settings.lock().unwrap();
            settings.motion_enabled = true;
            settings.motion_always_on = true;
            settings.auto.enabled = true;
            settings.auto.trigger_button = "m4".into();
        }
        let status = |motion: bool, auto: bool| {
            json!({"version":1,"type":"device_status","state":"connected","name":"test",
                "buttons":[false,motion,false,auto,false]})
        };
        let last_enabled = || events.lock().unwrap().last().unwrap()["enabled"].clone();
        let runtime = || {
            let settings = service.settings.lock().unwrap().clone();
            let device = service.device.lock().unwrap().clone();
            (
                super::movement_runtime(&settings, &device, None, 0).motion_enabled,
                super::effective_detection_settings(&settings, &device).2,
            )
        };

        service.dispatch(status(true, false)).unwrap();
        assert_eq!(last_enabled(), false, "a Motion hold cuts Auto");
        service.dispatch(status(true, true)).unwrap();
        assert_eq!(runtime(), (true, false), "Motion keeps the hold it started");
        service.dispatch(status(false, true)).unwrap();
        assert_eq!(last_enabled(), true);
        assert_eq!(
            runtime(),
            (false, true),
            "an Auto hold cuts Always-on Motion"
        );
        service.dispatch(status(false, false)).unwrap();
        assert_eq!(runtime(), (true, false), "Always-on resumes after the hold");
        assert_eq!(last_enabled(), true);

        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn an_old_frame_callback_cannot_rearm_auto_after_a_control_edit() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let mut events = AutoEvents {
            settings: AutoSettings::default(),
            revision: 0,
            emit: Arc::new(move |event| captured.lock().unwrap().push(event)),
        };
        let mut settings = AutoSettings::default();
        settings.enabled = true;
        events.configure(&settings);
        events.target(0, true); // callback finished under the previous config
        assert_eq!(output.lock().unwrap().len(), 1);
        events.target(1, true);
        let output = output.lock().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[1]["event"], "auto_target");
        assert_eq!(output[1]["target_valid"], true);
    }

    #[test]
    fn auto_controls_are_published_without_any_video_frames() {
        let (service, data_dir) = test_service("auto-control-no-frames");
        let guard = ServiceGuard(Arc::clone(&service));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        service.auto_events.lock().unwrap().emit = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });
        for (enabled, button, delay) in
            [(true, "right", 90), (false, "right", 90), (true, "m4", 30)]
        {
            service
                .dispatch(json!({"version":1,"type":"set_auto","settings":{
                    "enabled":enabled,"triggerButton":button,"delayMs":delay,
                    "cooldownMs":250,"sleepMs":5
                }}))
                .unwrap();
        }
        let output = events.lock().unwrap().clone();
        assert_eq!(output.len(), 3);
        for (index, event) in output.iter().enumerate() {
            assert_eq!(event["event"], "auto_config");
            assert_eq!(event["config_revision"], index + 1);
            assert_eq!(event["target_valid"], false);
            assert_eq!(
                event["timestamp_ms"], 0,
                "a control must not refresh a target sample"
            );
        }
        assert_eq!(output[1]["enabled"], false);
        assert_eq!(output[2]["trigger_button"], "m4");
        assert_eq!(output[2]["delay_ms"], 30);
        assert_eq!(service.pipeline.stats().processed_frames, 0);
        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn recovered_port_error_does_not_reappear_from_diagnostic_history() {
        let (service, data_dir) = test_service("recover-port-error");
        let guard = ServiceGuard(Arc::clone(&service));
        let busy = reserve_udp_socket();
        let port = busy.local_addr().unwrap().port();
        assert!(service.set_udp_port(port).is_err());
        assert!(!service.errors.lock().unwrap().is_empty());
        drop(busy);
        service.source.lock().unwrap().retry_at = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(3);
        while service.state()["port"] != port && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let state = service.state();
        assert_eq!(state["port"], port);
        assert_eq!(state["last_error"], Value::Null);
        assert!(
            !service.errors.lock().unwrap().is_empty(),
            "history should be retained separately"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

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
    fn color_filter_command_reaches_state_and_the_pipeline() {
        let (service, data_dir) = test_service("color-filter");
        let _guard = ServiceGuard(Arc::clone(&service));
        assert_eq!(service.state()["color_filter"], false);
        let response = service
            .dispatch(json!({"version":1,"type":"set_color_filter","enabled":true}))
            .unwrap();
        assert_eq!(response["state"]["color_filter"], true);
        // The next processed frame was filtered and carries its mask.
        let port = service.state()["port"].as_u64().unwrap() as u16;
        let jpeg = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/assets/red-cube-192.jpg"),
        )
        .unwrap();
        let sender = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let started = Instant::now();
        let mut id = 1;
        let processed = loop {
            if let Some(processed) = service.pipeline.latest() {
                break processed;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "no frame processed"
            );
            for packet in crate::udp::encode_jpeg_frame(id, 192, 192, id, &jpeg).unwrap() {
                sender.send_to(&packet, ("127.0.0.1", port)).unwrap();
            }
            id += 1;
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            processed.mask.as_ref().map(Vec::len),
            Some((192 * 192 + 7) / 8)
        );
        assert!(service
            .dispatch(json!({"version":1,"type":"set_color_filter"}))
            .is_err());
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn built_in_model_sets_object_detection_and_crop_resolution() {
        let (service, data_dir) = test_service("builtin-model");
        let _guard = ServiceGuard(Arc::clone(&service));
        // Graph input "images" [1, 3, 96, 96].
        fn tag(field: u8, payload: &[u8]) -> Vec<u8> {
            [
                vec![(field << 3) | 2, payload.len() as u8],
                payload.to_vec(),
            ]
            .concat()
        }
        let dims = [1u8, 3, 96, 96]
            .iter()
            .flat_map(|size| tag(1, &[1 << 3, *size]))
            .collect::<Vec<_>>();
        let tensor = tag(1, &[vec![1 << 3, 1], tag(2, &dims)].concat());
        let onnx: &'static [u8] =
            tag(7, &tag(11, &[tag(1, b"images"), tag(2, &tensor)].concat())).leak();
        let artifacts = [
            crate::builtin_models::EmbeddedArtifact {
                stem: "v11_96_fp16",
                extension: "onnx",
                bytes: onnx,
            },
            crate::builtin_models::EmbeddedArtifact {
                stem: "v11_96_int8",
                extension: "xml",
                bytes: b"<xml/>",
            },
            crate::builtin_models::EmbeddedArtifact {
                stem: "v11_96_int8",
                extension: "bin",
                bytes: b"weights",
            },
        ];
        let catalog = crate::builtin_models::install(
            &data_dir,
            &mut service.registry.lock().unwrap(),
            &mut service.settings.lock().unwrap(),
            &artifacts,
        )
        .unwrap();
        *service.builtin.lock().unwrap() = catalog;
        let listed = service
            .dispatch(json!({"version":1,"type":"get_app_settings"}))
            .unwrap();
        assert_eq!(listed["builtin_models"][0]["id"], "v11_96");
        assert_eq!(listed["builtin_models"][0]["label"], "V11 (96×96)");
        let mut requested = service.settings.lock().unwrap().clone();
        requested.strategy = "object_detection".into();
        requested.model_confidence = "0.7".into();
        requested.crop_resolution = 256;
        service
            .dispatch(json!({"version":1,"type":"apply_app_settings","settings":requested}))
            .unwrap();
        let applied = service.settings.lock().unwrap().clone();
        assert_eq!(applied.active_model_id.as_deref(), Some("v11_96"));
        assert_eq!(applied.crop_resolution, 256, "Crop Resolution stays Pixel Seek's");
        assert_eq!(
            service.state()["expected_source_resolution"],
            json!([96, 96]),
            "Object Detection captures the model's input size"
        );
        assert_eq!(*service.frame_request.lock().unwrap(), 96);
        assert_eq!(
            service.registry.lock().unwrap().models["v11_96@fp16"].confidence,
            0.7
        );
        // INT8 exists only for OpenVINO.
        let mut int8 = applied.clone();
        int8.model_precision = "int8".into();
        let error = service
            .dispatch(json!({"version":1,"type":"apply_app_settings","settings":int8}))
            .unwrap_err();
        assert!(error.contains("no INT8 model for ONNX Runtime"), "{error}");
        int8.model_backend = "openvino".into();
        service
            .dispatch(json!({"version":1,"type":"apply_app_settings","settings":int8}))
            .unwrap();
        assert_eq!(
            service.registry.lock().unwrap().active_model_id.as_deref(),
            Some("v11_96@int8")
        );
        assert!(service
            .dispatch(json!({"version":1,"type":"import_model_bundle","path":"x.onnx"}))
            .is_err());
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn object_detection_requires_an_imported_model() {
        let (service, data_dir) = test_service("object-detection-without-model");
        let _guard = ServiceGuard(Arc::clone(&service));
        let original = service.settings.lock().unwrap().clone();
        let original_file = std::fs::read(&service.settings_path).unwrap();
        let error = service
            .dispatch(json!({
                "version":1,
                "type":"set_strategy",
                "mode":"object_detection"
            }))
            .expect_err("Object Detection without a model must not reconfigure the receiver");
        assert!(error.contains("built-in"), "{error}");
        let mut requested = original.clone();
        requested.strategy = "object_detection".into();
        let error = service
            .dispatch(json!({
                "version":1,
                "type":"apply_app_settings",
                "settings":requested
            }))
            .expect_err("Apply Changes must reject Object Detection without a model");
        assert!(error.contains("built-in"), "{error}");
        assert_eq!(*service.settings.lock().unwrap(), original);
        assert_eq!(
            std::fs::read(&service.settings_path).unwrap(),
            original_file
        );
        assert_eq!(service.pipeline.stats().processed_frames, 0);
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn apply_settings_applies_the_rest_when_the_udp_port_is_busy() {
        let (service, data_dir) = test_service("apply-busy-port");
        let _guard = ServiceGuard(Arc::clone(&service));
        let busy = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let error = service
            .dispatch(json!({"version":1,"type":"apply_settings","commands":[
                {"version":1,"type":"set_udp_port","port":busy_port},
                {"version":1,"type":"set_crop_resolution","resolution":256},
                {"version":1,"type":"set_trigger_button","button":"m5"},
            ]}))
            .expect_err("the busy port is reported");
        assert!(error.contains("set_udp_port"), "{error}");
        let state = service.state();
        assert_eq!(state["crop_resolution"], 256);
        assert_eq!(state["trigger_button"], "m5");
        drop(busy);
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn startup_reclaims_old_per_frame_detection_logs() {
        let data_dir = std::env::temp_dir().join(format!(
            "sysx-old-sessions-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let old_log = data_dir.join("sessions").join("1").join("detections.jsonl");
        std::fs::create_dir_all(old_log.parent().unwrap()).unwrap();
        std::fs::write(&old_log, b"{}\n").unwrap();
        let reservation = reserve_udp_socket();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let service = ReceiverService::new("d".repeat(64), data_dir.clone(), port).unwrap();
        let _guard = ServiceGuard(Arc::clone(&service));
        assert!(!data_dir.join("sessions").exists());
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
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
        assert_eq!(
            super::ClientRole::parse(Some("monitor")),
            Some(super::ClientRole::Monitor)
        );
        assert_eq!(
            super::ClientRole::parse(Some("host")),
            Some(super::ClientRole::Host)
        );
        assert_eq!(super::ClientRole::parse(None), None);
        assert_eq!(super::ClientRole::parse(Some("config")), None);
    }

    #[test]
    fn middle_button_rising_edge_cycles_profiles_once_and_persists() {
        let (service, data_dir) = test_service("profiles");
        let _guard = ServiceGuard(Arc::clone(&service));
        service
            .dispatch(json!({"version":1,"type":"create_profile","duplicate":false}))
            .unwrap();
        service
            .dispatch(json!({"version":1,"type":"activate_profile","index":0}))
            .unwrap();
        let release = json!({
            "version":1,
            "type":"device_status",
            "state":"connected",
            "buttons":[false,false,false,false,false]
        });
        let press = json!({
            "version":1,
            "type":"device_status",
            "state":"connected",
            "buttons":[false,false,true,false,false]
        });
        service.dispatch(release).unwrap();
        service.dispatch(press.clone()).unwrap();
        assert_eq!(service.state()["profile_index"], 1);
        service.dispatch(press).unwrap();
        assert_eq!(service.state()["profile_index"], 1);
        let saved = load_settings(data_dir.join("app-settings.json"));
        assert_eq!(saved.active_profile, 1);
        assert_eq!(saved.profiles.len(), 2);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn motion_trigger_edge_advances_shared_movement_revision() {
        let (service, data_dir) = test_service("motion-trigger-revision");
        let _guard = ServiceGuard(Arc::clone(&service));
        service.settings.lock().unwrap().motion_enabled = true;
        let revision = service.auto_events.lock().unwrap().revision;

        service
            .dispatch(json!({
                "version": 1,
                "type": "device_status",
                "state": "connected",
                "name": "Mock Medius",
                "buttons": [false, true, false, false, false]
            }))
            .unwrap();

        assert_eq!(service.auto_events.lock().unwrap().revision, revision + 1);
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
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
    fn the_plugin_crops_crop_resolution_in_pixel_seek_and_the_model_size_in_object_detection() {
        let mut settings = AppSettings::default();
        settings.crop_resolution = 256;
        assert_eq!(super::frame_request(&settings, Some(640)), 256);
        settings.strategy = "object_detection".into();
        assert_eq!(super::frame_request(&settings, Some(640)), 640);
        assert_eq!(super::frame_request(&settings, None), 256);
    }

    #[test]
    fn dynamic_fov_keys_and_a_held_magnet_trigger_pick_the_target_fov() {
        let mut settings = AppSettings::default();
        settings.fov_degrees = 40;
        settings.dynamic_fov_degrees = 90;
        settings.dynamic_fov_key = "right".into();
        settings.magnet.enabled = true;
        settings.magnet.trigger_button = "m5".into();
        settings.magnet.fov_degrees = 60;
        settings.magnet.dynamic_fov_degrees = 120;
        settings.magnet.dynamic_fov_key = "right".into();
        let device = |right: bool, m5: bool| json!({"state":"connected","buttons":[false,right,false,false,m5]});
        let fov = |device: &Value| super::effective_detection_settings(&settings, device).0;
        assert_eq!(fov(&device(false, false)), 40);
        assert_eq!(fov(&device(true, false)), 90, "Motion dynamic FOV");
        assert_eq!(
            fov(&device(false, true)),
            60,
            "Magnet FOV while its trigger is held"
        );
        assert_eq!(fov(&device(true, true)), 120, "Magnet dynamic FOV");
    }

    #[test]
    fn auto_trigger_switches_effective_fov_without_changing_shared_target_color() {
        let mut settings = AppSettings::default();
        settings.fov_degrees = 42;
        settings.pixel_color = "yellow".into();
        settings.magnet.enabled = true;
        settings.magnet.mode = "auto".into();
        settings.magnet.trigger_button = "m4".into();
        settings.magnet.fov_degrees = 88;
        settings.auto = settings.magnet.auto();

        let released = json!({"state":"connected","buttons":[false,false,false,false,false]});
        assert_eq!(
            super::effective_detection_settings(&settings, &released),
            (42, "yellow".into(), false)
        );

        let held = json!({"state":"connected","buttons":[false,false,false,true,false]});
        assert_eq!(
            super::effective_detection_settings(&settings, &held),
            (88, "yellow".into(), true)
        );
        // A held Auto trigger owns routing even with Motion Always-on.
        settings.motion_always_on = true;
        assert_eq!(
            super::effective_detection_settings(&settings, &held),
            (88, "yellow".into(), true)
        );
    }

    #[test]
    fn a_held_auto_trigger_turns_motion_off() {
        let mut settings = AppSettings::default();
        settings.motion_enabled = true;
        settings.auto.enabled = true;
        settings.auto.trigger_button = "m4".into();
        let released = json!({"state":"connected","buttons":[false,false,false,false,false]});
        let held = json!({"state":"connected","buttons":[false,false,false,true,false]});
        assert!(super::movement_runtime(&settings, &released, None, 7).motion_enabled);
        let auto = super::movement_runtime(&settings, &held, None, 7);
        assert!(!auto.motion_enabled);
        assert_eq!(auto.config_revision, 7);
    }

    #[test]
    fn websocket_origin_and_token_gates_reject_non_local_or_wrong_credentials() {
        assert!(super::origin_allowed(Some("tauri://localhost")));
        assert!(!super::origin_allowed(Some("http://127.0.0.1:4201")));
        assert!(!super::origin_allowed(Some("https://example.invalid")));
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
    fn movement_settings_acknowledge_without_relocking_the_settings_guard() {
        let (service, data_dir) = test_service("movement-settings-ack");
        let guard = ServiceGuard(Arc::clone(&service));
        let worker_service = Arc::clone(&service);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_service.dispatch(json!({"version":1,"type":"apply_settings",
                "commands":[
                    {"version":1,"type":"set_movement_calibration","sens":0.23,"motion_enabled":true},
                    {"version":1,"type":"set_motion_config","speed_booster":true,"speed_near":0.12,"speed_mid":1.0,"speed_far":2.0,"continue_inside_box":true}
                ]}));
            let _ = tx.send(result);
        });
        let response = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("settings command must not deadlock")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(response["state"]["sens"], 0.23);
        assert_eq!(response["state"]["motion_enabled"], true);
        assert_eq!(response["state"]["continue_inside_box"], true);
        assert_eq!(response["state"]["speed_near"], 0.12);
        drop(guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn native_app_settings_are_saved_before_runtime_acknowledgement() {
        let (service, data_dir) = test_service("native-app-settings");
        let _guard = ServiceGuard(Arc::clone(&service));
        let initial = service
            .dispatch(json!({"version":1,"type":"get_app_settings"}))
            .expect("load native settings");
        assert_eq!(initial["event"], "app_settings");
        assert_eq!(initial["settings"]["continueInsideBox"], false);
        let mut settings = AppSettings::default();
        settings.continue_inside_box = true;
        settings.color_filter = true;
        settings.pixel_color = "red".into();
        settings.port = service.state()["port"].as_u64().unwrap().to_string();

        let response = service
            .dispatch(json!({
                "version":1,
                "type":"apply_app_settings",
                "settings":settings
            }))
            .expect("persist and apply native settings");
        assert_eq!(response["event"], "ack");
        assert_eq!(response["state"]["continue_inside_box"], true);
        assert_eq!(response["state"]["color_filter"], true);
        let saved = load_settings(service.settings_path.clone());
        assert!(saved.continue_inside_box);
        assert_eq!(saved.pixel_color, "red");
        assert_eq!(
            saved.profiles[0].settings["continueInsideBox"], true,
            "active profile snapshot is synchronized before saving"
        );

        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn an_oversized_command_is_answered_with_its_type_and_request_id() {
        let padding = "x".repeat(MAX_MESSAGE_BYTES);
        let text = json!({
            "version":1,"type":"apply_app_settings","request_id":"native-settings",
            "settings":{"padding":padding}
        })
        .to_string();
        let event = oversized_command_error(&text);
        assert_eq!(event["event"], "error");
        assert_eq!(event["type"], "apply_app_settings");
        assert_eq!(event["request_id"], "native-settings");
        assert_eq!(oversized_command_error("not json")["event"], "error");
        assert!(MAX_SOCKET_MESSAGE_BYTES > MAX_MESSAGE_BYTES);
    }

    #[test]
    fn applying_a_draft_from_before_a_profile_cycle_keeps_the_new_profile_active() {
        let (service, data_dir) = test_service("native-app-settings-profile-cycle");
        let _guard = ServiceGuard(Arc::clone(&service));
        service
            .dispatch(json!({"version":1,"type":"create_profile","duplicate":true}))
            .expect("create a second profile");
        service
            .dispatch(json!({"version":1,"type":"activate_profile","index":0}))
            .expect("select the first profile");
        let draft = service
            .dispatch(json!({"version":1,"type":"get_app_settings"}))
            .expect("load the Settings draft")["settings"]
            .clone();
        // The middle button cycles to the second profile while Settings edits.
        service.cycle_profile().expect("cycle profiles");
        let mut draft: AppSettings = serde_json::from_value(draft).unwrap();
        draft.continue_inside_box = true;
        service
            .dispatch(json!({"version":1,"type":"apply_app_settings","settings":draft}))
            .expect("apply the stale draft");

        let saved = load_settings(service.settings_path.clone());
        assert_eq!(saved.active_profile, 1, "the cycled profile stays active");
        assert!(!saved.continue_inside_box);
        assert_eq!(saved.profiles[0].settings["continueInsideBox"], true);
        assert_eq!(service.state()["profile_index"], 1);

        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn native_app_settings_with_busy_port_leave_disk_and_runtime_unchanged() {
        let (service, data_dir) = test_service("native-app-settings-busy-port");
        let _guard = ServiceGuard(Arc::clone(&service));
        let original_settings = service.settings.lock().unwrap().clone();
        let original_file = std::fs::read(&service.settings_path).unwrap();
        let original_port = service.state()["port"].as_u64().unwrap();
        let busy = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let mut requested = original_settings.clone();
        requested.port = busy.local_addr().unwrap().port().to_string();
        requested.color_filter = !requested.color_filter;

        let error = service
            .dispatch(json!({
                "version":1,
                "type":"apply_app_settings",
                "settings":requested
            }))
            .expect_err("a busy UDP port must reject the settings transaction");

        assert!(!error.is_empty());
        assert_eq!(*service.settings.lock().unwrap(), original_settings);
        assert_eq!(service.state()["port"], original_port);
        assert_eq!(
            std::fs::read(&service.settings_path).unwrap(),
            original_file
        );

        drop(busy);
        drop(_guard);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn apply_settings_keeps_auto_trigger_and_fov_separate() {
        let (service, data_dir) = test_service("apply-auto-settings");
        let _guard = ServiceGuard(Arc::clone(&service));
        let response = service
            .dispatch(json!({
                "version":1,
                "type":"apply_settings",
                "commands":[{
                    "version":1,
                    "type":"set_auto",
                    "settings":{"triggerButton":"m4","fovDegrees":72,"delayMs":100,"cooldownMs":250,"sleepMs":10}
                }]
            }))
            .expect("apply Auto settings");
        assert_eq!(response["state"]["auto"]["triggerButton"], "m4");
        assert_eq!(response["state"]["auto"]["fovDegrees"], 72);
        assert_eq!(response["state"]["auto"]["delayMs"], 100);
        assert_eq!(response["state"]["auto"]["cooldownMs"], 250);
        assert_eq!(response["state"]["auto"]["sleepMs"], 10);
        assert_eq!(response["state"]["trigger_button"], "right");
        assert_eq!(response["state"]["fov_degrees"], 36);

        let mut settings = service.settings.lock().unwrap().clone();
        settings.auto.enabled = true;
        settings.motion_enabled = true;
        *service.device.lock().unwrap() = json!({
            "state":"connected", "buttons":[false,true,false,true,false]
        });
        let config = service.pipeline_config(&settings).unwrap();
        assert!(
            config.auto_target_active,
            "Auto owns overlapping held triggers"
        );
        assert_eq!(
            config.fov_degrees, 36,
            "Motion FOV must not switch to Auto FOV"
        );
        assert_eq!(config.auto_fov_degrees, 72);
        settings.motion_always_on = true;
        let config = service.pipeline_config(&settings).unwrap();
        assert!(
            config.auto_target_active,
            "Always-on no longer blocks a held Auto trigger"
        );
        assert_eq!((config.fov_degrees, config.auto_fov_degrees), (36, 72));

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
