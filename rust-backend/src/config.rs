use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct RecoilSettings {
    pub recoil_enabled: bool,
    pub recoil_levels: Vec<i32>,
    pub recoil_start_delay_ms: u32,
    pub recoil_level_duration_ms: u32,
    pub recoil_percentage: u8,
    pub recoil_allow_early_resample: bool,
}

impl Default for RecoilSettings {
    fn default() -> Self {
        Self {
            recoil_enabled: false,
            recoil_levels: Vec::new(),
            recoil_start_delay_ms: 0,
            recoil_level_duration_ms: 0,
            recoil_percentage: 0,
            recoil_allow_early_resample: false,
        }
    }
}

impl RecoilSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.recoil_levels.len() > 64
            || self
                .recoil_levels
                .iter()
                .any(|value| !(-10_000..=10_000).contains(value))
        {
            return Err("recoil levels must contain at most 64 values from -10000 to 10000");
        }
        if self.recoil_start_delay_ms > 60_000 || self.recoil_level_duration_ms > 5_000 {
            return Err("recoil delay must be 0..60000 ms and level duration 0..5000 ms");
        }
        if self.recoil_enabled
            && !self.recoil_levels.is_empty()
            && self.recoil_level_duration_ms == 0
        {
            return Err("recoil level duration must be greater than 0 ms");
        }
        if self.recoil_percentage > 100 {
            return Err("recoil percentage must be 0..100");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    pub trigger_button: String,
    pub strategy: String,
    pub fov_degrees: u16,
    pub offset_x: f64,
    pub offset_y: f64,
    pub smoothing_alpha_x: f32,
    pub smoothing_alpha_y: f32,
    pub sens: f64,
    pub dpi: u32,
    pub scale: String,
    pub show_preview: bool,
    pub crop_resolution: u32,
    pub port: String,
    pub pixel_color: String,
    pub active_model_id: Option<String>,
    pub model_width: String,
    pub model_height: String,
    pub model_confidence: String,
    pub model_iou: String,
    pub model_backend: String,
    pub model_precision: String,
    pub compute_target: String,
    #[serde(rename = "recoil", alias = "nativeMotion", alias = "native_motion")]
    pub recoil: RecoilSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            trigger_button: "right".into(),
            strategy: "pixel_seek".into(),
            fov_degrees: 36,
            offset_x: 0.0,
            offset_y: 0.0,
            smoothing_alpha_x: 1.0,
            smoothing_alpha_y: 1.0,
            sens: 0.25,
            dpi: 1600,
            scale: "Smooth".into(),
            show_preview: true,
            crop_resolution: 640,
            port: "4200".into(),
            pixel_color: "yellow".into(),
            active_model_id: None,
            model_width: "640".into(),
            model_height: "640".into(),
            model_confidence: "0.5".into(),
            model_iou: "0.45".into(),
            model_backend: "onnxruntime".into(),
            model_precision: "fp16".into(),
            compute_target: "cpu".into(),
            recoil: RecoilSettings::default(),
        }
    }
}

