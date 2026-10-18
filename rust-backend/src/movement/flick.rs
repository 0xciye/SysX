//! Auto's aim mode: on each trigger press, aim at one target and keep
//! re-measuring it until the crosshair is on it, then report done so the
//! host fires once, all as fast as frames allow.
//!
//! Each move is the whole remaining error in device counts, sent at once.
//! The next move waits for a frame that already shows the previous one (the
//! target jumped toward the center, or the measured capture latency passed),
//! so a move still in flight is never sent twice. That frame also tells how
//! many pixels a count really moved: the learned scale corrects the next
//! move and later presses, so a Sens that does not match the game only costs
//! one extra correction instead of a miss. Done comes from a frame showing
//! the target on the crosshair (the center gate) or within
//! [`ON_TARGET_PX`] of it, never from a frame taken before the last move.

/// The target pixel this close to the aim point counts as on target.
pub const ON_TARGET_PX: f64 = 1.5;
/// A move counts as shown once the target jumped this share of it.
const LANDED_SHARE: f64 = 0.4;
/// Moves shorter than this are not visible enough to detect; they land by time.
const DETECTABLE_PX: f64 = 3.0;
/// Assume a move has landed after this long even if no jump was seen.
const LAND_TIMEOUT_S: f64 = 0.12;
const DEFAULT_LATENCY_S: f64 = 0.04;
/// Learned px-per-expected-px, per axis.
const SCALE_LIMITS: (f64, f64) = (0.25, 4.0);
const RATIO_LIMITS: (f64, f64) = (0.5, 2.0);

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Searching,
    /// A move is out; wait for the first frame that shows it.
    Waiting {
        sent_s: f64,
        /// Error when the move was computed (px).
        before: (f64, f64),
        /// Shift the move should cause, with the current scale (px).
        expected: (f64, f64),
    },
    /// The move showed in one frame; the next frame settles it.
    Landed {
        before: (f64, f64),
        expected: (f64, f64),
    },
    Done,
}

/// What one new frame shows the aim loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlickFrame {
    pub frame_id: u64,
    /// Monotonic time the receiver had the frame (s).
    pub arrival_s: f64,
    /// Target error from the aim point (crop px, offsets applied).
    pub error: Option<(f64, f64)>,
    /// Auto's center gate sees the target on the crosshair.
    pub on_center: bool,
}

#[derive(Debug)]
pub struct Flick {
    state: State,
    last_frame: Option<u64>,
    scale: (f64, f64),
    latency_s: f64,
}

impl Default for Flick {
    fn default() -> Self {
        Self {
            state: State::Searching,
            last_frame: None,
            scale: (1.0, 1.0),
            latency_s: DEFAULT_LATENCY_S,
        }
    }
}

impl Flick {
    /// Trigger released, mode off or routing changed: wait for a new press.
    /// The learned scale and latency carry over to the next press.
    pub fn reset(&mut self) {
        self.state = State::Searching;
        self.last_frame = None;
    }

    /// Sensitivity changed: what was learned no longer applies.
    pub fn forget(&mut self) {
        *self = Self::default();
    }

    pub fn searching(&self) -> bool {
        self.state == State::Searching
    }

    /// The crosshair is on the target; fire.
    pub fn done(&self) -> bool {
        self.state == State::Done
    }

