//! Auto target-validity gate.
//!
//! Auto does not rank pixels, lock onto a blob, or produce movement
//! coordinates. It only answers whether at least one pixel from the active
//! shared color mask is inside the active Auto FOV.

/// Returns `true` when any nonzero mask pixel lies inside the centered FOV.
///
/// The mask is expected to be row-major with exactly `width * height` bytes.
/// FOV degrees use the same inscribed-circle geometry as Motion Pixel Seek.
pub fn has_pixel_in_fov(mask: &[u8], width: u32, height: u32, degrees: u16) -> bool {
    if width == 0
        || height == 0
        || !(1..=360).contains(&degrees)
        || mask.len() != width as usize * height as usize
    {
        return false;
    }
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = (width.min(height) as f32 - 1.0) * degrees as f32 / 720.0;
    let radius_squared = radius * radius;
    for y in 0..height {
        let py = y as f32 + 0.5;
        for x in 0..width {
            if mask[(y * width + x) as usize] == 0 {
                continue;
            }
            let px = x as f32 + 0.5;
            let dx = px - cx;
            let dy = py - cy;
            if dx * dx + dy * dy <= radius_squared {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::has_pixel_in_fov;

    #[test]
    fn accepts_any_matching_pixel_inside_the_fov() {
        let mut mask = vec![0; 8 * 8];
        mask[3 * 8 + 4] = 1;
        assert!(has_pixel_in_fov(&mask, 8, 8, 180));
    }

    #[test]
    fn ignores_matching_pixels_outside_the_fov() {
        let mut mask = vec![0; 8 * 8];
        mask[0] = 1;
        assert!(!has_pixel_in_fov(&mask, 8, 8, 90));
    }

    #[test]
    fn rejects_invalid_masks_and_fov_values() {
        assert!(!has_pixel_in_fov(&[1, 0], 2, 2, 180));
        assert!(!has_pixel_in_fov(&[1], 1, 1, 0));
        assert!(!has_pixel_in_fov(&[1], 1, 1, 361));
    }
}