impl AppSettings {
    pub fn validate(&mut self) -> Result<(), SettingsError> {
        if !["right", "middle", "m4", "m5"].contains(&self.trigger_button.as_str()) {
            return Err(SettingsError::Invalid("invalid trigger button"));
        }
        if !["pixel_seek", "object_detection"].contains(&self.strategy.as_str()) {
            return Err(SettingsError::Invalid("invalid strategy"));
        }
        if !(1..=360).contains(&self.fov_degrees) {
            return Err(SettingsError::Invalid("FOV must be 1..360"));
        }
        if !self.offset_x.is_finite() || !self.offset_y.is_finite() {
            return Err(SettingsError::Invalid("offsets must be finite"));
        }
        if ![self.smoothing_alpha_x, self.smoothing_alpha_y]
            .iter()
            .all(|a| a.is_finite() && (0.01..=1.0).contains(a))
        {
            return Err(SettingsError::Invalid("smoothing alpha must be 0.01..1"));
        }
        if !self.sens.is_finite() || !(0.01..=10.0).contains(&self.sens) {
            return Err(SettingsError::Invalid("sensitivity must be 0.01..10"));
        }
        if !(100..=100_000).contains(&self.dpi) {
            return Err(SettingsError::Invalid("DPI must be 100..100000"));
        }
        if !["Smooth", "Pixel perfect"].contains(&self.scale.as_str()) {
            return Err(SettingsError::Invalid("invalid preview scale"));
        }
        let port = self
            .port
            .parse::<u16>()
            .map_err(|_| SettingsError::Invalid("UDP port must be 1..65535"))?;
        if port == 0 || self.port.is_empty() || !self.port.bytes().all(|c| c.is_ascii_digit()) {
            return Err(SettingsError::Invalid("UDP port must be 1..65535"));
        }
        if !["yellow", "red", "purple"].contains(&self.pixel_color.as_str()) {
            return Err(SettingsError::Invalid("invalid Pixel Seek color"));
        }
        if !["onnxruntime", "tensorrt", "openvino"].contains(&self.model_backend.as_str()) {
            return Err(SettingsError::Invalid("invalid inference backend"));
        }
        if !["fp16", "int8"].contains(&self.model_precision.as_str()) {
            return Err(SettingsError::Invalid("invalid inference precision"));
        }
        if !["cpu", "gpu", "npu"].contains(&self.compute_target.as_str()) {
            return Err(SettingsError::Invalid("invalid inference compute target"));
        }
        if !(1..=8192).contains(&self.crop_resolution) {
            return Err(SettingsError::Invalid("crop resolution must be 1..8192"));
        }
        if self
            .active_model_id
            .as_ref()
            .is_some_and(|id| id.is_empty())
        {
            return Err(SettingsError::Invalid("active model ID cannot be empty"));
        }
        self.recoil.validate().map_err(SettingsError::Invalid)?;
        self.show_preview = true;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn load_settings(path: impl AsRef<Path>) -> AppSettings {
    let Ok(raw) = fs::read_to_string(path) else {
        return AppSettings::default();
    };
    let Ok(mut value) = serde_json::from_str::<AppSettings>(&raw) else {
        return AppSettings::default();
    };
    if value.validate().is_err() {
        AppSettings::default()
    } else {
        value
    }
}

pub fn save_settings(path: impl AsRef<Path>, settings: &AppSettings) -> Result<(), SettingsError> {
    let mut settings = settings.clone();
    settings.validate()?;
    atomic_write(path.as_ref(), &serde_json::to_vec(&settings)?)?;
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelArtifacts {
    pub onnx: String,
    pub tensorrt: String,
    pub openvino_xml: String,
    pub openvino_bin: String,
}

impl ModelArtifacts {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !has_extension(&self.onnx, &["onnx"])
            || !has_extension(&self.tensorrt, &["engine", "plan"])
            || !has_extension(&self.openvino_xml, &["xml"])
            || !has_extension(&self.openvino_bin, &["bin"])
        {
            return Err(
                "model bundle requires ONNX, TensorRT engine, and OpenVINO XML/BIN artifacts",
            );
        }
        Ok(())
    }

    pub fn for_backend(&self, backend: &str) -> Result<&str, &'static str> {
        self.validate()?;
        match backend {
            "onnxruntime" => Ok(&self.onnx),
            "tensorrt" => Ok(&self.tensorrt),
            "openvino" => Ok(&self.openvino_xml),
            _ => Err("invalid inference backend"),
        }
    }

    pub fn paths(&self) -> [&str; 4] {
        [
            &self.onnx,
            &self.tensorrt,
            &self.openvino_xml,
            &self.openvino_bin,
        ]
    }
}

fn has_extension(path: &str, allowed: &[&str]) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            allowed
                .iter()
                .any(|value| extension.eq_ignore_ascii_case(value))
        })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    pub name: String,
    pub artifacts: ModelArtifacts,
    pub input_width: u32,
    pub input_height: u32,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default = "default_iou")]
    pub iou: f32,
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub class_names: Option<HashMap<u32, String>>,
}
fn default_confidence() -> f32 {
    0.5
}
fn default_iou() -> f32 {
    0.45
}
fn default_device() -> String {
    "auto".into()
}
fn default_enabled() -> bool {
    true
}

impl ModelDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err("model ID and name are required");
        }
        self.artifacts.validate()?;
        if !(32..=8192).contains(&self.input_width) || !(32..=8192).contains(&self.input_height) {
            return Err("model dimensions must be 32..8192");
        }
        if !self.confidence.is_finite()
            || !self.iou.is_finite()
            || !(0.0..=1.0).contains(&self.confidence)
            || !(0.0..=1.0).contains(&self.iou)
        {
            return Err("confidence and IoU must be 0..1");
        }
        if !["auto", "cpu", "cuda", "mps"].contains(&self.device.as_str()) {
            return Err("invalid model device");
        }
        Ok(())
    }

    pub fn artifact_for_backend(&self, backend: &str) -> Result<&str, &'static str> {
        self.artifacts.for_backend(backend)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    active_model_id: Option<String>,
    #[serde(default)]
    models: Vec<ModelDefinition>,
}

