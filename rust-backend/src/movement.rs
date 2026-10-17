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
/// The same radii in counts, for games where one count is several pixels.
const STOP_RADIUS_COUNTS: f64 = 0.6;
const RESUME_RADIUS_COUNTS: f64 = 1.2;
/// A target this much farther than the stroke's start distance starts a new stroke.
const RESTROKE_GROWTH: f64 = 1.5;
/// Time-correlated (Ornstein-Uhlenbeck) drift: lateral offset as a fraction
/// of the step, and a speed scale. Both relax back to 0.
const LATERAL_SIGMA: f64 = 0.08;
const LATERAL_TAU_S: f64 = 0.08;
const SPEED_SIGMA: f64 = 0.06;
const SPEED_TAU_S: f64 = 0.15;
const DEFAULT_TICK_S: f64 = 0.004;

/// How fast the aim closes an error: the remaining error shrinks with this
/// time constant (before NCAF and the speed curve).
const CORRECTION_TAU_S: f64 = 0.035;
/// Correction speed cap and floor, in image px/s.
const MAX_CORRECTION_PX_S: f64 = 4000.0;
const MIN_CORRECTION_PX_S: f64 = 300.0;

#[derive(Clone, Debug)]
pub struct MovementConfig {
    pub snap_radius: f64,
    pub near_radius: f64,
    pub alpha: f64,
    pub snap_boost: f64,
    pub min_speed_multiplier: f64,
    pub max_speed_multiplier: f64,
    /// Sens: sets the assumed image px per count, so it is the speed setting.
    pub sens: f64,
    pub clip_speed: f64,
    /// Fixed tuning (not user settings): correction time constant, assumed
    /// latency and feedforward gain.
    pub correction_tau_s: f64,
    pub assumed_latency_s: f64,
    pub feedforward_gain: f64,
}

impl Default for MovementConfig {
    fn default() -> Self {
        Self {
            snap_radius: 150.0,
            near_radius: 50.0,
            alpha: 1.5,
            snap_boost: 0.3,
            min_speed_multiplier: 0.01,
            max_speed_multiplier: 10.0,
            sens: 0.25,
            clip_speed: 1000.0,
            correction_tau_s: CORRECTION_TAU_S,
            assumed_latency_s: ASSUMED_LATENCY_S,
            feedforward_gain: FEEDFORWARD_GAIN,
        }
    }
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
    /// Scales the minimum step for this stroke.
    min_step_scale: f64,
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
            min_step_scale: rng.random_range(0.75..=1.25),
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

/// Fixed link between device counts and image motion (no online learning,
/// at the user's request). One count turns the camera yaw × sens degrees, so
/// image px per count scale with Sens; Sens is therefore the speed setting:
/// a lower value makes every correction larger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionModel {
    /// Image pixels the target shifts per applied device count.
    pub px_per_count: f64,
    /// Assumed time from a move being applied to its effect in a received frame.
    pub latency_s: f64,
}

impl MotionModel {
    pub fn for_config(config: &MovementConfig) -> Self {
        Self {
            px_per_count: PX_PER_COUNT_AT_SENS_1 * config.sens.clamp(0.01, 10.0),
            latency_s: config.assumed_latency_s,
        }
    }
}

/// Image px per count at Sens 1. About right for a 0.07°/count game at 1080p
/// with a 103° FOV, so in-game Sens is a good starting point.
const PX_PER_COUNT_AT_SENS_1: f64 = 1.0;
/// Capture → backend → device → game → capture again, assumed.
const ASSUMED_LATENCY_S: f64 = 0.03;
/// Moves kept for compensation (longer than any plausible latency).
const MOVE_LOG_S: f64 = 0.4;
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

#[derive(Clone, Copy, Debug)]
struct AppliedMove {
    time: f64,
    total: (f64, f64),
}

/// One detection result as the controller sees it.
#[derive(Clone, Copy, Debug)]
pub struct FrameError {
    pub frame_id: u64,
    pub received_s: f64,
    /// Unsmoothed error of the selected target (crop px).
    pub raw: (f64, f64),
    /// Smoothing factor per axis for this track (1 = none).
    pub smoothing_alpha: (f64, f64),
}

/// Smoothed control error, carried from frame to frame by the camera motion
/// our own moves caused, so smoothing only slows the target's own motion —
/// never the aim's — and cannot make the aim overshoot.
#[derive(Clone, Copy, Debug)]
struct SmoothedError {
    value: (f64, f64),
    /// When the frame it belongs to was captured.
    seen_at: f64,
}

