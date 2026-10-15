"""Target selection and motion contracts reconstructed from supplied sources."""

from __future__ import annotations

from dataclasses import dataclass
from enum import IntEnum
from typing import Protocol

import numpy as np

from .results import DetectionResult


class NativeDetectionMode(IntEnum):
    PIXEL_SEEK = 1
    OBJECT_DETECTION = 2


@dataclass(frozen=True, slots=True)
class FrameCoordinate:
    x: float
    y: float


@dataclass(frozen=True, slots=True)
class TargetCandidate:
    coordinate: FrameCoordinate
    bbox_xyxy: tuple[float, float, float, float]
    class_id: int | None
    score: float | None


@dataclass(frozen=True, slots=True)
class PriorTarget:
    bbox_xyxy: tuple[float, float, float, float] | None = None
    class_id: int | None = None


def object_detection_candidates(
    result: DetectionResult,
    *,
    allowed_class_ids: frozenset[int] | None = None,
    minimum_score: float = 0.0,
) -> tuple[TargetCandidate, ...]:
    """Apply class/score gates and expose each detection's box-center point."""
    if result.mode != "object_detection":
        raise ValueError("Object Detection candidates require an Object Detection result")
    if not 0 <= minimum_score <= 1:
        raise ValueError("minimum_score must be between 0 and 1")
    candidates = []
    for detection in result.objects:
        if allowed_class_ids is not None and detection.class_id not in allowed_class_ids:
            continue
        if detection.confidence is not None and detection.confidence < minimum_score:
            continue
        if detection.confidence is None and minimum_score > 0:
            continue
        left, top, right, bottom = detection.bbox_xyxy
        candidates.append(TargetCandidate(
            coordinate=FrameCoordinate((left + right) / 2, (top + bottom) / 2),
            bbox_xyxy=detection.bbox_xyxy,
            class_id=detection.class_id,
            score=detection.confidence,
        ))
    return tuple(candidates)


def object_detection_target(
    candidates: tuple[TargetCandidate, ...],
    *,
    frame_width: int,
    frame_height: int,
    fov_degrees: int,
    crop_x: int = 0,
    crop_y: int = 0,
    previous: PriorTarget | None = None,
    previous_is_fresh: bool = False,
    group_a_class_ids: frozenset[int] = frozenset(),
    group_b_class_ids: frozenset[int] = frozenset(),
    reject_group_b_center_crossing: bool = False,
) -> TargetCandidate | None:
    """Reconstruct the native center/box ranking and prior-box preference."""
    if frame_width < 1 or frame_height < 1 or not 1 <= fov_degrees <= 360:
        raise ValueError("frame dimensions and FOV angle must be valid")
    center_x, center_y = frame_width / 2, frame_height / 2
    radius = (min(frame_width, frame_height) - 1) * fov_degrees / 720
    radius_x = radius_y = radius

    def local_box(candidate: TargetCandidate) -> tuple[float, float, float, float]:
        left, top, right, bottom = candidate.bbox_xyxy
        return left - crop_x, top - crop_y, right - crop_x, bottom - crop_y

    eligible = []
    for candidate in candidates:
        left, top, right, bottom = local_box(candidate)
        width, height = right - left, bottom - top
        if width > 100:
            continue
        dx = max(left - center_x, 0.0, center_x - right)
        dy = max(top - center_y, 0.0, center_y - bottom)
        inside_fov = dx == 0 and dy == 0 if radius_x == 0 or radius_y == 0 else (
            dx * dx / (radius_x * radius_x) + dy * dy / (radius_y * radius_y) <= 1.0
        )
        if inside_fov:
            eligible.append(candidate)
    if not eligible:
        return None

    def distance_squared(candidate: TargetCandidate) -> float:
        left, top, right, bottom = local_box(candidate)
        dx = (left + right) * 0.5 - center_x
        dy = (top + bottom) * 0.5 - center_y
        return dx * dx + dy * dy

    nearest = min(eligible, key=distance_squared)
    tracked = None
    if previous_is_fresh and previous is not None and previous.bbox_xyxy is not None:
        old_left, old_top, old_right, old_bottom = previous.bbox_xyxy
        old_box = (old_left - crop_x, old_top - crop_y, old_right - crop_x, old_bottom - crop_y)
        for candidate in eligible:
            if candidate.class_id != previous.class_id:
                continue
            left, top, right, bottom = local_box(candidate)
            intersection = max(0.0, min(right, old_box[2]) - max(left, old_box[0])) * max(
                0.0, min(bottom, old_box[3]) - max(top, old_box[1]),
            )
            area = max(0.0, right - left) * max(0.0, bottom - top)
            if area > 0 and intersection / area > 0.5:
                tracked = candidate
        if tracked is not None and distance_squared(nearest) >= distance_squared(tracked) * 0.33:
            nearest = tracked

    if reject_group_b_center_crossing:
        for candidate in candidates:
            if candidate.class_id not in group_b_class_ids:
                continue
            left, top, right, bottom = local_box(candidate)
            if left <= center_x < right and top <= center_y < bottom:
                return None

    left, top, right, bottom = local_box(nearest)
    anchor_factor = 0.5 if nearest.class_id in group_a_class_ids else 0.05
    anchor = FrameCoordinate((left + right) * 0.5 + crop_x, top + (bottom - top) * anchor_factor + crop_y)
    return TargetCandidate(anchor, nearest.bbox_xyxy, nearest.class_id, nearest.score)


