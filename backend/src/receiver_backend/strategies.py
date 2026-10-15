from __future__ import annotations

import importlib
import math
import sys
import time
from dataclasses import dataclass, replace
from enum import StrEnum
from pathlib import Path

import cv2
import numpy as np

from .config import ModelDefinition
from .results import DetectionObject, DetectionResult
from .targeting import (
    PriorTarget,
    highest_pixel_in_fov,
    object_detection_candidates,
    object_detection_target,
)


class StrategyName(StrEnum):
    PIXEL_SEEK = "pixel_seek"
    OBJECT_DETECTION = "object_detection"


class FrameResolutionMismatch(ValueError):
    """An incoming frame does not fit the configured crop or model input."""


def validate_inference_device(device: str, *, torch_module=None, platform_name: str | None = None) -> None:
    """Fail early with a usable error when an explicitly selected accelerator is unavailable."""
    if device in {"auto", "cpu"}:
        return
    current_platform = platform_name or sys.platform
    if device == "mps" and current_platform != "darwin":
        raise RuntimeError("Apple MPS is available only on macOS; select Automatic or CPU on this platform")
    if torch_module is None:
        try:
            torch_module = importlib.import_module("torch")
        except ImportError as error:
            raise RuntimeError(f"Cannot use {device.upper()}: PyTorch is not installed in this backend") from error
    if device == "cuda":
        if not torch_module.cuda.is_available():
            raise RuntimeError("CUDA was selected, but PyTorch cannot access an NVIDIA CUDA device; select Automatic or CPU")
        return
    mps = getattr(getattr(torch_module, "backends", None), "mps", None)
    if mps is None or not mps.is_available():
        raise RuntimeError("Apple MPS was selected, but PyTorch reports it unavailable; select Automatic or CPU")


@dataclass(frozen=True, slots=True)
class PixelColorRule:
    class_id: int
    class_name: str
    hsv_ranges: tuple[tuple[tuple[int, int, int], tuple[int, int, int]], ...]
    min_area: float = 1
    min_pixels: int = 1
    enabled: bool = True
    roi: tuple[float, float, float, float] | None = None

    def __post_init__(self) -> None:
        if not self.hsv_ranges:
            raise ValueError("at least one HSV range is required")
        if self.min_area < 0 or self.min_pixels < 0:
            raise ValueError("area and pixel thresholds cannot be negative")
        if self.roi is not None and (
            len(self.roi) != 4 or not all(math.isfinite(value) for value in self.roi)
            or not (0 <= self.roi[0] < self.roi[2] <= 1 and 0 <= self.roi[1] < self.roi[3] <= 1)
        ):
            raise ValueError("ROI must be normalized as (left, top, right, bottom) within 0..1")
        for lower, upper in self.hsv_ranges:
            if len(lower) != 3 or len(upper) != 3:
                raise ValueError("HSV range bounds must have three channels")
            if any(value < 0 for value in lower + upper):
                raise ValueError("HSV values cannot be negative")
            if lower[0] > 179 or upper[0] > 179 or any(value > 255 for value in lower[1:] + upper[1:]):
                raise ValueError("HSV values are outside OpenCV's range")


