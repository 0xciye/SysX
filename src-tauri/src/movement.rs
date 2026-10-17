use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_DEVICE_DELTA: i64 = 32;
/// Auto's aim move is sent in one command (the SDK takes any i16 delta).
const MAX_FLICK_DELTA: i64 = 4096;
const TARGET_FRESHNESS_MS: u64 = 100;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecoilSettings {
    pub recoil_enabled: bool,
    pub recoil_levels: Vec<i32>,
    pub recoil_start_delay_ms: u32,
    pub recoil_level_duration_ms: u32,
    pub recoil_percentage: u8,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MovementTarget {
    #[serde(default, alias = "configRevision")]
    pub config_revision: u64,
    #[serde(alias = "targetValid")]
    pub target_valid: bool,
    pub dx: i64,
    pub dy: i64,
    #[serde(alias = "motionEnabled")]
    pub motion_enabled: bool,
    #[serde(alias = "motionAlwaysOn")]
    pub motion_always_on: bool,
    #[serde(alias = "triggerButton")]
    pub trigger_button: String,
    #[serde(alias = "timestampMs")]
    pub timestamp_ms: u64,
    #[serde(alias = "nativeMotion", alias = "native_motion")]
    pub recoil: RecoilSettings,
    /// Auto's aim move: one command, kept apart from Motion's pending deltas.
    #[serde(default)]
    pub flick: bool,
}

#[derive(Default)]
pub struct MovementState {
    pending: Mutex<Option<MovementTarget>>,
    /// Auto's aim move, so no later or older Motion event can merge it away.
    flick: Mutex<Option<MovementTarget>>,
    latest_target: Mutex<Option<MovementTarget>>,
    recoil: Mutex<RecoilSettings>,
}

pub fn set_target(state: &MovementState, value: &serde_json::Value) {
    let Ok(target) = serde_json::from_value::<MovementTarget>(value.clone()) else {
        return;
    };
    set_recoil_settings(state, target.recoil.clone());
    if let Ok(mut latest) = state.latest_target.lock() {
        *latest = Some(target.clone());
    }
    if target.flick && (target.dx != 0 || target.dy != 0) {
        if let Ok(mut flick) = state.flick.lock() {
            *flick = Some(target);
        }
        return;
    }
    if let Ok(mut pending) = state.pending.lock() {
        *pending = Some(MovementTarget::merge_pending(pending.take(), target));
    }
}

/// A fresh target delta is still waiting to be sent: Auto's aim mode clicks
/// only after its move has fully left.
pub fn has_pending_move(state: &MovementState) -> bool {
    let waiting = |slot: &Mutex<Option<MovementTarget>>| {
        slot.lock().is_ok_and(|pending| {
            pending.as_ref().is_some_and(|target| {
                target.target_valid && (target.dx != 0 || target.dy != 0) && target.is_fresh()
            })
        })
    };
    waiting(&state.flick) || waiting(&state.pending)
}

pub fn take_target(state: &MovementState) -> Option<MovementTarget> {
    // Auto's aim move goes first, whole.
    if let Some(flick) = state.flick.lock().ok().and_then(|mut slot| slot.take()) {
        if flick.is_fresh() {
            return Some(flick);
        }
    }
    let target = state.pending.lock().ok()?.take()?;
    target.is_fresh().then_some(target)
}

pub fn has_fresh_target(state: &MovementState) -> bool {
    state
        .latest_target
        .lock()
        .ok()
        .and_then(|target| target.clone())
        .is_some_and(|target| target.target_valid && target.is_fresh())
}

pub fn recoil_settings(state: &MovementState) -> RecoilSettings {
    state
        .recoil
        .lock()
        .map(|settings| settings.clone())
        .unwrap_or_default()
}

pub fn set_recoil_settings(state: &MovementState, settings: RecoilSettings) {
    if let Ok(mut recoil) = state.recoil.lock() {
        *recoil = settings;
    }
}

/// Put back the part of a target delta that one command could not carry, so
/// it is applied with the next command instead of being lost: the backend
/// counts every delta it sends as applied. It still expires after 100 ms.
pub fn restore_remainder(state: &MovementState, remainder: MovementTarget) {
    if remainder.dx == 0 && remainder.dy == 0 {
        return;
    }
    if remainder.flick {
        if let Ok(mut flick) = state.flick.lock() {
            flick.get_or_insert(remainder);
        }
        return;
    }
    if let Ok(mut pending) = state.pending.lock() {
        *pending = Some(match pending.take() {
            Some(newer) => MovementTarget::merge_pending(Some(remainder), newer),
            None => remainder,
        });
    }
}

pub fn trigger_button_index(button: &str) -> Option<u16> {
    match button {
        "right" => Some(1),
        "m4" => Some(3),
        "m5" => Some(4),
        _ => None,
    }
}

pub fn should_apply_target(target: &MovementTarget, trigger_held: bool) -> bool {
    target.motion_enabled && (target.motion_always_on || trigger_held) && target.target_valid
}

pub fn recoil_move(recoil_y: i64) -> (i16, i16) {
    (
        0,
        recoil_y.clamp(-MAX_DEVICE_DELTA, MAX_DEVICE_DELTA) as i16,
    )
}

impl MovementTarget {
    fn merge_pending(previous: Option<Self>, mut current: Self) -> Self {
        if current.target_valid {
            if let Some(previous) = previous.filter(|previous| {
                previous.target_valid
                    && previous.config_revision == current.config_revision
                    && previous.is_fresh_at(current.timestamp_ms)
            }) {
                current.dx = previous.dx.saturating_add(current.dx);
                current.dy = previous.dy.saturating_add(current.dy);
            }
        } else {
            current.dx = 0;
            current.dy = 0;
        }
        current
    }

    fn is_fresh(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(u64::MAX);
        self.is_fresh_at(now_ms)
    }

    fn is_fresh_at(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.timestamp_ms) <= TARGET_FRESHNESS_MS
            && self.timestamp_ms <= now_ms.saturating_add(TARGET_FRESHNESS_MS)
    }

    /// The clamped command and the target delta it leaves for the next one.
    /// Clipped recoil is not carried over: it is sampled fresh every command.
    pub fn device_move(&self, recoil_y: i64) -> ((i16, i16), Self) {
        let limit = if self.flick {
            MAX_FLICK_DELTA
        } else {
            MAX_DEVICE_DELTA
        };
        let x = self.dx.clamp(-limit, limit);
        let wanted_y = self.dy.saturating_add(recoil_y);
        let y = wanted_y.clamp(-limit, limit);
        // The clipped part of Y belongs to the target only as far as the
        // target's own delta points the same way.
        let clipped_y = wanted_y - y;
        let rest_y = if clipped_y.signum() == self.dy.signum() {
            clipped_y.signum() * clipped_y.abs().min(self.dy.abs())
        } else {
            0
        };
        let remainder = Self {
            dx: self.dx - x,
            dy: rest_y,
            ..self.clone()
        };
        ((x as i16, y as i16), remainder)
    }
}

