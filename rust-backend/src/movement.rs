use crate::config::RecoilSettings;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
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
    /// `ramp`: only a fresh acquisition accelerates from rest; strokes that
    /// resume while tracking start at full speed.
    fn random(rng: &mut StdRng, start_distance: f64, now: f64, ramp: bool) -> Self {
        let bend = rng.random_range(0.04..=0.26);
        Self {
            started_at: now,
            start_distance,
            curve_rad: if rng.random::<bool>() { bend } else { -bend },
            ramp_s: if ramp {
                rng.random_range(0.035..=0.11)
            } else {
                0.0
            },
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

/// Measured link between device counts and image motion, found online while
/// tracking (the Sens/DPI formula is a gain setting, not a physical mapping).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionCalibration {
    /// Image pixels the target shifts per applied device count.
    pub px_per_count: f64,
    /// Time from a move being applied to its effect reaching a received frame.
    pub latency_s: f64,
}

/// Candidate latencies 0, 4, ..., 100 ms tried side by side by the calibrator.
const LATENCY_STEPS: usize = 26;
const LATENCY_STEP_S: f64 = 0.004;
const MOVE_LOG_S: f64 = 0.4;
/// Exponential forgetting per frame, so the estimate follows a sens change.
const CALIBRATION_FORGET: f64 = 0.999;
const CALIBRATION_MIN_WEIGHT: f64 = 60.0;
/// Calibration switches on when the standard error falls below this fraction
/// of the estimate, and stays on (following the latest estimate) until it
/// rises above the second one.
const CALIBRATION_ENTER_REL_ERROR: f64 = 0.15;
const CALIBRATION_KEEP_REL_ERROR: f64 = 0.3;
/// Rates for calibration are taken over this span rather than between two
/// frames: whole-pixel detections make single-frame rates far too noisy.
const CALIBRATION_SPAN_S: f64 = 0.016;
const PX_PER_COUNT_RANGE: (f64, f64) = (0.01, 50.0);
/// Frame-to-frame target jump treated as a target switch (crop pixels).
const TARGET_JUMP_PX: f64 = 48.0;
const VELOCITY_ALPHA: f64 = 0.3;
/// A target moving at least this fast never "settles": no stop-and-chase.
const MOVING_TARGET_PX_S: f64 = 40.0;
/// Near-zone NCAF boost for moving targets: full at this speed...
const NEAR_BOOST_FULL_PX_S: f64 = 600.0;
/// ...lifting the factor at most this fraction of the way to 1.
const NEAR_BOOST_MAX: f64 = 0.6;
const FEEDFORWARD_GAIN: f64 = 0.9;
/// Probe for calibration: ±1 count per axis, random and independent of the
/// detections, sent on this share of ticks while the trigger is held (less
/// once calibrated, enough to follow changes). The controller's own moves
/// react to detection noise, which would bias a closed-loop estimate; the
/// probe does not.
const PROBE_PROBABILITY: f64 = 0.7;
const PROBE_PROBABILITY_CALIBRATED: f64 = 0.2;

type LatencyRates = [(f64, f64); LATENCY_STEPS];

#[derive(Clone, Copy, Debug)]
struct AppliedMove {
    time: f64,
    total: (f64, f64),
    probe: (f64, f64),
}

/// Per candidate latency, relates the change in apparent target rate (`y`) to
/// the change in applied move rate (`x`, all moves) and in the probe's move
/// rate (`z`). Differencing twice cancels a target moving at constant speed.
/// The latency is the one where the probe best explains `y`; pixels per count
/// is the instrumental-variable slope `Σzy / Σzx`: unbiased because the probe
/// is independent of detection noise, and precise because it uses every move.
#[derive(Clone, Debug)]
struct MotionCalibrator {
    szz: [f64; LATENCY_STEPS],
    szy: [f64; LATENCY_STEPS],
    szx: [f64; LATENCY_STEPS],
    sxx: [f64; LATENCY_STEPS],
    sxy: [f64; LATENCY_STEPS],
    syy: f64,
    weight: f64,
}

impl Default for MotionCalibrator {
    fn default() -> Self {
        Self {
            szz: [0.0; LATENCY_STEPS],
            szy: [0.0; LATENCY_STEPS],
            szx: [0.0; LATENCY_STEPS],
            sxx: [0.0; LATENCY_STEPS],
            sxy: [0.0; LATENCY_STEPS],
            syy: 0.0,
            weight: 0.0,
        }
    }
}

impl MotionCalibrator {
    /// One sample: `y` per axis, `xs`/`zs` per latency and axis. Samples taken
    /// every frame over overlapping spans count as `w` (< 1) of a sample each.
    fn add(&mut self, y: (f64, f64), xs: &LatencyRates, zs: &LatencyRates, w: f64) {
        const F: f64 = CALIBRATION_FORGET;
        for j in 0..LATENCY_STEPS {
            let (x, z) = (xs[j], zs[j]);
            self.szz[j] = self.szz[j] * F + w * (z.0 * z.0 + z.1 * z.1);
            self.szy[j] = self.szy[j] * F + w * (z.0 * y.0 + z.1 * y.1);
            self.szx[j] = self.szx[j] * F + w * (z.0 * x.0 + z.1 * x.1);
            self.sxx[j] = self.sxx[j] * F + w * (x.0 * x.0 + x.1 * x.1);
            self.sxy[j] = self.sxy[j] * F + w * (x.0 * y.0 + x.1 * y.1);
        }
        self.syy = self.syy * F + w * (y.0 * y.0 + y.1 * y.1);
        self.weight = self.weight * F + 2.0 * w;
    }

    /// Best estimate and its standard error relative to the estimate.
    fn estimate(&self) -> Option<(MotionCalibration, f64)> {
        if self.weight < CALIBRATION_MIN_WEIGHT {
            return None;
        }
        let best = (0..LATENCY_STEPS)
            .filter(|j| self.szz[*j] > 1e-9)
            .min_by(|a, b| {
                let unexplained = |j: usize| self.syy - self.szy[j] * self.szy[j] / self.szz[j];
                unexplained(*a).total_cmp(&unexplained(*b))
            })?;
        if self.szx[best] <= 1e-9 {
            return None;
        }
        let k = self.szy[best] / self.szx[best];
        if !(PX_PER_COUNT_RANGE.0..=PX_PER_COUNT_RANGE.1).contains(&k) {
            return None;
        }
        let sse = self.syy - 2.0 * k * self.sxy[best] + k * k * self.sxx[best];
        let standard_error =
            (sse.max(0.0) / (self.weight - 1.0) * self.szz[best]).sqrt() / self.szx[best];
        Some((
            MotionCalibration {
                px_per_count: k,
                latency_s: best as f64 * LATENCY_STEP_S,
            },
            standard_error / k,
        ))
    }
}

/// One detection result as the controller sees it: the error used for
/// control (smoothed target) and the raw error used for estimation.
#[derive(Clone, Copy, Debug)]
pub struct FrameError {
    pub frame_id: u64,
    pub received_s: f64,
    pub control: (f64, f64),
    pub raw: (f64, f64),
}

pub struct MovementController {
    pub config: MovementConfig,
    last_frame_id: Option<u64>,
    /// Frames of the current track: (receive time, raw error), newest last.
    track: VecDeque<(f64, (f64, f64))>,
    frame_error: (f64, f64),
    frame_received_s: f64,
    /// Target velocity in crop px/s: its own motion once calibrated, the
    /// apparent (target minus camera) motion before that.
    target_velocity: (f64, f64),
    calibrator: MotionCalibrator,
    calibration: Option<MotionCalibration>,
    /// Applied moves in device counts, newest last.
    move_log: VecDeque<AppliedMove>,
    last_tick_s: Option<f64>,
    residual: (f64, f64),
    last_step: (f64, f64),
    settled: bool,
    /// The next stroke is a fresh acquisition and gets the start-up ramp.
    acquiring: bool,
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
            last_frame_id: None,
            track: VecDeque::new(),
            frame_error: (0.0, 0.0),
            frame_received_s: 0.0,
            target_velocity: (0.0, 0.0),
            calibrator: MotionCalibrator::default(),
            calibration: None,
            move_log: VecDeque::new(),
            last_tick_s: None,
            residual: (0.0, 0.0),
            last_step: (0.0, 0.0),
            settled: false,
            acquiring: true,
            stroke: None,
            lateral: Drift::default(),
            speed: Drift::default(),
            rng,
        }
    }
    /// Target lost: forget the track. The calibration and the move log stay.
    pub fn reset(&mut self) {
        self.last_frame_id = None;
        self.track.clear();
        self.target_velocity = (0.0, 0.0);
        self.last_tick_s = None;
        self.settle();
        self.settled = false;
        self.acquiring = true;
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
        // A new in-game sensitivity changes pixels per count: measure again.
        self.calibrator = MotionCalibrator::default();
        self.calibration = None;
        self.reset();
        Ok(())
    }
    pub fn motion_calibration(&self) -> Option<MotionCalibration> {
        self.calibration
    }
    pub fn delta(
        &mut self,
        sample: &MovementTargetSample,
        offsets: (f64, f64),
        now: f64,
        trigger_held: bool,
    ) -> (i64, i64) {
        let Some(target) = sample.target else {
            self.reset();
            return (0, 0);
        };
        let raw = sample.raw_target.unwrap_or(target);
        let frame = FrameError {
            frame_id: sample.frame_id,
            received_s: sample.received_s,
            control: crop_target_error(target, sample.crop_origin, sample.processed_size, offsets),
            raw: crop_target_error(raw, sample.crop_origin, sample.processed_size, offsets),
        };
        self.track(frame, now, trigger_held)
    }
    /// Each call is a new frame received at `now`, with no move applied.
    pub fn step(&mut self, dx: f64, dy: f64, now: f64) -> (i64, i64) {
        let frame_id = self.last_frame_id.map_or(0, |id| id.wrapping_add(1));
        self.track(
            FrameError {
                frame_id,
                received_s: now,
                control: (dx, dy),
                raw: (dx, dy),
            },
            now,
            false,
        )
    }
    /// One ticker step. A frame is observed once (velocity, calibration);
    /// every tick then works from the error estimated for `now`, so moves
    /// already sent since the frame are not sent again.
    pub fn track(&mut self, frame: FrameError, now: f64, trigger_held: bool) -> (i64, i64) {
        if self.last_frame_id != Some(frame.frame_id) {
            self.observe(&frame, trigger_held);
        }
        let tick_s = self
            .last_tick_s
            .map(|previous| now - previous)
            .filter(|elapsed| *elapsed > 0.0)
            .unwrap_or(DEFAULT_TICK_S);
        self.last_tick_s = Some(now);
        let (px, py) = self.estimated_error(now);
        let target_speed = self.target_velocity.0.hypot(self.target_velocity.1);
        let moving = self.calibration.is_some() && target_speed >= MOVING_TARGET_PX_S;
        let (mut vx, mut vy) = self.shape(px, py, now, tick_s, moving, target_speed);

        // Feedforward: turn at the target's own speed, so a moving target is
        // followed without waiting for a large error to build up.
        if let Some(calibration) = self.calibration {
            let scale = FEEDFORWARD_GAIN * tick_s / calibration.px_per_count;
            vx += self.target_velocity.0 * scale;
            vy += self.target_velocity.1 * scale;
        }
        if vx == 0.0 && vy == 0.0 {
            return (0, 0);
        }
        let clip = self.config.clip_speed.abs();
        let (vx, vy) = (vx.clamp(-clip, clip), vy.clamp(-clip, clip));

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
        let (mut out_x, mut out_y) = (round_ties_away(sum_x), round_ties_away(sum_y));
        self.residual = (sum_x - out_x as f64, sum_y - out_y as f64);
        if trigger_held {
            let probe = (self.probe(), self.probe());
            out_x += probe.0;
            out_y += probe.1;
            if out_x != 0 || out_y != 0 || probe != (0, 0) {
                // The host applies at most ±32 per command.
                self.move_log.push_back(AppliedMove {
                    time: now,
                    total: (out_x.clamp(-32, 32) as f64, out_y.clamp(-32, 32) as f64),
                    probe: (probe.0 as f64, probe.1 as f64),
                });
            }
        }
        while self
            .move_log
            .front()
            .is_some_and(|applied| applied.time < now - MOVE_LOG_S)
        {
            self.move_log.pop_front();
        }
        (out_x, out_y)
    }

    fn probe(&mut self) -> i64 {
        let probability = if self.calibration.is_some() {
            PROBE_PROBABILITY_CALIBRATED
        } else {
            PROBE_PROBABILITY
        };
        if self.rng.random::<f64>() >= probability {
            0
        } else if self.rng.random::<bool>() {
            1
        } else {
            -1
        }
    }

    /// Counts applied in `(from, to]`: all moves, and the probe's share.
    fn moved_between(&self, from: f64, to: f64) -> ((f64, f64), (f64, f64)) {
        self.move_log
            .iter()
            .filter(|applied| applied.time > from && applied.time <= to)
            .fold(((0.0, 0.0), (0.0, 0.0)), |(total, probe), applied| {
                (
                    (total.0 + applied.total.0, total.1 + applied.total.1),
                    (probe.0 + applied.probe.0, probe.1 + applied.probe.1),
                )
            })
    }

    fn observe(&mut self, frame: &FrameError, trigger_held: bool) {
        let t = frame.received_s;
        if let Some(&(previous_t, previous_raw)) = self.track.back() {
            let dt = t - previous_t;
            let jump = (frame.raw.0 - previous_raw.0).hypot(frame.raw.1 - previous_raw.1);
            if jump > TARGET_JUMP_PX {
                // A different target: start over, with the acquisition ramp.
                self.track.clear();
                self.target_velocity = (0.0, 0.0);
                self.stroke = None;
                self.acquiring = true;
            } else if !(0.0005..=0.1).contains(&dt) {
                self.track.clear();
                self.target_velocity = (0.0, 0.0);
            } else {
                let rate = (
                    (frame.raw.0 - previous_raw.0) / dt,
                    (frame.raw.1 - previous_raw.1) / dt,
                );
                // The target's own velocity adds back the motion our moves caused.
                let own = match self.calibration {
                    Some(calibration) => {
                        let lag = calibration.latency_s;
                        let ((mx, my), _) = self.moved_between(previous_t - lag, t - lag);
                        let k = calibration.px_per_count;
                        (rate.0 + k * mx / dt, rate.1 + k * my / dt)
                    }
                    None => rate,
                };
                self.target_velocity = if self.track.len() >= 2 {
                    (
                        VELOCITY_ALPHA * own.0 + (1.0 - VELOCITY_ALPHA) * self.target_velocity.0,
                        VELOCITY_ALPHA * own.1 + (1.0 - VELOCITY_ALPHA) * self.target_velocity.1,
                    )
                } else {
                    own
                };
                // Without the trigger nothing is applied or probed; such samples
                // would only dilute the estimate until calibration was lost.
                if trigger_held {
                    self.calibrate(t, frame.raw, dt);
                }
            }
        }
        self.track.push_back((t, frame.raw));
        while self
            .track
            .front()
            .is_some_and(|(time, _)| *time < t - 3.0 * CALIBRATION_SPAN_S)
        {
            self.track.pop_front();
        }
        self.last_frame_id = Some(frame.frame_id);
        self.frame_error = frame.control;
        self.frame_received_s = t;
    }

    /// One calibration sample from three frames about a span apart: the change
    /// in apparent rate between two consecutive spans against the change in
    /// move rates, for every candidate latency.
    fn calibrate(&mut self, t: f64, raw: (f64, f64), frame_dt: f64) {
        let at_or_before = |limit: f64| {
            self.track
                .iter()
                .rev()
                .find(|(time, _)| *time <= limit)
                .copied()
        };
        let Some((tj, ej)) = at_or_before(t - CALIBRATION_SPAN_S) else {
            return;
        };
        let Some((ti, ei)) = at_or_before(tj - CALIBRATION_SPAN_S) else {
            return;
        };
        let (late, early) = (t - tj, tj - ti);
        if late > 2.0 * CALIBRATION_SPAN_S || early > 2.0 * CALIBRATION_SPAN_S {
            return;
        }
        let y = (
            (raw.0 - ej.0) / late - (ej.0 - ei.0) / early,
            (raw.1 - ej.1) / late - (ej.1 - ei.1) / early,
        );
        let mut xs: LatencyRates = [(0.0, 0.0); LATENCY_STEPS];
        let mut zs: LatencyRates = [(0.0, 0.0); LATENCY_STEPS];
        for j in 0..LATENCY_STEPS {
            let lag = j as f64 * LATENCY_STEP_S;
            let (early_total, early_probe) = self.moved_between(ti - lag, tj - lag);
            let (late_total, late_probe) = self.moved_between(tj - lag, t - lag);
            // Moving toward the target lowers its apparent rate: y = k·(m_early − m_late).
            xs[j] = (
                early_total.0 / early - late_total.0 / late,
                early_total.1 / early - late_total.1 / late,
            );
            zs[j] = (
                early_probe.0 / early - late_probe.0 / late,
                early_probe.1 / early - late_probe.1 / late,
            );
        }
        let weight = (frame_dt / CALIBRATION_SPAN_S).min(1.0);
        self.calibrator.add(y, &xs, &zs, weight);
        let threshold = if self.calibration.is_some() {
            CALIBRATION_KEEP_REL_ERROR
        } else {
            CALIBRATION_ENTER_REL_ERROR
        };
        match self.calibrator.estimate() {
            Some((estimate, relative_error)) if relative_error <= threshold => {
                self.calibration = Some(estimate);
            }
            Some(_) => self.calibration = None,
            None => {}
        }
    }

    /// Error expected at `now`. Calibrated: the frame shows the scene from
    /// `latency` ago, so add the target's motion since then and remove the
    /// moves applied since then. Before calibration: the frame error with a
    /// short velocity lead, as before.
    fn estimated_error(&self, now: f64) -> (f64, f64) {
        let (ex, ey) = self.frame_error;
        let (vx, vy) = self.target_velocity;
        match self.calibration {
            Some(calibration) => {
                let seen_at = self.frame_received_s - calibration.latency_s;
                let elapsed = (now - seen_at).max(0.0);
                let ((mx, my), _) = self.moved_between(seen_at, now);
                let k = calibration.px_per_count;
                (ex + vx * elapsed - k * mx, ey + vy * elapsed - k * my)
            }
            None => {
                let lead = self.config.prediction_interval;
                (ex + vx * lead, ey + vy * lead)
            }
        }
    }

    /// Error-correcting step (device counts, unrounded) for the estimated error.
    fn shape(
        &mut self,
        px: f64,
        py: f64,
        now: f64,
        tick_s: f64,
        moving: bool,
        target_speed: f64,
    ) -> (f64, f64) {
        let distance = px.hypot(py);
        let threshold = if self.settled {
            RESUME_RADIUS_PX
        } else {
            STOP_RADIUS_PX
        };
        if !distance.is_finite() || distance < threshold {
            // On a moving target only feedforward runs here; settling (and the
            // ramp of a new stroke afterwards) would turn tracking into
            // stop-and-chase.
            if !moving {
                self.settle();
            }
            return (0.0, 0.0);
        }
        self.settled = false;
        let stroke = match self.stroke {
            Some(stroke) if distance <= stroke.start_distance * RESTROKE_GROWTH => stroke,
            _ => {
                let ramp = std::mem::take(&mut self.acquiring);
                let stroke = Stroke::random(&mut self.rng, distance, now, ramp);
                self.stroke = Some(stroke);
                stroke
            }
        };

        let c = &self.config;
        let (mut mx, mut my) = image_to_mouse_scale(px, py, c.sens, c.dpi);
        mx *= c.x_speed;
        my *= c.y_speed;
        let mut factor = ncaf_factor(
            distance,
            c.snap_radius,
            c.near_radius,
            c.alpha * stroke.decel,
            c.snap_boost,
        );
        // A fast target needs a firmer near-zone response or the aim trails
        // it; lift the factor part of the way toward 1, scaled by its speed.
        if self.calibration.is_some() && factor < 1.0 {
            let boost = (target_speed / NEAR_BOOST_FULL_PX_S).min(1.0) * NEAR_BOOST_MAX;
            factor += (1.0 - factor) * boost;
        }
        let factor = factor.clamp(c.min_speed_multiplier, c.max_speed_multiplier);
        mx *= factor;
        my *= factor;
        let magnitude = mx.hypot(my);
        if magnitude <= 1e-12 {
            return (0.0, 0.0);
        }
        let (ux, uy) = (mx / magnitude, my / magnitude);

        // Variation (curve, drift) fades out approaching the target, so the
        // final approach stays precise and does not jitter around it.
        let spread = smoothstep((distance - RESUME_RADIUS_PX) / (c.near_radius - RESUME_RADIUS_PX));
        let ramp = if stroke.ramp_s > 0.0 {
            0.2 + 0.8 * smoothstep((now - stroke.started_at) / stroke.ramp_s)
        } else {
            1.0
        };
        let speed_noise = 1.0
            + spread
                * self
                    .speed
                    .advance(&mut self.rng, tick_s, SPEED_SIGMA, SPEED_TAU_S);
        let lateral = spread
            * self
                .lateral
                .advance(&mut self.rng, tick_s, LATERAL_SIGMA, LATERAL_TAU_S);

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
        (step * (hx - hy * lateral), step * (hy + hx * lateral))
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
    /// The selected physical trigger is held (from the host's device status),
    /// so moves sent now are applied and can be used for calibration.
    pub trigger_held: bool,
    pub recoil: RecoilSettings,
}

