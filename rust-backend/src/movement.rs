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
/// A rough calibration closes errors more slowly: the time constant grows by
/// this factor per unit of relative standard error.
const CORRECTION_TAU_PER_ERROR: f64 = 4.0;
/// Smallest uncalibrated step outside the dead zone, in device counts per
/// tick. Steps are vector lengths, so with the residual accumulator a small
/// diagonal error is walked as a diagonal, not a forced ±1 staircase.
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

/// How fast the aim closes an error once px/count is measured: the remaining
/// error shrinks with this time constant (before NCAF and the speed curve).
const CORRECTION_TAU_S: f64 = 0.035;
/// Correction speed cap and floor, in image px/s.
const MAX_CORRECTION_PX_S: f64 = 4000.0;
const MIN_CORRECTION_PX_S: f64 = 300.0;
/// Before px/count is measured (the first ~0.1 s, never with a saved file):
/// a deliberately slow step, safe for any game. A count turns the camera by
/// yaw × sens degrees, so a higher sens needs fewer counts: counts scale with
/// 1/sens. DPI only describes the physical mouse; injected counts ignore it.
const UNCALIBRATED_COUNTS_PER_PX_AT_SENS_1: f64 = 0.0035;
const UNCALIBRATED_MAX_STEP_COUNTS: f64 = 50.0;

#[derive(Clone, Debug)]
pub struct MovementConfig {
    pub snap_radius: f64,
    pub near_radius: f64,
    pub alpha: f64,
    pub snap_boost: f64,
    pub min_speed_multiplier: f64,
    pub max_speed_multiplier: f64,
    /// In-game sensitivity: only scales the uncalibrated step and keys the
    /// saved calibrations.
    pub sens: f64,
    pub clip_speed: f64,
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
        }
    }
}

/// Uncalibrated step size: device counts per image pixel of error.
pub fn uncalibrated_counts_per_px(sens: f64) -> f64 {
    UNCALIBRATED_COUNTS_PER_PX_AT_SENS_1 / sens.max(0.01)
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

/// Measured link between device counts and image motion, found online while
/// tracking: image pixels per count depend on the game's yaw, sensitivity,
/// FOV and capture resolution, none of which SysX knows.
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
const CALIBRATION_MIN_WEIGHT: f64 = 6.0;
/// Calibration switches on when the standard error falls below this fraction
/// of the estimate (the safety margin below covers the rest), then follows
/// every estimate up to the second fraction.
const CALIBRATION_ENTER_REL_ERROR: f64 = 0.4;
const CALIBRATION_KEEP_REL_ERROR: f64 = 0.6;
/// Saved to disk only once this tight.
const CALIBRATION_SAVE_REL_ERROR: f64 = 0.15;
/// A probe that moves the image less than this is below whole-pixel
/// detection; such estimates are rough, so they must be as tight as a
/// saveable one before they switch on.
const PROBE_VISIBLE_PX: f64 = 0.3;
/// Rates for calibration are taken over this span (2 frames at the intended
/// 240 FPS) rather than between two frames: whole-pixel detections make
/// single-frame rates too noisy. At lower frame rates a span is one frame.
const CALIBRATION_SPAN_S: f64 = 0.008;
/// Frames of the current track kept for calibration spans.
const TRACK_HISTORY_S: f64 = 0.1;
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
const PROBE_COUNTS: i64 = 2;
const PROBE_PROBABILITY: f64 = 1.0;
const PROBE_PROBABILITY_CALIBRATED: f64 = 0.2;
const PROBE_TARGET_PX: f64 = 0.8;
const PROBE_MAX_PX: f64 = 3.0;
/// Before the first estimate: the frame error with this short velocity lead
/// (the original uncompensated behaviour, stable for any game).
const UNCALIBRATED_LEAD_S: f64 = 0.016;
/// Until the estimate is tight, use px/count raised by this many standard
/// errors: too high only slows tracking a little, too low can make the loop
/// oscillate, so the uncertainty is spent on the safe side.
const CALIBRATION_SAFETY_SIGMAS: f64 = 1.5;
/// Zooming in (ADS, scope) multiplies px/count at once, and a loop running
/// with too low a px/count oscillates. A second estimate with this short
/// memory (about 0.15 s at 240 FPS) may raise px/count straight away, never
/// lower it: too high only slows tracking until the slow estimate catches up.
const FAST_CALIBRATION_FORGET: f64 = 0.97;
const FAST_CALIBRATION_MIN_WEIGHT: f64 = 12.0;
const FAST_RAISE_RATIO: f64 = 2.0;
const FAST_RAISE_REL_ERROR: f64 = 0.2;
/// Consecutive calibration samples that must agree before a raise.
const FAST_RAISE_SAMPLES: u32 = 4;
/// A loaded calibration is kept until this much new evidence (a few seconds
/// of held trigger) has been gathered; a calibration is only dropped then too.
const CALIBRATION_TRUST_WEIGHT: f64 = 400.0;

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
    forget: f64,
}