#[derive(Default)]
pub struct VerticalAdjustmentClock {
    started_at: Option<Instant>,
}

impl VerticalAdjustmentClock {
    pub fn reset(&mut self) {
        self.started_at = None;
    }

    pub fn sample(
        &mut self,
        left_button_held: bool,
        settings: &RecoilSettings,
        now: Instant,
    ) -> i64 {
        if !left_button_held || !settings.recoil_enabled || settings.recoil_levels.is_empty() {
            self.reset();
            return 0;
        }

        let started_at = *self.started_at.get_or_insert(now);
        let elapsed_ms = now.saturating_duration_since(started_at).as_millis();
        let delay_ms = u128::from(settings.recoil_start_delay_ms);
        if elapsed_ms < delay_ms {
            return 0;
        }

        let level_duration_ms = u128::from(settings.recoil_level_duration_ms.max(1));
        let level_index = ((elapsed_ms - delay_ms) / level_duration_ms)
            .min((settings.recoil_levels.len() - 1) as u128) as usize;
        i64::from(settings.recoil_levels[level_index])
            .saturating_mul(i64::from(settings.recoil_percentage))
            / 100
    }
}

#[cfg(test)]
mod tests {
    use super::{
        has_fresh_target, recoil_move, recoil_settings, restore_remainder, set_target,
        should_apply_target, trigger_button_index, MovementState, MovementTarget, RecoilSettings,
        VerticalAdjustmentClock, TARGET_FRESHNESS_MS,
    };
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn trigger_mapping_excludes_left_and_middle() {
        assert_eq!(trigger_button_index("left"), None);
        assert_eq!(trigger_button_index("right"), Some(1));
        assert_eq!(
            trigger_button_index("middle"),
            None,
            "middle cycles profiles"
        );
        assert_eq!(trigger_button_index("m4"), Some(3));
        assert_eq!(trigger_button_index("m5"), Some(4));
    }

