use crate::crop::CropRect;
use crate::results::DetectionObject;
use crate::targeting::FrameCoordinate;
use image::RgbImage;
use std::sync::OnceLock;

const SCRATCH_RETAIN_LIMIT: usize = 8 * 1024 * 1024;

/// Target colors shared by Motion and Auto. The selected module only changes
/// which profile is active; HSV thresholds and lookup tables stay centralized.
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
    /// Every accepted cluster, in original-frame coordinates.
    pub objects: Vec<DetectionObject>,
    pub mask: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct PixelSeekScratch {
    mask: Vec<u8>,
    clusters: Vec<PixelCluster>,
}

impl PixelSeekScratch {
    pub(crate) fn take_mask(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.mask)
    }

    pub(crate) fn recycle_mask(&mut self, mask: Vec<u8>) {
        let mut mask = mask;
        let len = mask.len();
        shrink_if_oversized(&mut mask, len);
        self.mask = mask;
    }
}

fn shrink_if_oversized<T>(buffer: &mut Vec<T>, minimum_capacity: usize) {
    if buffer.capacity() > minimum_capacity.saturating_mul(4)
        && buffer.capacity().saturating_mul(std::mem::size_of::<T>()) > SCRATCH_RETAIN_LIMIT
    {
        buffer.truncate(minimum_capacity);
        buffer.shrink_to(minimum_capacity);
    }
}

/// Builds the shared color mask used by Motion selection and Auto's
/// center-crossing target gate.
pub fn color_mask(image: &RgbImage, color: PixelTargetColor) -> Vec<u8> {
    let pixels = image.as_raw().chunks_exact(3);
    if let Some(lut) = profile_lut(color) {
        pixels
            .map(|pixel| profile_hit(color, Some(lut), pixel[0], pixel[1], pixel[2]))
            .collect()
    } else {
        pixels
            .map(|pixel| profile_hit(color, None, pixel[0], pixel[1], pixel[2]))
            .collect()
    }
}

pub(crate) fn color_mask_into(
    image: &RgbImage,
    color: PixelTargetColor,
    mask: &mut Vec<u8>,
) -> bool {
    let pixels = image.as_raw().len() / 3;
    if mask.len() < pixels {
        if mask.try_reserve_exact(pixels - mask.len()).is_err() {
            return false;
        }
        mask.resize(pixels, 0);
    } else {
        mask.truncate(pixels);
    }
    if let Some(lut) = profile_lut(color) {
        for (hit, pixel) in mask.iter_mut().zip(image.as_raw().chunks_exact(3)) {
            *hit = profile_hit(color, Some(lut), pixel[0], pixel[1], pixel[2]);
        }
    } else {
        for (hit, pixel) in mask.iter_mut().zip(image.as_raw().chunks_exact(3)) {
            *hit = profile_hit(color, None, pixel[0], pixel[1], pixel[2]);
        }
    }
    true
}

/// Color filter: the image with every pixel outside the color's profile
/// turned black, and the mask that decided it.
pub fn color_filtered(image: &RgbImage, color: PixelTargetColor) -> (RgbImage, Vec<u8>) {
    let mask = color_mask(image, color);
    (apply_color_mask(image, &mask), mask)
}

pub(crate) fn apply_color_mask(image: &RgbImage, mask: &[u8]) -> RgbImage {
    let mut filtered = image.clone();
    for (pixel, keep) in filtered.as_mut().chunks_exact_mut(3).zip(mask) {
        if *keep == 0 {
            pixel.fill(0);
        }
    }
    filtered
}

/// Packs a color mask to one bit per pixel, row-major, least significant bit
/// first (pixel `i` is bit `i % 8` of byte `i / 8`) for the Preview color filter.
pub fn pack_mask(mask: &[u8]) -> Option<Vec<u8>> {
    try_pack_mask(mask)
}

pub(crate) fn try_pack_mask(mask: &[u8]) -> Option<Vec<u8>> {
    let packed_len = mask.len().div_ceil(8);
    let mut packed = Vec::new();
    packed.try_reserve_exact(packed_len).ok()?;
    packed.resize(packed_len, 0);
    for (index, value) in mask.iter().enumerate() {
        if *value != 0 {
            packed[index / 8] |= 1 << (index % 8);
        }
    }
    Some(packed)
}

