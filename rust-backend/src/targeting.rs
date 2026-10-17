use crate::results::{DetectionMode, DetectionResult};
use rand::{rngs::StdRng, Rng, SeedableRng};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameCoordinate {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TargetCandidate {
    pub coordinate: FrameCoordinate,
    pub bbox_xyxy: [f32; 4],
    pub class_id: Option<u32>,
    pub score: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct PriorTarget {
    pub bbox_xyxy: Option<[f32; 4]>,
    pub class_id: Option<u32>,
}

/// FOV degrees map to pixels against this fixed reference side, not the
/// crop: the crop is a native-resolution cutout, so the same setting must be
/// the same circle on screen at every Crop Resolution (360° = 640 px).
pub const FOV_REFERENCE_PX: f32 = 640.0;

/// FOV circle radius in image pixels, limited to the image.
pub fn fov_radius_px(width: u32, height: u32, degrees: u16) -> f32 {
    let limit = (width.min(height) as f32 - 1.0).max(0.0) / 2.0;
    ((FOV_REFERENCE_PX - 1.0) * f32::from(degrees) / 720.0).min(limit)
}

pub fn highest_pixel_in_fov(
    mask: &[u8],
    width: u32,
    height: u32,
    degrees: u16,
) -> Option<FrameCoordinate> {
    highest_pixel_in_fov_within(mask, width, height, degrees, [0, 0, width, height])
}

/// Topmost mask pixel inside both the FOV circle and `region`
/// (`[left, top, right, bottom)` in mask pixels, right/bottom exclusive).
/// Ties on a row go to the pixel closest to the horizontal center.
pub fn highest_pixel_in_fov_within(
    mask: &[u8],
    width: u32,
    height: u32,
    degrees: u16,
    region: [u32; 4],
) -> Option<FrameCoordinate> {
    if width == 0
        || height == 0
        || !(1..=360).contains(&degrees)
        || mask.len() != width as usize * height as usize
    {
        return None;
    }
    let [left, top, right, bottom] = [
        region[0].min(width),
        region[1].min(height),
        region[2].min(width),
        region[3].min(height),
    ];
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = fov_radius_px(width, height, degrees);
    for y in top..bottom {
        let py = y as f32 + 0.5;
        let mut best_x: Option<u32> = None;
        for x in left..right {
            if mask[(y * width + x) as usize] == 0 {
                continue;
            }
            let px = x as f32 + 0.5;
            let dx = px - cx;
            let dy = py - cy;
            if dx * dx + dy * dy <= radius * radius
                && best_x
                    .is_none_or(|best| (x as f32 + 0.5 - cx).abs() < (best as f32 + 0.5 - cx).abs())
            {
                best_x = Some(x);
            }
        }
        if let Some(x) = best_x {
            return Some(FrameCoordinate {
                x: x as f32 + 0.5,
                y: py,
            });
        }
    }
    None
}

pub fn object_detection_candidates(
    result: &DetectionResult,
    minimum_score: f32,
) -> Vec<TargetCandidate> {
    if result.mode != DetectionMode::ObjectDetection || !(0.0..=1.0).contains(&minimum_score) {
        return Vec::new();
    }
    result
        .objects
        .iter()
        .filter_map(|object| {
            if object.confidence.is_none_or(|score| score < minimum_score) {
                return None;
            }
            let [left, top, right, bottom] = object.bbox_xyxy;
            Some(TargetCandidate {
                coordinate: FrameCoordinate {
                    x: (left + right) * 0.5,
                    y: (top + bottom) * 0.5,
                },
                bbox_xyxy: object.bbox_xyxy,
                class_id: Some(object.class_id),
                score: object.confidence,
            })
        })
        .collect()
}

pub fn object_detection_target(
    candidates: &[TargetCandidate],
    width: u32,
    height: u32,
    crop_x: u32,
    crop_y: u32,
    fov_degrees: u16,
    previous: Option<&PriorTarget>,
    previous_is_fresh: bool,
) -> Option<TargetCandidate> {
    if width == 0 || height == 0 || !(1..=360).contains(&fov_degrees) {
        return None;
    }
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = fov_radius_px(width, height, fov_degrees);
    let eligible: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            let [l, t, r, b] = candidate.bbox_xyxy;
            let (l, t, r, b) = (
                l - crop_x as f32,
                t - crop_y as f32,
                r - crop_x as f32,
                b - crop_y as f32,
            );
            if r - l > 100.0 {
                return false;
            }
            let dx = (l - cx).max(0.0).max(cx - r);
            let dy = (t - cy).max(0.0).max(cy - b);
            dx * dx + dy * dy <= radius * radius
        })
        .cloned()
        .collect();
    let distance = |candidate: &TargetCandidate| {
        let [l, t, r, b] = candidate.bbox_xyxy;
        ((l + r) * 0.5 - crop_x as f32 - cx).powi(2) + ((t + b) * 0.5 - crop_y as f32 - cy).powi(2)
    };
    let mut selected = eligible
        .iter()
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))?
        .clone();
    if previous_is_fresh {
        if let Some(old) = previous.and_then(|p| p.bbox_xyxy.map(|bbox| (bbox, p.class_id))) {
            let old_box = old.0;
            let tracked = eligible
                .iter()
                .filter(|candidate| candidate.class_id == old.1)
                .find(|candidate| {
                    let [l, t, r, b] = candidate.bbox_xyxy;
                    let [ol, ot, or, ob] = old_box;
                    let intersection =
                        (r.min(or) - l.max(ol)).max(0.0) * (b.min(ob) - t.max(ot)).max(0.0);
                    let area = (r - l).max(0.0) * (b - t).max(0.0);
                    area > 0.0 && intersection / area > 0.5
                });
            if let Some(tracked) = tracked {
                if distance(&selected) >= distance(tracked) * 0.33 {
                    selected = tracked.clone();
                }
            }
        }
    }
    let [left, top, right, bottom] = selected.bbox_xyxy;
    let anchor = FrameCoordinate {
        x: (left + right) * 0.5,
        y: top
            + (bottom - top)
                * if selected.class_id == Some(0) {
                    0.5
                } else {
                    0.05
                },
    };
    Some(TargetCandidate {
        coordinate: anchor,
        ..selected
    })
}