impl Default for MotionCalibrator {
    fn default() -> Self {
        Self {
            forget: CALIBRATION_FORGET,
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
    fn fast() -> Self {
        Self {
            forget: FAST_CALIBRATION_FORGET,
            ..Self::default()
        }
    }

    /// One sample: `y` per axis, `xs`/`zs` per latency and axis. Samples taken
    /// every frame over overlapping spans count as `w` (< 1) of a sample each.
    fn add(&mut self, y: (f64, f64), xs: &LatencyRates, zs: &LatencyRates, w: f64) {
        let forget = self.forget;
        for j in 0..LATENCY_STEPS {
            let (x, z) = (xs[j], zs[j]);
            self.szz[j] = self.szz[j] * forget + w * (z.0 * z.0 + z.1 * z.1);
            self.szy[j] = self.szy[j] * forget + w * (z.0 * y.0 + z.1 * y.1);
            self.szx[j] = self.szx[j] * forget + w * (z.0 * x.0 + z.1 * x.1);
            self.sxx[j] = self.sxx[j] * forget + w * (x.0 * x.0 + x.1 * x.1);
            self.sxy[j] = self.sxy[j] * forget + w * (x.0 * y.0 + x.1 * y.1);
        }
        self.syy = self.syy * forget + w * (y.0 * y.0 + y.1 * y.1);
        self.weight = self.weight * forget + 2.0 * w;
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
        let (k, relative_error) = self.slope_at(best)?;
        Some((
            MotionCalibration {
                px_per_count: k,
                latency_s: best as f64 * LATENCY_STEP_S,
            },
            relative_error,
        ))
    }

    /// Ordinary least-squares slope over all applied moves at latency step `j`
    /// and its relative standard error. Biased by a moving target, but the
    /// large moves of an oscillating loop make it precise within a few frames.
    fn least_squares_at(&self, j: usize) -> Option<(f64, f64)> {
        if self.weight < FAST_CALIBRATION_MIN_WEIGHT || self.sxx[j] <= 1e-9 {
            return None;
        }
        let k = self.sxy[j] / self.sxx[j];
        if !(PX_PER_COUNT_RANGE.0..=PX_PER_COUNT_RANGE.1).contains(&k) {
            return None;
        }
        let sse = self.syy - k * self.sxy[j];
        let standard_error = (sse.max(0.0) / (self.weight - 1.0) / self.sxx[j]).sqrt();
        Some((k, standard_error / k))
    }

    /// Pixels per count and its relative standard error at latency step `j`.
    fn slope_at(&self, j: usize) -> Option<(f64, f64)> {
        if self.szx[j] <= 1e-9 {
            return None;
        }
        let k = self.szy[j] / self.szx[j];
        if !(PX_PER_COUNT_RANGE.0..=PX_PER_COUNT_RANGE.1).contains(&k) {
            return None;
        }
        let sse = self.syy - 2.0 * k * self.sxy[j] + k * k * self.sxx[j];
        let standard_error =
            (sse.max(0.0) / (self.weight - 1.0) * self.szz[j]).sqrt() / self.szx[j];
        Some((k, standard_error / k))
    }
}

/// One detection result as the controller sees it: the error used for
/// control (smoothed target) and the raw error used for estimation.
#[derive(Clone, Copy, Debug)]
pub struct FrameError {
    pub frame_id: u64,
    pub received_s: f64,
    /// Unsmoothed error of the selected target (crop px).
    pub raw: (f64, f64),
    /// Smoothing factor per axis for this track (1 = none).
    pub smoothing_alpha: (f64, f64),
}

/// Smoothed control error. Calibrated, it is carried from frame to frame by
/// the camera motion our own moves caused, so smoothing only slows the
/// target's own motion — never the aim's — and cannot make the aim overshoot.
#[derive(Clone, Copy, Debug)]
struct SmoothedError {
    value: (f64, f64),
    /// When the frame it belongs to was captured.
    seen_at: f64,
}

pub struct MovementController {
    pub config: MovementConfig,
    last_frame_id: Option<u64>,
    /// Frames of the current track: (receive time, raw error), newest last.
    track: VecDeque<(f64, (f64, f64))>,
    frame_error: (f64, f64),
    frame_received_s: f64,
    smoothed: Option<SmoothedError>,
    /// Target velocity in crop px/s: its own motion once calibrated, the
    /// apparent (target minus camera) motion before that.
    target_velocity: (f64, f64),
    calibrator: MotionCalibrator,
    /// Short-memory twin of `calibrator` that catches a sudden zoom.
    fast_calibrator: MotionCalibrator,
    fast_raise_streak: u32,
    /// Simulation comparison only: run without the zoom raise.
    #[cfg(test)]
    zoom_guard_off: bool,
    calibration: Option<MotionCalibration>,
    /// Relative standard error of `calibration` (0 for a loaded one).
    calibration_error: f64,
    /// `calibration` came from the saved file and is not yet re-measured.
    calibration_loaded: bool,
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
            config,
            last_frame_id: None,
            track: VecDeque::new(),
            frame_error: (0.0, 0.0),
            frame_received_s: 0.0,
            smoothed: None,
            target_velocity: (0.0, 0.0),
            calibrator: MotionCalibrator::default(),
            fast_calibrator: MotionCalibrator::fast(),
            fast_raise_streak: 0,
            #[cfg(test)]
            zoom_guard_off: false,
            calibration: None,
            calibration_error: 0.0,
            calibration_loaded: false,
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
    /// A new in-game sensitivity. (DPI does not change injected counts.)
    pub fn set_sensitivity(&mut self, sens: f64) -> Result<(), &'static str> {
        if !(0.01..=10.0).contains(&sens) {
            return Err("sens must be 0.01..10");
        }
        self.config.sens = sens;
        // A new in-game sensitivity changes pixels per count: measure again.
        self.calibrator = MotionCalibrator::default();
        self.fast_calibrator = MotionCalibrator::fast();
        self.calibration = None;
        self.calibration_error = 0.0;
        self.calibration_loaded = false;
        self.reset();
        Ok(())
    }
    /// The measured (or loaded) calibration, if any.
    pub fn motion_calibration(&self) -> Option<MotionCalibration> {
        self.calibration
    }
    /// Start from a saved calibration (from an earlier session).
    pub fn load_calibration(&mut self, calibration: Option<MotionCalibration>) {
        self.calibration = calibration;
        self.calibration_error = 0.0;
        self.calibration_loaded = calibration.is_some();
    }
    /// The calibration once it is tight enough to save for later sessions.
    pub fn saveable_calibration(&self) -> Option<MotionCalibration> {
        self.calibration
            .filter(|_| self.calibrator.weight >= CALIBRATION_TRUST_WEIGHT)
            .filter(|_| self.calibration_error <= CALIBRATION_SAVE_REL_ERROR)
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
        let frame = FrameError {
            frame_id: sample.frame_id,
            received_s: sample.received_s,
            raw: crop_target_error(target, sample.crop_origin, sample.processed_size, offsets),
            smoothing_alpha: sample.smoothing_alpha,
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
                raw: (dx, dy),
                smoothing_alpha: (1.0, 1.0),
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
        // Before calibration the speed is the apparent one (it includes our own
        // moves), so only a calibrated speed may keep the aim from settling.
        let moving = self.calibration.is_some() && target_speed >= MOVING_TARGET_PX_S;
        let (mut vx, mut vy) = self.shape(px, py, now, tick_s, moving, target_speed);

        // Feedforward: turn at the target's own speed, so a moving target is
        // followed without waiting for a large error to build up.
        if let Some(calibration) = self.calibration {
            // Feedforward trusts the latency and px/count as much as they deserve:
            // with a wrong latency it would feed the loop's own oscillation.
            let caution = 1.0 + CORRECTION_TAU_PER_ERROR * self.calibration_error;
            let scale = FEEDFORWARD_GAIN / caution * tick_s / calibration.px_per_count;
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
        let (probability, counts) = match self.calibration {
            None => (PROBE_PROBABILITY, PROBE_COUNTS),
            // Calibrated: size the probe to about PROBE_TARGET_PX of image
            // motion, and stop probing where one count is already visible jitter.
            Some(calibration) => {
                let counts = (PROBE_TARGET_PX / calibration.px_per_count)
                    .round()
                    .clamp(1.0, PROBE_COUNTS as f64);
                if counts * calibration.px_per_count > PROBE_MAX_PX {
                    return 0;
                }
                (PROBE_PROBABILITY_CALIBRATED, counts as i64)
            }
        };
        if self.rng.random::<f64>() >= probability {
            0
        } else if self.rng.random::<bool>() {
            counts
        } else {
            -counts
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
                self.smoothed = None;
                self.target_velocity = (0.0, 0.0);
                self.stroke = None;
                self.acquiring = true;
            } else if !(0.0005..=0.1).contains(&dt) {
                self.track.clear();
                self.smoothed = None;
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
                // The velocity is not slowed by the smoothing strength: a slower
                // velocity made the aim trail and overshoot more in simulation.
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
            .is_some_and(|(time, _)| *time < t - TRACK_HISTORY_S)
        {
            self.track.pop_front();
        }
        self.last_frame_id = Some(frame.frame_id);
        self.frame_error = self.smooth(frame);
        self.frame_received_s = t;
    }

    /// Exponential smoothing of the error around a constant-velocity
    /// prediction. Calibrated, the previous smoothed error is first moved by
    /// the target's own velocity and by the image shift of the moves applied
    /// between the two captures, so only deviations of the target's own motion
    /// are filtered. Before calibration that shift is unknown and the image
    /// error is predicted with its apparent velocity.
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
        let seen_at = frame.received_s - self.calibration.map_or(0.0, |c| c.latency_s);
        let value = match self.smoothed {
            None => frame.raw,
            Some(previous) => {
                // Carry the previous value along the target's own velocity, so a
                // steadily moving target is not trailed; only deviations from
                // that motion (jitter, sudden jerks) are smoothed.
                let elapsed = (seen_at - previous.seen_at).max(0.0);
                let (vx, vy) = self.target_velocity;
                let predicted = (
                    previous.value.0 + vx * elapsed,
                    previous.value.1 + vy * elapsed,
                );
                let carried = match self.calibration {
                    Some(calibration) => {
                        let ((mx, my), _) = self.moved_between(previous.seen_at, seen_at);
                        let k = calibration.px_per_count;
                        (predicted.0 - k * mx, predicted.1 - k * my)
                    }
                    None => predicted,
                };
                (
                    carried.0 + ax * (frame.raw.0 - carried.0),
                    carried.1 + ay * (frame.raw.1 - carried.1),
                )
            }
        };
        self.smoothed = Some(SmoothedError { value, seen_at });
        value
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
        // Allow one frame interval of slack, so slower frame rates still work.
        let longest = 2.0 * CALIBRATION_SPAN_S.max(frame_dt);
        if late > longest || early > longest {
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
        self.fast_calibrator.add(y, &xs, &zs, weight);
        if self.raise_after_zoom() {
            return;
        }
        let Some((estimate, relative_error)) = self.calibrator.estimate() else {
            return;
        };
        let trusted = self.calibrator.weight >= CALIBRATION_TRUST_WEIGHT;
        let probe_px = estimate.px_per_count * PROBE_COUNTS as f64;
        let enter = if probe_px < PROBE_VISIBLE_PX {
            CALIBRATION_SAVE_REL_ERROR
        } else {
            CALIBRATION_ENTER_REL_ERROR
        };
        let accept = match self.calibration {
            None => relative_error <= enter,
            // A saved value is not traded for a rougher new one until enough
            // new evidence exists. A fresh estimate always follows the data,
            // so an early wrong guess is corrected as evidence arrives.
            Some(_) if self.calibration_loaded && !trusted => relative_error <= 0.0,
            Some(_) => relative_error <= CALIBRATION_KEEP_REL_ERROR,
        };
        if accept {
            self.calibration = Some(MotionCalibration {
                px_per_count: estimate.px_per_count
                    * (1.0 + CALIBRATION_SAFETY_SIGMAS * relative_error),
                ..estimate
            });
            self.calibration_error = relative_error;
            self.calibration_loaded = false;
        } else if trusted && self.calibration.is_some() {
            // Plenty of new evidence and it no longer fits: measure afresh.
            self.calibration = None;
        }
    }

    /// Raise px/count at once when the short-memory estimate (at the current
    /// latency, which a zoom does not change) is clearly above the one in use.
    /// The slow estimate restarts from the short one's evidence so it does not
    /// pull the value straight back down.
    fn raise_after_zoom(&mut self) -> bool {
        #[cfg(test)]
        if self.zoom_guard_off {
            return false;
        }
        let Some(calibration) = self.calibration else {
            self.fast_raise_streak = 0;
            return false;
        };
        if self.fast_calibrator.weight < FAST_CALIBRATION_MIN_WEIGHT {
            return false;
        }
        let step = (calibration.latency_s / LATENCY_STEP_S).round() as usize;
        let Some((k, relative_error)) = self
            .fast_calibrator
            .least_squares_at(step.min(LATENCY_STEPS - 1))
        else {
            return false;
        };
        let raised = k * (1.0 + CALIBRATION_SAFETY_SIGMAS * relative_error);
        if relative_error > FAST_RAISE_REL_ERROR || k < calibration.px_per_count * FAST_RAISE_RATIO
        {
            self.fast_raise_streak = 0;
            return false;
        }
        self.fast_raise_streak += 1;
        if self.fast_raise_streak < FAST_RAISE_SAMPLES {
            return false;
        }
        self.fast_raise_streak = 0;
        self.calibration = Some(MotionCalibration {
            px_per_count: raised,
            ..calibration
        });
        self.calibration_error = relative_error;
        self.calibration_loaded = false;
        self.calibrator = MotionCalibrator {
            forget: CALIBRATION_FORGET,
            ..self.fast_calibrator.clone()
        };
        true
    }

    /// Error expected at `now`. Calibrated: the frame shows the scene from
    /// `latency` ago, so add the target's motion since then and remove the
    /// moves applied since then.
    fn estimated_error(&self, now: f64) -> (f64, f64) {
        let (ex, ey) = self.frame_error;
        let (vx, vy) = self.target_velocity;
        let Some(calibration) = self.calibration else {
            return (ex + vx * UNCALIBRATED_LEAD_S, ey + vy * UNCALIBRATED_LEAD_S);
        };
        let seen_at = self.frame_received_s - calibration.latency_s;
        let elapsed = (now - seen_at).max(0.0);
        let ((mx, my), _) = self.moved_between(seen_at, now);
        let k = calibration.px_per_count;
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
        let count_px = self.calibration.map_or(0.0, |c| c.px_per_count);
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
        if self.calibration.is_some() && factor < 1.0 {
            let boost = (target_speed / NEAR_BOOST_FULL_PX_S).min(1.0) * NEAR_BOOST_MAX;
            factor += (1.0 - factor) * boost;
        }
        let factor = factor.clamp(c.min_speed_multiplier, c.max_speed_multiplier);
        // Step length in device counts per tick, with its cap and floor.
        let (magnitude, cap, floor) = match self.calibration {
            // Physical: close the error in image px/s, then convert with the
            // measured px/count.
            Some(calibration) => {
                let to_counts = tick_s / calibration.px_per_count;
                // A rough calibration (wrong latency, px/count) closes errors
                // more gently, so a wrong estimate cannot fling the aim.
                let caution = 1.0 + CORRECTION_TAU_PER_ERROR * self.calibration_error;
                (
                    distance / (CORRECTION_TAU_S * caution) * factor * to_counts,
                    MAX_CORRECTION_PX_S * to_counts,
                    MIN_CORRECTION_PX_S / caution * to_counts,
                )
            }
            None => (
                distance * uncalibrated_counts_per_px(c.sens) * factor,
                UNCALIBRATED_MAX_STEP_COUNTS,
                MIN_STEP_COUNTS,
            ),
        };
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
    /// Calibration accounts for either this gate or Motion Always-on.
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

const CALIBRATION_SAVE_INTERVAL: Duration = Duration::from_secs(2);
const SAVED_CALIBRATIONS: usize = 16;

fn calibration_changed(old: MotionCalibration, new: MotionCalibration) -> bool {
    (new.px_per_count / old.px_per_count - 1.0).abs() > 0.03
        || (new.latency_s - old.latency_s).abs() > LATENCY_STEP_S / 2.0
}

/// Keyed by sensitivity only: DPI does not change what a count does.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct SavedCalibration {
    sens: f64,
    px_per_count: f64,
    latency_s: f64,
}

/// Measured calibrations per sensitivity, kept in a small JSON file in the
/// app data directory so later sessions track at full speed from the start.
/// Writes happen on a short-lived background thread.
pub struct CalibrationStore {
    path: std::path::PathBuf,
    entries: Arc<Mutex<Vec<SavedCalibration>>>,
}

impl CalibrationStore {
    pub fn load(path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<SavedCalibration>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| {
                (PX_PER_COUNT_RANGE.0..=PX_PER_COUNT_RANGE.1).contains(&entry.px_per_count)
                    && (0.0..=LATENCY_STEP_S * (LATENCY_STEPS - 1) as f64)
                        .contains(&entry.latency_s)
            })
            .collect();
        Self {
            path,
            entries: Arc::new(Mutex::new(entries)),
        }
    }

    pub fn get(&self, sens: f64) -> Option<MotionCalibration> {
        let entries = self.entries.lock().ok()?;
        entries
            .iter()
            .find(|entry| entry.sens == sens)
            .map(|entry| MotionCalibration {
                px_per_count: entry.px_per_count,
                latency_s: entry.latency_s,
            })
    }

    pub fn put(&self, sens: f64, calibration: MotionCalibration) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.retain(|entry| entry.sens != sens);
        entries.push(SavedCalibration {
            sens,
            px_per_count: calibration.px_per_count,
            latency_s: calibration.latency_s,
        });
        let excess = entries.len().saturating_sub(SAVED_CALIBRATIONS);
        entries.drain(..excess);
        let path = self.path.clone();
        let Ok(bytes) = serde_json::to_vec_pretty(&*entries) else {
            return;
        };
        drop(entries);
        let _ = thread::Builder::new()
            .name("sysx-calibration-save".into())
            .spawn(move || {
                let temporary = path.with_extension("json.tmp");
                let result = std::fs::write(&temporary, bytes)
                    .and_then(|()| std::fs::rename(&temporary, &path));
                if let Err(error) = result {
                    eprintln!("Could not save motion calibration: {error}");
                }
            });
    }
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
/// every two seconds: whether calibration is on decides how fast the aim
/// tracks in a real game, which simulation cannot show.
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
        let calibration = match controller.calibration {
            Some(c) => format!(
                "on, {:.3} px/count, latency {:.0} ms, error {:.0}%{}",
                c.px_per_count,
                c.latency_s * 1000.0,
                controller.calibration_error * 100.0,
                if controller.calibration_loaded {
                    ", loaded"
                } else {
                    ""
                }
            ),
            None => format!(
                "off (uncalibrated slow path, evidence {:.0})",
                controller.calibrator.weight
            ),
        };
        let line = format!(
            "motion held {seconds:.1} s: {} frames ({:.0}/s), {} ticks, mean target error {:.1} px, {:.0} counts/s; calibration {calibration}",
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
        calibrations: Arc<CalibrationStore>,
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
                controller.load_calibration(calibrations.get(initial_sens));
                let mut sens = initial_sens;
                let mut saved: Option<(MotionCalibration, Instant)> = None;
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
                            controller.load_calibration(calibrations.get(runtime.sens));
                            sens = runtime.sens;
                            saved = None;
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
                        if let Some(measured) = controller.saveable_calibration() {
                            let due = saved.is_none_or(|(last, at)| {
                                at.elapsed() >= CALIBRATION_SAVE_INTERVAL
                                    && calibration_changed(last, measured)
                            });
                            if due {
                                calibrations.put(sens, measured);
                                saved = Some((measured, Instant::now()));
                            }
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
        let path = std::env::temp_dir().join(format!("sysx-unused-calibration-{}", unix_ms()));
        let ticker = MovementTicker::new(
            0.25,
            Arc::new(CalibrationStore::load(path)),
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
    fn uncalibrated_step_needs_fewer_counts_at_a_higher_sensitivity() {
        // One count turns yaw × sens degrees: twice the sens, half the counts.
        assert!(
            (uncalibrated_counts_per_px(0.5) * 2.0 - uncalibrated_counts_per_px(0.25)).abs()
                < 1e-12
        );
        assert!((uncalibrated_counts_per_px(0.25) - 0.014).abs() < 1e-12);
    }

    #[test]
    fn calibrated_correction_converts_image_speed_with_the_measured_px_per_count() {
        // Same error and shape, twice the px/count: half the counts.
        let step = |px_per_count: f64| {
            let mut controller = seeded(54);
            controller.acquiring = false;
            controller.calibration = Some(MotionCalibration {
                px_per_count,
                latency_s: 0.0,
            });
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
    fn held_trigger_summary_reports_frames_and_calibration_state() {
        let mut controller = seeded(1);
        let mut summary = MotionSummary::default();
        assert!(summary.finish(&controller).is_none(), "nothing gathered");
        for tick in 0..10u64 {
            assert!(summary.add(&controller, tick / 2, (2, -1)).is_none());
        }
        summary.started = Some(Instant::now() - Duration::from_secs(1));
        let line = summary.finish(&controller).expect("summary line");
        assert!(line.contains("5 frames"), "{line}");
        assert!(line.contains("10 ticks"), "{line}");
        assert!(line.contains("calibration off"), "{line}");
        controller.load_calibration(Some(MotionCalibration {
            px_per_count: 0.31,
            latency_s: 0.024,
        }));
        summary.add(&controller, 99, (0, 0));
        summary.started = Some(Instant::now() - Duration::from_secs(1));
        let line = summary.finish(&controller).unwrap();
        assert!(line.contains("on, 0.310 px/count, latency 24 ms"), "{line}");
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
        /// Constant target velocity along X (px/s), added to the strafe.
        drift: f64,
        /// Smoothing strength (1 = none), as set on the config page.
        smoothing: f64,
        /// Detection jitter amplitude (px, uniform ±).
        jitter: f64,
        /// Zoom (ADS/scope) from `.0` to `.1` seconds: px/count times `.2`.
        zoom: (f64, f64, f64),
    }

    impl World {
        fn px_per_count_at(&self, t: f64) -> f64 {
            let (from, to, factor) = self.zoom;
            if (from..to).contains(&t) {
                self.px_per_count * factor
            } else {
                self.px_per_count
            }
        }
    }

    const GAME: World = World {
        px_per_count: 0.25,
        latency_s: 0.024,
        fps: 240.0,
        strafe: 150.0,
        drift: 0.0,
        smoothing: 1.0,
        jitter: 0.7,
        zoom: (0.0, 0.0, 1.0),
    };

    fn simulate(aware: bool, seed: u64) -> (f64, Option<MotionCalibration>) {
        simulate_in(GAME, aware, seed)
    }

    fn simulate_in(world: World, aware: bool, seed: u64) -> (f64, Option<MotionCalibration>) {
        let (rms, calibration, _) = simulate_timed(world, aware, seed, None);
        (rms, calibration)
    }

    /// Also returns when calibration first switched on. `preset` starts the
    /// controller with a saved calibration.
    fn simulate_timed(
        world: World,
        aware: bool,
        seed: u64,
        preset: Option<MotionCalibration>,
    ) -> (f64, Option<MotionCalibration>, Option<f64>) {
        let run = simulate_run(world, aware, seed, preset);
        (run.rms, run.calibration, run.first_calibrated)
    }

    struct SimRun {
        /// RMS true error over the second half.
        rms: f64,
        /// RMS true error over the first second (before or while learning).
        early_rms: f64,
        /// Largest true error after the first 0.3 s (overshoot/instability).
        peak_after_start: f64,
        calibration: Option<MotionCalibration>,
        first_calibrated: Option<f64>,
        /// First time the true error fell under 4 px.
        settled_at: Option<f64>,
        /// Largest distance past the target along the initial error direction.
        overshoot: f64,
        /// RMS and peak true error while zoomed (0 without a zoom).
        zoom_rms: f64,
        zoom_peak: f64,
    }

    fn simulate_run(
        world: World,
        aware: bool,
        seed: u64,
        preset: Option<MotionCalibration>,
    ) -> SimRun {
        const SECONDS: f64 = 10.0;
        let frame_s = 1.0 / world.fps;
        let target = |t: f64| {
            (
                60.0 + world.strafe * (std::f64::consts::TAU * 0.8 * t).sin() + world.drift * t,
                -20.0 + world.strafe / 4.0 * (std::f64::consts::TAU * 0.3 * t).cos(),
            )
        };
        let mut controller = seeded(seed);
        controller.load_calibration(preset);
        let (mut early_squared, mut early_samples, mut peak) = (0.0, 0, 0.0_f64);
        let (mut settled_at, mut overshoot) = (None, 0.0_f64);
        let (mut zoom_squared, mut zoom_samples, mut zoom_peak) = (0.0, 0, 0.0_f64);
        let initial = target(0.0);
        let initial_direction = (
            initial.0 / initial.0.hypot(initial.1),
            initial.1 / initial.0.hypot(initial.1),
        );
        let mut first_calibrated = None;
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
                let mut jitter = || noise.random_range(-world.jitter..=world.jitter);
                let error = ((tx - cx + jitter()).trunc(), (ty - cy + jitter()).trunc());
                frame = Some(FrameError {
                    frame_id: next_frame,
                    received_s: captured + world.latency_s,
                    raw: error,
                    smoothing_alpha: (1.0 / world.smoothing, 1.0 / world.smoothing),
                });
                next_frame += 1;
            }
            let Some(current) = frame else { continue };
            let (dx, dy) = controller.track(current, now, aware);
            if first_calibrated.is_none() && controller.calibration.is_some() {
                first_calibrated = Some(now);
            }
            let (cx, cy) = camera.last().expect("camera").1;
            let k = world.px_per_count_at(now);
            camera.push((
                now,
                (
                    cx + dx.clamp(-32, 32) as f64 * k,
                    cy + dy.clamp(-32, 32) as f64 * k,
                ),
            ));
            {
                let (tx, ty) = target(now);
                let (cx, cy) = camera.last().expect("camera").1;
                let error = (tx - cx).hypot(ty - cy);
                if now <= 1.0 {
                    early_squared += error * error;
                    early_samples += 1;
                }
                if now >= 0.3 {
                    peak = peak.max(error);
                }
                if error < 4.0 && settled_at.is_none() {
                    settled_at = Some(now);
                }
                let along = (tx - cx) * initial_direction.0 + (ty - cy) * initial_direction.1;
                overshoot = overshoot.max(-along);
                if (world.zoom.0..world.zoom.1).contains(&now) {
                    zoom_squared += error * error;
                    zoom_samples += 1;
                    zoom_peak = zoom_peak.max(error);
                }
            }
            if now >= SECONDS / 2.0 {
                let (tx, ty) = target(now);
                let (cx, cy) = camera.last().expect("camera").1;
                squared += (tx - cx).powi(2) + (ty - cy).powi(2);
                samples += 1;
            }
        }
        SimRun {
            rms: (squared / samples as f64).sqrt(),
            early_rms: (early_squared / early_samples.max(1) as f64).sqrt(),
            peak_after_start: peak,
            calibration: controller.motion_calibration(),
            first_calibrated,
            settled_at,
            overshoot,
            zoom_rms: (zoom_squared / zoom_samples.max(1) as f64).sqrt(),
            zoom_peak,
        }
    }

    #[test]
    fn zooming_in_raises_px_per_count_before_the_loop_oscillates() {
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        let zoomed = |strafe: f64, factor: f64| {
            (600..603)
                .map(|seed| {
                    simulate_run(
                        World {
                            strafe,
                            zoom: (3.0, 6.0, factor),
                            ..GAME
                        },
                        true,
                        seed,
                        saved,
                    )
                })
                .collect::<Vec<_>>()
        };
        // A 4x zoom on a still target swung the aim about 115 px before.
        for run in zoomed(0.0, 4.0) {
            assert!(run.zoom_peak < 60.0, "peak {:.1} px", run.zoom_peak);
        }
        // Without a zoom a strafing target must not trigger a raise.
        for run in zoomed(150.0, 1.0) {
            assert!(
                run.calibration.is_some_and(|c| c.px_per_count < 0.4),
                "{:?}",
                run.calibration
            );
            assert!(run.zoom_rms < 16.0, "RMS {:.1} px", run.zoom_rms);
        }
    }

    /// ADS/scope zoom as a game shows it: the camera turns `base_k` unzoomed
    /// px per count, and the image magnifies the error, the target's motion
    /// and the effect of every count by the zoom factor. Zoom changes ramp
    /// linearly over 0.15 s (the ADS animation). A target outside the 256 px
    /// crop is lost (the controller resets, nothing is sent) until it returns.
    struct AdsScene {
        /// (time s, zoom factor) keyframes, linear in between.
        keys: Vec<(f64, f64)>,
        /// Strafe amplitude in unzoomed px (0.8 Hz), 0 = still.
        strafe: f64,
        seconds: f64,
        preset: Option<MotionCalibration>,
        guard: bool,
    }

    #[derive(Default)]
    struct AdsResult {
        /// Unzoomed (angular) error while zoomed, after each zoom-in settles.
        rms: f64,
        peak: f64,
        /// Share of zoomed time within 3 unzoomed px (a head) and lost.
        on_target: f64,
        lost: f64,
        /// Time from zoom-in until the error stays under 3 unzoomed px.
        settle_s: f64,
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

    fn simulate_ads(scene: &AdsScene, seed: u64) -> AdsResult {
        const BASE_K: f64 = 0.25;
        const LATENCY_S: f64 = 0.024;
        const FRAME_S: f64 = 1.0 / 240.0;
        const HALF_CROP: f64 = 128.0;
        const HEAD_PX: f64 = 3.0;
        let target = |t: f64| {
            (
                10.0 + scene.strafe * (std::f64::consts::TAU * 0.8 * t).sin(),
                -5.0 + scene.strafe / 4.0 * (std::f64::consts::TAU * 0.3 * t).cos(),
            )
        };
        let mut controller = seeded(seed);
        controller.zoom_guard_off = !scene.guard;
        controller.load_calibration(scene.preset);
        let mut noise = StdRng::seed_from_u64(seed ^ 0xad5);
        let mut camera = vec![(0.0_f64, (0.0_f64, 0.0_f64))];
        let camera_at = |camera: &[(f64, (f64, f64))], t: f64| {
            camera
                .iter()
                .rev()
                .find(|(time, _)| *time <= t)
                .map_or((0.0, 0.0), |(_, offset)| *offset)
        };
        // Zoom-in moments (start of each ramp up), for settle time and the
        // 0.25 s transient excluded from RMS/peak.
        let zoom_ins: Vec<f64> = std::iter::once(0.0)
            .chain(
                scene
                    .keys
                    .windows(2)
                    .filter(|pair| pair[1].1 > pair[0].1)
                    .map(|pair| pair[0].0),
            )
            .collect();
        let mut frame: Option<FrameError> = None;
        let mut visible = true;
        let mut next_frame = 0_u64;
        let (mut squared, mut counted, mut peak) = (0.0, 0, 0.0_f64);
        let (mut zoomed, mut on_target, mut lost) = (0, 0, 0);
        let mut settle: Vec<Option<f64>> = vec![None; zoom_ins.len()];
        let mut last_off_head: Vec<f64> = zoom_ins.clone();
        let ticks = (scene.seconds / DEFAULT_TICK_S) as usize;
        for tick in 1..=ticks {
            let now = tick as f64 * DEFAULT_TICK_S;
            while (next_frame as f64) * FRAME_S + LATENCY_S <= now {
                let captured = next_frame as f64 * FRAME_S;
                let zoom = zoom_at(&scene.keys, captured);
                let (tx, ty) = target(captured);
                let (cx, cy) = camera_at(&camera, captured);
                let mut jitter = || noise.random_range(-0.7..=0.7);
                let image = ((tx - cx) * zoom + jitter(), (ty - cy) * zoom + jitter());
                visible = image.0.abs() <= HALF_CROP && image.1.abs() <= HALF_CROP;
                frame = visible.then_some(FrameError {
                    frame_id: next_frame,
                    received_s: captured + LATENCY_S,
                    raw: (image.0.trunc(), image.1.trunc()),
                    smoothing_alpha: (1.0, 1.0),
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
            let (cx, cy) = camera.last().expect("camera").1;
            camera.push((
                now,
                (
                    cx + dx.clamp(-32, 32) as f64 * BASE_K,
                    cy + dy.clamp(-32, 32) as f64 * BASE_K,
                ),
            ));
            let zoom = zoom_at(&scene.keys, now);
            if zoom < 1.01 && scene.keys.iter().any(|(_, z)| *z > 1.01) {
                continue;
            }
            let (tx, ty) = target(now);
            let (cx, cy) = camera.last().expect("camera").1;
            let error = (tx - cx).hypot(ty - cy);
            zoomed += 1;
            if !visible {
                lost += 1;
            }
            if error < HEAD_PX {
                on_target += 1;
            }
            let index = zoom_ins
                .iter()
                .rposition(|start| *start <= now)
                .unwrap_or(0);
            if error >= HEAD_PX {
                last_off_head[index] = now;
            }
            settle[index] = Some(last_off_head[index] - zoom_ins[index]);
            if now - zoom_ins[index] >= 0.25 {
                squared += error * error;
                counted += 1;
                peak = peak.max(error);
            }
        }
        let settles: Vec<f64> = settle.iter().flatten().copied().collect();
        AdsResult {
            rms: (squared / counted.max(1) as f64).sqrt(),
            peak,
            on_target: on_target as f64 / zoomed.max(1) as f64,
            lost: lost as f64 / zoomed.max(1) as f64,
            settle_s: settles.iter().sum::<f64>() / settles.len().max(1) as f64,
        }
    }

    /// ADS from the start and toggled while firing, at small (1.15–1.5×,
    /// below the 2× raise) and large (2–5×) zooms (not a pass/fail test):
    /// `cargo test --release --lib -- ads_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ads_report() {
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        let toggled = |z: f64| {
            vec![
                (0.0, 1.0),
                (1.0, 1.0),
                (1.15, z),
                (2.4, z),
                (2.55, 1.0),
                (3.3, 1.0),
                (3.45, z),
                (5.0, z),
                (5.15, 1.0),
                (6.0, 1.0),
                (6.15, z),
                (8.0, z),
            ]
        };
        let scenes: [(
            &str,
            Box<dyn Fn(f64) -> Vec<(f64, f64)>>,
            f64,
            Option<MotionCalibration>,
        ); 3] = [
            (
                "ADS from start, hip-fire calibration saved",
                Box::new(|z| vec![(0.0, z)]),
                6.0,
                saved,
            ),
            (
                "ADS from start, no saved calibration",
                Box::new(|z| vec![(0.0, z)]),
                6.0,
                None,
            ),
            (
                "ADS toggled while firing (in 1.0 s, out 2.4 s, in 3.3 s, out 5.0 s, in 6.0 s)",
                Box::new(toggled),
                8.0,
                saved,
            ),
        ];
        for (title, keys, seconds, preset) in &scenes {
            for (target, strafe) in [("still", 0.0), ("strafe 20 px 0.8 Hz", 20.0)] {
                eprintln!("ADS {title} | {target}");
                eprintln!("ADS   zoom | guard ON: RMS  peak  head%  lost%  settle | guard OFF: RMS  peak  head%  lost%  settle");
                for zoom in [1.0, 1.15, 1.25, 1.5, 2.0, 2.5, 3.5, 5.0] {
                    let mut line = format!("ADS   {zoom:>4}x");
                    for guard in [true, false] {
                        let runs: Vec<AdsResult> = (700..710)
                            .map(|seed| {
                                simulate_ads(
                                    &AdsScene {
                                        keys: keys(zoom),
                                        strafe,
                                        seconds: *seconds,
                                        preset: *preset,
                                        guard,
                                    },
                                    seed,
                                )
                            })
                            .collect();
                        let mean = |f: fn(&AdsResult) -> f64| {
                            runs.iter().map(f).sum::<f64>() / runs.len() as f64
                        };
                        line += &format!(
                            " | {:>5.1} {:>5.1} {:>5.0}% {:>5.1}% {:>5.2}s",
                            mean(|r| r.rms),
                            mean(|r| r.peak),
                            mean(|r| r.on_target) * 100.0,
                            mean(|r| r.lost) * 100.0,
                            mean(|r| r.settle_s),
                        );
                    }
                    eprintln!("{line}");
                }
            }
        }
    }

    /// Zooming in (ADS/scope) multiplies px/count while a saved calibration is
    /// active (not a pass/fail test):
    /// `cargo test --release --lib -- zoom_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn zoom_report() {
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        for factor in [1.0, 1.5, 2.0, 3.0, 4.0, 0.5] {
            for (name, strafe) in [("strafe", 150.0), ("still", 0.0)] {
                let runs: Vec<SimRun> = (600..610)
                    .map(|seed| {
                        simulate_run(
                            World {
                                strafe,
                                zoom: (3.0, 6.0, factor),
                                ..GAME
                            },
                            true,
                            seed,
                            saved,
                        )
                    })
                    .collect();
                let mean =
                    |f: fn(&SimRun) -> f64| runs.iter().map(f).sum::<f64>() / runs.len() as f64;
                eprintln!(
                    "ZOOM x{factor} {name}: zoomed RMS {:.1} px, zoomed peak {:.1} px, after-zoom RMS {:.1} px, px/count at end {:.3}",
                    mean(|run| run.zoom_rms),
                    mean(|run| run.zoom_peak),
                    mean(|run| run.rms),
                    mean(|run| run.calibration.map_or(0.0, |c| c.px_per_count)),
                );
            }
        }
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

    /// Start-up behaviour across games, from no calibration:
    /// `cargo test --release --lib -- startup_report --ignored --nocapture`.
    /// Effect of the smoothing strength (saved calibration, 240 FPS):
    /// `cargo test --release --lib -- smoothing_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoothing_report() {
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        for smoothing in [1.0, 5.0, 10.0, 20.0, 35.0, 50.0] {
            let still = simulate_run(
                World {
                    strafe: 0.0,
                    smoothing,
                    ..GAME
                },
                true,
                500,
                saved,
            );
            let strafe = (500..505)
                .map(|seed| simulate_run(World { smoothing, ..GAME }, true, seed, saved).rms)
                .sum::<f64>()
                / 5.0;
            let shaky = simulate_run(
                World {
                    strafe: 0.0,
                    smoothing,
                    jitter: 3.0,
                    ..GAME
                },
                true,
                500,
                saved,
            );
            eprintln!(
                "SMOOTH strength {smoothing}: still target 63 px reached after {:.2}s, overshoot {:.1} px | strafing RMS {strafe:.1} px | aim wobble on a ±3 px jittery still target {:.2} px RMS",
                still.settled_at.unwrap_or(f64::INFINITY),
                still.overshoot,
                shaky.rms
            );
        }
    }

    #[test]
    #[ignore]
    fn startup_report() {
        let worlds = [
            (
                "k0.05",
                World {
                    px_per_count: 0.05,
                    ..GAME
                },
            ),
            ("k0.25", GAME),
            (
                "k0.25 still",
                World {
                    strafe: 0.0,
                    ..GAME
                },
            ),
            (
                "k0.6",
                World {
                    px_per_count: 0.6,
                    ..GAME
                },
            ),
            (
                "k2",
                World {
                    px_per_count: 2.0,
                    ..GAME
                },
            ),
            (
                "k4",
                World {
                    px_per_count: 4.0,
                    ..GAME
                },
            ),
            (
                "k12",
                World {
                    px_per_count: 12.0,
                    ..GAME
                },
            ),
            (
                "lat50",
                World {
                    latency_s: 0.05,
                    ..GAME
                },
            ),
            (
                "60fps",
                World {
                    fps: 60.0,
                    latency_s: 0.04,
                    ..GAME
                },
            ),
        ];
        for (name, world) in worlds {
            let (mut early, mut late, mut peak, mut first) = (0.0, 0.0, 0.0_f64, Vec::new());
            for seed in 400..410 {
                let run = simulate_run(world, true, seed, None);
                early += run.early_rms / 10.0;
                late += run.rms / 10.0;
                peak = peak.max(run.peak_after_start);
                first.push(run.first_calibrated.unwrap_or(f64::INFINITY));
            }
            first.sort_by(f64::total_cmp);
            let saved = Some(MotionCalibration {
                px_per_count: world.px_per_count,
                latency_s: world.latency_s,
            });
            let preset = simulate_run(world, true, 400, saved);
            let still = simulate_run(
                World {
                    strafe: 0.0,
                    ..world
                },
                true,
                400,
                None,
            );
            let still_saved = simulate_run(
                World {
                    strafe: 0.0,
                    ..world
                },
                true,
                400,
                saved,
            );
            eprintln!(
                "SETTLE {name}: still target 63 px away reached 4 px after {:.2}s (saved {:.2}s), overshoot {:.1} px (saved {:.1})",
                still.settled_at.unwrap_or(f64::INFINITY),
                still_saved.settled_at.unwrap_or(f64::INFINITY),
                still.overshoot,
                still_saved.overshoot
            );
            eprintln!(
                "STARTUP {name}: on after median {:.2}s max {:.2}s | first 1 s RMS {early:.0} px (saved file: {:.0}) | later {late:.0} px | peak {peak:.0} px",
                first[5], first[9], preset.early_rms
            );
        }
    }

    /// `cargo test --release --lib -- calibration_timing_report --ignored --nocapture`.
    #[test]
    #[ignore]
    fn calibration_timing_report() {
        for (name, strafe) in [
            ("60 px/s", 12.7),
            ("240 px/s", 50.9),
            ("480 px/s", 101.9),
            ("750 px/s", 150.0),
        ] {
            let mut times = Vec::new();
            let mut worst: f64 = 0.0;
            for seed in 300..330 {
                let (_, calibration, first) =
                    simulate_timed(World { strafe, ..GAME }, true, seed, None);
                times.push(first.unwrap_or(f64::INFINITY));
                if let Some(c) = calibration {
                    worst = worst.max((c.px_per_count - 0.25).abs() / 0.25);
                }
            }
            times.sort_by(f64::total_cmp);
            eprintln!(
                "TIMING {name}: first calibrated median {:.2}s p90 {:.2}s max {:.2}s; worst final px/count error {:.0}%",
                times[15], times[27], times[29], worst * 100.0
            );
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
    fn sub_pixel_probes_are_no_worse_on_average_and_a_still_target_stays_steady() {
        // ±2 counts move 0.1 px here: below detection, so the estimate is rough
        // (biased high, the safe side). On average no worse, and never unstable.
        let fine = World {
            px_per_count: 0.05,
            ..GAME
        };
        let (mut aware, mut blind) = (0.0, 0.0);
        for seed in 41..47 {
            let run = simulate_run(fine, true, seed, None);
            assert!(run.peak_after_start < 400.0, "{}", run.peak_after_start);
            aware += run.rms;
            blind += simulate_in(fine, false, seed).0;
        }
        assert!(
            aware <= blind * 1.1,
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
    fn fast_tracking_starts_within_a_fraction_of_a_second_and_stays_stable() {
        for world in [
            GAME,
            World {
                px_per_count: 2.0,
                ..GAME
            },
            World {
                px_per_count: 12.0,
                ..GAME
            },
            World {
                fps: 60.0,
                latency_s: 0.04,
                ..GAME
            },
        ] {
            let run = simulate_run(world, true, 61, None);
            let on = run.first_calibrated.expect("calibrated");
            assert!(on <= 0.3, "on after {on:.2} s");
            // Far below the runaway errors of an under-estimated px/count.
            assert!(run.peak_after_start < 300.0, "{}", run.peak_after_start);
        }
    }

    #[test]
    fn a_still_target_is_reached_quickly_without_overshoot() {
        let still = World {
            strafe: 0.0,
            ..GAME
        };
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        // 63 px away; with a saved calibration the aim is there in ~0.3 s.
        let run = simulate_run(still, true, 81, saved);
        let reached = run.settled_at.expect("reached the target");
        assert!(reached <= 0.4, "reached after {reached:.2} s");
        assert!(run.overshoot <= 8.0, "overshoot {:.1} px", run.overshoot);
        // Learning from scratch is a little slower but still overshoots little.
        let run = simulate_run(still, true, 81, None);
        assert!(
            run.settled_at.is_some_and(|at| at <= 0.8),
            "{:?}",
            run.settled_at
        );
        assert!(run.overshoot <= 8.0, "overshoot {:.1} px", run.overshoot);
    }

    #[test]
    fn a_wrong_first_latency_guess_is_corrected_instead_of_oscillating() {
        // At 50 ms the first rough estimate tends to pick the wrong latency;
        // caution and continuous updates keep the aim from swinging.
        let slow = World {
            strafe: 0.0,
            latency_s: 0.05,
            ..GAME
        };
        for seed in [400, 401, 402] {
            let run = simulate_run(slow, true, seed, None);
            assert!(run.overshoot <= 10.0, "overshoot {:.1} px", run.overshoot);
            let calibration = run.calibration.expect("calibrated");
            assert!(
                (calibration.latency_s - 0.05).abs() <= 0.0041,
                "{calibration:?}"
            );
        }
    }

    #[test]
    fn coarse_counts_do_not_hunt_around_the_target() {
        // One count is 12 px: the dead zone grows to about a count, so the aim
        // stops next to the target instead of hopping across it.
        let coarse = World {
            px_per_count: 12.0,
            strafe: 0.0,
            ..GAME
        };
        let run = simulate_run(coarse, true, 91, None);
        assert!(run.rms < 12.0, "RMS {:.1} px", run.rms);
        assert!(run.overshoot <= 13.0, "overshoot {:.1} px", run.overshoot);
    }

    #[test]
    fn a_saved_calibration_tracks_from_the_first_frame() {
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        let (mut fresh, mut loaded) = (0.0, 0.0);
        for seed in 71..74 {
            fresh += simulate_run(GAME, true, seed, None).early_rms;
            let run = simulate_run(GAME, true, seed, saved);
            // On from the first received frame (it arrives one latency in).
            assert!(run.first_calibrated.is_some_and(|on| on <= 0.03));
            loaded += run.early_rms;
        }
        // A fresh start learns within ~0.1 s, so the saved file's edge is that
        // first tenth of a second.
        assert!(
            loaded < fresh * 0.8,
            "loaded {loaded:.0} vs fresh {fresh:.0}"
        );
    }

    #[test]
    fn calibrations_are_saved_per_sensitivity_and_reloaded() {
        let directory = std::env::temp_dir().join(format!(
            "sysx-calibration-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("motion-calibration.json");
        let calibration = MotionCalibration {
            px_per_count: 0.31,
            latency_s: 0.028,
        };
        let store = CalibrationStore::load(&path);
        assert_eq!(store.get(0.25), None);
        store.put(0.25, calibration);
        assert_eq!(store.get(0.25), Some(calibration));
        assert_eq!(store.get(0.5), None, "another sensitivity");
        // Written on a background thread.
        let deadline = Instant::now() + Duration::from_secs(5);
        while CalibrationStore::load(&path).get(0.25).is_none() {
            assert!(
                Instant::now() < deadline,
                "calibration file was not written"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // A damaged or out-of-range file is ignored, not trusted.
        std::fs::write(
            &path,
            b"[{\"sens\":0.25,\"px_per_count\":-1,\"latency_s\":0}]",
        )
        .unwrap();
        assert_eq!(CalibrationStore::load(&path).get(0.25), None);
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(CalibrationStore::load(&path).get(0.25), None);
        let _ = std::fs::remove_dir_all(directory);
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
        // A target crossing at a steady 200 px/s: once on it, feedforward keeps
        // turning with it instead of stopping in the dead zone and chasing.
        let crossing = World {
            strafe: 0.0,
            drift: 200.0,
            ..GAME
        };
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        let run = simulate_run(crossing, true, 52, saved);
        assert!(run.rms < 6.0, "RMS {:.1} px", run.rms);
    }

    #[test]
    fn strong_smoothing_does_not_make_the_aim_overshoot() {
        // Smoothing filters the target's own motion, not the image motion our
        // moves cause, so even the strongest setting does not fling the aim.
        let saved = Some(MotionCalibration {
            px_per_count: 0.25,
            latency_s: 0.024,
        });
        for smoothing in [1.0, 10.0, 50.0] {
            let still = World {
                strafe: 0.0,
                smoothing,
                ..GAME
            };
            let run = simulate_run(still, true, 83, saved);
            assert!(
                run.overshoot <= 6.0,
                "strength {smoothing}: overshoot {:.1} px",
                run.overshoot
            );
            let reached = run.settled_at.expect("reached");
            assert!(
                reached <= 0.4,
                "strength {smoothing}: reached after {reached:.2} s"
            );
        }
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
