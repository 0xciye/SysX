import { useEffect, useRef } from "react";
import { fitPreviewImage, getCanvasBackingSize, getFovDiameter, mapDetectionBox, type PreviewScale } from "./preview-layout";
import type { DetectionObject } from "./backend-client";

export type PreviewFrame = {
  id: number;
  src: string;
  width: number;
  height: number;
};

type OverlayOptions = {
  boxes: boolean;
  classId: boolean;
  className: boolean;
  confidence: boolean;
};

export function PreviewCanvas({ frame, scale, detections = [], overlay = { boxes: true, classId: true, className: true, confidence: true }, detectionColor = "#51d7c4", processedWidth, processedHeight, fovDegrees = 36 }: {
  frame: PreviewFrame | null;
  scale: PreviewScale;
  detections?: DetectionObject[];
  overlay?: OverlayOptions;
  detectionColor?: string;
  processedWidth?: number;
  processedHeight?: number;
  fovDegrees?: number;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    if (!frame) {
      const bounds = canvas.getBoundingClientRect();
      const pixelRatio = window.devicePixelRatio || 1;
      const backingSize = getCanvasBackingSize(bounds.width, bounds.height, pixelRatio);
      canvas.width = backingSize.width;
      canvas.height = backingSize.height;
      const context = canvas.getContext("2d", { alpha: false });
      if (context) {
        context.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
        context.fillStyle = "#080a0b";
        context.fillRect(0, 0, bounds.width, bounds.height);
      }
      return;
    }
    const image = new Image();
    let active = true;

    const draw = () => {
      if (!active || !image.naturalWidth || !image.naturalHeight) return;
      const bounds = canvas.getBoundingClientRect();
      const pixelRatio = window.devicePixelRatio || 1;
      const backingSize = getCanvasBackingSize(bounds.width, bounds.height, pixelRatio);
      canvas.width = backingSize.width;
      canvas.height = backingSize.height;
      const context = canvas.getContext("2d", { alpha: false });
      if (!context) return;
      context.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
      context.fillStyle = "#080a0b";
      context.fillRect(0, 0, bounds.width, bounds.height);
      context.imageSmoothingEnabled = scale === "Smooth";
      if (scale === "Smooth") context.imageSmoothingQuality = "high";
      const rect = fitPreviewImage(image.naturalWidth, image.naturalHeight, bounds.width, bounds.height, scale);
      context.drawImage(image, rect.x, rect.y, rect.width, rect.height);
      if (detections.length && overlay.boxes && processedWidth && processedHeight) {
        context.lineWidth = 1.5;
        context.font = "12px Segoe UI, Arial, sans-serif";
        context.textBaseline = "middle";
        for (const detection of detections) {
          const box = mapDetectionBox(detection.bbox_xyxy, processedWidth, processedHeight, rect);
          context.strokeStyle = detectionColor;
          context.strokeRect(box.x, box.y, box.width, box.height);
          const label = [
            overlay.classId ? `#${detection.class_id}` : "",
            overlay.className ? detection.class_name : "",
            overlay.confidence && detection.confidence !== null ? `${(detection.confidence * 100).toFixed(1)}%` : "",
          ].filter(Boolean).join(" · ");
          if (!label) continue;
          const labelWidth = context.measureText(label).width + 10;
          const labelTop = Math.max(0, box.y - 19);
          context.fillStyle = "#0b1112";
          context.fillRect(box.x, labelTop, labelWidth, 18);
          context.fillStyle = "#c4fff5";
          context.fillText(label, box.x + 5, labelTop + 9);
        }
      }
      const diameter = getFovDiameter(rect, fovDegrees);
      context.beginPath();
      context.arc(rect.x + rect.width / 2, rect.y + rect.height / 2, diameter / 2, 0, Math.PI * 2);
      context.strokeStyle = "rgba(67,207,190,.78)";
      context.lineWidth = 1;
      context.stroke();
    };

    image.onload = draw;
    image.src = frame.src;
    if (image.complete) draw();
    const observer = new ResizeObserver(draw);
    observer.observe(canvas);
    return () => {
      active = false;
      observer.disconnect();
      image.onload = null;
    };
  }, [frame?.id, frame?.src, frame?.width, frame?.height, scale, detections, overlay, detectionColor, processedWidth, processedHeight, fovDegrees]);

  return <canvas ref={canvasRef} className="preview-bitmap" aria-label={frame ? `Preview frame ${frame.width} by ${frame.height}` : "No frame received"} />;
}
