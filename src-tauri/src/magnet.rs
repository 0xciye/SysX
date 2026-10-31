//! Magnet (ported from the user's `magnet.rs`): while its trigger is held,
//! pulls toward the target and fires. Modes:
//!
//! Mode logic follows the user's source `magnet.rs` (user request,
//! 2026-10-31: "do it like the original file"):
//!
//! - `spray`: press while a target is present and pull every frame by
//!   error / smoothing (device counts = px, as in the source); release on
//!   target loss (ready again at once) or after `forced_release_ms`, which
//!   is followed by Cooldown.
//! - `tap`: move error × 0.45 and press; release after `fire_hold_ms`, then
//!   wait `reset_timeout_ms` before the next tap.
//! - `silent_proximity`: once the target is within `flick_proximity_px`, flick
//!   the whole error at once (`pixel_to_counts`: px / (sens × 0.022 × 3.125))
//!   and press; release after `fire_hold_ms`, move back by the same amount
//!   and be ready at once.
//! - `silent_immediate`: the same flick at any distance.
//! - `auto`: center-crossing clicks, handled by `auto.rs`; nothing here.
//!
//! Added at the user's request on top of the source: Delay (target present
//! this long before firing) and Sleep (evaluate at most this often) for every
//! mode, and for spray a smoothing drawn in [min, max] per axis times the
//! distance speed booster (the 3-zone booster of Motion).
//!
//! The state machine only plans operations; the device loop executes them
//! one at a time through the serial queue.