/// Cluster filters and head point (from the user's `pixel_seek.rs`).
pub const MIN_CLUSTER_SIZE: u32 = 12;
pub const MAX_CLUSTER_SIZE: u32 = 6000;
pub const MIN_ASPECT_RATIO: f32 = 0.20;
pub const MAX_ASPECT_RATIO: f32 = 1.30;
/// The aim point sits this fraction of the cluster height below its top.
pub const HEAD_RATIO_Y: f32 = 0.14;
/// A mask pixel joins the first cluster whose box center is this close.
pub const MERGE_DISTANCE_PX: u32 = 18;
/// Bound on open clusters per frame; pixels that would open more are
/// ignored so a noisy frame cannot make clustering quadratic.
pub const MAX_CLUSTERS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelCluster {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
    pub count: u32,
}

impl PixelCluster {
    pub fn width(&self) -> u32 {
        self.max_x - self.min_x + 1
    }

    pub fn height(&self) -> u32 {
        self.max_y - self.min_y + 1
    }

    pub fn aspect_ratio(&self) -> f32 {
        self.width() as f32 / self.height() as f32
    }

    fn accepted(&self) -> bool {
        (MIN_CLUSTER_SIZE..=MAX_CLUSTER_SIZE).contains(&self.count)
            && (MIN_ASPECT_RATIO..=MAX_ASPECT_RATIO).contains(&self.aspect_ratio())
    }
}

/// Head point of a cluster box `[left, top, right, bottom)`: horizontally
/// centered, `HEAD_RATIO_Y` of the height below the top.
pub fn head_point(bbox_xyxy: [f32; 4]) -> FrameCoordinate {
    let [left, top, right, bottom] = bbox_xyxy;
    FrameCoordinate {
        x: (left + right) * 0.5,
        y: top + (bottom - top) * HEAD_RATIO_Y,
    }
}

/// Groups mask pixels, in row-major order, into clusters: each pixel joins
/// the first cluster whose box center is within `MERGE_DISTANCE_PX`, else
/// opens a new one. Keeps clusters that pass the size and aspect filters.
fn cluster_mask(mask: &[u8], width: u32, clusters: &mut Vec<PixelCluster>) {
    clusters.clear();
    let merge_squared = i64::from(MERGE_DISTANCE_PX * MERGE_DISTANCE_PX);
    for (index, _) in mask.iter().enumerate().filter(|(_, hit)| **hit != 0) {
        let x = index as u32 % width;
        let y = index as u32 / width;
        let joined = clusters.iter().position(|cluster| {
            let dx = i64::from(x) - i64::from((cluster.min_x + cluster.max_x) / 2);
            let dy = i64::from(y) - i64::from((cluster.min_y + cluster.max_y) / 2);
            dx * dx + dy * dy <= merge_squared
        });
        match joined {
            Some(found) => {
                let cluster = &mut clusters[found];
                cluster.min_x = cluster.min_x.min(x);
                cluster.max_x = cluster.max_x.max(x);
                cluster.min_y = cluster.min_y.min(y);
                cluster.max_y = cluster.max_y.max(y);
                cluster.count += 1;
            }
            None if clusters.len() < MAX_CLUSTERS => clusters.push(PixelCluster {
                min_x: x,
                min_y: y,
                max_x: x,
                max_y: y,
                count: 1,
            }),
            None => {}
        }
    }
    clusters.retain(PixelCluster::accepted);
}

pub fn process_pixel_seek(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
) -> PixelSeekOutput {
    process_pixel_seek_with_scratch(
        image,
        source_crop,
        color,
        color_mask(image, color),
        &mut PixelSeekScratch::default(),
    )
}

/// Masks the whole frame and reports every accepted cluster; target
/// selection (FOV, tracking, lock) happens afterwards.
pub(crate) fn process_pixel_seek_with_scratch(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    mask: Vec<u8>,
    scratch: &mut PixelSeekScratch,
) -> PixelSeekOutput {
    let width = image.width();
    let height = image.height();
    if width == 0
        || height == 0
        || mask.len() != width as usize * height as usize
        || mask.len() > u32::MAX as usize
    {
        return PixelSeekOutput {
            objects: Vec::new(),
            mask,
        };
    }
    cluster_mask(&mask, width, &mut scratch.clusters);
    let (class_id, class_name) = color.class();
    let (ox, oy) = (source_crop.x as f32, source_crop.y as f32);
    let objects = scratch
        .clusters
        .iter()
        .map(|cluster| DetectionObject {
            class_id,
            class_name: class_name.into(),
            bbox_xyxy: [
                cluster.min_x as f32 + ox,
                cluster.min_y as f32 + oy,
                (cluster.max_x + 1) as f32 + ox,
                (cluster.max_y + 1) as f32 + oy,
            ],
            confidence: None,
            pixel_count: Some(cluster.count),
        })
        .collect();
    PixelSeekOutput { objects, mask }
}

