//! Isolated Auto feature state and timing.
//!
//! The module only decides when an Auto action is ready. Vendor-specific
//! button calls stay outside this state machine so Auto never becomes part of
//! movement or assumes a device API.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Controls and samples share this input, but a control event always has an
/// invalid target and timestamp zero. Configuration never refreshes a sample.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "snake_case")]
pub(crate) struct AutoTargetEvent {
    config_revision: u64,
    enabled: bool,
    pub(crate) trigger_button: String,
    delay_ms: u32,
    cooldown_ms: u32,
    sleep_ms: u32,
    target_valid: bool,
    timestamp_ms: u64,
}

impl AutoTargetEvent {
    pub(crate) fn action(
        &self,
        state: &mut AutoState,
        trigger_held: bool,
        now: Instant,
    ) -> Option<AutoAction> {
        state.set_config_revision(self.config_revision);
        if state.enabled() != self.enabled {
            state.set_enabled(self.enabled);
        }
        let timing = AutoTiming {
            delay_ms: self.delay_ms,
            cooldown_ms: self.cooldown_ms,
            sleep_ms: self.sleep_ms,
        };
        if state.timing() != timing {
            state.set_timing(timing);
        }
        let wall_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(u64::MAX);
        let fresh = wall_now.saturating_sub(self.timestamp_ms) <= 100
            && self.timestamp_ms <= wall_now.saturating_add(100);
        state.update(trigger_held, fresh && self.target_valid, now)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoState {
    enabled: bool,
    timing: AutoTiming,
    phase: AutoPhase,
    deadline: Option<Instant>,
    /// The last click's minimum interval survives target loss/rearming.
    cooldown_until: Option<Instant>,
    config_revision: u64,
    /// Next time the Auto state may be evaluated. Sleep is a polling cadence,
    /// not part of the firing cooldown, so it never changes `deadline`.
    next_poll: Option<Instant>,
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
            cooldown_until: None,
            config_revision: 0,
            next_poll: None,
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

    pub fn set_config_revision(&mut self, revision: u64) {
        if self.config_revision != revision {
            self.config_revision = revision;
            self.reset();
        }
    }

    /// Advances Auto using the latest trigger and target state.
    ///
    /// `delay_ms` starts when a valid center-crossing target is first observed while the
    /// trigger is held. Once started, the delay survives target loss; releasing
    /// the trigger or disabling Auto cancels it. Further crossings while a
    /// delay is pending do not restart it or schedule additional clicks.
    /// `cooldown_ms` starts when the click action is emitted
    /// and is the minimum interval between two clicks. `sleep_ms` only spaces
    /// state evaluations to keep an idle loop from running continuously; it
    /// does not extend the cooldown. With a zero cooldown, one action is
    /// emitted per target acquisition and the state stays latched until the
    /// target is lost or the trigger is released.
    pub fn update(
        &mut self,
        trigger_held: bool,
        target_valid: bool,
        now: Instant,
    ) -> Option<AutoAction> {
        if !self.enabled || !trigger_held {
            self.reset();
            return None;
        }

        // A center crossing arms one delayed click. The target need not stay
        // at the center until it fires; target loss only rearms other phases.
        if !target_valid && self.phase != AutoPhase::WaitingDelay {
            self.reset();
            return None;
        }

        if self.next_poll.is_some_and(|next_poll| now < next_poll) {
            return None;
        }

        let action = match self.phase {
            AutoPhase::Armed => {
                if self.timing.delay_ms == 0 {
                    self.cooldown_elapsed(now).then(|| self.fire(now))
                } else {
                    self.phase = AutoPhase::WaitingDelay;
                    self.deadline = Some(now + millis(self.timing.delay_ms));
                    None
                }
            }
            AutoPhase::WaitingDelay | AutoPhase::WaitingCooldown => {
                if self.deadline.is_some_and(|deadline| now >= deadline)
                    && self.cooldown_elapsed(now)
                {
                    Some(self.fire(now))
                } else {
                    None
                }
            }
            AutoPhase::Latched => None,
        };
        self.schedule_next_poll(now);
        action
    }

    fn fire(&mut self, now: Instant) -> AutoAction {
        self.cooldown_until = Some(now + millis(self.timing.cooldown_ms));
        if self.timing.cooldown_ms == 0 {
            self.phase = AutoPhase::Latched;
            self.deadline = None;
        } else {
            self.phase = AutoPhase::WaitingCooldown;
            // Cooldown starts at the moment the action is emitted.
            self.deadline = Some(now + millis(self.timing.cooldown_ms));
        }
        AutoAction::LeftClick
    }

    fn cooldown_elapsed(&self, now: Instant) -> bool {
        self.cooldown_until.is_none_or(|deadline| now >= deadline)
    }

    fn schedule_next_poll(&mut self, now: Instant) {
        self.next_poll = (self.timing.sleep_ms > 0).then(|| now + millis(self.timing.sleep_ms));
    }

    fn reset(&mut self) {
        self.phase = AutoPhase::Armed;
        self.deadline = None;
        self.next_poll = None;
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
    fn wire_control_changes_cancel_delay_without_refreshing_target_freshness() {
        use super::AutoTargetEvent;
        let timestamp = super::SystemTime::now()
            .duration_since(super::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        for change in [
            serde_json::json!({"enabled":false}),
            serde_json::json!({"delay_ms":30}),
            serde_json::json!({"trigger_button":"m4"}),
            serde_json::json!({"enabled":true}), // off/on between device polls
        ] {
            let mut sample = serde_json::json!({
                "event":"auto_target","enabled":true,"trigger_button":"right",
                "delay_ms":90,"config_revision":1,"target_valid":true,"timestamp_ms":timestamp
            });
            let input: AutoTargetEvent = serde_json::from_value(sample.clone()).unwrap();
            let start = Instant::now();
            let mut state = AutoState::new();
            assert_eq!(input.action(&mut state, true, start), None);
            for (key, value) in change.as_object().unwrap() {
                sample[key] = value.clone();
            }
            sample["event"] = "auto_config".into();
            sample["config_revision"] = 3.into();
            sample["target_valid"] = false.into();
            sample["timestamp_ms"] = 0.into();
            let control: AutoTargetEvent = serde_json::from_value(sample).unwrap();
            assert_eq!(
                control.action(&mut state, true, start + Duration::from_millis(90)),
                None
            );
            assert_eq!(
                control.action(&mut state, true, start + Duration::from_secs(1)),
                None
            );
        }
    }

    #[test]
    fn cooldown_survives_target_flicker_and_trigger_rearming() {
        for release_trigger in [false, true] {
            let start = Instant::now();
            let mut state = AutoState::new();
            state.set_enabled(true);
            state.set_timing(AutoTiming {
                delay_ms: 0,
                cooldown_ms: 1000,
                sleep_ms: 0,
            });
            assert_eq!(state.update(true, true, start), Some(AutoAction::LeftClick));
            state.update(!release_trigger, false, start + Duration::from_millis(1));
            for ms in [2, 500, 999] {
                assert_eq!(
                    state.update(true, true, start + Duration::from_millis(ms)),
                    None
                );
            }
            assert_eq!(
                state.update(true, true, start + Duration::from_millis(1000)),
                Some(AutoAction::LeftClick)
            );
        }
    }

    #[test]
    fn reacquisition_delay_does_not_shorten_the_previous_click_cooldown() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 10,
            cooldown_ms: 1000,
            sleep_ms: 0,
        });
        state.update(true, true, start);
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(10)),
            Some(AutoAction::LeftClick)
        );
        state.update(true, false, start + Duration::from_millis(11));
        state.update(true, true, start + Duration::from_millis(12));
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(22)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(1010)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn configuration_revision_cancels_pending_click_without_a_new_target() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 90,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        state.set_config_revision(1);
        state.update(true, true, start);
        // Covers trigger changes and a disable/re-enable between device polls.
        state.set_config_revision(3);
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(90)),
            None
        );
        state.update(true, true, start + Duration::from_millis(100));
        state.set_config_revision(3); // ordinary frames must not reset Delay
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(190)),
            Some(AutoAction::LeftClick)
        );
    }

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
    fn cooldown_starts_when_the_click_is_emitted() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 0,
            cooldown_ms: 50,
            sleep_ms: 0,
        });
        assert_eq!(state.update(true, true, start), Some(AutoAction::LeftClick));
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(49)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(50)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn sleep_only_delays_polling_and_does_not_extend_cooldown() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 0,
            cooldown_ms: 5,
            sleep_ms: 50,
        });
        assert_eq!(state.update(true, true, start), Some(AutoAction::LeftClick));
        // Cooldown has elapsed, but Sleep has not allowed another poll yet.
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(49)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(50)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn brief_center_crossing_fires_once_after_delay_even_without_target() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 90,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        assert_eq!(state.update(true, true, start), None);
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(1)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(89)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(90)),
            Some(AutoAction::LeftClick)
        );
        for ms in 91..=200 {
            assert_eq!(
                state.update(true, false, start + Duration::from_millis(ms)),
                None
            );
        }
    }

    #[test]
    fn further_crossings_preserve_the_first_deadline() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 90,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        for (ms, valid) in [(0, true), (10, false), (40, true), (89, true)] {
            assert_eq!(
                state.update(true, valid, start + Duration::from_millis(ms)),
                None
            );
        }
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(90)),
            Some(AutoAction::LeftClick)
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(200)),
            None
        );
        // Target loss after the click permits the next crossing to arm again.
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(201)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(202)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(292)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn trigger_release_cancels_a_pending_delay() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 90,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        assert_eq!(state.update(true, true, start), None);
        assert_eq!(
            state.update(false, false, start + Duration::from_millis(50)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(90)),
            None
        );
        assert_eq!(
            state.update(true, true, start + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(190)),
            Some(AutoAction::LeftClick)
        );
    }

    #[test]
    fn disabling_auto_cancels_a_pending_delay() {
        let start = Instant::now();
        let mut state = AutoState::new();
        state.set_enabled(true);
        state.set_timing(AutoTiming {
            delay_ms: 90,
            cooldown_ms: 0,
            sleep_ms: 0,
        });
        assert_eq!(state.update(true, true, start), None);
        state.set_enabled(false);
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(90)),
            None
        );
        state.set_enabled(true);
        assert_eq!(
            state.update(true, false, start + Duration::from_millis(100)),
            None
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