def validate_pixel_mask(mask: np.ndarray, frame_width: int, frame_height: int) -> None:
    """Validate the mask contract without assigning scan or fallback semantics."""
    if frame_width < 1 or frame_height < 1:
        raise ValueError("frame dimensions must be positive")
    if mask.dtype != np.uint8 or mask.ndim != 2 or mask.shape != (frame_height, frame_width):
        raise ValueError("Pixel Seek mask must be a uint8 array matching the processed frame")


def coordinate_in_fov(x: float, y: float, frame_width: int, frame_height: int, degrees: int) -> bool:
    """Test a frame-coordinate against the same centered circle drawn in Preview."""
    if frame_width < 1 or frame_height < 1 or not 1 <= degrees <= 360:
        raise ValueError("frame dimensions and FOV angle must be valid")
    radius = (min(frame_width, frame_height) - 1) * degrees / 720
    dx, dy = x - frame_width / 2, y - frame_height / 2
    return dx * dx + dy * dy <= radius * radius


def highest_pixel_in_fov(mask: np.ndarray, degrees: int) -> tuple[float, float] | None:
    """Return the topmost mask pixel center inside FOV, scanning the full mask."""
    if mask.ndim != 2:
        raise ValueError("Pixel Seek mask must be a two-dimensional array")
    height, width = mask.shape
    validate_pixel_mask(mask, width, height)
    if not 1 <= degrees <= 360:
        raise ValueError("FOV angle must be between 1 and 360 degrees")
    center_x, center_y = width / 2, height / 2
    radius = (min(width, height) - 1) * degrees / 720
    radius_squared = radius * radius
    first_y = max(0, int(np.ceil(center_y - radius - 0.5)))
    last_y = min(height - 1, int(np.floor(center_y + radius - 0.5)))
    for y in range(first_y, last_y + 1):
        dy = y + 0.5 - center_y
        remaining = radius_squared - dy * dy
        if remaining < 0:
            continue
        half_width = remaining ** 0.5
        left = max(0, int(np.ceil(center_x - half_width - 0.5)))
        right = min(width - 1, int(np.floor(center_x + half_width - 0.5)))
        if left > right:
            continue
        hits = np.flatnonzero(mask[y, left:right + 1]) + left
        if hits.size:
            x = min((int(hit) for hit in hits), key=lambda hit: abs(hit + 0.5 - center_x))
            return x + 0.5, y + 0.5
    return None


