use crate::crop::CropRect;
use crate::results::DetectionObject;
use crate::targeting::{fov_radius_px, fov_row_bounds, highest_pixel_in_fov, FrameCoordinate};
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
    /// No target was found inside the current fresh lock region.
    pub lock_miss: bool,
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

    fn is_visited(&self, index: usize) -> bool {
        let word = index / u64::BITS as usize;
        self.visited_epochs.get(word) == Some(&self.generation)
            && self.visited_words[word] & (1u64 << (index % u64::BITS as usize)) != 0
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
/// Reject detached candidates beyond this unpredicted frame-to-frame gap.
const MAX_LOCK_GAP_PX: f32 = 6.0;
/// Keep associations inside the lock region rather than accepting edge noise.
const MAX_LOCK_ASSOCIATION_DISTANCE_SQUARED: f32 = 1.0;
pub(crate) const MAX_PIXEL_TRACK_SPEED_PX_PER_SECOND: f32 = 64.0 * 360.0;
/// Prediction error may grow the search region, but never without a bound.
const MAX_LOCK_PREDICTION_ERROR_PX: f32 = 16.0;

/// Search window for a locked target: the previous blob's box (original-frame
/// coordinates) grown by a margin, in crop pixels `[left, top, right, bottom)`.
/// Higher pixels that appear next to the target fall inside it, so the target
/// keeps climbing; a separate blob elsewhere in the FOV does not.
fn lock_region(
    previous: [f32; 4],
    predicted_shift: [f32; 2],
    crop: CropRect,
    width: u32,
    height: u32,
) -> Option<[u32; 4]> {
    if !previous.iter().all(|value| value.is_finite()) {
        return None;
    }
    let predicted_shift = predicted_shift.map(|value| if value.is_finite() { value } else { 0.0 });
    let [left, top, right, bottom] = [
        previous[0] + predicted_shift[0] - crop.x as f32,
        previous[1] + predicted_shift[1] - crop.y as f32,
        previous[2] + predicted_shift[0] - crop.x as f32,
        previous[3] + predicted_shift[1] - crop.y as f32,
    ];
    if right <= 0.0 || bottom <= 0.0 || left >= width as f32 || top >= height as f32 {
        return None;
    }
    let prediction_error =
        predicted_shift.map(|value| (value.abs() * 0.25).min(MAX_LOCK_PREDICTION_ERROR_PX));
    let margin = LOCK_MARGIN_PX.max((right - left).max(bottom - top) * LOCK_MARGIN_RATIO);
    let margin_x = margin + prediction_error[0];
    let margin_y = margin + prediction_error[1];
    let clamp = |value: f32, max: u32| value.clamp(0.0, max as f32) as u32;
    let region = [
        clamp((left - margin_x).floor(), width),
        clamp((top - margin_y).floor(), height),
        clamp((right + margin_x).ceil(), width),
        clamp((bottom + margin_y).ceil(), height),
    ];
    (region[0] < region[2] && region[1] < region[3]).then_some(region)
}

struct PixelBlob {
    bbox_xyxy: [f32; 4],
    count: u32,
    target: FrameCoordinate,
}

fn extract_blob(
    mask: &[u8],
    width: u32,
    height: u32,
    source_crop: CropRect,
    seed: FrameCoordinate,
    anchor_x: f32,
    fov_radius: f32,
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
    let mut target_pixel: Option<(u32, u32)> = None;
    let mut target_x_distance_squared = f32::INFINITY;
    let (center_x, center_y) = (width as f32 / 2.0, height as f32 / 2.0);
    let mut allocation_failed = false;

    while let Some(index) = scratch.stack.pop() {
        let index = index as usize;
        let x = index as u32 % width;
        let y = index as u32 / width;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
        count += 1;
        let (dx, dy) = (x as f32 + 0.5 - center_x, y as f32 + 0.5 - center_y);
        if dx * dx + dy * dy <= fov_radius * fov_radius
            && target_pixel.is_none_or(|(_, target_y)| y <= target_y)
        {
            let x_delta = x as f32 + 0.5 - anchor_x;
            let x_distance_squared = x_delta * x_delta;
            if target_pixel.is_none_or(|(target_x, target_y)| {
                y < target_y
                    || (y == target_y
                        && (x_distance_squared < target_x_distance_squared
                            || (x_distance_squared == target_x_distance_squared && x < target_x)))
            }) {
                target_pixel = Some((x, y));
                target_x_distance_squared = x_distance_squared;
            }
        }
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
                            allocation_failed = true;
                            break;
                        }
                    }
                    scratch.stack.push(next as u32);
                }
            }
            if allocation_failed {
                break;
            }
        }
        if allocation_failed {
            break;
        }
    }
    if allocation_failed {
        return Err(());
    }
    let target = target_pixel.ok_or(())?;
    Ok(PixelBlob {
        bbox_xyxy: [
            min_x as f32 + source_crop.x as f32,
            min_y as f32 + source_crop.y as f32,
            (max_x + 1) as f32 + source_crop.x as f32,
            (max_y + 1) as f32 + source_crop.y as f32,
        ],
        count,
        target: FrameCoordinate {
            x: target.0 as f32 + 0.5 + source_crop.x as f32,
            y: target.1 as f32 + 0.5 + source_crop.y as f32,
        },
    })
}

