import type { PreviewScale } from "./preview-layout";
import type { PixelTargetColor } from "./backend-client";
import type { Strategy } from "./detection-display";

export type AppSettings = {
  strategy: Strategy;
  fovDegrees: number;
  scale: PreviewScale;
  showPreview: boolean;
  cropResolution: number;
  port: string;
  pixelColor: PixelTargetColor;
  activeModelId: string | null;
  modelWidth: string;
  modelHeight: string;
  modelConfidence: string;
  modelIou: string;
  modelBackend: string;
  modelPrecision: string;
  computeTarget: string;
};

export const DEFAULT_APP_SETTINGS: AppSettings = {
  strategy: "pixel_seek",
  fovDegrees: 36,
  scale: "Smooth",
  showPreview: true,
  cropResolution: 640,
  port: "4200",
  pixelColor: "yellow",
  activeModelId: null,
  modelWidth: "640",
  modelHeight: "640",
  modelConfidence: "0.5",
  modelIou: "0.45",
  modelBackend: "onnxruntime",
  modelPrecision: "fp16",
  computeTarget: "cpu",
};

const STORAGE_KEY = "receiver.app-settings.v1";

export function loadAppSettings(storage: Pick<Storage, "getItem">): AppSettings {
  try {
    const saved = storage.getItem(STORAGE_KEY);
    if (!saved) return DEFAULT_APP_SETTINGS;
    const value = JSON.parse(saved) as Partial<AppSettings>;
    return {
      ...DEFAULT_APP_SETTINGS,
      ...value,
      strategy: value.strategy === "object_detection" ? "object_detection" : "pixel_seek",
      fovDegrees: Number.isInteger(value.fovDegrees) ? Math.min(360, Math.max(1, value.fovDegrees!)) : DEFAULT_APP_SETTINGS.fovDegrees,
      scale: value.scale === "Pixel perfect" ? "Pixel perfect" : "Smooth",
      showPreview: typeof value.showPreview === "boolean" ? value.showPreview : DEFAULT_APP_SETTINGS.showPreview,
      cropResolution: Number.isInteger(value.cropResolution) && value.cropResolution! >= 1 && value.cropResolution! <= 8192 ? value.cropResolution! : DEFAULT_APP_SETTINGS.cropResolution,
      port: /^\d{1,5}$/.test(value.port ?? "") && Number(value.port) > 0 && Number(value.port) <= 65535 ? value.port! : DEFAULT_APP_SETTINGS.port,
      pixelColor: value.pixelColor === "red" || value.pixelColor === "purple" ? value.pixelColor : "yellow",
      activeModelId: typeof value.activeModelId === "string" ? value.activeModelId : null,
    };
  } catch {
    return DEFAULT_APP_SETTINGS;
  }
}

export function saveAppSettings(storage: Pick<Storage, "setItem">, settings: AppSettings) {
  storage.setItem(STORAGE_KEY, JSON.stringify(settings));
}
