//! Continuous target tracking with hysteresis and a grace period (ported from
//! the user's `target_manager.rs`). Detections are associated greedily with
//! the nearest tracked entity within `MAX_ASSOCIATE_PX`; matched entities
//! move to an EMA of their box (`POSITION_ALPHA`, so box noise does not shake
//! the aim) and update an EMA velocity, unmatched ones stay where they were
//! last seen for up to `MAX_LOST`, and selection prefers the locked entity by
//! `HYSTERESIS_PX`. Lost entities are not extrapolated (user report,
//! 2026-10-31: the aim drifted off the target): while Motion turns the
//! camera, the on-screen velocity is mostly that turn, so a prediction moved
//! the aim to where nobody was.
//! Used by both Pixel Seek (color clusters) and Object Detection (boxes).

use std::time::{Duration, Instant};

pub const MAX_ASSOCIATE_PX: f32 = 60.0;
pub const HYSTERESIS_PX: f32 = 38.0;
pub const MAX_LOST: Duration = Duration::from_millis(140);
pub const ALPHA_EMA: f32 = 0.40;
/// Weight of a new box in the tracked position and size (1: no smoothing).
pub const POSITION_ALPHA: f32 = 0.6;

/// What a target is, in selection order (user request: head, then body,
/// then ability).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    Head,
    /// Any other enemy class, and Pixel Seek clusters.
    #[default]
    Body,
    Ability,
}

/// One detection in original-frame coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub bbox_xyxy: [f32; 4],
    /// Model class; `None` for Pixel Seek clusters.
    pub class_id: Option<u32>,
    pub confidence: Option<f32>,
    pub kind: TargetKind,
}

impl Detection {
    fn center(&self) -> (f32, f32) {
        let [l, t, r, b] = self.bbox_xyxy;
        ((l + r) * 0.5, (t + b) * 0.5)
    }

    fn size(&self) -> (f32, f32) {
        let [l, t, r, b] = self.bbox_xyxy;
        (r - l, b - t)
    }
}