class PixelSeekStrategy:
    def __init__(self, rules: tuple[PixelColorRule, ...], morphology_size: int = 1):
        if morphology_size < 1 or morphology_size % 2 == 0:
            raise ValueError("morphology_size must be a positive odd integer")
        self.rules = rules
        self.kernel = cv2.getStructuringElement(cv2.MORPH_RECT, (morphology_size, morphology_size))

    def process(self, frame: np.ndarray, frame_id: int, timestamp_ns: int | None = None, *,
                source_width: int | None = None, source_height: int | None = None,
                crop_x: int = 0, crop_y: int = 0, fov_degrees: int = 36) -> DetectionResult:
        if frame.ndim != 3 or frame.shape[2] != 3 or frame.size == 0:
            raise ValueError("frame must be a non-empty BGR image")
        height, width = frame.shape[:2]
        original_width = width + crop_x if source_width is None else source_width
        original_height = height + crop_y if source_height is None else source_height
        start = time.perf_counter_ns()
        rgb = cv2.cvtColor(frame, cv2.COLOR_BGR2RGB)
        hsv = cv2.cvtColor(rgb, cv2.COLOR_RGB2HSV)
        preprocess_done = time.perf_counter_ns()
        found: list[DetectionObject] = []
        target_mask = np.zeros((height, width), dtype=np.uint8)
        for rule in self.rules:
            if not rule.enabled:
                continue
            if rule.roi is None:
                left, top, right, bottom = 0, 0, width, height
            else:
                roi_left = math.floor(rule.roi[0] * original_width)
                roi_top = math.floor(rule.roi[1] * original_height)
                roi_right = math.ceil(rule.roi[2] * original_width)
                roi_bottom = math.ceil(rule.roi[3] * original_height)
                left, top = max(roi_left, crop_x) - crop_x, max(roi_top, crop_y) - crop_y
                right, bottom = min(roi_right, crop_x + width) - crop_x, min(roi_bottom, crop_y + height) - crop_y
                if right <= left or bottom <= top:
                    continue
            roi_hsv = hsv[top:bottom, left:right]
            mask = np.zeros(roi_hsv.shape[:2], dtype=np.uint8)
            for lower, upper in rule.hsv_ranges:
                mask = cv2.bitwise_or(mask, cv2.inRange(roi_hsv, np.array(lower, dtype=np.uint8), np.array(upper, dtype=np.uint8)))
            if self.kernel.shape[0] > 1:
                mask = cv2.morphologyEx(mask, cv2.MORPH_OPEN, self.kernel)
            contours, _ = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
            for contour in contours:
                area = cv2.contourArea(contour)
                if area < rule.min_area:
                    continue
                component = np.zeros_like(mask)
                cv2.drawContours(component, [contour], -1, 255, thickness=cv2.FILLED)
                pixel_count = int(cv2.countNonZero(cv2.bitwise_and(mask, component)))
                if pixel_count < rule.min_pixels:
                    continue
                target_mask[top:bottom, left:right] |= cv2.bitwise_and(mask, component)
                x, y, box_width, box_height = cv2.boundingRect(contour)
                found.append(DetectionObject(
                    class_id=rule.class_id,
                    class_name=rule.class_name or str(rule.class_id),
                    bbox_xyxy=(float(x + left + crop_x), float(y + top + crop_y), float(x + left + box_width + crop_x), float(y + top + box_height + crop_y)),
                    confidence=None,
                    pixel_count=pixel_count,
                ))
        pixel_target = highest_pixel_in_fov(target_mask, fov_degrees) if found else None
        target_xy = None if pixel_target is None else (pixel_target[0] + crop_x, pixel_target[1] + crop_y)
        done = time.perf_counter_ns()
        preprocess_ms = (preprocess_done - start) / 1_000_000
        detection_ms = (done - preprocess_done) / 1_000_000
        return DetectionResult(
            frame_id=frame_id,
            timestamp_ns=timestamp_ns if timestamp_ns is not None else time.time_ns(),
            mode=StrategyName.PIXEL_SEEK.value,
            model_id=None,
            source_width=original_width,
            source_height=original_height,
            processed_width=width,
            processed_height=height,
            objects=tuple(found),
            preprocess_ms=preprocess_ms,
            detection_ms=detection_ms,
            postprocess_ms=0.0,
            total_processing_ms=preprocess_ms + detection_ms,
            inference_device="cpu",
            target_xy=target_xy,
        )


