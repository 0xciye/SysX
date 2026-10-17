//! Auto target-validity gate.
//!
//! Auto does not rank pixels, lock onto a blob, or produce movement
//! coordinates. It answers whether a valid target (the shared color mask,
//! searched only inside the active Auto FOV circle) covers the screen center.
//! The game's crosshair hides the center and its arms hide the center row
//! and column, so the target counts as on center when its pixels surround the
//! center: some lie left, right, above and below it (on any rows/columns).

use crate::targeting::fov_radius_px;

/// Returns `true` when the target covers the screen center: a nonzero mask
/// pixel in the center cells, or mask pixels inside the Auto FOV on all four
/// sides of the center (left, right, above and below).
///
/// The mask is expected to be row-major with exactly `width * height` bytes.
/// For even-sized images the crosshair falls between pixels, so the four
/// cells touching that point are the center.
pub fn has_target_at_center(mask: &[u8], width: u32, height: u32, degrees: u16) -> bool {
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
    let radius = fov_radius_px(width, height, degrees);
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
    use super::has_target_at_center;

    fn mask(points: &[(u32, u32)]) -> Vec<u8> {
        let mut mask = vec![0; 64 * 64];
        for (x, y) in points {
            mask[(y * 64 + x) as usize] = 1;
        }
        mask
    }

    #[test]
    fn accepts_a_matching_pixel_at_the_center() {
        assert!(has_target_at_center(&mask(&[(32, 32)]), 64, 64, 1));
        // Even size: any of the four cells touching the center point.
        assert!(has_target_at_center(&mask(&[(31, 31)]), 64, 64, 1));
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
        assert!(has_target_at_center(&outline, 64, 64, 36));
        // Pixels only in two diagonal corners still surround the center.
        assert!(has_target_at_center(
            &mask(&[(27, 27), (37, 37)]),
            64,
            64,
            36
        ));
    }

    #[test]
    fn a_target_beside_or_above_the_center_does_not_count() {
        // Straddles the vertical line but sits entirely above the center.
        assert!(!has_target_at_center(
            &mask(&[(28, 26), (36, 26), (32, 22)]),
            64,
            64,
            90
        ));
        // Entirely to the right.
        assert!(!has_target_at_center(
            &mask(&[(37, 28), (40, 36)]),
            64,
            64,
            90
        ));
    }

    #[test]
    fn pixels_outside_the_fov_are_ignored() {
        // 36° is a 32 px radius (fixed 640 px reference); ±40 px is outside.
        let mut far = vec![0; 256 * 256];
        for (x, y) in [(88, 88), (168, 168)] {
            far[y * 256 + x] = 1;
        }
        assert!(!has_target_at_center(&far, 256, 256, 36));
        assert!(has_target_at_center(&far, 256, 256, 90));
    }

    #[test]
    fn rejects_invalid_masks_and_fov_values() {
        assert!(!has_target_at_center(&[1, 0], 2, 2, 180));
        assert!(!has_target_at_center(&[1], 1, 1, 0));
        assert!(!has_target_at_center(&[1], 1, 1, 361));
    }
}
