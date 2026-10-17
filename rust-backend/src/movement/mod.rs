//! Motion: target error → device counts, split by concern.
//!
//! - [`controller`]: the closed-loop controller (fixed motion model, no
//!   online learning) that combines the parts below;
//! - [`ncaf`]: NCAF three-zone speed factor;
//! - [`bezier`]: stroke path and speed curve;
//! - [`windmouse`]: WindMouse wind;
//! - [`target_smoother`]: target-switch hysteresis;
//! - this module: the movement ticker, batching and the host event format.

pub mod bezier;
pub mod controller;
pub mod ncaf;
pub mod target_smoother;
pub mod windmouse;

pub use controller::{FrameError, MotionModel, MovementConfig, MovementController};
pub use ncaf::ncaf_factor;

use crate::config::RecoilSettings;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn crop_target_error(
    target_xy: (f64, f64),
    crop_origin: (u32, u32),
    processed_size: (u32, u32),
    offsets: (f64, f64),
) -> (f64, f64) {
    (
        target_xy.0.trunc() - crop_origin.0 as f64 - processed_size.0 as f64 / 2.0 + offsets.0,
        target_xy.1.trunc() - crop_origin.1 as f64 - processed_size.1 as f64 / 2.0 + offsets.1,
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MovementEvent {
    #[serde(default)]
    pub config_revision: u64,
    pub event: String,
    pub frame_id: u64,
    pub target_valid: bool,
    pub dx: i64,
    pub dy: i64,
    #[serde(default)]
    pub motion_enabled: bool,
    #[serde(default)]
    pub motion_always_on: bool,
    pub trigger_button: String,
    pub timestamp_ms: u64,
    pub recoil: RecoilSettings,
}

#[derive(Clone, Debug)]
pub struct MovementRuntimeSettings {
    pub config_revision: u64,
    pub motion_enabled: bool,
    pub motion_always_on: bool,
    pub sens: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub trigger_button: String,
    /// The selected physical trigger is held (from the host's device status).
    /// Moves count as applied under either this gate or Motion Always-on.
    pub trigger_held: bool,
    pub recoil: RecoilSettings,
}

impl MovementRuntimeSettings {
    fn output_active(&self) -> bool {
        self.motion_enabled && (self.motion_always_on || self.trigger_held)
    }
}

#[derive(Clone, Debug)]
pub struct MovementTargetSample {
    pub config_revision: u64,
    pub frame_id: u64,
    /// The selected target, unsmoothed.
    pub target: Option<(f64, f64)>,
    /// Smoothing factor per axis drawn for this track (1 = none); applied to
    /// the target's own motion by the controller.
    pub smoothing_alpha: (f64, f64),
    pub crop_origin: (u32, u32),
    pub processed_size: (u32, u32),
    /// Monotonic receive time; set by `MovementTicker::update`.
    pub received_s: f64,
}

struct TickerState {
    latest: Option<(MovementTargetSample, Instant)>,
    stopping: bool,
}

/// Recomputes a small relative delta on a fixed cadence from the newest target.
/// Detection updates this state; movement never waits for the next detection frame.
pub struct MovementTicker {
    shared: Arc<(Mutex<TickerState>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Held-trigger tracking summary for the diagnostics log, at most one line
/// every two seconds, to compare real-game tracking with simulation.
#[derive(Default)]
struct MotionSummary {
    started: Option<Instant>,
    ticks: u32,
    frames: u32,
    last_frame: Option<u64>,
    error_px: f64,
    counts: i64,
}

impl MotionSummary {
    const PERIOD: Duration = Duration::from_secs(2);

    fn add(
        &mut self,
        controller: &MovementController,
        frame_id: u64,
        step: (i64, i64),
    ) -> Option<String> {
        let started = *self.started.get_or_insert_with(Instant::now);
        self.ticks += 1;
        if self.last_frame != Some(frame_id) {
            self.last_frame = Some(frame_id);
            self.frames += 1;
            self.error_px += controller.frame_error().0.hypot(controller.frame_error().1);
        }
        self.counts += step.0.abs() + step.1.abs();
        (started.elapsed() >= Self::PERIOD).then(|| self.take(controller))
    }

    /// Trigger released: report what was gathered, if it is worth a line.
    fn finish(&mut self, controller: &MovementController) -> Option<String> {
        let started = self.started?;
        if started.elapsed() < Duration::from_millis(250) {
            *self = Self::default();
            return None;
        }
        Some(self.take(controller))
    }

    fn take(&mut self, controller: &MovementController) -> String {
        let seconds = self
            .started
            .map_or(0.0, |at| at.elapsed().as_secs_f64())
            .max(1e-3);
        let model = controller.model();
        let line = format!(
            "motion held {seconds:.1} s: {} frames ({:.0}/s), {} ticks, mean target error {:.1} px, {:.0} counts/s; model {:.3} px/count, latency {:.0} ms",
            self.frames,
            f64::from(self.frames) / seconds,
            self.ticks,
            self.error_px / f64::from(self.frames.max(1)),
            self.counts as f64 / seconds,
            model.px_per_count,
            model.latency_s * 1000.0,
        );
        *self = Self::default();
        line
    }
}

impl MovementTicker {
    pub fn new(
        initial_sens: f64,
        settings: Arc<dyn Fn() -> Option<MovementRuntimeSettings> + Send + Sync>,
        on_event: Arc<dyn Fn(MovementEvent) + Send + Sync>,
    ) -> Self {
        let shared = Arc::new((
            Mutex::new(TickerState {
                latest: None,
                stopping: false,
            }),
            Condvar::new(),
        ));
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("sysx-movement-ticker".into())
            .spawn(move || {
                let mut controller = MovementController::new(initial_sens);
                let mut sens = initial_sens;
                let mut sent_invalid = false;
                let mut summary = MotionSummary::default();
                loop {
                    let (sample, updated_at) = {
                        let (lock, wake) = &*worker_shared;
                        let Ok(mut state) = lock.lock() else {
                            return;
                        };
                        while !state.stopping && state.latest.is_none() {
                            let Ok(next) = wake.wait(state) else {
                                return;
                            };
                            state = next;
                        }
                        if state.stopping {
                            return;
                        }
                        let latest = state.latest.clone().expect("latest movement target");
                        (latest.0, latest.1)
                    };

                    let fresh = updated_at.elapsed() <= Duration::from_millis(100);
                    if fresh {
                        let Some(runtime) = settings() else {
                            controller.reset();
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        };
                        if !runtime.motion_enabled
                            || sample.config_revision != runtime.config_revision
                        {
                            controller.reset();
                            if !sent_invalid {
                                on_event(MovementEvent {
                                    config_revision: runtime.config_revision,
                                    event: "movement_target".into(),
                                    frame_id: sample.frame_id,
                                    target_valid: false,
                                    dx: 0,
                                    dy: 0,
                                    motion_enabled: false,
                                    motion_always_on: false,
                                    trigger_button: runtime.trigger_button,
                                    timestamp_ms: unix_ms(),
                                    recoil: runtime.recoil,
                                });
                                sent_invalid = true;
                            }
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        if sample.target.is_none() {
                            controller.reset();
                            if !sent_invalid {
                                on_event(MovementEvent {
                                    config_revision: runtime.config_revision,
                                    event: "movement_target".into(),
                                    frame_id: sample.frame_id,
                                    target_valid: false,
                                    dx: 0,
                                    dy: 0,
                                    motion_enabled: runtime.motion_enabled,
                                    motion_always_on: runtime.motion_always_on,
                                    trigger_button: runtime.trigger_button,
                                    timestamp_ms: unix_ms(),
                                    recoil: runtime.recoil,
                                });
                                sent_invalid = true;
                            }
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        if sens != runtime.sens {
                            let _ = controller.set_sensitivity(runtime.sens);
                            sens = runtime.sens;
                        }
                        let now = monotonic_seconds();
                        let (dx, dy) = controller.delta(
                            &sample,
                            (runtime.offset_x, runtime.offset_y),
                            now,
                            runtime.output_active(),
                        );
                        if runtime.output_active() {
                            if let Some(line) = summary.add(&controller, sample.frame_id, (dx, dy))
                            {
                                crate::diagnostics::record(&line);
                            }
                        } else if let Some(line) = summary.finish(&controller) {
                            crate::diagnostics::record(&line);
                        }
                        on_event(MovementEvent {
                            config_revision: runtime.config_revision,
                            event: "movement_target".into(),
                            frame_id: sample.frame_id,
                            target_valid: sample.target.is_some(),
                            dx,
                            dy,
                            motion_enabled: runtime.motion_enabled,
                            motion_always_on: runtime.motion_always_on,
                            trigger_button: runtime.trigger_button,
                            timestamp_ms: unix_ms(),
                            recoil: runtime.recoil,
                        });
                        sent_invalid = false;
                    } else if !sent_invalid {
                        controller.reset();
                        if let Some(runtime) = settings() {
                            on_event(MovementEvent {
                                config_revision: runtime.config_revision,
                                event: "movement_target".into(),
                                frame_id: sample.frame_id,
                                target_valid: false,
                                dx: 0,
                                dy: 0,
                                motion_enabled: runtime.motion_enabled,
                                motion_always_on: runtime.motion_always_on,
                                trigger_button: runtime.trigger_button,
                                timestamp_ms: unix_ms(),
                                recoil: runtime.recoil,
                            });
                        }
                        sent_invalid = true;
                    }

                    let (lock, wake) = &*worker_shared;
                    let Ok(state) = lock.lock() else {
                        return;
                    };
                    if state.stopping {
                        return;
                    }
                    let _ = wake.wait_timeout(state, Duration::from_millis(4));
                }
            })
            .expect("movement ticker thread");
        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }

    pub fn update(&self, mut sample: MovementTargetSample) {
        sample.received_s = monotonic_seconds();
        let (lock, wake) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            if !state.stopping {
                state.latest = Some((sample, Instant::now()));
                wake.notify_one();
            }
        }
    }

    pub fn close(&self, timeout: Duration) -> bool {
        let (lock, wake) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            state.stopping = true;
            wake.notify_all();
        }
        let Ok(mut slot) = self.worker.lock() else {
            return false;
        };
        let Some(worker) = slot.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return false;
        }
        worker.join().is_ok()
    }
}

fn monotonic_seconds() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub fn merge_movement_events(
    previous: Option<MovementEvent>,
    mut current: MovementEvent,
) -> MovementEvent {
    if current.target_valid {
        if let Some(previous) = previous.filter(|previous| {
            previous.target_valid
                && previous.config_revision == current.config_revision
                && !is_stale(previous)
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

struct BatchState {
    pending: Option<MovementEvent>,
    stopping: bool,
}
pub struct MovementBatcher {
    shared: Arc<(Mutex<BatchState>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl MovementBatcher {
    pub fn new(on_batch: Arc<dyn Fn(MovementEvent) + Send + Sync>) -> Self {
        Self::with_interval(Duration::from_millis(4), on_batch)
    }
    pub fn with_interval(
        interval: Duration,
        on_batch: Arc<dyn Fn(MovementEvent) + Send + Sync>,
    ) -> Self {
        let shared = Arc::new((
            Mutex::new(BatchState {
                pending: None,
                stopping: false,
            }),
            Condvar::new(),
        ));
        let worker_state = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("sysx-movement-batcher".into())
            .spawn(move || loop {
                let batch = {
                    let (lock, wake) = &*worker_state;
                    let Ok(mut state) = lock.lock() else {
                        return;
                    };
                    while state.pending.is_none() && !state.stopping {
                        let Ok(next) = wake.wait(state) else {
                            return;
                        };
                        state = next;
                    }
                    if state.stopping {
                        return;
                    }
                    let deadline = Instant::now() + interval;
                    while !state.stopping {
                        let remain = deadline.saturating_duration_since(Instant::now());
                        if remain.is_zero() {
                            break;
                        }
                        let Ok((next, timeout)) = wake.wait_timeout(state, remain) else {
                            return;
                        };
                        state = next;
                        if timeout.timed_out() {
                            break;
                        }
                    }
                    if state.stopping {
                        return;
                    }
                    state.pending.take()
                };
                if let Some(event) = batch.filter(|event| !is_stale(event)) {
                    on_batch(event);
                }
            })
            .expect("movement batcher thread");
        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }
    pub fn submit(&self, event: MovementEvent) {
        let (lock, wake) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            if !state.stopping {
                state.pending = Some(merge_movement_events(state.pending.take(), event));
                wake.notify_one();
            }
        }
    }
    pub fn close(&self, timeout: Duration) -> bool {
        let (lock, wake) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            state.stopping = true;
            wake.notify_all();
        }
        let Ok(mut slot) = self.worker.lock() else {
            return false;
        };
        let Some(worker) = slot.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return false;
        }
        worker.join().is_ok()
    }
}

fn is_stale(event: &MovementEvent) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(u64::MAX);
    now.saturating_sub(event.timestamp_ms) > 100 || event.timestamp_ms > now.saturating_add(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    fn seeded(seed: u64) -> MovementController {
        MovementController::with_rng(MovementConfig::default(), StdRng::seed_from_u64(seed))
    }

    fn event(
        frame_id: u64,
        dx: i64,
        dy: i64,
        trigger_button: &str,
        timestamp_ms: u64,
    ) -> MovementEvent {
        MovementEvent {
            config_revision: 0,
            event: "movement_target".into(),
            frame_id,
            target_valid: true,
            dx,
            dy,
            motion_enabled: true,
            motion_always_on: false,
            trigger_button: trigger_button.into(),
            timestamp_ms,
            recoil: RecoilSettings::default(),
        }
    }

    fn runtime(always_on: bool, held: bool) -> MovementRuntimeSettings {
        MovementRuntimeSettings {
            config_revision: 2,
            motion_enabled: true,
            motion_always_on: always_on,
            sens: 0.25,
            offset_x: 0.0,
            offset_y: 0.0,
            trigger_button: "right".into(),
            trigger_held: held,
            recoil: RecoilSettings::default(),
        }
    }

    fn sample(revision: u64) -> MovementTargetSample {
        MovementTargetSample {
            config_revision: revision,
            frame_id: 1,
            target: Some((1000.0, 32.0)),
            smoothing_alpha: (1.0, 1.0),
            crop_origin: (0, 0),
            processed_size: (64, 64),
            received_s: 1.0,
        }
    }

    #[test]
    fn ticker_rejects_a_previous_modes_sample_until_a_current_frame_arrives() {
        let (tx, rx) = std::sync::mpsc::channel();
        let ticker = MovementTicker::new(
            0.25,
            Arc::new(|| Some(runtime(false, true))),
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        ticker.update(sample(1));
        let invalid = rx.recv_timeout(Duration::from_secs(2));
        ticker.update(sample(2));
        let valid = rx.recv_timeout(Duration::from_secs(2));
        assert!(ticker.close(Duration::from_secs(1)));
        let invalid = invalid.unwrap();
        assert_eq!(invalid.config_revision, 2);
        assert!(!invalid.target_valid);
        assert_eq!((invalid.dx, invalid.dy), (0, 0));
        let valid = valid.unwrap();
        assert!(valid.target_valid);
        assert_eq!(valid.config_revision, 2);
    }

    #[test]
    fn batches_do_not_sum_motion_across_control_revisions() {
        let first = event(1, 12, -7, "right", unix_ms());
        let mut next = event(2, 3, 4, "right", unix_ms());
        next.config_revision = 1;
        let merged = merge_movement_events(Some(first), next);
        assert_eq!((merged.dx, merged.dy), (3, 4));
        assert_eq!(merged.config_revision, 1);
    }

    #[test]
    fn queued_movement_deltas_sum_and_keep_latest_metadata() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let first = event(1, 4, -2, "right", now.saturating_sub(4));
        let latest = event(2, -1, 3, "m4", now);
        let merged = merge_movement_events(Some(first), latest);
        assert_eq!((merged.dx, merged.dy), (3, 1));
        assert_eq!(
            (
                merged.frame_id,
                merged.timestamp_ms,
                merged.trigger_button.as_str()
            ),
            (2, now, "m4")
        );
    }

    #[test]
    fn queued_movement_deltas_discard_expired_pending_events() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let stale = event(1, 70, -80, "right", now.saturating_sub(101));
        let latest = event(2, 3, -4, "m4", now);

        let merged = merge_movement_events(Some(stale), latest);

        assert_eq!((merged.dx, merged.dy), (3, -4));
        assert_eq!(merged.frame_id, 2);
    }

    #[test]
    fn a_lost_target_discards_pending_movement_and_carries_latest_motion_settings() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let pending = event(1, 9, -7, "right", now);
        let mut lost = event(2, 0, 0, "m4", now);
        lost.target_valid = false;
        lost.recoil.recoil_enabled = true;

        let merged = merge_movement_events(Some(pending), lost);

        assert_eq!((merged.dx, merged.dy), (0, 0));
        assert!(!merged.target_valid);
        assert_eq!(merged.trigger_button, "m4");
        assert!(merged.recoil.recoil_enabled);
    }

    #[test]
    fn crop_target_coordinates_truncate_toward_zero_before_center_and_offsets() {
        assert_eq!(
            crop_target_error((211.9, 190.9), (100, 80), (200, 200), (2.0, -3.0)),
            (13.0, 7.0)
        );
        assert_eq!(
            crop_target_error((-1.9, -1.9), (0, 0), (0, 0), (0.0, 0.0)),
            (-1.0, -1.0)
        );
    }

    #[test]
    fn held_trigger_summary_reports_frames_and_the_model() {
        let controller = seeded(1);
        let mut summary = MotionSummary::default();
        assert!(summary.finish(&controller).is_none(), "nothing gathered");
        for tick in 0..10u64 {
            assert!(summary.add(&controller, tick / 2, (2, -1)).is_none());
        }
        summary.started = Some(Instant::now() - Duration::from_secs(1));
        let line = summary.finish(&controller).expect("summary line");
        assert!(line.contains("5 frames"), "{line}");
        assert!(line.contains("10 ticks"), "{line}");
        assert!(
            line.contains("model 0.250 px/count, latency 30 ms"),
            "{line}"
        );
    }
}
