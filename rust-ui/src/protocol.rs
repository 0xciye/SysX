use crate::settings::{AutoSettings, RecoilSettings};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ConnectionInfo {
    pub host: String,
    pub port: u16,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BackendState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub motion_enabled: bool,
    #[serde(default)]
    pub motion_always_on: bool,
    #[serde(default)]
    pub fov_degrees: u16,
    #[serde(default)]
    pub target_offset_x: f64,
    #[serde(default)]
    pub target_offset_y: f64,
    #[serde(default)]
    pub trigger_button: String,
    #[serde(default)]
    pub smoothing_x_min: f32,
    #[serde(default)]
    pub smoothing_x_max: f32,
    #[serde(default)]
    pub smoothing_y_min: f32,
    #[serde(default)]
    pub smoothing_y_max: f32,
    #[serde(default)]
    pub sens: f64,
    #[serde(
        default,
        rename = "recoil",
        alias = "nativeMotion",
        alias = "native_motion"
    )]
    pub recoil: RecoilSettings,
    #[serde(default)]
    pub auto: AutoSettings,
    #[serde(default)]
    pub preview_scale: String,
    #[serde(default)]
    pub crop_resolution: u32,
    #[serde(default)]
    pub pixel_color: String,
    #[serde(default)]
    pub effective_fov_degrees: u16,
    #[serde(default)]
    pub effective_pixel_color: String,
    #[serde(default)]
    pub auto_trigger_held: bool,
    #[serde(default)]
    pub profile_index: u8,
    #[serde(default = "default_profile_count")]
    pub profile_count: u8,
    #[serde(default)]
    pub profiles: Vec<ProfileSummary>,
    #[serde(default)]
    pub pixel_profile_configured: bool,
    #[serde(default)]
    pub active_model_id: Option<String>,
    #[serde(default)]
    pub processing_fps: Option<f64>,
    #[serde(default)]
    pub source_fps: Option<f64>,
    #[serde(default)]
    pub dropped_input_frames: u64,
    #[serde(default)]
    pub transport_dropped_frames: u64,
    #[serde(default)]
    pub rejected_frames: u64,
    #[serde(default)]
    pub resolution_mismatches: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub device: Option<DeviceStatus>,
}

fn default_profile_count() -> u8 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProfileSummary {
    pub index: usize,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BackendModel {
    pub id: String,
    pub name: String,
    pub artifacts: ModelArtifacts,
    pub input_width: u32,
    pub input_height: u32,
    pub confidence: f32,
    pub iou: f32,
    pub device: String,
    pub enabled: bool,
    #[serde(default)]
    pub class_names: Option<std::collections::HashMap<u32, String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelArtifacts {
    pub onnx: String,
    pub tensorrt: String,
    pub openvino_xml: String,
    pub openvino_bin: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DetectionObject {
    pub class_id: u32,
    pub class_name: String,
    pub bbox_xyxy: [f32; 4],
    pub confidence: Option<f32>,
    #[serde(default)]
    pub pixel_count: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Detection {
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub mode: String,
    pub model_id: Option<String>,
    pub source_width: u32,
    pub source_height: u32,
    pub processed_width: u32,
    pub processed_height: u32,
    pub objects: Vec<DetectionObject>,
    pub total_processing_ms: f64,
    pub target_xy: Option<[f32; 2]>,
    #[serde(default)]
    pub smoothed_target_xy: Option<[f32; 2]>,
    #[serde(default)]
    pub auto_target_valid: bool,
    #[serde(default)]
    pub inference_device: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct LiveFrame {
    pub frame_id: u64,
    pub source: String,
    pub source_width: u32,
    pub source_height: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub mode: String,
    pub model_id: Option<String>,
    pub processing_fps: Option<f64>,
    pub source_fps: Option<f64>,
    pub dropped_input_frames: u64,
    pub transport_dropped_frames: u64,
    pub detection: Detection,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DeviceStatus {
    pub state: String,
    pub name: Option<String>,
    #[serde(default)]
    pub buttons: Vec<Option<bool>>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug)]
pub enum BackendEvent {
    Ready,
    Disconnected,
    State(BackendState),
    CommandResponse {
        request_id: String,
        command_type: String,
        error: Option<String>,
    },
    Models(Vec<BackendModel>, Option<String>),
    Error(String),
}