pub struct MovementController {
    pub config: MovementConfig,
    model: MotionModel,
    last_frame_id: Option<u64>,
    /// Previous frame of the current track: (receive time, raw error).
    previous: Option<(f64, (f64, f64))>,
    frame_error: (f64, f64),
    frame_received_s: f64,
    smoothed: Option<SmoothedError>,
    /// The target's own velocity in crop px/s (camera motion of our moves added back).
    target_velocity: (f64, f64),
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
    pub fn new(sens: f64) -> Self {
        Self::with_config(MovementConfig {
            sens,
            ..MovementConfig::default()
        })
    }
    pub fn with_config(config: MovementConfig) -> Self {
        Self::with_rng(config, StdRng::from_os_rng())
    }
    fn with_rng(config: MovementConfig, rng: StdRng) -> Self {
        Self {
            model: MotionModel::for_config(&config),
            config,
            last_frame_id: None,
            previous: None,
            frame_error: (0.0, 0.0),
            frame_received_s: 0.0,
            smoothed: None,
            target_velocity: (0.0, 0.0),
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
    /// Target lost: forget the track. The move log stays.
    pub fn reset(&mut self) {
        self.last_frame_id = None;
        self.previous = None;
        self.smoothed = None;
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
    /// A new Sens setting: the speed of every correction follows it.
    pub fn set_sensitivity(&mut self, sens: f64) -> Result<(), &'static str> {
        if !(0.01..=10.0).contains(&sens) {
            return Err("sens must be 0.01..10");
        }
        self.config.sens = sens;
        self.model = MotionModel::for_config(&self.config);
        self.reset();
        Ok(())
    }
    pub fn model(&self) -> MotionModel {
        self.model
    }

    /// `applied`: the host applies this tick's output (trigger held or
    /// Always-on). Only applied moves are compensated for.
    pub fn delta(
        &mut self,
        sample: &MovementTargetSample,
        offsets: (f64, f64),
        now: f64,
        applied: bool,
    ) -> (i64, i64) {
        let Some(target) = sample.target else {
            self.reset();
            return (0, 0);
        };
        let frame = FrameError {
            frame_id: sample.frame_id,
            received_s: sample.received_s,
            raw: crop_target_error(target, sample.crop_origin, sample.processed_size, offsets),
            smoothing_alpha: sample.smoothing_alpha,
        };
        self.track(frame, now, applied)
    }
    /// Each call is a new frame received at `now`, with no move applied.
    pub fn step(&mut self, dx: f64, dy: f64, now: f64) -> (i64, i64) {
        let frame_id = self.last_frame_id.map_or(0, |id| id.wrapping_add(1));
        self.track(
            FrameError {
                frame_id,
                received_s: now,
                raw: (dx, dy),
                smoothing_alpha: (1.0, 1.0),
            },
            now,
            false,
        )
    }
    /// One ticker step. A frame is observed once (velocity, smoothing); every
    /// tick then works from the error estimated for `now`, so moves already
    /// sent since the frame are not sent again.
    pub fn track(&mut self, frame: FrameError, now: f64, applied: bool) -> (i64, i64) {
        if self.last_frame_id != Some(frame.frame_id) {
            self.observe(&frame);
        }
        let tick_s = self
            .last_tick_s
            .map(|previous| now - previous)
            .filter(|elapsed| *elapsed > 0.0)
            .unwrap_or(DEFAULT_TICK_S);
        self.last_tick_s = Some(now);
        let (px, py) = self.estimated_error(now);
        let target_speed = self.target_velocity.0.hypot(self.target_velocity.1);
        let moving = target_speed >= MOVING_TARGET_PX_S;
        let (mut vx, mut vy) = self.shape(px, py, now, tick_s, moving, target_speed);

        // Feedforward: turn at the target's own speed, so a moving target is
        // followed without waiting for a large error to build up.
        let scale = self.config.feedforward_gain * tick_s / self.model.px_per_count;
        vx += self.target_velocity.0 * scale;
        vy += self.target_velocity.1 * scale;
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
        let (out_x, out_y) = (round_ties_away(sum_x), round_ties_away(sum_y));
        self.residual = (sum_x - out_x as f64, sum_y - out_y as f64);
        if applied && (out_x != 0 || out_y != 0) {
            // The host applies at most ±32 per command and carries the rest.
            self.move_log.push_back(AppliedMove {
                time: now,
                total: (out_x as f64, out_y as f64),
            });
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

    /// Counts applied in `(from, to]`.
    fn moved_between(&self, from: f64, to: f64) -> (f64, f64) {
        self.move_log
            .iter()
            .filter(|applied| applied.time > from && applied.time <= to)
            .fold((0.0, 0.0), |total, applied| {
                (total.0 + applied.total.0, total.1 + applied.total.1)
            })
    }

    fn observe(&mut self, frame: &FrameError) {
        let t = frame.received_s;
        if let Some((previous_t, previous_raw)) = self.previous {
            let dt = t - previous_t;
            let jump = (frame.raw.0 - previous_raw.0).hypot(frame.raw.1 - previous_raw.1);
            if jump > TARGET_JUMP_PX {
                // A different target: start over, with the acquisition ramp.
                self.smoothed = None;
                self.target_velocity = (0.0, 0.0);
                self.stroke = None;
                self.acquiring = true;
            } else if !(0.0005..=0.1).contains(&dt) {
                self.smoothed = None;
                self.target_velocity = (0.0, 0.0);
            } else {
                // The target's own velocity adds back the motion our moves caused.
                let lag = self.model.latency_s;
                let (mx, my) = self.moved_between(previous_t - lag, t - lag);
                let k = self.model.px_per_count;
                let own = (
                    (frame.raw.0 - previous_raw.0 + k * mx) / dt,
                    (frame.raw.1 - previous_raw.1 + k * my) / dt,
                );
                // The velocity is not slowed by the smoothing strength: a slower
                // velocity made the aim trail and overshoot more in simulation.
                self.target_velocity = (
                    VELOCITY_ALPHA * own.0 + (1.0 - VELOCITY_ALPHA) * self.target_velocity.0,
                    VELOCITY_ALPHA * own.1 + (1.0 - VELOCITY_ALPHA) * self.target_velocity.1,
                );
            }
        }
        self.previous = Some((t, frame.raw));
        self.last_frame_id = Some(frame.frame_id);
        self.frame_error = self.smooth(frame);
        self.frame_received_s = t;
    }

    /// Exponential smoothing of the error around a constant-velocity
    /// prediction: the previous smoothed error is first moved by the target's
    /// own velocity and by the image shift of the moves applied between the
    /// two captures, so only deviations of the target's own motion are filtered.
    fn smooth(&mut self, frame: &FrameError) -> (f64, f64) {
        let alpha = |value: f64| {
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                1.0
            }
        };
        let (ax, ay) = (
            alpha(frame.smoothing_alpha.0),
            alpha(frame.smoothing_alpha.1),
        );
        let seen_at = frame.received_s - self.model.latency_s;
        let value = match self.smoothed {
            None => frame.raw,
            Some(previous) => {
                let elapsed = (seen_at - previous.seen_at).max(0.0);
                let (vx, vy) = self.target_velocity;
                let (mx, my) = self.moved_between(previous.seen_at, seen_at);
                let k = self.model.px_per_count;
                let carried = (
                    previous.value.0 + vx * elapsed - k * mx,
                    previous.value.1 + vy * elapsed - k * my,
                );
                (
                    carried.0 + ax * (frame.raw.0 - carried.0),
                    carried.1 + ay * (frame.raw.1 - carried.1),
                )
            }
        };
        self.smoothed = Some(SmoothedError { value, seen_at });
        value
    }

    /// Error expected at `now`: the frame shows the scene from `latency` ago,
    /// so add the target's motion since then and remove the moves applied since.
    fn estimated_error(&self, now: f64) -> (f64, f64) {
        let (ex, ey) = self.frame_error;
        let (vx, vy) = self.target_velocity;
        let seen_at = self.frame_received_s - self.model.latency_s;
        let elapsed = (now - seen_at).max(0.0);
        let (mx, my) = self.moved_between(seen_at, now);
        let k = self.model.px_per_count;
        (ex + vx * elapsed - k * mx, ey + vy * elapsed - k * my)
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
        // The aim cannot settle closer than about half a count, so where one
        // count is several pixels the dead zone grows with it.
        let count_px = self.model.px_per_count;
        let threshold = if self.settled {
            RESUME_RADIUS_PX.max(RESUME_RADIUS_COUNTS * count_px)
        } else {
            STOP_RADIUS_PX.max(STOP_RADIUS_COUNTS * count_px)
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
        let mut factor = ncaf_factor(
            distance,
            c.snap_radius,
            c.near_radius,
            c.alpha * stroke.decel,
            c.snap_boost,
        );
        // A fast target needs a firmer near-zone response or the aim trails
        // it; lift the factor part of the way toward 1, scaled by its speed.
        if factor < 1.0 {
            let boost = (target_speed / NEAR_BOOST_FULL_PX_S).min(1.0) * NEAR_BOOST_MAX;
            factor += (1.0 - factor) * boost;
        }
        let factor = factor.clamp(c.min_speed_multiplier, c.max_speed_multiplier);
        // Close the error in image px/s, then convert with px/count.
        let to_counts = tick_s / self.model.px_per_count;
        let magnitude = distance / c.correction_tau_s * factor * to_counts;
        let (cap, floor) = (
            MAX_CORRECTION_PX_S * to_counts,
            MIN_CORRECTION_PX_S * to_counts,
        );
        if magnitude <= 1e-12 {
            return (0.0, 0.0);
        }
        let (ux, uy) = (px / distance, py / distance);

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

        let step = bezier_response(magnitude.min(cap), cap, BEZIER_STRENGTH, stroke.bezier);
        let step = (step * ramp * speed_noise).max(floor * stroke.min_step_scale);

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
            self.error_px += controller.frame_error.0.hypot(controller.frame_error.1);
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
    fn always_on_without_physical_trigger_records_applied_moves() {
        for (always_on, held) in [(true, false), (false, true), (false, false)] {
            let mut controller = seeded(54);
            let runtime = runtime(always_on, held);
            controller.delta(&sample(2), (0.0, 0.0), 1.0, runtime.output_active());
            assert_eq!(!controller.move_log.is_empty(), always_on || held);
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
    fn correction_counts_scale_with_one_over_sens() {
        // Sens sets the assumed px/count: twice the Sens, half the counts.
        let step = |sens: f64| {
            let mut controller = seeded(54);
            controller.set_sensitivity(sens).unwrap();
            controller.acquiring = false;
            let (x, y) = controller.shape(200.0, 0.0, 1.0, DEFAULT_TICK_S, false, 0.0);
            x.hypot(y)
        };
        assert!((step(0.25) / step(0.5) - 2.0).abs() < 1e-9);
        // Beyond the snap radius: 200 px closed at 1/τ, a bit under the cap.
        let expected = 200.0 / CORRECTION_TAU_S * DEFAULT_TICK_S / 0.25;
        assert!(
            step(0.25) <= expected && step(0.25) > expected * 0.4,
            "{}",
            step(0.25)
        );
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

    /// Closed loop against a simulated game: image px per count (the real one,
    /// which the controller only assumes from Sens), frames that show the
    /// scene `latency_s` late, integer pixel detections with jitter, and a
    /// strafing target. Zoom (ADS) magnifies the error, the target's motion and
    /// each count; changes ramp over the keyframes.
    #[derive(Clone)]
    struct World {
        px_per_count: f64,
        latency_s: f64,
        fps: f64,
        /// Strafe amplitude in px (0.8 Hz); 0 keeps the target still.
        strafe: f64,
        /// Smoothing strength (1 = none), as set on the config page.
        smoothing: f64,
        /// Detection jitter amplitude (px, uniform ±).
        jitter: f64,
        /// (time s, zoom) keyframes, linear in between.
        zoom: Vec<(f64, f64)>,
        seconds: f64,
        /// Controller tuning override: (τ, assumed latency, feedforward gain).
        tuning: Option<(f64, f64, f64)>,
    }

    fn game() -> World {
        World {
            px_per_count: 0.25,
            latency_s: 0.024,
            fps: 240.0,
            strafe: 150.0,
            smoothing: 1.0,
            jitter: 0.7,
            zoom: vec![(0.0, 1.0)],
            seconds: 6.0,
            tuning: None,
        }
    }

    fn zoom_at(keys: &[(f64, f64)], t: f64) -> f64 {
        let mut value = keys[0].1;
        for pair in keys.windows(2) {
            let ((t0, z0), (t1, z1)) = (pair[0], pair[1]);
            if t >= t1 {
                value = z1;
            } else if t > t0 {
                return z0 + (z1 - z0) * (t - t0) / (t1 - t0);
            }
        }
        value
    }

    #[derive(Debug)]
    struct SimRun {
        /// RMS error (unzoomed px) over the second half.
        rms: f64,
        /// Largest error after the first 0.5 s.
        peak_after_start: f64,
        /// First time the error fell under 4 px.
        settled_at: Option<f64>,
        /// Largest distance past the target along the initial error direction.
        overshoot: f64,
        /// Ticks with output while the error stayed inside 3 px (after 1 s).
        moves_on_target: usize,
        ticks_on_target: usize,
        /// Share of time the target was outside the 256 px crop.
        lost: f64,
    }

    fn simulate_run(world: &World, seed: u64) -> SimRun {
        let frame_s = 1.0 / world.fps;
        let target = |t: f64| {
            (
                60.0 + world.strafe * (std::f64::consts::TAU * 0.8 * t).sin(),
                -20.0 + world.strafe / 4.0 * (std::f64::consts::TAU * 0.3 * t).cos(),
            )
        };
        let mut controller = seeded(seed);
        if let Some((tau, latency, feedforward)) = world.tuning {
            controller.config.correction_tau_s = tau;
            controller.config.assumed_latency_s = latency;
            controller.config.feedforward_gain = feedforward;
            controller.model = MotionModel::for_config(&controller.config);
        }
        let initial = target(0.0);
        let norm = initial.0.hypot(initial.1);
        let direction = (initial.0 / norm, initial.1 / norm);
        let mut noise = StdRng::seed_from_u64(seed ^ 0x5eed);
        // Camera offset (unzoomed px) after each applied move.
        let mut camera = vec![(0.0_f64, (0.0_f64, 0.0_f64))];
        let camera_at = |camera: &[(f64, (f64, f64))], t: f64| {
            camera
                .iter()
                .rev()
                .find(|(time, _)| *time <= t)
                .map_or((0.0, 0.0), |(_, offset)| *offset)
        };
        let mut frame: Option<FrameError> = None;
        let mut next_frame = 0_u64;
        let (mut squared, mut samples, mut peak) = (0.0, 0, 0.0_f64);
        let (mut settled_at, mut overshoot) = (None, 0.0_f64);
        let (mut moves_on_target, mut ticks_on_target, mut lost, mut ticks) = (0, 0, 0, 0);
        let mut visible = true;
        for tick in 1..=(world.seconds / DEFAULT_TICK_S) as usize {
            let now = tick as f64 * DEFAULT_TICK_S;
            while (next_frame as f64) * frame_s + world.latency_s <= now {
                let captured = next_frame as f64 * frame_s;
                let zoom = zoom_at(&world.zoom, captured);
                let (tx, ty) = target(captured);
                let (cx, cy) = camera_at(&camera, captured);
                let mut jitter = || noise.random_range(-world.jitter..=world.jitter);
                let image = ((tx - cx) * zoom + jitter(), (ty - cy) * zoom + jitter());
                visible = image.0.abs() <= 128.0 && image.1.abs() <= 128.0;
                frame = visible.then_some(FrameError {
                    frame_id: next_frame,
                    received_s: captured + world.latency_s,
                    raw: (image.0.trunc(), image.1.trunc()),
                    smoothing_alpha: (1.0 / world.smoothing, 1.0 / world.smoothing),
                });
                next_frame += 1;
            }
            let (dx, dy) = match frame {
                Some(current) => controller.track(current, now, true),
                None => {
                    controller.reset();
                    (0, 0)
                }
            };
            let k = world.px_per_count / zoom_at(&world.zoom, now);
            let (cx, cy) = camera.last().expect("camera").1;
            camera.push((
                now,
                (
                    cx + dx.clamp(-32, 32) as f64 * k,
                    cy + dy.clamp(-32, 32) as f64 * k,
                ),
            ));
            let (tx, ty) = target(now);
            let (cx, cy) = camera.last().expect("camera").1;
            let error = (tx - cx).hypot(ty - cy);
            ticks += 1;
            if !visible {
                lost += 1;
            }
            if now >= 0.5 {
                peak = peak.max(error);
            }
            if error < 4.0 && settled_at.is_none() {
                settled_at = Some(now);
            }
            overshoot = overshoot.max(-((tx - cx) * direction.0 + (ty - cy) * direction.1));
            if now >= 1.0 && error < 3.0 {
                ticks_on_target += 1;
                if dx != 0 || dy != 0 {
                    moves_on_target += 1;
                }
            }
            if now >= world.seconds / 2.0 {
                squared += error * error;
                samples += 1;
            }
        }
        SimRun {
            rms: (squared / samples.max(1) as f64).sqrt(),
            peak_after_start: peak,
            settled_at,
            overshoot,
            moves_on_target,
            ticks_on_target,
            lost: lost as f64 / ticks.max(1) as f64,
        }
    }

    fn still() -> World {
        World {
            strafe: 0.0,
            ..game()
        }
    }

    #[test]
    fn a_still_target_is_reached_quickly_without_overshoot() {
        for latency_s in [0.016, 0.024, 0.04] {
            for seed in 0..4 {
                let run = simulate_run(
                    &World {
                        latency_s,
                        ..still()
                    },
                    900 + seed,
                );
                let reached = run.settled_at.expect("reached");
                assert!(reached < 0.35, "{latency_s}: reached after {reached:.2}s");
                assert!(
                    run.overshoot < 4.0,
                    "{latency_s}: overshoot {:.1}",
                    run.overshoot
                );
                assert!(run.peak_after_start < 5.0, "{run:?}");
            }
        }
    }

    #[test]
    fn a_settled_aim_does_not_shake_on_a_still_target() {
        for seed in 0..4 {
            let run = simulate_run(&still(), 910 + seed);
            assert!(run.ticks_on_target > 1000, "{run:?}");
            let share = run.moves_on_target as f64 / run.ticks_on_target as f64;
            assert!(
                share < 0.05,
                "moved on {:.1}% of on-target ticks",
                share * 100.0
            );
        }
    }

    #[test]
    fn the_loop_stays_stable_when_sens_underestimates_the_game() {
        // The game turns 1.5× more per count than Sens implies, with 50 ms latency.
        for seed in 0..4 {
            let run = simulate_run(
                &World {
                    px_per_count: 0.375,
                    latency_s: 0.05,
                    ..still()
                },
                920 + seed,
            );
            assert!(run.peak_after_start < 6.0, "{run:?}");
            assert!(run.rms < 3.0, "{run:?}");
        }
    }

    #[test]
    fn a_strafing_target_is_tracked() {
        let rms = (0..4)
            .map(|seed| simulate_run(&game(), 930 + seed).rms)
            .sum::<f64>()
            / 4.0;
        assert!(rms < 15.0, "strafing RMS {rms:.1} px");
    }

    #[test]
    fn strong_smoothing_does_not_make_the_aim_overshoot() {
        for smoothing in [10.0, 50.0] {
            let run = simulate_run(
                &World {
                    smoothing,
                    ..still()
                },
                940,
            );
            assert!(run.overshoot < 6.0, "strength {smoothing}: {run:?}");
        }
    }

    #[test]
    fn only_a_new_acquisition_ramps_up() {
        let mut controller = seeded(5);
        let first = controller.step(200.0, 0.0, 0.0).0;
        for tick in 1..100 {
            controller.step(200.0, 0.0, tick as f64 * DEFAULT_TICK_S);
        }
        let mut resumed = seeded(5);
        resumed.acquiring = false;
        assert!(resumed.step(200.0, 0.0, 0.0).0 > first);
    }

    #[test]
    fn near_zone_response_rises_with_target_speed() {
        let step = |speed: f64| {
            let mut controller = seeded(54);
            controller.acquiring = false;
            let (x, y) = controller.shape(20.0, 0.0, 1.0, DEFAULT_TICK_S, false, speed);
            x.hypot(y)
        };
        assert!(step(600.0) > step(0.0) * 1.5);
    }

    fn report_line(world: &World, seeds: std::ops::Range<u64>) -> String {
        let runs: Vec<SimRun> = seeds.map(|seed| simulate_run(world, seed)).collect();
        let n = runs.len() as f64;
        let mean = |f: &dyn Fn(&SimRun) -> f64| runs.iter().map(f).sum::<f64>() / n;
        format!(
            "reach {:.2}s, overshoot {:.1} px, peak {:.1} px, RMS {:.1} px, on-target moves {:.1}%, lost {:.0}%",
            mean(&|r| r.settled_at.unwrap_or(f64::INFINITY)),
            mean(&|r| r.overshoot),
            mean(&|r| r.peak_after_start),
            mean(&|r| r.rms),
            mean(&|r| 100.0 * r.moves_on_target as f64 / r.ticks_on_target.max(1) as f64),
            mean(&|r| 100.0 * r.lost),
        )
    }

    /// Robustness to a wrong Sens (real px/count vs. assumed 0.25) and latency
    /// (not a pass/fail test):
    /// `cargo test --release --lib -- simulation_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn simulation_report() {
        for (target, base) in [("still", still()), ("strafe", game())] {
            for latency_s in [0.016, 0.03, 0.05, 0.08] {
                for ratio in [0.5, 0.75, 1.0, 1.5, 2.0] {
                    let world = World {
                        px_per_count: 0.25 * ratio,
                        latency_s,
                        ..base.clone()
                    };
                    eprintln!(
                        "SIM {target} latency {:.0} ms, real/assumed px/count {ratio}: {}",
                        latency_s * 1000.0,
                        report_line(&world, 0..8)
                    );
                }
            }
        }
    }

    /// Tuning sweep: worst case over latency 16–80 ms and real/assumed
    /// px/count 0.5–2 (not a pass/fail test):
    /// `cargo test --release --lib -- tuning_sweep --ignored --nocapture`.
    #[test]
    #[ignore]
    fn tuning_sweep() {
        let taus: Vec<f64> = std::env::var("SWEEP_TAU")
            .map(|v| v.split(',').map(|x| x.parse().unwrap()).collect())
            .unwrap_or(vec![0.035, 0.06, 0.1]);
        let latencies: Vec<f64> = std::env::var("SWEEP_LAT")
            .map(|v| v.split(',').map(|x| x.parse().unwrap()).collect())
            .unwrap_or(vec![0.0, 0.03, 0.05]);
        let feedforwards: Vec<f64> = std::env::var("SWEEP_FF")
            .map(|v| v.split(',').map(|x| x.parse().unwrap()).collect())
            .unwrap_or(vec![0.0, 0.5, 0.9]);
        for &tau in &taus {
            for &assumed in &latencies {
                for &feedforward in &feedforwards {
                    let mut worst_peak: f64 = 0.0;
                    let mut worst_reach: f64 = 0.0;
                    let mut worst_strafe: f64 = 0.0;
                    let mut worst_moves: f64 = 0.0;
                    for latency_s in [0.016, 0.03, 0.05, 0.08] {
                        for ratio in [0.5, 1.0, 2.0] {
                            for (base, strafing) in [(still(), false), (game(), true)] {
                                let world = World {
                                    px_per_count: 0.25 * ratio,
                                    latency_s,
                                    seconds: 4.0,
                                    tuning: Some((tau, assumed, feedforward)),
                                    ..base
                                };
                                for seed in 0..3 {
                                    let run = simulate_run(&world, seed);
                                    if strafing {
                                        worst_strafe = worst_strafe.max(run.rms);
                                    } else {
                                        worst_peak = worst_peak.max(run.peak_after_start);
                                        worst_reach = worst_reach
                                            .max(run.settled_at.unwrap_or(f64::INFINITY));
                                        worst_moves = worst_moves.max(
                                            run.moves_on_target as f64
                                                / run.ticks_on_target.max(1) as f64,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    eprintln!(
                        "SWEEP tau {tau} lat {assumed} ff {feedforward}: still peak {worst_peak:.1} px, reach {worst_reach:.2} s, on-target moves {:.0}%, strafe RMS {worst_strafe:.1} px",
                        worst_moves * 100.0
                    );
                }
            }
        }
    }

    /// ADS from the start and toggled while firing:
    /// `cargo test --release --lib -- ads_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ads_report() {
        for zoom in [1.0, 1.15, 1.25, 1.5, 2.0, 3.0] {
            for (name, keys) in [
                ("from start", vec![(0.0, zoom)]),
                (
                    "toggled",
                    vec![
                        (0.0, 1.0),
                        (1.0, 1.0),
                        (1.15, zoom),
                        (2.4, zoom),
                        (2.55, 1.0),
                        (3.3, 1.0),
                        (3.45, zoom),
                        (6.0, zoom),
                    ],
                ),
            ] {
                for (target, strafe) in [("still", 0.0), ("strafe 20 px", 20.0)] {
                    let world = World {
                        strafe,
                        zoom: keys.clone(),
                        ..game()
                    };
                    eprintln!(
                        "ADS {zoom}x {name}, {target}: {}",
                        report_line(&world, 0..8)
                    );
                }
            }
        }
    }
}