/// Smoothing strength on a 1–50 scale: 1 is no smoothing, 50 the strongest.
/// A strength `s` filters with alpha `1 / s` (roughly an average over `s`
/// frames). Each track draws its strength at random from `min..=max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmoothingRange {
    pub min: f32,
    pub max: f32,
}

impl SmoothingRange {
    pub const NONE: Self = Self { min: 1.0, max: 1.0 };
    pub const LIMITS: (f32, f32) = (1.0, 50.0);

    pub fn new(min: f32, max: f32) -> Option<Self> {
        let range = Self { min, max };
        range.is_valid().then_some(range)
    }

    pub fn is_valid(&self) -> bool {
        self.min.is_finite()
            && self.max.is_finite()
            && Self::LIMITS.0 <= self.min
            && self.min <= self.max
            && self.max <= Self::LIMITS.1
    }

    fn draw_alpha(&self, rng: &mut StdRng) -> f32 {
        let strength = if self.max > self.min {
            rng.random_range(self.min..=self.max)
        } else {
            self.min
        };
        1.0 / strength
    }
}

#[derive(Clone, Debug)]
pub struct ExponentialTargetSmoother {
    range_x: SmoothingRange,
    range_y: SmoothingRange,
    alpha_x: f32,
    alpha_y: f32,
    previous: Option<FrameCoordinate>,
    rng: StdRng,
}

impl ExponentialTargetSmoother {
    pub fn new(range_x: SmoothingRange, range_y: SmoothingRange) -> Option<Self> {
        Self::with_rng(range_x, range_y, StdRng::from_os_rng())
    }

    fn with_rng(range_x: SmoothingRange, range_y: SmoothingRange, rng: StdRng) -> Option<Self> {
        (range_x.is_valid() && range_y.is_valid()).then_some(Self {
            range_x,
            range_y,
            alpha_x: 1.0,
            alpha_y: 1.0,
            previous: None,
            rng,
        })
    }

    /// Filter factors drawn for the current track (1 = unsmoothed).
    pub fn alphas(&self) -> (f32, f32) {
        (self.alpha_x, self.alpha_y)
    }

