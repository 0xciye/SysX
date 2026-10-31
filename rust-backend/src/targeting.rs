use crate::results::{DetectionMode, DetectionResult};
use crate::target_manager::{Detection, TargetKind};
use std::collections::HashMap;

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

/// FOV degrees map to pixels against this fixed reference side, not the
/// crop: the crop is a native-resolution cutout, so the same setting must be
/// the same circle on screen at every Crop Resolution (360° = 640 px).
pub const FOV_REFERENCE_PX: f32 = 640.0;

/// Center of an `[left, top, right, bottom]` box, in the box's own coordinate
/// space (original-frame pixels for model detections).
pub fn bbox_center(bbox_xyxy: [f32; 4]) -> FrameCoordinate {
    let [left, top, right, bottom] = bbox_xyxy;
    FrameCoordinate {
        x: (left + right) * 0.5,
        y: (top + bottom) * 0.5,
    }
}

/// FOV circle radius in image pixels, limited to the image.
pub fn fov_radius_px(width: u32, height: u32, degrees: u16) -> f32 {
    let limit = (width.min(height) as f32 - 1.0).max(0.0) / 2.0;
    ((FOV_REFERENCE_PX - 1.0) * f32::from(degrees) / 720.0).min(limit)
}

/// What a model class is for targeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassRole {
    /// Selected first, aimed at its center.
    Head,
    Target,
    /// Enemy utility (`ability` classes): a target with its own Min ability
    /// confidence, selected only without heads and bodies in the FOV.
    Ability,
    /// Teammates, utility, weapons, dead bodies, the own character.
    Ignore,
}

/// Name words of classes that are never targets.
const IGNORED_WORDS: &[&str] = &[
    "teammate", "teammates", "team", "tm", "ally", "allies", "friend", "friendly", "utility",
    "flash", "smoke", "fire", "molotov", "grenade", "nade",
    "projectile", "weapon", "gun", "dead", "corpse", "third", "self",
];

/// The role of `class_id` from the model's own class names (Ultralytics
/// `names` metadata): a name with a word from `IGNORED_WORDS` is ignored,
/// the word `ability`/`abilities` makes an ability, `head` a head, anything
/// else is a target. Without names the source's fixed classes apply:
/// 1 head, 2 teammate, 3 ability, 4 utility.
pub fn class_role(names: Option<&HashMap<u32, String>>, class_id: u32) -> ClassRole {
    let Some(names) = names else {
        return match class_id {
            1 => ClassRole::Head,
            3 => ClassRole::Ability,
            2 | 4 => ClassRole::Ignore,
            _ => ClassRole::Target,
        };
    };
    let Some(name) = names.get(&class_id) else {
        return ClassRole::Target;
    };
    let name = name.to_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    if words.iter().any(|word| IGNORED_WORDS.contains(word)) {
        ClassRole::Ignore
    } else if words.iter().any(|word| ["ability", "abilities"].contains(word)) {
        ClassRole::Ability
    } else if words.contains(&"head") {
        ClassRole::Head
    } else {
        ClassRole::Target
    }
}

/// Head aim point: this fraction of the box height below its top.
pub const HEAD_RATIO_Y: f32 = 0.15;

/// Where Object Detection aims inside a box: a body `HEAD_RATIO_Y` below
/// its top, a head or an ability at its center. Heads are selected first
/// (`TargetManager::select_target`).
pub fn aim_point(bbox_xyxy: [f32; 4], kind: TargetKind) -> FrameCoordinate {
    let [left, top, right, bottom] = bbox_xyxy;
    if kind != TargetKind::Body {
        return bbox_center(bbox_xyxy);
    }
    FrameCoordinate {
        x: (left + right) * 0.5,
        y: top + HEAD_RATIO_Y * (bottom - top),
    }
}

/// Color swap: a box counts as an enemy only with at least this many pixels
/// of the selected outline color inside it or within `OUTLINE_MARGIN_PX`.
pub const OUTLINE_MIN_PIXELS: usize = 3;
pub const OUTLINE_MARGIN_PX: f32 = 2.0;