#[derive(Clone, Debug)]
pub struct MovementTargetSample {
    pub frame_id: u64,
    /// Smoothed target used for control.
    pub target: Option<(f64, f64)>,
    /// Unsmoothed target used for velocity and calibration.
    pub raw_target: Option<(f64, f64)>,
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
                            &sample,
                            (runtime.offset_x, runtime.offset_y),
                            now,
                            runtime.trigger_held,
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

    /// Closed loop against a simulated game (`World`): image pixels per count,
    /// frames that show the scene `latency_s` late, integer pixel detections
    /// with jitter, and a strafing target. `aware` reports the
    /// trigger as held (moves are known); otherwise the controller cannot tell
    /// which moves were applied and runs uncompensated. Returns the RMS true
    /// error over the last half and the final calibration.
    #[derive(Clone, Copy)]
    struct World {
        px_per_count: f64,
        latency_s: f64,
        fps: f64,
        /// Strafe amplitude in px; 0 keeps the target still.
        strafe: f64,
    }

    const GAME: World = World {
        px_per_count: 0.25,
        latency_s: 0.024,
        fps: 240.0,
        strafe: 150.0,
    };

    fn simulate(aware: bool, seed: u64) -> (f64, Option<MotionCalibration>) {
        simulate_in(GAME, aware, seed)
    }

