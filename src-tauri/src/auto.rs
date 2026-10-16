//! Isolated Auto feature state.
//!
//! Auto actions will be implemented here.  Keeping the state in its own
//! module prevents future automatic actions from becoming part of movement.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoState {
    enabled: bool,
    timing: AutoTiming,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoTiming {
    pub delay_ms: u32,
    pub cooldown_ms: u32,
    pub sleep_ms: u32,
}

impl AutoState {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            timing: AutoTiming::new(),
        }
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub const fn timing(self) -> AutoTiming {
        self.timing
    }

    pub fn set_timing(&mut self, timing: AutoTiming) {
        self.timing = timing;
    }
}

impl AutoTiming {
    pub const fn new() -> Self {
        Self {
            delay_ms: 0,
            cooldown_ms: 0,
            sleep_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoState, AutoTiming};

    #[test]
    fn auto_state_starts_disabled_and_can_be_toggled() {
        let mut state = AutoState::new();
        assert!(!state.enabled());
        state.set_enabled(true);
        assert!(state.enabled());
        state.set_timing(AutoTiming {
            delay_ms: 100,
            cooldown_ms: 250,
            sleep_ms: 10,
        });
        assert_eq!(state.timing().cooldown_ms, 250);
    }
}
