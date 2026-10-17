use crate::targeting::SmoothingRange;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
pub struct AutoSettings {
    pub enabled: bool,
    pub trigger_button: String,
    pub fov_degrees: u16,
    pub delay_ms: u32,
    pub cooldown_ms: u32,
    pub sleep_ms: u32,
}

pub const MAX_PROFILES: usize = 3;

/// Motion and Auto trigger choices. Left fires the weapon and Middle cycles
/// profiles, so neither can be a trigger.
pub const TRIGGER_BUTTONS: [&str; 3] = ["right", "m4", "m5"];

/// Persisted snapshot for one profile. The snapshot deliberately contains
/// only the normal AppSettings fields; profile metadata is kept by the outer
/// AppSettings object so snapshots cannot recursively contain themselves.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ProfileSlot {
    pub name: String,
    pub settings: Value,
}

impl Default for ProfileSlot {
    fn default() -> Self {
        Self {
            name: String::new(),
            settings: Value::Object(Default::default()),
        }
    }
}

impl Default for AutoSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_button: "right".into(),
            fov_degrees: 36,
            delay_ms: 0,
            cooldown_ms: 0,
            sleep_ms: 0,
        }
    }
}

impl AutoSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !TRIGGER_BUTTONS.contains(&self.trigger_button.as_str()) {
            return Err("invalid Auto trigger button");
        }
        if !(1..=360).contains(&self.fov_degrees) {
            return Err("Auto FOV must be 1..360");
        }
        if self.delay_ms > 60_000 || self.cooldown_ms > 60_000 || self.sleep_ms > 60_000 {
            return Err("Auto delay, cooldown, and sleep must be 0..60000 ms");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    /// Zero-based active profile slot. The UI presents the active name and
    /// allows up to three saved profiles.
    pub active_profile: u8,
    /// Up to three independent setting snapshots.
    pub profiles: Vec<ProfileSlot>,
    pub motion_enabled: bool,
    pub motion_always_on: bool,
    /// Preview shows only pixels of the selected target color; the rest is black.
    pub color_filter: bool,
    pub trigger_button: String,
    pub strategy: String,
    pub fov_degrees: u16,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Smoothing strength 1–50 per axis (1 = none); each track draws a
    /// strength between min and max.
    pub smoothing_x_min: f32,
    pub smoothing_x_max: f32,
    pub smoothing_y_min: f32,
    pub smoothing_y_max: f32,
    pub sens: f64,
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
    pub auto: AutoSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            active_profile: 0,
            profiles: Vec::new(),
            motion_enabled: false,
            motion_always_on: false,
            color_filter: false,
            trigger_button: "right".into(),
            strategy: "pixel_seek".into(),
            fov_degrees: 36,
            offset_x: 0.0,
            offset_y: 0.0,
            smoothing_x_min: 1.0,
            smoothing_x_max: 1.0,
            smoothing_y_min: 1.0,
            smoothing_y_max: 1.0,
            sens: 0.25,
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
            auto: AutoSettings::default(),
        }
    }
}

impl AppSettings {
    pub fn normalize_profiles(&mut self) {
        let current = profile_snapshot(self);
        if self.profiles.len() > MAX_PROFILES {
            self.profiles.truncate(MAX_PROFILES);
        }
        if self.profiles.is_empty() {
            self.profiles.push(ProfileSlot {
                name: "Default".into(),
                settings: current,
            });
        }
        for (index, slot) in self.profiles.iter_mut().enumerate() {
            if slot.name.trim().is_empty() {
                slot.name = if index == 0 {
                    "Default".into()
                } else {
                    format!("Profile {}", index + 1)
                };
            }
            if !slot.settings.is_object()
                || serde_json::from_value::<AppSettings>(slot.settings.clone()).is_err()
            {
                slot.settings = profile_snapshot(&AppSettings::default());
            }
        }
        self.active_profile =
            usize::from(self.active_profile).min(self.profiles.len().saturating_sub(1)) as u8;
    }

    pub fn sync_active_profile(&mut self) {
        self.normalize_profiles();
        let index = usize::from(self.active_profile).min(self.profiles.len() - 1);
        let snapshot = profile_snapshot(self);
        self.profiles[index].settings = snapshot;
    }

    pub fn activate_profile(&mut self, index: usize) -> Result<(), SettingsError> {
        self.normalize_profiles();
        if index >= self.profiles.len() {
            return Err(SettingsError::Invalid("profile does not exist"));
        }
        self.sync_active_profile();
        self.load_profile(index)
    }