    /// Device counts to send for this frame. Only a new frame acts.
    pub fn step(&mut self, frame: FlickFrame, counts_per_px: f64, now_s: f64) -> (i64, i64) {
        if self.done() || self.last_frame == Some(frame.frame_id) {
            return (0, 0);
        }
        self.last_frame = Some(frame.frame_id);
        match self.state {
            State::Done => (0, 0),
            State::Searching => match frame.error {
                Some(error) => self.aim(error, frame.on_center, counts_per_px, now_s),
                None => (0, 0),
            },
            State::Waiting {
                sent_s,
                before,
                expected,
            } => {
                // Captured before the move left: it cannot show it.
                if frame.arrival_s <= sent_s {
                    return (0, 0);
                }
                let elapsed = frame.arrival_s - sent_s;
                let shown = frame.error.is_some_and(|error| {
                    let length = expected.0 * expected.0 + expected.1 * expected.1;
                    let shift = (before.0 - error.0, before.1 - error.1);
                    length.sqrt() >= DETECTABLE_PX
                        && (shift.0 * expected.0 + shift.1 * expected.1) / length >= LANDED_SHARE
                });
                if shown {
                    self.latency_s = self.latency_s * 0.7 + elapsed.min(LAND_TIMEOUT_S) * 0.3;
                }
                let by_time = if expected.0.hypot(expected.1) < DETECTABLE_PX {
                    elapsed >= self.latency_s
                } else {
                    elapsed >= LAND_TIMEOUT_S
                };
                if shown || by_time {
                    self.state = State::Landed { before, expected };
                }
                (0, 0)
            }
            State::Landed { before, expected } => {
                let Some(error) = frame.error else {
                    // Lost from view after the move: look for it again.
                    self.state = State::Searching;
                    return (0, 0);
                };
                self.learn(before, expected, error);
                self.aim(error, frame.on_center, counts_per_px, now_s)
            }
        }
    }

    fn aim(
        &mut self,
        error: (f64, f64),
        on_center: bool,
        counts_per_px: f64,
        now_s: f64,
    ) -> (i64, i64) {
        if on_center || error.0.hypot(error.1) <= ON_TARGET_PX {
            self.state = State::Done;
            return (0, 0);
        }
        let count = |px: f64, scale: f64| {
            let counts = (px * counts_per_px / scale).round();
            if counts.is_finite() {
                counts as i64
            } else {
                0
            }
        };
        let counts = (count(error.0, self.scale.0), count(error.1, self.scale.1));
        if counts == (0, 0) {
            // Closer than one count: nothing finer can be sent.
            self.state = State::Done;
            return counts;
        }
        let expected = (
            counts.0 as f64 / counts_per_px * self.scale.0,
            counts.1 as f64 / counts_per_px * self.scale.1,
        );
        self.state = State::Waiting {
            sent_s: now_s,
            before: error,
            expected,
        };
        counts
    }

