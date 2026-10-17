//! Optional upstream ABCurves Renderer after the unchanged Normal controller.
//! Numerical 1 ms reports are aggregated into SysX's existing output batches;
//! this is NOT a physical 1 kHz output path or the Continuous Planner.
use crate::config::MotionRendererSettings;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

static MODEL: &[u8] = include_bytes!("../../../third-party/abcurves/renderer_global_h80.bin");
extern "C" {
    fn sysx_abc_prepare(
        blob: *const c_void,
        bytes: usize,
        reports: *const i16,
        count: usize,
    ) -> *mut c_void;
    fn sysx_abc_begin(profile: *const c_void, seed: u64) -> *mut c_void;
    fn sysx_abc_step(event: *mut c_void, x: i32, y: i32, out: *mut i16) -> i32;
    fn sysx_abc_free(value: *mut c_void);
}

pub struct Profile {
    raw: NonNull<c_void>,
    fault: AtomicI32,
}
impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ABCurvesProfile")
    }
}
// C preparation freezes this allocation. Event creation only reads it and copies
// into an independently owned mutable renderer. MODEL has static lifetime.
unsafe impl Send for Profile {}
unsafe impl Sync for Profile {}
impl Drop for Profile {
    fn drop(&mut self) {
        unsafe { sysx_abc_free(self.raw.as_ptr()) }
    }
}
impl Profile {
    pub fn error(&self) -> Option<String> {
        let code = self.fault.load(Ordering::Relaxed);
        (code != 0).then(|| format!("ABCurves output stopped (status {code}). Choose Normal or reapply a valid mouse profile."))
    }

    pub fn prepare(settings: &MotionRendererSettings) -> Result<Arc<Self>, String> {
        settings.validate().map_err(str::to_owned)?;
        if settings.reports.len() != 256 {
            return Err("ABCurves profile is missing.".into());
        }
        // [i16;2] arrays are contiguous and remain borrowed for this synchronous call.
        let raw = unsafe {
            sysx_abc_prepare(
                MODEL.as_ptr().cast(),
                MODEL.len(),
                settings.reports.as_ptr().cast(),
                settings.reports.len(),
            )
        };
        NonNull::new(raw)
            .map(|raw| {
                Arc::new(Self {
                    raw,
                    fault: AtomicI32::new(0),
                })
            })
            .ok_or_else(|| "ABCurves rejected the model or physical report profile.".into())
    }
    fn begin(self: &Arc<Self>, seed: u64) -> Result<Stream, i32> {
        let raw = unsafe { sysx_abc_begin(self.raw.as_ptr(), seed) };
        NonNull::new(raw)
            .map(|raw| Stream {
                raw,
                _profile: Arc::clone(self),
            })
            .ok_or(-1)
    }
}
struct Stream {
    raw: NonNull<c_void>,
    _profile: Arc<Profile>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { sysx_abc_free(self.raw.as_ptr()) }
    }
}
impl Stream {
    fn step(&mut self, intent: (i32, i32)) -> Result<(i64, i64), i32> {
        let mut report = [0i16; 2];
        let status =
            unsafe { sysx_abc_step(self.raw.as_ptr(), intent.0, intent.1, report.as_mut_ptr()) };
        if status == 0 {
            Ok((i64::from(report[0]), i64::from(report[1])))
        } else {
            Err(status)
        }
    }
}

