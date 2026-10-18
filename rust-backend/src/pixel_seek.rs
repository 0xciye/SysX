use crate::crop::CropRect;
use crate::results::DetectionObject;
use crate::targeting::{highest_pixel_in_fov, highest_pixel_in_fov_within, FrameCoordinate};
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
    pub objects: Vec<DetectionObject>,
    pub target: Option<FrameCoordinate>,
    pub mask: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct PixelSeekScratch {
    mask: Vec<u8>,
    visited_words: Vec<u64>,
    visited_epochs: Vec<u32>,
    // Frame indices fit in u32 under the receiver's pixel budget and halve
    // flood-fill peak memory on 64-bit hosts.
    stack: Vec<u32>,
    generation: u32,
}

impl PixelSeekScratch {
    fn release_oversized(&mut self, pixels: usize) {
        let words = pixels.div_ceil(u64::BITS as usize);
        shrink_if_oversized(&mut self.visited_words, words);
        shrink_if_oversized(&mut self.visited_epochs, words);
        self.stack.clear();
        shrink_if_oversized(&mut self.stack, pixels);
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.visited_epochs.fill(0);
            self.generation = 1;
        }
    }

    fn mark_visited(&mut self, index: usize) -> bool {
        let word = index / u64::BITS as usize;
        if self.visited_epochs[word] != self.generation {
            self.visited_words[word] = 0;
            self.visited_epochs[word] = self.generation;
        }
        let bit = 1u64 << (index % u64::BITS as usize);
        let was_visited = self.visited_words[word] & bit != 0;
        self.visited_words[word] |= bit;
        !was_visited
    }
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

struct PixelBlob {
    bbox_xyxy: [f32; 4],
    count: u32,
}

/// The connected (8-neighbour) mask component containing `seed`.
fn extract_blob(
    mask: &[u8],
    width: u32,
    height: u32,
    source_crop: CropRect,
    seed: FrameCoordinate,
    scratch: &mut PixelSeekScratch,
) -> Result<PixelBlob, ()> {
    let start = (seed.y as u32 * width + seed.x as u32) as usize;
    scratch.stack.clear();
    if scratch.stack.capacity() < 64 && scratch.stack.try_reserve(64).is_err() {
        return Err(());
    }
    scratch.stack.push(start as u32);
    scratch.mark_visited(start);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    let mut count = 0u32;

    while let Some(index) = scratch.stack.pop() {
        let index = index as usize;
        let x = index as u32 % width;
        let y = index as u32 / width;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
        count += 1;
        for ny in y.saturating_sub(1)..=y.saturating_add(1).min(height - 1) {
            for nx in x.saturating_sub(1)..=x.saturating_add(1).min(width - 1) {
                if nx == x && ny == y {
                    continue;
                }
                let next = (ny * width + nx) as usize;
                if mask[next] != 0 && scratch.mark_visited(next) {
                    if scratch.stack.len() == scratch.stack.capacity() {
                        let additional = scratch.stack.capacity().max(64);
                        if scratch.stack.try_reserve(additional).is_err() {
                            return Err(());
                        }
                    }
                    scratch.stack.push(next as u32);
                }
            }
        }
    }
    Ok(PixelBlob {
        bbox_xyxy: [
            min_x as f32 + source_crop.x as f32,
            min_y as f32 + source_crop.y as f32,
            (max_x + 1) as f32 + source_crop.x as f32,
            (max_y + 1) as f32 + source_crop.y as f32,
        ],
        count,
    })
}

pub fn process_pixel_seek(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
) -> PixelSeekOutput {
    process_pixel_seek_with_scratch(
        image,
        source_crop,
        color,
        fov_degrees,
        locked,
        color_mask(image, color),
        &mut PixelSeekScratch::default(),
    )
}

