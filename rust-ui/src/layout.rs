#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

fn dimensions_valid(values: &[f64]) -> bool {
    values.iter().all(|value| value.is_finite() && *value > 0.0)
}

pub fn fit_preview_image(
    source_width: f64,
    source_height: f64,
    viewport_width: f64,
    viewport_height: f64,
    pixel_perfect: bool,
) -> Option<Rect> {
    if !dimensions_valid(&[source_width, source_height, viewport_width, viewport_height]) {
        return None;
    }
    let fit = (viewport_width / source_width).min(viewport_height / source_height);
    let scale = if pixel_perfect && fit >= 1.0 {
        fit.floor().max(1.0)
    } else {
        fit
    };
    let width = source_width * scale;
    let height = source_height * scale;
    Some(Rect {
        x: (viewport_width - width) / 2.0,
        y: (viewport_height - height) / 2.0,
        width,
        height,
    })
}

pub fn map_detection_point(
    point: [f64; 2],
    processed_width: f64,
    processed_height: f64,
    image_rect: Rect,
    crop_x: f64,
    crop_y: f64,
) -> Option<[f64; 2]> {
    if !dimensions_valid(&[
        processed_width,
        processed_height,
        image_rect.width,
        image_rect.height,
    ]) || !point
        .iter()
        .chain([crop_x, crop_y, image_rect.x, image_rect.y].iter())
        .all(|v| v.is_finite())
    {
        return None;
    }
    Some([
        image_rect.x + (point[0] - crop_x) / processed_width * image_rect.width,
        image_rect.y + (point[1] - crop_y) / processed_height * image_rect.height,
    ])
}

pub fn map_detection_box(
    box_xyxy: [f64; 4],
    processed_width: f64,
    processed_height: f64,
    image_rect: Rect,
    crop_x: f64,
    crop_y: f64,
) -> Option<Rect> {
    if !dimensions_valid(&[
        processed_width,
        processed_height,
        image_rect.width,
        image_rect.height,
    ]) || !box_xyxy
        .iter()
        .chain([crop_x, crop_y, image_rect.x, image_rect.y].iter())
        .all(|v| v.is_finite())
    {
        return None;
    }
    let [left, top, right, bottom] = box_xyxy;
    Some(Rect {
        x: image_rect.x + (left - crop_x) / processed_width * image_rect.width,
        y: image_rect.y + (top - crop_y) / processed_height * image_rect.height,
        width: (right - left) / processed_width * image_rect.width,
        height: (bottom - top) / processed_height * image_rect.height,
    })
}

pub fn canvas_backing_size(width: f64, height: f64, pixel_ratio: f64) -> Option<(u32, u32)> {
    if !dimensions_valid(&[width, height, pixel_ratio]) {
        return None;
    }
    Some((
        (width * pixel_ratio).round().max(1.0) as u32,
        (height * pixel_ratio).round().max(1.0) as u32,
    ))
}

/// FOV degrees map to source pixels against this fixed side (the backend's
/// `FOV_REFERENCE_PX`), so a setting is the same circle on screen at every
/// Crop Resolution (360° = 640 px, limited to the image).
pub const FOV_REFERENCE_PX: f64 = 640.0;

/// FOV circle diameter in display pixels for a `source` image drawn into
/// `image_rect`.
pub fn fov_diameter(image_rect: Rect, source: (u32, u32), degrees: u16) -> Option<f64> {
    if !dimensions_valid(&[image_rect.width, image_rect.height])
        || source.0 == 0
        || source.1 == 0
        || !(1..=360).contains(&degrees)
    {
        return None;
    }
    let limit = (f64::from(source.0.min(source.1)) - 1.0).max(0.0);
    let source_px = ((FOV_REFERENCE_PX - 1.0) * f64::from(degrees) / 360.0).min(limit);
    Some(source_px * image_rect.width / f64::from(source.0))
}

pub fn preview_detections<T>(
    detections: &[T],
    strategy: &str,
    minimum_confidence: f32,
    confidence: impl Fn(&T) -> Option<f32>,
) -> Vec<T>
where
    T: Clone,
{
    if strategy == "pixel_seek" {
        return detections.to_vec();
    }
    let threshold = if minimum_confidence.is_finite() {
        minimum_confidence
    } else {
        0.0
    };
    detections
        .iter()
        .filter(|detection| confidence(detection).is_some_and(|score| score >= threshold))
        .cloned()
        .collect()
}

/// `#rrggbb` → `rgba(r, g, b, alpha)` for canvas strokes; `None` for any
/// other color syntax so callers can fall back to the original string.
pub fn color_with_alpha(hex: &str, alpha: f64) -> Option<String> {
    let digits = hex.strip_prefix('#').filter(|digits| digits.len() == 6)?;
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(format!(
        "rgba({}, {}, {}, {})",
        value >> 16,
        (value >> 8) & 0xff,
        value & 0xff,
        alpha.clamp(0.0, 1.0)
    ))
}