    /// How far the target really moved per expected pixel, per axis. A
    /// moving target adds noise, so each update is bounded and damped.
    fn learn(&mut self, before: (f64, f64), expected: (f64, f64), after: (f64, f64)) {
        let update = |scale: &mut f64, before: f64, expected: f64, after: f64| {
            if expected.abs() < DETECTABLE_PX {
                return;
            }
            let ratio = ((before - after) / expected).clamp(RATIO_LIMITS.0, RATIO_LIMITS.1);
            *scale = (*scale * ratio.powf(0.7)).clamp(SCALE_LIMITS.0, SCALE_LIMITS.1);
        };
        update(&mut self.scale.0, before.0, expected.0, after.0);
        update(&mut self.scale.1, before.1, expected.1, after.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(frame_id: u64, arrival_s: f64, error: Option<(f64, f64)>) -> FlickFrame {
        FlickFrame {
            frame_id,
            arrival_s,
            error,
            on_center: false,
        }
    }

    #[test]
    fn a_correct_scale_moves_once_then_fires_on_the_frame_that_shows_it() {
        let mut flick = Flick::default();
        assert_eq!(
            flick.step(frame(1, 0.0, None), 2.0, 0.0),
            (0, 0),
            "no target"
        );
        assert!(flick.searching());
        assert_eq!(
            flick.step(frame(2, 0.004, Some((100.0, -37.3))), 2.0, 0.005),
            (200, -75)
        );
        assert!(!flick.searching() && !flick.done());
        // Frames from before the move and before it shows: no second move.
        assert_eq!(
            flick.step(frame(3, 0.004, Some((100.0, -37.3))), 2.0, 0.006),
            (0, 0)
        );
        assert_eq!(
            flick.step(frame(4, 0.02, Some((100.0, -37.3))), 2.0, 0.02),
            (0, 0)
        );
        assert_eq!(
            flick.step(frame(5, 0.03, Some((0.0, 0.0))), 2.0, 0.03),
            (0, 0)
        );
        assert!(!flick.done(), "one more frame settles the move");
        assert_eq!(
            flick.step(frame(6, 0.034, Some((0.5, 0.0))), 2.0, 0.034),
            (0, 0)
        );
        assert!(flick.done());
        assert_eq!(
            flick.step(frame(7, 0.04, Some((9.0, 9.0))), 2.0, 0.04),
            (0, 0),
            "fired"
        );
        flick.reset();
        assert!(flick.searching(), "a new press aims again");
    }

    /// Closed loop: frames every 4 ms, a move shows 20 ms after it is sent,
    /// the game turns `true_scale` px per expected px, the target drifts at
    /// `drift` px/s. Returns (time to fire, error when it fired) in s/px.
    fn simulate(flick: &mut Flick, true_scale: f64, start: (f64, f64), drift: f64) -> (f64, f64) {
        let mut moves: Vec<(f64, (i64, i64))> = Vec::new();
        for n in 1..200u64 {
            let now = n as f64 * 0.004;
            let shown = moves.iter().filter(|(sent, _)| now - sent >= 0.02).fold(
                (0.0, 0.0),
                |sum, (_, counts)| {
                    (
                        sum.0 + counts.0 as f64 * true_scale,
                        sum.1 + counts.1 as f64 * true_scale,
                    )
                },
            );
            let error = (start.0 + drift * now - shown.0, start.1 - shown.1);
            let counts = flick.step(frame(n, now, Some(error)), 1.0, now);
            if counts != (0, 0) {
                moves.push((now, counts));
            }
            if flick.done() {
                return (now, error.0.hypot(error.1));
            }
        }
        panic!("never fired");
    }

    #[test]
    fn a_wrong_scale_is_corrected_on_the_same_press_and_learned_for_the_next() {
        // Sens does not match the game: it really turns half as far.
        let mut flick = Flick::default();
        let (first, error) = simulate(&mut flick, 0.5, (80.0, -30.0), 0.0);
        assert!(error <= ON_TARGET_PX, "fired on target: {error}");
        assert!(
            flick.scale.0 < 0.8 && flick.scale.1 < 0.8,
            "{:?}",
            flick.scale
        );
        // The next press needs fewer corrections.
        flick.reset();
        let (second, error) = simulate(&mut flick, 0.5, (-60.0, 20.0), 0.0);
        assert!(error <= ON_TARGET_PX);
        assert!(second < first, "learned: {second} s vs {first} s");
        // Too far instead of too short is corrected as well.
        let mut flick = Flick::default();
        let (_, error) = simulate(&mut flick, 1.8, (70.0, 10.0), 0.0);
        assert!(error <= ON_TARGET_PX);
    }

    #[test]
    fn a_matching_scale_fires_one_latency_after_the_move() {
        let mut flick = Flick::default();
        let (time, error) = simulate(&mut flick, 1.0, (90.0, 25.0), 0.0);
        assert!(error <= ON_TARGET_PX);
        // Move at 4 ms, shows at 24 ms, settles on the next frame.
        assert!(time <= 0.032, "{time} s");
    }

    #[test]
    fn a_moving_target_is_followed_until_the_crosshair_is_on_it() {
        let mut flick = Flick::default();
        let (time, error) = simulate(&mut flick, 1.0, (60.0, 0.0), 40.0);
        assert!(error <= ON_TARGET_PX, "{error}");
        assert!(time < 0.3, "{time} s");
    }

    #[test]
    fn a_target_already_on_the_crosshair_fires_without_moving() {
        let mut flick = Flick::default();
        let mut on = frame(1, 0.0, Some((6.0, 4.0)));
        on.on_center = true;
        assert_eq!(flick.step(on, 2.0, 0.0), (0, 0));
        assert!(flick.done());
    }

    #[test]
    fn a_short_move_lands_by_the_measured_latency() {
        let mut flick = Flick::default();
        assert_eq!(
            flick.step(frame(1, 0.0, Some((2.0, 0.0))), 1.0, 0.0),
            (2, 0)
        );
        flick.step(frame(2, 0.01, Some((2.0, 0.0))), 1.0, 0.01);
        assert!(!flick.done());
        flick.step(
            frame(3, DEFAULT_LATENCY_S + 0.001, Some((0.0, 0.0))),
            1.0,
            0.05,
        );
        flick.step(
            frame(4, DEFAULT_LATENCY_S + 0.005, Some((0.0, 0.0))),
            1.0,
            0.05,
        );
        assert!(flick.done());
    }
}