/// Exactly one target per frame. While a fresh lock exists, take the topmost
/// FOV pixel near the locked target so two targets in the FOV do not
/// alternate; when the locked target is gone, take the topmost mask pixel in
/// the whole FOV. Only the blob containing it is reported.
pub(crate) fn process_pixel_seek_with_scratch(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    mask: Vec<u8>,
    scratch: &mut PixelSeekScratch,
) -> PixelSeekOutput {
    let width = image.width();
    let height = image.height();
    scratch.release_oversized(width as usize * height as usize);
    let empty = |mask| PixelSeekOutput {
        objects: Vec::new(),
        target: None,
        mask,
    };
    if width == 0
        || height == 0
        || mask.len() != width as usize * height as usize
        || mask.len() > u32::MAX as usize
    {
        return empty(mask);
    }
    let found = locked
        .and_then(|previous| lock_region(previous, source_crop, width, height))
        .and_then(|region| {
            highest_pixel_in_fov_within(&mask, width, height, fov_degrees, region, None)
        })
        .or_else(|| highest_pixel_in_fov(&mask, width, height, fov_degrees));
    let Some(top) = found else {
        return empty(mask);
    };
    let visited_words = mask.len().div_ceil(u64::BITS as usize);
    if scratch.visited_words.len() < visited_words {
        let more = visited_words - scratch.visited_words.len();
        if scratch.visited_words.try_reserve_exact(more).is_err()
            || scratch.visited_epochs.try_reserve_exact(more).is_err()
        {
            return empty(mask);
        }
        scratch.visited_words.resize(visited_words, 0);
        scratch.visited_epochs.resize(visited_words, 0);
    }
    scratch.advance_generation();
    let (class_id, class_name) = color.class();
    let Ok(blob) = extract_blob(&mask, width, height, source_crop, top, scratch) else {
        return empty(mask);
    };
    PixelSeekOutput {
        objects: vec![DetectionObject {
            class_id,
            class_name: class_name.into(),
            bbox_xyxy: blob.bbox_xyxy,
            confidence: None,
            pixel_count: Some(blob.count),
        }],
        target: Some(FrameCoordinate {
            x: top.x + source_crop.x as f32,
            y: top.y + source_crop.y as f32,
        }),
        mask,
    }
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
    fn flood_fill_stack_is_not_trimmed_below_the_current_frame_size() {
        let mut scratch = PixelSeekScratch {
            stack: Vec::with_capacity(5 * 1024 * 1024),
            ..Default::default()
        };
        let capacity = scratch.stack.capacity();

        scratch.release_oversized(2 * 1024 * 1024);

        assert_eq!(scratch.stack.capacity(), capacity);
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
    fn diagonally_adjacent_color_pixels_remain_one_target_component() {
        let mut image = RgbImage::from_pixel(64, 64, Rgb([0, 0, 0]));
        for offset in 0..4 {
            image.put_pixel(30 + offset, 30 + offset, Rgb([255, 0, 0]));
        }
        let output = process_pixel_seek(&image, crop(64), PixelTargetColor::Red, 360, None);
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [30.0, 30.0, 34.0, 34.0]);
        assert_eq!(output.objects[0].pixel_count, Some(4));
        assert_eq!(output.target, Some(FrameCoordinate { x: 30.5, y: 30.5 }));
    }

    #[test]
    fn symmetric_topmost_pixels_choose_the_left_pixel_deterministically() {
        let mut image = RgbImage::from_pixel(64, 64, Rgb([0, 0, 0]));
        image.put_pixel(30, 20, Rgb([255, 0, 0]));
        image.put_pixel(33, 20, Rgb([255, 0, 0]));
        for x in 30..=33 {
            image.put_pixel(x, 21, Rgb([255, 0, 0]));
        }

        let output = process_pixel_seek(&image, crop(64), PixelTargetColor::Red, 360, None);

        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.target, Some(FrameCoordinate { x: 30.5, y: 20.5 }));
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
    fn pixel_seek_reuses_flood_fill_buffers_between_frames() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 45, 40, 6);
        let mut scratch = PixelSeekScratch::default();
        let process = |scratch: &mut PixelSeekScratch| {
            process_pixel_seek_with_scratch(
                &image,
                crop(100),
                PixelTargetColor::Red,
                360,
                None,
                color_mask(&image, PixelTargetColor::Red),
                scratch,
            )
        };
        let first = process(&mut scratch);
        assert_eq!(first.objects[0].bbox_xyxy, [45.0, 40.0, 51.0, 46.0]);
        let visited_words = scratch.visited_words.as_ptr();
        let visited_epochs = scratch.visited_epochs.as_ptr();
        let stack = scratch.stack.as_ptr();
        let capacities = (
            scratch.visited_words.capacity(),
            scratch.visited_epochs.capacity(),
            scratch.stack.capacity(),
        );
        let target = first.target.unwrap();
        let target_index = target.y as usize * image.width() as usize + target.x as usize;
        let word = target_index / u64::BITS as usize;
        let stale_index = (word * u64::BITS as usize
            ..((word + 1) * u64::BITS as usize).min(first.mask.len()))
            .find(|index| first.mask[*index] == 0)
            .unwrap();
        let stale_bit = 1u64 << (stale_index % u64::BITS as usize);
        scratch.generation = u8::MAX as u32;
        scratch.visited_epochs[word] = scratch.generation;
        scratch.visited_words[word] = stale_bit;
        let second = process(&mut scratch);
        assert_eq!(second.objects[0].bbox_xyxy, [45.0, 40.0, 51.0, 46.0]);
        assert_eq!(scratch.generation, u8::MAX as u32 + 1);
        assert_eq!(scratch.visited_epochs[word], scratch.generation);
        assert_eq!(scratch.visited_words[word] & stale_bit, 0);
        assert_eq!(scratch.visited_words.as_ptr(), visited_words);
        assert_eq!(scratch.visited_epochs.as_ptr(), visited_epochs);
        assert_eq!(scratch.stack.as_ptr(), stack);
        assert_eq!(
            (
                scratch.visited_words.capacity(),
                scratch.visited_epochs.capacity(),
                scratch.stack.capacity(),
            ),
            capacities
        );
    }

    #[test]
    fn generation_wrap_clears_epochs_for_previously_larger_frames() {
        let mut scratch = PixelSeekScratch {
            visited_epochs: vec![2; 16],
            generation: u32::MAX,
            ..Default::default()
        };

        scratch.advance_generation();

        assert_eq!(scratch.generation, 1);
        assert!(scratch.visited_epochs.iter().all(|epoch| *epoch == 0));
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
    fn a_single_valid_pixel_near_the_fov_edge_is_detected() {
        let mut image = RgbImage::from_pixel(64, 64, Rgb([0, 0, 0]));
        image.put_pixel(32, 1, Rgb([255, 255, 0]));

        let output = process_pixel_seek(&image, crop(64), PixelTargetColor::Yellow, 360, None);

        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [32.0, 1.0, 33.0, 2.0]);
        assert_eq!(output.objects[0].pixel_count, Some(1));
        assert_eq!(output.target, Some(FrameCoordinate { x: 32.5, y: 1.5 }));
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
