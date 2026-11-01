#[path = "../../native-shared/motion_renderer.rs"]
pub mod motion_renderer;
pub use motion_renderer::MotionRendererSettings;

use crate::targeting::SmoothingRange;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Recoil patterns (user's `recoil.rs`): one vertical pull per bullet,
/// in device counts at 100 %.
pub const RECOIL_PATTERNS: [&str; 3] = ["vandal", "phantom", "custom"];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct RecoilSettings {
    pub recoil_enabled: bool,
    /// "vandal", "phantom" or "custom" (`recoil_levels`).
    pub recoil_pattern: String,
    /// Custom pattern: the pull for each bullet; the last repeats.
    pub recoil_levels: Vec<i32>,
    pub recoil_start_delay_ms: u32,
    /// Time per bullet.
    pub recoil_level_duration_ms: u32,
    pub recoil_percentage: u8,
}

impl Default for RecoilSettings {
    fn default() -> Self {
        Self {
            recoil_enabled: false,
            recoil_pattern: "vandal".into(),
            recoil_levels: Vec::new(),
            recoil_start_delay_ms: 120,
            recoil_level_duration_ms: 100,
            recoil_percentage: 60,
        }
    }
}

impl RecoilSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !RECOIL_PATTERNS.contains(&self.recoil_pattern.as_str()) {
            return Err("invalid recoil pattern");
        }
        if self.recoil_levels.len() > 64
            || self
                .recoil_levels
                .iter()
                .any(|value| !(-10_000..=10_000).contains(value))
        {
            return Err("recoil levels must contain at most 64 values from -10000 to 10000");
        }
        if self.recoil_start_delay_ms > 60_000 || self.recoil_level_duration_ms > 5_000 {
            return Err("recoil delay must be 0..60000 ms and time per bullet 0..5000 ms");
        }
        if self.recoil_enabled && self.recoil_level_duration_ms == 0 {
            return Err("recoil time per bullet must be greater than 0 ms");
        }
        if self.recoil_percentage > 200 {
            return Err("recoil strength must be 0..200 %");
        }
        Ok(())
    }
}

/// Magnet modes (user's `magnet.rs`, plus Auto's center-crossing click).
pub const MAGNET_MODES: [&str; 4] = [
    "spray",
    "tap",
    "silent_immediate",
    "auto",
];

/// Dynamic FOV keys: "off" or a trigger button held to widen the FOV.
pub const DYNAMIC_FOV_KEYS: [&str; 4] = ["off", "right", "m4", "m5"];

/// Magnet: while its trigger is held, pulls toward the target and fires.
/// Every mode shares Delay, Cooldown and Sleep; Auto has no pull.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MagnetSettings {
    pub enabled: bool,
    pub mode: String,
    pub trigger_button: String,
    pub fov_degrees: u16,
    /// FOV while `dynamic_fov_key` is held ("off": never).
    pub dynamic_fov_degrees: u16,
    pub dynamic_fov_key: String,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Auto mode: wait after a target appears before firing.
    pub delay_ms: u32,
    /// Auto mode: wait after a shot before the next one.
    pub cooldown_ms: u32,
    /// Auto mode: evaluate at most this often (0: every device tick).
    pub sleep_ms: u32,
    /// Spray: the longest burst.
    pub forced_release_ms: u32,
    /// Pull divisor drawn in [min, max] per move (1 = the whole error).
    pub smooth_x_min: f64,
    pub smooth_x_max: f64,
    pub smooth_y_min: f64,
    pub smooth_y_max: f64,
    pub speed_booster: bool,
    pub speed_near: f64,
    pub speed_mid: f64,
    pub speed_far: f64,
}

impl Default for MagnetSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "spray".into(),
            trigger_button: "m5".into(),
            fov_degrees: 36,
            dynamic_fov_degrees: 160,
            dynamic_fov_key: "off".into(),
            offset_x: 0.0,
            offset_y: 0.0,
            delay_ms: 0,
            cooldown_ms: 90,
            sleep_ms: 0,
            forced_release_ms: 250,
            smooth_x_min: 4.0,
            smooth_x_max: 4.0,
            smooth_y_min: 5.0,
            smooth_y_max: 5.0,
            speed_booster: false,
            speed_near: 1.0,
            speed_mid: 1.0,
            speed_far: 1.0,
        }
    }
}

