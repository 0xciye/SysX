use crate::crop::CropRect;
use crate::results::DetectionObject;
use crate::targeting::{highest_pixel_in_fov, highest_pixel_in_fov_within, FrameCoordinate};
use image::RgbImage;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelTargetColor {
    Yellow,
    Red,
    Purple,
}

impl PixelTargetColor {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "yellow" => Some(Self::Yellow),
            "red" => Some(Self::Red),
            "purple" => Some(Self::Purple),
            _ => None,
        }
    }
    pub fn class(self) -> (u32, &'static str) {
        match self {
            Self::Yellow => (1, "Yellow"),
            Self::Red => (2, "Red"),
            Self::Purple => (3, "Purple"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PixelSeekOutput {
    pub objects: Vec<DetectionObject>,
    pub target: Option<FrameCoordinate>,
    pub mask: Vec<u8>,
}

/// Minimum lock margin around the previous target box, in crop pixels.
const LOCK_MARGIN_PX: f32 = 12.0;
/// Lock margin as a fraction of the previous box's larger side.
const LOCK_MARGIN_RATIO: f32 = 0.5;

/// Search window for a locked target: the previous blob's box (original-frame
/// coordinates) grown by a margin, in crop pixels `[left, top, right, bottom)`.
/// Higher pixels that appear next to the target fall inside it, so the target
/// keeps climbing; a separate blob elsewhere in the FOV does not.
fn lock_region(previous: [f32; 4], crop: CropRect, width: u32, height: u32) -> Option<[u32; 4]> {
    if !previous.iter().all(|value| value.is_finite()) {
        return None;
    }
    let [left, top, right, bottom] = [
        previous[0] - crop.x as f32,
        previous[1] - crop.y as f32,
        previous[2] - crop.x as f32,
        previous[3] - crop.y as f32,
    ];
    let margin = LOCK_MARGIN_PX.max((right - left).max(bottom - top) * LOCK_MARGIN_RATIO);
    let clamp = |value: f32, max: u32| value.clamp(0.0, max as f32) as u32;
    let region = [
        clamp((left - margin).floor(), width),
        clamp((top - margin).floor(), height),
        clamp((right + margin).ceil(), width),
        clamp((bottom + margin).ceil(), height),
    ];
    (region[0] < region[2] && region[1] < region[3]).then_some(region)
}

pub fn process_pixel_seek(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
) -> PixelSeekOutput {
    let width = image.width();
    let height = image.height();
    let lut = profile_lut(color);
    let mask: Vec<u8> = image
        .as_raw()
        .chunks_exact(3)
        .map(|pixel| lut_hit(lut, pixel[0], pixel[1], pixel[2]))
        .collect();
    // Exactly one target per frame. While a fresh lock exists, take the
    // topmost FOV pixel near the locked target so two targets in the FOV do
    // not alternate; when the locked target is gone, take the topmost mask
    // pixel in the whole FOV. Only the blob containing it is reported.
    let found = locked
        .and_then(|previous| lock_region(previous, source_crop, width, height))
        .and_then(|region| highest_pixel_in_fov_within(&mask, width, height, fov_degrees, region))
        .or_else(|| highest_pixel_in_fov(&mask, width, height, fov_degrees));
    let Some(top) = found else {
        return PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
        };
    };
    let start = (top.y as u32 * width + top.x as u32) as usize;
    let mut visited = vec![false; mask.len()];
    let (class_id, class_name) = color.class();
    let mut objects = Vec::with_capacity(1);
    {
        let mut stack = vec![start];
        visited[start] = true;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
        let mut count = 0u32;
        while let Some(index) = stack.pop() {
            let x = index as u32 % width;
            let y = index as u32 / width;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            count += 1;
            let neighbors = [
                (x.checked_sub(1), Some(y)),
                (Some(x + 1).filter(|nx| *nx < width), Some(y)),
                (Some(x), y.checked_sub(1)),
                (Some(x), Some(y + 1).filter(|ny| *ny < height)),
            ];
            for (nx, ny) in neighbors {
                if let (Some(nx), Some(ny)) = (nx, ny) {
                    let next = (ny * width + nx) as usize;
                    if mask[next] != 0 && !visited[next] {
                        visited[next] = true;
                        stack.push(next);
                    }
                }
            }
        }
        objects.push(DetectionObject {
            class_id,
            class_name: class_name.into(),
            bbox_xyxy: [
                min_x as f32 + source_crop.x as f32,
                min_y as f32 + source_crop.y as f32,
                (max_x + 1) as f32 + source_crop.x as f32,
                (max_y + 1) as f32 + source_crop.y as f32,
            ],
            confidence: None,
            pixel_count: Some(count),
        });
    }
    let target = Some(FrameCoordinate {
        x: top.x + source_crop.x as f32,
        y: top.y + source_crop.y as f32,
    });
    PixelSeekOutput {
        objects,
        target,
        mask,
    }
}

/// One bit per 24-bit RGB color (2 MiB per profile): the HSV test below,
/// evaluated once for every color, so a frame costs one lookup per pixel
/// instead of a float HSV conversion. Results are identical by construction.
static PROFILE_LUTS: [OnceLock<Box<[u64]>>; 3] =
    [OnceLock::new(), OnceLock::new(), OnceLock::new()];

fn profile_lut(color: PixelTargetColor) -> &'static [u64] {
    PROFILE_LUTS[color as usize].get_or_init(|| {
        let mut bits = vec![0u64; (1 << 24) / 64].into_boxed_slice();
        for rgb in 0..1u32 << 24 {
            let (h, s, v) = rgb_to_hsv((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
            if in_profile(color, h, s, v) {
                bits[(rgb >> 6) as usize] |= 1 << (rgb & 63);
            }
        }
        bits
    })
}

#[inline]
fn lut_hit(lut: &[u64], r: u8, g: u8, b: u8) -> u8 {
    let rgb = (r as usize) << 16 | (g as usize) << 8 | b as usize;
    ((lut[rgb >> 6] >> (rgb & 63)) & 1) as u8
}

/// Builds all three profile tables off the processing thread so the first
/// frame after start or a color change does not pay for it.
pub fn warm_profile_luts() {
    let _ = std::thread::Builder::new()
        .name("pixel-seek-lut".into())
        .spawn(|| {
            for color in [
                PixelTargetColor::Yellow,
                PixelTargetColor::Red,
                PixelTargetColor::Purple,
            ] {
                profile_lut(color);
            }
        });
}

fn in_profile(color: PixelTargetColor, h: f32, s: f32, v: f32) -> bool {
    match color {
        PixelTargetColor::Red => {
            (0.0..=10.0).contains(&h) && (50.0..=255.0).contains(&s) && (180.0..=255.0).contains(&v)
        }
        PixelTargetColor::Yellow => {
            (20.0..=40.0).contains(&h)
                && (157.0..=255.0).contains(&s)
                && (252.0..=255.0).contains(&v)
        }
        PixelTargetColor::Purple => {
            (140.0..=160.0).contains(&h)
                && (60.0..=255.0).contains(&s)
                && (240.0..=255.0).contains(&v)
        }
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max == 0.0 { 0.0 } else { delta / max * 255.0 };
    (hue / 2.0, saturation, max * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn yellow_profile_detects_exact_rgb_and_keeps_confidence_null() {
        let image = RgbImage::from_pixel(8, 8, Rgb([0, 0, 0]));
        let mut image = image;
        for y in 2..5 {
            for x in 3..6 {
                image.put_pixel(x, y, Rgb([255, 255, 0]));
            }
        }
        let output = process_pixel_seek(
            &image,
            CropRect {
                x: 10,
                y: 20,
                width: 8,
                height: 8,
            },
            PixelTargetColor::Yellow,
            360,
            None,
        );
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [13.0, 22.0, 16.0, 25.0]);
        assert_eq!(output.objects[0].confidence, None);
        assert_eq!(output.objects[0].pixel_count, Some(9));
    }

    #[test]
    fn lookup_tables_match_the_hsv_profiles_exactly() {
        for color in [
            PixelTargetColor::Yellow,
            PixelTargetColor::Red,
            PixelTargetColor::Purple,
        ] {
            let lut = profile_lut(color);
            // A coprime stride walks ~450k colors spread over the whole cube.
            for rgb in (0..1u32 << 24).step_by(37) {
                let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
                let (h, s, v) = rgb_to_hsv(r, g, b);
                assert_eq!(
                    lut_hit(lut, r, g, b),
                    u8::from(in_profile(color, h, s, v)),
                    "{color:?} {r},{g},{b}"
                );
            }
        }
        // Profile edges from the spec.
        assert_eq!(lut_hit(profile_lut(PixelTargetColor::Red), 255, 0, 0), 1);
        assert_eq!(
            lut_hit(profile_lut(PixelTargetColor::Yellow), 255, 255, 0),
            1
        );
        assert_eq!(lut_hit(profile_lut(PixelTargetColor::Red), 255, 255, 0), 0);
        assert_eq!(lut_hit(profile_lut(PixelTargetColor::Purple), 0, 0, 0), 0);
    }

    fn paint(image: &mut RgbImage, x0: u32, y0: u32, size: u32) {
        for y in y0..y0 + size {
            for x in x0..x0 + size {
                image.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
    }

    fn crop(size: u32) -> CropRect {
        CropRect {
            x: 0,
            y: 0,
            width: size,
            height: size,
        }
    }

    #[test]
    fn only_the_topmost_blob_inside_fov_becomes_the_single_target() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 45, 60, 6); // lower blob
        paint(&mut image, 52, 30, 4); // higher blob, still inside a 360° FOV
        let output = process_pixel_seek(&image, crop(100), PixelTargetColor::Red, 360, None);
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [52.0, 30.0, 56.0, 34.0]);
        let target = output.target.expect("target");
        assert_eq!((target.y, (52.0..56.0).contains(&target.x)), (30.5, true));
        // The whole frame is still masked.
        assert_eq!(
            output.mask.iter().filter(|value| **value != 0).count(),
            36 + 16
        );
    }

    #[test]
    fn a_locked_target_is_kept_when_another_higher_target_is_in_the_fov() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 50, 4); // locked target
        paint(&mut image, 30, 20, 4); // separate, higher target elsewhere
        let unlocked = process_pixel_seek(&image, crop(100), PixelTargetColor::Red, 360, None);
        assert_eq!(unlocked.objects[0].bbox_xyxy, [30.0, 20.0, 34.0, 24.0]);
        let locked = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert_eq!(locked.objects.len(), 1);
        assert_eq!(locked.objects[0].bbox_xyxy, [60.0, 50.0, 64.0, 54.0]);
        assert_eq!(locked.target.unwrap().y, 50.5);
    }

    #[test]
    fn a_locked_target_still_climbs_to_higher_pixels_next_to_it() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 50, 4); // previous target box
        paint(&mut image, 61, 42, 2); // higher pixels appearing just above it
        paint(&mut image, 30, 10, 4); // unrelated higher target
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert_eq!(output.target.unwrap().y, 42.5);
        assert_eq!(output.objects[0].bbox_xyxy, [61.0, 42.0, 63.0, 44.0]);
    }

    #[test]
    fn a_lost_lock_falls_back_to_the_topmost_target_in_the_fov() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 30, 20, 4);
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert_eq!(output.objects[0].bbox_xyxy, [30.0, 20.0, 34.0, 24.0]);
    }

    #[test]
    fn blobs_outside_the_fov_are_masked_but_never_targeted() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 2, 2, 5); // top-left corner, outside a small FOV
        paint(&mut image, 48, 55, 4); // near the center
        let small = process_pixel_seek(&image, crop(100), PixelTargetColor::Red, 60, None);
        assert_eq!(small.objects.len(), 1);
        assert_eq!(small.objects[0].bbox_xyxy, [48.0, 55.0, 52.0, 59.0]);
        let none = process_pixel_seek(
            &RgbImage::from_fn(100, 100, |x, y| {
                if x < 5 && y < 5 {
                    Rgb([255, 0, 0])
                } else {
                    Rgb([0, 0, 0])
                }
            }),
            crop(100),
            PixelTargetColor::Red,
            60,
            None,
        );
        assert!(none.objects.is_empty() && none.target.is_none());
    }
}
