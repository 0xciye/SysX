use crate::crop::CropRect;
use crate::results::DetectionObject;
use crate::targeting::{highest_pixel_in_fov, FrameCoordinate};
use image::RgbImage;

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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yellow => "yellow",
            Self::Red => "red",
            Self::Purple => "purple",
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

pub fn process_pixel_seek(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
) -> PixelSeekOutput {
    let width = image.width();
    let height = image.height();
    let mut mask = vec![0u8; (width * height) as usize];
    for (x, y, pixel) in image.enumerate_pixels() {
        let (h, s, v) = rgb_to_hsv(pixel.0[0], pixel.0[1], pixel.0[2]);
        mask[(y * width + x) as usize] = u8::from(in_profile(color, h, s, v));
    }
    // Exactly one target: the topmost mask pixel inside the FOV. Only the blob
    // containing it becomes the (single) detection; other colored regions are
    // masked but ignored.
    let Some(top) = highest_pixel_in_fov(&mask, width, height, fov_degrees) else {
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
        );
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [13.0, 22.0, 16.0, 25.0]);
        assert_eq!(output.objects[0].confidence, None);
        assert_eq!(output.objects[0].pixel_count, Some(9));
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
        let output = process_pixel_seek(&image, crop(100), PixelTargetColor::Red, 360);
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
    fn blobs_outside_the_fov_are_masked_but_never_targeted() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 2, 2, 5); // top-left corner, outside a small FOV
        paint(&mut image, 48, 55, 4); // near the center
        let small = process_pixel_seek(&image, crop(100), PixelTargetColor::Red, 60);
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
        );
        assert!(none.objects.is_empty() && none.target.is_none());
    }
}
