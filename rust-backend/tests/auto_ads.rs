//! Auto under ADS/scope zoom, with the real Pixel Seek mask, center-cell gate
//! and host Auto state machine (simulation only, no device or game).
//!
//! `cargo test --release --test auto_ads -- --ignored --nocapture`

#[allow(dead_code)]
#[path = "../../src-tauri/src/auto.rs"]
mod host_auto;

use host_auto::{AutoAction, AutoState, AutoTiming};
use image::{Rgb, RgbImage};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::time::{Duration, Instant};
use sysx_backend::auto_target::has_target_at_center;
use sysx_backend::pixel_seek::{color_mask, PixelTargetColor};

/// Only the pixels around the crosshair matter to the center gate; an even
/// 32 px image has the same 2 × 2 center cells as the 256 px crop.
const SIZE: u32 = 32;
const FPS: f64 = 240.0;
/// Capture to backend (OBS encode, UDP, decode).
const RECEIVE_S: f64 = 0.016;
/// Host loop period, and click to the game registering it.
const HOST_TICK_S: f64 = 0.004;
const CLICK_S: f64 = 0.008;
const BACKGROUND: [f64; 3] = [70.0, 78.0, 88.0];
const YELLOW: [f64; 3] = [255.0, 255.0, 0.0];

#[derive(Clone, Copy)]
enum Look {
    /// Body filled with the target color; its width scales with the zoom.
    Solid,
    /// Only a highlight outline of fixed screen thickness (px) at the body
    /// edges, as games draw it: the band does not grow with the zoom.
    Outline(f64),
}

struct Scene {
    keys: Vec<(f64, f64)>,
    look: Look,
    /// Hitbox width in unzoomed px.
    body: f64,
    /// Strafe: amplitude (unzoomed px) and frequency; the crosshair stays still.
    amplitude: f64,
    hz: f64,
    seconds: f64,
    timing: AutoTiming,
}

