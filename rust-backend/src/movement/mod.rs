//! Motion: target error → device counts, plus the 4 ms ticker and batcher
//! that carry deltas to the host.
//!
//! - [`normal`]: the per-frame Normal motion (speed, smoothing inside the
//!   Smooth FOV, fractional carry-over).
//! - this module: crop-target conversion, the controller wrapper that acts
//!   once per frame, the ticker, the batcher and the IPC event.

pub mod abcurves;
pub mod flick;
pub mod normal;

/// Default Normal speed per axis: in simulation it stays stable from 16 to
/// 50 ms latency with px/count off by 0.5–2× (0.06 was not at 50 ms × 2).
pub const DEFAULT_SPEED: f64 = 0.045;

use crate::config::RecoilSettings;
use crate::targeting::fov_radius_px;
use normal::{counts_per_px, NormalMotion, NormalStep};
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Per-tick Motion settings from the runtime.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionParams {
    pub offsets: (f64, f64),
    pub speed: (f64, f64),
    pub smooth_fov_degrees: u16,
    pub no_deadzone: bool,
}

/// Turns each new frame's target into one move; ticks without a new frame
/// send nothing, so an error is never sent twice.
pub struct MovementController {
    sens: f64,
    last_frame_id: Option<u64>,
    motion: NormalMotion,
    /// Last frame's error, for the diagnostics summary.
    frame_error: (f64, f64),
}

impl MovementController {
    pub fn new(sens: f64) -> Self {
        Self {
            sens: sens.clamp(0.01, 10.0),
            last_frame_id: None,
            motion: NormalMotion::default(),
            frame_error: (0.0, 0.0),
        }
    }

    /// Target lost or Motion inactive: forget the frame and the carry-over.
    pub fn reset(&mut self) {
        self.last_frame_id = None;
        self.motion.reset();
    }

    pub fn set_sensitivity(&mut self, sens: f64) -> Result<(), &'static str> {
        if !(0.01..=10.0).contains(&sens) {
            return Err("sens must be 0.01..10");
        }
        self.sens = sens;
        self.reset();
        Ok(())
    }

    pub fn delta(&mut self, sample: &MovementTargetSample, params: MotionParams) -> (i64, i64) {
        let Some(target) = sample.target else {
            self.reset();
            return (0, 0);
        };
        if self.last_frame_id == Some(sample.frame_id) {
            return (0, 0);
        }
        self.last_frame_id = Some(sample.frame_id);
        let error = crop_target_error(
            target,
            sample.crop_origin,
            sample.processed_size,
            params.offsets,
        );
        self.frame_error = error;
        let (width, height) = sample.processed_size;
        let smooth_radius = fov_radius_px(width, height, params.smooth_fov_degrees);
        let strength = |alpha: f64| {
            if alpha.is_finite() && alpha > 0.0 {
                (1.0 / alpha).max(1.0)
            } else {
                1.0
            }
        };
        self.motion.step(NormalStep {
            error,
            speed: params.speed,
            smooth: (
                strength(sample.smoothing_alpha.0),
                strength(sample.smoothing_alpha.1),
            ),
            smooth_fov_px: f64::from(smooth_radius),
            counts_per_px: counts_per_px(self.sens),
            dead_zone_px: if params.no_deadzone {
                0.0
            } else {
                normal::DEAD_ZONE_PX
            },
        })
    }
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
    /// Auto's aim move: the host sends it in one command instead of ±32 steps.
    #[serde(default)]
    pub flick: bool,
}

#[derive(Clone, Debug)]
pub struct MovementRuntimeSettings {
    pub renderer_profile: Option<Arc<abcurves::Profile>>,
    pub config_revision: u64,
    pub motion_enabled: bool,
    pub motion_always_on: bool,
    pub sens: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Normal motion speed per axis (fraction of the error per frame).
    pub speed_x: f64,
    pub speed_y: f64,
    /// Inside this FOV the speed is divided by the smoothing strength.
    pub smooth_fov_degrees: u16,
    pub no_deadzone: bool,
    pub trigger_button: String,
    /// The selected physical trigger is held (from the host's device status).
    pub trigger_held: bool,
    /// Auto's aim mode is held: take one target and move exactly onto it
    /// (`flick`) instead of following targets.
    pub auto_flick: bool,
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
    /// Smoothing factor per axis drawn for this track (1 = none); its
    /// strength (1/alpha) divides the speed inside the Smooth FOV.
    pub smoothing_alpha: (f64, f64),
    pub crop_origin: (u32, u32),
    pub processed_size: (u32, u32),
    /// Monotonic receive time; set by `MovementTicker::update`.
    pub received_s: f64,
    /// How long ago the receiver had this frame (decode and processing);
    /// `received_s` and freshness count from then, not from publication.
    pub age_s: f64,
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
    flick_done: Arc<AtomicBool>,
}

