//! Magnet (user request; modes from the user's `magnet.rs`, reshaped on
//! 2026-11-01): each press of its trigger is one action, started on the
//! first frame after the press that has a target in Magnet's FOV (the frame
//! on screen at the press is skipped; with no target it waits while the
//! trigger is held):
//!
//! - `tap`: pulls error × 0.45 (the source's tap pull, px sent as device
//!   counts) and fires one shot.
//! - `spray`: pulls on every new frame by error / smoothing (drawn in
//!   [min, max] per axis, px sent as device counts like the source) × the
//!   distance speed booster, and fires only once the crosshair is inside
//!   the target's box (user request: "spray fires only on a valid target"),
//!   holding the button for a burst while it keeps pulling; released after
//!   `forced_release_ms` ("Max burst") or on target loss.
//! - `silent_immediate` ("Flick"): the only snap (user request): the whole
//!   error at once through the source's `pixel_to_counts`
//!   (px / (sens × 0.022 × 3.125)), one shot, then moves back by the snap.
//! - `auto`: nothing here; it only clicks when a valid target passes the
//!   center (`auto.rs`), with no snap.
//!
//! Holding the trigger does not repeat the action; release and press again.
//! A shot holds the button 25–40 ms (drawn per shot): a press and release
//! sent back to back can land in one mouse report and never reach the game.
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
/// Degrees per count at sensitivity 1 (source `pixel_to_counts`: 0.022 × 3.125).
const DEGREES_PER_COUNT: f64 = 0.022 * 3.125;
/// Tap pull: this fraction of the error (source `tick_tap`).
const TAP_PULL: f64 = 0.45;
/// A shot holds the button this long, drawn per shot.
const CLICK_MS: (f64, f64) = (25.0, 40.0);

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MagnetSettings {
    pub enabled: bool,
    pub mode: String,
    pub trigger_button: String,
    /// Spray: the longest burst.
    pub forced_release_ms: u64,
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
    /// The crosshair is inside the target's box.
    #[serde(default)]
    pub on_target: bool,
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
    /// Pressed: waiting for the first frame after the press with a target.
    Armed,
    /// One shot: released after `hold`, then moved by `back` (Flick).
    Shooting {
        at: Instant,
        hold: Duration,
        back: (i64, i64),
    },
    /// Spray pulling toward the target, not firing yet.
    Pulling,
    /// Spray burst started at this time.
    Spraying(Instant),
    /// This press is spent until the trigger is released.
    Done,
}

/// XorShift64 for the per-pull smoothing and per-shot hold draws (the
/// source's `FastRng`).
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
    /// The frame on screen when the trigger went down; `None` while it is up.
    press_frame: Option<u64>,
    last_frame: Option<u64>,
    residual: (f64, f64),
    /// The left button is pressed by Magnet (until its Release is sent).
    holding: bool,
    ops: VecDeque<MagnetOp>,
    rng: FastRng,
}