    /// Load a snapshot without saving the current top-level values. In
    /// particular, a deleted profile must never be saved into its successor.
    fn load_profile(&mut self, index: usize) -> Result<(), SettingsError> {
        let mut next: AppSettings =
            serde_json::from_value(self.profiles[index].settings.clone())
                .map_err(|_| SettingsError::Invalid("profile settings are invalid"))?;
        next.profiles = self.profiles.clone();
        next.active_profile = index as u8;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn rename_profile(&mut self, index: usize, name: &str) -> Result<(), SettingsError> {
        self.normalize_profiles();
        let name = name.trim();
        if index >= self.profiles.len() {
            return Err(SettingsError::Invalid("profile does not exist"));
        }
        if name.is_empty() || name.chars().count() > 32 {
            return Err(SettingsError::Invalid(
                "profile name must be 1..32 characters",
            ));
        }
        self.profiles[index].name = name.to_owned();
        Ok(())
    }

    pub fn create_profile(
        &mut self,
        name: Option<&str>,
        duplicate_current: bool,
    ) -> Result<(), SettingsError> {
        self.normalize_profiles();
        if self.profiles.len() >= MAX_PROFILES {
            return Err(SettingsError::Invalid("maximum of 3 profiles reached"));
        }
        let index = self.profiles.len();
        let snapshot = if duplicate_current {
            self.sync_active_profile();
            self.profiles[usize::from(self.active_profile)]
                .settings
                .clone()
        } else {
            profile_snapshot(&AppSettings::default())
        };
        let generated_name = if duplicate_current {
            format!(
                "{} Copy",
                self.profiles[usize::from(self.active_profile)].name
            )
        } else {
            format!("Profile {}", index + 1)
        };
        let profile_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or(generated_name);
        if profile_name.chars().count() > 32 {
            return Err(SettingsError::Invalid(
                "profile name must be 1..32 characters",
            ));
        }
        self.profiles.push(ProfileSlot {
            name: profile_name,
            settings: snapshot,
        });
        self.activate_profile(index)
    }

    pub fn delete_profile(&mut self, index: usize) -> Result<(), SettingsError> {
        self.normalize_profiles();
        if self.profiles.len() <= 1 {
            return Err(SettingsError::Invalid("at least one profile is required"));
        }
        if index >= self.profiles.len() {
            return Err(SettingsError::Invalid("profile does not exist"));
        }
        let previous_active = usize::from(self.active_profile);
        self.sync_active_profile();
        self.profiles.remove(index);
        let next_index = if previous_active > index {
            previous_active - 1
        } else {
            previous_active.min(self.profiles.len() - 1)
        };
        self.load_profile(next_index)
    }

    pub fn smoothing_x(&self) -> SmoothingRange {
        SmoothingRange {
            min: self.smoothing_x_min,
            max: self.smoothing_x_max,
        }
    }

    pub fn smoothing_y(&self) -> SmoothingRange {
        SmoothingRange {
            min: self.smoothing_y_min,
            max: self.smoothing_y_max,
        }
    }

    pub fn validate(&mut self) -> Result<(), SettingsError> {
        if !TRIGGER_BUTTONS.contains(&self.trigger_button.as_str()) {
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
        if !self.smoothing_x().is_valid() || !self.smoothing_y().is_valid() {
            return Err(SettingsError::Invalid(
                "smoothing must be 1..50 with min <= max",
            ));
        }
        if !self.sens.is_finite() || !(0.01..=10.0).contains(&self.sens) {
            return Err(SettingsError::Invalid("sensitivity must be 0.01..10"));
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
        self.auto.validate().map_err(SettingsError::Invalid)?;
        self.show_preview = true;
        Ok(())
    }
}

fn profile_snapshot(settings: &AppSettings) -> Value {
    let mut value =
        serde_json::to_value(settings).unwrap_or_else(|_| Value::Object(Default::default()));
    if let Some(object) = value.as_object_mut() {
        object.remove("activeProfile");
        object.remove("profiles");
    }
    value
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
        return default_settings_with_profiles();
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return default_settings_with_profiles();
    };
    migrate_smoothing(&mut json);
    migrate_trigger_buttons(&mut json);
    let Ok(mut value) = serde_json::from_value::<AppSettings>(json) else {
        return default_settings_with_profiles();
    };
    value.normalize_profiles();
    if value.validate().is_err() {
        default_settings_with_profiles()
    } else {
        value
    }
}

fn default_settings_with_profiles() -> AppSettings {
    let mut settings = AppSettings::default();
    settings.normalize_profiles();
    settings
}

/// Middle was a trigger choice before it became the profile-cycle button;
/// files that still select it fall back to Right, in every profile too.
fn migrate_trigger_buttons(json: &mut serde_json::Value) {
    fn migrate(settings: &mut serde_json::Value) {
        let middle = |value: &serde_json::Value| {
            value
                .get("triggerButton")
                .is_some_and(|button| button == "middle")
        };
        if middle(settings) {
            settings["triggerButton"] = serde_json::json!("right");
        }
        if settings.get("auto").is_some_and(middle) {
            settings["auto"]["triggerButton"] = serde_json::json!("right");
        }
    }
    migrate(json);
    if let Some(profiles) = json
        .get_mut("profiles")
        .and_then(serde_json::Value::as_array_mut)
    {
        for profile in profiles {
            if let Some(settings) = profile.get_mut("settings") {
                migrate(settings);
            }
        }
    }
}

/// Files from before the 1–50 scale stored one alpha per axis; strength is
/// `1 / alpha`, used as both min and max.
fn migrate_smoothing(json: &mut serde_json::Value) {
    for (axis, alpha_key) in [("X", "smoothingAlphaX"), ("Y", "smoothingAlphaY")] {
        let (min_key, max_key) = (format!("smoothing{axis}Min"), format!("smoothing{axis}Max"));
        if json.get(&min_key).is_some() || json.get(&max_key).is_some() {
            continue;
        }
        let Some(alpha) = json.get(alpha_key).and_then(serde_json::Value::as_f64) else {
            continue;
        };
        if alpha.is_finite() && alpha > 0.0 {
            let strength = (1.0 / alpha).clamp(
                SmoothingRange::LIMITS.0 as f64,
                SmoothingRange::LIMITS.1 as f64,
            );
            json[min_key] = serde_json::json!(strength);
            json[max_key] = serde_json::json!(strength);
        }
    }
}

pub fn save_settings(path: impl AsRef<Path>, settings: &AppSettings) -> Result<(), SettingsError> {
    let mut settings = settings.clone();
    settings.sync_active_profile();
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

    #[test]
    fn deleting_any_profile_preserves_every_surviving_snapshot() {
        for active in 0..3 {
            for deleted in 0..3 {
                let mut settings = AppSettings::default();
                settings.normalize_profiles();
                for index in 0..3 {
                    if index > 0 {
                        settings
                            .create_profile(Some(&format!("Profile {index}")), false)
                            .unwrap();
                    }
                    settings.sens = [0.23, 0.75, 1.25][index];
                    settings.auto.delay_ms = 10 + index as u32;
                    settings.sync_active_profile();
                }
                settings.activate_profile(active).unwrap();
                let expected: Vec<_> = settings
                    .profiles
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != deleted)
                    .map(|(_, slot)| slot.clone())
                    .collect();
                settings.delete_profile(deleted).unwrap();
                assert_eq!(
                    settings.profiles, expected,
                    "active={active}, deleted={deleted}"
                );
                let selected: AppSettings = serde_json::from_value(
                    expected[usize::from(settings.active_profile)]
                        .settings
                        .clone(),
                )
                .unwrap();
                assert_eq!(settings.sens, selected.sens);
                assert_eq!(settings.auto, selected.auto);
            }
        }
    }

    #[test]
    fn settings_have_three_independent_profile_slots() {
        let mut first = AppSettings::default();
        first.fov_degrees = 77;
        first.normalize_profiles();
        assert_eq!(first.profiles.len(), 1);
        assert_eq!(first.profiles[0].name, "Default");
        first.sync_active_profile();
        first.create_profile(Some("Second"), false).unwrap();
        first.activate_profile(1).unwrap();
        assert_eq!(first.active_profile, 1);
        assert_eq!(first.fov_degrees, AppSettings::default().fov_degrees);
        first.fov_degrees = 123;
        first.activate_profile(0).unwrap();
        assert_eq!(first.fov_degrees, 77);
        first.create_profile(Some("Third"), false).unwrap();
        first.activate_profile(2).unwrap();
        first.activate_profile(0).unwrap();
        assert_eq!(first.profiles.len(), MAX_PROFILES);
    }

    #[test]
    fn profile_crud_uses_clear_generated_names_and_keeps_one_profile() {
        let mut settings = AppSettings::default();
        settings.normalize_profiles();
        settings.rename_profile(0, "Aim").unwrap();
        settings.create_profile(None, false).unwrap();
        assert_eq!(settings.profiles[1].name, "Profile 2");
        settings.create_profile(None, true).unwrap();
        assert_eq!(settings.profiles[2].name, "Profile 2 Copy");

        settings.delete_profile(1).unwrap();
        assert_eq!(settings.profiles.len(), 2);
        settings.delete_profile(0).unwrap();
        assert_eq!(settings.profiles.len(), 1);
        assert!(settings.delete_profile(0).is_err());
    }

    #[test]
    fn profile_slots_are_trimmed_and_active_index_is_clamped() {
        let mut settings = AppSettings::default();
        settings.active_profile = 99;
        settings.profiles = vec![ProfileSlot::default(); 8];
        settings.normalize_profiles();
        assert_eq!(settings.profiles.len(), MAX_PROFILES);
        assert_eq!(settings.active_profile, 2);
    }

    #[test]
    fn middle_is_no_trigger_and_older_middle_triggers_become_right() {
        let mut settings = AppSettings::default();
        settings.trigger_button = "middle".into();
        assert!(settings.validate().is_err());
        let mut auto = AutoSettings::default();
        auto.trigger_button = "middle".into();
        assert!(auto.validate().is_err());

        let directory = std::env::temp_dir().join(format!(
            "sysx-settings-middle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app-settings.json");
        let mut old = serde_json::to_value(AppSettings::default()).unwrap();
        old["triggerButton"] = serde_json::json!("middle");
        old["auto"]["triggerButton"] = serde_json::json!("middle");
        let mut snapshot = old.clone();
        snapshot.as_object_mut().unwrap().remove("profiles");
        snapshot["triggerButton"] = serde_json::json!("m4");
        old["profiles"] = serde_json::json!([
            {"name": "Default", "settings": old.clone()},
            {"name": "Second", "settings": snapshot},
        ]);
        old["profiles"][0]["settings"]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let mut loaded = load_settings(&path);
        assert_eq!(loaded.trigger_button, "right");
        assert_eq!(loaded.auto.trigger_button, "right");
        assert_eq!(
            loaded.profiles.len(),
            2,
            "the file was not reset to defaults"
        );
        loaded.activate_profile(1).unwrap();
        assert_eq!(loaded.trigger_button, "m4");
        assert_eq!(loaded.auto.trigger_button, "right");
        loaded.activate_profile(0).unwrap();
        assert_eq!(loaded.trigger_button, "right");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn older_alpha_smoothing_settings_become_the_one_to_ten_scale() {
        let directory = std::env::temp_dir().join(format!(
            "sysx-settings-smoothing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app-settings.json");
        let mut old = serde_json::to_value(AppSettings::default()).unwrap();
        for key in [
            "smoothingXMin",
            "smoothingXMax",
            "smoothingYMin",
            "smoothingYMax",
        ] {
            old.as_object_mut().unwrap().remove(key);
        }
        old["smoothingAlphaX"] = serde_json::json!(0.25);
        old["smoothingAlphaY"] = serde_json::json!(1.0);
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let loaded = load_settings(&path);
        assert_eq!((loaded.smoothing_x_min, loaded.smoothing_x_max), (4.0, 4.0));
        assert_eq!((loaded.smoothing_y_min, loaded.smoothing_y_max), (1.0, 1.0));
        save_settings(&path, &loaded).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("smoothingAlphaX").is_none());

        // Out of range or reversed ranges are rejected.
        for (min, max) in [(6.0, 3.0), (0.5, 3.0), (1.0, 51.0)] {
            let mut invalid = AppSettings {
                smoothing_x_min: min,
                smoothing_x_max: max,
                ..AppSettings::default()
            };
            assert!(invalid.validate().is_err(), "{min}..{max}");
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn settings_saved_with_the_removed_dpi_field_still_load_and_drop_it_on_save() {
        let directory = std::env::temp_dir().join(format!(
            "sysx-settings-dpi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app-settings.json");
        let mut old = serde_json::to_value(AppSettings::default()).unwrap();
        old["sens"] = serde_json::json!(0.42);
        old["dpi"] = serde_json::json!(800);
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let loaded = load_settings(&path);
        assert_eq!(loaded.sens, 0.42, "older files keep their sensitivity");
        save_settings(&path, &loaded).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("dpi").is_none());
        assert_eq!(saved["sens"], 0.42);
        let _ = fs::remove_dir_all(directory);
    }

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
    fn auto_defaults_are_separate_and_valid() {
        let settings = AppSettings::default();
        assert_eq!(settings.auto, AutoSettings::default());
        assert!(settings.auto.validate().is_ok());
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
