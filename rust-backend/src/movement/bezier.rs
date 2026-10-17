//! Cubic Bézier curves used by Motion.
//!
//! - [`BezierPath`]: the stroke's path shape. A cubic from the aim to the
//!   target with control points offset perpendicular to the line (like the
//!   `ctrl_x/ctrl_y` offsets of a segmented Bézier move), drawn once per
//!   stroke. The controller is closed-loop, so instead of precomputing points
//!   it steers along the curve's tangent at the stroke's progress, re-anchored
//!   to the target every tick, so a moving target still bends the path.
//! - [`speed_response`]: a cubic speed curve applied to the step length.

use rand::{rngs::StdRng, Rng};

/// Point on a cubic Bézier curve at `t` in [0, 1].
pub fn cubic_point(t: f64, p: [(f64, f64); 4]) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0,
        a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1,
    )
}

/// Derivative of the cubic at `t`.
pub fn cubic_tangent(t: f64, p: [(f64, f64); 4]) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c) = (3.0 * u * u, 6.0 * u * t, 3.0 * t * t);
    (
        a * (p[1].0 - p[0].0) + b * (p[2].0 - p[1].0) + c * (p[3].0 - p[2].0),
        a * (p[1].1 - p[0].1) + b * (p[2].1 - p[1].1) + c * (p[3].1 - p[2].1),
    )
}

/// Perpendicular control-point offsets as fractions of the stroke length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BezierPath {
    pub ctrl1: f64,
    pub ctrl2: f64,
}

impl BezierPath {
    pub fn random(rng: &mut StdRng) -> Self {
        // Mostly bowed to one side, sometimes a gentle S.
        let side = if rng.random::<bool>() { 1.0 } else { -1.0 };
        Self {
            ctrl1: side * rng.random_range(0.04..=0.18),
            ctrl2: side * rng.random_range(-0.06..=0.14),
        }
    }

    /// Unit heading toward `error` (the target, aim at the origin) at stroke
    /// `progress` in [0, 1], with the bend scaled by `bend` in [0, 1].
    pub fn heading(&self, error: (f64, f64), progress: f64, bend: f64) -> (f64, f64) {
        let length = error.0.hypot(error.1);
        if length <= 1e-9 {
            return (0.0, 0.0);
        }
        let (ux, uy) = (error.0 / length, error.1 / length);
        let (nx, ny) = (-uy, ux);
        let offset = |fraction: f64, along: f64| {
            (
                ux * along * length + nx * fraction * bend * length,
                uy * along * length + ny * fraction * bend * length,
            )
        };
        let points = [
            (0.0, 0.0),
            offset(self.ctrl1, 1.0 / 3.0),
            offset(self.ctrl2, 2.0 / 3.0),
            error,
        ];
        let (tx, ty) = cubic_tangent(progress.clamp(0.0, 1.0), points);
        let norm = tx.hypot(ty);
        if norm <= 1e-9 {
            (ux, uy)
        } else {
            (tx / norm, ty / norm)
        }
    }
}

/// Cubic Bézier speed response applied to the step length (not per axis, so
/// it never bends the direction). `(y1, y2)` are chosen once per stroke.
pub fn speed_response(value: f64, scale: f64, strength: f64, (y1, y2): (f64, f64)) -> f64 {
    if value == 0.0 || scale <= 0.0 || strength <= 0.0 {
        return value;
    }
    let sign = value.signum();
    let linear = (value.abs() / scale).min(1.0);
    let (x1, x2): (f64, f64) = (0.25, 0.75);
    let (mut low, mut high): (f64, f64) = (0.0, 1.0);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let u = 1.0 - t;
        let x = 3.0 * u * u * t * x1 + 3.0 * u * t * t * x2 + t.powi(3);
        if x < linear {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    let u = 1.0 - t;
    let curved = 3.0 * u * u * t * y1 + 3.0 * u * t * t * y2 + t.powi(3);
    sign * (linear * (1.0 - strength) + curved * strength) * scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn cubic_endpoints_and_tangents() {
        let p = [(0.0, 0.0), (1.0, 2.0), (3.0, 2.0), (4.0, 0.0)];
        assert_eq!(cubic_point(0.0, p), (0.0, 0.0));
        assert_eq!(cubic_point(1.0, p), (4.0, 0.0));
        assert_eq!(cubic_tangent(0.0, p), (3.0, 6.0));
        assert_eq!(cubic_tangent(1.0, p), (3.0, -6.0));
    }

    #[test]
    fn the_path_bends_at_the_start_and_points_at_the_target_without_bend() {
        let path = BezierPath {
            ctrl1: 0.15,
            ctrl2: 0.1,
        };
        let straight = path.heading((100.0, 0.0), 0.0, 0.0);
        assert!((straight.0 - 1.0).abs() < 1e-9 && straight.1.abs() < 1e-9);
        let bent = path.heading((100.0, 0.0), 0.0, 1.0);
        assert!(bent.1 > 0.2 && bent.0 > 0.0, "{bent:?}");
        let late = path.heading((100.0, 0.0), 1.0, 1.0);
        assert!(
            late.1 < 0.0,
            "turns back toward the line near the end: {late:?}"
        );
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..100 {
            let random = BezierPath::random(&mut rng);
            assert!(random.ctrl1.abs() <= 0.18 && random.ctrl2.abs() <= 0.18);
        }
    }

    #[test]
    fn speed_response_keeps_sign_and_scale() {
        assert_eq!(speed_response(0.0, 10.0, 0.35, (0.1, 0.9)), 0.0);
        assert!((speed_response(10.0, 10.0, 0.35, (0.1, 0.9)) - 10.0).abs() < 1e-3);
        assert!(speed_response(-5.0, 10.0, 0.35, (0.1, 0.9)) < 0.0);
    }
}
