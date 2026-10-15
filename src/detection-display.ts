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
