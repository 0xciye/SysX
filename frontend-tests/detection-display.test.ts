import assert from "node:assert/strict";
import test from "node:test";
import { filterPreviewDetections, formatDetectionSummary } from "../src/detection-display.ts";
import type { DetectionObject } from "../src/backend-client.ts";

const detections: DetectionObject[] = [
  { class_id: 0, class_name: "below", bbox_xyxy: [0, 0, 1, 1], confidence: 0.49 },
  { class_id: 1, class_name: "at threshold", bbox_xyxy: [1, 1, 2, 2], confidence: 0.5 },
  { class_id: 2, class_name: "above", bbox_xyxy: [2, 2, 3, 3], confidence: 0.9 },
  { class_id: 3, class_name: "no score", bbox_xyxy: [3, 3, 4, 4], confidence: null },
];

test("Object Detection hides below-threshold and unscored detections", () => {
  assert.deepEqual(
    filterPreviewDetections(detections, "object_detection", 0.5).map(({ class_name }) => class_name),
    ["at threshold", "above"],
  );
});

test("Pixel Seek leaves its detections unfiltered by AI confidence", () => {
  assert.equal(filterPreviewDetections(detections, "pixel_seek", 1), detections);
});

test("an invalid confidence threshold falls back to zero but still excludes unscored objects", () => {
  assert.deepEqual(
    filterPreviewDetections(detections, "object_detection", Number.NaN).map(({ class_name }) => class_name),
    ["below", "at threshold", "above"],
  );
});

test("Object Detection summary shows class IDs and rounded scores separated by vertical bars", () => {
  assert.equal(formatDetectionSummary(detections), "[0] 49% | [1] 50% | [2] 90%");
});