/// Whether `bbox_xyxy` (image pixels) holds the outline color of `mask`
/// (one byte per pixel of a `width` × `height` image, nonzero = color).
pub fn box_has_outline(mask: &[u8], width: u32, height: u32, bbox_xyxy: [f32; 4]) -> bool {
    if width == 0 || mask.len() != width as usize * height as usize {
        return false;
    }
    let clamp = |value: f32, limit: u32| value.max(0.0).min(limit as f32) as usize;
    let [left, top, right, bottom] = bbox_xyxy;
    let (x0, x1) = (
        clamp((left - OUTLINE_MARGIN_PX).floor(), width),
        clamp((right + OUTLINE_MARGIN_PX).ceil(), width),
    );
    let (y0, y1) = (
        clamp((top - OUTLINE_MARGIN_PX).floor(), height),
        clamp((bottom + OUTLINE_MARGIN_PX).ceil(), height),
    );
    let mut count = 0;
    for y in y0..y1 {
        let row = &mask[y * width as usize..][..width as usize];
        count += row[x0..x1].iter().filter(|value| **value != 0).count();
        if count >= OUTLINE_MIN_PIXELS {
            return true;
        }
    }
    false
}

/// Model detections whose class is a target, at or above `minimum_score`
/// (`ability_score` for abilities), for the target manager.
pub fn object_detection_candidates(
    result: &DetectionResult,
    minimum_score: f32,
    ability_score: f32,
    names: Option<&HashMap<u32, String>>,
) -> Vec<Detection> {
    if result.mode != DetectionMode::ObjectDetection
        || !(0.0..=1.0).contains(&minimum_score)
        || !(0.0..=1.0).contains(&ability_score)
    {
        return Vec::new();
    }
    result
        .objects
        .iter()
        .filter_map(|object| {
            let (kind, threshold) = match class_role(names, object.class_id) {
                ClassRole::Ignore => return None,
                ClassRole::Head => (TargetKind::Head, minimum_score),
                ClassRole::Target => (TargetKind::Body, minimum_score),
                ClassRole::Ability => (TargetKind::Ability, ability_score),
            };
            object.confidence.filter(|score| *score >= threshold)?;
            Some(Detection {
                bbox_xyxy: object.bbox_xyxy,
                class_id: Some(object.class_id),
                confidence: object.confidence,
                kind,
            })
        })
        .collect()
}

