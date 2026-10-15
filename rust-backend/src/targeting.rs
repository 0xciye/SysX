use crate::results::{DetectionMode, DetectionResult};

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

pub fn coordinate_in_fov(x: f32, y: f32, width: u32, height: u32, degrees: u16) -> bool {
    if width == 0 || height == 0 || !(1..=360).contains(&degrees) {
        return false;
    }
    let radius = (width.min(height) as f32 - 1.0) * degrees as f32 / 720.0;
    let dx = x - width as f32 / 2.0;
    let dy = y - height as f32 / 2.0;
    dx * dx + dy * dy <= radius * radius
}

pub fn highest_pixel_in_fov(
    mask: &[u8],
    width: u32,
    height: u32,
    degrees: u16,
) -> Option<FrameCoordinate> {
    if width == 0
        || height == 0
        || !(1..=360).contains(&degrees)
        || mask.len() != width as usize * height as usize
    {
        return None;
    }
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = (width.min(height) as f32 - 1.0) * degrees as f32 / 720.0;
    for y in 0..height {
        let py = y as f32 + 0.5;
        let mut best_x: Option<u32> = None;
        for x in 0..width {
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
    let radius = (width.min(height) as f32 - 1.0) * fov_degrees as f32 / 720.0;
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

#[derive(Clone, Debug)]
pub struct ExponentialTargetSmoother {
    alpha_x: f32,
    alpha_y: f32,
    previous: Option<FrameCoordinate>,
}

impl ExponentialTargetSmoother {
    pub fn new(alpha_x: f32, alpha_y: f32) -> Option<Self> {
        (alpha_x.is_finite()
            && alpha_y.is_finite()
            && (0.01..=1.0).contains(&alpha_x)
            && (0.01..=1.0).contains(&alpha_y))
        .then_some(Self {
            alpha_x,
            alpha_y,
            previous: None,
        })
    }
    pub fn update(&mut self, target: Option<FrameCoordinate>) -> Option<FrameCoordinate> {
        let Some(current) = target else {
            self.previous = None;
            return None;
        };
        let next = match self.previous {
            None => current,
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
}
