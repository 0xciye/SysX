from __future__ import annotations

import time
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

import cv2
import numpy as np

from .udp import EncodedFrame, UdpFrameSource


@dataclass(frozen=True, slots=True)
class SourceFrame:
    frame_id: int
    timestamp_ns: int
    image: np.ndarray
    source: str

    @property
    def width(self) -> int:
        return int(self.image.shape[1])

    @property
    def height(self) -> int:
        return int(self.image.shape[0])


class FrameSource(Protocol):
    def read(self) -> SourceFrame | None: ...
    def close(self) -> None: ...


class LocalImageSource:
    def __init__(self, path: str | Path):
        self.path = Path(path).expanduser()
        self._read = False

    def read(self) -> SourceFrame | None:
        if self._read:
            return None
        self._read = True
        image = cv2.imread(str(self.path), cv2.IMREAD_COLOR)
        if image is None or image.size == 0:
            raise ValueError(f"unable to decode image: {self.path}")
        return SourceFrame(1, time.time_ns(), image, str(self.path))

    def close(self) -> None:
        return None


class LocalVideoSource:
    """Read local video at its source cadence; drop overdue frames to stay realtime."""

    def __init__(self, path: str | Path):
        self.path = Path(path).expanduser()
        self._capture: cv2.VideoCapture | None = None
        self._frame_id = 0
        self._fps = 0.0
        self._next_due = 0.0
        self._closed = threading.Event()

    def open(self) -> None:
        if self._capture is not None:
            return
        capture = cv2.VideoCapture(str(self.path))
        if not capture.isOpened():
            capture.release()
            raise ValueError(f"unable to open video: {self.path}")
        self._capture = capture
        fps = capture.get(cv2.CAP_PROP_FPS)
        self._fps = fps if np.isfinite(fps) and fps > 0 else 0.0
        self._next_due = time.monotonic()

    def read(self) -> SourceFrame | None:
        self.open()
        assert self._capture is not None
        if self._fps:
            now = time.monotonic()
            if now < self._next_due:
                if self._closed.wait(self._next_due - now):
                    return None
            interval = 1.0 / self._fps
            self._next_due += interval
            now = time.monotonic()
            if self._next_due < now - interval:
                self._next_due = now
        ok, image = self._capture.read()
        if not ok:
            return None
        self._frame_id += 1
        return SourceFrame(self._frame_id, time.time_ns(), image, str(self.path))

    def close(self) -> None:
        self._closed.set()
        capture, self._capture = self._capture, None
        if capture is not None:
            capture.release()


class UdpDecodedFrameSource:
    def __init__(self, host: str = "0.0.0.0", port: int = 4200):
        self.receiver = UdpFrameSource(host, port)

    @property
    def address(self) -> tuple[str, int] | None:
        return self.receiver.address

    def read(self) -> SourceFrame | None:
        encoded = self.receiver.read(timeout_s=0.25)
        if encoded is None:
            return None
        return decode_udp_frame(encoded)

    def close(self) -> None:
        self.receiver.close()


def decode_udp_frame(frame: EncodedFrame) -> SourceFrame:
    if (
        len(frame.jpeg) < 4
        or not frame.jpeg.startswith(b"\xff\xd8")
        or not frame.jpeg.endswith(b"\xff\xd9")
    ):
        raise ValueError(f"frame {frame.frame_id} is not a complete JPEG frame")
    data = np.frombuffer(frame.jpeg, dtype=np.uint8)
    image = cv2.imdecode(data, cv2.IMREAD_COLOR)
    if image is None or image.size == 0:
        raise ValueError(f"frame {frame.frame_id} contains invalid JPEG data")
    height, width = image.shape[:2]
    if (width, height) != (frame.width, frame.height):
        raise ValueError(
            f"frame {frame.frame_id} declares {frame.width}x{frame.height} "
            f"but JPEG decodes as {width}x{height}"
        )
    return SourceFrame(frame.frame_id, frame.timestamp_ns, image, "udp")