fn lock_overlap_score(
    bbox: [f32; 4],
    previous: [f32; 4],
    predicted_shift: [f32; 2],
) -> Option<f32> {
    let predicted = [
        previous[0] + predicted_shift[0],
        previous[1] + predicted_shift[1],
        previous[2] + predicted_shift[0],
        previous[3] + predicted_shift[1],
    ];
    let x_gap = ((bbox[0] + bbox[2]) - (predicted[0] + predicted[2])).abs() * 0.5;
    let y_gap = ((bbox[1] + bbox[3]) - (predicted[1] + predicted[3])).abs() * 0.5;
    let lock_gap = MAX_LOCK_GAP_PX.max(
        (predicted[2] - predicted[0])
            .max(predicted[3] - predicted[1])
            .max(0.0)
            * LOCK_MARGIN_RATIO,
    );
    let prediction_error =
        predicted_shift.map(|value| (value.abs() * 0.25).min(MAX_LOCK_PREDICTION_ERROR_PX));
    let max_gap_x = lock_gap + prediction_error[0];
    let max_gap_y = lock_gap + prediction_error[1];
    if (x_gap / max_gap_x).powi(2) + (y_gap / max_gap_y).powi(2) > 1.0 {
        return None;
    }

    let intersection = (bbox[2].min(predicted[2]) - bbox[0].max(predicted[0])).max(0.0)
        * (bbox[3].min(predicted[3]) - bbox[1].max(predicted[1])).max(0.0);
    let area = (bbox[2] - bbox[0]).max(0.0) * (bbox[3] - bbox[1]).max(0.0);
    let predicted_area =
        (predicted[2] - predicted[0]).max(0.0) * (predicted[3] - predicted[1]).max(0.0);
    let union = area + predicted_area - intersection;
    Some(if union > 0.0 {
        intersection / union
    } else {
        0.0
    })
}