    fn simulate_in(world: World, aware: bool, seed: u64) -> (f64, Option<MotionCalibration>) {
        const SECONDS: f64 = 10.0;
        let frame_s = 1.0 / world.fps;
        let target = |t: f64| {
            (
                60.0 + world.strafe * (std::f64::consts::TAU * 0.8 * t).sin(),
                -20.0 + world.strafe / 4.0 * (std::f64::consts::TAU * 0.3 * t).cos(),
            )
        };
        let mut controller = seeded(seed);
        let mut noise = StdRng::seed_from_u64(seed ^ 0x5eed);
        // Camera offset in px after each applied move, with the time applied.
        let mut camera = vec![(0.0_f64, (0.0_f64, 0.0_f64))];
        let camera_at = |camera: &[(f64, (f64, f64))], t: f64| {
            camera
                .iter()
                .rev()
                .find(|(time, _)| *time <= t)
                .map_or((0.0, 0.0), |(_, offset)| *offset)
        };
        let (mut squared, mut samples) = (0.0, 0);
        let mut frame: Option<FrameError> = None;
        let mut next_frame = 0_u64;
        let ticks = (SECONDS / DEFAULT_TICK_S) as usize;
        for tick in 1..=ticks {
            let now = tick as f64 * DEFAULT_TICK_S;
            // Newest frame received by now: captured at `received - LATENCY_S`.
            while (next_frame as f64) * frame_s + world.latency_s <= now {
                let captured = next_frame as f64 * frame_s;
                let (tx, ty) = target(captured);
                let (cx, cy) = camera_at(&camera, captured);
                let mut jitter = || noise.random_range(-0.7..0.7);
                let error = ((tx - cx + jitter()).trunc(), (ty - cy + jitter()).trunc());
                frame = Some(FrameError {
                    frame_id: next_frame,
                    received_s: captured + world.latency_s,
                    control: error,
                    raw: error,
                });
                next_frame += 1;
            }
            let Some(current) = frame else { continue };
            let (dx, dy) = controller.track(current, now, aware);
            let (cx, cy) = camera.last().expect("camera").1;
            camera.push((
                now,
                (
                    cx + dx.clamp(-32, 32) as f64 * world.px_per_count,
                    cy + dy.clamp(-32, 32) as f64 * world.px_per_count,
                ),
            ));
            if now >= SECONDS / 2.0 {
                let (tx, ty) = target(now);
                let (cx, cy) = camera.last().expect("camera").1;
                squared += (tx - cx).powi(2) + (ty - cy).powi(2);
                samples += 1;
            }
        }
        (
            (squared / samples as f64).sqrt(),
            controller.motion_calibration(),
        )
    }

