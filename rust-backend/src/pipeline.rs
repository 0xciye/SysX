use crate::auto_target::{center_sides, CenterCrossing, CrosshairTracker};
use crate::config::ModelDefinition;
use crate::crop::CropRect;
use crate::inference::{decode_yolo_output, to_detection_objects, DetectionRuntime};
use crate::pixel_seek::{color_mask, process_pixel_seek, PixelSeekOutput, PixelTargetColor};
use crate::receiver::SourceFrame;
use crate::results::{DetectionMode, DetectionResult};
use crate::targeting::{
    object_detection_candidates, object_detection_target, ExponentialTargetSmoother,
    FrameCoordinate, PriorTarget, SmoothingRange, TargetCandidate,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub strategy: DetectionMode,
    pub pixel_color: PixelTargetColor,
    pub crop_resolution: Option<u32>,
    pub fov_degrees: u16,
    pub auto_fov_degrees: u16,
    /// True only while the enabled Auto trigger is held. In that state the
    /// Pixel Seek mask feeds only Auto's center-crossing validity gate.
    pub auto_target_active: bool,
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
    /// Color filter on: the mask it applied (nonzero = selected color,
    /// processed size, at `crop_x/crop_y`), so Preview shows the same image.
    pub mask: Option<Vec<u8>>,
    /// Measured crosshair radius in pixels (0 until measured).
    pub crosshair_px: f32,
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
    config: PipelineConfig,
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

pub struct LatestFramePipeline {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LatestFramePipeline {
    pub fn new(
        config: PipelineConfig,
        on_processed: Option<Arc<dyn Fn(&ProcessedFrame) + Send + Sync>>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                config,
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
            .spawn(move || processing_loop(worker_shared, on_processed))
            .expect("processing worker thread");
        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn submit(&self, frame: SourceFrame) -> Result<(), &'static str> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| "pipeline state lock poisoned")?;
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
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| "pipeline state lock poisoned")?;
        state.config = config;
        state.generation += 1;
        state.latest = None;
        if state.pending.take().is_some() {
            state.stats.dropped_input_frames += 1;
        }
        self.shared.wake.notify_all();
        Ok(())
    }
    pub fn latest(&self) -> Option<Arc<ProcessedFrame>> {
        self.shared.state.lock().ok()?.latest.clone()
    }
    pub fn stats(&self) -> PipelineStats {
        let Ok(mut state) = self.shared.state.lock() else {
            return PipelineStats::default();
        };
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
        if let Ok(mut state) = self.shared.state.lock() {
            state.stopping = true;
            state.pending = None;
            self.shared.wake.notify_all();
        }
        let Ok(mut slot) = self.worker.lock() else {
            return false;
        };
        let Some(worker) = slot.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return false;
        }
        worker.join().is_ok()
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
    let mut runtime_cache = None;
    // Not reset on reconfiguration: the game's crosshair stays the same.
    let mut crosshair = CrosshairTracker::default();
    let mut crossing = CenterCrossing::default();
    loop {
        let (pending, config) = {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            while state.pending.is_none() && !state.stopping {
                let Ok(next) = shared.wake.wait(state) else {
                    return;
                };
                state = next;
            }
            if state.stopping {
                return;
            }
            let pending = state.pending.take().expect("pending input frame");
            (pending, state.config.clone())
        };
        if smoother_generation != Some(pending.generation) {
            smoother = ExponentialTargetSmoother::new(config.smoothing_x, config.smoothing_y);
            smoother_generation = Some(pending.generation);
            prior_target = None;
            prior_target_at = None;
        }
        let Some(smoother) = smoother.as_mut() else {
            return;
        };
        let started = Instant::now();
        let prior_is_fresh = prior_target_at
            .is_some_and(|timestamp| timestamp.elapsed() <= Duration::from_millis(100));
        let processed = process_frame(
            pending.frame,
            &config,
            smoother,
            &mut runtime_cache,
            prior_target.as_ref(),
            prior_is_fresh,
            &mut crosshair,
            &mut crossing,
        );
        let finished_ns = monotonic_ns();
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
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
                        prior_target_at = Some(Instant::now());
                    } else {
                        prior_target = None;
                        prior_target_at = None;
                    }
                } else {
                    // Pixel Seek reports only its locked target's blob.
                    prior_target = processed.result.objects.first().map(|object| PriorTarget {
                        bbox_xyxy: Some(object.bbox_xyxy),
                        class_id: None,
                    });
                    prior_target_at = prior_target.as_ref().map(|_| Instant::now());
                }
                drop(state);
                if let Some(callback) = &on_processed {
                    callback(&processed);
                }
            }
            Err(error) => {
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

fn process_frame(
    frame: SourceFrame,
    config: &PipelineConfig,
    smoother: &mut ExponentialTargetSmoother,
    runtime_cache: &mut Option<RuntimeCache>,
    previous_target: Option<&PriorTarget>,
    previous_is_fresh: bool,
    crosshair: &mut CrosshairTracker,
    crossing: &mut CenterCrossing,
) -> Result<ProcessedFrame, ProcessError> {
    let frame_started = Instant::now();
    // The OBS plugin is the sole owner of capture cropping.  The receiver
    // must process the JPEG exactly as received; applying another centered
    // crop here would double-crop the image and waste a full-frame copy.
    let source_width = frame.image.width();
    let source_height = frame.image.height();
    let rect = CropRect {
        x: 0,
        y: 0,
        width: source_width,
        height: source_height,
    };
    let color_filtered = config
        .color_filter
        .then(|| crate::pixel_seek::color_filtered(&frame.image, config.pixel_color));
    let processed = color_filtered
        .as_ref()
        .map_or(&frame.image, |(image, _)| image);
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
            previous_target,
            previous_is_fresh,
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
        return Ok(ProcessedFrame {
            auto_config_revision: config.auto_config_revision,
            frame: SourceFrame { ..frame },
            result,
            published_ns: monotonic_ns(),
            crop_x: rect.x,
            crop_y: rect.y,
            selected_target,
            smoothing_alpha: smoother.alphas(),
            mask: color_filtered.map(|(_, mask)| mask),
            crosshair_px: 0.0,
        });
    }
    let started = Instant::now();
    let locked = previous_target
        .filter(|_| previous_is_fresh)
        .and_then(|previous| previous.bbox_xyxy);
    // Auto owns this frame while active. Never run Motion selection or retain
    // its coordinates/lock during Auto, even when no pixel touches the center;
    // Auto's aim mode selects its own target inside Auto FOV.
    let auto_aim = config.auto_target_active && config.auto_aim;
    let output = if config.auto_target_active && !auto_aim {
        PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask: color_mask(processed, config.pixel_color),
        }
    } else {
        process_pixel_seek(
            processed,
            rect,
            config.pixel_color,
            if auto_aim {
                config.auto_fov_degrees
            } else {
                config.fov_degrees
            },
            locked,
        )
    };
    // Measured on the original image: the filter blacks the crosshair out.
    crosshair.update(&frame.image);
    let crosshair_px = crosshair.radius_px();
    let detection_ms = started.elapsed().as_secs_f64() * 1000.0;
    let target_xy = output.target.map(|point| [point.x, point.y]);
    // Aim mode fires on the target it aims at, as Motion selects it inside
    // Auto FOV; center-crossing mode needs the target to pass the center.
    let auto_target_valid = if auto_aim {
        crossing.reset();
        target_xy.is_some()
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
        mask: config.color_filter.then_some(output.mask),
        crosshair_px,
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
    match cache.as_mut().expect("runtime cache initialized") {
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
                &mut Default::default(),
                &mut Default::default(),
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
            result.auto_target_valid,
            "a valid target fires, off center too"
        );
        assert_eq!(result.fov_degrees, 20);

        // Only the pixel outside Auto FOV: no target, no fire.
        let mut image = image::RgbImage::new(64, 64);
        image.put_pixel(32, 2, image::Rgb([255, 0, 0]));
        let result = run(image, 2, &mut smoother, &mut cache);
        assert_eq!(result.target_xy, None);
        assert!(!result.auto_target_valid);
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
        let processed = process_frame(
            frame,
            &config,
            &mut smoother,
            &mut runtime_cache,
            None,
            false,
            &mut Default::default(),
            &mut Default::default(),
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
                &mut Default::default(),
                &mut Default::default(),
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
                &mut Default::default(),
                &mut Default::default(),
            )
            .unwrap()
        };
        let plain = run(false);
        let filtered = run(true);
        assert!(plain.mask.is_none(), "no mask without the filter");
        let mask = filtered.mask.as_ref().expect("filter mask");
        assert_eq!(mask.len(), 64 * 64);
        assert_eq!(mask.iter().filter(|value| **value != 0).count(), 4);
        assert_ne!(mask[20 * 64 + 30], 0);
        assert_eq!(mask[5 * 64 + 5], 0);
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

        let error = process_frame(
            frame,
            &config,
            &mut smoother,
            &mut runtime_cache,
            None,
            false,
            &mut Default::default(),
            &mut Default::default(),
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