    /// `None` ends the track; the next target starts a new one with freshly
    /// drawn strengths.
    pub fn update(&mut self, target: Option<FrameCoordinate>) -> Option<FrameCoordinate> {
        let Some(current) = target else {
            self.previous = None;
            return None;
        };
        let next = match self.previous {
            None => {
                self.alpha_x = self.range_x.draw_alpha(&mut self.rng);
                self.alpha_y = self.range_y.draw_alpha(&mut self.rng);
                current
            }
            Some(previous) => FrameCoordinate {
                x: previous.x + self.alpha_x * (current.x - previous.x),
                y: previous.y + self.alpha_y * (current.y - previous.y),
            },
        };
        self.previous = Some(next);
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(class_id: u32, bbox_xyxy: [f32; 4]) -> TargetCandidate {
        TargetCandidate {
            coordinate: FrameCoordinate {
                x: (bbox_xyxy[0] + bbox_xyxy[2]) * 0.5,
                y: (bbox_xyxy[1] + bbox_xyxy[3]) * 0.5,
            },
            bbox_xyxy,
            class_id: Some(class_id),
            score: Some(0.9),
        }
    }

    #[test]
    fn object_detection_uses_nearest_fov_box_and_class_anchor() {
        let candidates = [
            candidate(1, [45.0, 40.0, 55.0, 60.0]),
            candidate(0, [72.0, 45.0, 82.0, 55.0]),
        ];

        let selected = object_detection_target(&candidates, 100, 100, 0, 0, 360, None, false)
            .expect("center box is in the FOV");

        assert_eq!(selected.class_id, Some(1));
        assert_eq!(selected.coordinate, FrameCoordinate { x: 50.0, y: 41.0 });
    }

    #[test]
    fn object_detection_keeps_a_fresh_overlapping_prior_class() {
        let candidates = [
            candidate(1, [55.0, 45.0, 65.0, 55.0]),
            candidate(0, [60.0, 45.0, 70.0, 55.0]),
        ];
        let previous = PriorTarget {
            bbox_xyxy: Some([60.0, 45.0, 70.0, 55.0]),
            class_id: Some(0),
        };

        let selected =
            object_detection_target(&candidates, 100, 100, 0, 0, 360, Some(&previous), true)
                .expect("both boxes are in the FOV");

        assert_eq!(selected.class_id, Some(0));
        assert_eq!(selected.coordinate, FrameCoordinate { x: 65.0, y: 50.0 });
    }

    #[test]
    fn smoothing_strength_is_drawn_per_track_within_its_range() {
        use super::{ExponentialTargetSmoother, SmoothingRange};
        use rand::{rngs::StdRng, SeedableRng};
        assert!(SmoothingRange::new(0.5, 2.0).is_none());
        assert!(SmoothingRange::new(4.0, 3.0).is_none());
        assert!(SmoothingRange::new(1.0, 51.0).is_none());
        let point = |x: f32| Some(FrameCoordinate { x, y: x });

        // Strength 1 (no smoothing) follows the target exactly.
        let mut none =
            ExponentialTargetSmoother::new(SmoothingRange::NONE, SmoothingRange::NONE).unwrap();
        none.update(point(0.0));
        assert_eq!(none.update(point(10.0)), point(10.0));

        // Strength 4 moves a quarter of the way per frame.
        let fixed = SmoothingRange::new(4.0, 4.0).unwrap();
        let mut smoother = ExponentialTargetSmoother::new(fixed, fixed).unwrap();
        smoother.update(point(0.0));
        assert_eq!(smoother.update(point(8.0)), point(2.0));

        // A range draws a strength per track, held for the whole track.
        let range = SmoothingRange::new(2.0, 8.0).unwrap();
        let mut smoother =
            ExponentialTargetSmoother::with_rng(range, range, StdRng::seed_from_u64(7)).unwrap();
        let mut strengths = Vec::new();
        for _ in 0..40 {
            smoother.update(point(0.0));
            let alpha = smoother.alpha_x;
            assert!((1.0 / 8.0..=0.5).contains(&alpha), "{alpha}");
            smoother.update(point(1.0));
            assert_eq!(smoother.alpha_x, alpha, "held within the track");
            strengths.push((1.0 / alpha * 100.0).round() as i32);
            smoother.update(None); // target lost: next track draws again
        }
        strengths.sort_unstable();
        strengths.dedup();
        assert!(strengths.len() > 10, "draws vary between tracks");
    }
}
