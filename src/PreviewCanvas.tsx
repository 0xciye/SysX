import { useEffect, useRef } from "react";
import { fitPreviewImage, getCanvasBackingSize, getFovDiameter, mapDetectionBox, mapDetectionPoint, type PreviewScale } from "./preview-layout";
import type { DetectionObject } from "./backend-client";
import { formatDetectionSummary } from "./detection-display";

export type PreviewFrame = {
  id: number;
  src: string;
  width: number;
  height: number;
};

type OverlayOptions = {
  boxes: boolean;
  classId: boolean;
  confidence: boolean;
};

function drawTargetGuide(
  context: CanvasRenderingContext2D,
  centerX: number,
  centerY: number,
  targetX: number,
  targetY: number,
  color: string,
) {
  const dx = targetX - centerX;
  const dy = targetY - centerY;
  const distance = Math.hypot(dx, dy);
  const ringRadius = 9;
  const lineEndX = distance > ringRadius ? targetX - dx / distance * ringRadius : targetX;
  const lineEndY = distance > ringRadius ? targetY - dy / distance * ringRadius : targetY;

  context.lineCap = "round";
  context.beginPath();
  context.moveTo(centerX, centerY);
  context.lineTo(lineEndX, lineEndY);
  context.strokeStyle = "rgba(5, 9, 10, .82)";
  context.lineWidth = 4;
  context.stroke();

  context.beginPath();
  context.moveTo(centerX, centerY);
  context.lineTo(lineEndX, lineEndY);
  context.strokeStyle = color;
  context.globalAlpha = 0.92;
  context.lineWidth = 2;
  context.stroke();
  context.globalAlpha = 1;

  context.beginPath();
  context.arc(centerX, centerY, 3.5, 0, Math.PI * 2);
  context.strokeStyle = "rgba(5, 9, 10, .9)";
  context.lineWidth = 3;
  context.stroke();
  context.beginPath();
  context.arc(centerX, centerY, 2.5, 0, Math.PI * 2);
  context.strokeStyle = color;
  context.lineWidth = 1;
  context.stroke();

  context.beginPath();
  context.arc(targetX, targetY, ringRadius + 1.5, 0, Math.PI * 2);
  context.strokeStyle = "rgba(5, 9, 10, .9)";
  context.lineWidth = 4;
  context.stroke();
  context.beginPath();
  context.arc(targetX, targetY, ringRadius, 0, Math.PI * 2);
  context.strokeStyle = color;
  context.lineWidth = 2;
  context.stroke();
  context.beginPath();
  context.arc(targetX, targetY, 4.2, 0, Math.PI * 2);
  context.fillStyle = color;
  context.fill();
}

function classColor(classId: number): string {
  return `hsl(${(classId * 137.508 + 24) % 360} 82% 62%)`;
}

function drawDetectionBox(
  context: CanvasRenderingContext2D,
  box: { x: number; y: number; width: number; height: number },
  color: string,
) {
  const corner = Math.min(16, Math.max(7, Math.min(box.width, box.height) * 0.22));
  context.lineJoin = "round";
  context.strokeStyle = "rgba(5, 9, 10, .88)";
  context.lineWidth = 4;
  context.strokeRect(box.x, box.y, box.width, box.height);
  context.strokeStyle = color;
  context.lineWidth = 1.5;
  context.strokeRect(box.x, box.y, box.width, box.height);
  context.lineWidth = 3;
  context.beginPath();
  context.moveTo(box.x, box.y + corner); context.lineTo(box.x, box.y); context.lineTo(box.x + corner, box.y);
  context.moveTo(box.x + box.width - corner, box.y); context.lineTo(box.x + box.width, box.y); context.lineTo(box.x + box.width, box.y + corner);
  context.moveTo(box.x + box.width, box.y + box.height - corner); context.lineTo(box.x + box.width, box.y + box.height); context.lineTo(box.x + box.width - corner, box.y + box.height);
  context.moveTo(box.x + corner, box.y + box.height); context.lineTo(box.x, box.y + box.height); context.lineTo(box.x, box.y + box.height - corner);
  context.strokeStyle = color;
  context.stroke();
}

function drawDetectionSummary(context: CanvasRenderingContext2D, summary: string, rect: { x: number; y: number; width: number }, color: string) {
  if (!summary) return;
  const maxWidth = Math.max(0, rect.width - 20);
  if (maxWidth < 12) return;
  const fontSize = Math.min(15, Math.max(10, maxWidth / Math.max(12, summary.length * 0.58)));
  context.font = `600 ${fontSize}px Segoe UI, Arial, sans-serif`;
  const width = Math.min(maxWidth, context.measureText(summary).width + 22);
  const height = 29;
  const x = rect.x + (rect.width - width) / 2;
  const y = rect.y + 10;
  context.beginPath();
  context.roundRect(x, y, width, height, 8);
  context.fillStyle = "rgba(10, 13, 15, .94)";
  context.fill();
  context.strokeStyle = color;
  context.lineWidth = 1.5;
  context.stroke();
  context.fillStyle = "#E6EBEC";
  context.textBaseline = "middle";
  context.textAlign = "center";
  context.fillText(summary, x + width / 2, y + height / 2, width - 12);
  context.textAlign = "start";
}

export function PreviewCanvas({ frame, scale, detections = [], targetPoint = null, overlay = { boxes: true, classId: true, confidence: true }, detectionColor = "#51d7c4", processedWidth, processedHeight, cropX = 0, cropY = 0, fovDegrees = 36 }: {
  frame: PreviewFrame | null;
  scale: PreviewScale;
  detections?: DetectionObject[];
  targetPoint?: [number, number] | null;
  overlay?: OverlayOptions;
  detectionColor?: string;
  processedWidth?: number;
  processedHeight?: number;
  cropX?: number;
  cropY?: number;
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
        for (const detection of detections) {
          const box = mapDetectionBox(detection.bbox_xyxy, processedWidth, processedHeight, rect, cropX, cropY);
          drawDetectionBox(context, box, overlay.classId ? classColor(detection.class_id) : detectionColor);
        }
        if (overlay.classId && overlay.confidence) {
          drawDetectionSummary(context, formatDetectionSummary(detections), rect, classColor(detections[0].class_id));
        }
      }
      const diameter = getFovDiameter(rect, fovDegrees);
      context.beginPath();
      context.arc(rect.x + rect.width / 2, rect.y + rect.height / 2, diameter / 2, 0, Math.PI * 2);
      context.strokeStyle = "rgba(67,207,190,.78)";
      context.lineWidth = 1;
      context.stroke();
      if (targetPoint && processedWidth && processedHeight) {
        const target = mapDetectionPoint(targetPoint, processedWidth, processedHeight, rect, cropX, cropY);
        const centerX = rect.x + rect.width / 2;
        const centerY = rect.y + rect.height / 2;
        drawTargetGuide(context, centerX, centerY, target.x, target.y, detectionColor);
      }
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
  }, [frame?.id, frame?.src, frame?.width, frame?.height, scale, detections, targetPoint, overlay, detectionColor, processedWidth, processedHeight, cropX, cropY, fovDegrees]);

  return <canvas ref={canvasRef} className="preview-bitmap" aria-label={frame ? `Preview frame ${frame.width} by ${frame.height}` : "No frame received"} />;
}
