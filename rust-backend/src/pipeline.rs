use crate::auto_target::{center_sides, CenterCrossing, CrosshairTracker};
use crate::config::ModelDefinition;
use crate::crop::CropRect;
use crate::inference::{decode_yolo_output, to_detection_objects, DetectionRuntime};
use crate::pixel_seek::{
    color_mask, color_mask_into, process_pixel_seek_with_scratch, try_pack_mask, PixelSeekOutput,
    PixelSeekScratch, PixelTargetColor,
};
use crate::receiver::SourceFrame;
use crate::results::{DetectionMode, DetectionResult};
use crate::targeting::{
    object_detection_candidates, object_detection_target, ExponentialTargetSmoother,
    FrameCoordinate, PriorTarget, SmoothingRange, TargetCandidate,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PIXEL_LOCK_WINDOW: Duration = Duration::from_millis(100);

fn target_is_fresh(received: Instant, now: Instant) -> bool {
    now.saturating_duration_since(received) <= PIXEL_LOCK_WINDOW
}

fn frame_size_changed(previous: Option<(u32, u32)>, current: (u32, u32)) -> bool {
    previous.is_some_and(|size| size != current)
}

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub strategy: DetectionMode,
    pub pixel_color: PixelTargetColor,
    pub crop_resolution: Option<u32>,
    pub fov_degrees: u16,
    pub auto_fov_degrees: u16,
    /// True only while the enabled Auto trigger is held. In that state the
    /// Pixel Seek's selected target feeds Auto validity only inside Auto FOV.
    pub auto_target_active: bool,
    /// A target lock belongs to the current held Motion/Auto action only.
    pub target_lock_active: bool,
    /// Auto's aim mode (only with `auto_target_active`): Pixel Seek also
    /// selects a target inside Auto FOV for movement.
    pub auto_aim: bool,
    /// Shared control/routing revision used to reject callbacks and Motion samples
    /// completed before a configuration or Auto trigger transition.
    pub auto_config_revision: u64,
    /// Black out every pixel but the selected target color before Motion,
    /// Auto (and Object Detection) process the frame.
    pub color_filter: bool,
    pub smoothing_x: SmoothingRange,
    pub smoothing_y: SmoothingRange,
    pub model: Option<ModelDefinition>,
    pub model_backend: String,
    pub compute_target: String,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            strategy: DetectionMode::PixelSeek,
            pixel_color: PixelTargetColor::Yellow,
            crop_resolution: Some(640),
            fov_degrees: 36,
            auto_fov_degrees: 36,
            auto_target_active: false,
            target_lock_active: true,
            auto_aim: false,
            auto_config_revision: 0,
            color_filter: false,
            smoothing_x: SmoothingRange::NONE,
            smoothing_y: SmoothingRange::NONE,
            model: None,
            model_backend: "onnxruntime".into(),
            compute_target: "cpu".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessedFrame {
    pub auto_config_revision: u64,
    pub frame: SourceFrame,
    pub result: DetectionResult,
    pub published_ns: u64,
    pub crop_x: u32,
    pub crop_y: u32,
    pub selected_target: Option<TargetCandidate>,
    /// Smoothing factors drawn for this track; movement applies them to the
    /// target's own motion (Preview uses `smoothed_target_xy`).
    pub smoothing_alpha: (f32, f32),
    /// Color filter on: its applied mask, packed one bit per pixel, so Preview
    /// shows the same image Motion/Auto processed.
    pub mask: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct PipelineStats {
    pub processed_frames: u64,
    pub dropped_input_frames: u64,
    pub stale_results: u64,
    pub rejected_frames: u64,
    pub resolution_mismatches: u64,
    pub processing_fps: Option<f64>,
    pub last_error: Option<String>,
}

struct Pending {
    frame: SourceFrame,
    generation: u64,
}
struct State {
    config: Arc<PipelineConfig>,
    generation: u64,
    pending: Option<Pending>,
    latest: Option<Arc<ProcessedFrame>>,
    stats: PipelineStats,
    completed_ns: VecDeque<u64>,
    stopping: bool,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

fn catch_processing_panic<T>(run: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).map_err(|panic| {
        panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic")
            .to_owned()
    })
}

pub struct LatestFramePipeline {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LatestFramePipeline {
    pub fn new(
        config: PipelineConfig,
        on_processed: Option<Arc<dyn Fn(&ProcessedFrame) + Send + Sync>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                config: Arc::new(config),
                generation: 0,
                pending: None,
                latest: None,
                stats: PipelineStats::default(),
                completed_ns: VecDeque::with_capacity(4096),
                stopping: false,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("sysx-frame-processing".into())
            .spawn(move || processing_supervisor(worker_shared, on_processed))?;
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub fn submit(&self, frame: SourceFrame) -> Result<(), &'static str> {
        let mut state = lock_state(&self.shared.state);
        if state.stopping {
            return Err("pipeline is stopping");
        }
        let generation = state.generation;
        if state
            .pending
            .replace(Pending { frame, generation })
            .is_some()
        {
            state.stats.dropped_input_frames += 1;
        }
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn configure(&self, config: PipelineConfig) -> Result<(), &'static str> {
        validate_config(&config)?;
        let mut state = lock_state(&self.shared.state);
        state.config = Arc::new(config);
        state.generation += 1;
        state.latest = None;
        if state.pending.take().is_some() {
            state.stats.dropped_input_frames += 1;
        }
        self.shared.wake.notify_all();
        Ok(())
    }
    pub fn latest(&self) -> Option<Arc<ProcessedFrame>> {
        lock_state(&self.shared.state).latest.clone()
    }
    pub fn stats(&self) -> PipelineStats {
        let mut state = lock_state(&self.shared.state);
        let now = monotonic_ns();
        while state
            .completed_ns
            .front()
            .is_some_and(|stamp| now.saturating_sub(*stamp) > 1_000_000_000)
        {
            state.completed_ns.pop_front();
        }
        let fps = if state.completed_ns.len() >= 2 {
            let elapsed = state
                .completed_ns
                .back()
                .copied()
                .unwrap_or(0)
                .saturating_sub(state.completed_ns.front().copied().unwrap_or(0));
            (elapsed > 0).then_some((state.completed_ns.len() - 1) as f64 * 1e9 / elapsed as f64)
        } else {
            None
        };
        PipelineStats {
            processing_fps: fps,
            ..state.stats.clone()
        }
    }
    pub fn close(&self, timeout: Duration) -> bool {
        let mut state = lock_state(&self.shared.state);
        state.stopping = true;
        state.pending = None;
        self.shared.wake.notify_all();
        drop(state);
        let Ok(mut slot) = self.worker.lock() else {
            return false;
        };
        let Some(worker) = slot.as_ref() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return false;
        }
        slot.take().is_none_or(|worker| worker.join().is_ok())
    }
}

impl Drop for LatestFramePipeline {
    fn drop(&mut self) {
        let _ = self.close(Duration::from_secs(1));
    }
}

fn processing_loop(
    shared: Arc<Shared>,
    on_processed: Option<Arc<dyn Fn(&ProcessedFrame) + Send + Sync>>,
) {
    let mut smoother = None;
    let mut smoother_generation = None;
    let mut prior_target: Option<PriorTarget> = None;
    let mut prior_target_at: Option<Instant> = None;
    let mut pixel_seek_scratch = PixelSeekScratch::default();
    // Not reset on reconfiguration: the game's crosshair stays the same.
    let mut crosshair = CrosshairTracker::default();
    let mut crossing = CenterCrossing::default();
    let mut runtime_cache = None;
    let mut previous_frame_size = None;
    loop {
        let (pending, config) = {
            let mut state = lock_state(&shared.state);
            while state.pending.is_none() && !state.stopping {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.stopping {
                return;
            }
            let Some(pending) = state.pending.take() else {
                continue;
            };
            (pending, state.config.clone())
        };
        let frame_size = (pending.frame.image.width(), pending.frame.image.height());
        if frame_size_changed(previous_frame_size, frame_size) {
            // Track coordinates and Auto crossings are expressed in source pixels.
            smoother_generation = None;
        }
        previous_frame_size = Some(frame_size);
        if smoother_generation != Some(pending.generation) {
            smoother = ExponentialTargetSmoother::new(config.smoothing_x, config.smoothing_y);
            smoother_generation = Some(pending.generation);
            prior_target = None;
            prior_target_at = None;
            crossing.reset();
        }
        let Some(smoother) = smoother.as_mut() else {
            continue;
        };
        let started = Instant::now();
        let prior_is_fresh =
            prior_target_at.is_some_and(|received| target_is_fresh(received, Instant::now()));
        let processed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_frame_with_scratch(
                pending.frame,
                &config,
                smoother,
                &mut runtime_cache,
                prior_target.as_ref(),
                prior_is_fresh,
                &mut pixel_seek_scratch,
                &mut crosshair,
                &mut crossing,
            )
        }))
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            Err(ProcessError::FramePanic(message.to_owned()))
        });
        let finished_ns = monotonic_ns();
        let mut state = lock_state(&shared.state);
        if pending.generation != state.generation {
            state.stats.stale_results += 1;
            continue;
        }
        match processed {
            Ok(processed) => {
                state.stats.processed_frames += 1;
                state.stats.last_error = None;
                if state.completed_ns.len() == 4096 {
                    state.completed_ns.pop_front();
                }
                state.completed_ns.push_back(finished_ns);
                // Shared, not copied: the frame image is ~1.2 MB at 640x640 and
                // preview senders read `latest` at the input frame rate.
                let processed = Arc::new(processed);
                state.latest = Some(Arc::clone(&processed));
                if processed.result.mode == DetectionMode::ObjectDetection {
                    if let Some(target) = &processed.selected_target {
                        prior_target = Some(PriorTarget {
                            bbox_xyxy: Some(target.bbox_xyxy),
                            class_id: target.class_id,
                        });
                        prior_target_at = Some(processed.frame.received);
                    } else {
                        prior_target = None;
                        prior_target_at = None;
                    }
                } else if !config.target_lock_active && !config.auto_target_active {
                    // Releasing every target-producing trigger cancels the
                    // previous track; the next press acquires a fresh one.
                    prior_target = None;
                    prior_target_at = None;
                } else {
                    // Pixel Seek reports only its locked target's blob.
                    prior_target = processed.result.objects.first().map(|object| PriorTarget {
                        bbox_xyxy: Some(object.bbox_xyxy),
                        class_id: None,
                    });
                    prior_target_at = prior_target.as_ref().map(|_| processed.frame.received);
                }
                drop(state);
                if let Some(callback) = &on_processed {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        callback(&processed)
                    }))
                    .is_err()
                    {
                        crate::diagnostics::record(
                            "frame callback panicked; processing worker recovered",
                        );
                    }
                }
            }
            Err(error) => {
                if matches!(&error, ProcessError::FramePanic(_)) {
                    runtime_cache = None;
                    crossing.reset();
                    prior_target = None;
                    prior_target_at = None;
                }
                state.stats.rejected_frames += 1;
                if matches!(error, ProcessError::ModelResolutionMismatch { .. }) {
                    state.stats.resolution_mismatches += 1;
                }
                state.stats.last_error = Some(error.to_string());
                smoother.update(None);
            }
        }
        let _ = started;
    }
}

