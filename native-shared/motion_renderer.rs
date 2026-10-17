//! Persisted opt-in texture settings shared by backend and configuration UI.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct MotionRendererSettings {
    pub enabled: bool,
    pub sample_interval_us: u32,
    /// Exactly 256 chronological genuine physical reports in native X-right,
    /// Y-down counts. Empty until explicitly imported; never synthesized.
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
        if (self.enabled || !self.reports.is_empty()) && self.reports.len() != 256 {
            return Err("Import exactly 256 chronological physical mouse reports for ABCurves.");
        }
        if self.reports.iter().any(|report| report[1] == i16::MIN) {
            return Err("ABCurves profile Y counts must be from -32767 to 32767.");
        }
        Ok(())
    }
}

/// Import never enables output implicitly. Apply Changes remains the only save.
pub fn parse_profile(text: &str) -> Result<MotionRendererSettings, &'static str> {
    if text.len() > 16 * 1024 {
        return Err("ABCurves profile must be at most 16 KiB.");
    }
    let mut profile: MotionRendererSettings = serde_json::from_str(text)
        .map_err(|_| "Invalid ABCurves profile JSON or signed 16-bit reports.")?;
    if profile.reports.len() != 256 {
        return Err("ABCurves needs exactly 256 physical reports.");
    }
    profile.enabled = false;
    profile.validate()?;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_settings_keep_normal_and_import_cannot_enable_output() {
        let old: MotionRendererSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(old, MotionRendererSettings::default());
        let text = serde_json::json!({"enabled":true,"sampleIntervalUs":1000,
            "reports": vec![[1i16,-2i16];256]})
        .to_string();
        let imported = parse_profile(&text).unwrap();
        assert!(!imported.enabled);
        assert_eq!(imported.reports.len(), 256);
    }
    #[test]
    fn rejects_wrong_cadence_lengths_and_ranges() {
        for count in [0, 255, 257] {
            assert!(
                parse_profile(&serde_json::json!({"reports":vec![[0,0];count]}).to_string())
                    .is_err()
            );
        }
        for interval in [0, 500, 4000] {
            assert!(parse_profile(
                &serde_json::json!({"sampleIntervalUs":interval,
                "reports":vec![[0,0];256]})
                .to_string()
            )
            .is_err());
        }
        assert!(
            parse_profile(&serde_json::json!({"reports":vec![[32768,0];256]}).to_string()).is_err()
        );
    }
}
