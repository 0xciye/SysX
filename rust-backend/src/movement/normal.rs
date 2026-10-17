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
    /// The target is inside the Smooth FOV.
    pub in_smooth_fov: bool,
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
        let axis = |error: f64, speed: f64, smooth: f64, residual: &mut f64, last: &mut f64| {
            let divisor = if step.in_smooth_fov {
                smooth.max(1.0)
            } else {
                1.0
            };
            let wanted = error * speed.max(0.0) / divisor * step.counts_per_px;
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
            in_smooth_fov: false,
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
        inside.in_smooth_fov = true;
        assert_eq!(motion.step(inside), (10, 20));
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