    #[test]
    fn motion_switch_gates_all_target_commands() {
        let target = MovementTarget {
            target_valid: true,
            motion_enabled: false,
            ..MovementTarget::default()
        };
        assert!(!should_apply_target(&target, true));
        assert!(!should_apply_target(
            &MovementTarget {
                motion_enabled: true,
                ..target.clone()
            },
            false
        ));
        assert!(should_apply_target(
            &MovementTarget {
                motion_enabled: true,
                ..target
            },
            true
        ));
    }

    #[test]
    fn always_on_motion_bypasses_only_the_trigger_gate() {
        let target = MovementTarget {
            target_valid: true,
            motion_enabled: true,
            motion_always_on: true,
            ..MovementTarget::default()
        };
        assert!(should_apply_target(&target, false));
        assert!(!should_apply_target(
            &MovementTarget {
                target_valid: false,
                ..target
            },
            false
        ));
    }

    #[test]
    fn recoil_is_sampled_by_elapsed_time_and_added_before_clamping() {
        let start = Instant::now();
        let settings = RecoilSettings {
            recoil_enabled: true,
            recoil_levels: vec![10, 20],
            recoil_start_delay_ms: 100,
            recoil_level_duration_ms: 50,
            recoil_percentage: 50,
        };
        let mut clock = VerticalAdjustmentClock::default();
        assert_eq!(clock.sample(true, &settings, start), 0);
        assert_eq!(
            clock.sample(true, &settings, start + Duration::from_millis(100)),
            5
        );
        assert_eq!(
            clock.sample(true, &settings, start + Duration::from_millis(150)),
            10
        );

        let target = MovementTarget {
            target_valid: true,
            dx: 40,
            dy: 28,
            ..MovementTarget::default()
        };
        assert_eq!(target.device_move(10).0, (32, 32));
    }

    #[test]
    fn recoil_clock_resets_when_the_left_button_is_released() {
        let start = Instant::now();
        let settings = RecoilSettings {
            recoil_enabled: true,
            recoil_levels: vec![10, 20],
            recoil_level_duration_ms: 50,
            recoil_percentage: 100,
            ..RecoilSettings::default()
        };
        let mut clock = VerticalAdjustmentClock::default();
        assert_eq!(clock.sample(true, &settings, start), 10);
        assert_eq!(
            clock.sample(true, &settings, start + Duration::from_millis(60)),
            20
        );
        assert_eq!(
            clock.sample(false, &settings, start + Duration::from_millis(61)),
            0
        );
        assert_eq!(
            clock.sample(true, &settings, start + Duration::from_millis(62)),
            10
        );
    }

    #[test]
    fn auto_aim_moves_go_out_whole_and_first_and_survive_other_events() {
        use super::{has_pending_move, take_target};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let event = |dx: i64, valid: bool, flick: bool| {
            serde_json::json!({
                "target_valid": valid, "dx": dx, "dy": -300, "motion_enabled": true,
                "motion_always_on": false, "trigger_button": "m4",
                "timestamp_ms": now_ms, "recoil": {}, "flick": flick,
            })
        };
        let state = MovementState::default();
        set_target(&state, &event(900, true, true));
        // A late Motion event (lost target) must not merge the move away.
        set_target(&state, &event(0, false, false));
        assert!(has_pending_move(&state));
        let flick = take_target(&state).expect("the aim move");
        assert!(flick.flick);
        assert_eq!(flick.device_move(0).0, (900, -300), "one command");
        assert!(!has_pending_move(&state));

        let huge = MovementTarget { dx: 5000, ..flick };
        let ((x, _), rest) = huge.device_move(0);
        assert_eq!((x, rest.dx), (4096, 904));
        restore_remainder(&state, rest);
        assert!(take_target(&state).is_some_and(|t| t.flick && t.dx == 904));
    }

    #[test]
    fn recoil_move_is_clamped() {
        assert_eq!(recoil_move(12), (0, 12));
        assert_eq!(recoil_move(100), (0, 32));
        assert_eq!(recoil_move(-100), (0, -32));
    }

    #[test]
    fn recoil_requires_a_fresh_valid_motion_target() {
        let state = MovementState::default();
        assert!(!has_fresh_target(&state));
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let target = |timestamp_ms, target_valid| {
            serde_json::json!({
                "target_valid": target_valid,
                "dx": 0,
                "dy": 0,
                "motion_enabled": true,
                "motion_always_on": false,
                "trigger_button": "right",
                "timestamp_ms": timestamp_ms,
                "recoil": {}
            })
        };
        set_target(&state, &target(now_ms, true));
        assert!(has_fresh_target(&state));
        set_target(&state, &target(now_ms, false));
        assert!(!has_fresh_target(&state));
        set_target(
            &state,
            &target(now_ms.saturating_sub(TARGET_FRESHNESS_MS + 1), true),
        );
        assert!(!has_fresh_target(&state));
    }