class PixelSeekSelector(Protocol):
    def select(self, mask: np.ndarray, frame_width: int, frame_height: int,
               fov_degrees: int) -> FrameCoordinate | None:
        """Select from the full mask with FOV applied only to this target stage."""


class TopmostPixelSeekSelector:
    """Select the highest accepted Pixel Seek mask pixel inside FOV."""

    def select(self, mask: np.ndarray, frame_width: int, frame_height: int,
               fov_degrees: int) -> FrameCoordinate | None:
        validate_pixel_mask(mask, frame_width, frame_height)
        point = highest_pixel_in_fov(mask, fov_degrees)
        return FrameCoordinate(*point) if point is not None else None


class ObjectDetectionSelector(Protocol):
    def select(
        self,
        candidates: tuple[TargetCandidate, ...],
        frame_width: int,
        frame_height: int,
        prior: PriorTarget | None,
    ) -> TargetCandidate | None:
        """Implement center ellipse, ranking, prior-box IoU, and class anchor."""


def select_target(
    result: DetectionResult,
    *,
    pixel_mask: np.ndarray | None,
    frame_width: int,
    frame_height: int,
    pixel_selector: PixelSeekSelector,
    object_selector: ObjectDetectionSelector,
    allowed_class_ids: frozenset[int] | None = None,
    minimum_score: float = 0.0,
    fov_degrees: int = 36,
    crop_x: int = 0,
    crop_y: int = 0,
    prior: PriorTarget | None = None,
) -> FrameCoordinate | None:
    """Route a detection result through its native-mode selection contract."""
    if result.mode == "pixel_seek":
        mode = NativeDetectionMode.PIXEL_SEEK
    elif result.mode == "object_detection":
        mode = NativeDetectionMode.OBJECT_DETECTION
    else:
        raise ValueError(f"unsupported detection result mode: {result.mode}")

    if mode is NativeDetectionMode.PIXEL_SEEK:
        if pixel_mask is None:
            return None
        validate_pixel_mask(pixel_mask, frame_width, frame_height)
        return pixel_selector.select(pixel_mask, frame_width, frame_height, fov_degrees)
    candidates = object_detection_candidates(
        result, allowed_class_ids=allowed_class_ids, minimum_score=minimum_score,
    )
    candidates = tuple(candidate for candidate in candidates if coordinate_in_fov(
        candidate.coordinate.x - crop_x, candidate.coordinate.y - crop_y,
        frame_width, frame_height, fov_degrees,
    ))
    selected = object_selector.select(candidates, frame_width, frame_height, prior)
    return selected.coordinate if selected is not None else None


@dataclass(slots=True)
class FractionalDeltaAccumulator:
    """Carry sub-pixel movement forward and emit integer relative deltas."""

    residual_x: float = 0.0
    residual_y: float = 0.0

    def consume(self, dx: float, dy: float) -> tuple[int, int]:
        total_x, total_y = self.residual_x + dx, self.residual_y + dy
        integer_x, integer_y = int(total_x), int(total_y)
        self.residual_x, self.residual_y = total_x - integer_x, total_y - integer_y
        return integer_x, integer_y


def apply_axis_lock(dx: int, dy: int, *, lock_x: bool = False, lock_y: bool = False) -> tuple[int, int]:
    """Zero any axis currently locked by a future control surface."""
    return (0 if lock_x else dx, 0 if lock_y else dy)


class LogicManager(Protocol):
    def relative_delta(
        self,
        selected: FrameCoordinate,
        frame_width: int,
        frame_height: int,
    ) -> tuple[float, float]:
        """Implement source-defined error, offsets, and divisor interpolation."""


class CommandMouseTransport(Protocol):
    def send_command(self, command: str, payload: dict[str, int]) -> None: ...


class MethodMouseTransport(Protocol):
    def moveMouseRelative(self, dx: int, dy: int) -> None: ...
