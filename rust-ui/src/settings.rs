use serde::{Deserialize, Serialize};

/// Only a successful local settings API response confirms durable storage.
pub fn require_settings_saved(status: u16, server_error: &str) -> Result<(), String> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!(
            "Settings were not saved (HTTP {status}): {}",
            server_error.trim()
        ))
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::require_settings_saved;

    #[test]
    fn storage_validation_and_service_failures_are_not_successful_saves() {
        for status in [400, 403, 409, 500, 503] {
            let message = require_settings_saved(status, "permission denied").unwrap_err();
            assert!(message.contains("not saved"));
            assert!(message.contains(&status.to_string()));
            assert!(message.contains("permission denied"));
        }
        assert!(require_settings_saved(200, "").is_ok());
        assert!(require_settings_saved(204, "").is_ok());
    }
}

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
            return Err("Use at most 64 recoil levels, each from -10000 to 10000.");
        }
        if self.recoil_start_delay_ms > 60_000 || self.recoil_level_duration_ms > 5_000 {
            return Err("Recoil delay must be from 0 to 60000 ms and interval from 0 to 5000 ms.");
        }
        if self.recoil_enabled
            && !self.recoil_levels.is_empty()
            && self.recoil_level_duration_ms == 0
        {
            return Err("Recoil interval must be greater than 0 ms when levels are configured.");
        }
        if self.recoil_percentage > 100 {
            return Err("Recoil percentage must be from 0 to 100.");
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
        if !["right", "m4", "m5"].contains(&self.trigger_button.as_str()) {
            return Err("Choose Right, M4, or M5 for the Auto trigger key.");
        }
        if !(1..=360).contains(&self.fov_degrees) {
            return Err("Auto FOV must be from 1 to 360 degrees.");
        }
        if self.delay_ms > 60_000 || self.cooldown_ms > 60_000 || self.sleep_ms > 60_000 {
            return Err("Auto delay, cooldown, and sleep must be from 0 to 60000 ms.");
        }
        Ok(())
    }
}

pub fn parse_recoil_levels(value: &str) -> Result<Vec<i32>, &'static str> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let levels = value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<i32>()
                .map_err(|_| "Use comma-separated whole numbers for recoil levels.")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if levels.len() > 64
        || levels
            .iter()
            .any(|level| !(-10_000..=10_000).contains(level))
    {
        return Err("Use at most 64 recoil levels, each from -10000 to 10000.");
    }
    Ok(levels)
}

pub fn format_recoil_levels(levels: &[i32]) -> String {
    levels
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default)]
    pub active_profile: u8,
    pub motion_enabled: bool,
    #[serde(default)]
    pub motion_always_on: bool,
    pub trigger_button: String,
    pub strategy: String,
    pub fov_degrees: u16,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Smoothing strength 1–50 per axis (1 = none); each new target draws a
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
            motion_enabled: false,
            motion_always_on: false,
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

