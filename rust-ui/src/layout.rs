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

pub fn fov_diameter(image_rect: Rect, degrees: u16) -> Option<f64> {
    if !dimensions_valid(&[image_rect.width, image_rect.height]) || !(1..=360).contains(&degrees) {
        return None;
    }
    Some((image_rect.width.min(image_rect.height) - 1.0).max(0.0) * degrees as f64 / 360.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DetectionObject;

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
        assert_eq!(fov_diameter(rect, 360), Some(319.0));
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
