//! WindMouse "wind": a random force that carries over between ticks
//! (`w = w / √3 + uniform(−1, 1) × strength`), reduced inside a distance
//! threshold so the final approach stays precise. The WindMouse "gravity"
//! is the controller's NCAF-shaped pull toward the target; its momentum,
//! reaction delay, fatigue shake and random micro-corrections are left out
//! (they slow the aim or make it shake).

use rand::{rngs::StdRng, Rng};

/// Wind as a fraction of the step, per axis.
pub const WIND_STRENGTH: f64 = 0.12;
/// Inside this distance (px) the wind drops to `NEAR_WIND_SCALE` of itself.
pub const WIND_DISTANCE_THRESHOLD_PX: f64 = 50.0;
pub const NEAR_WIND_SCALE: f64 = 0.3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Wind(pub f64, pub f64);

impl Wind {
    /// Advances one tick and returns the wind (fraction of the step).
    pub fn advance(&mut self, rng: &mut StdRng, distance: f64) -> (f64, f64) {
        let strength = if distance < WIND_DISTANCE_THRESHOLD_PX {
            WIND_STRENGTH * NEAR_WIND_SCALE
        } else {
            WIND_STRENGTH
        };
        let sqrt3 = 3.0_f64.sqrt();
        self.0 = self.0 / sqrt3 + rng.random_range(-1.0..=1.0) * strength;
        self.1 = self.1 / sqrt3 + rng.random_range(-1.0..=1.0) * strength;
        (self.0, self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn wind_is_bounded_mean_free_and_calmer_near_the_target() {
        let mut rng = StdRng::seed_from_u64(3);
        let stats = |rng: &mut StdRng, distance: f64| {
            let mut wind = Wind::default();
            let samples: Vec<f64> = (0..20_000).map(|_| wind.advance(rng, distance).0).collect();
            let mean = samples.iter().sum::<f64>() / samples.len() as f64;
            let max = samples.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
            (mean, max)
        };
        let (mean, far_max) = stats(&mut rng, 200.0);
        assert!(mean.abs() < 0.01, "{mean}");
        // |w| ≤ s / (1 − 1/√3) ≈ 2.37 s.
        assert!(far_max <= WIND_STRENGTH * 2.37, "{far_max}");
        let (_, near_max) = stats(&mut rng, 10.0);
        assert!(near_max < far_max * 0.5, "{near_max} vs {far_max}");
    }
}