#[derive(Default)]
struct Outcome {
    crossings: usize,
    /// Crossings that got at least one click landing on the body.
    crossings_hit: usize,
    shots: usize,
    hits: usize,
    /// Body entry to the first landed hit, per hit crossing.
    reaction_s: Vec<f64>,
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

/// Overlap of pixel column `[x, x + 1)` with `[from, to)`.
fn coverage(x: f64, from: f64, to: f64) -> f64 {
    (to.min(x + 1.0) - from.max(x)).clamp(0.0, 1.0)
}

/// Renders the frame around the crosshair: target offset `center` (image px,
/// + right) and zoom, with the color blended by pixel coverage plus noise.
fn render(scene: &Scene, center: f64, zoom: f64, rng: &mut StdRng) -> RgbImage {
    let half = scene.body * zoom / 2.0;
    let (left, right) = (center - half, center + half);
    let mid = SIZE as f64 / 2.0;
    RgbImage::from_fn(SIZE, SIZE, |x, _| {
        let column = x as f64 - mid;
        let c = match scene.look {
            Look::Solid => coverage(column, left, right),
            Look::Outline(band) => (coverage(column, left, left + band)
                + coverage(column, right - band, right))
            .min(1.0),
        };
        let mut pixel = [0u8; 3];
        for (channel, value) in pixel.iter_mut().enumerate() {
            let blended = c * YELLOW[channel] + (1.0 - c) * BACKGROUND[channel];
            *value = (blended + rng.random_range(-2.0..=2.0))
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        Rgb(pixel)
    })
}

fn simulate(scene: &Scene, seed: u64) -> Outcome {
    let mut rng = StdRng::seed_from_u64(seed);
    let phase = rng.random_range(0.0..std::f64::consts::TAU);
    let target = |t: f64| scene.amplitude * (std::f64::consts::TAU * scene.hz * t + phase).sin();
    let on_body = |t: f64| target(t).abs() <= scene.body / 2.0;
    let start = Instant::now();
    let mut auto = AutoState::new();
    auto.set_enabled(true);
    auto.set_timing(scene.timing);

    // Center-gate result per frame, visible to the host from `received`.
    let frame_s = 1.0 / FPS;
    let frames = (scene.seconds * FPS) as usize;
    let gate: Vec<(f64, bool)> = (0..frames)
        .map(|n| {
            let captured = n as f64 * frame_s;
            let zoom = zoom_at(&scene.keys, captured);
            let image = render(scene, target(captured) * zoom, zoom, &mut rng);
            let mask = color_mask(&image, PixelTargetColor::Yellow);
            (
                captured + RECEIVE_S,
                has_target_at_center(&mask, SIZE, SIZE, 36),
            )
        })
        .collect();

    let mut outcome = Outcome::default();
    let mut landed = Vec::new();
    let mut latest = None;
    let mut next = 0;
    let mut t = 0.0;
    while t < scene.seconds {
        while next < gate.len() && gate[next].0 <= t {
            latest = Some(gate[next].1);
            next += 1;
        }
        let valid = latest.unwrap_or(false);
        if auto.update(true, valid, start + Duration::from_secs_f64(t))
            == Some(AutoAction::LeftClick)
        {
            landed.push(t + CLICK_S);
        }
        t += HOST_TICK_S;
    }

    // Body crossings (entry times, measured from the first full second so
    // every crossing has had time for a click to land).
    let step = 0.0005;
    let mut entries = Vec::new();
    let mut inside = on_body(0.0);
    let mut time = step;
    while time < scene.seconds - 0.1 {
        let now = on_body(time);
        if now && !inside && time >= 0.5 {
            entries.push(time);
        }
        inside = now;
        time += step;
    }
    outcome.crossings = entries.len();
    outcome.shots = landed.iter().filter(|shot| **shot >= 0.5).count();
    outcome.hits = landed
        .iter()
        .filter(|shot| **shot >= 0.5 && on_body(**shot))
        .count();
    for (index, entry) in entries.iter().enumerate() {
        let until = entries.get(index + 1).copied().unwrap_or(scene.seconds);
        if let Some(shot) = landed
            .iter()
            .find(|shot| **shot >= *entry && **shot < until && on_body(**shot))
        {
            outcome.crossings_hit += 1;
            outcome.reaction_s.push(shot - entry);
        }
    }
    outcome
}

#[test]
#[ignore]
fn auto_ads_report() {
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
    let schedules: [(&str, fn(f64) -> Vec<(f64, f64)>); 2] = [
        ("ADS from start", |z| vec![(0.0, z)]),
        ("ADS toggled while firing", |_| Vec::new()),
    ];
    let timings = [
        (
            "Delay 0 / Cooldown 0",
            AutoTiming {
                delay_ms: 0,
                cooldown_ms: 0,
                sleep_ms: 0,
            },
        ),
        (
            "Delay 0 / Cooldown 120 ms",
            AutoTiming {
                delay_ms: 0,
                cooldown_ms: 120,
                sleep_ms: 0,
            },
        ),
    ];
    for (look_name, look, body) in [
        ("solid head 6 px", Look::Solid, 6.0),
        ("outline 2 px, body 14 px", Look::Outline(2.0), 14.0),
    ] {
        for (speed_name, amplitude, hz) in [
            ("strafe 100 px/s", 20.0, 0.8),
            ("fast strafe 250 px/s", 20.0, 2.0),
        ] {
            for (timing_name, timing) in timings {
                for (schedule_name, schedule) in schedules {
                    eprintln!("AUTO {look_name} | {speed_name} | {timing_name} | {schedule_name}");
                    eprintln!("AUTO   zoom | crossings hit | shots on body | reaction");
                    for zoom in [1.0, 2.0, 2.5, 3.5, 5.0] {
                        let keys = if schedule_name.starts_with("ADS toggled") {
                            toggled(zoom)
                        } else {
                            schedule(zoom)
                        };
                        let scene = Scene {
                            keys,
                            look,
                            body,
                            amplitude,
                            hz,
                            seconds: 8.0,
                            timing,
                        };
                        let mut total = Outcome::default();
                        for seed in 0..20 {
                            let run = simulate(&scene, 900 + seed);
                            total.crossings += run.crossings;
                            total.crossings_hit += run.crossings_hit;
                            total.shots += run.shots;
                            total.hits += run.hits;
                            total.reaction_s.extend(run.reaction_s);
                        }
                        let reaction = total.reaction_s.iter().sum::<f64>()
                            / total.reaction_s.len().max(1) as f64;
                        eprintln!(
                            "AUTO   {zoom:>4}x | {:>5.1}% of {:>4} | {:>5.1}% of {:>4} | {:>4.0} ms",
                            100.0 * total.crossings_hit as f64 / total.crossings.max(1) as f64,
                            total.crossings,
                            100.0 * total.hits as f64 / total.shots.max(1) as f64,
                            total.shots,
                            reaction * 1000.0,
                        );
                    }
                }
            }
        }
    }
}
