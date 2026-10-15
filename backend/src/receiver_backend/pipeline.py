from __future__ import annotations

import threading
import time
from collections import deque
from dataclasses import dataclass
from typing import Callable

from .results import DetectionResult
from .storage import DetectionDataStore
from .strategies import FrameResolutionMismatch, PixelColorRule, StrategyFactory, StrategyName
from .config import ModelDefinition
from .sources import SourceFrame


@dataclass(frozen=True, slots=True)
class ProcessedFrame:
    frame: SourceFrame
    result: DetectionResult
    published_ns: int
    crop_x: int
    crop_y: int


@dataclass(frozen=True, slots=True)
class PipelineStats:
    processed_frames: int
    dropped_input_frames: int
    stale_results: int
    rejected_frames: int
    resolution_mismatches: int
    processing_fps: float | None
    last_error: str | None


class LatestFramePipeline:
    """Single-worker strategy runner; bounds pending work to one newest frame."""

    def __init__(self, *, strategy: StrategyName | str = StrategyName.PIXEL_SEEK,
                 pixel_rules: tuple[PixelColorRule, ...] = (), model: ModelDefinition | None = None,
                 crop_resolution: int | None = None, fov_degrees: int = 36,
                 store: DetectionDataStore | None = None,
                 on_error: Callable[[str], None] | None = None,
                 on_recovered: Callable[[str], None] | None = None):
        if crop_resolution is not None and (not isinstance(crop_resolution, int) or isinstance(crop_resolution, bool) or crop_resolution < 1):
            raise ValueError("crop resolution must be a positive integer")
        if not isinstance(fov_degrees, int) or isinstance(fov_degrees, bool) or not 1 <= fov_degrees <= 360:
            raise ValueError("FOV must be an integer from 1 to 360 degrees")
        self._condition = threading.Condition()
        self._strategy_name = StrategyName(strategy)
        self._pixel_rules = pixel_rules
        self._model_definition = model
        self._strategy = StrategyFactory.create(self._strategy_name, pixel_rules=pixel_rules, model=model)
        self._crop_resolution = crop_resolution
        self._fov_degrees = fov_degrees
        self._generation = 0
        self._pending: tuple[SourceFrame, int] | None = None
        self._latest: ProcessedFrame | None = None
        self._store = store
        self._on_error = on_error or (lambda _message: None)
        self._on_recovered = on_recovered or (lambda _message: None)
        self._processed = 0
        self._dropped = 0
        self._stale = 0
        self._rejected = 0
        self._resolution_mismatches = 0
        self._errors: deque[str] = deque(maxlen=1)
        self._completed_times: deque[int] = deque(maxlen=4096)
        self._stopping = False
        self._thread = threading.Thread(target=self._run, name="receiver-processing", daemon=True)
        self._thread.start()

    def submit(self, frame: SourceFrame) -> None:
        with self._condition:
            if self._stopping:
                raise RuntimeError("pipeline is shutting down")
            if self._pending is not None:
                self._dropped += 1
            self._pending = (frame, self._generation)
            self._condition.notify()

    def configure(self, *, strategy: StrategyName | str, pixel_rules: tuple[PixelColorRule, ...] = (),
                  model: ModelDefinition | None = None, crop_resolution: int | None = None,
                  fov_degrees: int | None = None) -> None:
        selected = StrategyName(strategy)
        resolution = self._crop_resolution if crop_resolution is None else crop_resolution
        if resolution is not None and (not isinstance(resolution, int) or isinstance(resolution, bool) or resolution < 1):
            raise ValueError("crop resolution must be a positive integer")
        degrees = self._fov_degrees if fov_degrees is None else fov_degrees
        if not isinstance(degrees, int) or isinstance(degrees, bool) or not 1 <= degrees <= 360:
            raise ValueError("FOV must be an integer from 1 to 360 degrees")
        candidate = StrategyFactory.create(selected, pixel_rules=pixel_rules, model=model)
        with self._condition:
            self._strategy_name = selected
            self._pixel_rules = pixel_rules
            self._model_definition = model
            self._strategy = candidate
            self._crop_resolution = resolution
            self._fov_degrees = degrees
            self._generation += 1
            self._latest = None
            if self._pending is not None:
                self._pending = None
                self._dropped += 1
            self._condition.notify_all()

    @property
    def latest(self) -> ProcessedFrame | None:
        with self._condition:
            return self._latest

    def stats(self) -> PipelineStats:
        with self._condition:
            cutoff = time.perf_counter_ns() - 1_000_000_000
            while self._completed_times and self._completed_times[0] < cutoff:
                self._completed_times.popleft()
            fps = None
            if len(self._completed_times) >= 2:
                elapsed = self._completed_times[-1] - self._completed_times[0]
                if elapsed > 0:
                    fps = (len(self._completed_times) - 1) * 1_000_000_000 / elapsed
            return PipelineStats(self._processed, self._dropped, self._stale, self._rejected,
                                 self._resolution_mismatches, fps, self._errors[-1] if self._errors else None)

    def close(self, timeout: float = 5.0) -> bool:
        with self._condition:
            self._stopping = True
            self._pending = None
            self._condition.notify_all()
        self._thread.join(timeout)
        return not self._thread.is_alive()

    def _run(self) -> None:
        while True:
            with self._condition:
                while self._pending is None and not self._stopping:
                    self._condition.wait()
                if self._stopping:
                    return
                frame, generation = self._pending
                self._pending = None
                strategy = self._strategy
                fov_degrees = self._fov_degrees
            try:
                source_height, source_width = frame.image.shape[:2]
                resolution = self._crop_resolution
                if resolution is not None and (resolution > source_width or resolution > source_height):
                    raise FrameResolutionMismatch(
                        f"frame is {source_width}x{source_height}; crop resolution {resolution}x{resolution} does not fit"
                    )
                crop_width, crop_height = (source_width, source_height) if resolution is None else (resolution, resolution)
                crop_x = (source_width - crop_width) // 2
                crop_y = (source_height - crop_height) // 2
                cropped = frame.image[crop_y:crop_y + crop_height, crop_x:crop_x + crop_width]
                result = strategy.process(
                    cropped, frame_id=frame.frame_id, timestamp_ns=frame.timestamp_ns,
                    source_width=source_width, source_height=source_height,
                    crop_x=crop_x, crop_y=crop_y, fov_degrees=fov_degrees,
                )
                finished = time.perf_counter_ns()
                recovered_error = None
                with self._condition:
                    if generation != self._generation:
                        self._stale += 1
                        continue
                    preview_frame = SourceFrame(frame.frame_id, frame.timestamp_ns, cropped, frame.source)
                    processed = ProcessedFrame(preview_frame, result, finished, crop_x, crop_y)
                    self._latest = processed
                    self._processed += 1
                    self._completed_times.append(finished)
                    if self._errors:
                        recovered_error = self._errors[-1]
                        self._errors.clear()
                if recovered_error is not None:
                    self._on_recovered(recovered_error)
                if self._store is not None:
                    self._store.submit(result)
            except Exception as error:
                message = str(error)
                with self._condition:
                    self._rejected += 1
                    if isinstance(error, FrameResolutionMismatch):
                        self._resolution_mismatches += 1
                    self._errors.append(message)
                self._on_error(message)