/// At least 2 × 2 px. Classes that are not targets (teammates, utility)
/// and confidence are filtered earlier (`targeting::object_detection_candidates`).
pub fn is_valid_target(detection: &Detection) -> bool {
    let (width, height) = detection.size();
    width >= 2.0 && height >= 2.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackedEntity {
    pub id: u64,
    /// Center, extrapolated along the velocity while lost.
    pub x: f32,
    pub y: f32,
    /// Px per second.
    pub vx: f32,
    pub vy: f32,
    pub width: f32,
    pub height: f32,
    pub class_id: Option<u32>,
    pub confidence: Option<f32>,
    pub kind: TargetKind,
    pub is_lost: bool,
    /// Center where the entity was last seen; extrapolation starts here so a
    /// lost entity does not drift further on every frame.
    seen: (f32, f32),
    last_seen: Instant,
}

impl TrackedEntity {
    pub fn distance_to(&self, cx: f32, cy: f32) -> f32 {
        (self.x - cx).hypot(self.y - cy)
    }

    pub fn bbox_xyxy(&self) -> [f32; 4] {
        [
            self.x - self.width * 0.5,
            self.y - self.height * 0.5,
            self.x + self.width * 0.5,
            self.y + self.height * 0.5,
        ]
    }
}

#[derive(Debug, Default)]
pub struct TargetManager {
    entities: Vec<TrackedEntity>,
    next_id: u64,
    locked_id: Option<u64>,
}

impl TargetManager {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Forget the lock but keep tracking (every trigger released).
    pub fn release_lock(&mut self) {
        self.locked_id = None;
    }

    pub fn entities(&self) -> &[TrackedEntity] {
        &self.entities
    }

    pub fn update(&mut self, detections: &[Detection], now: Instant) {
        let valid: Vec<&Detection> = detections.iter().filter(|d| is_valid_target(d)).collect();
        let mut matched_detection = vec![false; valid.len()];
        let mut matches = Vec::new();
        // Greedy nearest neighbour, in entity order.
        for (entity_index, entity) in self.entities.iter().enumerate() {
            let mut best_distance = MAX_ASSOCIATE_PX;
            let mut best = None;
            for (index, detection) in valid.iter().enumerate() {
                // A head never continues a body track (or the reverse).
                if matched_detection[index] || detection.kind != entity.kind {
                    continue;
                }
                let (cx, cy) = detection.center();
                let distance = (cx - entity.x).hypot(cy - entity.y);
                if distance < best_distance {
                    best_distance = distance;
                    best = Some(index);
                }
            }
            if let Some(index) = best {
                matched_detection[index] = true;
                matches.push((entity_index, index));
            }
        }
        for (entity_index, index) in matches {
            let entity = &mut self.entities[entity_index];
            let detection = valid[index];
            let (nx, ny) = detection.center();
            let dt = now
                .saturating_duration_since(entity.last_seen)
                .as_secs_f32()
                .max(0.001);
            let (vx, vy) = ((nx - entity.seen.0) / dt, (ny - entity.seen.1) / dt);
            entity.vx = ALPHA_EMA * vx + (1.0 - ALPHA_EMA) * entity.vx;
            entity.vy = ALPHA_EMA * vy + (1.0 - ALPHA_EMA) * entity.vy;
            let ema = |old: f32, new: f32| old + POSITION_ALPHA * (new - old);
            let (x, y) = (ema(entity.seen.0, nx), ema(entity.seen.1, ny));
            (entity.x, entity.y) = (x, y);
            entity.seen = (x, y);
            let (width, height) = detection.size();
            (entity.width, entity.height) = (ema(entity.width, width), ema(entity.height, height));
            entity.class_id = detection.class_id;
            entity.confidence = detection.confidence;
            entity.last_seen = now;
            entity.is_lost = false;
        }
        for (index, detection) in valid.iter().enumerate() {
            if matched_detection[index] {
                continue;
            }
            self.next_id += 1;
            let (x, y) = detection.center();
            let (width, height) = detection.size();
            self.entities.push(TrackedEntity {
                id: self.next_id,
                x,
                y,
                vx: 0.0,
                vy: 0.0,
                width,
                height,
                class_id: detection.class_id,
                confidence: detection.confidence,
                kind: detection.kind,
                is_lost: false,
                seen: (x, y),
                last_seen: now,
            });
        }
        self.entities.retain_mut(|entity| {
            let lost_for = now.saturating_duration_since(entity.last_seen);
            if lost_for > MAX_LOST {
                return false;
            }
            if !lost_for.is_zero() {
                // Held where it was last seen (no prediction).
                entity.is_lost = true;
                (entity.x, entity.y) = entity.seen;
            }
            true
        });
        if let Some(id) = self.locked_id {
            if !self.entities.iter().any(|entity| entity.id == id) {
                self.locked_id = None;
            }
        }
    }

    /// The entity closest to `crosshair` within `fov_px`, the locked one
    /// counting `HYSTERESIS_PX` closer. When `ranked`, the kind comes
    /// first: a body is chosen only when no head is in the FOV, an ability
    /// only when neither is. Locks the result.
    pub fn select_target(
        &mut self,
        crosshair: (f32, f32),
        fov_px: f32,
        ranked: bool,
    ) -> Option<&TrackedEntity> {
        let mut best = None;
        let mut min_cost = (TargetKind::Ability, f32::MAX);
        let mut found = false;
        for (index, entity) in self.entities.iter().enumerate() {
            let distance = entity.distance_to(crosshair.0, crosshair.1);
            if distance > fov_px {
                continue;
            }
            let mut cost = distance;
            if Some(entity.id) == self.locked_id {
                cost -= HYSTERESIS_PX;
            }
            // (kind, distance): heads sort first, then bodies, then abilities.
            let kind = if ranked { entity.kind } else { TargetKind::Body };
            let cost = (kind, cost);
            if !found || cost.0 < min_cost.0 || (cost.0 == min_cost.0 && cost.1 < min_cost.1) {
                found = true;
                min_cost = cost;
                best = Some(index);
            }
        }
        self.locked_id = best.map(|index| self.entities[index].id);
        best.map(|index| &self.entities[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(cx: f32, cy: f32, class_id: Option<u32>) -> Detection {
        Detection {
            bbox_xyxy: [cx - 5.0, cy - 10.0, cx + 5.0, cy + 10.0],
            class_id,
            confidence: Some(0.9),
            kind: TargetKind::Body,
        }
    }

    fn head(cx: f32, cy: f32) -> Detection {
        Detection {
            bbox_xyxy: [cx - 3.0, cy - 3.0, cx + 3.0, cy + 3.0],
            class_id: Some(1),
            confidence: Some(0.9),
            kind: TargetKind::Head,
        }
    }

    #[test]
    fn tiny_boxes_are_not_targets() {
        assert!(is_valid_target(&detection(0.0, 0.0, Some(0))));
        assert!(is_valid_target(&detection(0.0, 0.0, None)));
        assert!(!is_valid_target(&Detection {
            bbox_xyxy: [0.0, 0.0, 1.5, 10.0],
            class_id: None,
            confidence: None,
            kind: TargetKind::Body,
        }));
    }

    #[test]
    fn a_head_in_the_fov_wins_over_a_closer_or_locked_body() {
        let start = Instant::now();
        let center = (320.0, 320.0);
        let mut manager = TargetManager::default();
        manager.update(&[detection(322.0, 320.0, Some(0))], start);
        let body = manager.select_target(center, 200.0, true).unwrap().id;
        // The head 40 px above the body center starts its own track and is
        // chosen although the body is locked and closer.
        let now = start + Duration::from_millis(5);
        manager.update(&[detection(322.0, 320.0, Some(0)), head(322.0, 280.0)], now);
        assert_eq!(manager.entities().len(), 2);
        let chosen = manager.select_target(center, 200.0, true).unwrap();
        assert_eq!(chosen.kind, TargetKind::Head);
        assert_ne!(chosen.id, body);
        // Unranked (Pixel Seek) picks the nearest.
        assert_eq!(manager.select_target(center, 200.0, false).unwrap().kind, TargetKind::Body);
        // A head outside the FOV leaves the body.
        let mut manager = TargetManager::default();
        manager.update(&[detection(322.0, 320.0, Some(0)), head(600.0, 320.0)], start);
        assert_eq!(manager.select_target(center, 200.0, true).unwrap().kind, TargetKind::Body);
        // An ability is chosen only without heads and bodies in the FOV.
        let ability = Detection {
            kind: TargetKind::Ability,
            ..detection(320.0, 320.0, Some(3))
        };
        let mut manager = TargetManager::default();
        manager.update(&[ability.clone(), detection(400.0, 320.0, Some(0))], start);
        assert_eq!(manager.select_target(center, 200.0, true).unwrap().kind, TargetKind::Body);
        let mut manager = TargetManager::default();
        manager.update(&[ability, detection(600.0, 320.0, Some(0))], start);
        assert_eq!(manager.select_target(center, 200.0, true).unwrap().kind, TargetKind::Ability);
    }

    #[test]
    fn a_moving_detection_keeps_its_id_and_learns_its_velocity() {
        let start = Instant::now();
        let mut manager = TargetManager::default();
        manager.update(&[detection(100.0, 100.0, Some(0))], start);
        let id = manager.entities()[0].id;
        manager.update(
            &[detection(110.0, 100.0, Some(0))],
            start + Duration::from_millis(10),
        );
        let entity = &manager.entities()[0];
        // The position follows the box through an EMA: 100 + 0.6 × 10.
        assert_eq!(entity.id, id);
        assert!((entity.x - 106.0).abs() < 1e-4, "{}", entity.x);
        assert!(
            (entity.vx - 400.0).abs() < 1.0,
            "0.4 × 1000 px/s: {}",
            entity.vx
        );
    }

    #[test]
    fn a_lost_entity_stays_where_it_was_last_seen_then_is_dropped() {
        let start = Instant::now();
        let mut manager = TargetManager::default();
        manager.update(&[detection(100.0, 100.0, None)], start);
        manager.update(
            &[detection(110.0, 100.0, None)],
            start + Duration::from_millis(10),
        );
        let seen = manager.entities()[0].x;
        assert!(manager.entities()[0].vx > 0.0, "moving");
        for ms in [30, 60] {
            manager.update(&[], start + Duration::from_millis(ms));
            let entity = &manager.entities()[0];
            assert!(entity.is_lost);
            assert_eq!(entity.x, seen, "{ms}: no prediction");
        }
        manager.update(&[], start + Duration::from_millis(151));
        assert!(manager.entities().is_empty());
    }

    #[test]
    fn hysteresis_keeps_the_locked_target_until_another_is_much_closer() {
        let start = Instant::now();
        let center = (320.0, 320.0);
        let now = start + Duration::from_millis(5);
        // Locked 20 px away; a new target on the center is only 20 px closer.
        let mut manager = TargetManager::default();
        manager.update(&[detection(340.0, 320.0, None)], start);
        let locked = manager.select_target(center, 200.0, true).unwrap().id;
        manager.update(
            &[detection(340.0, 320.0, None), detection(320.0, 320.0, None)],
            now,
        );
        assert_eq!(manager.select_target(center, 200.0, true).unwrap().id, locked);
        // Locked 50 px away; a new target 5 px away is 45 px closer.
        let mut manager = TargetManager::default();
        manager.update(&[detection(370.0, 320.0, None)], start);
        let locked = manager.select_target(center, 200.0, true).unwrap().id;
        manager.update(
            &[detection(370.0, 320.0, None), detection(325.0, 320.0, None)],
            now,
        );
        assert_ne!(manager.select_target(center, 200.0, true).unwrap().id, locked);
    }

    #[test]
    fn targets_outside_the_fov_are_not_selected_and_release_the_lock() {
        let start = Instant::now();
        let mut manager = TargetManager::default();
        manager.update(&[detection(500.0, 320.0, None)], start);
        assert!(manager.select_target((320.0, 320.0), 100.0, true).is_none());
        assert!(manager.locked_id.is_none());
    }
}
