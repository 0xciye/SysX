use crate::auto_target::{center_sides, CenterCrossing, CrosshairTracker};
use crate::config::ModelDefinition;
use crate::crop::CropRect;
use crate::inference::{
    decode_end2end_output, decode_yolo_output, to_detection_objects, DetectionRuntime,
};
use crate::pixel_seek::{
    color_mask, color_mask_into, head_point, process_pixel_seek_with_scratch, try_pack_mask,
    PixelSeekScratch, PixelTargetColor,
};
use crate::receiver::SourceFrame;
use crate::results::{DetectionMode, DetectionObject, DetectionResult};
use crate::target_manager::{Detection, TargetKind, TargetManager, TrackedEntity};
use crate::targeting::{
    aim_point, box_has_outline, fov_radius_px, object_detection_candidates, FrameCoordinate,
};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    /// Shared control/routing revision used to reject callbacks and Motion samples
    /// completed before a configuration or Auto trigger transition.
    pub auto_config_revision: u64,
    /// Black out every pixel but the selected target color before Motion,
    /// Auto (and Object Detection) process the frame.
    pub color_filter: bool,
    /// Object Detection aim point inside the target box.
    /// Object Detection: only boxes holding this enemy outline color are
    /// targets (Color swap).
    pub color_swap: Option<PixelTargetColor>,
    /// Min confidence of ability classes (`targeting::ClassRole::Ability`).
    pub ability_confidence: f32,
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
            auto_config_revision: 0,
            color_filter: false,
            color_swap: None,
            ability_confidence: 0.6,
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
    /// The target's box in original-frame coordinates (Motion slows down
    /// while the crosshair is inside it).
    pub target_box: Option<[f32; 4]>,
    /// Where a Magnet trigger press snaps: `result.target_xy`, or while Auto
    /// owns the frame (which publishes no target) the tracked target's head
    /// point.
    pub snap_target: Option<[f32; 2]>,
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
    let mut targets = TargetManager::default();
    let mut targets_generation = None;
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
            // Tracks and Auto crossings are expressed in source pixels.
            targets_generation = None;
        }
        previous_frame_size = Some(frame_size);
        if targets_generation != Some(pending.generation) {
            targets.reset();
            targets_generation = Some(pending.generation);
            crossing.reset();
        }
        let started = Instant::now();
        let processed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_frame_with_scratch(
                pending.frame,
                &config,
                &mut runtime_cache,
                &mut targets,
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
                drop(state);
                if let Some(callback) = &on_processed {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        callback(&processed)
                    }))
                    .is_err()
                    {}
                }
            }
            Err(error) => {
                if matches!(&error, ProcessError::FramePanic(_)) {
                    runtime_cache = None;
                    crossing.reset();
                    targets.reset();
                }
                state.stats.rejected_frames += 1;
                if matches!(error, ProcessError::ModelResolutionMismatch { .. }) {
                    state.stats.resolution_mismatches += 1;
                }
                state.stats.last_error = Some(error.to_string());
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
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
fn process_frame(
    frame: SourceFrame,
    config: &PipelineConfig,
    runtime_cache: &mut Option<RuntimeCache>,
    targets: &mut TargetManager,
    crossing: &mut CenterCrossing,
) -> Result<ProcessedFrame, ProcessError> {
    process_frame_with_scratch(
        frame,
        config,
        runtime_cache,
        targets,
        &mut PixelSeekScratch::default(),
        &mut CrosshairTracker::default(),
        crossing,
    )
}

/// Feeds this frame's detections to the tracker and returns the selected
/// entity inside the FOV. Outside a held Motion/Auto action nothing stays
/// locked, so the next press acquires a fresh target.
fn track_target<'a>(
    targets: &'a mut TargetManager,
    detections: &[Detection],
    frame: &SourceFrame,
    size: (u32, u32),
    fov_degrees: u16,
    lock_active: bool,
    prefer_head: bool,
) -> Option<&'a TrackedEntity> {
    targets.update(detections, frame.received);
    if !lock_active {
        targets.release_lock();
    }
    let crosshair = (size.0 as f32 / 2.0, size.1 as f32 / 2.0);
    targets.select_target(
        crosshair,
        fov_radius_px(size.0, size.1, fov_degrees),
        prefer_head,
    )
}