fn processing_supervisor(
    shared: Arc<Shared>,
    on_processed: Option<Arc<dyn Fn(&ProcessedFrame) + Send + Sync>>,
) {
    loop {
        let result =
            catch_processing_panic(|| processing_loop(Arc::clone(&shared), on_processed.clone()));
        let Err(detail) = result else {
            return;
        };
        let message = format!("Frame processing worker recovered from panic: {detail}");
        {
            let mut state = lock_state(&shared.state);
            if state.stopping {
                return;
            }
            state.stats.last_error = Some(message.clone());
        }
        crate::diagnostics::record(&message);
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
fn process_frame(
    frame: SourceFrame,
    config: &PipelineConfig,
    smoother: &mut ExponentialTargetSmoother,
    runtime_cache: &mut Option<RuntimeCache>,
    previous_target: Option<&PriorTarget>,
    previous_is_fresh: bool,
    crossing: &mut CenterCrossing,
) -> Result<ProcessedFrame, ProcessError> {
    process_frame_with_scratch(
        frame,
        config,
        smoother,
        runtime_cache,
        previous_target,
        previous_is_fresh,
        &mut PixelSeekScratch::default(),
        &mut CrosshairTracker::default(),
        crossing,
    )
}

fn process_frame_with_scratch(
    frame: SourceFrame,
    config: &PipelineConfig,
    smoother: &mut ExponentialTargetSmoother,
    runtime_cache: &mut Option<RuntimeCache>,
    previous_target: Option<&PriorTarget>,
    previous_is_fresh: bool,
    pixel_seek_scratch: &mut PixelSeekScratch,
    crosshair: &mut CrosshairTracker,
    crossing: &mut CenterCrossing,
) -> Result<ProcessedFrame, ProcessError> {
    let frame_started = Instant::now();
    // The OBS plugin is the sole owner of capture cropping.  The receiver
    // must process the JPEG exactly as received; applying another centered
    // crop here would double-crop the image and waste a full-frame copy.
    let source_width = frame.image.width();
    let source_height = frame.image.height();
    let target_lock_active = config.target_lock_active || config.auto_target_active;
    let rect = CropRect {
        x: 0,
        y: 0,
        width: source_width,
        height: source_height,
    };
    let filter_mask = if config.color_filter {
        if config.strategy == DetectionMode::ObjectDetection {
            Some(color_mask(&frame.image, config.pixel_color))
        } else {
            let mut mask = pixel_seek_scratch.take_mask();
            if !color_mask_into(&frame.image, config.pixel_color, &mut mask) {
                pixel_seek_scratch.recycle_mask(mask);
                return Err(ProcessError::ColorMaskAllocationFailed);
            }
            Some(mask)
        }
    } else {
        None
    };
    let filtered_image = (config.strategy == DetectionMode::ObjectDetection)
        .then(|| {
            filter_mask
                .as_deref()
                .map(|mask| crate::pixel_seek::apply_color_mask(&frame.image, mask))
        })
        .flatten();
    let processed = filtered_image.as_ref().unwrap_or(&frame.image);
    let crop_ms = 0.0;
    if config.strategy == DetectionMode::ObjectDetection {
        let model = config.model.as_ref().ok_or_else(|| {
            ProcessError::ObjectDetectionUnavailable("select a model bundle".into())
        })?;
        if processed.width() != model.input_width || processed.height() != model.input_height {
            return Err(ProcessError::ModelResolutionMismatch {
                expected_width: model.input_width,
                expected_height: model.input_height,
                actual_width: processed.width(),
                actual_height: processed.height(),
            });
        }
        let runtime = cached_runtime(
            runtime_cache,
            model,
            &config.model_backend,
            &config.compute_target,
        )
        .map_err(ProcessError::ObjectDetectionUnavailable)?;
        let inference = runtime
            .infer(processed)
            .map_err(|error| ProcessError::ObjectDetectionUnavailable(error.to_string()))?;
        let postprocess_started = Instant::now();
        let boxes = decode_yolo_output(
            &inference.shape,
            &inference.values,
            model.confidence,
            model.iou,
        )
        .map_err(|error| ProcessError::ObjectDetectionUnavailable(error.to_string()))?;
        let objects = to_detection_objects(boxes, model.class_names.as_ref(), rect.x, rect.y);
        let mut result = DetectionResult {
            frame_id: frame.frame_id,
            timestamp_ns: frame.timestamp_ns,
            mode: DetectionMode::ObjectDetection,
            model_id: Some(model.id.clone()),
            source_width,
            source_height,
            processed_width: processed.width(),
            processed_height: processed.height(),
            objects,
            preprocess_ms: crop_ms + inference.preprocess_ms,
            detection_ms: inference.inference_ms,
            postprocess_ms: 0.0,
            total_processing_ms: 0.0,
            inference_device: Some(inference.device),
            target_xy: None,
            smoothed_target_xy: None,
            auto_target_valid: false,
            auto_active: false,
            auto_aim: false,
            fov_degrees: config.fov_degrees,
        };
        let candidates = object_detection_candidates(&result, model.confidence);
        let selected_target = object_detection_target(
            &candidates,
            processed.width(),
            processed.height(),
            rect.x,
            rect.y,
            config.fov_degrees,
            previous_target.filter(|_| target_lock_active),
            previous_is_fresh && target_lock_active,
        );
        result.target_xy = selected_target
            .as_ref()
            .map(|target| [target.coordinate.x, target.coordinate.y]);
        let filtered = smoother.update(result.target_xy.map(|point| FrameCoordinate {
            x: point[0],
            y: point[1],
        }));
        result.smoothed_target_xy = filtered.map(|point| [point.x, point.y]);
        result.postprocess_ms = postprocess_started.elapsed().as_secs_f64() * 1000.0;
        result.total_processing_ms = frame_started.elapsed().as_secs_f64() * 1000.0;
        let output_mask = match filter_mask.as_deref() {
            Some(mask) => Some(try_pack_mask(mask).ok_or(ProcessError::ColorMaskAllocationFailed)?),
            None => None,
        };
        return Ok(ProcessedFrame {
            auto_config_revision: config.auto_config_revision,
            frame: SourceFrame { ..frame },
            result,
            published_ns: monotonic_ns(),
            crop_x: rect.x,
            crop_y: rect.y,
            selected_target,
            smoothing_alpha: smoother.alphas(),
            mask: output_mask,
        });
    }
    let started = Instant::now();
    let locked = previous_target
        .filter(|_| previous_is_fresh && target_lock_active)
        .and_then(|previous| previous.bbox_xyxy);
    let auto_aim = config.auto_target_active && config.auto_aim;
    let mask = if let Some(mask) = filter_mask {
        mask
    } else {
        let mut mask = pixel_seek_scratch.take_mask();
        if !color_mask_into(processed, config.pixel_color, &mut mask) {
            pixel_seek_scratch.recycle_mask(mask);
            return Err(ProcessError::ColorMaskAllocationFailed);
        }
        mask
    };
    // Auto owns this frame while active. Center mode only checks the mask
    // around the center; aim mode selects its own target inside Auto FOV.
    let auto_without_aim = config.auto_target_active && !auto_aim;
    let output = if auto_without_aim {
        PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
        }
    } else {
        process_pixel_seek_with_scratch(
            processed,
            rect,
            config.pixel_color,
            if auto_aim {
                config.auto_fov_degrees
            } else {
                config.fov_degrees
            },
            locked,
            mask,
            pixel_seek_scratch,
        )
    };
    // Measured on the original image: the filter blacks the crosshair out.
    crosshair.update(&frame.image);
    let crosshair_px = crosshair.radius_px();
    let detection_ms = started.elapsed().as_secs_f64() * 1000.0;
    let target_xy = output.target.map(|point| [point.x, point.y]);
    // Aim mode: whether the target covers the crosshair now (the movement
    // ticker fires on it only from a frame that shows its last move);
    // center-crossing mode needs the target to pass the center.
    let auto_target_valid = if auto_aim {
        crossing.reset();
        center_sides(
            &output.mask,
            processed.width(),
            processed.height(),
            config.auto_fov_degrees,
            crosshair_px,
        )
        .covers()
    } else if config.auto_target_active {
        let sides = center_sides(
            &output.mask,
            processed.width(),
            processed.height(),
            config.auto_fov_degrees,
            crosshair_px,
        );
        crossing.update(sides, frame.received)
    } else {
        crossing.reset();
        false
    };
    let filtered = smoother.update(target_xy.map(|point| FrameCoordinate {
        x: point[0],
        y: point[1],
    }));
    let output_mask = if config.color_filter {
        match try_pack_mask(&output.mask) {
            Some(mask) => Some(mask),
            None => {
                pixel_seek_scratch.recycle_mask(output.mask);
                return Err(ProcessError::ColorMaskAllocationFailed);
            }
        }
    } else {
        None
    };
    pixel_seek_scratch.recycle_mask(output.mask);
    let total = frame_started.elapsed().as_secs_f64() * 1000.0;
    let result = DetectionResult {
        frame_id: frame.frame_id,
        timestamp_ns: frame.timestamp_ns,
        mode: DetectionMode::PixelSeek,
        model_id: None,
        source_width,
        source_height,
        processed_width: processed.width(),
        processed_height: processed.height(),
        objects: output.objects,
        preprocess_ms: crop_ms,
        detection_ms,
        postprocess_ms: 0.0,
        total_processing_ms: total,
        inference_device: Some("cpu".into()),
        target_xy,
        smoothed_target_xy: filtered.map(|point| [point.x, point.y]),
        auto_target_valid,
        auto_active: config.auto_target_active,
        auto_aim,
        fov_degrees: if config.auto_target_active {
            config.auto_fov_degrees
        } else {
            config.fov_degrees
        },
    };
    Ok(ProcessedFrame {
        auto_config_revision: config.auto_config_revision,
        frame: SourceFrame { ..frame },
        result,
        published_ns: monotonic_ns(),
        crop_x: rect.x,
        crop_y: rect.y,
        selected_target: None,
        smoothing_alpha: smoother.alphas(),
        mask: output_mask,
    })
}

#[derive(PartialEq, Eq)]
struct RuntimeKey {
    model_id: String,
    backend: String,
    compute_target: String,
    artifact_paths: [String; 4],
    input_width: u32,
    input_height: u32,
}

enum RuntimeCache {
    Loaded {
        key: RuntimeKey,
        runtime: DetectionRuntime,
    },
    Failed {
        key: RuntimeKey,
        message: String,
    },
}

impl RuntimeCache {
    fn key(&self) -> &RuntimeKey {
        match self {
            Self::Loaded { key, .. } | Self::Failed { key, .. } => key,
        }
    }
}

fn cached_runtime<'a>(
    cache: &'a mut Option<RuntimeCache>,
    model: &ModelDefinition,
    backend: &str,
    compute_target: &str,
) -> Result<&'a mut DetectionRuntime, String> {
    let key = RuntimeKey {
        model_id: model.id.clone(),
        backend: backend.to_owned(),
        compute_target: compute_target.to_owned(),
        artifact_paths: model.artifacts.paths().map(str::to_owned),
        input_width: model.input_width,
        input_height: model.input_height,
    };
    let matches = cache.as_ref().is_some_and(|entry| entry.key() == &key);
    if !matches {
        *cache = Some(
            match DetectionRuntime::load(model, backend, compute_target) {
                Ok(runtime) => RuntimeCache::Loaded { key, runtime },
                Err(error) => RuntimeCache::Failed {
                    key,
                    message: error.to_string(),
                },
            },
        );
    }
    match cache
        .as_mut()
        .ok_or_else(|| "model runtime cache could not be initialized".to_owned())?
    {
        RuntimeCache::Loaded { runtime, .. } => Ok(runtime),
        RuntimeCache::Failed { message, .. } => Err(message.clone()),
    }
}

