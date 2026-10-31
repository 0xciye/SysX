//! Motion step: the confirmed Normal step of `stable/motion-2026-10-18`
//! (user request, 2026-11-01: go back to the old movement and keep the new
//! target selection). Once per new frame each axis moves the target error
//! times its Speed, divided by the smoothing strength while the target is
//! inside the Smooth FOV; the strength fades out toward the target along
//! (distance / radius)³, so it slows the entry without making the last px
//! slow. The strength is drawn from `[min, max]` once per engagement (stable
//! drew it per track). The optional distance speed booster (Short/Medium/Long)
//! multiplies the step. A 1 px dead zone (off with No deadzone) and a
//! `MIN_STEP_PX` floor per axis keep the last px from stalling; fractions of a
//! count carry over to the next frame (dropped on reversal).

use crate::targeting::SmoothingRange;
use rand::{rngs::StdRng, Rng, SeedableRng};

/// Image pixels one device count turns the camera at sensitivity 1. The real
/// value depends on the game's yaw, its FOV and the capture resolution;
/// Speed absorbs the rest.
pub const PX_PER_COUNT_AT_SENS_1: f64 = 1.0;
/// Default Speed per axis (stable `DEFAULT_SPEED`).
pub const DEFAULT_SPEED: f64 = 0.045;
/// No move once the error is this small (crop px), unless No deadzone.
pub const DEAD_ZONE_PX: f64 = 1.0;
/// Each axis closes at least this much per frame (or its whole error if
/// smaller), so the last few px never crawl or stall.
pub const MIN_STEP_PX: f64 = 0.07;

/// Device counts per image pixel at `sens`: a higher sensitivity turns
/// farther per count, so it needs fewer counts.
pub fn counts_per_px(sens: f64) -> f64 {
    1.0 / (PX_PER_COUNT_AT_SENS_1 * sens.clamp(0.01, 10.0))
}

/// Speed multipliers near (< 30 px), mid (30–80 px) and far (≥ 200 px),
/// interpolated linearly between.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeedBooster {
    pub near: f64,
    pub mid: f64,
    pub far: f64,
}

impl SpeedBooster {
    pub fn multiplier(&self, distance: f64) -> f64 {
        if distance < 30.0 {
            self.near
        } else if distance <= 80.0 {
            self.near + (distance - 30.0) / 50.0 * (self.mid - self.near)
        } else {
            let factor = ((distance - 80.0) / 120.0).clamp(0.0, 1.0);
            self.mid + factor * (self.far - self.mid)
        }
    }
}

/// What one frame asks of Motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionStep {
    /// Target error in crop px (target minus center, offsets applied).
    pub error: (f64, f64),
    /// Fraction of the error corrected per frame, per axis.
    pub speed: (f64, f64),
    /// Smoothing strength ranges (1 = none) used inside the Smooth FOV.
    pub smoothing: (SmoothingRange, SmoothingRange),
    /// Smooth FOV radius in crop px (0: no smoothing).
    pub smooth_fov_px: f64,
    pub booster: Option<SpeedBooster>,
    pub no_deadzone: bool,
    pub counts_per_px: f64,
}

pub struct Motion {
    rng: StdRng,
    residual: (f64, f64),
    /// Last requested move per axis, to notice a reversal.
    last: (f64, f64),
    /// Strength drawn for this engagement, with the ranges it came from.
    drawn: Option<((SmoothingRange, SmoothingRange), (f64, f64))>,
}

impl Default for Motion {
    fn default() -> Self {
        Self::with_rng(StdRng::from_os_rng())
    }
}

impl Motion {
    pub fn with_rng(rng: StdRng) -> Self {
        Self {
            rng,
            residual: (0.0, 0.0),
            last: (0.0, 0.0),
            drawn: None,
        }
    }

    /// Target lost or Motion inactive: the next engagement draws again.
    pub fn reset(&mut self) {
        self.residual = (0.0, 0.0);
        self.last = (0.0, 0.0);
        self.drawn = None;
    }

    fn draw(&mut self, range: SmoothingRange) -> f64 {
        let value = if range.max > range.min {
            self.rng.random_range(range.min..=range.max)
        } else {
            range.min
        };
        f64::from(value).max(1.0)
    }

    /// The strength of this engagement, drawn once (again when the ranges
    /// change).
    fn strength(&mut self, ranges: (SmoothingRange, SmoothingRange)) -> (f64, f64) {
        match self.drawn {
            Some((drawn_ranges, values)) if drawn_ranges == ranges => values,
            _ => {
                let values = (self.draw(ranges.0), self.draw(ranges.1));
                self.drawn = Some((ranges, values));
                values
            }
        }
    }

    /// Step in px per axis for one frame, before conversion to counts.
    pub fn step_px(&mut self, step: &MotionStep) -> (f64, f64) {
        let (ex, ey) = step.error;
        if !(ex.is_finite() && ey.is_finite()) {
            return (0.0, 0.0);
        }
        let distance = ex.hypot(ey);
        if !step.no_deadzone && distance <= DEAD_ZONE_PX {
            return (0.0, 0.0);
        }
        let strength = self.strength(step.smoothing);
        let inside = distance < step.smooth_fov_px;
        let closeness = if inside {
            (distance / step.smooth_fov_px).powi(3)
        } else {
            1.0
        };
        let mult = step
            .booster
            .map_or(1.0, |booster| booster.multiplier(distance));
        let axis = |error: f64, speed: f64, smooth: f64| {
            if error == 0.0 {
                return 0.0;
            }
            let divisor = if inside {
                1.0 + (smooth - 1.0) * closeness
            } else {
                1.0
            };
            let px = error * speed.max(0.0) * mult / divisor;
            px.signum() * px.abs().max(error.abs().min(MIN_STEP_PX))
        };
        (
            axis(ex, step.speed.0, strength.0),
            axis(ey, step.speed.1, strength.1),
        )
    }