pub const SMOOTHING_LIMITS: (f32, f32) = (1.0, 50.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmoothingAxis {
    X,
    Y,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmoothingBound {
    Min,
    Max,
}

impl AppSettings {
    pub fn smoothing(&self, axis: SmoothingAxis, bound: SmoothingBound) -> f32 {
        match (axis, bound) {
            (SmoothingAxis::X, SmoothingBound::Min) => self.smoothing_x_min,
            (SmoothingAxis::X, SmoothingBound::Max) => self.smoothing_x_max,
            (SmoothingAxis::Y, SmoothingBound::Min) => self.smoothing_y_min,
            (SmoothingAxis::Y, SmoothingBound::Max) => self.smoothing_y_max,
        }
    }

    /// Sets one bound (clamped to 1–10) and moves the other bound along when
    /// needed, so Min never ends above Max.
    pub fn set_smoothing(&mut self, axis: SmoothingAxis, bound: SmoothingBound, value: f32) {
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(SMOOTHING_LIMITS.0, SMOOTHING_LIMITS.1);
        let (min, max) = match axis {
            SmoothingAxis::X => (&mut self.smoothing_x_min, &mut self.smoothing_x_max),
            SmoothingAxis::Y => (&mut self.smoothing_y_min, &mut self.smoothing_y_max),
        };
        match bound {
            SmoothingBound::Min => {
                *min = value;
                *max = max.max(value);
            }
            SmoothingBound::Max => {
                *max = value;
                *min = min.min(value);
            }
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !["right", "m4", "m5"].contains(&self.trigger_button.as_str()) {
            return Err("Choose Right, M4, or M5 for the trigger button.");
        }
        if self.strategy != "pixel_seek" {
            return Err("Choose Pixel Seek. Object Detection is not available yet.");
        }
        if !(1..=360).contains(&self.fov_degrees) {
            return Err("FOV must be from 1 to 360 degrees.");
        }
        if !self.offset_x.is_finite() || !self.offset_y.is_finite() {
            return Err("Target offsets must be finite numbers.");
        }
        let range_ok = |min: f32, max: f32| {
            min.is_finite()
                && max.is_finite()
                && (SMOOTHING_LIMITS.0..=SMOOTHING_LIMITS.1).contains(&min)
                && (SMOOTHING_LIMITS.0..=SMOOTHING_LIMITS.1).contains(&max)
                && min <= max
        };
        if !range_ok(self.smoothing_x_min, self.smoothing_x_max)
            || !range_ok(self.smoothing_y_min, self.smoothing_y_max)
        {
            return Err("Smoothing must be from 1 to 50, with Min not above Max.");
        }
        if !self.sens.is_finite() || !(0.01..=10.0).contains(&self.sens) {
            return Err("Sensitivity must be from 0.01 to 10.");
        }
        if !["Smooth", "Pixel perfect"].contains(&self.scale.as_str()) {
            return Err("Choose Smooth or Pixel perfect preview scaling.");
        }
        let port = self
            .port
            .parse::<u16>()
            .map_err(|_| "UDP port must be from 1 to 65535.")?;
        if port == 0 || !self.port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("UDP port must be from 1 to 65535.");
        }
        if !["yellow", "red", "purple"].contains(&self.pixel_color.as_str()) {
            return Err("Choose Yellow, Red, or Purple for Pixel Seek.");
        }
        if !(1..=8192).contains(&self.crop_resolution) {
            return Err("Crop resolution must be from 1 to 8192 pixels.");
        }
        self.recoil.validate()?;
        self.auto.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{format_recoil_levels, parse_recoil_levels, AppSettings, AutoSettings};

    #[test]
    fn unavailable_object_detection_cannot_be_applied_from_the_ui() {
        let mut settings = super::AppSettings::default();
        settings.strategy = "object_detection".into();
        assert!(settings.validate().unwrap_err().contains("not available"));
        settings.strategy = "pixel_seek".into();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn auto_defaults_are_valid_and_separate_from_motion_settings() {
        let settings = AppSettings::default();
        assert_eq!(settings.auto, AutoSettings::default());
        assert!(settings.auto.validate().is_ok());
    }

    #[test]
    fn recoil_levels_parse_and_format_as_signed_integer_lists() {
        let levels = parse_recoil_levels("1, -2, 3").unwrap();
        assert_eq!(levels, [1, -2, 3]);
        assert_eq!(format_recoil_levels(&levels), "1, -2, 3");
        assert!(parse_recoil_levels("1, nope").is_err());
        assert!(parse_recoil_levels(&vec!["1"; 65].join(",")).is_err());
    }

    #[test]
    fn smoothing_bounds_stay_ordered_and_in_range() {
        use super::{AppSettings, SmoothingAxis::*, SmoothingBound::*};
        let mut settings = AppSettings::default();
        assert_eq!(settings.smoothing(X, Min), 1.0);
        settings.set_smoothing(X, Max, 6.0);
        settings.set_smoothing(X, Min, 3.0);
        assert_eq!(
            (settings.smoothing_x_min, settings.smoothing_x_max),
            (3.0, 6.0)
        );
        // Raising Min past Max carries Max along, and the reverse.
        settings.set_smoothing(X, Min, 8.0);
        assert_eq!(
            (settings.smoothing_x_min, settings.smoothing_x_max),
            (8.0, 8.0)
        );
        settings.set_smoothing(X, Max, 2.0);
        assert_eq!(
            (settings.smoothing_x_min, settings.smoothing_x_max),
            (2.0, 2.0)
        );
        // Clamped to 1–50; Y untouched.
        settings.set_smoothing(X, Max, 75.0);
        assert_eq!(settings.smoothing_x_max, 50.0);
        settings.set_smoothing(X, Min, f32::NAN);
        assert_eq!(settings.smoothing_x_min, 2.0);
        assert_eq!(
            (settings.smoothing_y_min, settings.smoothing_y_max),
            (1.0, 1.0)
        );
        assert!(settings.validate().is_ok());
        settings.smoothing_y_min = 5.0;
        assert!(settings.validate().is_err(), "Min above Max");
    }
}
