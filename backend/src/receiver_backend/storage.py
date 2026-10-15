from __future__ import annotations

import csv
import json
import queue
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from .results import DetectionResult


_STOP = object()


@dataclass(slots=True)
class _FlushRequest:
    completed: threading.Event = field(default_factory=threading.Event)
    error: OSError | None = None


class DetectionDataStore:
    """Non-blocking JSONL session writer with bounded buffering and CSV export."""

    def __init__(self, session_dir: str | Path, *, queue_size: int = 4096, on_error=None):
        self.session_dir = Path(session_dir)
        self.session_dir.mkdir(parents=True, exist_ok=True)
        self.jsonl_path = self.session_dir / "detections.jsonl"
        self._queue: queue.Queue[DetectionResult | _FlushRequest | object] = queue.Queue(maxsize=queue_size)
        self._on_error = on_error or (lambda _message: None)
        self._closed = False
        self._write_error: str | None = None
        self.dropped_results = 0
        self._thread = threading.Thread(target=self._writer, name="detection-store", daemon=True)
        self._thread.start()

    def submit(self, result: DetectionResult) -> bool:
        if self._closed or self._write_error is not None:
            return False
        try:
            self._queue.put_nowait(result)
            return True
        except queue.Full:
            self.dropped_results += 1
            self._on_error("Detection storage queue is full; a result was not persisted")
            return False

    def close(self, timeout: float | None = 2.0) -> bool:
        if self._closed:
            return not self._thread.is_alive()
        if not self._thread.is_alive():
            self._closed = True
            return self._write_error is None
        try:
            self._queue.put(_STOP, timeout=timeout)
        except queue.Full:
            self._on_error("Detection storage queue did not drain before shutdown")
            return False
        self._closed = True
        self._thread.join(timeout)
        return not self._thread.is_alive()

    def export_csv(self, path: str | Path) -> None:
        destination = Path(path).expanduser()
        if destination.resolve() == self.jsonl_path.resolve():
            raise ValueError("CSV destination cannot replace session JSONL data")
        self.flush()
        rows = []
        if self.jsonl_path.exists():
            with self.jsonl_path.open("r", encoding="utf-8") as stream:
                for line_number, line in enumerate(stream, start=1):
                    try:
                        result = json.loads(line)
                    except json.JSONDecodeError as error:
                        raise ValueError(f"invalid JSONL at line {line_number}") from error
                    for obj in result.get("objects", []):
                        rows.append({
                            "frame_id": result["frame_id"],
                            "timestamp_ns": result["timestamp_ns"],
                            "mode": result["mode"],
                            "model_id": result["model_id"],
                            "inference_device": result.get("inference_device"),
                            "source_width": result["source_width"],
                            "source_height": result["source_height"],
                            "class_id": obj["class_id"],
                            "class_name": obj["class_name"],
                            "x1": obj["bbox_xyxy"][0],
                            "y1": obj["bbox_xyxy"][1],
                            "x2": obj["bbox_xyxy"][2],
                            "y2": obj["bbox_xyxy"][3],
                            "confidence": obj["confidence"],
                            "pixel_count": obj["pixel_count"],
                            "total_processing_ms": result["total_processing_ms"],
                        })
        destination.parent.mkdir(parents=True, exist_ok=True)
        fieldnames = ["frame_id", "timestamp_ns", "mode", "model_id", "inference_device", "source_width", "source_height", "class_id", "class_name", "x1", "y1", "x2", "y2", "confidence", "pixel_count", "total_processing_ms"]
        with destination.open("w", encoding="utf-8", newline="") as stream:
            writer = csv.DictWriter(stream, fieldnames=fieldnames)
            writer.writeheader()
            writer.writerows(rows)

    def flush(self, timeout: float = 5.0) -> None:
        if self._write_error is not None:
            raise OSError(self._write_error)
        if self._closed:
            if self._thread.is_alive():
                raise TimeoutError("detection storage is still shutting down")
            return
        request = _FlushRequest()
        try:
            self._queue.put(request, timeout=timeout)
        except queue.Full as error:
            raise TimeoutError("detection storage queue did not accept a flush request") from error
        if not request.completed.wait(timeout):
            raise TimeoutError("detection storage flush timed out")
        if request.error is not None:
            raise OSError(f"Could not flush detection data: {request.error}") from request.error
        if self._write_error is not None:
            raise OSError(self._write_error)

    def _writer(self) -> None:
        try:
            with self.jsonl_path.open("a", encoding="utf-8", buffering=1) as stream:
                last_flush = time.monotonic()
                while True:
                    try:
                        item = self._queue.get(timeout=0.1)
                    except queue.Empty:
                        item = None
                    try:
                        if item is _STOP:
                            stream.flush()
                            return
                        if isinstance(item, _FlushRequest):
                            try:
                                stream.flush()
                            except OSError as error:
                                item.error = error
                                raise
                        elif isinstance(item, DetectionResult):
                            stream.write(json.dumps(item.to_dict(), separators=(",", ":"), allow_nan=False) + "\n")
                        if time.monotonic() - last_flush >= 0.25:
                            stream.flush()
                            last_flush = time.monotonic()
                    finally:
                        if isinstance(item, _FlushRequest):
                            item.completed.set()
                        if item is not None:
                            self._queue.task_done()
        except OSError as error:
            self._write_error = str(error)
            self._on_error(f"Could not write detection data: {error}")
            while True:
                try:
                    item = self._queue.get_nowait()
                except queue.Empty:
                    break
                if isinstance(item, _FlushRequest):
                    item.error = error
                    item.completed.set()
                self._queue.task_done()
