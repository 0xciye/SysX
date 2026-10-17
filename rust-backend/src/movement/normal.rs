//! Normal motion (from the user's `normal_1.py` Normal mode): once per new
//! frame, move by the target error times the per-axis speed, divided by the
//! smoothing strength while the target is inside the Smooth FOV. Fractions of
//! a count carry over to the next frame. Nothing is learned: speed is what the
//! user sets, converted to device counts with the in-game sensitivity.
//!
//! Each frame is acted on once. The ticker runs every 4 ms on the latest
//! sample, so acting on every tick would re-send the same error until the
//! next frame arrives. The original's forced ±1 step for errors ≥ 3 px is
//! left out: with capture latency it is re-sent every frame before the first
//! one shows, and in simulation it made the aim hunt around the target.

/// Image pixels one device count turns the camera at sensitivity 1. The real
/// value depends on the game's yaw, its FOV and the capture resolution;
/// close to Valorant-like games at 1080p native capture. Speed absorbs the rest.
pub const PX_PER_COUNT_AT_SENS_1: f64 = 1.0;
/// No move once the error is this small (crop px).
pub const DEAD_ZONE_PX: f64 = 1.0;
/// Each axis closes at least this much per frame (or its whole error if
/// smaller), so the last few px never crawl or stall (user report). Small
/// enough that the frames still in flight cannot add up to a visible hunt.
pub const MIN_STEP_PX: f64 = 0.07;

/// Device counts per image pixel at `sens`: a higher sensitivity turns
/// farther per count, so it needs fewer counts.
pub fn counts_per_px(sens: f64) -> f64 {
    1.0 / (PX_PER_COUNT_AT_SENS_1 * sens.clamp(0.01, 10.0))
}

/// What one frame asks of Normal motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalStep {
    /// Target error in crop px (target minus center, offsets applied).
    pub error: (f64, f64),
    /// Speed per axis: the fraction of the error corrected per frame.
    pub speed: (f64, f64),
    /// Smoothing strength per axis (≥ 1), divides the speed inside the
    /// Smooth FOV.
    pub smooth: (f64, f64),
    /// Smooth FOV radius in crop px (0: no smoothing).
    pub smooth_fov_px: f64,
    pub counts_per_px: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NormalMotion {
    residual: (f64, f64),
    /// Last requested move per axis, to notice a reversal.
    last: (f64, f64),
}