    /// Report over many seeds and worlds (not a pass/fail test):
    /// `cargo test --release --lib -- simulation_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn simulation_report() {
        for (name, world) in [
            ("default", GAME),
            (
                "px/count 0.05",
                World {
                    px_per_count: 0.05,
                    ..GAME
                },
            ),
            (
                "px/count 2.0",
                World {
                    px_per_count: 2.0,
                    ..GAME
                },
            ),
            (
                "latency 50 ms",
                World {
                    latency_s: 0.05,
                    ..GAME
                },
            ),
            (
                "60 FPS, 40 ms",
                World {
                    fps: 60.0,
                    latency_s: 0.04,
                    ..GAME
                },
            ),
            (
                "still target",
                World {
                    strafe: 0.0,
                    ..GAME
                },
            ),
        ] {
            let mut line = String::new();
            for seed in 200..210 {
                let (aware, calibration) = simulate_in(world, true, seed);
                let (blind, _) = simulate_in(world, false, seed);
                let found = calibration.map_or("none".into(), |c| {
                    format!("{:.3}/{:.0}ms", c.px_per_count, c.latency_s * 1000.0)
                });
                line += &format!(" [{found} {aware:.0}/{blind:.0}px]");
            }
            eprintln!("{name}:{line}");
        }
    }

    #[test]
    fn calibration_recovers_pixels_per_count_and_latency_while_tracking() {
        for seed in [21, 22, 23] {
            let (_, calibration) = simulate(true, seed);
            let calibration = calibration.expect("calibrated while tracking");
            assert!(
                (calibration.px_per_count - 0.25).abs() <= 0.25 * 0.2,
                "{calibration:?}"
            );
            assert!(
                (calibration.latency_s - 0.024).abs() <= 0.0041,
                "{calibration:?}"
            );
        }
        let coarse = World {
            px_per_count: 2.0,
            ..GAME
        };
        let (_, calibration) = simulate_in(coarse, true, 24);
        let calibration = calibration.expect("calibrated");
        assert!(
            (calibration.px_per_count - 2.0).abs() <= 2.0 * 0.2,
            "{calibration:?}"
        );
    }

    #[test]
    fn compensation_and_feedforward_track_a_strafing_target_closer() {
        for seed in [31, 32, 33] {
            let (blind, blind_calibration) = simulate(false, seed);
            let (aware, _) = simulate(true, seed);
            assert!(blind_calibration.is_none());
            assert!(
                aware < blind * 0.4,
                "aware {aware:.1} px vs blind {blind:.1} px"
            );
        }
        let low_fps = World {
            fps: 60.0,
            latency_s: 0.04,
            ..GAME
        };
        let (blind, _) = simulate_in(low_fps, false, 34);
        let (aware, _) = simulate_in(low_fps, true, 34);
        assert!(
            aware < blind * 0.6,
            "aware {aware:.1} px vs blind {blind:.1} px"
        );
    }

    #[test]
    fn without_a_usable_calibration_tracking_is_no_worse_and_a_still_target_stays_steady() {
        // ±1 count moves 0.05 px here: below detection, so no calibration.
        let fine = World {
            px_per_count: 0.05,
            ..GAME
        };
        let (aware, calibration) = simulate_in(fine, true, 41);
        let (blind, _) = simulate_in(fine, false, 41);
        assert!(calibration.is_none());
        assert!(
            aware <= blind * 1.05,
            "aware {aware:.1} px vs blind {blind:.1} px"
        );
        // The probe does not visibly shake a still target.
        let still = World {
            strafe: 0.0,
            ..GAME
        };
        let (aware, _) = simulate_in(still, true, 42);
        let (blind, _) = simulate_in(still, false, 42);
        assert!(
            aware <= blind + 1.0,
            "aware {aware:.1} px vs blind {blind:.1} px"
        );
    }

    #[test]
    fn only_a_new_acquisition_ramps_up() {
        let mut controller = seeded(51);
        controller.step(30.0, 0.0, 0.0);
        assert!(controller.stroke.expect("stroke").ramp_s > 0.0);
        controller.step(1.0, 0.0, 1.0);
        assert!(controller.settled);
        controller.step(30.0, 0.0, 2.0);
        assert_eq!(
            controller.stroke.expect("stroke").ramp_s,
            0.0,
            "resumed while tracking"
        );
        controller.reset();
        controller.step(30.0, 0.0, 3.0);
        assert!(
            controller.stroke.expect("stroke").ramp_s > 0.0,
            "target re-acquired"
        );
    }

    #[test]
    fn a_moving_target_is_followed_without_settling() {
        let mut controller = seeded(52);
        controller.calibration = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.0,
        });
        // The aim is on target (1 px, inside the stop radius) but the target
        // moves right at 200 px/s: keep turning with it instead of stopping.
        let mut moved = 0;
        for tick in 0..50 {
            let now = tick as f64 * DEFAULT_TICK_S;
            let frame = FrameError {
                frame_id: tick,
                received_s: now,
                control: (1.0, 0.0),
                raw: (200.0 * now, 0.0),
            };
            let (dx, _) = controller.track(frame, now, false);
            moved += dx;
            // From the second frame on the speed is known: never a stop.
            if tick >= 1 {
                assert!(dx > 0, "tick {tick}");
            }
        }
        // Feedforward: 0.9 · 200 px/s · 0.2 s / 0.25 px per count ≈ 144 counts.
        assert!((100..=180).contains(&moved), "{moved}");
    }

    #[test]
    fn near_zone_response_rises_with_target_speed() {
        let step_at = |speed: f64| {
            let mut controller = seeded(53);
            controller.calibration = Some(MotionCalibration {
                px_per_count: 0.25,
                latency_s: 0.0,
            });
            controller.acquiring = false; // already tracking: no start-up ramp
            let (x, y) = controller.shape(100.0, 0.0, 1.0, DEFAULT_TICK_S, false, speed);
            x.hypot(y)
        };
        // Between the near and snap radii; closer in, the minimum step dominates.
        assert!(step_at(600.0) > step_at(0.0) * 1.2);
    }
}
