import assert from "node:assert/strict";
import test from "node:test";
import { DEFAULT_APP_SETTINGS, loadAppSettings, saveAppSettings } from "../src/app-settings.ts";

function createStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  };
}

test("settings can be saved and restored as a single applied snapshot", () => {
  const storage = createStorage();
  const settings = { ...DEFAULT_APP_SETTINGS, strategy: "object_detection" as const, fovDegrees: 180, scale: "Pixel perfect" as const, showPreview: false, cropResolution: 256, port: "4202", pixelColor: "purple" as const };
  saveAppSettings(storage, settings);
  assert.deepEqual(loadAppSettings(storage), settings);
});

test("invalid or corrupted persisted values fall back to safe defaults", () => {
  const storage = createStorage({ "receiver.app-settings.v1": JSON.stringify({ strategy: "unknown", fovDegrees: 999, scale: "bad", cropResolution: 9000, port: "99999", pixelColor: "green" }) });
  const restored = loadAppSettings(storage);
  assert.equal(restored.strategy, DEFAULT_APP_SETTINGS.strategy);
  assert.equal(restored.fovDegrees, 360);
  assert.equal(restored.scale, DEFAULT_APP_SETTINGS.scale);
  assert.equal(restored.showPreview, DEFAULT_APP_SETTINGS.showPreview);
  assert.equal(restored.cropResolution, DEFAULT_APP_SETTINGS.cropResolution);
  assert.equal(restored.port, DEFAULT_APP_SETTINGS.port);
  assert.equal(restored.pixelColor, DEFAULT_APP_SETTINGS.pixelColor);
});