fn best_locked_blob(
    mask: &[u8],
    width: u32,
    height: u32,
    source_crop: CropRect,
    degrees: u16,
    region: [u32; 4],
    anchor: [f32; 2],
    previous: [f32; 4],
    predicted_shift: [f32; 2],
    scratch: &mut PixelSeekScratch,
) -> Result<Option<PixelBlob>, ()> {
    if !(1..=360).contains(&degrees) || !anchor.iter().all(|value| value.is_finite()) {
        return Ok(None);
    }
    let [left, region_top, right, region_bottom] = [
        region[0].min(width),
        region[1].min(height),
        region[2].min(width),
        region[3].min(height),
    ];
    let radius = fov_radius_px(width, height, degrees);
    let radius_squared = radius * radius;
    let (center_x, center_y) = (width as f32 / 2.0, height as f32 / 2.0);
    let (fov_top, fov_bottom) = fov_row_bounds(height, radius);
    let top = region_top.max(fov_top);
    let bottom = region_bottom.min(fov_bottom);
    let predicted_center = [
        (previous[0] + previous[2]) * 0.5 + predicted_shift[0],
        (previous[1] + previous[3]) * 0.5 + predicted_shift[1],
    ];
    let lock_gap = MAX_LOCK_GAP_PX.max(
        (previous[2] - previous[0])
            .max(previous[3] - previous[1])
            .max(0.0)
            * LOCK_MARGIN_RATIO,
    );
    let prediction_error =
        predicted_shift.map(|value| (value.abs() * 0.25).min(MAX_LOCK_PREDICTION_ERROR_PX));
    let max_gap_x = lock_gap + prediction_error[0];
    let max_gap_y = lock_gap + prediction_error[1];
    let previous_area =
        ((previous[2] - previous[0]).max(0.0) * (previous[3] - previous[1]).max(0.0)) as u64;
    let mut best: Option<((f32, f32), PixelBlob)> = None;

    for y in top..bottom {
        let dy = y as f32 + 0.5 - center_y;
        let dy_squared = dy * dy;
        if dy_squared > radius_squared {
            continue;
        }
        let half_width = (radius_squared - dy_squared).max(0.0).sqrt();
        let row_left = ((center_x - half_width - 0.5).floor() as i64)
            .max(left as i64)
            .max(0) as u32;
        let row_right = ((center_x + half_width - 0.5).ceil() as i64 + 1)
            .min(right as i64)
            .min(width as i64)
            .max(0) as u32;
        for x in row_left..row_right {
            let index = (y * width + x) as usize;
            if mask[index] == 0 || scratch.is_visited(index) {
                continue;
            }
            let dx = x as f32 + 0.5 - center_x;
            if dx * dx + dy_squared > radius_squared {
                continue;
            }
            let blob = extract_blob(
                mask,
                width,
                height,
                source_crop,
                FrameCoordinate {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                },
                anchor[0],
                radius,
                scratch,
            )?;
            let Some(overlap) = lock_overlap_score(blob.bbox_xyxy, previous, predicted_shift)
            else {
                continue;
            };
            // A one-pixel speck inside a formerly large blob is not enough
            // evidence to inherit its lock; small targets remain valid when
            // their prior footprint was small too.
            if u64::from(blob.count) * 256 < previous_area {
                continue;
            }
            let target = [
                blob.target.x - source_crop.x as f32,
                blob.target.y - source_crop.y as f32,
            ];
            // Balance blob continuity against center motion: a tiny overlap
            // must not beat a much closer candidate solely due to box size.
            let center = [
                (blob.bbox_xyxy[0] + blob.bbox_xyxy[2]) * 0.5,
                (blob.bbox_xyxy[1] + blob.bbox_xyxy[3]) * 0.5,
            ];
            let pixel_distance = (target[0] - anchor[0]).powi(2) + (target[1] - anchor[1]).powi(2);
            let normalized_center_distance = (center[0] - predicted_center[0]).powi(2)
                / max_gap_x.powi(2)
                + (center[1] - predicted_center[1]).powi(2) / max_gap_y.powi(2);
            if normalized_center_distance > MAX_LOCK_ASSOCIATION_DISTANCE_SQUARED {
                continue;
            }
            let score = (
                normalized_center_distance + 6.0 * (1.0 - overlap),
                pixel_distance,
            );
            let is_better = best.as_ref().is_none_or(|(best_score, best_blob)| {
                score < *best_score
                    || (score == *best_score
                        && (blob.target.y < best_blob.target.y
                            || (blob.target.y == best_blob.target.y
                                && blob.target.x < best_blob.target.x)))
            });
            if is_better {
                best = Some((score, blob));
            }
        }
    }
    Ok(best.map(|(_, blob)| blob))
}

pub fn process_pixel_seek(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
) -> PixelSeekOutput {
    process_pixel_seek_with_prediction(image, source_crop, color, fov_degrees, locked, [0.0; 2])
}

pub(crate) fn process_pixel_seek_with_prediction(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    predicted_shift: [f32; 2],
) -> PixelSeekOutput {
    process_pixel_seek_with_mask(
        image,
        source_crop,
        color,
        fov_degrees,
        locked,
        predicted_shift,
        color_mask(image, color),
    )
}

pub(crate) fn process_pixel_seek_with_mask(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    predicted_shift: [f32; 2],
    mask: Vec<u8>,
) -> PixelSeekOutput {
    process_pixel_seek_with_mask_anchor(
        image,
        source_crop,
        color,
        fov_degrees,
        locked,
        predicted_shift,
        None,
        mask,
    )
}

