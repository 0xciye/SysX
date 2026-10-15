export type PreviewScale = "Smooth" | "Pixel perfect";

export type PreviewImageRect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

export type DetectionBox = readonly [number, number, number, number];

export function mapDetectionBox(
  box: DetectionBox,
  processedWidth: number,
  processedHeight: number,
  imageRect: PreviewImageRect,
): PreviewImageRect {
  if (![processedWidth, processedHeight, ...box, imageRect.x, imageRect.y, imageRect.width, imageRect.height].every(Number.isFinite) ||
      processedWidth <= 0 || processedHeight <= 0 || imageRect.width <= 0 || imageRect.height <= 0) {
    throw new RangeError("Detection and preview dimensions must be finite and positive");
  }
  const [left, top, right, bottom] = box;
  return {
    x: imageRect.x + left / processedWidth * imageRect.width,
    y: imageRect.y + top / processedHeight * imageRect.height,
    width: (right - left) / processedWidth * imageRect.width,
    height: (bottom - top) / processedHeight * imageRect.height,
  };
}

export function getCanvasBackingSize(width: number, height: number, pixelRatio: number): { width: number; height: number } {
  if (![width, height, pixelRatio].every(Number.isFinite) || width <= 0 || height <= 0 || pixelRatio <= 0) {
    throw new RangeError("Canvas dimensions and pixel ratio must be finite and positive");
  }
  return {
    width: Math.max(1, Math.round(width * pixelRatio)),
    height: Math.max(1, Math.round(height * pixelRatio)),
  };
}

export function getFovDiameter(imageRect: PreviewImageRect, degrees: number): number {
  if (![imageRect.x, imageRect.y, imageRect.width, imageRect.height, degrees].every(Number.isFinite) ||
      imageRect.width <= 0 || imageRect.height <= 0 || !Number.isInteger(degrees) || degrees < 1 || degrees > 360) {
    throw new RangeError("FOV frame dimensions and angle must be valid");
  }
  return Math.max(0, Math.min(imageRect.width, imageRect.height) - 1) * degrees / 360;
}

export function fitPreviewImage(
  sourceWidth: number,
  sourceHeight: number,
  viewportWidth: number,
  viewportHeight: number,
  mode: PreviewScale,
): PreviewImageRect {
  if (![sourceWidth, sourceHeight, viewportWidth, viewportHeight].every(Number.isFinite) ||
      sourceWidth <= 0 || sourceHeight <= 0 || viewportWidth <= 0 || viewportHeight <= 0) {
    throw new RangeError("Source and preview dimensions must be positive finite numbers");
  }

  const fit = Math.min(viewportWidth / sourceWidth, viewportHeight / sourceHeight);
  const scale = mode === "Pixel perfect" && fit >= 1 ? Math.max(1, Math.floor(fit)) : fit;
  const width = sourceWidth * scale;
  const height = sourceHeight * scale;
  return {
    x: (viewportWidth - width) / 2,
    y: (viewportHeight - height) / 2,
    width,
    height,
  };
}