/// One bit per 24-bit RGB color (2 MiB per profile): the HSV test below,
/// evaluated once for every color, so a frame costs one lookup per pixel
/// instead of a float HSV conversion. Results are identical by construction.
static PROFILE_LUTS: [OnceLock<Option<Box<[u64]>>>; 3] =
    [OnceLock::new(), OnceLock::new(), OnceLock::new()];

// A failed allocation stays on the HSV fallback until restart; avoid a per-frame retry storm.
fn profile_lut(color: PixelTargetColor) -> Option<&'static [u64]> {
    PROFILE_LUTS[color as usize]
        .get_or_init(|| {
            let mut bits = Vec::new();
            if bits.try_reserve_exact((1 << 24) / 64).is_err() {
                return None;
            }
            bits.resize((1 << 24) / 64, 0u64);
            for rgb in 0..1u32 << 24 {
                let (h, s, v) = rgb_to_hsv((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
                if in_profile(color, h, s, v) {
                    bits[(rgb >> 6) as usize] |= 1 << (rgb & 63);
                }
            }
            Some(bits.into_boxed_slice())
        })
        .as_deref()
}

fn hsv_profile_hit(color: PixelTargetColor, r: u8, g: u8, b: u8) -> u8 {
    let (h, s, v) = rgb_to_hsv(r, g, b);
    u8::from(in_profile(color, h, s, v))
}

#[inline]
fn profile_hit(color: PixelTargetColor, lut: Option<&[u64]>, r: u8, g: u8, b: u8) -> u8 {
    lut.map_or_else(
        || hsv_profile_hit(color, r, g, b),
        |lut| lut_hit(lut, r, g, b),
    )
}

#[inline]
fn lut_hit(lut: &[u64], r: u8, g: u8, b: u8) -> u8 {
    let rgb = (r as usize) << 16 | (g as usize) << 8 | b as usize;
    ((lut[rgb >> 6] >> (rgb & 63)) & 1) as u8
}

/// Build all shared tables before the receiver accepts frames so a cold
/// profile can never stall target tracking on the processing worker.
pub fn warm_profile_luts() {
    for color in [
        PixelTargetColor::Yellow,
        PixelTargetColor::Red,
        PixelTargetColor::Purple,
    ] {
        profile_lut(color);
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

    #[test]
    fn color_filter_keeps_only_the_selected_color() {
        let mut image = RgbImage::from_pixel(4, 2, image::Rgb([30, 200, 60]));
        image.put_pixel(1, 0, image::Rgb([255, 255, 0]));
        image.put_pixel(3, 1, image::Rgb([255, 0, 0]));
        let (filtered, mask) = color_filtered(&image, PixelTargetColor::Yellow);
        assert_eq!(mask.iter().filter(|value| **value != 0).count(), 1);
        for (x, y, pixel) in filtered.enumerate_pixels() {
            let expected = if (x, y) == (1, 0) {
                [255, 255, 0]
            } else {
                [0, 0, 0]
            };
            assert_eq!(pixel.0, expected, "({x},{y})");
        }
    }

    #[test]
    fn reusable_color_mask_matches_reference_without_reallocating() {
        let image = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 0]));
        let mut reusable = Vec::new();
        assert!(color_mask_into(
            &image,
            PixelTargetColor::Yellow,
            &mut reusable
        ));
        let allocation = reusable.as_ptr();
        assert!(color_mask_into(
            &image,
            PixelTargetColor::Yellow,
            &mut reusable
        ));
        assert_eq!(reusable.as_ptr(), allocation);
        assert_eq!(reusable, color_mask(&image, PixelTargetColor::Yellow));
    }

    #[test]
    fn recycled_mask_releases_large_capacity_after_resolution_drops() {
        let mut scratch = PixelSeekScratch::default();
        let mut mask = vec![0; SCRATCH_RETAIN_LIMIT + 4096];
        mask.truncate(16);

        scratch.recycle_mask(mask);

        assert_eq!(scratch.mask.len(), 16);
        assert!(scratch.mask.capacity() <= SCRATCH_RETAIN_LIMIT);
    }

    #[test]
    fn packed_mask_puts_pixel_i_in_bit_i_mod_8_of_byte_i_div_8() {
        let mut mask = vec![0u8; 19];
        for index in [0, 3, 7, 8, 18] {
            mask[index] = 1;
        }
        assert_eq!(
            super::pack_mask(&mask),
            Some(vec![0b1000_1001, 0b0000_0001, 0b0000_0100])
        );
        assert_eq!(
            super::try_pack_mask(&mask),
            Some(vec![0b1000_1001, 0b0000_0001, 0b0000_0100])
        );
    }
    use image::Rgb;

    fn paint(image: &mut RgbImage, x0: u32, y0: u32, width: u32, height: u32) {
        for y in y0..y0 + height {
            for x in x0..x0 + width {
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
    fn a_cluster_is_reported_in_original_frame_coordinates_without_confidence() {
        let mut image = RgbImage::from_pixel(64, 64, Rgb([0, 0, 0]));
        paint(&mut image, 3, 2, 4, 5);
        let output = process_pixel_seek(
            &image,
            CropRect {
                x: 10,
                y: 20,
                width: 64,
                height: 64,
            },
            PixelTargetColor::Red,
        );
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [13.0, 22.0, 17.0, 27.0]);
        assert_eq!(output.objects[0].confidence, None);
        assert_eq!(output.objects[0].pixel_count, Some(20));
    }

    #[test]
    fn clusters_are_filtered_by_size_and_aspect_ratio() {
        let mut image = RgbImage::from_pixel(200, 200, Rgb([0, 0, 0]));
        paint(&mut image, 10, 10, 3, 3); // 9 px: too small
        paint(&mut image, 60, 10, 30, 2); // too wide: no part reaches 1.3 or less
        paint(&mut image, 150, 10, 4, 30); // 0.13: too narrow
        paint(&mut image, 10, 100, 8, 12); // accepted
        let output = process_pixel_seek(&image, crop(200), PixelTargetColor::Red);
        let boxes: Vec<_> = output.objects.iter().map(|o| o.bbox_xyxy).collect();
        assert_eq!(boxes, vec![[10.0, 100.0, 18.0, 112.0]]);
    }

    #[test]
    fn pixels_farther_than_the_merge_distance_open_a_new_cluster() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 10, 10, 4, 4);
        paint(&mut image, 50, 10, 4, 4);
        let output = process_pixel_seek(&image, crop(100), PixelTargetColor::Red);
        let boxes: Vec<_> = output.objects.iter().map(|o| o.bbox_xyxy).collect();
        assert_eq!(
            boxes,
            vec![[10.0, 10.0, 14.0, 14.0], [50.0, 10.0, 54.0, 14.0]]
        );
    }

    #[test]
    fn the_head_point_is_centered_and_14_percent_below_the_top() {
        let head = head_point([10.0, 100.0, 30.0, 150.0]);
        assert_eq!(head.x, 20.0);
        assert!((head.y - 107.0).abs() < 1e-4);
    }

    #[test]
    fn lookup_tables_match_the_hsv_profiles_exactly() {
        for color in [
            PixelTargetColor::Yellow,
            PixelTargetColor::Red,
            PixelTargetColor::Purple,
        ] {
            let lut = profile_lut(color).expect("profile lookup table allocation");
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
        assert_eq!(hsv_profile_hit(PixelTargetColor::Red, 255, 0, 0), 1);
        assert_eq!(
            lut_hit(profile_lut(PixelTargetColor::Yellow).unwrap(), 255, 255, 0),
            1
        );
        assert_eq!(
            profile_hit(PixelTargetColor::Red, None, 255, 0, 0),
            1,
            "HSV fallback preserves detection if a lookup table cannot be allocated"
        );
        assert_eq!(
            lut_hit(profile_lut(PixelTargetColor::Red).unwrap(), 255, 255, 0),
            0
        );
        assert_eq!(
            lut_hit(profile_lut(PixelTargetColor::Purple).unwrap(), 0, 0, 0),
            0
        );
    }
}
