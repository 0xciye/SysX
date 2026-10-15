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
                 store: DetectionDataStore | None = None, on_error: Callable[[str], None] | None = None):
        self._condition = threading.Condition()
        self._strategy_name = StrategyName(strategy)
        self._pixel_rules = pixel_rules
        self._model_definition = model
        self._strategy = StrategyFactory.create(self._strategy_name, pixel_rules=pixel_rules, model=model)
        self._generation = 0
        self._pending: tuple[SourceFrame, int] | None = None
        self._latest: ProcessedFrame | None = None
        self._store = store
        self._on_error = on_error or (lambda _message: None)
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

    def configure(self, *, strategy: StrategyName | str, pixel_rules: tuple[PixelColorRule, ...] = (), model: ModelDefinition | None = None) -> None:
        selected = StrategyName(strategy)
        candidate = StrategyFactory.create(selected, pixel_rules=pixel_rules, model=model)
        with self._condition:
            self._strategy_name = selected
            self._pixel_rules = pixel_rules
            self._model_definition = model
            self._strategy = candidate
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
            try:
                result = strategy.process(frame.image, frame_id=frame.frame_id, timestamp_ns=frame.timestamp_ns)
                finished = time.perf_counter_ns()
                with self._condition:
                    if generation != self._generation:
                        self._stale += 1
                        continue
                    processed = ProcessedFrame(frame, result, finished)
                    self._latest = processed
                    self._processed += 1
                    self._completed_times.append(finished)
                    self._errors.clear()
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