fn process_frame_with_scratch(
    frame: SourceFrame,
    config: &PipelineConfig,
    runtime_cache: &mut Option<RuntimeCache>,
    targets: &mut TargetManager,
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
        // Abilities may have a lower Min confidence than the other classes.
        let decode_threshold = model.confidence.min(config.ability_confidence);
        let boxes = if model.end2end {
            decode_end2end_output(&inference.shape, &inference.values, decode_threshold)
        } else {
            decode_yolo_output(
                &inference.shape,
                &inference.values,
                decode_threshold,
                model.iou,
            )
        }
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
            fov_degrees: config.fov_degrees,
        };
        let mut candidates = object_detection_candidates(
            &result,
            model.confidence,
            config.ability_confidence,
            model.class_names.as_ref(),
        );
        // Color swap: enemies carry the selected outline color, teammates do
        // not. Checked on the original image, before any Color Filter.
        if let Some(color) = config.color_swap {
            let outline = color_mask(&frame.image, color);
            let (width, height) = frame.image.dimensions();
            candidates.retain(|detection| {
                box_has_outline(&outline, width, height, detection.bbox_xyxy)
            });
        }
        let selected = track_target(
            targets,
            &candidates,
            &frame,
            (processed.width(), processed.height()),
            config.fov_degrees,
            target_lock_active,
            true,
        );
        let target_box = selected.map(TrackedEntity::bbox_xyxy);
        result.target_xy = selected.map(|entity| {
            let point = aim_point(entity.bbox_xyxy(), entity.kind);
            [point.x, point.y]
        });
        result.smoothed_target_xy = result.target_xy;
        result.postprocess_ms = postprocess_started.elapsed().as_secs_f64() * 1000.0;
        result.total_processing_ms = frame_started.elapsed().as_secs_f64() * 1000.0;
        let output_mask = match filter_mask.as_deref() {
            Some(mask) => Some(try_pack_mask(mask).ok_or(ProcessError::ColorMaskAllocationFailed)?),
            None => None,
        };
        let snap_target = result.target_xy;
        return Ok(ProcessedFrame {
            auto_config_revision: config.auto_config_revision,
            frame: SourceFrame { ..frame },
            result,
            published_ns: monotonic_ns(),
            crop_x: rect.x,
            crop_y: rect.y,
            target_box,
            snap_target,
            mask: output_mask,
        });
    }
    let started = Instant::now();
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
    // Auto fires from the mask around the center; the target is still
    // tracked so a Magnet trigger press in the Auto mode can snap to it.
    let mut output = process_pixel_seek_with_scratch(
        processed,
        rect,
        config.pixel_color,
        mask,
        pixel_seek_scratch,
    );
    let selected = {
        let clusters: Vec<Detection> = output
            .objects
            .iter()
            .map(|object| Detection {
                bbox_xyxy: object.bbox_xyxy,
                class_id: None,
                confidence: None,
                kind: TargetKind::Body,
            })
            .collect();
        track_target(
            targets,
            &clusters,
            &frame,
            (processed.width(), processed.height()),
            config.fov_degrees,
            target_lock_active,
            false,
        )
        .map(TrackedEntity::bbox_xyxy)
    };
    // Only the targeted cluster is reported; Auto frames report none.
    let snap_target = selected.map(|bbox_xyxy| {
        let point: FrameCoordinate = head_point(bbox_xyxy);
        [point.x, point.y]
    });
    let selected = selected.filter(|_| !config.auto_target_active);
    let (class_id, class_name) = config.pixel_color.class();
    output.objects = selected
        .map(|bbox_xyxy| DetectionObject {
            class_id,
            class_name: class_name.into(),
            bbox_xyxy,
            confidence: None,
            pixel_count: None,
        })
        .into_iter()
        .collect();
    // Measured on the original image: the filter blacks the crosshair out.
    crosshair.update(&frame.image);
    let crosshair_px = crosshair.radius_px();
    let detection_ms = started.elapsed().as_secs_f64() * 1000.0;
    let target_xy = selected.map(|bbox_xyxy| {
        let point: FrameCoordinate = head_point(bbox_xyxy);
        [point.x, point.y]
    });
    let auto_target_valid = if config.auto_target_active {
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
        smoothed_target_xy: target_xy,
        auto_target_valid,
        auto_active: config.auto_target_active,
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
        target_box: selected,
        snap_target,
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
            motion_toggle: false,
            sens: 0.25,
            offset_x: 0.0,
            offset_y: 0.0,
            speed_x: crate::movement::motion::DEFAULT_SPEED,
            speed_y: crate::movement::motion::DEFAULT_SPEED,
            smoothing_x: crate::targeting::SmoothingRange::NONE,
            smoothing_y: crate::targeting::SmoothingRange::NONE,
            smooth_fov_degrees: 20,
            booster: None,
            no_deadzone: false,
            trigger_button: "right".into(),
            trigger_held: true,
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
                        target_box: processed.target_box.map(|b| b.map(f64::from)),
                        crop_origin: (processed.crop_x, processed.crop_y),
                        processed_size: (
                            processed.result.processed_width,
                            processed.result.processed_height,
                        ),
                        received_s: 0.0,
                        age_s: processed.frame.received.elapsed().as_secs_f64(),
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
        let mut targets = TargetManager::default();
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
            for y in 30..34 {
                for x in 38..42 {
                    image.put_pixel(x, y, image::Rgb([255, 0, 0]));
                }
            }
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
                &mut cache,
                &mut targets,
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
        let mut targets = TargetManager::default();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();
        let processed = process_frame(
            frame,
            &config,
            &mut runtime_cache,
            &mut targets,
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
    fn pixel_seek_aims_at_the_head_of_the_tracked_cluster_and_bridges_a_lost_frame() {
        let config = PipelineConfig {
            pixel_color: PixelTargetColor::Red,
            fov_degrees: 360,
            ..PipelineConfig::default()
        };
        let mut targets = TargetManager::default();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();
        let mut image = image::RgbImage::new(64, 64);
        for y in 20..40 {
            for x in 27..37 {
                image.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let start = Instant::now();
        let mut run = |frame_id, image: &image::RgbImage, received| {
            process_frame(
                SourceFrame {
                    frame_id,
                    timestamp_ns: 0,
                    image: image.clone(),
                    source: "test".into(),
                    jpeg: Default::default(),
                    received,
                },
                &config,
                &mut runtime_cache,
                &mut targets,
                &mut crossing,
            )
            .unwrap()
        };
        let seen = run(1, &image, start);
        assert_eq!(seen.result.objects.len(), 1);
        assert_eq!(seen.target_box, Some([27.0, 20.0, 37.0, 40.0]));
        let target = seen.result.target_xy.expect("target");
        assert_eq!(target[0], 32.0);
        assert!((target[1] - 22.8).abs() < 1e-4, "14 % below the top");
        // A frame without the target inside the grace period keeps it.
        let lost = run(
            2,
            &image::RgbImage::new(64, 64),
            start + Duration::from_millis(50),
        );
        assert_eq!(lost.result.target_xy, seen.result.target_xy);
        let gone = run(
            3,
            &image::RgbImage::new(64, 64),
            start + Duration::from_millis(200),
        );
        assert!(gone.result.target_xy.is_none() && gone.target_box.is_none());
    }

    #[test]
    fn color_filter_runs_before_detection_and_hands_its_mask_to_preview() {
        let mut image = image::RgbImage::from_pixel(64, 64, image::Rgb([40, 90, 200]));
        for y in 20..24 {
            for x in 30..34 {
                image.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        image.put_pixel(5, 5, image::Rgb([250, 250, 250]));
        let run = |color_filter: bool| {
            let config = PipelineConfig {
                pixel_color: PixelTargetColor::Red,
                fov_degrees: 360,
                color_filter,
                ..PipelineConfig::default()
            };
            let mut targets = TargetManager::default();
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
                &mut None,
                &mut targets,
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
        assert_eq!(mask.iter().map(|byte| byte.count_ones()).sum::<u32>(), 16);
        assert!(bit_is_set(20 * 64 + 30));
        assert!(!bit_is_set(5 * 64 + 5));
        // Only selected-color pixels remain, so the target is the same.
        assert!(plain.result.target_xy.is_some());
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
            end2end: false,
        };
        let config = PipelineConfig {
            strategy: DetectionMode::ObjectDetection,
            model: Some(model),
            ..PipelineConfig::default()
        };
        let mut targets = TargetManager::default();
        let mut runtime_cache = None;
        let mut crossing = CenterCrossing::default();

        let error = process_frame(
            frame,
            &config,
            &mut runtime_cache,
            &mut targets,
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
