#[path = "../../native-shared/motion_renderer.rs"]
pub mod motion_renderer;
pub use motion_renderer::MotionRendererSettings;

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
    pub motion_renderer: MotionRendererSettings,
    #[serde(default)]
    pub color_filter: bool,
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
    /// Normal motion speed per axis: fraction of the error moved per frame.
    pub motion_speed_x: f64,
    pub motion_speed_y: f64,
    #[serde(default)]
    pub motion_no_deadzone: bool,
    /// Inside this FOV the speed is divided by the smoothing strength.
    pub smooth_fov_degrees: u16,
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
            motion_renderer: MotionRendererSettings::default(),
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
            motion_speed_x: DEFAULT_MOTION_SPEED,
            motion_speed_y: DEFAULT_MOTION_SPEED,
            motion_no_deadzone: false,
            smooth_fov_degrees: 20,
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

/// Default Normal motion speed per axis (matches the backend).
pub const DEFAULT_MOTION_SPEED: f64 = 0.045;
pub const SMOOTHING_LIMITS: (f32, f32) = (1.0, 50.0);

/// The single runtime-apply payload used by both the native and web settings
/// surfaces. Persistence happens separately before this command is sent.
pub fn apply_settings_fields(
    settings: &AppSettings,
    has_model: bool,
    request_id: &str,
) -> serde_json::Value {
    let command = |kind: &str, fields: serde_json::Value| {
        let mut value = fields.as_object().cloned().unwrap_or_default();
        value.insert("version".into(), serde_json::json!(1));
        value.insert("type".into(), serde_json::json!(kind));
        serde_json::Value::Object(value)
    };
    let mut commands = vec![
        command(
            "set_udp_port",
            serde_json::json!({"port":settings.port.parse::<u16>().unwrap_or(4200)}),
        ),
        command(
            "set_pixel_color",
            serde_json::json!({"color":settings.pixel_color}),
        ),
        command(
            "set_display_config",
            serde_json::json!({"fov_degrees":settings.fov_degrees,"preview_scale":settings.scale}),
        ),
        command(
            "set_target_offset",
            serde_json::json!({"x":settings.offset_x,"y":settings.offset_y}),
        ),
        command(
            "set_trigger_button",
            serde_json::json!({"button":settings.trigger_button}),
        ),
        command(
            "set_smoothing_config",
            serde_json::json!({
                "x_min": settings.smoothing_x_min,
                "x_max": settings.smoothing_x_max,
                "y_min": settings.smoothing_y_min,
                "y_max": settings.smoothing_y_max,
            }),
        ),
        command(
            "set_motion_renderer",
            serde_json::json!({"settings": settings.motion_renderer}),
        ),
        command(
            "set_motion_speed",
            serde_json::json!({
                "x": settings.motion_speed_x,
                "y": settings.motion_speed_y,
                "smooth_fov_degrees": settings.smooth_fov_degrees,
                "no_deadzone": settings.motion_no_deadzone,
            }),
        ),
        command(
            "set_movement_calibration",
            serde_json::json!({"sens":settings.sens,"motion_enabled":settings.motion_enabled,"motion_always_on":settings.motion_always_on}),
        ),
        command(
            "set_color_filter",
            serde_json::json!({"enabled":settings.color_filter}),
        ),
        command(
            "set_recoil",
            serde_json::json!({"settings":settings.recoil}),
        ),
        command("set_auto", serde_json::json!({"settings":settings.auto})),
        command(
            "set_crop_resolution",
            serde_json::json!({"resolution":settings.crop_resolution}),
        ),
        command(
            "set_inference_config",
            serde_json::json!({"backend":settings.model_backend,"compute_target":settings.compute_target}),
        ),
    ];
    if settings.strategy == "pixel_seek" || has_model {
        commands.push(command(
            "set_strategy",
            serde_json::json!({"mode":settings.strategy}),
        ));
    }
    if has_model {
        if let Some(id) = settings.active_model_id.as_deref() {
            commands.push(command(
                "update_model",
                serde_json::json!({
                    "id":id,
                    "input_width":settings.model_width.parse::<u32>().unwrap_or(640),
                    "input_height":settings.model_height.parse::<u32>().unwrap_or(640),
                    "confidence":settings.model_confidence.parse::<f64>().unwrap_or(0.5),
                    "iou":settings.model_iou.parse::<f64>().unwrap_or(0.45)
                }),
            ));
            commands.push(command("activate_model", serde_json::json!({"id":id})));
        }
    }
    serde_json::json!({"commands":commands,"request_id":request_id})
}

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
        if !["pixel_seek", "object_detection"].contains(&self.strategy.as_str()) {
            return Err("Choose Pixel Seek or Object Detection.");
        }
        if self.strategy == "object_detection" && self.active_model_id.is_none() {
            return Err("Import a model bundle before selecting Object Detection.");
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
        if ![self.motion_speed_x, self.motion_speed_y]
            .iter()
            .all(|speed| speed.is_finite() && (0.01..=1.0).contains(speed))
        {
            return Err("Speed must be from 0.01 to 1.");
        }
        self.motion_renderer.validate()?;
        if self.smooth_fov_degrees > 360 {
            return Err("Smooth FOV must be from 0 to 360 degrees.");
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
        if !(1..=crate::protocol::MAX_CROP_RESOLUTION).contains(&self.crop_resolution) {
            return Err("Crop resolution exceeds the receiver's frame limit.");
        }
        self.recoil.validate()?;
        self.auto.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_settings_fields, format_recoil_levels, parse_recoil_levels, AppSettings, AutoSettings,
    };

    #[test]
    fn object_detection_needs_an_imported_model() {
        let mut settings = super::AppSettings::default();
        settings.strategy = "object_detection".into();
        assert!(settings.validate().unwrap_err().contains("model bundle"));
        settings.active_model_id = Some("yolo".into());
        assert!(settings.validate().is_ok());
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
    fn crop_resolution_stays_within_the_receiver_frame_budget() {
        let mut settings = AppSettings::default();
        settings.crop_resolution = crate::protocol::MAX_CROP_RESOLUTION;
        assert!(settings.validate().is_ok());
        settings.crop_resolution += 1;
        assert!(settings
            .validate()
            .unwrap_err()
            .contains("receiver's frame limit"));
    }

    #[test]
    fn shared_apply_payload_carries_runtime_settings_and_request_id() {
        let mut settings = AppSettings::default();
        settings.pixel_color = "purple".into();
        settings.color_filter = true;
        settings.fov_degrees = 240;
        let payload = apply_settings_fields(&settings, false, "native-apply-1");
        assert_eq!(payload["request_id"], "native-apply-1");
        let commands = payload["commands"].as_array().unwrap();
        let command = |kind: &str| commands.iter().find(|value| value["type"] == kind).unwrap();
        assert_eq!(command("set_pixel_color")["color"], "purple");
        assert_eq!(command("set_color_filter")["enabled"], true);
        assert_eq!(command("set_motion_speed")["no_deadzone"], false);
        assert_eq!(command("set_display_config")["fov_degrees"], 240);
        assert!(commands.iter().all(|value| value["version"] == 1));
    }

    #[test]
    fn no_deadzone_is_backward_compatible_and_reaches_apply_payload() {
        let legacy: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(!legacy.motion_no_deadzone);

        let mut enabled = AppSettings::default();
        enabled.motion_no_deadzone = true;
        let payload = apply_settings_fields(&enabled, false, "test");
        assert_eq!(payload["commands"][7]["no_deadzone"], true);
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

/// Adopt the backend's automatic sample save without overwriting unrelated
/// drafts (including a user's unsaved switch back to Normal).
pub fn merge_automatic_profile(
    draft: &mut AppSettings,
    applied: &mut AppSettings,
    incoming: &MotionRendererSettings,
    profile_index: u8,
) -> bool {
    if draft.active_profile != profile_index
        || applied.active_profile != profile_index
        || !applied.motion_renderer.enabled
        || !applied.motion_renderer.reports.is_empty()
        || !draft.motion_renderer.reports.is_empty()
        || !incoming.enabled
        || incoming.reports.len() != 256
    {
        return false;
    }
    draft.motion_renderer.reports = incoming.reports.clone();
    draft.motion_renderer.sample_interval_us = incoming.sample_interval_us;
    applied.motion_renderer = incoming.clone();
    true
}

#[cfg(test)]
mod automatic_profile_tests {
    use super::*;
    #[test]
    fn automatic_profile_preserves_drafts_and_adopts_only_valid_capture() {
        let mut applied = AppSettings::default();
        applied.motion_renderer.enabled = true;
        let mut draft = applied.clone();
        draft.motion_renderer.enabled = false;
        draft.motion_speed_x = 0.12;
        let incoming = MotionRendererSettings {
            enabled: true,
            reports: vec![[1, 2]; 256],
            ..MotionRendererSettings::default()
        };
        assert!(merge_automatic_profile(
            &mut draft,
            &mut applied,
            &incoming,
            0
        ));
        assert!(!draft.motion_renderer.enabled);
        assert_eq!(draft.motion_speed_x, 0.12);
        assert_eq!(applied.motion_renderer, incoming);
        assert_eq!(draft.motion_renderer.reports.len(), 256);
        assert!(!merge_automatic_profile(
            &mut draft,
            &mut applied,
            &incoming,
            1
        ));
        assert!(!merge_automatic_profile(
            &mut draft,
            &mut applied,
            &incoming,
            0
        ));
    }
}
