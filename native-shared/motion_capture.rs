//! Timestamped physical-report capture contract. No timers synthesize input.
use serde::{Deserialize, Serialize};
pub const REPORT_COUNT: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CaptureRequest {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapturedProfile {
    pub capture_id: String,
    pub reports: Vec<[i16; 2]>,
    pub timestamps_us: Vec<u32>,
}

impl CapturedProfile {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.reports.len() != REPORT_COUNT || self.timestamps_us.len() != REPORT_COUNT {
            return Err("Capture requires exactly 256 timestamped physical reports.");
        }
        if self.reports.iter().any(|p| p[1] == i16::MIN) {
            return Err("Captured Y counts cannot be inverted safely.");
        }
        if !self
            .timestamps_us
            .windows(2)
            .all(|t| (800..=1200).contains(&t[1].wrapping_sub(t[0])))
        {
            return Err(
                "Move the physical mouse continuously at 1,000 Hz; missing reports are not filled.",
            );
        }
        let span = self.timestamps_us[255].wrapping_sub(self.timestamps_us[0]);
        if !(255 * 980..=255 * 1020).contains(&span) {
            return Err("Captured reports are not at the required 1 ms cadence.");
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct PhysicalCapture {
    reports: Vec<[i16; 2]>,
    stamps: Vec<u32>,
    drops: Option<u64>,
}
impl PhysicalCapture {
    pub fn count(&self) -> usize {
        self.reports.len()
    }
    pub fn reset(&mut self) {
        self.reports.clear();
        self.stamps.clear();
    }
    /// Called only for real SDK Motion events stamped by HostChip. A loss or
    /// discontinuity discards the window, never pads or resamples it.
    pub fn push(&mut self, stamp: u32, report: [i16; 2], host_clock: bool, drops: u64) -> bool {
        let loss = self.drops.is_some_and(|previous| previous != drops);
        self.drops = Some(drops);
        if !host_clock || report[1] == i16::MIN {
            self.reset();
            return false;
        }
        if loss
            || self
                .stamps
                .last()
                .is_some_and(|last| !(800..=1200).contains(&stamp.wrapping_sub(*last)))
        {
            self.reset();
        }
        if self.reports.len() == REPORT_COUNT {
            return true;
        }
        self.reports.push(report);
        self.stamps.push(stamp);
        if self.count() == REPORT_COUNT {
            let span = stamp.wrapping_sub(self.stamps[0]);
            if !(255 * 980..=255 * 1020).contains(&span) {
                self.reset();
                self.reports.push(report);
                self.stamps.push(stamp);
            }
        }
        self.count() == REPORT_COUNT
    }
    pub fn finish(self, capture_id: String) -> CapturedProfile {
        CapturedProfile {
            capture_id,
            reports: self.reports,
            timestamps_us: self.stamps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_real_contiguous_reports_and_chip_clock_wrap() {
        let mut capture = PhysicalCapture::default();
        let start = u32::MAX - 5000;
        for i in 0..256u32 {
            assert_eq!(
                capture.push(start.wrapping_add(i * 1000), [i as i16, -1], true, 0),
                i == 255
            );
        }
        let result = capture.finish("session:1".into());
        result.validate().unwrap();
        assert_eq!(result.reports[255], [255, -1]);
    }
    #[test]
    fn never_fills_gaps_and_restarts_on_sdk_loss_or_invalid_clock() {
        let mut capture = PhysicalCapture::default();
        capture.push(0, [1, 2], true, 0);
        capture.push(1000, [2, 3], true, 0);
        capture.push(4000, [4, 5], true, 0);
        assert_eq!(capture.count(), 1);
        capture.push(5000, [5, 6], true, 1);
        assert_eq!(capture.count(), 1);
        capture.push(6000, [5, 6], false, 1);
        assert_eq!(capture.count(), 0);
        capture.push(7000, [5, i16::MIN], true, 1);
        assert_eq!(capture.count(), 0);
    }
    #[test]
    fn rejects_other_polling_rates_reordering_and_missing_reports() {
        for interval in [125, 500, 2000, 8000] {
            let mut capture = PhysicalCapture::default();
            for i in 0..600 {
                assert!(!capture.push(i * interval, [1, 1], true, 0));
            }
        }
        let mut capture = PhysicalCapture::default();
        capture.push(1000, [1, 1], true, 0);
        capture.push(1000, [1, 1], true, 0);
        assert_eq!(capture.count(), 1);
        capture.push(900, [1, 1], true, 0);
        assert_eq!(capture.count(), 1);
        let invalid = CapturedProfile {
            capture_id: "test".into(),
            reports: vec![[1, 1]; 256],
            timestamps_us: (0..256).map(|i| i * 2000).collect(),
        };
        assert!(invalid.validate().is_err());
    }
}