impl NormalMotion {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Device counts for one new frame.
    pub fn step(&mut self, step: NormalStep) -> (i64, i64) {
        let (ex, ey) = step.error;
        if !(ex.is_finite() && ey.is_finite()) || ex.hypot(ey) <= DEAD_ZONE_PX {
            self.reset();
            return (0, 0);
        }
        // Smoothing is strongest at the Smooth FOV edge and fades to none at
        // the target, so it slows the approach without making the last px
        // take a long time.
        let distance = ex.hypot(ey);
        let closeness = if distance < step.smooth_fov_px {
            distance / step.smooth_fov_px
        } else {
            1.0
        };
        let inside = distance < step.smooth_fov_px;
        let axis = |error: f64, speed: f64, smooth: f64, residual: &mut f64, last: &mut f64| {
            let divisor = if inside {
                1.0 + (smooth.max(1.0) - 1.0) * closeness
            } else {
                1.0
            };
            let px = error * speed.max(0.0) / divisor;
            let px = px.signum() * px.abs().max(error.abs().min(MIN_STEP_PX));
            let wanted = px * step.counts_per_px;
            // A leftover fraction belongs to the old direction. (A negative
            // leftover after rounding up is normal and is kept.)
            if wanted * *last < 0.0 {
                *residual = 0.0;
            }
            *last = wanted;
            let sum = wanted + *residual;
            let out = sum.round();
            *residual = sum - out;
            out as i64
        };
        let x = axis(
            ex,
            step.speed.0,
            step.smooth.0,
            &mut self.residual.0,
            &mut self.last.0,
        );
        let y = axis(
            ey,
            step.speed.1,
            step.smooth.1,
            &mut self.residual.1,
            &mut self.last.1,
        );
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(error: (f64, f64)) -> NormalStep {
        NormalStep {
            error,
            speed: (0.1, 0.1),
            smooth: (1.0, 1.0),
            smooth_fov_px: 0.0,
            counts_per_px: 4.0,
        }
    }

    #[test]
    fn moves_the_error_times_speed_in_counts() {
        let mut motion = NormalMotion::default();
        assert_eq!(motion.step(step((100.0, -50.0))), (40, -20));
    }

    #[test]
    fn smoothing_divides_the_speed_only_inside_the_smooth_fov() {
        let mut motion = NormalMotion::default();
        let mut inside = step((100.0, 100.0));
        inside.smooth = (4.0, 2.0);
        assert_eq!(motion.step(inside), (40, 40), "outside: full speed");
        // 100√2 px away, at the edge of a 150 px Smooth FOV: 94 % of the
        // strength applies.
        inside.smooth_fov_px = 150.0;
        assert_eq!(motion.step(inside), (10, 21));
    }

    #[test]
    fn smoothing_fades_out_toward_the_target() {
        let at = |distance: f64| {
            let mut motion = NormalMotion::default();
            let mut step = step((distance, 0.0));
            step.smooth = (5.0, 5.0);
            step.smooth_fov_px = 20.0;
            step.counts_per_px = 100.0; // fine counts to see the rate
            motion.step(step).0 as f64 / (distance * 0.1 * 100.0)
        };
        // Fraction of the unsmoothed move: 1/5 at the edge, almost all near.
        assert!((at(19.9) - 1.0 / 4.98).abs() < 0.01, "{}", at(19.9));
        assert!((at(10.0) - 1.0 / 3.0).abs() < 0.01, "{}", at(10.0));
        assert!(at(2.0) >= 0.7, "1/1.4 at 2 px: {}", at(2.0));
    }

    #[test]
    fn the_last_pixels_close_at_a_minimum_rate() {
        let mut motion = NormalMotion::default();
        let mut slow = step((2.0, 0.0));
        slow.speed = (0.01, 0.01); // 0.02 px a frame without the floor
        slow.counts_per_px = 10.0;
        let total: i64 = (0..10).map(|_| motion.step(slow).0).sum();
        assert_eq!(total, 7, "{MIN_STEP_PX} px × 10 frames × 10 counts/px");
        let mut motion = NormalMotion::default();
        let mut tiny = step((1.2, 0.0));
        tiny.counts_per_px = 10.0;
        assert_eq!(motion.step(tiny).0, 1, "1.2 px × 0.1 is above the floor");
    }

    #[test]
    fn fractions_carry_over_and_reset_on_reversal_and_in_the_dead_zone() {
        let mut motion = NormalMotion::default();
        let small = step((1.5, 0.0));
        let total: i64 = (0..10).map(|_| motion.step(small).0).sum();
        assert_eq!(total, 6, "1.5 px × 0.1 × 4 = 0.6 counts per frame");
        assert_eq!(motion.step(step((0.5, 0.0))), (0, 0), "dead zone");
        assert_eq!(motion.residual, (0.0, 0.0));
        let mut reverse = NormalMotion {
            residual: (0.4, 0.0),
            last: (0.6, 0.0),
        };
        assert_eq!(reverse.step(step((-1.5, 0.0))).0, -1, "-0.6 rounds to -1");
        assert!((reverse.residual.0 - 0.4).abs() < 1e-9, "-0.6 - (-1) = 0.4");
    }

    #[test]
    fn a_higher_sensitivity_needs_fewer_counts() {
        assert!((counts_per_px(0.5) * 2.0 - counts_per_px(0.25)).abs() < 1e-12);
        assert!((counts_per_px(0.25) - 4.0).abs() < 1e-12);
    }
}
