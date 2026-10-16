use crate::config::RecoilSettings;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BEZIER_STRENGTH: f64 = 0.35;
/// Stop moving inside this error radius (crop pixels)...
const STOP_RADIUS_PX: f64 = 2.0;
/// ...and resume only once the error grows past this one (hysteresis).
const RESUME_RADIUS_PX: f64 = 4.0;
/// Smallest step outside the dead zone, in device counts per tick. It is a
/// vector length, so with the residual accumulator a small diagonal error
/// is walked as a diagonal instead of a forced ±1 staircase on one axis.
const MIN_STEP_COUNTS: f64 = 0.4;
/// A target this much farther than the stroke's start distance starts a new stroke.
const RESTROKE_GROWTH: f64 = 1.5;
/// Time-correlated (Ornstein-Uhlenbeck) drift: lateral offset as a fraction
/// of the step, and a speed scale. Both relax back to 0.
const LATERAL_SIGMA: f64 = 0.08;
const LATERAL_TAU_S: f64 = 0.08;
const SPEED_SIGMA: f64 = 0.06;
const SPEED_TAU_S: f64 = 0.15;
const DEFAULT_TICK_S: f64 = 0.004;

#[derive(Clone, Debug)]
pub struct MovementConfig {
    pub snap_radius: f64,
    pub near_radius: f64,
    pub alpha: f64,
    pub snap_boost: f64,
    pub max_step: f64,
    pub min_speed_multiplier: f64,
    pub max_speed_multiplier: f64,
    pub prediction_interval: f64,
    pub x_speed: f64,
    pub y_speed: f64,
    pub sens: f64,
    pub dpi: f64,
    pub clip_speed: f64,
}

impl Default for MovementConfig {
    fn default() -> Self {
        Self {
            snap_radius: 150.0,
            near_radius: 50.0,
            alpha: 1.5,
            snap_boost: 0.3,
            max_step: 50.0,
            min_speed_multiplier: 0.01,
            max_speed_multiplier: 10.0,
            prediction_interval: 0.016,
            x_speed: 3.0,
            y_speed: 3.0,
            sens: 0.235,
            dpi: 800.0,
            clip_speed: 1000.0,
        }
    }
}

pub fn image_to_mouse_scale(dx: f64, dy: f64, sens: f64, dpi: f64) -> (f64, f64) {
    let factor = 360.0 * 2.54 * sens.max(0.01) / (54.54 * dpi);
    (dx * factor, dy * factor)
}

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

pub fn ncaf_factor(
    distance: f64,
    snap_radius: f64,
    near_radius: f64,
    alpha: f64,
    snap_boost: f64,
) -> f64 {
    let (snap, near) = if snap_radius < near_radius {
        (near_radius, snap_radius)
    } else {
        (snap_radius, near_radius)
    };
    if distance > snap {
        return 1.0;
    }
    if distance > near {
        let gap = snap - near;
        if gap < 1e-6 {
            return snap_boost;
        }
        return 1.0 + ((snap - distance) / gap) * (snap_boost - 1.0);
    }
    if near > 1e-6 {
        snap_boost * (distance / near).powf(alpha.max(0.0))
    } else {
        snap_boost
    }
}

