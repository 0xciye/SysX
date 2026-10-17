//! Auto target-validity gate.
//!
//! Auto does not rank pixels, lock onto a blob, or produce movement
//! coordinates. It answers whether a valid target (the shared color mask,
//! searched only inside the active Auto FOV circle) covers the screen center.
//! The game's crosshair hides the center and its arms hide the center row
//! and column, so the target counts as on center when its pixels surround the
//! center: some lie left, right, above and below it (on any rows/columns).
//! A crosshair dot can be larger than the Auto FOV and hide every target
//! pixel inside it, so [`CrosshairTracker`] measures the dot from the image
//! and the surround search reaches just past it.

use crate::targeting::fov_radius_px;
use image::RgbImage;

/// Half side of the square around the center the crosshair is measured in.
const WINDOW_HALF_PX: u32 = 48;
/// Luminance step that counts as a pixel changing between frames.
const CHANGE_LEVEL: i16 = 10;
/// A frame is a sample only when this share of the window's border changed:
/// the scene moved, so every pixel not drawn by the crosshair should change.
const SCENE_MOVED: f32 = 0.5;
const RATE_ALPHA: f32 = 0.02;
const MIN_SAMPLES: u32 = 30;
/// Pixels changing in fewer than this share of samples belong to the overlay.
const STATIC_RATE: f32 = 0.1;
/// Search this far beyond the measured crosshair radius.
pub const CROSSHAIR_MARGIN_PX: f32 = 3.0;

/// Measures how far the game's crosshair hides the center: the crosshair is
/// drawn over the scene, so while the scene moves its pixels stay the same.
#[derive(Default)]
pub struct CrosshairTracker {
    size: (u32, u32),
    half: u32,
    previous: Vec<u8>,
    rate: Vec<f32>,
    samples: u32,
}

impl CrosshairTracker {
    pub fn update(&mut self, image: &RgbImage) {
        let (width, height) = image.dimensions();
        let half = WINDOW_HALF_PX.min(width.min(height) / 2);
        if (width, height) != self.size {
            let side = (2 * half) as usize;
            *self = Self {
                size: (width, height),
                half,
                previous: Vec::new(),
                rate: vec![0.0; side * side],
                samples: 0,
            };
        }
        if half < 8 {
            return;
        }
        let side = 2 * half;
        let (x0, y0) = (width / 2 - half, height / 2 - half);
        let mut current = Vec::with_capacity((side * side) as usize);
        for y in y0..y0 + side {
            for x in x0..x0 + side {
                let [r, g, b] = image.get_pixel(x, y).0;
                current.push(((2 * u16::from(r) + 5 * u16::from(g) + u16::from(b)) / 8) as u8);
            }
        }
        if self.previous.len() == current.len() {
            let changed = |i: usize| {
                (i16::from(current[i]) - i16::from(self.previous[i])).abs() > CHANGE_LEVEL
            };
            let border = 6.min(half);
            let (mut ring, mut ring_changed) = (0u32, 0u32);
            for y in 0..side {
                for x in 0..side {
                    if x < border || y < border || x >= side - border || y >= side - border {
                        ring += 1;
                        ring_changed += u32::from(changed((y * side + x) as usize));
                    }
                }
            }
            if ring_changed as f32 >= SCENE_MOVED * ring as f32 {
                for i in 0..current.len() {
                    let step = if changed(i) { 1.0 } else { 0.0 };
                    self.rate[i] += (step - self.rate[i]) * RATE_ALPHA;
                }
                self.samples = self.samples.saturating_add(1);
            }
        }
        self.previous = current;
    }

    /// Radius in pixels of the crosshair at the center, or 0 before enough
    /// moving-scene samples or when nothing static covers the center.
    /// Measured along the diagonals, which miss the crosshair's arms; the
    /// second shortest of the four keeps a flat, unchanging background in
    /// one or two directions from inflating it.
    pub fn radius_px(&self) -> f32 {
        if self.samples < MIN_SAMPLES || self.half < 8 {
            return 0.0;
        }
        let side = 2 * self.half;
        let center = (
            self.size.0 as f32 / 2.0 - (self.size.0 / 2 - self.half) as f32,
            self.size.1 as f32 / 2.0 - (self.size.1 / 2 - self.half) as f32,
        );
        let mut reach = [0.0f32; 4];
        for (slot, (sx, sy)) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)]
            .into_iter()
            .enumerate()
        {
            for k in 0.. {
                let offset = k as f32 + 0.5;
                let (x, y) = (
                    (center.0 + sx * offset).floor(),
                    (center.1 + sy * offset).floor(),
                );
                if x < 0.0 || y < 0.0 || x >= side as f32 || y >= side as f32 {
                    break;
                }
                if self.rate[y as usize * side as usize + x as usize] >= STATIC_RATE {
                    break;
                }
                reach[slot] = (offset + 0.5) * std::f32::consts::SQRT_2;
            }
        }
        reach.sort_by(f32::total_cmp);
        reach[1]
    }
}

