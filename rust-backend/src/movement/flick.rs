//! Auto's aim mode: on each trigger press, take one target, then move to
//! exactly that position and report done so the host fires once.
//!
//! The target is not searched again while moving: the move is the locked
//! error converted to whole device counts, spent over several ticks with the
//! Normal speed (divided by the smoothing strength inside the Smooth FOV) and
//! at least one count per tick, so it always ends exactly on the target.

use std::time::{Duration, Instant};

/// After the last count is sent, wait this long before reporting done: the
/// batcher can hold the final delta for one 4 ms interval, and the click must
/// not overtake it.
pub const SETTLE: Duration = Duration::from_millis(12);

/// The target taken on the first fresh frame after the press.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlickTarget {
    /// Crop px from the screen center, offsets applied.
    pub error: (f64, f64),
    /// Smoothing strength per axis (≥ 1) drawn for this track.
    pub smooth: (f64, f64),
}

/// Values the move is spent with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlickParams {
    pub speed: (f64, f64),
    /// Smooth FOV radius in crop px.
    pub smooth_radius_px: f64,
    pub counts_per_px: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    #[default]
    Searching,
    Moving {
        remaining: (i64, i64),
        smooth: (f64, f64),
    },
    Settling {
        until: Instant,
    },
    Done,
}

#[derive(Debug, Default)]
pub struct Flick {
    state: State,
}

impl Flick {
    /// Trigger released, mode off or routing changed: wait for a new press.
    pub fn reset(&mut self) {
        self.state = State::Searching;
    }

    pub fn searching(&self) -> bool {
        self.state == State::Searching
    }

    /// The move reached the target and the last count had time to leave.
    pub fn done(&self) -> bool {
        self.state == State::Done
    }

    /// One tick. `found` is only used while searching.
    pub fn step(
        &mut self,
        found: Option<FlickTarget>,
        params: FlickParams,
        now: Instant,
    ) -> (i64, i64) {
        if let (State::Searching, Some(target)) = (self.state, found) {
            let count = |px: f64| {
                let counts = (px * params.counts_per_px).round();
                if counts.is_finite() {
                    counts as i64
                } else {
                    0
                }
            };
            self.state = State::Moving {
                remaining: (count(target.error.0), count(target.error.1)),
                smooth: target.smooth,
            };
        }
        match self.state {
            State::Searching | State::Done => (0, 0),
            State::Settling { until } => {
                if now >= until {
                    self.state = State::Done;
                }
                (0, 0)
            }
            State::Moving { remaining, smooth } => {
                let distance_px =
                    (remaining.0 as f64).hypot(remaining.1 as f64) / params.counts_per_px;
                let in_smooth_fov = distance_px < params.smooth_radius_px;
                let axis = |left: i64, speed: f64, smooth: f64| {
                    if left == 0 {
                        return 0;
                    }
                    let divisor = if in_smooth_fov { smooth.max(1.0) } else { 1.0 };
                    let step = (left as f64 * speed.max(0.0) / divisor).round() as i64;
                    // At least one count, never past the target.
                    step.abs().clamp(1, left.abs()) * left.signum()
                };
                let step = (
                    axis(remaining.0, params.speed.0, smooth.0),
                    axis(remaining.1, params.speed.1, smooth.1),
                );
                let left = (remaining.0 - step.0, remaining.1 - step.1);
                self.state = if left == (0, 0) {
                    State::Settling {
                        until: now + SETTLE,
                    }
                } else {
                    State::Moving {
                        remaining: left,
                        smooth,
                    }
                };
                step
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> FlickParams {
        FlickParams {
            speed: (0.2, 0.2),
            smooth_radius_px: 10.0,
            counts_per_px: 2.0,
        }
    }

    fn run(flick: &mut Flick, target: FlickTarget, params: FlickParams) -> (i64, i64, u32) {
        let start = Instant::now();
        let (mut x, mut y, mut ticks) = (0, 0, 0);
        let mut found = Some(target);
        while !flick.done() {
            let now = start + Duration::from_millis(4 * u64::from(ticks));
            let (dx, dy) = flick.step(found.take(), params, now);
            x += dx;
            y += dy;
            ticks += 1;
            assert!(ticks < 10_000, "never finished");
        }
        (x, y, ticks)
    }

    #[test]
    fn moves_exactly_to_the_locked_target_then_reports_done() {
        let mut flick = Flick::default();
        let target = FlickTarget {
            error: (100.0, -37.3),
            smooth: (1.0, 1.0),
        };
        let (x, y, _) = run(&mut flick, target, params());
        assert_eq!((x, y), (200, -75), "error × 2 counts/px, rounded once");
        assert_eq!(flick.step(None, params(), Instant::now()), (0, 0));
    }

    #[test]
    fn the_target_is_taken_once_and_later_frames_are_ignored() {
        let mut flick = Flick::default();
        let now = Instant::now();
        let first = FlickTarget {
            error: (10.0, 0.0),
            smooth: (1.0, 1.0),
        };
        assert!(flick.searching());
        flick.step(Some(first), params(), now);
        assert!(!flick.searching());
        let elsewhere = FlickTarget {
            error: (-50.0, 0.0),
            ..first
        };
        let mut total = 4; // 20 counts × 0.2 on the first tick
        for tick in 1..1_000 {
            if flick.done() {
                break;
            }
            let at = now + Duration::from_millis(4 * tick);
            total += flick.step(Some(elsewhere), params(), at).0;
        }
        assert!(flick.done());
        assert_eq!(total, 20);
        flick.reset();
        assert!(flick.searching(), "a new press searches again");
    }

    #[test]
    fn smoothing_slows_the_move_inside_the_smooth_fov_but_it_still_lands() {
        let target = FlickTarget {
            error: (40.0, 0.0),
            smooth: (5.0, 1.0),
        };
        let mut plain = Flick::default();
        let (_, _, fast) = run(
            &mut plain,
            FlickTarget {
                smooth: (1.0, 1.0),
                ..target
            },
            params(),
        );
        let mut smooth = Flick::default();
        let (x, _, slow) = run(&mut smooth, target, params());
        assert_eq!(x, 80);
        assert!(slow > fast, "{slow} vs {fast} ticks");
    }

    #[test]
    fn a_target_on_center_is_done_without_moving() {
        let mut flick = Flick::default();
        let (x, y, ticks) = run(
            &mut flick,
            FlickTarget {
                error: (0.2, -0.2),
                smooth: (1.0, 1.0),
            },
            params(),
        );
        assert_eq!((x, y), (0, 0));
        assert!(ticks <= 6, "only the settle time: {ticks}");
    }
}