/// Held-trigger Motion summary for the diagnostics log, at most one line
/// every two seconds.
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
            self.error_px += controller.frame_error.0.hypot(controller.frame_error.1);
        }
        self.counts += step.0.abs() + step.1.abs();
        (started.elapsed() >= Self::PERIOD).then(|| self.take())
    }

    /// Trigger released: report what was gathered, if it is worth a line.
    fn finish(&mut self) -> Option<String> {
        let started = self.started?;
        if started.elapsed() < Duration::from_millis(250) {
            *self = Self::default();
            return None;
        }
        Some(self.take())
    }

    fn take(&mut self) -> String {
        let seconds = self
            .started
            .map_or(0.0, |at| at.elapsed().as_secs_f64())
            .max(1e-3);
        let line = format!(
            "motion held {seconds:.1} s: {} frames ({:.0}/s), {} ticks, mean target error {:.1} px, {:.0} counts/s",
            self.frames,
            f64::from(self.frames) / seconds,
            self.ticks,
            self.error_px / f64::from(self.frames.max(1)),
            self.counts as f64 / seconds,
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
    ) -> io::Result<Self> {
        let shared = Arc::new((
            Mutex::new(TickerState {
                latest: None,
                stopping: false,
            }),
            Condvar::new(),
        ));
        let worker_shared = Arc::clone(&shared);
        let flick_done = Arc::new(AtomicBool::new(false));
        let worker_flick_done = Arc::clone(&flick_done);
        let worker = thread::Builder::new()
            .name("sysx-movement-ticker".into())
            .spawn(move || {
                let mut flick = flick::Flick::default();
                let mut rendered = abcurves::MotionOutput::default();
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
                        let Some(latest) = state.latest.clone() else {
                            continue;
                        };
                        (latest.0, latest.1)
                    };

                    let fresh = updated_at.elapsed() <= Duration::from_millis(100);
                    if fresh {
                        let Some(runtime) = settings() else {
                            controller.reset();
                            rendered.reset();
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        };
                        if !runtime.motion_enabled
                            || sample.config_revision != runtime.config_revision
                        {
                            controller.reset();
                            rendered.reset();
                            flick.reset();
                            worker_flick_done.store(false, Ordering::Release);
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
                                    flick: false,
                                });
                                sent_invalid = true;
                            }
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        if runtime.auto_flick {
                            controller.reset();
                            rendered.reset();
                            if sens != runtime.sens {
                                let _ = controller.set_sensitivity(runtime.sens);
                                sens = runtime.sens;
                            }
                            let found = flick
                                .searching()
                                .then(|| flick_target(&sample, &runtime))
                                .flatten();
                            let (dx, dy) = flick.step(found, counts_per_px(sens));
                            on_event(MovementEvent {
                                config_revision: runtime.config_revision,
                                event: "movement_target".into(),
                                frame_id: sample.frame_id,
                                // Valid from the lock on, so the host keeps
                                // the rest of the move until it is sent.
                                target_valid: !flick.searching(),
                                flick: true,
                                dx,
                                dy,
                                motion_enabled: true,
                                motion_always_on: false,
                                trigger_button: runtime.trigger_button,
                                timestamp_ms: unix_ms(),
                                recoil: runtime.recoil,
                            });
                            // Done only after the move was handed on, so the
                            // click can never overtake it.
                            worker_flick_done.store(flick.done(), Ordering::Release);
                            sent_invalid = false;
                            // Poll for the first target every 1 ms instead of
                            // waiting for the next tick.
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        flick.reset();
                        worker_flick_done.store(false, Ordering::Release);
                        if sample.target.is_none() {
                            controller.reset();
                            rendered.reset();
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
                                    flick: false,
                                });
                                sent_invalid = true;
                            }
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        if sens != runtime.sens {
                            let _ = controller.set_sensitivity(runtime.sens);
                            sens = runtime.sens;
                            rendered.reset();
                        }
                        let output_active = runtime.output_active();
                        let (dx, dy) = if output_active {
                            controller.delta(
                                &sample,
                                MotionParams {
                                    offsets: (runtime.offset_x, runtime.offset_y),
                                    speed: (runtime.speed_x, runtime.speed_y),
                                    smooth_fov_degrees: runtime.smooth_fov_degrees,
                                    no_deadzone: runtime.no_deadzone,
                                },
                            )
                        } else {
                            // A released trigger must not build fractional
                            // movement that can leak into the next press.
                            controller.reset();
                            rendered.reset();
                            (0, 0)
                        };
                        let (dx, dy) = if let Some(profile) = runtime.renderer_profile.as_ref() {
                            if !output_active
                                || (!runtime.no_deadzone
                                    && controller.frame_error.0.hypot(controller.frame_error.1)
                                        <= normal::DEAD_ZONE_PX)
                            {
                                rendered.reset();
                                (0, 0)
                            } else {
                                rendered
                                    .filter(Some(profile), (dx, dy), Instant::now())
                                    .unwrap_or_else(|status| {
                                        crate::diagnostics::record(&format!(
                                            "ABCurves renderer stopped: status {status}"
                                        ));
                                        (0, 0)
                                    })
                            }
                        } else {
                            // Default Normal bypasses ABCurves completely.
                            (dx, dy)
                        };
                        if output_active {
                            if let Some(line) = summary.add(&controller, sample.frame_id, (dx, dy))
                            {
                                crate::diagnostics::record(&line);
                            }
                        } else if let Some(line) = summary.finish() {
                            crate::diagnostics::record(&line);
                        }
                        on_event(MovementEvent {
                            config_revision: runtime.config_revision,
                            event: "movement_target".into(),
                            frame_id: sample.frame_id,
                            target_valid: sample.target.is_some()
                                && (runtime.renderer_profile.is_none() || !rendered.is_failed()),
                            dx,
                            dy,
                            motion_enabled: runtime.motion_enabled,
                            motion_always_on: runtime.motion_always_on,
                            trigger_button: runtime.trigger_button,
                            timestamp_ms: unix_ms(),
                            recoil: runtime.recoil,
                            flick: false,
                        });
                        sent_invalid = false;
                    } else if !sent_invalid {
                        controller.reset();
                        rendered.reset();
                        flick.reset();
                        worker_flick_done.store(false, Ordering::Release);
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
                                flick: false,
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
            })?;
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
            flick_done,
        })
    }

    /// Auto's aim move reached its locked target (see [`flick`]).
    pub fn flick_done(&self) -> bool {
        self.flick_done.load(Ordering::Acquire)
    }

    pub fn update(&self, mut sample: MovementTargetSample) {
        let age = Duration::from_secs_f64(sample.age_s.clamp(0.0, 1.0));
        sample.received_s = monotonic_seconds() - age.as_secs_f64();
        let received = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        let (lock, wake) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            if !state.stopping {
                state.latest = Some((sample, received));
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

impl Drop for MovementTicker {
    fn drop(&mut self) {
        let _ = self.close(Duration::from_secs(1));
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

/// The error Auto's aim mode locks: this frame's target, as Motion selects
/// it, with Auto's offsets.
fn flick_target(
    sample: &MovementTargetSample,
    runtime: &MovementRuntimeSettings,
) -> Option<(f64, f64)> {
    Some(crop_target_error(
        sample.target?,
        sample.crop_origin,
        sample.processed_size,
        (runtime.offset_x, runtime.offset_y),
    ))
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
            current.flick |= previous.flick;
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
    pub fn new(on_batch: Arc<dyn Fn(MovementEvent) + Send + Sync>) -> io::Result<Self> {
        Self::with_interval(Duration::from_millis(4), on_batch)
    }
    pub fn with_interval(
        interval: Duration,
        on_batch: Arc<dyn Fn(MovementEvent) + Send + Sync>,
    ) -> io::Result<Self> {
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
            })?;
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
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

impl Drop for MovementBatcher {
    fn drop(&mut self) {
        let _ = self.close(Duration::from_secs(1));
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
            flick: false,
        }
    }

    fn runtime(always_on: bool, held: bool) -> MovementRuntimeSettings {
        MovementRuntimeSettings {
            renderer_profile: None,
            config_revision: 2,
            motion_enabled: true,
            motion_always_on: always_on,
            sens: 0.25,
            offset_x: 0.0,
            offset_y: 0.0,
            speed_x: 0.06,
            speed_y: 0.06,
            smooth_fov_degrees: 20,
            no_deadzone: false,
            trigger_button: "right".into(),
            trigger_held: held,
            auto_flick: false,
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
            age_s: 0.0,
        }
    }

    #[test]
    fn ticker_rejects_a_previous_modes_sample_until_a_current_frame_arrives() {
        let profile = abcurves::Profile::prepare(&crate::config::MotionRendererSettings {
            enabled: true,
            reports: (0..256)
                .map(|i| [(i % 9) as i16 - 4, (i % 5) as i16 - 2])
                .collect(),
            ..crate::config::MotionRendererSettings::default()
        })
        .unwrap();
        for profile in [None, Some(profile)] {
            let (tx, rx) = std::sync::mpsc::channel();
            let ticker = MovementTicker::new(
                0.25,
                Arc::new(move || {
                    let mut settings = runtime(false, true);
                    settings.renderer_profile = profile.clone();
                    Some(settings)
                }),
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
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

    fn params() -> MotionParams {
        MotionParams {
            offsets: (0.0, 0.0),
            speed: (0.1, 0.1),
            smooth_fov_degrees: 20,
            no_deadzone: false,
        }
    }

    #[test]
    fn each_frame_is_acted_on_once() {
        let mut controller = MovementController::new(0.25);
        let mut frame = sample(2);
        frame.target = Some((132.0, 32.0)); // 100 px right of a 64 px crop's center
        let first = controller.delta(&frame, params());
        assert_eq!(first, (40, 0), "100 px × 0.1 × 4 counts/px");
        assert_eq!(
            controller.delta(&frame, params()),
            (0, 0),
            "same frame again"
        );
        frame.frame_id += 1;
        assert_eq!(controller.delta(&frame, params()), (40, 0));
        frame.target = None;
        assert_eq!(controller.delta(&frame, params()), (0, 0));
    }

    #[test]
    fn no_deadzone_setting_controls_small_target_motion() {
        let mut controller = MovementController::new(0.1);
        let mut frame = sample(2);
        frame.target = Some((32.0, 32.0));
        let mut normal = params();
        normal.offsets = (0.5, 0.0);
        normal.speed = (0.1, 0.1);
        assert_eq!(controller.delta(&frame, normal), (0, 0));

        controller.reset();
        frame.frame_id += 1;
        normal.no_deadzone = true;
        assert_eq!(controller.delta(&frame, normal), (1, 0));
    }

    #[test]
    fn inactive_motion_emits_no_delta_and_resets_fractional_carry() {
        let held = Arc::new(AtomicBool::new(true));
        let settings_held = Arc::clone(&held);
        let (tx, rx) = std::sync::mpsc::channel();
        let ticker = MovementTicker::new(
            10.0,
            Arc::new(move || {
                let mut settings = runtime(false, settings_held.load(Ordering::Acquire));
                settings.sens = 10.0;
                settings.speed_x = 0.005;
                settings.speed_y = 0.01;
                Some(settings)
            }),
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
        let receive_frame = |expected_frame| loop {
            let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if event.frame_id == expected_frame {
                break event;
            }
        };

        let mut frame = sample(2);
        ticker.update(frame.clone());
        let first = receive_frame(frame.frame_id);
        assert!(first.target_valid);

        held.store(false, Ordering::Release);
        frame.frame_id += 1;
        ticker.update(frame.clone());
        let released = receive_frame(frame.frame_id);
        assert!(
            released.target_valid,
            "target validity remains available to Recoil"
        );
        assert_eq!((released.dx, released.dy), (0, 0));

        held.store(true, Ordering::Release);
        frame.frame_id += 1;
        ticker.update(frame.clone());
        let pressed = receive_frame(frame.frame_id);
        assert_eq!((pressed.dx, pressed.dy), (0, 0));
        assert!(ticker.close(Duration::from_secs(1)));
    }

    #[test]
    fn smoothing_slows_the_aim_inside_the_smooth_fov_only() {
        let mut controller = MovementController::new(0.25);
        let mut frame = sample(2);
        frame.smoothing_alpha = (0.25, 0.5); // strengths 4 and 2
                                             // 20° Smooth FOV = 17.75 px radius; 10 px away is inside.
        frame.target = Some((42.0, 42.0));
        assert_eq!(controller.delta(&frame, params()), (2, 3));
        frame.frame_id += 1;
        frame.target = Some((132.0, 32.0));
        assert_eq!(
            controller.delta(&frame, params()),
            (40, 0),
            "outside: full speed"
        );
        frame.frame_id += 1;
        frame.target = Some((42.0, 42.0));
        let no_smooth_fov = MotionParams {
            smooth_fov_degrees: 0,
            ..params()
        };
        assert_eq!(
            controller.delta(&frame, no_smooth_fov),
            (4, 4),
            "a 0° Smooth FOV never smooths"
        );
    }

    /// Closed loop with capture/command latency and a px/count the default
    /// conversion gets wrong by `ratio`: the default speed reaches a still
    /// target 60 px away without overshooting more than a few px, and keeps
    /// it within 1.5 px.
    fn settle(latency_s: f64, ratio: f64, speed: f64) -> (f64, f64, f64) {
        const FPS: f64 = 240.0;
        const TICK: f64 = 0.004;
        let mut controller = MovementController::new(0.25);
        let px_per_count = ratio / counts_per_px(0.25);
        let (mut aim, mut applied, mut history) = (0.0_f64, Vec::new(), Vec::new());
        let (mut t, mut next_frame, mut frame_id) = (0.0, 0.0, 0);
        let (mut overshoot, mut tail_error, mut reached) = (0.0_f64, 0.0_f64, None);
        let target = 60.0;
        while t < 2.0 {
            applied.retain(|&(at, px): &(f64, f64)| {
                if at <= t {
                    aim += px;
                    false
                } else {
                    true
                }
            });
            history.push((t, aim));
            if t >= next_frame {
                frame_id += 1;
                next_frame += 1.0 / FPS;
                let seen = history
                    .iter()
                    .rev()
                    .find(|(at, _)| *at <= t - latency_s * 0.6)
                    .map_or(0.0, |(_, a)| *a);
                let mut frame = sample(2);
                frame.frame_id = frame_id;
                frame.target = Some(((target - seen).round() + 32.0, 32.0));
                let (dx, _) = controller.delta(
                    &frame,
                    MotionParams {
                        speed: (speed, speed),
                        ..params()
                    },
                );
                if dx != 0 {
                    applied.push((t + latency_s * 0.4, dx as f64 * px_per_count));
                }
            }
            let error = target - aim;
            overshoot = overshoot.max(-error);
            if reached.is_none() && error.abs() <= 3.0 {
                reached = Some(t);
            }
            if t > 1.0 {
                tail_error = tail_error.max(error.abs());
            }
            t += TICK;
        }
        (reached.unwrap_or(f64::INFINITY), overshoot, tail_error)
    }

    #[test]
    fn the_default_speed_is_stable_across_latency_and_a_wrong_px_per_count() {
        for latency in [0.016, 0.03, 0.05] {
            for ratio in [0.5, 1.0, 2.0] {
                let (reached, overshoot, tail) = settle(latency, ratio, DEFAULT_SPEED);
                assert!(reached < 0.8, "{latency} {ratio}: reached at {reached}");
                assert!(tail <= 1.5, "{latency} {ratio}: still {tail} px off");
                let limit = if latency * ratio >= 0.05 { 40.0 } else { 12.0 };
                assert!(
                    overshoot <= limit,
                    "{latency} {ratio}: overshoot {overshoot}"
                );
            }
        }
        // Faster than the default is faster, and at short latency still fine.
        let (fast, overshoot, _) = settle(0.016, 1.0, 0.12);
        assert!(fast < settle(0.016, 1.0, DEFAULT_SPEED).0 && overshoot <= 8.0);
    }

    #[test]
    #[ignore]
    fn normal_report() {
        for speed in [0.03, DEFAULT_SPEED, 0.06, 0.1] {
            for latency in [0.016, 0.03, 0.05, 0.08] {
                let row: Vec<String> = [0.5, 1.0, 2.0]
                    .iter()
                    .map(|&ratio| {
                        let (reached, overshoot, tail) = settle(latency, ratio, speed);
                        format!(
                            "×{ratio}: {reached:.2} s, overshoot {overshoot:.0}, tail {tail:.1}"
                        )
                    })
                    .collect();
                println!(
                    "speed {speed} latency {:.0} ms | {}",
                    latency * 1000.0,
                    row.join(" | ")
                );
            }
        }
    }
}
