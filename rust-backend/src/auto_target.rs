//! Auto target-validity gate.
//!
//! Auto does not rank pixels, lock onto a blob, or produce movement
//! coordinates. It answers whether a valid target (the shared color mask,
//! searched only inside the active Auto FOV circle) is passing through the
//! screen center. The game's crosshair usually covers the exact center, so a
//! target also counts as on center when its pixels lie on both sides of it.

use crate::targeting::fov_radius_px;

/// Returns `true` when the target covers the screen center: a nonzero mask
/// pixel in the center cells, or mask pixels inside the Auto FOV on both sides
/// of the center along its row (left and right) or column (above and below).
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
    let left = width.saturating_sub(1) / 2;
    let right = width / 2;
    let top = height.saturating_sub(1) / 2;
    let bottom = height / 2;
    let hit = |x: u32, y: u32| mask[(y * width + x) as usize] != 0;
    for y in top..=bottom {
        for x in left..=right {
            if hit(x, y) {
                return true;
            }
        }
    }
    let radius = fov_radius_px(width, height, degrees);
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let inside = |x: u32, y: u32| {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        dx * dx + dy * dy <= radius * radius
    };
    let reach = radius.ceil() as u32 + 1;
    // Steps away from the center cells: (x, y) of step `d` in one direction.
    let found = |cell: &dyn Fn(u32) -> (u32, u32)| {
        (1..=reach)
            .map(cell)
            .any(|(x, y)| x < width && y < height && inside(x, y) && hit(x, y))
    };
    let across_row =
        (top..=bottom).any(|y| found(&|d| (left.wrapping_sub(d), y)) && found(&|d| (right + d, y)));
    let across_column =
        (left..=right).any(|x| found(&|d| (x, top.wrapping_sub(d))) && found(&|d| (x, bottom + d)));
    across_row || across_column
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
        // Center cells hidden by the crosshair, target pixels on both sides.
        let row = mask(&[(27, 32), (37, 32)]);
        assert!(has_target_at_center(&row, 64, 64, 36));
        let column = mask(&[(31, 26), (31, 38)]);
        assert!(has_target_at_center(&column, 64, 64, 36));
    }

    #[test]
    fn a_target_on_one_side_only_does_not_count() {
        // Inside the FOV but not across the center.
        assert!(!has_target_at_center(
            &mask(&[(37, 32), (40, 32)]),
            64,
            64,
            90
        ));
        assert!(!has_target_at_center(
            &mask(&[(37, 30), (37, 34)]),
            64,
            64,
            90
        ));
    }

    #[test]
    fn pixels_outside_the_fov_are_ignored() {
        // 36° is a 32 px radius (fixed 640 px reference); ±40 px is outside.
        let mut far = vec![0; 256 * 256];
        far[128 * 256 + 88] = 1;
        far[128 * 256 + 168] = 1;
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