impl MagnetSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !MAGNET_MODES.contains(&self.mode.as_str()) {
            return Err("invalid Magnet mode");
        }
        if !TRIGGER_BUTTONS.contains(&self.trigger_button.as_str()) {
            return Err("invalid Magnet trigger button");
        }
        if !DYNAMIC_FOV_KEYS.contains(&self.dynamic_fov_key.as_str()) {
            return Err("invalid Magnet dynamic FOV key");
        }
        if !(1..=360).contains(&self.fov_degrees) || !(1..=360).contains(&self.dynamic_fov_degrees)
        {
            return Err("Magnet FOV must be 1..360");
        }
        if !self.offset_x.is_finite() || !self.offset_y.is_finite() {
            return Err("Magnet offsets must be finite");
        }
        if [
            self.delay_ms,
            self.cooldown_ms,
            self.sleep_ms,
            self.forced_release_ms,
        ]
        .iter()
        .any(|ms| *ms > 10_000)
        {
            return Err("Magnet times must be 0..10000 ms");
        }
        let range = |min: f64, max: f64| {
            min.is_finite() && max.is_finite() && 1.0 <= min && min <= max && max <= 50.0
        };
        if !range(self.smooth_x_min, self.smooth_x_max)
            || !range(self.smooth_y_min, self.smooth_y_max)
        {
            return Err("Magnet smoothing must be 1..50 with min <= max");
        }
        if ![self.speed_near, self.speed_mid, self.speed_far]
            .iter()
            .all(|speed| speed.is_finite() && (0.0..=10.0).contains(speed))
        {
            return Err("Magnet booster speeds must be 0..10");
        }
        Ok(())
    }

    /// Auto is the Magnet mode "auto"; its click timing keeps its own state.
    pub fn auto(&self) -> AutoSettings {
        AutoSettings {
            enabled: self.enabled && self.mode == "auto",
            trigger_button: self.trigger_button.clone(),
            fov_degrees: self.fov_degrees,
            delay_ms: self.delay_ms,
            cooldown_ms: self.cooldown_ms,
            sleep_ms: self.sleep_ms,
        }
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
    /// Each press of the trigger button turns Motion on or off instead of
    /// Motion running only while the button is held.
    pub motion_toggle: bool,
    pub motion_renderer: MotionRendererSettings,
    /// Preview shows only pixels of the selected target color; the rest is black.
    pub color_filter: bool,
    pub trigger_button: String,
    pub strategy: String,
    pub fov_degrees: u16,
    /// Motion FOV while `dynamic_fov_key` is held ("off": never).
    pub dynamic_fov_degrees: u16,
    pub dynamic_fov_key: String,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Smoothing 1–50 per axis: Motion divides the error by a value drawn
    /// between min and max on every frame (1 moves the whole error).
    /// Motion Speed per axis: the fraction of the error moved per frame
    /// (stable Normal).
    pub motion_speed_x: f64,
    pub motion_speed_y: f64,
    /// Inside this FOV the speed is divided by the smoothing strength.
    pub smooth_fov_degrees: u16,
    pub smoothing_x_min: f32,
    pub smoothing_x_max: f32,
    pub smoothing_y_min: f32,
    pub smoothing_y_max: f32,
    pub sens: f64,
    /// Scale the Motion step by distance: near (< 30 px), mid (30–80 px)
    /// and far (≥ 200 px), interpolated between.
    pub speed_booster: bool,
    pub speed_near: f64,
    pub speed_mid: f64,
    pub speed_far: f64,
    /// Keep full speed while the crosshair is inside the target box
    /// (otherwise 35 %).
    pub continue_inside_box: bool,
    /// Object Detection Color swap: the enemy outline color in game
    /// ("off", "yellow", "red", "purple"); only boxes holding that color are
    /// targets, so teammates (no enemy outline) are not.
    pub color_swap: String,
    /// Object Detection: Min confidence of ability classes (0..1).
    pub ability_confidence: f64,
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
    pub magnet: MagnetSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            active_profile: 0,
            profiles: Vec::new(),
            motion_enabled: false,
            motion_always_on: false,
            motion_toggle: false,
            motion_renderer: MotionRendererSettings::default(),
            color_filter: false,
            trigger_button: "right".into(),
            strategy: "pixel_seek".into(),
            fov_degrees: 36,
            dynamic_fov_degrees: 160,
            dynamic_fov_key: "off".into(),
            offset_x: 0.0,
            offset_y: 0.0,
            motion_speed_x: crate::movement::motion::DEFAULT_SPEED,
            motion_speed_y: crate::movement::motion::DEFAULT_SPEED,
            smooth_fov_degrees: 20,
            smoothing_x_min: 1.0,
            smoothing_x_max: 1.0,
            smoothing_y_min: 1.0,
            smoothing_y_max: 1.0,
            sens: 0.25,
            speed_booster: false,
            speed_near: 1.0,
            speed_mid: 1.0,
            speed_far: 1.0,
            continue_inside_box: false,
            color_swap: "off".into(),
            ability_confidence: 0.6,
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
            magnet: MagnetSettings::default(),
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

    pub fn speed_booster(&self) -> Option<crate::movement::motion::SpeedBooster> {
        self.speed_booster
            .then_some(crate::movement::motion::SpeedBooster {
                near: self.speed_near,
                mid: self.speed_mid,
                far: self.speed_far,
            })
    }

    /// The enemy outline color Object Detection requires, if any.
    pub fn color_swap(&self) -> Option<crate::pixel_seek::PixelTargetColor> {
        crate::pixel_seek::PixelTargetColor::parse(&self.color_swap)
    }

    pub fn validate(&mut self) -> Result<(), SettingsError> {
        // Silent flick (near) was removed (user request, 2026-11-01).
        if self.magnet.mode == "silent_proximity" {
            self.magnet.mode = "silent_immediate".into();
        }
        // Always on and Toggle exclude each other; Always on wins in files
        // saved with both.
        if self.motion_always_on {
            self.motion_toggle = false;
        }
        // Smoothing and offsets are whole numbers (user request).
        for value in [
            &mut self.smoothing_x_min,
            &mut self.smoothing_x_max,
            &mut self.smoothing_y_min,
            &mut self.smoothing_y_max,
        ] {
            *value = value.round();
        }
        for value in [
            &mut self.offset_x,
            &mut self.offset_y,
            &mut self.magnet.offset_x,
            &mut self.magnet.offset_y,
            &mut self.magnet.smooth_x_min,
            &mut self.magnet.smooth_x_max,
            &mut self.magnet.smooth_y_min,
            &mut self.magnet.smooth_y_max,
        ] {
            *value = value.round();
        }
        if !TRIGGER_BUTTONS.contains(&self.trigger_button.as_str()) {
            return Err(SettingsError::Invalid("invalid trigger button"));
        }
        if !["pixel_seek", "object_detection"].contains(&self.strategy.as_str()) {
            return Err(SettingsError::Invalid("invalid detection strategy"));
        }
        if self
            .model_confidence
            .parse::<f32>()
            .map_or(true, |value| !(0.0..=1.0).contains(&value))
        {
            return Err(SettingsError::Invalid("Min confidence must be from 0 to 1"));
        }
        if !(1..=360).contains(&self.fov_degrees) || !(1..=360).contains(&self.dynamic_fov_degrees)
        {
            return Err(SettingsError::Invalid("FOV must be 1..360"));
        }
        if !DYNAMIC_FOV_KEYS.contains(&self.dynamic_fov_key.as_str()) {
            return Err(SettingsError::Invalid("invalid dynamic FOV key"));
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
        if ![self.speed_near, self.speed_mid, self.speed_far]
            .iter()
            .all(|speed| speed.is_finite() && (0.0..=10.0).contains(speed))
        {
            return Err(SettingsError::Invalid("booster speeds must be 0..10"));
        }
        if ![self.motion_speed_x, self.motion_speed_y]
            .iter()
            .all(|speed| speed.is_finite() && (0.01..=1.0).contains(speed))
        {
            return Err(SettingsError::Invalid("motion speed must be 0.01..1"));
        }
        if self.smooth_fov_degrees > 360 {
            return Err(SettingsError::Invalid("Smooth FOV must be 0..360"));
        }
        if !(0.0..=1.0).contains(&self.ability_confidence) {
            return Err(SettingsError::Invalid("ability confidence must be 0..1"));
        }
        if self.color_swap != "off" && self.color_swap().is_none() {
            return Err(SettingsError::Invalid("invalid color swap"));
        }
        self.motion_renderer
            .validate()
            .map_err(SettingsError::Invalid)?;
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
        if !INFERENCE_BACKENDS.contains(&self.model_backend.as_str()) {
            return Err(SettingsError::Invalid("invalid inference backend"));
        }
        if !["fp16", "int8"].contains(&self.model_precision.as_str()) {
            return Err(SettingsError::Invalid("invalid inference precision"));
        }
        if !["cpu", "gpu", "npu"].contains(&self.compute_target.as_str()) {
            return Err(SettingsError::Invalid("invalid inference compute target"));
        }
        if !(1..=sysx_ui::protocol::MAX_CROP_RESOLUTION).contains(&self.crop_resolution) {
            return Err(SettingsError::Invalid(
                "crop resolution exceeds the receiver's frame limit",
            ));
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
        self.magnet.validate().map_err(SettingsError::Invalid)?;
        // Auto is a Magnet mode; its stored section follows Magnet.
        self.auto = self.magnet.auto();
        self.show_preview = true;
        Ok(())
    }
}

fn profile_snapshot(settings: &AppSettings) -> Value {
    // Auto is derived from Magnet; snapshots store the derived section.
    let mut derived = settings.clone();
    derived.auto = derived.magnet.auto();
    let mut value =
        serde_json::to_value(derived).unwrap_or_else(|_| Value::Object(Default::default()));
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
    migrate_recoil(&mut json);
    migrate_auto_into_magnet(&mut json);
    migrate_no_deadzone(&mut json);
    migrate_crop_resolution(&mut json);
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

/// Stable's `motionNoDeadzone` is `continueInsideBox` ("No deadzone") now,
/// in the settings and in every profile.
fn migrate_no_deadzone(json: &mut serde_json::Value) {
    fn migrate(settings: &mut serde_json::Value) {
        let Some(object) = settings.as_object_mut() else {
            return;
        };
        if let Some(value) = object.remove("motionNoDeadzone") {
            object.entry("continueInsideBox").or_insert(value);
        }
    }
    migrate(json);
    if let Some(profiles) = json.get_mut("profiles").and_then(Value::as_array_mut) {
        for profile in profiles {
            if let Some(settings) = profile.get_mut("settings") {
                migrate(settings);
            }
        }
    }
}

/// Auto became a Magnet mode (user request, 2026-10-31). Settings saved
/// before that (no `magnet.fovDegrees`) take Magnet's trigger, FOV and timing
/// from Auto, and select the Auto mode when Auto was on.
fn migrate_auto_into_magnet(json: &mut serde_json::Value) {
    fn migrate(settings: &mut serde_json::Value) {
        let Some(auto) = settings.get("auto").cloned() else {
            return;
        };
        if settings
            .get("magnet")
            .and_then(|magnet| magnet.get("fovDegrees"))
            .is_some()
        {
            return;
        }
        if !settings.get("magnet").is_some_and(Value::is_object) {
            settings["magnet"] = serde_json::json!({});
        }
        let magnet = &mut settings["magnet"];
        for (from, to) in [
            ("triggerButton", "triggerButton"),
            ("fovDegrees", "fovDegrees"),
            ("delayMs", "delayMs"),
            ("cooldownMs", "cooldownMs"),
            ("sleepMs", "sleepMs"),
        ] {
            if let Some(value) = auto.get(from) {
                magnet[to] = value.clone();
            }
        }
        if auto.get("enabled") == Some(&serde_json::json!(true)) {
            magnet["enabled"] = serde_json::json!(true);
            magnet["mode"] = serde_json::json!("auto");
        }
    }
    migrate(json);
    if let Some(profiles) = json.get_mut("profiles").and_then(Value::as_array_mut) {
        for profile in profiles {
            if let Some(settings) = profile.get_mut("settings") {
                migrate(settings);
            }
        }
    }
}

/// Recoil before patterns: its levels become the Custom pattern, and an
/// unset time per bullet (0) becomes the default, in every profile too.
fn migrate_recoil(json: &mut serde_json::Value) {
    fn migrate(settings: &mut serde_json::Value) {
        let Some(recoil) = settings.get_mut("recoil").and_then(Value::as_object_mut) else {
            return;
        };
        if !recoil.contains_key("recoilPattern") {
            let custom = recoil
                .get("recoilLevels")
                .and_then(Value::as_array)
                .is_some_and(|levels| !levels.is_empty());
            recoil.insert(
                "recoilPattern".into(),
                serde_json::json!(if custom { "custom" } else { "vandal" }),
            );
        }
        if recoil.get("recoilLevelDurationMs").and_then(Value::as_u64) == Some(0) {
            recoil.insert("recoilLevelDurationMs".into(), serde_json::json!(100));
        }
    }
    migrate(json);
    if let Some(profiles) = json.get_mut("profiles").and_then(Value::as_array_mut) {
        for profile in profiles {
            if let Some(settings) = profile.get_mut("settings") {
                migrate(settings);
            }
        }
    }
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

/// Earlier builds accepted crops larger than the receiver's pixel budget.
/// Clamp only that field so loading them preserves the rest of user settings.
fn migrate_crop_resolution(json: &mut serde_json::Value) {
    fn migrate(settings: &mut serde_json::Value) {
        if settings
            .get("cropResolution")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|resolution| {
                resolution > u64::from(sysx_ui::protocol::MAX_CROP_RESOLUTION)
            })
        {
            settings["cropResolution"] = serde_json::json!(sysx_ui::protocol::MAX_CROP_RESOLUTION);
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

// All writers in this backend process share the same lock, including HTTP Apply
// and automatic capture. Capture's compare-and-save must not overwrite a newer Apply.
static SETTINGS_IO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn capture_identity(settings: &AppSettings) -> serde_json::Value {
    let mut current = settings.clone();
    current.auto = current.magnet.auto(); // derived on save
    current.profiles.clear(); // inactive snapshots are preserved from disk on save
    serde_json::to_value(current).unwrap_or_default()
}

pub fn save_captured_renderer(
    path: impl AsRef<Path>,
    expected: &AppSettings,
    renderer: MotionRendererSettings,
) -> Result<AppSettings, SettingsError> {
    let _guard = SETTINGS_IO_LOCK
        .lock()
        .map_err(|_| SettingsError::Invalid("settings file lock poisoned"))?;
    let mut saved = load_settings(path.as_ref());
    if !expected.motion_renderer.enabled
        || !expected.motion_renderer.reports.is_empty()
        || capture_identity(&saved) != capture_identity(expected)
    {
        return Err(SettingsError::Invalid(
            "Settings changed while collecting; capture was not saved.",
        ));
    }
    renderer.validate().map_err(SettingsError::Invalid)?;
    if renderer.reports.len() != 256 {
        return Err(SettingsError::Invalid("Capture is incomplete."));
    }
    saved.motion_renderer = renderer;
    saved.sync_active_profile();
    save_settings_unlocked(path, &saved)?;
    Ok(saved)
}

pub fn save_settings(path: impl AsRef<Path>, settings: &AppSettings) -> Result<(), SettingsError> {
    let _guard = SETTINGS_IO_LOCK
        .lock()
        .map_err(|_| SettingsError::Invalid("settings file lock poisoned"))?;
    save_settings_unlocked(path, settings)
}

fn save_settings_unlocked(
    path: impl AsRef<Path>,
    settings: &AppSettings,
) -> Result<(), SettingsError> {
    let mut settings = settings.clone();
    settings.sync_active_profile();
    settings.validate()?;
    atomic_write(path.as_ref(), &serde_json::to_vec(&settings)?)?;
    Ok(())
}

/// Saved `modelBackend` values: ONNX Runtime on CPU, ONNX Runtime with the
/// DirectML, CUDA, TensorRT or CoreML execution provider, and OpenVINO.
pub const INFERENCE_BACKENDS: [&str; 6] = [
    "onnxruntime",
    "directml",
    "cuda",
    "tensorrt",
    "openvino",
    "coreml",
];

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelArtifacts {
    pub onnx: String,
    pub tensorrt: String,
    pub openvino_xml: String,
    pub openvino_bin: String,
}

impl ModelArtifacts {
    /// ONNX is required; the TensorRT engine and the OpenVINO XML/BIN pair
    /// are optional, but must carry their own extensions when present.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !has_extension(&self.onnx, &["onnx"]) {
            return Err("model bundle requires an ONNX .onnx file");
        }
        if !self.tensorrt.is_empty() && !has_extension(&self.tensorrt, &["engine", "plan"]) {
            return Err("TensorRT artifact must be an .engine or .plan file");
        }
        if self.openvino_xml.is_empty() != self.openvino_bin.is_empty()
            || (!self.openvino_xml.is_empty()
                && (!has_extension(&self.openvino_xml, &["xml"])
                    || !has_extension(&self.openvino_bin, &["bin"])))
        {
            return Err("OpenVINO artifacts need both an .xml and a .bin file");
        }
        Ok(())
    }

    /// The file a runtime loads. Every runtime can start from the ONNX file;
    /// TensorRT and OpenVINO prefer their own artifacts when the bundle has
    /// them.
    pub fn for_backend(&self, backend: &str) -> Result<&str, &'static str> {
        self.validate()?;
        match backend {
            "onnxruntime" | "directml" | "cuda" | "coreml" => Ok(&self.onnx),
            "tensorrt" if self.tensorrt.is_empty() => Ok(&self.onnx),
            "tensorrt" => Ok(&self.tensorrt),
            "openvino" if self.openvino_xml.is_empty() => Ok(&self.onnx),
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
    /// Output already decoded and NMS-free (`onnx_info::OnnxInfo::end2end`).
    #[serde(default)]
    pub end2end: bool,
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
                    settings.magnet.delay_ms = 10 + index as u32;
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
                assert_eq!(settings.magnet, selected.magnet);
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
        // Saved before Auto became a Magnet mode.
        old.as_object_mut().unwrap().remove("magnet");
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
    fn object_detection_strategy_loads_in_every_profile() {
        let directory = std::env::temp_dir().join(format!(
            "sysx-settings-object-detection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app-settings.json");
        let mut old = serde_json::to_value(AppSettings::default()).unwrap();
        old["strategy"] = serde_json::json!("object_detection");
        let mut second = old.clone();
        second.as_object_mut().unwrap().remove("profiles");
        second["speedNear"] = serde_json::json!(0.12);
        old["profiles"] = serde_json::json!([
            {"name":"Default","settings":old.clone()},
            {"name":"Second","settings":second}
        ]);
        old["profiles"][0]["settings"]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let mut loaded = load_settings(&path);
        assert_eq!(loaded.strategy, "object_detection");
        assert_eq!(loaded.profiles.len(), 2);
        assert_eq!(loaded.profiles[0].settings["strategy"], "object_detection");
        assert_eq!(loaded.profiles[1].settings["strategy"], "object_detection");
        loaded.activate_profile(1).unwrap();
        assert_eq!(loaded.speed_near, 0.12);
        assert_eq!(loaded.strategy, "object_detection");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn oversized_legacy_crops_are_clamped_without_resetting_profiles() {
        let directory = std::env::temp_dir().join(format!(
            "sysx-settings-crop-limit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app-settings.json");
        let mut old = serde_json::to_value(AppSettings::default()).unwrap();
        old["cropResolution"] = serde_json::json!(8192);
        old["speedNear"] = serde_json::json!(0.12);
        let mut second = old.clone();
        second.as_object_mut().unwrap().remove("profiles");
        second["cropResolution"] = serde_json::json!(256);
        old["profiles"] = serde_json::json!([
            {"name":"Default","settings":old.clone()},
            {"name":"Second","settings":second}
        ]);
        old["profiles"][0]["settings"]
            .as_object_mut()
            .unwrap()
            .remove("profiles");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let mut loaded = load_settings(&path);
        assert_eq!(
            loaded.crop_resolution,
            sysx_ui::protocol::MAX_CROP_RESOLUTION
        );
        assert_eq!(loaded.speed_near, 0.12);
        assert_eq!(loaded.profiles.len(), 2);
        assert_eq!(
            loaded.profiles[0].settings["cropResolution"],
            sysx_ui::protocol::MAX_CROP_RESOLUTION
        );
        loaded.activate_profile(1).unwrap();
        assert_eq!(loaded.crop_resolution, 256);
        assert_eq!(loaded.speed_near, 0.12);
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
        for (min, max) in [(6.0, 3.0), (0.0, 3.0), (1.0, 51.0)] {
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
        artifacts = valid_artifacts();
        artifacts.onnx.clear();
        assert!(artifacts.validate().is_err());
    }

    #[test]
    fn an_onnx_only_model_runs_on_every_runtime() {
        let artifacts = ModelArtifacts {
            onnx: "/models/v11_256.onnx".into(),
            ..ModelArtifacts::default()
        };
        assert!(artifacts.validate().is_ok());
        for backend in INFERENCE_BACKENDS {
            assert_eq!(
                artifacts.for_backend(backend),
                Ok("/models/v11_256.onnx"),
                "{backend}"
            );
        }
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
    fn crop_resolution_cannot_exceed_the_receiver_frame_budget() {
        let mut settings = AppSettings::default();
        settings.crop_resolution = sysx_ui::protocol::MAX_CROP_RESOLUTION;
        assert!(settings.validate().is_ok());
        settings.crop_resolution += 1;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn old_recoil_levels_become_the_custom_pattern_and_magnet_is_validated() {
        let mut json = serde_json::json!({
            "recoil":{"recoilEnabled":true,"recoilLevels":[3,7],"recoilLevelDurationMs":0},
            "profiles":[{"name":"Default","settings":{"recoil":{"recoilLevels":[]}}}]
        });
        migrate_recoil(&mut json);
        assert_eq!(json["recoil"]["recoilPattern"], "custom");
        assert_eq!(json["recoil"]["recoilLevelDurationMs"], 100);
        assert_eq!(
            json["profiles"][0]["settings"]["recoil"]["recoilPattern"],
            "vandal"
        );
        let mut settings: AppSettings = serde_json::from_value(json).unwrap();
        assert!(settings.validate().is_ok());

        let defaults = MagnetSettings::default();
        assert!(!defaults.enabled && defaults.validate().is_ok());
        for broken in [
            MagnetSettings {
                mode: "aim".into(),
                ..MagnetSettings::default()
            },
            MagnetSettings {
                trigger_button: "left".into(),
                ..MagnetSettings::default()
            },
            MagnetSettings {
                smooth_x_min: 0.5,
                ..MagnetSettings::default()
            },
            MagnetSettings {
                forced_release_ms: 20_000,
                ..MagnetSettings::default()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
    }

    #[test]
    fn an_older_auto_becomes_the_magnet_auto_mode() {
        let mut json = serde_json::json!({
            "auto":{"enabled":true,"triggerButton":"m4","fovDegrees":50,"delayMs":20,"cooldownMs":300,"sleepMs":5},
            "magnet":{"enabled":false,"mode":"spray"}
        });
        migrate_auto_into_magnet(&mut json);
        let mut settings: AppSettings = serde_json::from_value(json).unwrap();
        settings.validate().unwrap();
        let magnet = &settings.magnet;
        assert!(magnet.enabled);
        assert_eq!(magnet.mode, "auto");
        assert_eq!(
            (
                magnet.trigger_button.as_str(),
                magnet.fov_degrees,
                magnet.delay_ms,
                magnet.cooldown_ms,
                magnet.sleep_ms
            ),
            ("m4", 50, 20, 300, 5)
        );
        assert_eq!(settings.auto, magnet.auto());
        // Saved after the change: Magnet already has its own FOV.
        let mut later = serde_json::to_value(&settings).unwrap();
        later["magnet"]["mode"] = serde_json::json!("tap");
        migrate_auto_into_magnet(&mut later);
        assert_eq!(later["magnet"]["mode"], "tap");
        let mut tap: AppSettings = serde_json::from_value(later).unwrap();
        tap.validate().unwrap();
        assert!(!tap.auto.enabled, "Auto runs only in the auto mode");
    }

    #[test]
    fn recoil_defaults_are_disabled_and_valid() {
        let settings = AppSettings::default();
        assert!(!settings.recoil.recoil_enabled);
        assert!(settings.recoil.validate().is_ok());
    }

    #[test]
    fn motion_defaults_are_stable_normal_and_its_keys_load() {
        let defaults = AppSettings::default();
        assert!(!defaults.continue_inside_box && !defaults.speed_booster);
        assert_eq!(
            (defaults.motion_speed_x, defaults.motion_speed_y, defaults.smooth_fov_degrees),
            (0.045, 0.045, 20)
        );
        assert_eq!(
            (defaults.smoothing_x(), defaults.smoothing_y()),
            (SmoothingRange::NONE, SmoothingRange::NONE)
        );
        let mut legacy: AppSettings =
            serde_json::from_str(r#"{"motionSpeedX":0.1,"motionSpeedY":0.12,"smoothFovDegrees":30}"#)
                .unwrap();
        assert!(legacy.validate().is_ok());
        assert_eq!((legacy.motion_speed_x, legacy.motion_speed_y), (0.1, 0.12));
        assert_eq!(legacy.smooth_fov_degrees, 30);
        let mut json = serde_json::json!({
            "motionNoDeadzone": true,
            "profiles": [{"name": "A", "settings": {"motionNoDeadzone": true}}]
        });
        migrate_no_deadzone(&mut json);
        assert_eq!(json["continueInsideBox"], true);
        assert_eq!(json["profiles"][0]["settings"]["continueInsideBox"], true);
        assert!(json.get("motionNoDeadzone").is_none());
        legacy.motion_speed_x = 0.0;
        assert!(legacy.validate().is_err(), "speed 0.01..1");
    }

    #[test]
    fn always_on_excludes_toggle_and_smoothing_and_offsets_are_whole() {
        let mut settings = AppSettings {
            motion_always_on: true,
            motion_toggle: true,
            smoothing_x_min: 5.4,
            smoothing_x_max: 7.6,
            offset_x: -2.5,
            offset_y: 1.4,
            ..AppSettings::default()
        };
        settings.magnet.smooth_y_max = 6.7;
        settings.magnet.offset_x = 3.2;
        settings.speed_near = 1.25;
        settings.validate().unwrap();
        assert!(settings.motion_always_on && !settings.motion_toggle);
        assert_eq!((settings.smoothing_x_min, settings.smoothing_x_max), (5.0, 8.0));
        assert_eq!((settings.offset_x, settings.offset_y), (-3.0, 1.0));
        assert_eq!((settings.magnet.smooth_y_max, settings.magnet.offset_x), (7.0, 3.0));
        assert_eq!(settings.speed_near, 1.25, "the booster keeps its decimals");
        // Toggle alone stays on; old files with axisGuard load and drop it.
        let mut toggle: AppSettings =
            serde_json::from_str(r#"{"motionToggle":true,"axisGuard":false}"#).unwrap();
        toggle.validate().unwrap();
        assert!(toggle.motion_toggle);
        assert!(serde_json::to_value(&toggle).unwrap().get("axisGuard").is_none());
    }

    #[test]
    fn the_removed_silent_flick_near_loads_as_silent_flick() {
        let mut settings = AppSettings::default();
        settings.magnet.mode = "silent_proximity".into();
        settings.validate().unwrap();
        assert_eq!(settings.magnet.mode, "silent_immediate");
        let saved: AppSettings =
            serde_json::from_str(r#"{"magnet":{"flickProximityPx":26.0}}"#).unwrap();
        assert!(serde_json::to_value(&saved).unwrap()["magnet"]
            .get("flickProximityPx")
            .is_none());
    }

    #[test]
    fn color_swap_is_off_by_default_and_validated() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.color_swap(), None);
        settings.color_swap = "yellow".into();
        assert_eq!(
            settings.color_swap(),
            Some(crate::pixel_seek::PixelTargetColor::Yellow)
        );
        assert!(settings.validate().is_ok());
        settings.color_swap = "green".into();
        assert!(settings.validate().is_err());
        // Older files load without it; their targetZone is dropped.
        let loaded: AppSettings =
            serde_json::from_str(r#"{"targetZone":"body","colorSwapTo":"red"}"#).unwrap();
        assert_eq!(loaded.color_swap, "off");
        let saved = serde_json::to_value(&loaded).unwrap();
        assert!(saved.get("targetZone").is_none());
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
