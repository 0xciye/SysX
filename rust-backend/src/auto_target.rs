//! Auto validity gate for one Pixel Seek target crossing the screen center.

use crate::pixel_seek::MAX_PIXEL_TRACK_SPEED_PX_PER_SECOND;
use std::time::{Duration, Instant};

const TARGET_WINDOW: Duration = Duration::from_millis(100);
/// Keeps a crossing observable to the host's millisecond-scale polling.
pub const CENTER_HOLD: Duration = Duration::from_millis(30);
/// Allow at most half a pixel of interpolation error at the screen center.
const CENTER_CROSS_TOLERANCE_PX: f32 = 0.5;

#[derive(Default)]
pub struct CenterCrossing {
    target: Option<([f32; 2], Instant)>,
    valid_until: Option<Instant>,
}

impl CenterCrossing {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keep the locked track across an occluded/missing-color frame. The
    /// caller uses this only while Pixel Seek still reports a fresh lock miss.
    pub fn hold_locked_target(&mut self, now: Instant) -> bool {
        if self
            .target
            .is_some_and(|(_, seen)| now.saturating_duration_since(seen) > TARGET_WINDOW)
        {
            self.target = None;
        }
        self.valid_until.is_some_and(|until| now < until)
    }

    /// A single selected color pixel must traverse the center. The caller
    /// resets this gate when Pixel Seek's target lock expires.
    pub fn update_target_pixel(
        &mut self,
        point: Option<[f32; 2]>,
        width: u32,
        height: u32,
        now: Instant,
    ) -> bool {
        let Some(point) = point.filter(|point| point.iter().all(|value| value.is_finite())) else {
            self.target = None;
            return self.valid_until.is_some_and(|until| now < until);
        };
        if width == 0 || height == 0 {
            self.reset();
            return false;
        }

        let center = [width as f32 / 2.0, height as f32 / 2.0];
        if self.target.is_some_and(|(previous, seen)| {
            let elapsed = now.saturating_duration_since(seen);
            elapsed <= TARGET_WINDOW
                && (point[0] - previous[0]).hypot(point[1] - previous[1])
                    > MAX_PIXEL_TRACK_SPEED_PX_PER_SECOND * elapsed.as_secs_f32()
        }) {
            // A discontinuous selected pixel starts a new track; do not let
            // it become the previous sample for a later center-crossing check.
            self.target = None;
            return self.valid_until.is_some_and(|until| now < until);
        }
        let crossed = self.target.is_some_and(|(previous, seen)| {
            let elapsed = now.saturating_duration_since(seen);
            if elapsed > TARGET_WINDOW {
                return false;
            }
            let delta = [point[0] - previous[0], point[1] - previous[1]];
            let crosses_center = |axis: usize, other: usize| {
                let from = previous[axis] - center[axis];
                let to = point[axis] - center[axis];
                // Require the selected pixel to change sides of center.
                if from == 0.0 || (to != 0.0 && from.signum() == to.signum()) {
                    return false;
                }
                let fraction = if to == 0.0 { 1.0 } else { -from / (to - from) };
                let other_at_center = previous[other] + fraction * delta[other];
                (other_at_center - center[other]).abs() <= CENTER_CROSS_TOLERANCE_PX
            };
            crosses_center(0, 1) || crosses_center(1, 0)
        });

        self.target = Some((point, now));
        if crossed {
            self.valid_until = Some(now + CENTER_HOLD);
        }
        self.valid_until.is_some_and(|until| now < until)
    }
}

#[cfg(test)]
mod tests {
    use super::{CenterCrossing, CENTER_HOLD};
    use std::time::{Duration, Instant};

    #[test]
    fn selected_pixel_crossing_center_arms_only_for_a_short_hold() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([30.5, 32.5]), 64, 64, start,));
        assert!(crossing.update_target_pixel(
            Some([33.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
        assert!(!crossing.update_target_pixel(
            None,
            64,
            64,
            start + Duration::from_millis(8) + CENTER_HOLD,
        ));
    }

    #[test]
    fn a_pixel_must_approach_the_exact_center_from_a_side() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([30.5, 31.5]), 63, 63, start,));
        assert!(crossing.update_target_pixel(
            Some([31.5, 31.5]),
            63,
            63,
            start + Duration::from_millis(8),
        ));
    }

    #[test]
    fn a_stationary_pixel_at_the_exact_center_does_not_arm_auto() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([31.5, 31.5]), 63, 63, start));
        assert!(!crossing.update_target_pixel(
            Some([31.5, 31.5]),
            63,
            63,
            start + Duration::from_millis(8),
        ));
        assert!(!crossing.update_target_pixel(
            Some([32.5, 31.5]),
            63,
            63,
            start + Duration::from_millis(16),
        ));
    }

    #[test]
    fn a_fresh_locked_pixel_can_cross_center_across_one_occluded_frame() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([30.5, 32.5]), 64, 64, start,));
        assert!(!crossing.hold_locked_target(start + Duration::from_millis(8)));
        assert!(crossing.update_target_pixel(
            Some([33.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(16),
        ));
    }

    #[test]
    fn a_fast_locked_pixel_can_cross_center_between_distant_frames() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([10.5, 32.5]), 64, 64, start,));
        assert!(crossing.update_target_pixel(
            Some([54.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
    }

    #[test]
    fn an_implausible_selected_pixel_jump_cannot_arm_auto() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([10.5, 32.5]), 64, 64, start,));
        assert!(!crossing.update_target_pixel(
            Some([54.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(1),
        ));
        assert!(crossing.target.is_none());
        assert!(!crossing.update_target_pixel(
            Some([54.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(2),
        ));
    }

    #[test]
    fn stationary_or_off_center_color_does_not_trigger() {
        let start = Instant::now();
        let mut stationary = CenterCrossing::default();
        assert!(!stationary.update_target_pixel(Some([28.5, 32.5]), 64, 64, start));
        assert!(!stationary.update_target_pixel(
            Some([28.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));

        let mut off_center = CenterCrossing::default();
        assert!(!off_center.update_target_pixel(Some([30.5, 24.5]), 64, 64, start,));
        assert!(!off_center.update_target_pixel(
            Some([33.5, 24.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
    }

    #[test]
    fn near_miss_past_center_does_not_trigger() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([30.5, 28.5]), 64, 64, start,));
        // Neither axis crossing comes within half a pixel of the center.
        assert!(!crossing.update_target_pixel(
            Some([35.5, 33.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
    }

    #[test]
    fn center_crossing_uses_the_selected_pixel_not_blob_geometry() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([27.5, 32.5]), 64, 64, start,));
        // Blob association is owned by Pixel Seek; this gate sees only the
        // selected pixel coordinates.
        assert!(crossing.update_target_pixel(
            Some([35.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
    }

    #[test]
    fn separate_blobs_and_lost_targets_cannot_complete_a_crossing() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([12.5, 32.5]), 64, 64, start,));
        assert!(!crossing.update_target_pixel(None, 64, 64, start + Duration::from_millis(8),));
        assert!(!crossing.update_target_pixel(
            Some([51.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(16),
        ));
    }

    #[test]
    fn reset_prevents_a_crossing_from_spanning_an_auto_trigger_release() {
        let start = Instant::now();
        let mut crossing = CenterCrossing::default();
        assert!(!crossing.update_target_pixel(Some([30.5, 32.5]), 64, 64, start,));
        crossing.reset();
        assert!(!crossing.update_target_pixel(
            Some([33.5, 32.5]),
            64,
            64,
            start + Duration::from_millis(8),
        ));
    }
}
