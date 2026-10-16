//! Auto target-validity gate.
//!
//! Auto does not rank pixels, lock onto a blob, or produce movement
//! coordinates. It only answers whether the active shared color mask reaches
//! the screen center (the crosshair) inside the active Auto FOV.

/// Returns `true` when a nonzero mask pixel occupies the screen-center cell.
///
/// The mask is expected to be row-major with exactly `width * height` bytes.
/// For even-sized images the crosshair falls between pixels, so the four
/// cells touching that point are accepted. FOV degrees use the same
/// inscribed-circle geometry as Motion Pixel Seek; the geometric center is
/// always inside a valid FOV.
pub fn has_center_pixel_in_fov(mask: &[u8], width: u32, height: u32, degrees: u16) -> bool {
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
    for y in top..=bottom {
        for x in left..=right {
            if mask[(y * width + x) as usize] != 0 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::has_center_pixel_in_fov;

    #[test]
    fn accepts_a_matching_pixel_at_the_center() {
        let mut mask = vec![0; 8 * 8];
        mask[4 * 8 + 4] = 1;
        assert!(has_center_pixel_in_fov(&mask, 8, 8, 1));
    }

    #[test]
    fn accepts_either_cell_when_the_center_falls_between_even_pixels() {
        let mut mask = vec![0; 8 * 8];
        mask[3 * 8 + 3] = 1;
        assert!(has_center_pixel_in_fov(&mask, 8, 8, 1));
    }

    #[test]
    fn ignores_matching_pixels_away_from_the_center_even_inside_the_fov() {
        let mut mask = vec![0; 8 * 8];
        mask[3 * 8 + 5] = 1;
        assert!(!has_center_pixel_in_fov(&mask, 8, 8, 180));
    }

    #[test]
    fn ignores_matching_pixels_outside_the_center() {
        let mut mask = vec![0; 8 * 8];
        mask[0] = 1;
        assert!(!has_center_pixel_in_fov(&mask, 8, 8, 360));
    }

    #[test]
    fn rejects_invalid_masks_and_fov_values() {
        assert!(!has_center_pixel_in_fov(&[1, 0], 2, 2, 180));
        assert!(!has_center_pixel_in_fov(&[1], 1, 1, 0));
        assert!(!has_center_pixel_in_fov(&[1], 1, 1, 361));
    }
}
