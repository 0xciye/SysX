//! Isolated Auto feature state and timing.
//!
//! The module only decides when an Auto action is ready. Vendor-specific
//! button calls stay outside this state machine so Auto never becomes part of
//! movement or assumes a device API.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoState {
    enabled: bool,
    timing: AutoTiming,
    phase: AutoPhase,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AutoPhase {
    #[default]
    Armed,
    WaitingDelay,
    WaitingCooldown,
    Latched,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoAction {
    LeftClick,
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
            phase: AutoPhase::Armed,
            deadline: None,
        }
    }

    pub const fn enabled(self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.reset();
    }

    pub const fn timing(self) -> AutoTiming {
        self.timing
    }

    pub fn set_timing(&mut self, timing: AutoTiming) {
        self.timing = timing;
        self.reset();
    }

    /// Advances Auto using the latest trigger and target state.
    ///
    /// `delay_ms` applies when a target first enters the FOV. After an action,
    /// `cooldown_ms + sleep_ms` is the quiet period before another action while
    /// the target remains valid. With both set to zero, one action is emitted
    /// per target acquisition and the state stays latched until the target is
    /// lost or the trigger is released.
    pub fn update(
        &mut self,
        trigger_held: bool,
        target_valid: bool,
        now: Instant,
    ) -> Option<AutoAction> {
        if !self.enabled || !trigger_held || !target_valid {
            self.reset();
            return None;
        }

        match self.phase {
            AutoPhase::Armed => {
                if self.timing.delay_ms == 0 {
                    Some(self.fire(now))
                } else {
                    self.phase = AutoPhase::WaitingDelay;
                    self.deadline = Some(now + millis(self.timing.delay_ms));
                    None
                }
            }
            AutoPhase::WaitingDelay | AutoPhase::WaitingCooldown => {
                if self.deadline.is_some_and(|deadline| now >= deadline) {
                    Some(self.fire(now))
                } else {
                    None
                }
            }
            AutoPhase::Latched => None,
        }
    }

    fn fire(&mut self, now: Instant) -> AutoAction {
        let quiet_ms = self.timing.cooldown_ms.saturating_add(self.timing.sleep_ms);
        if quiet_ms == 0 {
            self.phase = AutoPhase::Latched;
            self.deadline = None;
        } else {
            self.phase = AutoPhase::WaitingCooldown;
            self.deadline = Some(now + millis(quiet_ms));
        }
        AutoAction::LeftClick
    }

    fn reset(&mut self) {
        self.phase = AutoPhase::Armed;
        self.deadline = None;
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

fn millis(value: u32) -> Duration {
    Duration::from_millis(u64::from(value))
}

#[cfg(test)]
mod tests {
    use super::{AutoAction, AutoState, AutoTiming};
    use std::time::{Duration, Instant};

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

    #[test]
    fn auto_waits_for_delay_then_emits_a_left_click() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 100,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        assert_eq!(state.update(true, true, start), None);
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(99)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(100)),
            Some(AutoAction::LeftClick)
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(101)),
            None
        );
    }

    #[test]
    fn cooldown_and_sleep_allow_repeated_actions_while_target_stays_valid() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 0,
            cooldown_ms: 50,
            sleep_ms: 10,
        });
        assert_eq!(state.update(true, true, start), Some(AutoAction::LeftClick));
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(59)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(60)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn losing_target_or_trigger_rearms_auto() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        assert_eq!(state.update(true, true, start), Some(AutoAction::LeftClick));
        assert_eq!(
            state.update(false, true, start + Duration::from_millis(1)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(2)),
            Some(AutoAction::LeftClick)
        );
    }
}
