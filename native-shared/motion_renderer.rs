//! Persisted opt-in texture settings shared by backend and configuration UI.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct MotionRendererSettings {
    pub enabled: bool,
    pub sample_interval_us: u32,
    /// Exactly 256 chronological genuine physical reports in native X-right,
    /// Y-down counts. Empty until automatically captured; never synthesized.
    pub reports: Vec<[i16; 2]>,
}

impl Default for MotionRendererSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_interval_us: 1000,
            reports: Vec::new(),
        }
    }
}

impl MotionRendererSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.sample_interval_us != 1000 {
            return Err("ABCurves requires physical reports sampled at exactly 1 ms.");
        }
        if !self.reports.is_empty() && self.reports.len() != 256 {
            return Err("ABCurves needs exactly 256 chronological physical mouse reports.");
        }
        if self.reports.iter().any(|report| report[1] == i16::MIN) {
            return Err("ABCurves profile Y counts must be from -32767 to 32767.");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_settings_keep_normal_until_a_physical_profile_is_captured() {
        let old: MotionRendererSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(old, MotionRendererSettings::default());
        old.validate().unwrap();
    }
    #[test]
    fn rejects_wrong_cadence_lengths_and_ranges() {
        for count in [255, 257] {
            let settings = MotionRendererSettings {
                enabled: true,
                reports: vec![[0, 0]; count],
                ..MotionRendererSettings::default()
            };
            assert!(settings.validate().is_err());
        }
        MotionRendererSettings::default().validate().unwrap();
        for interval in [0, 500, 4000] {
            let settings = MotionRendererSettings {
                enabled: true,
                sample_interval_us: interval,
                reports: vec![[0, 0]; 256],
            };
            assert!(settings.validate().is_err());
        }
        let settings = MotionRendererSettings {
            enabled: true,
            reports: vec![[i16::MAX, i16::MIN]; 256],
            ..MotionRendererSettings::default()
        };
        assert!(settings.validate().is_err());
    }
}
