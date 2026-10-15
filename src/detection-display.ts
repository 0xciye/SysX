import type { DetectionObject } from "./backend-client";

export type Strategy = "pixel_seek" | "object_detection";

export function filterPreviewDetections(
  detections: DetectionObject[],
  strategy: Strategy,
  minimumConfidence: number,
): DetectionObject[] {
  if (strategy === "pixel_seek") return detections;
  const threshold = Number.isFinite(minimumConfidence) ? minimumConfidence : 0;
  return detections.filter((detection) => detection.confidence !== null && detection.confidence >= threshold);
}

export function formatDetectionSummary(detections: DetectionObject[]): string {
  return detections
    .filter((detection) => detection.confidence !== null)
    .map((detection) => `[${detection.class_id}] ${Math.round((detection.confidence ?? 0) * 100)}%`)
    .join(" | ");
}
