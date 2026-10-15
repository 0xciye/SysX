from __future__ import annotations

from dataclasses import asdict, dataclass
import math
from typing import Literal


@dataclass(frozen=True, slots=True)
class DetectionObject:
    class_id: int
    class_name: str
    bbox_xyxy: tuple[float, float, float, float]
    confidence: float | None
    pixel_count: int | None

    def __post_init__(self) -> None:
        if self.class_id < 0 or not self.class_name:
            raise ValueError("class ID must be non-negative and class name is required")
        if len(self.bbox_xyxy) != 4:
            raise ValueError("bbox_xyxy must contain exactly four coordinates")
        if not all(math.isfinite(value) for value in self.bbox_xyxy):
            raise ValueError("bbox coordinates must be finite")
        if self.confidence is not None and not 0 <= self.confidence <= 1:
            raise ValueError("confidence must be between 0 and 1")
        if self.pixel_count is not None and self.pixel_count < 0:
            raise ValueError("pixel_count cannot be negative")


@dataclass(frozen=True, slots=True)
class DetectionResult:
    frame_id: int
    timestamp_ns: int
    mode: Literal["pixel_seek", "object_detection"]
    model_id: str | None
    source_width: int
    source_height: int
    processed_width: int
    processed_height: int
    objects: tuple[DetectionObject, ...]
    preprocess_ms: float
    detection_ms: float
    postprocess_ms: float
    total_processing_ms: float
    inference_device: str | None = None

    def __post_init__(self) -> None:
        if self.frame_id < 0 or self.timestamp_ns < 0:
            raise ValueError("frame ID and timestamp must be non-negative")
        if min(self.source_width, self.source_height, self.processed_width, self.processed_height) < 1:
            raise ValueError("source and processed dimensions must be positive")
        timings = (self.preprocess_ms, self.detection_ms, self.postprocess_ms, self.total_processing_ms)
        if not all(math.isfinite(value) and value >= 0 for value in timings):
            raise ValueError("processing timings must be finite and non-negative")

    def to_dict(self) -> dict[str, object]:
        return asdict(self)