/// Cubic Bézier speed response applied to the step length (not per axis, so it
/// never bends the direction). `(y1, y2)` are chosen once per stroke.
fn bezier_response(value: f64, scale: f64, strength: f64, (y1, y2): (f64, f64)) -> f64 {
    if value == 0.0 || scale <= 0.0 || strength <= 0.0 {
        return value;
    }
    let sign = value.signum();
    let linear = (value.abs() / scale).min(1.0);
    let (x1, x2): (f64, f64) = (0.25, 0.75);
    let (mut low, mut high): (f64, f64) = (0.0, 1.0);
    for _ in 0..16 {
        let t = (low + high) * 0.5;
        let u = 1.0 - t;
        let x = 3.0 * u * u * t * x1 + 3.0 * u * t * t * x2 + t.powi(3);
        if x < linear {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    let u = 1.0 - t;
    let curved = 3.0 * u * u * t * y1 + 3.0 * u * t * t * y2 + t.powi(3);
    sign * (linear * (1.0 - strength) + curved * strength) * scale
}

fn smoothstep(value: f64) -> f64 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Shape of one movement stroke (from acquiring or re-acquiring a target until
/// it settles), drawn once per stroke so strokes differ while each stays smooth.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Stroke {
    started_at: f64,
    start_distance: f64,
    /// Initial heading bend (radians); fades to 0 as the stroke closes in.
    curve_rad: f64,
    /// Time to reach full speed.
    ramp_s: f64,
    /// Scales the NCAF near-zone exponent: >1 brakes harder near the target.
    decel: f64,
    bezier: (f64, f64),
    min_step: f64,
}

impl Stroke {
    fn random(rng: &mut StdRng, start_distance: f64, now: f64) -> Self {
        let bend = rng.random_range(0.04..=0.26);
        Self {
            started_at: now,
            start_distance,
            curve_rad: if rng.random::<bool>() { bend } else { -bend },
            ramp_s: rng.random_range(0.035..=0.11),
            decel: rng.random_range(0.8..=1.25),
            bezier: (rng.random_range(0.06..=0.2), rng.random_range(0.8..=0.94)),
            min_step: MIN_STEP_COUNTS * rng.random_range(0.75..=1.25),
        }
    }
}

/// Mean-reverting noise: changes smoothly between ticks and decays toward 0.
#[derive(Clone, Copy, Debug, Default)]
struct Drift(f64);

impl Drift {
    fn advance(&mut self, rng: &mut StdRng, dt: f64, sigma: f64, tau: f64) -> f64 {
        let decay = (-dt.max(0.0) / tau).exp();
        self.0 = self.0 * decay + sigma * (1.0 - decay * decay).sqrt() * gaussian(rng);
        self.0
    }
}

fn gaussian(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.random_range(f64::EPSILON..1.0);
    let u2: f64 = rng.random();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

pub struct MovementController {
    pub config: MovementConfig,
    last_error: Option<(f64, f64)>,
    last_seconds: Option<f64>,
    velocity: (f64, f64),
    residual: (f64, f64),
    last_step: (f64, f64),
    settled: bool,
    stroke: Option<Stroke>,
    lateral: Drift,
    speed: Drift,
    rng: StdRng,
}

impl MovementController {
    pub fn new(sens: f64, dpi: f64) -> Self {
        Self::with_config(MovementConfig {
            sens,
            dpi,
            ..MovementConfig::default()
        })
    }
    pub fn with_config(config: MovementConfig) -> Self {
        Self::with_rng(config, StdRng::from_os_rng())
    }
    fn with_rng(config: MovementConfig, rng: StdRng) -> Self {
        Self {
            config,
            last_error: None,
            last_seconds: None,
            velocity: (0.0, 0.0),
            residual: (0.0, 0.0),
            last_step: (0.0, 0.0),
            settled: false,
            stroke: None,
            lateral: Drift::default(),
            speed: Drift::default(),
            rng,
        }
    }
    pub fn reset(&mut self) {
        self.last_error = None;
        self.last_seconds = None;
        self.velocity = (0.0, 0.0);
        self.settle();
        self.settled = false;
    }
    /// Inside the dead zone: drop the stroke, the fractional residual and the
    /// drift so nothing creeps out after the target is reached.
    fn settle(&mut self) {
        self.settled = true;
        self.stroke = None;
        self.residual = (0.0, 0.0);
        self.last_step = (0.0, 0.0);
        self.lateral = Drift::default();
        self.speed = Drift::default();
    }
    pub fn set_calibration(&mut self, sens: f64, dpi: f64) -> Result<(), &'static str> {
        if !(0.01..=10.0).contains(&sens) || !(100.0..=100_000.0).contains(&dpi) {
            return Err("sens must be 0.01..10 and DPI must be 100..100000");
        }
        self.config.sens = sens;
        self.config.dpi = dpi;
        self.reset();
        Ok(())
    }
    pub fn delta(
        &mut self,
        target: Option<(f64, f64)>,
        crop_origin: (u32, u32),
        processed_size: (u32, u32),
        offsets: (f64, f64),
        now: f64,
    ) -> (i64, i64) {
        let Some((x, y)) = target else {
            self.reset();
            return (0, 0);
        };
        let error = crop_target_error((x, y), crop_origin, processed_size, offsets);
        self.step(error.0, error.1, now)
    }
    pub fn step(&mut self, dx: f64, dy: f64, now: f64) -> (i64, i64) {
        let c = &self.config;
        let (mut pred_x, mut pred_y) = (0.0, 0.0);
        if let (Some(previous), Some(previous_time)) = (self.last_error, self.last_seconds) {
            let elapsed = now - previous_time;
            if elapsed > 0.0 {
                pred_x = self.velocity.0 * c.prediction_interval;
                pred_y = self.velocity.1 * c.prediction_interval;
                let alpha = 0.3;
                self.velocity.0 =
                    alpha * ((dx - previous.0) / elapsed) + (1.0 - alpha) * self.velocity.0;
                self.velocity.1 =
                    alpha * ((dy - previous.1) / elapsed) + (1.0 - alpha) * self.velocity.1;
            }
        }
        let dt = self
            .last_seconds
            .map(|previous| now - previous)
            .filter(|elapsed| *elapsed > 0.0)
            .unwrap_or(DEFAULT_TICK_S);
        self.last_error = Some((dx, dy));
        self.last_seconds = Some(now);
        let (px, py) = (dx + pred_x, dy + pred_y);
        let distance = px.hypot(py);
        let threshold = if self.settled {
            RESUME_RADIUS_PX
        } else {
            STOP_RADIUS_PX
        };
        if !distance.is_finite() || distance < threshold {
            self.settle();
            return (0, 0);
        }
        self.settled = false;
        let stroke = match self.stroke {
            Some(stroke) if distance <= stroke.start_distance * RESTROKE_GROWTH => stroke,
            _ => {
                let stroke = Stroke::random(&mut self.rng, distance, now);
                self.stroke = Some(stroke);
                stroke
            }
        };

        let c = &self.config;
        let (mut mx, mut my) = image_to_mouse_scale(px, py, c.sens, c.dpi);
        mx *= c.x_speed;
        my *= c.y_speed;
        let factor = ncaf_factor(
            distance,
            c.snap_radius,
            c.near_radius,
            c.alpha * stroke.decel,
            c.snap_boost,
        )
        .clamp(c.min_speed_multiplier, c.max_speed_multiplier);
        mx *= factor;
        my *= factor;
        let magnitude = mx.hypot(my);
        if magnitude <= 1e-12 {
            return (0, 0);
        }
        let (ux, uy) = (mx / magnitude, my / magnitude);

        // Variation (curve, drift) fades out approaching the target, so the
        // final approach stays precise and does not jitter around it.
        let spread = smoothstep((distance - RESUME_RADIUS_PX) / (c.near_radius - RESUME_RADIUS_PX));
        let ramp = 0.2 + 0.8 * smoothstep((now - stroke.started_at) / stroke.ramp_s);
        let speed_noise = 1.0
            + spread
                * self
                    .speed
                    .advance(&mut self.rng, dt, SPEED_SIGMA, SPEED_TAU_S);
        let lateral = spread
            * self
                .lateral
                .advance(&mut self.rng, dt, LATERAL_SIGMA, LATERAL_TAU_S);

        let mut step = magnitude;
        if c.max_step > 0.0 {
            step = step.min(c.max_step);
            step = bezier_response(step, c.max_step, BEZIER_STRENGTH, stroke.bezier);
        }
        step = (step * ramp * speed_noise).max(stroke.min_step);

        // Bend the heading by the stroke's curve, fading as the gap closes, then
        // add the correlated lateral drift along the perpendicular.
        let progress = (1.0 - distance / stroke.start_distance).clamp(0.0, 1.0);
        let (sin, cos) = (stroke.curve_rad * (1.0 - progress) * spread).sin_cos();
        let (hx, hy) = (ux * cos - uy * sin, ux * sin + uy * cos);
        let clip = c.clip_speed.abs();
        let vx = (step * (hx - hy * lateral)).clamp(-clip, clip);
        let vy = (step * (hy + hx * lateral)).clamp(-clip, clip);

        // When an axis reverses direction its leftover fraction belongs to the
        // old direction; drop it. (A negative residual after rounding up is
        // normal and must be kept, or the average step would drift upward.)
        if vx * self.last_step.0 < 0.0 {
            self.residual.0 = 0.0;
        }
        if vy * self.last_step.1 < 0.0 {
            self.residual.1 = 0.0;
        }
        self.last_step = (vx, vy);
        let (sum_x, sum_y) = (vx + self.residual.0, vy + self.residual.1);
        let (out_x, out_y) = (round_ties_away(sum_x), round_ties_away(sum_y));
        self.residual = (sum_x - out_x as f64, sum_y - out_y as f64);
        (out_x, out_y)
    }
}

fn round_ties_away(value: f64) -> i64 {
    if value.is_finite() {
        value.round() as i64
    } else {
        0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MovementEvent {
    pub event: String,
    pub frame_id: u64,
    pub target_valid: bool,
    pub dx: i64,
    pub dy: i64,
    pub trigger_button: String,
    pub timestamp_ms: u64,
    pub recoil: RecoilSettings,
}

#[derive(Clone, Debug)]
pub struct MovementRuntimeSettings {
    pub sens: f64,
    pub dpi: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub trigger_button: String,
    pub recoil: RecoilSettings,
}

#[derive(Clone, Debug)]
pub struct MovementTargetSample {
    pub frame_id: u64,
    pub target: Option<(f64, f64)>,
    pub crop_origin: (u32, u32),
    pub processed_size: (u32, u32),
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

impl MovementTicker {
    pub fn new(
        initial_sens: f64,
        initial_dpi: f64,
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
                let mut controller = MovementController::new(initial_sens, initial_dpi);
                let mut calibration = (initial_sens, initial_dpi);
                let mut sent_invalid = false;
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
                        if sample.target.is_none() {
                            controller.reset();
                            if !sent_invalid {
                                on_event(MovementEvent {
                                    event: "movement_target".into(),
                                    frame_id: sample.frame_id,
                                    target_valid: false,
                                    dx: 0,
                                    dy: 0,
                                    trigger_button: runtime.trigger_button,
                                    timestamp_ms: unix_ms(),
                                    recoil: runtime.recoil,
                                });
                                sent_invalid = true;
                            }
                            thread::sleep(Duration::from_millis(4));
                            continue;
                        }
                        if calibration != (runtime.sens, runtime.dpi) {
                            let _ = controller.set_calibration(runtime.sens, runtime.dpi);
                            calibration = (runtime.sens, runtime.dpi);
                        }
                        let now = monotonic_seconds();
                        let (dx, dy) = controller.delta(
                            sample.target,
                            sample.crop_origin,
                            sample.processed_size,
                            (runtime.offset_x, runtime.offset_y),
                            now,
                        );
                        on_event(MovementEvent {
                            event: "movement_target".into(),
                            frame_id: sample.frame_id,
                            target_valid: sample.target.is_some(),
                            dx,
                            dy,
                            trigger_button: runtime.trigger_button,
                            timestamp_ms: unix_ms(),
                            recoil: runtime.recoil,
                        });
                        sent_invalid = false;
                    } else if !sent_invalid {
                        controller.reset();
                        if let Some(runtime) = settings() {
                            on_event(MovementEvent {
                                event: "movement_target".into(),
                                frame_id: sample.frame_id,
                                target_valid: false,
                                dx: 0,
                                dy: 0,
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

    pub fn update(&self, sample: MovementTargetSample) {
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
        if let Some(previous) =
            previous.filter(|previous| previous.target_valid && !is_stale(previous))
        {
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

    fn event(
        frame_id: u64,
        dx: i64,
        dy: i64,
        trigger_button: &str,
        timestamp_ms: u64,
    ) -> MovementEvent {
        MovementEvent {
            event: "movement_target".into(),
            frame_id,
            target_valid: true,
            dx,
            dy,
            trigger_button: trigger_button.into(),
            timestamp_ms,
            recoil: RecoilSettings::default(),
        }
    }

    #[test]
    fn sensitivity_dpi_formula_matches_configured_conversion() {
        let factor = 360.0 * 2.54 * 0.235 / (54.54 * 800.0);
        let (x, y) = image_to_mouse_scale(1.0, -1.0, 0.235, 800.0);
        assert!((x - factor).abs() < 1e-12);
        assert!((y + factor).abs() < 1e-12);
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

    fn seeded(seed: u64) -> MovementController {
        MovementController::with_rng(MovementConfig::default(), StdRng::seed_from_u64(seed))
    }

    /// Feeds a constant crop error every 4 ms and returns each tick's output.
    fn run(
        controller: &mut MovementController,
        error: (f64, f64),
        ticks: usize,
    ) -> Vec<(i64, i64)> {
        (0..ticks)
            .map(|tick| controller.step(error.0, error.1, tick as f64 * DEFAULT_TICK_S))
            .collect()
    }

    #[test]
    fn dead_zone_stops_inside_and_resumes_only_past_the_hysteresis_radius() {
        let mut controller = seeded(1);
        // Seconds apart, so velocity prediction stays negligible.
        controller.step(3.0, 0.0, 0.0);
        assert!(!controller.settled, "3 px is outside the 2 px stop radius");
        assert_eq!(controller.step(1.0, 0.0, 1.0), (0, 0));
        assert!(controller.settled);
        assert_eq!(controller.residual, (0.0, 0.0));
        assert_eq!(
            controller.step(3.0, 0.0, 2.0),
            (0, 0),
            "below the 4 px resume radius"
        );
        assert!(controller.settled);
        controller.step(5.0, 0.0, 3.0);
        assert!(!controller.settled);
    }

    #[test]
    fn small_errors_accumulate_a_diagonal_instead_of_a_forced_axis_staircase() {
        let mut controller = seeded(2);
        let outputs = run(&mut controller, (4.4, 2.2), 400);
        // Not one count every tick: the sub-count step accumulates in the residual.
        assert!(outputs.contains(&(0, 0)));
        assert!(outputs.iter().all(|(x, y)| x.abs() <= 1 && y.abs() <= 1));
        // Both axes move in the error's 2:1 proportion (the old code moved X only).
        let (sum_x, sum_y) = outputs
            .iter()
            .fold((0, 0), |(sx, sy), (x, y)| (sx + x, sy + y));
        assert!(sum_y > 0, "{sum_x} {sum_y}");
        let ratio = sum_x as f64 / sum_y as f64;
        assert!((1.8..=2.2).contains(&ratio), "{sum_x} {sum_y}");
    }

    #[test]
    fn stroke_shape_is_drawn_once_per_stroke_and_differs_between_strokes() {
        let mut controller = seeded(3);
        controller.step(300.0, 0.0, 0.0);
        let first = controller.stroke.expect("stroke");
        for tick in 1..20 {
            controller.step(300.0 - tick as f64 * 5.0, 0.0, tick as f64 * DEFAULT_TICK_S);
            assert_eq!(controller.stroke, Some(first), "kept for the whole stroke");
        }
        controller.reset();
        controller.step(300.0, 0.0, 1.0);
        assert_ne!(
            controller.stroke,
            Some(first),
            "a new stroke gets a new shape"
        );

        // Same error sequence, different seeds: different paths; same seed: identical.
        let a = run(&mut seeded(10), (300.0, 0.0), 60);
        let b = run(&mut seeded(11), (300.0, 0.0), 60);
        let a_again = run(&mut seeded(10), (300.0, 0.0), 60);
        assert_ne!(a, b);
        assert_eq!(a, a_again);
    }

    #[test]
    fn a_far_target_curves_but_variation_fades_near_the_target() {
        // Far away, the curve and lateral drift add a perpendicular (Y) component.
        let curved = (0..8).any(|seed| {
            run(&mut seeded(seed), (300.0, 0.0), 40)
                .iter()
                .any(|(_, y)| *y != 0)
        });
        assert!(curved);
        // Just outside the dead zone the step goes straight at the target.
        for seed in 0..8 {
            assert!(run(&mut seeded(seed), (4.5, 0.0), 300)
                .iter()
                .all(|(x, y)| *y == 0 && *x >= 0));
        }
    }

    #[test]
    fn each_stroke_accelerates_from_rest() {
        let outputs = run(&mut seeded(4), (300.0, 0.0), 60);
        let first = outputs[0].0.abs();
        let settled_speed = outputs[50].0.abs();
        assert!(first <= 1, "{outputs:?}");
        assert!(settled_speed >= 3, "{outputs:?}");
    }

    #[test]
    fn drift_is_time_correlated_and_mean_reverting() {
        let mut rng = StdRng::seed_from_u64(5);
        let mut drift = Drift::default();
        let samples: Vec<f64> = (0..5000)
            .map(|_| drift.advance(&mut rng, DEFAULT_TICK_S, 1.0, 0.08))
            .collect();
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!(mean.abs() < 0.2, "{mean}");
        // Neighbouring ticks are close (smooth), unlike independent noise.
        let tick_change = samples
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .sum::<f64>()
            / (samples.len() - 1) as f64;
        assert!(tick_change < 0.4, "{tick_change}");
        // Without new noise it relaxes back to 0.
        let mut quiet = Drift(1.0);
        quiet.advance(&mut rng, 1.0, 0.0, 0.08);
        assert!(quiet.0.abs() < 1e-3);
    }
}