pub struct ModelRegistry {
    path: PathBuf,
    pub models: HashMap<String, ModelDefinition>,
    pub active_model_id: Option<String>,
}

impl ModelRegistry {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        let mut registry = Self {
            path,
            models: HashMap::new(),
            active_model_id: None,
        };
        if registry.path.exists() {
            let file: RegistryFile = serde_yaml::from_slice(&fs::read(&registry.path)?)?;
            for model in file.models {
                model.validate().map_err(RegistryError::Invalid)?;
                if registry.models.insert(model.id.clone(), model).is_some() {
                    return Err(RegistryError::Invalid("duplicate model ID"));
                }
            }
            if file
                .active_model_id
                .as_ref()
                .is_some_and(|id| !registry.models.contains_key(id))
            {
                return Err(RegistryError::Invalid("active model is missing"));
            }
            registry.active_model_id = file.active_model_id;
        }
        Ok(registry)
    }
    pub fn save(&self) -> Result<(), RegistryError> {
        let mut file = RegistryFile {
            active_model_id: self.active_model_id.clone(),
            models: self.models.values().cloned().collect(),
        };
        file.models.sort_by(|a, b| a.id.cmp(&b.id));
        atomic_write(&self.path, &serde_yaml::to_string(&file)?.into_bytes())?;
        Ok(())
    }
    pub fn add(&mut self, model: ModelDefinition) -> Result<(), RegistryError> {
        model.validate().map_err(RegistryError::Invalid)?;
        if self.models.contains_key(&model.id) {
            return Err(RegistryError::Invalid("model ID already exists"));
        }
        self.models.insert(model.id.clone(), model);
        self.save()
    }
    pub fn update(&mut self, model: ModelDefinition) -> Result<(), RegistryError> {
        model.validate().map_err(RegistryError::Invalid)?;
        if !self.models.contains_key(&model.id) {
            return Err(RegistryError::Missing);
        }
        self.models.insert(model.id.clone(), model);
        self.save()
    }
    pub fn remove(&mut self, id: &str) -> Result<(), RegistryError> {
        if self.models.remove(id).is_none() {
            return Err(RegistryError::Missing);
        }
        if self.active_model_id.as_deref() == Some(id) {
            self.active_model_id = None;
        }
        self.save()
    }
    pub fn activate(&mut self, id: Option<&str>) -> Result<(), RegistryError> {
        if id.is_some_and(|id| !self.models.contains_key(id)) {
            return Err(RegistryError::Missing);
        }
        self.active_model_id = id.map(str::to_owned);
        self.save()
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid model registry: {0}")]
    Invalid(&'static str),
    #[error("model not found")]
    Missing,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|v| v.to_str()).unwrap_or("data"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_artifacts() -> ModelArtifacts {
        ModelArtifacts {
            onnx: "/models/model.onnx".into(),
            tensorrt: "/models/model.plan".into(),
            openvino_xml: "/models/model.xml".into(),
            openvino_bin: "/models/model.bin".into(),
        }
    }

    #[test]
    fn selects_the_artifact_matching_the_backend() {
        let artifacts = valid_artifacts();
        assert_eq!(
            artifacts.for_backend("onnxruntime"),
            Ok("/models/model.onnx")
        );
        assert_eq!(artifacts.for_backend("tensorrt"), Ok("/models/model.plan"));
        assert_eq!(artifacts.for_backend("openvino"), Ok("/models/model.xml"));
        assert_eq!(
            artifacts.for_backend("other"),
            Err("invalid inference backend")
        );
    }

    #[test]
    fn rejects_incomplete_or_mislabeled_bundles() {
        let mut artifacts = valid_artifacts();
        artifacts.openvino_bin.clear();
        assert!(artifacts.validate().is_err());
        artifacts = valid_artifacts();
        artifacts.tensorrt = "/models/model.onnx".into();
        assert!(artifacts.validate().is_err());
    }

    #[test]
    fn validates_saved_inference_preferences() {
        let mut settings = AppSettings::default();
        settings.model_backend = "openvino".into();
        assert!(settings.validate().is_ok());
        settings.model_backend = "missing".into();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn recoil_defaults_are_disabled_and_valid() {
        let settings = AppSettings::default();
        assert!(!settings.recoil.recoil_enabled);
        assert!(settings.recoil.validate().is_ok());
    }

    #[test]
    fn recoil_rejects_a_missing_level_interval() {
        let settings = RecoilSettings {
            recoil_enabled: true,
            recoil_levels: vec![1, 2],
            recoil_level_duration_ms: 0,
            ..RecoilSettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn older_saved_settings_load_new_recoil_defaults() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.recoil, RecoilSettings::default());
    }
}