class ObjectDetectionStrategy:
    """Lazy Ultralytics adapter; it never downloads or substitutes a model."""

    def __init__(self, definition: ModelDefinition):
        self.definition = definition
        self._model = None
        self._previous_target: PriorTarget | None = None
        self._previous_frame_id: int | None = None
        validate_inference_device(definition.device)

    def _load(self):
        path = Path(self.definition.path).expanduser()
        if not path.is_file():
            raise FileNotFoundError(f"model file not found: {path}")
        try:
            ultralytics = importlib.import_module("ultralytics")
        except ImportError as error:
            raise RuntimeError("Object Detection requires the optional 'inference' dependencies") from error
        self._model = ultralytics.YOLO(str(path))
        return self._model

    def process(self, frame: np.ndarray, frame_id: int, timestamp_ns: int | None = None, allow_resize: bool = False, *,
                source_width: int | None = None, source_height: int | None = None,
                crop_x: int = 0, crop_y: int = 0, fov_degrees: int = 36) -> DetectionResult:
        if frame.ndim != 3 or frame.shape[2] != 3 or frame.size == 0:
            raise ValueError("frame must be a non-empty BGR image")
        height, width = frame.shape[:2]
        if (width, height) != (self.definition.input_width, self.definition.input_height) and not allow_resize:
            raise FrameResolutionMismatch(
                f"frame is {width}x{height}; active model expects "
                f"{self.definition.input_width}x{self.definition.input_height}"
            )
        source_width = width + crop_x if source_width is None else source_width
        source_height = height + crop_y if source_height is None else source_height
        if allow_resize and (width, height) != (self.definition.input_width, self.definition.input_height):
            frame = cv2.resize(frame, (self.definition.input_width, self.definition.input_height), interpolation=cv2.INTER_AREA)
            height, width = frame.shape[:2]
        started = time.perf_counter_ns()
        model = self._model or self._load()
        device = self.definition.device if self.definition.device != "auto" else None
        prediction = model.predict(
            source=frame,
            imgsz=(self.definition.input_height, self.definition.input_width),
            conf=self.definition.confidence,
            iou=self.definition.iou,
            agnostic_nms=False,
            device=device,
            verbose=False,
        )[0]
        inferred = time.perf_counter_ns()
        runtime_device = getattr(getattr(model, "predictor", None), "device", None)
        inference_device = str(runtime_device) if runtime_device is not None else None
        names = prediction.names or {}
        objects = []
        boxes = prediction.boxes
        if boxes is not None:
            for xyxy, cls, confidence in zip(boxes.xyxy.tolist(), boxes.cls.tolist(), boxes.conf.tolist()):
                class_id = int(cls)
                label = (self.definition.class_names or {}).get(class_id, names.get(class_id, str(class_id)))
                objects.append(DetectionObject(
                    class_id=class_id,
                    class_name=str(label),
                    bbox_xyxy=(float(xyxy[0] + crop_x), float(xyxy[1] + crop_y), float(xyxy[2] + crop_x), float(xyxy[3] + crop_y)),
                    confidence=float(confidence),
                    pixel_count=None,
                ))
        done = time.perf_counter_ns()
        result = DetectionResult(
            frame_id=frame_id,
            timestamp_ns=timestamp_ns if timestamp_ns is not None else time.time_ns(),
            mode=StrategyName.OBJECT_DETECTION.value,
            model_id=self.definition.id,
            source_width=source_width,
            source_height=source_height,
            processed_width=width,
            processed_height=height,
            objects=tuple(objects),
            preprocess_ms=0.0,
            detection_ms=(inferred - started) / 1_000_000,
            postprocess_ms=(done - inferred) / 1_000_000,
            total_processing_ms=(done - started) / 1_000_000,
            inference_device=inference_device,
        )
        candidates = object_detection_candidates(result, minimum_score=self.definition.confidence)
        selected = object_detection_target(
            candidates,
            frame_width=width,
            frame_height=height,
            fov_degrees=fov_degrees,
            crop_x=crop_x,
            crop_y=crop_y,
            previous=self._previous_target,
            previous_is_fresh=self._previous_frame_id is not None and frame_id == self._previous_frame_id + 1,
        )
        done = time.perf_counter_ns()
        self._previous_target = None if selected is None else PriorTarget(selected.bbox_xyxy, selected.class_id)
        self._previous_frame_id = frame_id if selected is not None else None
        return replace(
            result,
            postprocess_ms=(done - inferred) / 1_000_000,
            total_processing_ms=(done - started) / 1_000_000,
            target_xy=None if selected is None else (selected.coordinate.x, selected.coordinate.y),
        )


class StrategyFactory:
    @staticmethod
    def create(name: StrategyName | str, *, pixel_rules: tuple[PixelColorRule, ...] = (), model: ModelDefinition | None = None):
        strategy = StrategyName(name)
        if strategy is StrategyName.PIXEL_SEEK:
            return PixelSeekStrategy(pixel_rules)
        if model is None:
            raise ValueError("an active user model is required for Object Detection")
        return ObjectDetectionStrategy(model)