/// Prepared off the ticker. A trigger transition reuses the immutable profile;
/// only a changed profile/settings value performs the 256-observation preparation.
pub struct ProfileCache {
    settings: MotionRendererSettings,
    pub profile: Option<Arc<Profile>>,
}
impl ProfileCache {
    pub fn new(settings: &MotionRendererSettings) -> Result<Self, String> {
        settings.validate().map_err(str::to_owned)?;
        let profile = if settings.enabled && settings.reports.len() == 256 {
            Some(Profile::prepare(settings)?)
        } else {
            None
        };
        Ok(Self {
            settings: settings.clone(),
            profile,
        })
    }
    pub fn configure(&mut self, settings: &MotionRendererSettings) -> Result<(), String> {
        if self.settings != *settings {
            *self = Self::new(settings)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MotionOutput {
    profile: Option<Arc<Profile>>,
    stream: Option<Stream>,
    clock: Option<Instant>,
    pending: (i64, i64),
    remaining: i64,
    failed: bool,
}
impl MotionOutput {
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    pub fn reset(&mut self) {
        if let Some(profile) = &self.profile {
            profile.fault.store(0, Ordering::Relaxed);
        }
        *self = Self::default();
    }

    pub fn filter(
        &mut self,
        profile: Option<&Arc<Profile>>,
        normal: (i64, i64),
        now: Instant,
    ) -> Result<(i64, i64), i32> {
        // Hard bypass: no model call, no quantization, no changed counts or clocks.
        let Some(profile) = profile else {
            self.reset();
            return Ok(normal);
        };
        if self
            .profile
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, profile))
        {
            self.reset();
            self.profile = Some(Arc::clone(profile));
        }
        if self.failed {
            return Ok((0, 0));
        }
        match self.advance(profile, normal, now) {
            Ok(output) => Ok(output),
            Err(status) => {
                profile.fault.store(status, Ordering::Relaxed);
                self.stream = None;
                self.pending = (0, 0);
                self.remaining = 0;
                self.failed = true; // no silent fallback or replay after an error
                Err(status)
            }
        }
    }

    fn advance(
        &mut self,
        profile: &Arc<Profile>,
        normal: (i64, i64),
        now: Instant,
    ) -> Result<(i64, i64), i32> {
        let elapsed = self
            .clock
            .map_or(0, |clock| now.saturating_duration_since(clock).as_millis());
        if self.stream.is_none() || elapsed > 8 {
            // Never replay a delayed backlog in a burst after a scheduling stall.
            self.stream = Some(profile.begin(rand::random())?);
            self.clock = Some(now);
            self.pending = (0, 0);
            self.remaining = 0;
        }
        let ticks = if elapsed > 8 { 0 } else { elapsed as u32 };
        let mut output = (0, 0);
        for _ in 0..ticks {
            let intent = if self.remaining > 0 {
                let intent = (
                    self.pending.0 / self.remaining,
                    self.pending.1 / self.remaining,
                );
                self.pending.0 -= intent.0;
                self.pending.1 -= intent.1;
                self.remaining -= 1;
                (
                    i32::try_from(intent.0).map_err(|_| -7)?,
                    i32::try_from(intent.1).map_err(|_| -7)?,
                )
            } else {
                (0, 0)
            };
            let report = self.stream.as_mut().ok_or(-1)?.step(intent)?;
            output.0 += report.0;
            output.1 += report.1;
        }
        if ticks > 0 {
            self.clock = self
                .clock
                .map(|clock| clock + Duration::from_millis(u64::from(ticks)));
        }
        // Today's observation cannot affect already closed numerical intervals.
        // Spread each Normal frame's counts over the next four 1 ms intervals.
        if normal != (0, 0) {
            let add = |pending: i64, counts: i64| {
                counts
                    .checked_mul(65536)
                    .and_then(|v| pending.checked_add(v))
                    .filter(|v| v.unsigned_abs() <= (i32::MAX as u64) * 4)
                    .ok_or(-7)
            };
            self.pending = (
                add(self.pending.0, normal.0)?,
                add(self.pending.1, normal.1)?,
            );
            self.remaining = 4;
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn synthetic_profile() -> Arc<Profile> {
        // Synthetic input ONLY for numerical tests, never installed as a user profile.
        Profile::prepare(&MotionRendererSettings {
            enabled: true,
            reports: (0..256)
                .map(|i| [((i % 9) as i16) - 4, ((i % 5) as i16) - 2])
                .collect(),
            ..MotionRendererSettings::default()
        })
        .unwrap()
    }
    #[test]
    fn binding_matches_upstreams_frozen_sequence_with_y_conversion() {
        // Upstream runtime/c/tests/test_loader.c, seed 123. Synthetic parity
        // fixture only; not a physical user profile or production preset.
        let profile = Profile::prepare(&MotionRendererSettings {
            enabled: true,
            reports: (0..256).map(|i| [(i & 1) as i16, 0]).collect(),
            ..MotionRendererSettings::default()
        })
        .unwrap();
        let mut stream = profile.begin(123).unwrap();
        let expected = [
            (1, 0),
            (1, 1),
            (1, 0),
            (0, 0),
            (2, 2),
            (1, 0),
            (1, 0),
            (1, 1),
            (1, 0),
            (1, 1),
            (1, 0),
            (1, 1),
            (1, 0),
            (1, 1),
            (1, 0),
            (1, 1),
        ];
        for (x, y_up) in expected {
            assert_eq!(stream.step((65536, -32768)).unwrap(), (x, -y_up));
        }
    }

    #[test]
    fn optional_texture_still_closes_a_synthetic_feedback_loop() {
        use crate::movement::{
            MotionParams, MovementController, MovementTargetSample, DEFAULT_SPEED,
        };
        // Mathematical loop only: no OBS, device or game claims. Nominal conversion
        // at sensitivity .25 is .25 image px/count. Include delayed visual feedback.
        for latency_ms in [16, 30, 50] {
            let profile = synthetic_profile();
            let start = Instant::now();
            let mut controller = MovementController::new(0.25);
            let mut output = MotionOutput::default();
            let mut position = 0.0f64;
            let mut positions = vec![0.0f64];
            let mut peak = 0.0f64;
            for tick in 0..750usize {
                let delayed = positions[tick.saturating_sub(latency_ms / 4)];
                let error = 60.0 - delayed;
                let sample = MovementTargetSample {
                    config_revision: 0,
                    frame_id: tick as u64,
                    target: Some((92.0 - delayed, 32.0)),
                    smoothing_alpha: (1.0, 1.0),
                    crop_origin: (0, 0),
                    processed_size: (64, 64),
                    received_s: tick as f64 * 0.004,
                    age_s: 0.0,
                };
                let normal = controller.delta(
                    &sample,
                    MotionParams {
                        offsets: (0.0, 0.0),
                        speed: (DEFAULT_SPEED, DEFAULT_SPEED),
                        smooth_fov_degrees: 20,
                    },
                );
                let report = if error.abs() <= 1.0 {
                    output.reset();
                    (0, 0)
                } else {
                    output
                        .filter(
                            Some(&profile),
                            normal,
                            start + Duration::from_millis(tick as u64 * 4),
                        )
                        .unwrap()
                };
                position += report.0.clamp(-32, 32) as f64 * 0.25;
                peak = peak.max(position);
                positions.push(position);
            }
            assert!(
                (position - 60.0).abs() < 8.0,
                "latency {latency_ms}: end {position}"
            );
            assert!(peak < 85.0, "latency {latency_ms}: peak {peak}");
        }
    }

    #[test]
    fn disabled_is_exact_identity_for_every_count_and_timing() {
        let mut output = MotionOutput::default();
        let now = Instant::now();
        for dt in [0, 1, 4, 8, 100] {
            for input in [(0, 0), (12, -7), (i64::MAX, i64::MIN), (-33, 32)] {
                assert_eq!(
                    output
                        .filter(None, input, now + Duration::from_millis(dt))
                        .unwrap(),
                    input
                );
            }
        }
    }
    #[test]
    fn renderer_is_causal_and_reset_discards_all_pending_intent() {
        let profile = synthetic_profile();
        let now = Instant::now();
        let mut output = MotionOutput::default();
        assert_eq!(
            output.filter(Some(&profile), (12, -7), now).unwrap(),
            (0, 0)
        );
        assert_eq!(output.filter(Some(&profile), (0, 0), now).unwrap(), (0, 0));
        assert_eq!(output.pending, (12 * 65536, -7 * 65536));
        output
            .filter(Some(&profile), (0, 0), now + Duration::from_millis(4))
            .unwrap();
        assert_eq!(output.pending, (0, 0));
        output.reset();
        assert!(output.stream.is_none());
        assert_eq!(output.pending, (0, 0));
        assert_eq!(
            output
                .filter(Some(&profile), (0, 0), now + Duration::from_millis(5))
                .unwrap(),
            (0, 0)
        );
    }
    #[test]
    fn stalls_and_errors_cannot_replay_old_motion() {
        let profile = synthetic_profile();
        let now = Instant::now();
        let mut output = MotionOutput::default();
        output.filter(Some(&profile), (100, 100), now).unwrap();
        assert_eq!(
            output
                .filter(Some(&profile), (0, 0), now + Duration::from_millis(50))
                .unwrap(),
            (0, 0)
        );
        assert_eq!(output.pending, (0, 0));
        assert!(output
            .filter(
                Some(&profile),
                (i64::MAX, 0),
                now + Duration::from_millis(51)
            )
            .is_err());
        assert_eq!(
            output
                .filter(Some(&profile), (10, 0), now + Duration::from_millis(55))
                .unwrap(),
            (0, 0)
        );
        output.reset();
        assert!(!output.failed);
    }
    #[test]
    fn event_states_do_not_mutate_the_reusable_profile() {
        let profile = synthetic_profile();
        let mut a = profile.begin(7).unwrap();
        let mut b = profile.begin(7).unwrap();
        for _ in 0..32 {
            assert_eq!(
                a.step((65536, -32768)).unwrap(),
                b.step((65536, -32768)).unwrap()
            );
        }
        let mut fresh = profile.begin(7).unwrap();
        let mut reference = synthetic_profile().begin(7).unwrap();
        assert_eq!(
            fresh.step((65536, 0)).unwrap(),
            reference.step((65536, 0)).unwrap()
        );
    }
}