/// Returns `true` when the target covers the screen center: a nonzero mask
/// pixel in the center cells, or mask pixels inside the Auto FOV on all four
/// sides of the center (left, right, above and below). When a measured
/// crosshair (`crosshair_px`, see [`CrosshairTracker`]) is larger than the
/// FOV, the surround search reaches [`CROSSHAIR_MARGIN_PX`] past it instead.
///
/// The mask is expected to be row-major with exactly `width * height` bytes.
/// For even-sized images the crosshair falls between pixels, so the four
/// cells touching that point are the center.
pub fn has_target_at_center(
    mask: &[u8],
    width: u32,
    height: u32,
    degrees: u16,
    crosshair_px: f32,
) -> bool {
    if width == 0
        || height == 0
        || !(1..=360).contains(&degrees)
        || mask.len() != width as usize * height as usize
    {
        return false;
    }
    let (left, right) = (width.saturating_sub(1) / 2, width / 2);
    let (top, bottom) = (height.saturating_sub(1) / 2, height / 2);
    let hit = |x: u32, y: u32| mask[(y * width + x) as usize] != 0;
    if (top..=bottom).any(|y| (left..=right).any(|x| hit(x, y))) {
        return true;
    }
    let limit = (width.min(height) as f32 - 1.0).max(0.0) / 2.0;
    let crosshair = if crosshair_px > 0.0 {
        crosshair_px + CROSSHAIR_MARGIN_PX
    } else {
        0.0
    };
    let radius = fov_radius_px(width, height, degrees).max(crosshair.min(limit));
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let reach = radius.ceil() as u32 + 1;
    let (x0, x1) = (left.saturating_sub(reach), (right + reach).min(width - 1));
    let (y0, y1) = (top.saturating_sub(reach), (bottom + reach).min(height - 1));
    let (mut west, mut east, mut north, mut south) = (false, false, false, false);
    for y in y0..=y1 {
        let dy = y as f32 + 0.5 - cy;
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            if dx * dx + dy * dy > radius * radius || !hit(x, y) {
                continue;
            }
            west |= dx < 0.0;
            east |= dx > 0.0;
            north |= dy < 0.0;
            south |= dy > 0.0;
            if west && east && north && south {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{has_target_at_center, CrosshairTracker};
    use image::{Rgb, RgbImage};

    fn mask(points: &[(u32, u32)]) -> Vec<u8> {
        let mut mask = vec![0; 64 * 64];
        for (x, y) in points {
            mask[(y * 64 + x) as usize] = 1;
        }
        mask
    }

    #[test]
    fn accepts_a_matching_pixel_at_the_center() {
        assert!(has_target_at_center(&mask(&[(32, 32)]), 64, 64, 1, 0.0));
        // Even size: any of the four cells touching the center point.
        assert!(has_target_at_center(&mask(&[(31, 31)]), 64, 64, 1, 0.0));
    }

    #[test]
    fn a_target_around_a_crosshair_covered_center_counts() {
        // A head outline around the center; the crosshair hides the center
        // cells and its arms hide the center row and column.
        let mut points = Vec::new();
        for d in 0..8u32 {
            points.extend([(26 + d, 26), (26 + d, 38), (26, 26 + d), (38, 26 + d)]);
        }
        let outline = mask(&points);
        assert!(has_target_at_center(&outline, 64, 64, 36, 0.0));
        // Pixels only in two diagonal corners still surround the center.
        assert!(has_target_at_center(
            &mask(&[(27, 27), (37, 37)]),
            64,
            64,
            36,
            0.0
        ));
    }

    #[test]
    fn a_target_beside_or_above_the_center_does_not_count() {
        // Straddles the vertical line but sits entirely above the center.
        assert!(!has_target_at_center(
            &mask(&[(28, 26), (36, 26), (32, 22)]),
            64,
            64,
            90,
            0.0
        ));
        // Entirely to the right.
        assert!(!has_target_at_center(
            &mask(&[(37, 28), (40, 36)]),
            64,
            64,
            90,
            0.0
        ));
    }

    #[test]
    fn pixels_outside_the_fov_are_ignored() {
        // 36° is a 32 px radius (fixed 640 px reference); ±40 px is outside.
        let mut far = vec![0; 256 * 256];
        for (x, y) in [(88, 88), (168, 168)] {
            far[y * 256 + x] = 1;
        }
        assert!(!has_target_at_center(&far, 256, 256, 36, 0.0));
        assert!(has_target_at_center(&far, 256, 256, 90, 0.0));
    }

    #[test]
    fn rejects_invalid_masks_and_fov_values() {
        assert!(!has_target_at_center(&[1, 0], 2, 2, 180, 0.0));
        assert!(!has_target_at_center(&[1], 1, 1, 0, 0.0));
        assert!(!has_target_at_center(&[1], 1, 1, 361, 0.0));
    }

    /// Noise that changes every frame, like a moving scene, with an optional
    /// crosshair (dot of `dot` px radius plus 2 px arms) drawn on top.
    fn frame(size: u32, dot: Option<f32>, seed: &mut u64) -> RgbImage {
        let center = size as f32 / 2.0;
        RgbImage::from_fn(size, size, |x, y| {
            let (dx, dy) = (x as f32 + 0.5 - center, y as f32 + 0.5 - center);
            if let Some(radius) = dot {
                if dx * dx + dy * dy <= radius * radius {
                    return Rgb([255, 40, 24]);
                }
                if dx.abs() < 1.0 || dy.abs() < 1.0 {
                    return Rgb([80, 60, 40]);
                }
            }
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let v = (*seed >> 56) as u8;
            Rgb([v, v, v])
        })
    }

    #[test]
    fn the_tracker_measures_a_crosshair_dot_while_the_scene_moves() {
        let mut seed = 7;
        let mut tracker = CrosshairTracker::default();
        for _ in 0..80 {
            tracker.update(&frame(256, Some(7.0), &mut seed));
        }
        let radius = tracker.radius_px();
        assert!((6.0..=9.0).contains(&radius), "radius {radius}");

        let mut bare = CrosshairTracker::default();
        for _ in 0..80 {
            bare.update(&frame(256, None, &mut seed));
        }
        assert!(bare.radius_px() < 1.5, "radius {}", bare.radius_px());

        // A still scene gives no evidence.
        let still = frame(256, None, &mut seed);
        let mut idle = CrosshairTracker::default();
        for _ in 0..80 {
            idle.update(&still);
        }
        assert_eq!(idle.radius_px(), 0.0);
    }

    #[test]
    fn a_target_hidden_behind_a_crosshair_larger_than_the_fov_counts() {
        // Target pixels just outside a 7 px dot; the 2° Auto FOV is smaller.
        let mut points = Vec::new();
        for d in 0..21u32 {
            points.extend([(22 + d, 22), (22 + d, 42), (22, 22 + d), (42, 22 + d)]);
        }
        let around = mask(&points);
        assert!(!has_target_at_center(&around, 64, 64, 2, 0.0));
        assert!(has_target_at_center(&around, 64, 64, 2, 7.0));
        // Still only a target that surrounds the center.
        let beside = mask(&[(42, 26), (42, 38)]);
        assert!(!has_target_at_center(&beside, 64, 64, 2, 7.0));
    }

    /// The user's crosshair (no center dot): 2 px dark lines across the whole
    /// view through the center, over a tan wall, with a yellow target on it.
    #[test]
    fn a_target_under_full_length_crosshair_lines_counts() {
        use crate::pixel_seek::{color_mask, PixelTargetColor};
        let scene = |target: &dyn Fn(u32, u32) -> bool| {
            RgbImage::from_fn(128, 128, |x, y| {
                if (63..=64).contains(&x) || (63..=64).contains(&y) {
                    Rgb([0x85, 0x70, 0x58])
                } else if target(x, y) {
                    Rgb([255, 230, 0])
                } else {
                    Rgb([0xc3, 0xa4, 0x82])
                }
            })
        };
        let filled = scene(&|x, y| (54..74).contains(&x) && (50..80).contains(&y));
        let outline = scene(&|x, y| {
            (54..74).contains(&x)
                && (50..80).contains(&y)
                && !((56..72).contains(&x) && (52..78).contains(&y))
        });
        let beside = scene(&|x, y| (70..90).contains(&x) && (50..80).contains(&y));
        for (image, expected) in [(&filled, true), (&outline, true), (&beside, false)] {
            let mask = color_mask(image, PixelTargetColor::Yellow);
            assert_eq!(has_target_at_center(&mask, 128, 128, 90, 0.0), expected);
        }
    }
}