use serde::Deserialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A sample older than this is no target (Magnet disabled, Auto owning
/// routing, or processing stalled).
const FRESH_MS: u64 = 100;
/// Largest move per device command.
const MAX_STEP: i64 = 127;
/// Tap pull: this fraction of the error (source `tick_tap`).
const TAP_PULL: f64 = 0.45;
/// Degrees per count at sensitivity 1 (source `pixel_to_counts`: 0.022 × 3.125).
const DEGREES_PER_COUNT: f64 = 0.022 * 3.125;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MagnetSettings {
    pub enabled: bool,
    pub mode: String,
    pub trigger_button: String,
    pub delay_ms: u64,
    pub cooldown_ms: u64,
    pub sleep_ms: u64,
    pub fire_hold_ms: u64,
    pub forced_release_ms: u64,
    /// Tap: wait after a release before the next tap.
    pub reset_timeout_ms: u64,
    pub flick_proximity_px: f64,
    pub smooth_x_min: f64,
    pub smooth_x_max: f64,
    pub smooth_y_min: f64,
    pub smooth_y_max: f64,
    pub speed_booster: bool,
    pub speed_near: f64,
    pub speed_mid: f64,
    pub speed_far: f64,
}

impl MagnetSettings {
    /// Distance speed booster (user's `motion.rs`); 1 when off.
    fn boost(&self, distance: f64) -> f64 {
        if !self.speed_booster {
            1.0
        } else if distance < 30.0 {
            self.speed_near
        } else if distance <= 80.0 {
            self.speed_near + (distance - 30.0) / 50.0 * (self.speed_mid - self.speed_near)
        } else {
            let t = ((distance - 80.0) / 120.0).clamp(0.0, 1.0);
            self.speed_mid + t * (self.speed_far - self.speed_mid)
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct MagnetTargetEvent {
    #[serde(default)]
    pub config_revision: u64,
    pub frame_id: u64,
    /// Target minus crosshair in crop px; `None` when no target.
    pub error: Option<[f64; 2]>,
    pub counts_per_px: f64,
    pub timestamp_ms: u64,
    pub settings: MagnetSettings,
}

impl MagnetTargetEvent {
    fn is_fresh_at(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.timestamp_ms) <= FRESH_MS
            && self.timestamp_ms <= now_ms.saturating_add(FRESH_MS)
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MagnetOp {
    Move(i16, i16),
    Press,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Idle,
    Spraying(Instant),
    Holding(Instant),
    Flicked { at: Instant, back: (i64, i64) },
    /// Waiting this long after a release before the next shot.
    Cooling(Instant, Duration),
}

/// XorShift64 for the per-pull smoothing draw (the source's `FastRng`).
struct FastRng(u64);

impl FastRng {
    fn seeded() -> Self {
        let mut bytes = [0u8; 8];
        let seed = getrandom::fill(&mut bytes)
            .map(|_| u64::from_le_bytes(bytes))
            .unwrap_or_else(|_| unix_ms());
        Self(seed.max(1))
    }

    fn range(&mut self, min: f64, max: f64) -> f64 {
        if max <= min {
            return min;
        }
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        min + (self.0 >> 11) as f64 / (1u64 << 53) as f64 * (max - min)
    }
}

pub struct MagnetState {
    phase: Phase,
    last_frame: Option<u64>,
    /// When the current target was first seen (for Delay).
    seen_at: Option<Instant>,
    next_eval: Option<Instant>,
    residual: (f64, f64),
    /// The left button is pressed by Magnet (until its Release is sent).
    holding: bool,
    ops: VecDeque<MagnetOp>,
    rng: FastRng,
}

impl Default for MagnetState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            last_frame: None,
            seen_at: None,
            next_eval: None,
            residual: (0.0, 0.0),
            holding: false,
            ops: VecDeque::new(),
            rng: FastRng::seeded(),
        }
    }
}

impl MagnetState {
    /// Magnet holds the left button (Recoil follows it like a physical press).
    pub fn holding(&self) -> bool {
        self.holding
    }

    /// The next device operation, if any.
    pub fn next_op(&mut self) -> Option<MagnetOp> {
        self.ops.pop_front()
    }

    fn push_move(&mut self, mut dx: i64, mut dy: i64) {
        while dx != 0 || dy != 0 {
            let x = dx.clamp(-MAX_STEP, MAX_STEP);
            let y = dy.clamp(-MAX_STEP, MAX_STEP);
            self.ops.push_back(MagnetOp::Move(x as i16, y as i16));
            dx -= x;
            dy -= y;
        }
    }

    fn press(&mut self) {
        if !self.holding {
            self.holding = true;
            self.ops.push_back(MagnetOp::Press);
        }
    }

    fn release(&mut self) {
        if self.holding {
            self.holding = false;
            self.ops.push_back(MagnetOp::Release);
        }
    }

    /// Rounds a move, carrying the fractions to the next one.
    fn counts(&mut self, x: f64, y: f64) -> (i64, i64) {
        let (x, y) = (x + self.residual.0, y + self.residual.1);
        let out = (x.round(), y.round());
        self.residual = (x - out.0, y - out.1);
        (out.0 as i64, out.1 as i64)
    }

    /// Spray pull: error / smoothing (drawn per axis) × booster. Like the
    /// source, the error in px is sent as device counts.
    fn pull(&mut self, error: [f64; 2], event: &MagnetTargetEvent) -> (i64, i64) {
        let settings = &event.settings;
        let boost = settings.boost(error[0].hypot(error[1]));
        let smooth_x = self
            .rng
            .range(settings.smooth_x_min, settings.smooth_x_max)
            .max(0.1);
        let smooth_y = self
            .rng
            .range(settings.smooth_y_min, settings.smooth_y_max)
            .max(0.1);
        self.counts(error[0] / smooth_x * boost, error[1] / smooth_y * boost)
    }

    /// Silent flick: the whole error at once through the source's
    /// `pixel_to_counts` (px / (sens × 0.022 × 3.125)); `counts_per_px` is
    /// 1 / sens.
    fn flick(&mut self, error: [f64; 2], event: &MagnetTargetEvent) -> (i64, i64) {
        let per_px = if event.counts_per_px.abs() < 1e-6 {
            1.0
        } else {
            event.counts_per_px / DEGREES_PER_COUNT
        };
        self.counts(error[0] * per_px, error[1] * per_px)
    }

    /// Trigger released or Magnet off: drop planned moves, release the
    /// button and forget the cycle (no snap back).
    pub fn cancel(&mut self) {
        self.ops.retain(|op| *op == MagnetOp::Release);
        self.release();
        self.phase = Phase::Idle;
        self.last_frame = None;
        self.seen_at = None;
        self.next_eval = None;
        self.residual = (0.0, 0.0);
    }

    /// Ends a shot: release now and wait `wait` before the next one.
    fn finish(&mut self, now: Instant, wait: Duration) {
        self.release();
        // Fractions belong to one shot; the next starts clean.
        self.residual = (0.0, 0.0);
        self.phase = if wait.is_zero() {
            Phase::Idle
        } else {
            Phase::Cooling(now, wait)
        };
        self.seen_at = None;
    }

    /// One device-loop tick. `event` is the latest Magnet sample.
    pub fn update(
        &mut self,
        event: Option<&MagnetTargetEvent>,
        trigger_held: bool,
        now: Instant,
        now_ms: u64,
    ) {
        // Samples stop when Magnet is turned off or processing stalls; a
        // stale one is the same as Magnet off. Auto has its own state.
        let Some(event) = event.filter(|event| {
            event.settings.enabled && event.settings.mode != "auto" && event.is_fresh_at(now_ms)
        }) else {
            self.cancel();
            return;
        };
        if !trigger_held {
            self.cancel();
            return;
        }
        let settings = event.settings.clone();
        if self.next_eval.is_some_and(|next| now < next) {
            return;
        }
        self.next_eval = Some(now + Duration::from_millis(settings.sleep_ms));
        let new_frame = self.last_frame != Some(event.frame_id);
        self.last_frame = Some(event.frame_id);
        let target = event.error;
        let elapsed = |since: Instant| now.saturating_duration_since(since);
        let ms = Duration::from_millis;

        match self.phase {
            Phase::Cooling(at, wait) => {
                if elapsed(at) < wait {
                    return;
                }
                self.phase = Phase::Idle;
            }
            Phase::Holding(at) => {
                // Tap: release, then the source's reset timeout.
                if elapsed(at) >= ms(settings.fire_hold_ms) {
                    self.finish(now, ms(settings.reset_timeout_ms));
                }
                return;
            }
            Phase::Flicked { at, back } => {
                // Silent: release, move back (always; user request) and be
                // ready at once, as in the source.
                if elapsed(at) >= ms(settings.fire_hold_ms) {
                    self.finish(now, Duration::ZERO);
                    self.push_move(back.0, back.1);
                }
                return;
            }
            Phase::Spraying(at) => {
                if elapsed(at) >= ms(settings.forced_release_ms) {
                    self.finish(now, ms(settings.cooldown_ms));
                    return;
                }
                match target {
                    Some(error) => {
                        if new_frame {
                            let (dx, dy) = self.pull(error, event);
                            self.push_move(dx, dy);
                        }
                    }
                    // Target lost: release and be ready at once (source).
                    None => self.finish(now, Duration::ZERO),
                }
                return;
            }
            Phase::Idle => {}
        }

        // Idle: a target must be present for Delay before the shot.
        let Some(error) = target else {
            self.seen_at = None;
            return;
        };
        let near = settings.mode != "silent_proximity"
            || error[0].hypot(error[1]) <= settings.flick_proximity_px;
        if !near {
            self.seen_at = None;
            return;
        }
        let seen_at = *self.seen_at.get_or_insert(now);
        if elapsed(seen_at) < ms(settings.delay_ms) {
            return;
        }
        let (dx, dy) = match settings.mode.as_str() {
            "tap" => self.counts(error[0] * TAP_PULL, error[1] * TAP_PULL),
            "silent_proximity" | "silent_immediate" => self.flick(error, event),
            _ => self.pull(error, event),
        };
        self.push_move(dx, dy);
        self.press();
        self.phase = match settings.mode.as_str() {
            "tap" => Phase::Holding(now),
            "silent_proximity" | "silent_immediate" => Phase::Flicked {
                at: now,
                back: (-dx, -dy),
            },
            _ => Phase::Spraying(now),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str) -> MagnetSettings {
        MagnetSettings {
            enabled: true,
            mode: mode.into(),
            trigger_button: "m5".into(),
            delay_ms: 0,
            cooldown_ms: 90,
            sleep_ms: 0,
            fire_hold_ms: 30,
            forced_release_ms: 250,
            reset_timeout_ms: 250,
            flick_proximity_px: 26.0,
            smooth_x_min: 1.0,
            smooth_x_max: 1.0,
            smooth_y_min: 1.0,
            smooth_y_max: 1.0,
            speed_booster: false,
            speed_near: 1.0,
            speed_mid: 1.0,
            speed_far: 1.0,
        }
    }

    fn event_with(
        settings: MagnetSettings,
        frame_id: u64,
        error: Option<[f64; 2]>,
    ) -> MagnetTargetEvent {
        MagnetTargetEvent {
            config_revision: 0,
            frame_id,
            error,
            counts_per_px: 1.0,
            timestamp_ms: 1_000,
            settings,
        }
    }

    fn event(mode: &str, frame_id: u64, error: Option<[f64; 2]>) -> MagnetTargetEvent {
        event_with(settings(mode), frame_id, error)
    }

    fn drain(state: &mut MagnetState) -> Vec<MagnetOp> {
        std::iter::from_fn(|| state.next_op()).collect()
    }

    #[test]
    fn spray_presses_pulls_every_frame_and_releases_on_target_loss() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut spray = settings("spray");
        (spray.smooth_x_min, spray.smooth_x_max) = (4.0, 4.0);
        (spray.smooth_y_min, spray.smooth_y_max) = (5.0, 5.0);
        let sample = |frame, error| event_with(spray.clone(), frame, error);
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([40.0, 50.0]))), true, at(0), 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(10, 10), MagnetOp::Press]
        );
        assert!(state.holding());
        state.update(Some(&sample(1, Some([40.0, 50.0]))), true, at(4), 1_000);
        assert!(drain(&mut state).is_empty(), "same frame: no second pull");
        state.update(Some(&sample(2, Some([40.0, 50.0]))), true, at(8), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(10, 10)]);
        state.update(Some(&sample(3, None)), true, at(12), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        assert!(!state.holding());
        // Ready again at once: no cooldown after a target loss.
        state.update(Some(&sample(4, Some([40.0, 50.0]))), true, at(16), 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(10, 10), MagnetOp::Press]
        );
    }

    #[test]
    fn tap_pulls_45_percent_and_waits_the_reset_timeout_after_a_delay() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut tap = settings("tap");
        tap.delay_ms = 20;
        // Smoothing does not apply to a tap (source: error × 0.45).
        (tap.smooth_x_min, tap.smooth_x_max) = (4.0, 4.0);
        let sample = |frame, error| event_with(tap.clone(), frame, error);
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([20.0, -10.0]))), true, at(0), 1_000);
        state.update(Some(&sample(2, Some([20.0, -10.0]))), true, at(10), 1_000);
        assert!(drain(&mut state).is_empty(), "delay");
        state.update(Some(&sample(3, Some([20.0, -10.0]))), true, at(20), 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(9, -5), MagnetOp::Press]
        );
        state.update(Some(&sample(4, Some([0.0, 0.0]))), true, at(50), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        state.update(Some(&sample(5, Some([0.0, 0.0]))), true, at(290), 1_000);
        assert!(drain(&mut state).is_empty(), "reset timeout 250 ms");
        state.update(Some(&sample(6, Some([0.0, 0.0]))), true, at(300), 1_000);
        assert!(drain(&mut state).is_empty(), "delay starts again");
        state.update(Some(&sample(7, Some([0.0, 0.0]))), true, at(320), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Press]);
    }

    #[test]
    fn sleep_spaces_evaluations() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut spray = settings("spray");
        spray.sleep_ms = 10;
        let sample = |frame| event_with(spray.clone(), frame, Some([3.0, 0.0]));
        let mut state = MagnetState::default();
        state.update(Some(&sample(1)), true, at(0), 1_000);
        drain(&mut state);
        state.update(Some(&sample(2)), true, at(5), 1_000);
        assert!(drain(&mut state).is_empty(), "asleep");
        state.update(Some(&sample(3)), true, at(10), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(3, 0)]);
    }

    #[test]
    fn smoothing_is_drawn_per_pull_and_the_booster_scales_it() {
        let mut spray = settings("spray");
        (spray.smooth_x_min, spray.smooth_x_max) = (2.0, 4.0);
        let mut state = MagnetState::default();
        let sample = event_with(spray.clone(), 1, Some([100.0, 0.0]));
        let pulls: Vec<i64> = (0..40)
            .map(|_| {
                state.residual = (0.0, 0.0);
                state.pull([100.0, 0.0], &sample).0
            })
            .collect();
        assert!(pulls.iter().all(|x| (25..=50).contains(x)), "{pulls:?}");
        assert!(pulls.iter().any(|x| *x != pulls[0]));

        spray.speed_booster = true;
        (spray.speed_near, spray.speed_mid, spray.speed_far) = (0.5, 1.0, 2.0);
        assert_eq!(spray.boost(10.0), 0.5);
        assert_eq!(spray.boost(55.0), 0.75);
        assert_eq!(spray.boost(140.0), 1.5);
        assert_eq!(spray.boost(400.0), 2.0);
    }

    #[test]
    fn spray_is_force_released_then_cools_down() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut state = MagnetState::default();
        state.update(
            Some(&event("spray", 1, Some([0.0, 0.0]))),
            true,
            at(0),
            1_000,
        );
        drain(&mut state);
        state.update(
            Some(&event("spray", 2, Some([0.0, 0.0]))),
            true,
            at(250),
            1_000,
        );
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        state.update(
            Some(&event("spray", 3, Some([0.0, 0.0]))),
            true,
            at(300),
            1_000,
        );
        assert!(drain(&mut state).is_empty());
        state.update(
            Some(&event("spray", 4, Some([0.0, 0.0]))),
            true,
            at(340),
            1_000,
        );
        assert_eq!(drain(&mut state), vec![MagnetOp::Press]);
    }

    #[test]
    fn silent_proximity_flicks_the_whole_error_only_near_and_snaps_back() {
        let start = Instant::now();
        let mut silent = settings("silent_proximity");
        // Smoothing does not apply to a flick.
        (silent.smooth_x_min, silent.smooth_x_max) = (4.0, 4.0);
        // counts_per_px 0.06875 (sens 14.5) makes pixel_to_counts 1 count/px.
        let sample = |frame, error| MagnetTargetEvent {
            counts_per_px: DEGREES_PER_COUNT,
            ..event_with(silent.clone(), frame, error)
        };
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([60.0, 0.0]))), true, start, 1_000);
        assert!(drain(&mut state).is_empty(), "outside 26 px");
        state.update(Some(&sample(2, Some([20.0, -6.0]))), true, start, 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(20, -6), MagnetOp::Press]
        );
        let release = start + Duration::from_millis(30);
        state.update(Some(&sample(3, None)), true, release, 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Release, MagnetOp::Move(-20, 6)]
        );
        // Ready again at once (no cooldown in the source).
        state.update(Some(&sample(4, Some([10.0, 0.0]))), true, release, 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(10, 0), MagnetOp::Press]
        );
    }

    #[test]
    fn a_flick_converts_pixels_like_the_source_pixel_to_counts() {
        // Sensitivity 0.35: 1 px = 1 / (0.35 × 0.06875) ≈ 41.6 counts.
        let sample = MagnetTargetEvent {
            counts_per_px: 1.0 / 0.35,
            ..event("silent_immediate", 1, Some([2.0, 0.0]))
        };
        let mut state = MagnetState::default();
        assert_eq!(state.flick([2.0, 0.0], &sample), (83, 0));
    }

    #[test]
    fn silent_immediate_flicks_far_targets_in_device_sized_steps() {
        let mut state = MagnetState::default();
        let sample = MagnetTargetEvent {
            counts_per_px: DEGREES_PER_COUNT,
            ..event("silent_immediate", 1, Some([300.0, 0.0]))
        };
        state.update(Some(&sample), true, Instant::now(), 1_000);
        assert_eq!(
            drain(&mut state),
            vec![
                MagnetOp::Move(127, 0),
                MagnetOp::Move(127, 0),
                MagnetOp::Move(46, 0),
                MagnetOp::Press
            ]
        );
    }

    #[test]
    fn auto_mode_plans_nothing_here() {
        let mut state = MagnetState::default();
        let sample = event("auto", 1, Some([30.0, 0.0]));
        state.update(Some(&sample), true, Instant::now(), 1_000);
        assert!(drain(&mut state).is_empty());
        assert!(!state.holding());
    }

    #[test]
    fn releasing_the_trigger_or_a_stale_sample_releases_the_button() {
        let start = Instant::now();
        let mut state = MagnetState::default();
        state.update(
            Some(&event("spray", 1, Some([8.0, 0.0]))),
            true,
            start,
            1_000,
        );
        drain(&mut state);
        state.update(
            Some(&event("spray", 1, Some([8.0, 0.0]))),
            false,
            start,
            1_000,
        );
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        state.update(
            Some(&event("spray", 2, Some([8.0, 0.0]))),
            true,
            start,
            1_000,
        );
        drain(&mut state);
        state.update(
            Some(&event("spray", 2, Some([8.0, 0.0]))),
            true,
            start,
            1_200,
        );
        assert_eq!(drain(&mut state), vec![MagnetOp::Release], "stale sample");
    }
}