fn validate_config(config: &PipelineConfig) -> Result<(), &'static str> {
    if !(1..=360).contains(&config.fov_degrees) || !(1..=360).contains(&config.auto_fov_degrees) {
        return Err("FOV must be 1..360");
    }
    if !config.smoothing_x.is_valid() || !config.smoothing_y.is_valid() {
        return Err("smoothing must be 1..50 with min <= max");
    }
    if config.crop_resolution == Some(0) {
        return Err("crop resolution must be positive");
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ProcessError {
    #[error("could not allocate the Pixel Seek color or Preview mask")]
    ColorMaskAllocationFailed,
    #[error("frame processing panicked and was recovered: {0}")]
    FramePanic(String),
    #[error("model resolution {expected_width}x{expected_height} does not match received cropped frame {actual_width}x{actual_height}")]
    ModelResolutionMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    #[error("Object Detection is not available in the Rust runtime yet: {0}")]
    ObjectDetectionUnavailable(String),
}

fn monotonic_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelArtifacts;
    use crate::movement::{MovementRuntimeSettings, MovementTargetSample, MovementTicker};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn target_freshness_includes_time_spent_waiting_for_processing() {
        let received = Instant::now();
        assert!(target_is_fresh(
            received,
            received + Duration::from_millis(100)
        ));
        assert!(!target_is_fresh(
            received,
            received + Duration::from_millis(101)
        ));
    }

    #[test]
    fn frame_size_changes_invalidate_source_pixel_tracks() {
        assert!(!frame_size_changed(None, (640, 640)));
        assert!(!frame_size_changed(Some((640, 640)), (640, 640)));
        assert!(frame_size_changed(Some((640, 640)), (320, 320)));
        assert!(frame_size_changed(Some((640, 360)), (360, 640)));
    }

    #[test]
    fn pipeline_state_lock_recovers_after_poisoning() {
        let state = Mutex::new(State {
            config: Arc::new(PipelineConfig::default()),
            generation: 0,
            pending: None,
            latest: None,
            stats: PipelineStats::default(),
            completed_ns: VecDeque::new(),
            stopping: false,
        });
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.lock().unwrap();
            panic!("simulate processing worker panic while holding state");
        }));
        assert!(poisoned.is_err());
        lock_state(&state).stopping = true;
        assert!(lock_state(&state).stopping);
    }

    #[test]
    fn processing_worker_panic_is_reported_for_supervisor_recovery() {
        let error =
            catch_processing_panic(|| -> () { panic!("simulated worker panic") }).unwrap_err();
        assert_eq!(error, "simulated worker panic");
    }

    #[test]
    fn a_panicking_frame_callback_does_not_stop_pipeline_processing() {
        let panic_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let should_panic = Arc::clone(&panic_once);
        let (delivered, received) = std::sync::mpsc::channel();
        let pipeline = LatestFramePipeline::new(
            PipelineConfig::default(),
            Some(Arc::new(move |frame| {
                if should_panic.swap(false, Ordering::AcqRel) {
                    panic!("simulated processed-frame callback panic");
                }
                let _ = delivered.send(frame.frame.frame_id);
            })),
        )
        .unwrap();
        let submit = |frame_id| {
            pipeline
                .submit(SourceFrame {
                    frame_id,
                    timestamp_ns: frame_id,
                    image: image::RgbImage::from_pixel(32, 32, image::Rgb([0, 0, 0])),
                    source: "callback-panic-test".into(),
                    jpeg: Default::default(),
                    received: Instant::now(),
                })
                .unwrap();
        };

        submit(1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while pipeline.stats().processed_frames == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pipeline.stats().processed_frames, 1);

        submit(2);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        assert_eq!(pipeline.stats().processed_frames, 2);
        assert!(pipeline.close(Duration::from_secs(2)));
    }

    #[test]
    fn pipeline_close_timeout_keeps_join_handle_for_retry() {
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let callback_gate = Arc::clone(&gate);
        let pipeline = LatestFramePipeline::new(
            PipelineConfig::default(),
            Some(Arc::new(move |_| {
                let (state, changed) = &*callback_gate;
                let mut state = state.lock().unwrap();
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap();
                }
            })),
        )
        .unwrap();
        pipeline
            .submit(SourceFrame {
                frame_id: 1,
                timestamp_ns: 1,
                image: image::RgbImage::from_pixel(32, 32, image::Rgb([0, 0, 0])),
                source: "close-timeout-test".into(),
                jpeg: Default::default(),
                received: Instant::now(),
            })
            .unwrap();
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        while !state.0 {
            state = changed.wait(state).unwrap();
        }
        drop(state);

        assert!(!pipeline.close(Duration::ZERO));
        assert!(!pipeline.close(Duration::ZERO));
        let mut state = gate.0.lock().unwrap();
        state.1 = true;
        gate.1.notify_all();
        drop(state);
        assert!(pipeline.close(Duration::from_secs(2)));
    }

    #[test]
    #[ignore = "long-running, headless movement/detection performance soak"]
    fn sustained_held_trigger_tracks_a_continuously_moving_pixel_target() {
        let seconds = std::env::var("SYSX_SOAK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(120);
        let input_fps = std::env::var("SYSX_SOAK_INPUT_FPS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60)
            .max(1);
        let frame_period = Duration::from_secs_f64(1.0 / input_fps as f64);
        let warm_started = Instant::now();
        crate::pixel_seek::warm_profile_luts();
        let warm_ms = warm_started.elapsed().as_secs_f64() * 1000.0;
        let settings = MovementRuntimeSettings {
            renderer_profile: None,
            config_revision: 0,
            motion_enabled: true,
            motion_always_on: false,
            sens: 0.25,
            offset_x: 0.0,
            offset_y: 0.0,
            speed_x: 0.06,
            speed_y: 0.06,
            smooth_fov_degrees: 20,
            no_deadzone: false,
            trigger_button: "right".into(),
            trigger_held: true,
            auto_flick: false,
            recoil: crate::config::RecoilSettings::default(),
        };
        let commands = Arc::new(AtomicU64::new(0));
        let moved = Arc::clone(&commands);
        let ticker = Arc::new(
            MovementTicker::new(
                settings.sens,
                Arc::new(move || Some(settings.clone())),
                Arc::new(move |event| {
                    if event.target_valid && (event.dx != 0 || event.dy != 0) {
                        moved.fetch_add(1, Ordering::Relaxed);
                    }
                }),
            )
            .unwrap(),
        );
        let frames_processed = Arc::new(AtomicU64::new(0));
        let timings = Arc::new(Mutex::new(Vec::new()));
        let callback_ticker = Arc::clone(&ticker);
        let callback_count = Arc::clone(&frames_processed);
        let callback_timings = Arc::clone(&timings);
        let config = PipelineConfig {
            strategy: DetectionMode::PixelSeek,
            pixel_color: PixelTargetColor::Red,
            crop_resolution: Some(640),
            fov_degrees: 360,
            ..PipelineConfig::default()
        };
        let pipeline = LatestFramePipeline::new(
            config,
            Some(Arc::new(move |processed| {
                if let Some(target) = processed.result.target_xy {
                    callback_ticker.update(MovementTargetSample {
                        config_revision: processed.auto_config_revision,
                        frame_id: processed.frame.frame_id,
                        target: Some((f64::from(target[0]), f64::from(target[1]))),
                        smoothing_alpha: (
                            f64::from(processed.smoothing_alpha.0),
                            f64::from(processed.smoothing_alpha.1),
                        ),
                        crop_origin: (processed.crop_x, processed.crop_y),
                        processed_size: (
                            processed.result.processed_width,
                            processed.result.processed_height,
                        ),
                        received_s: 0.0,
                        age_s: processed.frame.received.elapsed().as_secs_f64(),
                        on_center: false,
                    });
                }
                callback_count.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut samples) = callback_timings.lock() {
                    samples.push((
                        processed.result.total_processing_ms,
                        processed.result.detection_ms,
                        processed.frame.received.elapsed().as_secs_f64() * 1000.0,
                        processed.frame.frame_id,
                    ));
                }
            })),
        )
        .expect("pipeline worker starts");
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut frame_id = 0_u64;
        while Instant::now() < deadline {
            let now = Instant::now();
            let phase = frame_id as f64 * 0.035;
            let target_x = (320.0 + phase.sin() * 210.0) as u32;
            let target_y = (320.0 + (phase * 0.63).cos() * 145.0) as u32;
            let mut image = image::RgbImage::from_pixel(640, 640, image::Rgb([0, 0, 0]));
            for y in target_y.saturating_sub(4)..(target_y + 4).min(640) {
                for x in target_x.saturating_sub(4)..(target_x + 4).min(640) {
                    image.put_pixel(x, y, image::Rgb([255, 0, 0]));
                }
            }
            pipeline
                .submit(SourceFrame {
                    frame_id,
                    timestamp_ns: 0,
                    image,
                    source: "soak-test".into(),
                    jpeg: Default::default(),
                    received: now,
                })
                .expect("pipeline remains available during soak");
            frame_id += 1;
            thread::sleep(frame_period.saturating_sub(now.elapsed()));
        }
        assert!(
            pipeline.close(Duration::from_secs(5)),
            "pipeline worker stopped"
        );
        assert!(
            ticker.close(Duration::from_secs(5)),
            "movement ticker stopped"
        );
        let stats = pipeline.stats();
        let mut timings = timings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        timings.sort_by(|left, right| left.0.total_cmp(&right.0));
        let percentile = |fraction: f64| {
            let index = ((timings.len().saturating_sub(1)) as f64 * fraction).round() as usize;
            timings.get(index).copied().unwrap_or_default()
        };
        let mut detection_ms = timings.iter().map(|sample| sample.1).collect::<Vec<_>>();
        detection_ms.sort_by(f64::total_cmp);
        let detection_percentile = |fraction: f64| {
            detection_ms
                .get(((detection_ms.len().saturating_sub(1)) as f64 * fraction).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let max = timings.last().copied().unwrap_or_default();
        println!(
            "held-trigger moving-target soak: duration={}s input_fps={} lut_warm_ms={:.2} submitted={} processed={} dropped={} stale={} rejected={} movement_commands={} processing_ms[p50={:.2},p95={:.2},p99={:.2},max={:.2},max_detection={:.2},max_frame_age={:.2},max_frame_id={}]",
            seconds,
            input_fps,
            warm_ms,
            frame_id,
            stats.processed_frames,
            stats.dropped_input_frames,
            stats.stale_results,
            stats.rejected_frames,
            commands.load(Ordering::Relaxed),
            percentile(0.50).0,
            percentile(0.95).0,
            percentile(0.99).0,
            max.0,
            max.1,
            max.2,
            max.3,
        );
        println!(
            "pixel-seek detection_ms[p95={:.2},p99={:.2},max={:.2}]",
            detection_percentile(0.95),
            detection_percentile(0.99),
            detection_ms.last().copied().unwrap_or(0.0),
        );
        assert!(stats.processed_frames > 0, "at least one frame processed");
        assert!(
            commands.load(Ordering::Relaxed) > 0,
            "held trigger emitted movement"
        );
        assert_eq!(
            stats.rejected_frames, 0,
            "pipeline reported no rejected frames"
        );
    }

    #[test]
    fn auto_frames_never_publish_motion_targets_and_use_their_own_fov() {
        let mut config = PipelineConfig {
            pixel_color: PixelTargetColor::Red,
            fov_degrees: 360,
            auto_fov_degrees: 1,
            ..PipelineConfig::default()
        };
        let mut smoother =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        let mut cache = None;
        // Motion first, then Auto off-center, center crossing, target loss,
        // and Motion again. Auto intent must survive target loss/delayed clicks.
        for (id, auto, center) in [
            (1, false, false),
            (2, true, false),
            (3, true, true),
            (4, true, false),
            (5, false, false),
        ] {
            config.auto_target_active = auto;
            let mut image = image::RgbImage::new(64, 64);
            image.put_pixel(40, 32, image::Rgb([255, 0, 0]));
            if center {
                image.put_pixel(32, 32, image::Rgb([255, 0, 0]));
            }
            let frame = SourceFrame {
                frame_id: id,
                timestamp_ns: 0,
                image,
                source: "test".into(),
                jpeg: Default::default(),
                received: std::time::Instant::now(),
            };
            let result = process_frame(
                frame,
                &config,
                &mut smoother,
                &mut cache,
                None,
                false,
                &mut Default::default(),
            )
            .unwrap()
            .result;
            assert_eq!(result.auto_active, auto);
            assert_eq!(result.auto_target_valid, auto && center);
            assert_eq!(result.fov_degrees, if auto { 1 } else { 360 });
            assert_eq!(result.target_xy.is_none(), auto);
            assert_eq!(result.smoothed_target_xy.is_none(), auto);
            if auto {
                assert!(result.objects.is_empty());
            }
        }
    }

    #[test]
    fn auto_aim_fires_on_the_target_it_selects_inside_auto_fov() {
        let config = PipelineConfig {
            pixel_color: PixelTargetColor::Red,
            fov_degrees: 360,
            auto_fov_degrees: 20,
            auto_target_active: true,
            auto_aim: true,
            ..PipelineConfig::default()
        };
        let mut smoother =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        let mut cache = None;
        let run = |image: image::RgbImage,
                   id: u64,
                   smoother: &mut ExponentialTargetSmoother,
                   cache: &mut Option<_>| {
            let mut crossing = CenterCrossing::default();
            process_frame(
                SourceFrame {
                    frame_id: id,
                    timestamp_ns: 0,
                    image,
                    source: "test".into(),
                    jpeg: Default::default(),
                    received: std::time::Instant::now(),
                },
                &config,
                smoother,
                cache,
                None,
                false,
                &mut crossing,
            )
            .unwrap()
            .result
        };
        // 20° Auto FOV is about 17 px: the pixel 6 px away is selected, the one
        // 30 px away (inside the 360° Motion FOV) is not.
        let mut image = image::RgbImage::new(64, 64);
        image.put_pixel(38, 32, image::Rgb([255, 0, 0]));
        image.put_pixel(32, 2, image::Rgb([255, 0, 0]));
        let result = run(image, 1, &mut smoother, &mut cache);
        assert!(result.auto_active && result.auto_aim);
        assert_eq!(result.target_xy, Some([38.5, 32.5]), "pixel centers");
        assert!(
            !result.auto_target_valid,
            "off center: the ticker moves first, nothing covers the crosshair"
        );
        assert_eq!(result.fov_degrees, 20);

        // Only the pixel outside Auto FOV: no target, no fire.
        let mut image = image::RgbImage::new(64, 64);
        image.put_pixel(32, 2, image::Rgb([255, 0, 0]));
        let result = run(image, 2, &mut smoother, &mut cache);
        assert_eq!(result.target_xy, None);
        assert!(!result.auto_target_valid);

        // The target on the crosshair: the center gate reports it.
        let mut image = image::RgbImage::new(64, 64);
        image.put_pixel(32, 32, image::Rgb([255, 0, 0]));
        let result = run(image, 3, &mut smoother, &mut cache);
        assert!(result.auto_target_valid);
    }

    #[test]
    fn processing_keeps_the_plugin_cropped_frame_without_recropping() {
        let frame = SourceFrame {
            frame_id: 7,
            timestamp_ns: 11,
            image: image::RgbImage::new(320, 320),
            source: "udp".into(),
            jpeg: Default::default(),
            received: std::time::Instant::now(),
        };
        let config = PipelineConfig::default();
        let mut smoother =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();
        let processed = process_frame(
            frame,
            &config,
            &mut smoother,
            &mut runtime_cache,
            None,
            false,
            &mut crossing,
        )
        .expect("the already-cropped frame should be processed as-is");

        assert_eq!(
            (
                processed.frame.image.width(),
                processed.frame.image.height()
            ),
            (320, 320)
        );
        assert_eq!((processed.crop_x, processed.crop_y), (0, 0));
        assert_eq!(
            (
                processed.result.source_width,
                processed.result.source_height
            ),
            (320, 320)
        );
        assert_eq!(
            (
                processed.result.processed_width,
                processed.result.processed_height
            ),
            (320, 320)
        );
    }

    #[test]
    fn pixel_seek_keeps_climbing_to_each_newly_visible_higher_pixel() {
        // A target is first seen at row 4; as rows 3, 2, 1 and 0 appear, every
        // frame must retarget to the new topmost pixel (no stickiness).
        let config = PipelineConfig {
            pixel_color: PixelTargetColor::Red,
            fov_degrees: 360,
            ..PipelineConfig::default()
        };
        let mut smoother =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();
        let mut image = image::RgbImage::new(64, 64);
        for row in (0..=4u32).rev() {
            image.put_pixel(32, row + 20, image::Rgb([255, 0, 0]));
            let processed = process_frame(
                SourceFrame {
                    frame_id: u64::from(row),
                    timestamp_ns: 0,
                    image: image.clone(),
                    source: "test".into(),
                    jpeg: Default::default(),
                    received: std::time::Instant::now(),
                },
                &config,
                &mut smoother,
                &mut runtime_cache,
                None,
                false,
                &mut crossing,
            )
            .unwrap();
            let target = processed.result.smoothed_target_xy.expect("target");
            assert_eq!(target[1], (row + 20) as f32 + 0.5, "frame for row {row}");
            assert_eq!(processed.result.objects.len(), 1);
        }
    }

    #[test]
    fn color_filter_runs_before_detection_and_hands_its_mask_to_preview() {
        let mut image = image::RgbImage::from_pixel(64, 64, image::Rgb([40, 90, 200]));
        for (x, y) in [(30, 20), (31, 20), (30, 21), (31, 21)] {
            image.put_pixel(x, y, image::Rgb([255, 0, 0]));
        }
        image.put_pixel(5, 5, image::Rgb([250, 250, 250]));
        let run = |color_filter: bool| {
            let config = PipelineConfig {
                pixel_color: PixelTargetColor::Red,
                fov_degrees: 360,
                color_filter,
                ..PipelineConfig::default()
            };
            let mut smoother =
                ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
            let mut crossing = CenterCrossing::default();
            process_frame(
                SourceFrame {
                    frame_id: 1,
                    timestamp_ns: 0,
                    image: image.clone(),
                    source: "test".into(),
                    jpeg: Default::default(),
                    received: std::time::Instant::now(),
                },
                &config,
                &mut smoother,
                &mut None,
                None,
                false,
                &mut crossing,
            )
            .unwrap()
        };
        let plain = run(false);
        let filtered = run(true);
        assert!(plain.mask.is_none(), "no mask without the filter");
        let mask = filtered.mask.as_ref().expect("filter mask");
        assert_eq!(mask.len(), (64 * 64 + 7) / 8);
        let bit_is_set = |index: usize| mask[index / 8] & (1 << (index % 8)) != 0;
        assert_eq!(mask.iter().map(|byte| byte.count_ones()).sum::<u32>(), 4);
        assert!(bit_is_set(20 * 64 + 30));
        assert!(!bit_is_set(5 * 64 + 5));
        // Only selected-color pixels remain, so the target is the same.
        assert_eq!(filtered.result.target_xy, plain.result.target_xy);
        assert_eq!(filtered.result.objects.len(), 1);
    }

    #[test]
    fn object_detection_rejects_crop_size_that_does_not_match_model_input() {
        let frame = SourceFrame {
            frame_id: 1,
            timestamp_ns: 10,
            image: image::RgbImage::new(640, 640),
            source: "test".into(),
            jpeg: Default::default(),
            received: std::time::Instant::now(),
        };
        let model = ModelDefinition {
            id: "test-model".into(),
            name: "Test model".into(),
            artifacts: ModelArtifacts {
                onnx: "model.onnx".into(),
                tensorrt: "model.engine".into(),
                openvino_xml: "model.xml".into(),
                openvino_bin: "model.bin".into(),
            },
            input_width: 320,
            input_height: 320,
            confidence: 0.5,
            iou: 0.45,
            device: "auto".into(),
            enabled: true,
            class_names: None,
        };
        let config = PipelineConfig {
            strategy: DetectionMode::ObjectDetection,
            model: Some(model),
            ..PipelineConfig::default()
        };
        let mut smoother =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();

        let error = process_frame(
            frame,
            &config,
            &mut smoother,
            &mut runtime_cache,
            None,
            false,
            &mut crossing,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ProcessError::ModelResolutionMismatch {
                expected_width: 320,
                expected_height: 320,
                actual_width: 640,
                actual_height: 640
            }
        ));
    }
}