    #[test]
    fn recoil_settings_update_even_when_the_target_is_invalid() {
        let state = MovementState::default();
        set_target(
            &state,
            &serde_json::json!({
                "target_valid": false,
                "dx": 0,
                "dy": 0,
                "motion_enabled": true,
                "motion_always_on": false,
                "trigger_button": "right",
                "timestamp_ms": 0,
                "recoil": {
                    "recoilEnabled": true,
                    "recoilLevels": [3, 7],
                    "recoilStartDelayMs": 125,
                    "recoilLevelDurationMs": 40,
                    "recoilPercentage": 80
                },
            }),
        );
        let cached = recoil_settings(&state);
        assert!(cached.recoil_enabled);
        assert_eq!(cached.recoil_levels, vec![3, 7]);
        assert_eq!(cached.recoil_start_delay_ms, 125);
        assert_eq!(cached.recoil_level_duration_ms, 40);
    }

    #[test]
    fn stale_or_future_targets_fail_the_freshness_gate() {
        let target = MovementTarget {
            timestamp_ms: 899,
            ..MovementTarget::default()
        };
        assert!(!target.is_fresh_at(1_000));
        assert!(!MovementTarget {
            timestamp_ms: 1_101,
            ..MovementTarget::default()
        }
        .is_fresh_at(1_000));
        assert!(MovementTarget {
            timestamp_ms: 1_050,
            ..MovementTarget::default()
        }
        .is_fresh_at(1_000));
    }

    #[test]
    fn deltas_beyond_one_command_carry_over_but_clipped_recoil_does_not() {
        let target = MovementTarget {
            target_valid: true,
            dx: 45,
            dy: -40,
            ..MovementTarget::default()
        };
        let (command, remainder) = target.device_move(0);
        assert_eq!(command, (32, -32));
        assert_eq!((remainder.dx, remainder.dy), (13, -8));

        // Recoil pushes Y past the clamp: only the target's share carries over.
        let target = MovementTarget {
            target_valid: true,
            dx: 0,
            dy: 10,
            ..MovementTarget::default()
        };
        let (command, remainder) = target.device_move(30);
        assert_eq!(command, (0, 32));
        assert_eq!((remainder.dx, remainder.dy), (0, 8));
        let (_, remainder) = MovementTarget {
            dy: -5,
            ..target.clone()
        }
        .device_move(40);
        assert_eq!(remainder.dy, 0, "recoil alone overflowed");

        let state = MovementState::default();
        restore_remainder(
            &state,
            MovementTarget {
                timestamp_ms: 1_000,
                dx: 13,
                dy: -8,
                ..target.clone()
            },
        );
        let event = |valid: bool, dx: i64, dy: i64| {
            serde_json::json!({
                "target_valid": valid, "dx": dx, "dy": dy, "motion_enabled": true,
                "motion_always_on": false, "trigger_button": "right",
                "timestamp_ms": 1_004, "recoil": {},
            })
        };
        set_target(&state, &event(true, 2, 1));
        let merged = state.pending.lock().unwrap().clone().unwrap();
        assert_eq!((merged.dx, merged.dy, merged.timestamp_ms), (15, -7, 1_004));

        // A newer lost target discards the remainder.
        let state = MovementState::default();
        set_target(&state, &event(false, 0, 0));
        restore_remainder(
            &state,
            MovementTarget {
                timestamp_ms: 1_000,
                dx: 13,
                ..target
            },
        );
        let merged = state.pending.lock().unwrap().clone().unwrap();
        assert_eq!((merged.dx, merged.dy, merged.target_valid), (0, 0, false));
    }

    #[test]
    fn pending_deltas_sum_until_consumed_and_reset_after_target_loss_or_staleness() {
        let event = |timestamp_ms, dx, dy, target_valid| MovementTarget {
            timestamp_ms,
            dx,
            dy,
            target_valid,
            ..MovementTarget::default()
        };
        let merged = MovementTarget::merge_pending(
            Some(event(1_000, 4, -3, true)),
            event(1_004, -1, 5, true),
        );
        assert_eq!((merged.dx, merged.dy), (3, 2));

        let lost = MovementTarget::merge_pending(Some(merged), event(1_005, 0, 0, false));
        assert_eq!((lost.dx, lost.dy), (0, 0));

        let stale =
            MovementTarget::merge_pending(Some(event(899, 20, 20, true)), event(1_000, 1, 1, true));
        assert_eq!((stale.dx, stale.dy), (1, 1));
    }
}
