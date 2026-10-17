//! Auto's aim mode: on each trigger press, take one target, move onto it in
//! one go and report done so the host fires once, all as fast as possible.
//!
//! The target is not searched again: the locked error becomes whole device
//! counts sent as a single move. The service prints that move immediately
//! (not through the 4 ms batcher) before done is visible, and the host sends
//! it in one command and clicks as soon as it has left.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Searching,
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

    /// The move for this press has been produced.
    pub fn done(&self) -> bool {
        self.state == State::Done
    }

    /// `found` (crop px from the center, offsets applied) is only used while
    /// searching; the whole move comes out on that call.
    pub fn step(&mut self, found: Option<(f64, f64)>, counts_per_px: f64) -> (i64, i64) {
        let (State::Searching, Some(error)) = (self.state, found) else {
            return (0, 0);
        };
        self.state = State::Done;
        let count = |px: f64| {
            let counts = (px * counts_per_px).round();
            if counts.is_finite() {
                counts as i64
            } else {
                0
            }
        };
        (count(error.0), count(error.1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_move_goes_out_at_once_and_only_once_per_press() {
        let mut flick = Flick::default();
        assert_eq!(flick.step(None, 2.0), (0, 0), "no target yet");
        assert!(flick.searching());
        assert_eq!(flick.step(Some((100.0, -37.3)), 2.0), (200, -75));
        assert!(flick.done());
        assert_eq!(flick.step(Some((5.0, 5.0)), 2.0), (0, 0), "taken once");
        flick.reset();
        assert!(flick.searching(), "a new press searches again");
        assert_eq!(flick.step(Some((0.2, -0.2)), 2.0), (0, 0), "on center");
        assert!(flick.done());
    }
}