/// Smoothing strength on a 1–50 scale: Motion divides the error by a value
/// drawn from `min..=max` on every frame (1 moves the whole error).
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_center_is_the_midpoint_of_both_axes() {
        assert_eq!(
            bbox_center([10.0, 20.0, 30.0, 60.0]),
            FrameCoordinate { x: 20.0, y: 40.0 }
        );
        assert_eq!(
            bbox_center([5.0, 5.0, 6.0, 8.0]),
            FrameCoordinate { x: 5.5, y: 6.5 }
        );
    }

    #[test]
    fn object_detection_candidates_keep_class_and_score_above_the_minimum() {
        let object = |class_id, confidence| crate::results::DetectionObject {
            class_id,
            class_name: class_id.to_string(),
            bbox_xyxy: [40.0, 10.0, 60.0, 50.0],
            confidence,
            pixel_count: None,
        };
        let result = DetectionResult {
            frame_id: 1,
            timestamp_ns: 0,
            mode: DetectionMode::ObjectDetection,
            model_id: None,
            source_width: 100,
            source_height: 100,
            processed_width: 100,
            processed_height: 100,
            objects: vec![
                object(0, Some(0.8)),
                object(1, Some(0.9)),
                object(2, Some(0.9)),
                object(0, Some(0.49)),
                object(1, None),
                object(3, Some(0.65)),
                object(3, Some(0.55)),
            ],
            preprocess_ms: 0.0,
            detection_ms: 0.0,
            postprocess_ms: 0.0,
            total_processing_ms: 0.0,
            inference_device: None,
            target_xy: None,
            smoothed_target_xy: None,
            auto_target_valid: false,
            auto_active: false,
            fov_degrees: 0,
        };

        let names: HashMap<u32, String> = [(0, "body"), (1, "head"), (2, "tm"), (3, "ability")]
            .map(|(id, name)| (id, name.to_owned()))
            .into();
        let candidates = object_detection_candidates(&result, 0.5, 0.6, Some(&names));

        let bbox_xyxy = [40.0, 10.0, 60.0, 50.0];
        assert_eq!(
            candidates,
            vec![
                Detection {
                    bbox_xyxy,
                    class_id: Some(0),
                    confidence: Some(0.8),
                    kind: TargetKind::Body,
                },
                Detection {
                    bbox_xyxy,
                    class_id: Some(1),
                    confidence: Some(0.9),
                    kind: TargetKind::Head,
                },
                Detection {
                    bbox_xyxy,
                    class_id: Some(3),
                    confidence: Some(0.65),
                    kind: TargetKind::Ability,
                },
            ]
        );
    }

    #[test]
    fn class_roles_come_from_the_model_names() {
        let names: HashMap<u32, String> = [
            "player", "bot", "weapon", "outline", "dead_body", "Enemy Head", "smoke", "fire",
            "third_person", "body", "tm", "ability", "flash", "Teammate", "enemy",
        ]
        .iter()
        .enumerate()
        .map(|(id, name)| (id as u32, (*name).to_owned()))
        .collect();
        let roles: Vec<ClassRole> = (0..names.len() as u32 + 1)
            .map(|id| class_role(Some(&names), id))
            .collect();
        use ClassRole::{Ability as A, Head as H, Ignore as I, Target as T};
        assert_eq!(roles, [T, T, I, T, I, H, I, I, I, T, I, A, I, I, T, T]);
        // No names: the source's fixed classes.
        let roles: Vec<ClassRole> = (0..6).map(|id| class_role(None, id)).collect();
        assert_eq!(roles, [T, H, I, A, I, T]);
    }

    #[test]
    fn heads_are_aimed_at_their_center_other_boxes_near_the_top() {
        let bbox = [0.0, 100.0, 20.0, 200.0];
        let center = FrameCoordinate { x: 10.0, y: 150.0 };
        assert_eq!(aim_point(bbox, TargetKind::Body), FrameCoordinate { x: 10.0, y: 115.0 });
        assert_eq!(aim_point(bbox, TargetKind::Head), center);
        assert_eq!(aim_point(bbox, TargetKind::Ability), center);
    }

    #[test]
    fn only_boxes_with_the_outline_color_count() {
        let (width, height) = (20u32, 10u32);
        let mut mask = vec![0u8; 200];
        // Three outline pixels in column 12, rows 2..5.
        for y in 2..5 {
            mask[y * 20 + 12] = 1;
        }
        assert!(box_has_outline(&mask, width, height, [10.0, 1.0, 14.0, 8.0]));
        // Two pixels away from the box edge still counts (margin).
        assert!(box_has_outline(&mask, width, height, [3.0, 1.0, 10.5, 8.0]));
        assert!(!box_has_outline(&mask, width, height, [0.0, 0.0, 8.0, 9.0]));
        // Fewer than OUTLINE_MIN_PIXELS.
        mask[4 * 20 + 12] = 0;
        assert!(!box_has_outline(&mask, width, height, [10.0, 1.0, 14.0, 8.0]));
        assert!(!box_has_outline(&mask[..10], width, height, [0.0, 0.0, 20.0, 10.0]));
    }

    #[test]
    fn fov_radius_uses_the_fixed_reference_and_is_limited_to_the_image() {
        assert!((fov_radius_px(640, 640, 360) - 319.5).abs() < 1e-3);
        assert!((fov_radius_px(100, 100, 360) - 49.5).abs() < 1e-3);
    }
}