pub fn detection_summary(detections: &[crate::protocol::DetectionObject]) -> String {
    detections
        .iter()
        .filter_map(|detection| {
            detection.confidence.map(|score| {
                format!(
                    "[{}] {}%",
                    detection.class_id,
                    (score * 100.0).round() as i32
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// HUD FPS tone: how much of the input rate detection keeps up with.
/// "live" ≥ 80 %, "wait" ≥ 50 %, "error" below; `None` without measurements.
pub fn fps_tone(processing_fps: Option<f64>, source_fps: Option<f64>) -> Option<&'static str> {
    let (processing, source) = (processing_fps?, source_fps?);
    if !(source > 0.0) || !processing.is_finite() {
        return None;
    }
    let ratio = processing / source;
    Some(if ratio >= 0.8 {
        "live"
    } else if ratio >= 0.5 {
        "wait"
    } else {
        "error"
    })
}

/// Latency above this is flagged "(HIGH)" in the HUD.
pub const LATENCY_HIGH_MS: f64 = 10.0;

/// HUD latency tone on a fixed 0–10 ms scale, the same at every input rate:
/// "live" < 4 ms, "wait" < 7 ms, "error" from 7 ms (above 10 ms also HIGH).
pub fn latency_tone(latency_ms: f64) -> Option<&'static str> {
    if !latency_ms.is_finite() || latency_ms < 0.0 {
        return None;
    }
    Some(if latency_ms < 4.0 {
        "live"
    } else if latency_ms < 7.0 {
        "wait"
    } else {
        "error"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DetectionObject;

    #[test]
    fn hud_tones_follow_the_input_rate() {
        assert_eq!(fps_tone(Some(239.0), Some(240.0)), Some("live"));
        assert_eq!(fps_tone(Some(192.0), Some(240.0)), Some("live")); // exactly 80 %
        assert_eq!(fps_tone(Some(191.0), Some(240.0)), Some("wait"));
        assert_eq!(fps_tone(Some(120.0), Some(240.0)), Some("wait")); // exactly 50 %
        assert_eq!(fps_tone(Some(119.0), Some(240.0)), Some("error"));
        // A 60 FPS sender fully kept up with is healthy, not slow.
        assert_eq!(fps_tone(Some(60.0), Some(60.0)), Some("live"));
        assert_eq!(fps_tone(None, Some(240.0)), None);
        assert_eq!(fps_tone(Some(10.0), Some(0.0)), None);

        assert_eq!(latency_tone(0.1), Some("live"));
        assert_eq!(latency_tone(3.99), Some("live"));
        assert_eq!(latency_tone(4.0), Some("wait"));
        assert_eq!(latency_tone(6.99), Some("wait"));
        assert_eq!(latency_tone(7.0), Some("error"));
        assert_eq!(latency_tone(25.0), Some("error"));
        assert!(25.0 > LATENCY_HIGH_MS && 10.0 <= LATENCY_HIGH_MS);
        assert_eq!(latency_tone(f64::NAN), None);
    }

    #[test]
    fn preview_fit_preserves_aspect_and_pixel_perfect_scaling() {
        let smooth = fit_preview_image(192.0, 192.0, 640.0, 480.0, false).unwrap();
        assert_eq!(
            smooth,
            Rect {
                x: 80.0,
                y: 0.0,
                width: 480.0,
                height: 480.0
            }
        );
        let sharp = fit_preview_image(192.0, 192.0, 640.0, 480.0, true).unwrap();
        assert_eq!(sharp.width, 384.0);
        assert_eq!(sharp.height, 384.0);
    }

    #[test]
    fn crop_origin_is_removed_before_preview_mapping() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 200.0,
        };
        assert_eq!(
            map_detection_point([120.0, 70.0], 100.0, 50.0, rect, 100.0, 50.0),
            Some([90.0, 100.0])
        );
        assert_eq!(
            map_detection_box([110.0, 60.0, 130.0, 80.0], 100.0, 50.0, rect, 100.0, 50.0),
            Some(Rect {
                x: 50.0,
                y: 60.0,
                width: 80.0,
                height: 80.0
            })
        );
    }

    #[test]
    fn fov_is_proportional_and_canvas_scales_for_hidpi() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 480.0,
            height: 320.0,
        };
        // Same degrees, same source pixels at every crop; drawn at display scale.
        assert_eq!(fov_diameter(rect, (480, 320), 360), Some(319.0));
        assert_eq!(fov_diameter(rect, (640, 640), 36), Some(63.9 * 480.0 / 640.0));
        let small = fov_diameter(rect, (256, 256), 50).unwrap() * 256.0 / 480.0;
        let large = fov_diameter(rect, (640, 640), 50).unwrap() * 640.0 / 480.0;
        assert!((small - large).abs() < 1e-9, "{small} vs {large}");
        assert_eq!(canvas_backing_size(480.0, 320.0, 2.0), Some((960, 640)));
    }

    #[test]
    fn hex_colors_gain_alpha_and_other_syntax_is_rejected() {
        assert_eq!(
            color_with_alpha("#e26868", 0.1).as_deref(),
            Some("rgba(226, 104, 104, 0.1)")
        );
        assert_eq!(
            color_with_alpha("#E3BF4D", 2.0).as_deref(),
            Some("rgba(227, 191, 77, 1)")
        );
        assert_eq!(color_with_alpha("hsl(10 82% 62%)", 0.5), None);
        assert_eq!(color_with_alpha("#fff", 0.5), None);
        assert_eq!(color_with_alpha("#zzzzzz", 0.5), None);
    }

    #[test]
    fn object_preview_filters_missing_confidence_but_pixel_seek_bypasses() {
        let detections = vec![
            DetectionObject {
                class_id: 1,
                class_name: "one".into(),
                bbox_xyxy: [0.0; 4],
                confidence: Some(0.5),
                pixel_count: None,
            },
            DetectionObject {
                class_id: 2,
                class_name: "two".into(),
                bbox_xyxy: [0.0; 4],
                confidence: None,
                pixel_count: None,
            },
        ];
        assert_eq!(
            preview_detections(&detections, "object_detection", 0.5, |item| item.confidence).len(),
            1
        );
        assert_eq!(
            preview_detections(&detections, "pixel_seek", 0.99, |item| item.confidence).len(),
            2
        );
    }
}
