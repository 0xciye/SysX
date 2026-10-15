import assert from "node:assert/strict";
import test from "node:test";
import { fitPreviewImage, getCanvasBackingSize, getFovDiameter, mapDetectionBox, mapDetectionPoint } from "../src/preview-layout.ts";

test("smooth mode fills the largest inscribed image rectangle without distortion", () => {
  const rect = fitPreviewImage(192, 108, 640, 640, "Smooth");
  assert.deepEqual(rect, { x: 0, y: 140, width: 640, height: 360 });
  assert.equal(rect.width / rect.height, 192 / 108);
});

test("pixel perfect enlarges a 192 square frame by an integer factor", () => {
  const rect = fitPreviewImage(192, 192, 640, 598, "Pixel perfect");
  assert.deepEqual(rect, { x: 32, y: 11, width: 576, height: 576 });
});

test("large source frames fit inside the shared viewport", () => {
  const rect = fitPreviewImage(1280, 720, 640, 598, "Smooth");
  assert.equal(rect.width, 640);
  assert.equal(rect.height, 360);
  assert.equal(rect.x, 0);
  assert.equal(rect.y, 119);
});

test("invalid dimensions are rejected", () => {
  assert.throws(() => fitPreviewImage(192, 0, 640, 640, "Smooth"), RangeError);
});

test("detection boxes map into the image rectangle after letterboxing", () => {
  const image = fitPreviewImage(1920, 1080, 640, 640, "Smooth");
  const box = mapDetectionBox([480, 270, 1440, 810], 1920, 1080, image);
  assert.deepEqual(box, { x: 160, y: 230, width: 320, height: 180 });
});

test("full-frame detection boxes map onto the centered crop preview", () => {
  const crop = fitPreviewImage(256, 256, 640, 640, "Smooth");
  const box = mapDetectionBox([640, 300, 700, 360], 256, 256, crop, 640, 300);
  assert.deepEqual(box, { x: 0, y: 0, width: 150, height: 150 });
});

test("selected frame coordinates map onto the cropped preview pixel center", () => {
  const crop = fitPreviewImage(256, 256, 640, 640, "Smooth");
  assert.deepEqual(mapDetectionPoint([768.5, 428.5], 256, 256, crop, 640, 300), { x: 321.25, y: 321.25 });
});

test("HiDPI canvas backing dimensions scale by the display pixel ratio", () => {
  assert.deepEqual(getCanvasBackingSize(640, 360, 2), { width: 1280, height: 720 });
});

test("FOV angle scales against the displayed frame's shorter side", () => {
  const image = fitPreviewImage(1920, 1080, 640, 640, "Smooth");
  assert.equal(getFovDiameter(image, 360), 359);
  assert.equal(getFovDiameter(image, 180), 179.5);
  assert.equal(getFovDiameter(image, 1), 359 / 360);
});

test("invalid detection and backing dimensions are rejected", () => {
  const image = fitPreviewImage(192, 192, 640, 640, "Smooth");
  assert.throws(() => mapDetectionBox([0, 0, 10, 10], 0, 192, image), RangeError);
  assert.throws(() => getCanvasBackingSize(640, 360, 0), RangeError);
  assert.throws(() => getFovDiameter(image, 0), RangeError);
  assert.throws(() => getFovDiameter(image, 361), RangeError);
});
