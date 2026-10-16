use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DetectionMode {
    PixelSeek,
    ObjectDetection,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DetectionObject {
    pub class_id: u32,
    pub class_name: String,
    pub bbox_xyxy: [f32; 4],
    pub confidence: Option<f32>,
    pub pixel_count: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DetectionResult {
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub mode: DetectionMode,
    pub model_id: Option<String>,
    pub source_width: u32,
    pub source_height: u32,
    pub processed_width: u32,
    pub processed_height: u32,
    pub objects: Vec<DetectionObject>,
    pub preprocess_ms: f64,
    pub detection_ms: f64,
    pub postprocess_ms: f64,
    pub total_processing_ms: f64,
    pub inference_device: Option<String>,
    pub target_xy: Option<[f32; 2]>,
    #[serde(default)]
    pub smoothed_target_xy: Option<[f32; 2]>,
    /// Auto's presence gate: an active-color pixel must occupy the
    /// screen-center cell inside Auto FOV (the crosshair must touch it).
    #[serde(default)]
    pub auto_target_valid: bool,
    /// Frame-level routing intent, independent of center-pixel validity.
    #[serde(default)]
    pub auto_active: bool,
    /// FOV used for this frame's active feature and preview circle.
    #[serde(default)]
    pub fov_degrees: u16,
}

impl DetectionResult {
    pub fn is_valid(&self) -> bool {
        self.source_width > 0
            && self.source_height > 0
            && self.processed_width > 0
            && self.processed_height > 0
            && [
                self.preprocess_ms,
                self.detection_ms,
                self.postprocess_ms,
                self.total_processing_ms,
            ]
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
            && self.objects.iter().all(|object| {
                object.bbox_xyxy.iter().all(|value| value.is_finite())
                    && object
                        .confidence
                        .is_none_or(|score| score.is_finite() && (0.0..=1.0).contains(&score))
            })
            && self
                .target_xy
                .is_none_or(|point| point.iter().all(|value| value.is_finite()))
    }
}
