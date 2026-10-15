use crate::config::ModelDefinition;
use crate::crop::{centered_crop, CropError};
use crate::inference::{decode_yolo_output, to_detection_objects, DetectionRuntime};
use crate::pixel_seek::{process_pixel_seek, PixelTargetColor};
use crate::receiver::SourceFrame;
use crate::results::{DetectionMode, DetectionResult};
use crate::storage::DetectionDataStore;
use crate::targeting::{
    object_detection_candidates, object_detection_target, ExponentialTargetSmoother,
    FrameCoordinate, PriorTarget, TargetCandidate,
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
    pub smoothing_alpha_x: f32,
    pub smoothing_alpha_y: f32,
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
            smoothing_alpha_x: 1.0,
            smoothing_alpha_y: 1.0,
            model: None,
            model_backend: "onnxruntime".into(),
            compute_target: "cpu".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessedFrame {
    pub frame: SourceFrame,
    pub result: DetectionResult,
    pub published_ns: u64,
    pub crop_x: u32,
    pub crop_y: u32,
    pub selected_target: Option<TargetCandidate>,
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
    latest: Option<ProcessedFrame>,
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
        store: Option<Arc<DetectionDataStore>>,
        on_processed: Option<Arc<dyn Fn(ProcessedFrame) + Send + Sync>>,
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
            .spawn(move || processing_loop(worker_shared, store, on_processed))
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
    pub fn latest(&self) -> Option<ProcessedFrame> {
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
    store: Option<Arc<DetectionDataStore>>,
    on_processed: Option<Arc<dyn Fn(ProcessedFrame) + Send + Sync>>,
) {
    let mut smoother = None;
    let mut smoother_generation = None;
    let mut prior_target: Option<PriorTarget> = None;
    let mut prior_target_at: Option<Instant> = None;
    let mut runtime_cache = None;
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
            smoother =
                ExponentialTargetSmoother::new(config.smoothing_alpha_x, config.smoothing_alpha_y);
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
                state.latest = Some(processed.clone());
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
                    prior_target = None;
                    prior_target_at = None;
                }
                drop(state);
                if let Some(callback) = &on_processed {
                    callback(processed.clone());
                }
                if let Some(store) = &store {
                    if !store.submit(processed.result) {
                        eprintln!(
                            "Detection result was not persisted (store queue full or closed)"
                        );
                    }
                }
            }
            Err(error) => {
                state.stats.rejected_frames += 1;
                if matches!(
                    error,
                    ProcessError::Crop(_) | ProcessError::ModelResolutionMismatch { .. }
                ) {
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
) -> Result<ProcessedFrame, ProcessError> {
    let frame_started = Instant::now();
    let source_width = frame.image.width();
    let source_height = frame.image.height();
    let (cropped, rect) = centered_crop(&frame.image, config.crop_resolution)?;
    let crop_ms = frame_started.elapsed().as_secs_f64() * 1000.0;
    if config.strategy == DetectionMode::ObjectDetection {
        let model = config.model.as_ref().ok_or_else(|| {
            ProcessError::ObjectDetectionUnavailable("select a model bundle".into())
        })?;
        if cropped.width() != model.input_width || cropped.height() != model.input_height {
            return Err(ProcessError::ModelResolutionMismatch {
                expected_width: model.input_width,
                expected_height: model.input_height,
                actual_width: cropped.width(),
                actual_height: cropped.height(),
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
            .infer(&cropped)
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
            processed_width: cropped.width(),
            processed_height: cropped.height(),
            objects,
            preprocess_ms: crop_ms + inference.preprocess_ms,
            detection_ms: inference.inference_ms,
            postprocess_ms: 0.0,
            total_processing_ms: 0.0,
            inference_device: Some(inference.device),
            target_xy: None,
            smoothed_target_xy: None,
        };
        let candidates = object_detection_candidates(&result, model.confidence);
        let selected_target = object_detection_target(
            &candidates,
            cropped.width(),
            cropped.height(),
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
            frame: SourceFrame {
                image: cropped,
                ..frame
            },
            result,
            published_ns: monotonic_ns(),
            crop_x: rect.x,
            crop_y: rect.y,
            selected_target,
        });
    }
    let started = Instant::now();
    let output = process_pixel_seek(&cropped, rect, config.pixel_color, config.fov_degrees);
    let detection_ms = started.elapsed().as_secs_f64() * 1000.0;
    let target_xy = output.target.map(|point| [point.x, point.y]);
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
        processed_width: cropped.width(),
        processed_height: cropped.height(),
        objects: output.objects,
        preprocess_ms: crop_ms,
        detection_ms,
        postprocess_ms: 0.0,
        total_processing_ms: total,
        inference_device: Some("cpu".into()),
        target_xy,
        smoothed_target_xy: filtered.map(|point| [point.x, point.y]),
    };
    Ok(ProcessedFrame {
        frame: SourceFrame {
            image: cropped,
            ..frame
        },
        result,
        published_ns: monotonic_ns(),
        crop_x: rect.x,
        crop_y: rect.y,
        selected_target: None,
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
    if !(1..=360).contains(&config.fov_degrees) {
        return Err("FOV must be 1..360");
    }
    if ExponentialTargetSmoother::new(config.smoothing_alpha_x, config.smoothing_alpha_y).is_none()
    {
        return Err("smoothing alpha must be 0.01..1");
    }
    if config.crop_resolution == Some(0) {
        return Err("crop resolution must be positive");
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ProcessError {
    #[error(transparent)]
    Crop(#[from] CropError),
    #[error("model resolution {expected_width}x{expected_height} does not match cropped frame {actual_width}x{actual_height}")]
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
    fn object_detection_rejects_crop_size_that_does_not_match_model_input() {
        let frame = SourceFrame {
            frame_id: 1,
            timestamp_ns: 10,
            image: image::RgbImage::new(640, 640),
            source: "test".into(),
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
        let mut smoother = ExponentialTargetSmoother::new(1.0, 1.0).unwrap();
        let mut runtime_cache = None;

        let error = process_frame(
            frame,
            &config,
            &mut smoother,
            &mut runtime_cache,
            None,
            false,
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