pub(crate) fn process_pixel_seek_with_mask_anchor(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    predicted_shift: [f32; 2],
    prior_pixel_target: Option<[f32; 2]>,
    mask: Vec<u8>,
) -> PixelSeekOutput {
    process_pixel_seek_with_scratch(
        image,
        source_crop,
        color,
        fov_degrees,
        locked,
        predicted_shift,
        prior_pixel_target,
        mask,
        &mut PixelSeekScratch::default(),
    )
}

pub(crate) fn process_pixel_seek_with_scratch(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    predicted_shift: [f32; 2],
    prior_pixel_target: Option<[f32; 2]>,
    mask: Vec<u8>,
    scratch: &mut PixelSeekScratch,
) -> PixelSeekOutput {
    process_pixel_seek_with_scratch_and_pixel_prediction(
        image,
        source_crop,
        color,
        fov_degrees,
        locked,
        predicted_shift,
        predicted_shift,
        prior_pixel_target,
        mask,
        scratch,
    )
}

pub(crate) fn process_pixel_seek_with_scratch_and_pixel_prediction(
    image: &RgbImage,
    source_crop: CropRect,
    color: PixelTargetColor,
    fov_degrees: u16,
    locked: Option<[f32; 4]>,
    predicted_shift: [f32; 2],
    predicted_pixel_shift: [f32; 2],
    prior_pixel_target: Option<[f32; 2]>,
    mask: Vec<u8>,
    scratch: &mut PixelSeekScratch,
) -> PixelSeekOutput {
    let width = image.width();
    let height = image.height();
    scratch.release_oversized(width as usize * height as usize);
    if width == 0
        || height == 0
        || mask.len() != width as usize * height as usize
        || mask.len() > u32::MAX as usize
    {
        return PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
            lock_miss: locked.is_some(),
        };
    }
    // Exactly one target per frame. While a fresh lock exists, evaluate all
    // components in the bounded expanded lock ROI once; a plausible distractor
    // in the old box must not hide a closer match to the moving target.
    let predicted_shift = predicted_shift.map(|value| if value.is_finite() { value } else { 0.0 });
    let predicted_pixel_shift =
        predicted_pixel_shift.map(|value| if value.is_finite() { value } else { 0.0 });
    let anchor = locked.map(|previous| {
        let point = prior_pixel_target
            .filter(|point| point.iter().all(|value| value.is_finite()))
            .unwrap_or([
                (previous[0] + previous[2]) * 0.5,
                (previous[1] + previous[3]) * 0.5,
            ]);
        [
            point[0] + predicted_pixel_shift[0] - source_crop.x as f32,
            point[1] + predicted_pixel_shift[1] - source_crop.y as f32,
        ]
    });
    let visited_words = mask.len().div_ceil(u64::BITS as usize);
    if scratch.visited_words.len() < visited_words {
        let more = visited_words - scratch.visited_words.len();
        if scratch.visited_words.try_reserve_exact(more).is_err()
            || scratch.visited_epochs.try_reserve_exact(more).is_err()
        {
            return PixelSeekOutput {
                objects: Vec::new(),
                target: None,
                mask,
                lock_miss: locked.is_some(),
            };
        }
        scratch.visited_words.resize(visited_words, 0);
        scratch.visited_epochs.resize(visited_words, 0);
    }
    scratch.advance_generation();
    if scratch.stack.capacity() < 64 && scratch.stack.try_reserve(64).is_err() {
        return PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
            lock_miss: locked.is_some(),
        };
    }
    let lock_search =
        locked.map(|previous| lock_region(previous, predicted_shift, source_crop, width, height));
    if lock_search.is_some_and(|search| search.is_none()) {
        return PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
            lock_miss: true,
        };
    }
    let (class_id, class_name) = color.class();
    let blob = if let Some(previous) = locked {
        let Some(region) = lock_search.flatten() else {
            return PixelSeekOutput {
                objects: Vec::new(),
                target: None,
                mask,
                lock_miss: true,
            };
        };
        let anchor = anchor.unwrap_or([width as f32 / 2.0, height as f32 / 2.0]);
        match best_locked_blob(
            &mask,
            width,
            height,
            source_crop,
            fov_degrees,
            region,
            anchor,
            previous,
            predicted_shift,
            scratch,
        ) {
            Ok(blob) => blob,
            Err(()) => None,
        }
    } else {
        highest_pixel_in_fov(&mask, width, height, fov_degrees).and_then(|seed| {
            extract_blob(
                &mask,
                width,
                height,
                source_crop,
                seed,
                width as f32 / 2.0,
                fov_radius_px(width, height, fov_degrees),
                scratch,
            )
            .ok()
        })
    };
    let Some(blob) = blob else {
        return PixelSeekOutput {
            objects: Vec::new(),
            target: None,
            mask,
            lock_miss: locked.is_some(),
        };
    };
    PixelSeekOutput {
        objects: vec![DetectionObject {
            class_id,
            class_name: class_name.into(),
            bbox_xyxy: blob.bbox_xyxy,
            confidence: None,
            pixel_count: Some(blob.count),
        }],
        target: Some(blob.target),
        mask,
        lock_miss: false,
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
                [0.0; 2],
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
    fn locked_scan_clips_each_row_to_a_small_fov() {
        let mut image = RgbImage::from_pixel(200, 200, Rgb([0, 0, 0]));
        paint(&mut image, 99, 99, 2);
        paint(&mut image, 25, 25, 3);
        let output = process_pixel_seek(
            &image,
            crop(200),
            PixelTargetColor::Red,
            10,
            Some([99.0, 99.0, 101.0, 101.0]),
        );
        assert_eq!(output.objects.len(), 1);
        assert_eq!(output.objects[0].bbox_xyxy, [99.0, 99.0, 101.0, 101.0]);
    }

    #[test]
    fn locked_target_wins_same_row_tie_over_blob_nearer_to_screen_center() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 62, 40, 4); // locked target
        paint(&mut image, 58, 40, 3); // competitor inside the tight lock margin
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([62.0, 40.0, 66.0, 44.0]),
        );
        assert_eq!(output.objects[0].bbox_xyxy, [62.0, 40.0, 66.0, 44.0]);
    }

    #[test]
    fn a_candidate_in_the_padding_cannot_steal_a_target_still_in_its_predicted_box() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 40, 1); // locked target remains in the predicted box
        paint(&mut image, 70, 40, 1); // competitor is closer to the previous pixel
        let output = process_pixel_seek_with_mask_anchor(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 40.0, 70.0, 44.0]),
            [0.0; 2],
            Some([69.5, 40.5]),
            color_mask(&image, PixelTargetColor::Red),
        );
        assert_eq!(output.objects[0].bbox_xyxy, [60.0, 40.0, 61.0, 41.0]);
    }

    #[test]
    fn moving_target_in_lock_padding_beats_a_valid_blob_in_the_old_core() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 31, 49, 2); // valid distractor inside the old lock box
        paint(&mut image, 49, 55, 2); // selected target moved just below that box
        let output = process_pixel_seek_with_mask_anchor(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([30.0, 45.0, 70.0, 55.0]),
            [0.0; 2],
            Some([32.5, 49.5]),
            color_mask(&image, PixelTargetColor::Red),
        );

        assert_eq!(output.objects[0].bbox_xyxy, [49.0, 55.0, 51.0, 57.0]);
        assert_eq!(output.target, Some(FrameCoordinate { x: 49.5, y: 55.5 }));
    }

    #[test]
    fn stable_blob_center_beats_a_distractor_near_the_old_top_pixel() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 64, 64, 4); // locked target remains near its predicted center
        paint(&mut image, 59, 50, 2); // closer to the old top-pixel anchor
        let output = process_pixel_seek_with_mask_anchor(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 40.0, 100.0, 100.0]),
            [0.0; 2],
            Some([60.5, 50.5]),
            color_mask(&image, PixelTargetColor::Red),
        );

        assert_eq!(output.objects[0].bbox_xyxy, [64.0, 64.0, 68.0, 68.0]);
    }

    #[test]
    fn overlapping_moving_target_beats_a_nearer_small_distractor() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 47, 40, 8); // same target, moved while retaining overlap
        paint(&mut image, 49, 49, 2); // nearer to the previous center, but separate noise
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 40.0, 60.0, 60.0]),
        );

        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [47.0, 40.0, 55.0, 48.0]);
    }

    #[test]
    fn rejected_nearby_blob_does_not_hide_the_moving_locked_target() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 47, 40, 20); // locked blob moved right by 7 px
        paint(&mut image, 42, 40, 2); // nearer to the old pixel, but not associated
        let output = process_pixel_seek_with_mask_anchor(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 40.0, 60.0, 60.0]),
            [0.0; 2],
            Some([40.5, 40.5]),
            color_mask(&image, PixelTargetColor::Red),
        );

        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [47.0, 40.0, 67.0, 60.0]);
        assert_eq!(output.target, Some(FrameCoordinate { x: 47.5, y: 40.5 }));
    }

    #[test]
    fn a_higher_separate_blob_cannot_steal_a_fresh_target_lock() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 62, 40, 4); // locked target
        paint(&mut image, 62, 32, 4); // separate, higher target inside the lock window
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([62.0, 40.0, 66.0, 44.0]),
        );
        assert_eq!(output.objects[0].bbox_xyxy, [62.0, 40.0, 66.0, 44.0]);
    }

    #[test]
    fn a_nearby_blob_cannot_inherit_a_lock_after_the_target_disappears() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 42, 4); // separate blob inside the expanded search region
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert!(output.objects.is_empty());
        assert!(output.target.is_none());
        assert!(output.lock_miss);
    }

    #[test]
    fn diagonal_nearby_blob_cannot_inherit_a_lock_from_two_axis_gaps() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 66, 56, 4); // 6 px away on both axes from the old box
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert!(output.objects.is_empty());
        assert!(output.target.is_none());
        assert!(output.lock_miss);
    }

    #[test]
    fn noisy_lock_region_does_not_switch_to_detached_blobs() {
        let mut image = RgbImage::from_pixel(200, 200, Rgb([0, 0, 0]));
        for x in (80..=112).step_by(2) {
            paint(&mut image, x, 80, 1);
        }
        let output = process_pixel_seek_with_mask_anchor(
            &image,
            crop(200),
            PixelTargetColor::Red,
            360,
            Some([80.0, 80.0, 120.0, 120.0]),
            [0.0; 2],
            Some([80.5, 80.5]),
            color_mask(&image, PixelTargetColor::Red),
        );

        assert!(output.objects.is_empty());
        assert!(output.target.is_none());
        assert!(output.lock_miss);
    }

    #[test]
    fn velocity_prediction_keeps_a_moving_target_ahead_of_a_competitor() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 50, 4); // same target, moved 20 px right
        paint(&mut image, 10, 10, 4); // unrelated target remains higher
        let output = process_pixel_seek_with_prediction(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 50.0, 44.0, 54.0]),
            [20.0, 0.0],
        );
        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [60.0, 50.0, 64.0, 54.0]);
        let target = output.target.unwrap();
        assert!((60.5..=63.5).contains(&target.x));
        assert_eq!(target.y, 50.5);
    }

    #[test]
    fn pixel_anchor_prediction_is_independent_of_blob_center_shape_change() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 44, 40, 4);
        for y in 42..46 {
            for x in 48..54 {
                image.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
        let output = process_pixel_seek_with_scratch_and_pixel_prediction(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 40.0, 44.0, 44.0]),
            [7.0, 1.0], // blob-center motion
            [4.0, 0.0], // selected top-pixel motion
            Some([41.5, 40.5]),
            color_mask(&image, PixelTargetColor::Red),
            &mut PixelSeekScratch::default(),
        );

        assert!(!output.lock_miss);
        assert_eq!(output.target, Some(FrameCoordinate { x: 45.5, y: 40.5 }));
    }

    #[test]
    fn prediction_over_64_pixels_keeps_lock_on_the_moving_target() {
        let mut image = RgbImage::from_pixel(160, 120, Rgb([0, 0, 0]));
        paint(&mut image, 110, 50, 4); // locked target, moved 100 px
        paint(&mut image, 85, 50, 4); // competitor near the old clamped prediction
        let output = process_pixel_seek_with_prediction(
            &image,
            crop(160),
            PixelTargetColor::Red,
            360,
            Some([10.0, 50.0, 14.0, 54.0]),
            [100.0, 0.0],
        );
        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [110.0, 50.0, 114.0, 54.0]);
    }

    #[test]
    fn an_offscreen_prediction_does_not_reassign_the_lock_to_an_edge_blob() {
        let mut image = RgbImage::from_pixel(160, 120, Rgb([0, 0, 0]));
        paint(&mut image, 145, 50, 4); // unrelated blob at the edge
        let output = process_pixel_seek_with_prediction(
            &image,
            crop(160),
            PixelTargetColor::Red,
            360,
            Some([10.0, 50.0, 14.0, 54.0]),
            [170.0, 0.0],
        );
        assert!(output.objects.is_empty());
        assert!(output.lock_miss);
    }

    #[test]
    fn velocity_padding_recovers_a_target_that_lags_its_prediction() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 50, 4); // 4 px behind the predicted bbox
        paint(&mut image, 10, 10, 4); // unrelated target outside the lock region
        let output = process_pixel_seek_with_prediction(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([40.0, 50.0, 44.0, 54.0]),
            [12.0, 0.0],
        );
        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [60.0, 50.0, 64.0, 54.0]);
    }

    #[test]
    fn small_target_motion_can_bootstrap_velocity_without_losing_its_lock() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 53, 50, 1); // target moved 3 px before velocity was known
        paint(&mut image, 20, 20, 2); // unrelated higher blob
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([50.0, 50.0, 51.0, 51.0]),
        );
        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [53.0, 50.0, 54.0, 51.0]);
        assert_eq!(output.target.unwrap(), FrameCoordinate { x: 53.5, y: 50.5 });
    }

    #[test]
    fn a_single_pixel_target_can_move_beyond_three_pixels_before_velocity_is_known() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        image.put_pixel(56, 50, Rgb([255, 0, 0]));
        paint(&mut image, 20, 20, 2); // unrelated blob remains outside the lock
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([50.0, 50.0, 51.0, 51.0]),
        );
        assert!(!output.lock_miss);
        assert_eq!(output.objects[0].bbox_xyxy, [56.0, 50.0, 57.0, 51.0]);
        assert_eq!(output.target, Some(FrameCoordinate { x: 56.5, y: 50.5 }));
    }

    #[test]
    fn a_locked_target_still_climbs_to_higher_pixels_next_to_it() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 60, 50, 4); // previous target box
        paint(&mut image, 61, 46, 4); // higher pixels connected to the same target
        paint(&mut image, 30, 10, 4); // unrelated higher target
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert_eq!(output.target.unwrap().y, 46.5);
        assert_eq!(output.objects[0].bbox_xyxy, [60.0, 46.0, 65.0, 54.0]);
    }

    #[test]
    fn a_moving_locked_blob_stays_selected_while_a_higher_blob_remains_in_the_fov() {
        let mut scratch = PixelSeekScratch::default();
        let mut locked = None;
        let mut prior_pixel = None;
        let mut velocity = [0.0; 2];
        for frame in 0..5 {
            let target_x = 60 + frame * 4;
            let mut image = RgbImage::from_pixel(160, 120, Rgb([0, 0, 0]));
            paint(&mut image, target_x, 50, 4);
            if frame > 0 {
                paint(&mut image, 50, 25, 2);
            }
            let output = process_pixel_seek_with_scratch(
                &image,
                crop(160),
                PixelTargetColor::Red,
                360,
                locked,
                velocity,
                prior_pixel,
                color_mask(&image, PixelTargetColor::Red),
                &mut scratch,
            );
            assert_eq!(
                output.objects[0].bbox_xyxy,
                [target_x as f32, 50.0, target_x as f32 + 4.0, 54.0]
            );
            let point = output.target.unwrap();
            assert!(point.x >= target_x as f32 + 0.5 && point.x <= target_x as f32 + 3.5);
            assert_eq!(point.y, 50.5);
            velocity = prior_pixel.map_or([0.0; 2], |previous| {
                [point.x - previous[0], point.y - previous[1]]
            });
            prior_pixel = Some([point.x, point.y]);
            locked = output.objects.first().map(|object| object.bbox_xyxy);
        }
    }

    #[test]
    fn a_missing_target_does_not_switch_away_from_a_fresh_lock() {
        let mut image = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
        paint(&mut image, 30, 20, 4);
        let output = process_pixel_seek(
            &image,
            crop(100),
            PixelTargetColor::Red,
            360,
            Some([60.0, 50.0, 64.0, 54.0]),
        );
        assert!(output.objects.is_empty());
        assert!(output.target.is_none());
        assert!(output.lock_miss);
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
