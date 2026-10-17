//! NCAF (Nonlinear Close-Aim with Focus) speed factor: three zones from the
//! outside in.
//!
//! - outside `snap_radius`: full speed (1.0);
//! - between `snap_radius` and `near_radius`: linear 1.0 → `snap_boost`;
//! - inside `near_radius`: `snap_boost × (d / near_radius)^α`.
//!
//! The factor is continuous at both boundaries; the radii swap if given in
//! the wrong order.

pub fn ncaf_factor(
    distance: f64,
    snap_radius: f64,
    near_radius: f64,
    alpha: f64,
    snap_boost: f64,
) -> f64 {
    let (snap, near) = if snap_radius < near_radius {
        (near_radius, snap_radius)
    } else {
        (snap_radius, near_radius)
    };
    if distance > snap {
        return 1.0;
    }
    if distance > near {
        let gap = snap - near;
        if gap < 1e-6 {
            return snap_boost;
        }
        return 1.0 + ((snap - distance) / gap) * (snap_boost - 1.0);
    }
    if near > 1e-6 {
        snap_boost * (distance / near).powf(alpha.max(0.0))
    } else {
        snap_boost
    }
}

#[cfg(test)]
mod tests {
    use super::ncaf_factor;

    #[test]
    fn three_zones_are_continuous_and_swap_radii() {
        let f = |d| ncaf_factor(d, 150.0, 50.0, 1.5, 0.3);
        assert_eq!(f(200.0), 1.0);
        assert!((f(150.0) - 1.0).abs() < 1e-9);
        assert!((f(50.0) - 0.3).abs() < 1e-9);
        assert!((f(25.0) - 0.3 * 0.5_f64.powf(1.5)).abs() < 1e-9);
        assert_eq!(f(0.0), 0.0);
        assert_eq!(ncaf_factor(100.0, 50.0, 150.0, 1.5, 0.3), f(100.0));
    }
}