impl Default for MagnetState {
    fn default() -> Self {
        Self {
            phase: Phase::Armed,
            press_frame: None,
            last_frame: None,
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

    /// The snap: the whole error at once through the source's
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

    fn shot_hold(&mut self) -> Duration {
        Duration::from_millis(self.rng.range(CLICK_MS.0, CLICK_MS.1).round() as u64)
    }

    /// Ends this press's action: release, nothing more until the trigger
    /// is released.
    fn finish(&mut self) {
        self.release();
        self.residual = (0.0, 0.0);
        self.phase = Phase::Done;
    }

    /// Trigger released or Magnet off: drop planned moves, release the
    /// button and arm for the next press (no move back).
    pub fn cancel(&mut self) {
        self.ops.retain(|op| *op == MagnetOp::Release);
        self.release();
        self.phase = Phase::Armed;
        self.press_frame = None;
        self.last_frame = None;
        self.residual = (0.0, 0.0);
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
        // stale one is the same as Magnet off.
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
        let press_frame = *self.press_frame.get_or_insert(event.frame_id);
        let new_frame = self.last_frame != Some(event.frame_id);
        self.last_frame = Some(event.frame_id);
        let elapsed = now.saturating_duration_since(match self.phase {
            Phase::Shooting { at, .. } | Phase::Spraying(at) => at,
            Phase::Armed | Phase::Pulling | Phase::Done => now,
        });
        match self.phase {
            Phase::Armed => {
                if event.frame_id == press_frame {
                    return;
                }
                let Some(error) = event.error else {
                    return;
                };
                let mode = event.settings.mode.as_str();
                let (dx, dy) = match mode {
                    "silent_immediate" => self.flick(error, event),
                    "tap" => self.counts(error[0] * TAP_PULL, error[1] * TAP_PULL),
                    _ => self.pull(error, event),
                };
                self.push_move(dx, dy);
                if mode == "spray" {
                    self.phase = Phase::Pulling;
                    if event.on_target {
                        self.press();
                        self.phase = Phase::Spraying(now);
                    }
                    return;
                }
                self.press();
                // Tap and Flick: one shot (Flick then moves back).
                self.residual = (0.0, 0.0);
                self.phase = Phase::Shooting {
                    at: now,
                    hold: self.shot_hold(),
                    back: if mode == "silent_immediate" {
                        (-dx, -dy)
                    } else {
                        (0, 0)
                    },
                };
            }
            Phase::Shooting { hold, back, .. } => {
                if elapsed >= hold {
                    self.finish();
                    self.push_move(back.0, back.1);
                }
            }
            Phase::Pulling => match event.error {
                Some(error) if new_frame => {
                    let (dx, dy) = self.pull(error, event);
                    self.push_move(dx, dy);
                    if event.on_target {
                        self.press();
                        self.phase = Phase::Spraying(now);
                    }
                }
                Some(_) => {}
                None => self.finish(),
            },
            Phase::Spraying(_) => {
                if elapsed >= Duration::from_millis(event.settings.forced_release_ms) {
                    self.finish();
                    return;
                }
                match event.error {
                    Some(error) if new_frame => {
                        let (dx, dy) = self.pull(error, event);
                        self.push_move(dx, dy);
                    }
                    Some(_) => {}
                    None => self.finish(),
                }
            }
            Phase::Done => {}
        }
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
            forced_release_ms: 250,
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

    /// counts_per_px 0.06875 (sens 14.5) makes the snap 1 count per px.
    fn event_with(
        settings: MagnetSettings,
        frame_id: u64,
        error: Option<[f64; 2]>,
    ) -> MagnetTargetEvent {
        MagnetTargetEvent {
            config_revision: 0,
            frame_id,
            error,
            on_target: false,
            counts_per_px: DEGREES_PER_COUNT,
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

    /// Presses the trigger on frame 1 and snaps on frame 2.
    fn snapped(mode: &str, error: [f64; 2], start: Instant) -> MagnetState {
        let mut state = MagnetState::default();
        state.update(Some(&event(mode, 1, Some(error))), true, start, 1_000);
        assert!(drain(&mut state).is_empty(), "the frame at the press is skipped");
        state.update(Some(&event(mode, 2, Some(error))), true, start, 1_000);
        state
    }

    #[test]
    fn tap_pulls_45_percent_and_fires_one_shot_per_press() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut state = snapped("tap", [40.0, -12.0], start);
        // No snap: error × 0.45 (px as counts, like the source).
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(18, -5), MagnetOp::Press]
        );
        state.update(Some(&event("tap", 3, Some([0.0, 0.0]))), true, at(20), 1_000);
        assert!(drain(&mut state).is_empty(), "a shot holds 25–40 ms");
        state.update(Some(&event("tap", 4, Some([0.0, 0.0]))), true, at(40), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        // Held on: no second shot.
        for frame in 5..20 {
            state.update(Some(&event("tap", frame, Some([9.0, 0.0]))), true, at(40 + frame), 1_000);
        }
        assert!(drain(&mut state).is_empty());
        // Released and pressed again: another shot.
        state.update(Some(&event("tap", 20, None)), false, at(80), 1_000);
        state.update(Some(&event("tap", 20, Some([9.0, 0.0]))), true, at(84), 1_000);
        state.update(Some(&event("tap", 21, Some([20.0, 0.0]))), true, at(88), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(9, 0), MagnetOp::Press]);
    }

    #[test]
    fn spray_pulls_and_fires_only_on_the_target_then_bursts_until_max_burst() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut spray = settings("spray");
        (spray.smooth_x_min, spray.smooth_x_max) = (4.0, 4.0);
        (spray.smooth_y_min, spray.smooth_y_max) = (5.0, 5.0);
        let sample = |frame, error, on_target| MagnetTargetEvent {
            on_target,
            ..event_with(spray.clone(), frame, error)
        };
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([40.0, 50.0]), false)), true, at(0), 1_000);
        state.update(Some(&sample(2, Some([40.0, 50.0]), false)), true, at(4), 1_000);
        // Off the target: pull only (40 / 4, 50 / 5).
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(10, 10)]);
        assert!(!state.holding());
        state.update(Some(&sample(2, Some([40.0, 50.0]), false)), true, at(8), 1_000);
        assert!(drain(&mut state).is_empty(), "same frame: no second pull");
        // On the target: pull and fire.
        state.update(Some(&sample(3, Some([8.0, 10.0]), true)), true, at(12), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(2, 2), MagnetOp::Press]);
        // The burst keeps pulling, then ends after Max burst (250 ms).
        state.update(Some(&sample(4, Some([4.0, 5.0]), false)), true, at(16), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(1, 1)]);
        assert!(state.holding());
        state.update(Some(&sample(5, Some([4.0, 5.0]), true)), true, at(262), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release], "Max burst");
        state.update(Some(&sample(6, Some([4.0, 5.0]), true)), true, at(300), 1_000);
        assert!(drain(&mut state).is_empty(), "one burst per press");

        // Target lost before or during the burst: no shot / release.
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([40.0, 0.0]), false)), true, at(0), 1_000);
        state.update(Some(&sample(2, Some([40.0, 0.0]), false)), true, at(4), 1_000);
        state.update(Some(&sample(3, None, false)), true, at(8), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(10, 0)], "never fired");
        let mut state = MagnetState::default();
        state.update(Some(&sample(1, Some([4.0, 0.0]), true)), true, at(0), 1_000);
        state.update(Some(&sample(2, Some([4.0, 0.0]), true)), true, at(4), 1_000);
        drain(&mut state);
        state.update(Some(&sample(3, None, false)), true, at(8), 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release], "target lost");
    }

    #[test]
    fn flick_snaps_fires_and_moves_back() {
        let start = Instant::now();
        let mut state = snapped("silent_immediate", [20.0, -6.0], start);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Move(20, -6), MagnetOp::Press]
        );
        let release = start + Duration::from_millis(40);
        state.update(Some(&event("silent_immediate", 3, None)), true, release, 1_000);
        assert_eq!(
            drain(&mut state),
            vec![MagnetOp::Release, MagnetOp::Move(-20, 6)]
        );
        state.update(
            Some(&event("silent_immediate", 4, Some([5.0, 0.0]))),
            true,
            release,
            1_000,
        );
        assert!(drain(&mut state).is_empty(), "one flick per press");
    }

    #[test]
    fn a_far_snap_is_split_into_device_sized_steps() {
        let mut state = snapped("silent_immediate", [300.0, 0.0], Instant::now());
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
    fn without_a_target_the_action_waits_for_one() {
        let start = Instant::now();
        let mode = "silent_immediate";
        let mut state = MagnetState::default();
        state.update(Some(&event(mode, 1, None)), true, start, 1_000);
        state.update(Some(&event(mode, 2, None)), true, start, 1_000);
        assert!(drain(&mut state).is_empty());
        state.update(Some(&event(mode, 3, Some([6.0, 0.0]))), true, start, 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Move(6, 0), MagnetOp::Press]);
    }

    #[test]
    fn auto_mode_plans_nothing_here() {
        let start = Instant::now();
        let mut state = MagnetState::default();
        for frame in 1..5 {
            state.update(Some(&event("auto", frame, Some([30.0, 0.0]))), true, start, 1_000);
        }
        assert!(drain(&mut state).is_empty(), "Auto clicks only on center crossings");
        assert!(!state.holding());
    }

    #[test]
    fn a_snap_converts_pixels_like_the_source_pixel_to_counts() {
        // Sensitivity 0.35: 1 px = 1 / (0.35 × 0.06875) ≈ 41.6 counts.
        let sample = MagnetTargetEvent {
            counts_per_px: 1.0 / 0.35,
            ..event("tap", 1, Some([2.0, 0.0]))
        };
        let mut state = MagnetState::default();
        assert_eq!(state.flick([2.0, 0.0], &sample), (83, 0));
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
    fn releasing_the_trigger_or_a_stale_sample_releases_the_button() {
        let start = Instant::now();
        let mut state = snapped("tap", [8.0, 0.0], start);
        assert!(state.holding());
        drain(&mut state);
        state.update(Some(&event("tap", 2, Some([8.0, 0.0]))), false, start, 1_000);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release]);
        let mut state = snapped("tap", [8.0, 0.0], start);
        drain(&mut state);
        state.update(Some(&event("tap", 3, Some([8.0, 0.0]))), true, start, 1_200);
        assert_eq!(drain(&mut state), vec![MagnetOp::Release], "stale sample");
    }
}
