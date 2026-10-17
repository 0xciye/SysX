//! Target switch hysteresis: a target that jumps farther than
//! `SWITCH_RADIUS_PX` from where the current one is predicted to be is only
//! adopted once it has been seen in about the same place for
//! `SWITCH_CONFIRM_FRAMES` frames in a row. Single-frame jumps (a flickering
//! top pixel, another color blob for one frame) are ignored instead of
//! jerking the aim. The EMA part of this idea is the per-axis Smoothing
//! (strength 1–50) applied by the controller.

pub const SWITCH_RADIUS_PX: f64 = 18.0;
pub const SWITCH_CONFIRM_FRAMES: u32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SwitchConfirm {
    candidate: Option<(f64, f64)>,
    count: u32,
}

impl SwitchConfirm {
    /// Whether `observed` may be used, given where the current target is
    /// `predicted` to be in the same frame.
    pub fn accept(&mut self, observed: (f64, f64), predicted: (f64, f64)) -> bool {
        let near = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1) <= SWITCH_RADIUS_PX;
        if near(observed, predicted) {
            *self = Self::default();
            return true;
        }
        match self.candidate {
            Some(candidate) if near(observed, candidate) => self.count += 1,
            _ => self.count = 1,
        }
        self.candidate = Some(observed);
        if self.count >= SWITCH_CONFIRM_FRAMES {
            *self = Self::default();
            true
        } else {
            false
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_frame_jump_is_ignored_and_a_steady_one_is_adopted() {
        let mut gate = SwitchConfirm::default();
        assert!(gate.accept((5.0, 0.0), (0.0, 0.0)), "nearby: accepted");
        assert!(!gate.accept((60.0, 0.0), (0.0, 0.0)), "one-frame jump");
        assert!(gate.accept((1.0, 0.0), (0.0, 0.0)), "back to the target");
        assert!(!gate.accept((60.0, 0.0), (0.0, 0.0)));
        assert!(!gate.accept((62.0, 1.0), (0.0, 0.0)));
        assert!(gate.accept((63.0, 1.0), (0.0, 0.0)), "third frame in place");
        // Jumps that do not agree with each other never confirm.
        assert!(!gate.accept((60.0, 0.0), (0.0, 0.0)));
        assert!(!gate.accept((-60.0, 0.0), (0.0, 0.0)));
        assert!(!gate.accept((60.0, 30.0), (0.0, 0.0)));
    }
}
