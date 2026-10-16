use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_DEVICE_DELTA: i64 = 32;
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
#[serde(default, rename_all = "camelCase")]
pub struct MovementTarget {
    pub target_valid: bool,
    pub dx: i64,
    pub dy: i64,
    pub motion_enabled: bool,
    pub trigger_button: String,
    pub timestamp_ms: u64,
    #[serde(alias = "nativeMotion", alias = "native_motion")]
    pub recoil: RecoilSettings,
}

#[derive(Default)]
pub struct MovementState(Mutex<Option<MovementTarget>>);

pub fn set_target(state: &MovementState, value: &serde_json::Value) {
    let Ok(target) = serde_json::from_value::<MovementTarget>(value.clone()) else {
        return;
    };
    if let Ok(mut pending) = state.0.lock() {
        *pending = Some(MovementTarget::merge_pending(pending.take(), target));
    }
}

pub fn take_target(state: &MovementState) -> Option<MovementTarget> {
    let target = state.0.lock().ok()?.take()?;
    target.is_fresh().then_some(target)
}

pub fn trigger_button_index(button: &str) -> Option<u16> {
    match button {
        "right" => Some(1),
        "middle" => Some(2),
        "m4" => Some(3),
        "m5" => Some(4),
        _ => None,
    }
}

pub fn should_apply_target(target: &MovementTarget, trigger_held: bool) -> bool {
    target.motion_enabled && trigger_held && target.target_valid
}

impl MovementTarget {
    fn merge_pending(previous: Option<Self>, mut current: Self) -> Self {
        if current.target_valid {
            if let Some(previous) = previous.filter(|previous| {
                previous.target_valid && previous.is_fresh_at(current.timestamp_ms)
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

    pub fn device_delta(&self, recoil_y: i64) -> (i16, i16) {
        let x = self.dx.clamp(-MAX_DEVICE_DELTA, MAX_DEVICE_DELTA);
        let y = self
            .dy
            .saturating_add(recoil_y)
            .clamp(-MAX_DEVICE_DELTA, MAX_DEVICE_DELTA);
        (x as i16, y as i16)
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

    pub fn sample(&mut self, trigger_held: bool, settings: &RecoilSettings, now: Instant) -> i64 {
        if !trigger_held || !settings.recoil_enabled || settings.recoil_levels.is_empty() {
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
        should_apply_target, trigger_button_index, MovementTarget, RecoilSettings,
        VerticalAdjustmentClock,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn trigger_mapping_excludes_left_and_uses_four_supported_buttons() {
        assert_eq!(trigger_button_index("left"), None);
        assert_eq!(trigger_button_index("right"), Some(1));
        assert_eq!(trigger_button_index("middle"), Some(2));
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
        assert_eq!(target.device_delta(10), (32, 32));
    }

    #[test]
    fn recoil_clock_resets_when_the_trigger_is_released() {
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
