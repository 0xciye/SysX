use crate::layout::{
    canvas_backing_size, detection_summary, fit_preview_image, fov_diameter, map_detection_box,
    map_detection_point, Rect,
};
use crate::protocol::{DetectionObject, LiveFrame};
use leptos::prelude::*;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement};

#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub frame: LiveFrame,
    pub image_url: String,
    pub preview_fps: Option<f64>,
}

#[component]
pub fn PreviewCanvas(
    frame: RwSignal<Option<PreviewFrame>>,
    scale: RwSignal<String>,
    detections: RwSignal<Vec<DetectionObject>>,
    target_point: RwSignal<Option<[f32; 2]>>,
    detection_color: RwSignal<String>,
    fov_degrees: RwSignal<u16>,
) -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let draw = Rc::new(move || {
        let Some(canvas) = canvas_ref.get() else {
            return;
        };
        let frame_signal = frame;
        let frame = frame_signal.get_untracked();
        let scale = scale.get_untracked() == "Pixel perfect";
        let detections = detections.get_untracked();
        let target = target_point.get_untracked();
        let color = detection_color.get_untracked();
        let fov = fov_degrees.get_untracked();
        let Some(current) = frame else {
            draw_empty(&canvas);
            return;
        };
        let frame_id = current.frame.frame_id;
        let latest = frame_signal;
        let image_url = current.image_url.clone();
        spawn_local(async move {
            let Ok(image) = HtmlImageElement::new() else {
                return;
            };
            image.set_src(&image_url);
            if JsFuture::from(image.decode()).await.is_err() {
                return;
            }
            if latest
                .get_untracked()
                .as_ref()
                .is_none_or(|value| value.frame.frame_id != frame_id)
            {
                return;
            }
            draw_image(
                &canvas,
                &image,
                &current.frame,
                scale,
                &detections,
                target,
                &color,
                fov,
            );
        });
    });

    Effect::new({
        let draw = Rc::clone(&draw);
        move |_| {
            let _ = frame.get();
            let _ = scale.get();
            let _ = detections.get();
            let _ = target_point.get();
            let _ = detection_color.get();
            let _ = fov_degrees.get();
            draw();
        }
    });

    view! {
        <canvas
            node_ref=canvas_ref
            class="preview-bitmap"
            aria-label="Live preview"
        />
    }
}

fn context(canvas: &HtmlCanvasElement) -> Option<(CanvasRenderingContext2d, f64, f64, f64)> {
    let bounds = canvas.get_bounding_client_rect();
    let width = bounds.width();
    let height = bounds.height();
    let ratio = web_sys::window()
        .map(|window| window.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0);
    let (backing_width, backing_height) = canvas_backing_size(width, height, ratio)?;
    canvas.set_width(backing_width);
    canvas.set_height(backing_height);
    let context = canvas
        .get_context("2d")
        .ok()??
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;
    context
        .set_transform(ratio, 0.0, 0.0, ratio, 0.0, 0.0)
        .ok()?;
    Some((context, width, height, ratio))
}

fn draw_empty(canvas: &HtmlCanvasElement) {
    let Some((context, width, height, _)) = context(canvas) else {
        return;
    };
    context.set_fill_style_str("#05080b");
    context.fill_rect(0.0, 0.0, width, height);
}

fn draw_image(
    canvas: &HtmlCanvasElement,
    image: &HtmlImageElement,
    frame: &LiveFrame,
    pixel_perfect: bool,
    detections: &[DetectionObject],
    target: Option<[f32; 2]>,
    detection_color: &str,
    fov_degrees: u16,
) {
    let Some((context, width, height, _)) = context(canvas) else {
        return;
    };
    context.set_fill_style_str("#05080b");
    context.fill_rect(0.0, 0.0, width, height);
    context.set_image_smoothing_enabled(!pixel_perfect);
    let Some(rect) = fit_preview_image(
        image.natural_width() as f64,
        image.natural_height() as f64,
        width,
        height,
        pixel_perfect,
    ) else {
        return;
    };
    let _ = context.draw_image_with_html_image_element_and_dw_and_dh(
        image,
        rect.x,
        rect.y,
        rect.width,
        rect.height,
    );
    let is_object_detection = frame.mode == "object_detection";
    if frame.detection.processed_width > 0 && frame.detection.processed_height > 0 {
        for detection in detections {
            if let Some(box_rect) = map_detection_box(
                detection.bbox_xyxy.map(f64::from),
                frame.detection.processed_width as f64,
                frame.detection.processed_height as f64,
                rect,
                frame.crop_x as f64,
                frame.crop_y as f64,
            ) {
                let color = if is_object_detection {
                    class_color(detection.class_id)
                } else {
                    detection_color.to_owned()
                };
                draw_box(&context, box_rect, &color);
            }
        }
        if is_object_detection {
            draw_summary(
                &context,
                &detection_summary(detections),
                rect,
                detection_color,
            );
        }
        if let Some(diameter) = fov_diameter(rect, fov_degrees) {
            context.begin_path();
            let _ = context.arc(
                rect.x + rect.width / 2.0,
                rect.y + rect.height / 2.0,
                diameter / 2.0,
                0.0,
                std::f64::consts::TAU,
            );
            context.set_stroke_style_str("rgba(67,207,160,.8)");
            context.set_line_width(1.0);
            context.stroke();
        }
        if let Some(point) = target {
            if let Some([target_x, target_y]) = map_detection_point(
                [point[0] as f64, point[1] as f64],
                frame.detection.processed_width as f64,
                frame.detection.processed_height as f64,
                rect,
                frame.crop_x as f64,
                frame.crop_y as f64,
            ) {
                draw_target(&context, rect, target_x, target_y, detection_color);
            }
        }
    }
}