    /// Device counts for one new frame.
    pub fn step(&mut self, step: MotionStep) -> (i64, i64) {
        let (x, y) = self.step_px(&step);
        let axis = |px: f64, residual: &mut f64, last: &mut f64| {
            if px == 0.0 {
                *residual = 0.0;
                *last = 0.0;
                return 0;
            }
            let wanted = px * step.counts_per_px;
            // A leftover fraction belongs to the old direction.
            if wanted * *last < 0.0 {
                *residual = 0.0;
            }
            *last = wanted;
            let sum = wanted + *residual;
            let out = sum.round();
            *residual = sum - out;
            out as i64
        };
        (
            axis(x, &mut self.residual.0, &mut self.last.0),
            axis(y, &mut self.residual.1, &mut self.last.1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(value: f32) -> SmoothingRange {
        SmoothingRange {
            min: value,
            max: value,
        }
    }

    fn step(error: (f64, f64)) -> MotionStep {
        MotionStep {
            error,
            speed: (0.1, 0.1),
            smoothing: (fixed(1.0), fixed(1.0)),
            smooth_fov_px: 0.0,
            booster: None,
            no_deadzone: false,
            counts_per_px: 4.0,
        }
    }

    fn motion() -> Motion {
        Motion::with_rng(StdRng::seed_from_u64(7))
    }

    #[test]
    fn moves_the_error_times_speed_in_counts() {
        assert_eq!(motion().step(step((100.0, -50.0))), (40, -20));
    }

    #[test]
    fn smoothing_divides_the_speed_only_inside_the_smooth_fov_fading_out() {
        let mut inside = step((100.0, 100.0));
        inside.smoothing = (fixed(4.0), fixed(2.0));
        assert_eq!(motion().step(inside), (40, 40), "no Smooth FOV: full speed");
        // 100√2 px away in a 150 px Smooth FOV: 0.943³ = 84 % of the
        // strength applies (stable's figures).
        inside.smooth_fov_px = 150.0;
        assert_eq!(motion().step(inside), (11, 22));
    }

    #[test]
    fn the_strength_is_drawn_once_per_engagement() {
        let mut motion = motion();
        let mut input = step((10.0, 0.0));
        input.smooth_fov_px = 1000.0;
        input.smoothing.0 = SmoothingRange {
            min: 2.0,
            max: 40.0,
        };
        let mut engagements = Vec::new();
        for _ in 0..20 {
            let first = motion.step_px(&input).0;
            assert_eq!(motion.step_px(&input).0, first, "steady within one");
            engagements.push(first);
            motion.reset();
        }
        assert!(engagements.iter().any(|px| (px - engagements[0]).abs() > 1e-9));
    }

    #[test]
    fn a_dead_zone_and_a_minimum_step_keep_the_last_px_moving() {
        assert_eq!(motion().step_px(&step((0.6, 0.5))), (0.0, 0.0), "within 1 px");
        let mut no_deadzone = step((0.6, 0.0));
        no_deadzone.no_deadzone = true;
        assert!((motion().step_px(&no_deadzone).0 - 0.07).abs() < 1e-9);
        // 0.3 × 0.1 = 0.03 px becomes the 0.07 px floor.
        assert!((motion().step_px(&step((2.0, 0.3))).1 - 0.07).abs() < 1e-9);
    }

    #[test]
    fn the_speed_booster_scales_the_step() {
        let booster = SpeedBooster {
            near: 0.5,
            mid: 1.0,
            far: 2.0,
        };
        assert_eq!(booster.multiplier(10.0), 0.5);
        assert_eq!(booster.multiplier(55.0), 0.75);
        assert_eq!(booster.multiplier(80.0), 1.0);
        assert_eq!(booster.multiplier(140.0), 1.5);
        assert_eq!(booster.multiplier(500.0), 2.0);
        let mut boosted = step((140.0, 0.0));
        boosted.booster = Some(booster);
        assert!((motion().step_px(&boosted).0 - 21.0).abs() < 1e-9, "14 × 1.5");
    }

    #[test]
    fn fractions_carry_over_and_reset_on_reversal() {
        let mut motion = motion();
        let small = MotionStep {
            counts_per_px: 1.0,
            ..step((4.0, 0.0)) // 0.4 px a frame
        };
        let total: i64 = (0..10).map(|_| motion.step(small).0).sum();
        assert_eq!(total, 4);
        motion.residual = (0.4, 0.0);
        motion.last = (0.4, 0.0);
        let back = MotionStep {
            counts_per_px: 1.0,
            ..step((-4.0, 0.0))
        };
        assert_eq!(motion.step(back).0, 0, "-0.4 alone rounds to 0");
        assert!((motion.residual.0 + 0.4).abs() < 1e-9);
    }

    #[test]
    fn a_higher_sensitivity_needs_fewer_counts() {
        assert!((counts_per_px(0.5) * 2.0 - counts_per_px(0.25)).abs() < 1e-12);
        assert!((counts_per_px(0.25) - 4.0).abs() < 1e-12);
    }
}