fn class_color(class_id: u32) -> String {
    format!(
        "hsl({} 82% 62%)",
        (class_id as f64 * 137.508 + 24.0) % 360.0
    )
}

fn draw_box(context: &CanvasRenderingContext2d, rect: Rect, color: &str) {
    let corner = 16.0_f64.min(7.0_f64.max(rect.width.min(rect.height) * 0.22));
    context.set_line_join("round");
    context.set_stroke_style_str("rgba(5, 9, 10, .88)");
    context.set_line_width(4.0);
    context.stroke_rect(rect.x, rect.y, rect.width, rect.height);
    context.set_stroke_style_str(color);
    context.set_line_width(1.5);
    context.stroke_rect(rect.x, rect.y, rect.width, rect.height);
    context.set_line_width(3.0);
    context.begin_path();
    for (x, y, sx, sy) in [
        (rect.x, rect.y + corner, rect.x, rect.y),
        (rect.x, rect.y, rect.x + corner, rect.y),
        (
            rect.x + rect.width - corner,
            rect.y,
            rect.x + rect.width,
            rect.y,
        ),
        (
            rect.x + rect.width,
            rect.y,
            rect.x + rect.width,
            rect.y + corner,
        ),
        (
            rect.x + rect.width,
            rect.y + rect.height - corner,
            rect.x + rect.width,
            rect.y + rect.height,
        ),
        (
            rect.x + rect.width,
            rect.y + rect.height,
            rect.x + rect.width - corner,
            rect.y + rect.height,
        ),
        (
            rect.x + corner,
            rect.y + rect.height,
            rect.x,
            rect.y + rect.height,
        ),
        (
            rect.x,
            rect.y + rect.height,
            rect.x,
            rect.y + rect.height - corner,
        ),
    ] {
        context.move_to(x, y);
        context.line_to(sx, sy);
    }
    context.set_stroke_style_str(color);
    context.stroke();
}

fn draw_summary(context: &CanvasRenderingContext2d, summary: &str, rect: Rect, color: &str) {
    if summary.is_empty() {
        return;
    }
    let max_width = (rect.width - 20.0).max(0.0);
    if max_width < 12.0 {
        return;
    }
    let font_size = 15.0_f64.min(10.0_f64.max(max_width / (summary.len() as f64 * 0.58).max(12.0)));
    context.set_font(&format!(
        "500 {font_size}px Cascadia Mono, Consolas, ui-monospace, monospace"
    ));
    let text_width = context
        .measure_text(summary)
        .map(|metric| metric.width())
        .unwrap_or(0.0);
    let badge_width = max_width.min(text_width + 22.0);
    let x = rect.x + (rect.width - badge_width) / 2.0;
    // Bottom-center keeps the top-left corner free for the HTML HUD plate.
    let y = rect.y + rect.height - 29.0 - 10.0;
    context.set_fill_style_str("rgba(6, 9, 12, .86)");
    context.fill_rect(x, y, badge_width, 29.0);
    context.set_stroke_style_str(color);
    context.set_line_width(1.0);
    context.stroke_rect(x, y, badge_width, 29.0);
    context.set_fill_style_str("#EBF1F5");
    context.set_text_baseline("middle");
    context.set_text_align("center");
    let _ = context.fill_text_with_max_width(
        summary,
        x + badge_width / 2.0,
        y + 14.5,
        badge_width - 12.0,
    );
    context.set_text_align("start");
}

fn draw_target(
    context: &CanvasRenderingContext2d,
    rect: Rect,
    target_x: f64,
    target_y: f64,
    color: &str,
) {
    let center_x = rect.x + rect.width / 2.0;
    let center_y = rect.y + rect.height / 2.0;
    let dx = target_x - center_x;
    let dy = target_y - center_y;
    let distance = dx.hypot(dy);
    let ring = 9.0;
    let end_x = if distance > ring {
        target_x - dx / distance * ring
    } else {
        target_x
    };
    let end_y = if distance > ring {
        target_y - dy / distance * ring
    } else {
        target_y
    };
    context.begin_path();
    context.move_to(center_x, center_y);
    context.line_to(end_x, end_y);
    context.set_stroke_style_str("rgba(5, 9, 10, .82)");
    context.set_line_width(4.0);
    context.set_line_cap("round");
    context.stroke();
    context.begin_path();
    context.move_to(center_x, center_y);
    context.line_to(end_x, end_y);
    context.set_stroke_style_str(color);
    context.set_line_width(2.0);
    context.stroke();
    for (x, y, radius, stroke, line_width) in [
        (center_x, center_y, 3.5, "rgba(5, 9, 10, .9)", 3.0),
        (center_x, center_y, 2.5, color, 1.0),
        (target_x, target_y, ring + 1.5, "rgba(5, 9, 10, .9)", 4.0),
        (target_x, target_y, ring, color, 2.0),
    ] {
        context.begin_path();
        let _ = context.arc(x, y, radius, 0.0, std::f64::consts::TAU);
        context.set_stroke_style_str(stroke);
        context.set_line_width(line_width);
        context.stroke();
    }
    context.begin_path();
    let _ = context.arc(target_x, target_y, 4.2, 0.0, std::f64::consts::TAU);
    context.set_fill_style_str(color);
    context.fill();
}
