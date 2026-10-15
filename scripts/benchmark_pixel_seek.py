from __future__ import annotations

import argparse
import json
import math
import platform
import time

import cv2
import numpy as np

from receiver_backend.strategies import PixelColorRule, PixelSeekStrategy
from receiver_backend.udp import MAX_DIMENSION, MAX_FRAME_PIXELS


def positive(value: str) -> int:
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def main() -> int:
    parser = argparse.ArgumentParser(description="Measure Pixel Seek strategy throughput on a synthetic frame.")
    parser.add_argument("--width", type=positive, default=1280)
    parser.add_argument("--height", type=positive, default=720)
    parser.add_argument("--frames", type=positive, default=3000)
    parser.add_argument("--warmup", type=int, default=200)
    args = parser.parse_args()
    if args.warmup < 0:
        parser.error("--warmup cannot be negative")
    if args.width > MAX_DIMENSION or args.height > MAX_DIMENSION or args.width * args.height > MAX_FRAME_PIXELS:
        parser.error("dimensions exceed Receiver safety limits")

    # Stress fixture only: the full HSV interval exercises mask/contour work;
    # these values are not a Yellow, Red, or Purple production profile.
    rule = PixelColorRule(0, "Synthetic fixture", (((0, 0, 0), (179, 255, 255)),))
    strategy = PixelSeekStrategy((rule,))
    frame = np.zeros((args.height, args.width, 3), dtype=np.uint8)
    for index in range(args.warmup):
        strategy.process(frame, frame_id=index)

    durations: list[int] = []
    start = time.perf_counter_ns()
    result = None
    for index in range(args.frames):
        frame_start = time.perf_counter_ns()
        result = strategy.process(frame, frame_id=index)
        durations.append(time.perf_counter_ns() - frame_start)
    elapsed_ns = time.perf_counter_ns() - start

    durations.sort()
    p95_ns = durations[math.ceil(0.95 * len(durations)) - 1]
    print(json.dumps({
        "scope": "synthetic PixelSeekStrategy microbenchmark; excludes UDP, JPEG decode, storage, model inference, Tauri, and UI preview",
        "fixture": "all HSV values match; not an application color profile",
        "platform": platform.platform(),
        "opencv_version": cv2.__version__,
        "opencv_threads": cv2.getNumThreads(),
        "width": args.width,
        "height": args.height,
        "frames": args.frames,
        "warmup_frames": args.warmup,
        "elapsed_ms": elapsed_ns / 1_000_000,
        "throughput_fps": args.frames * 1_000_000_000 / elapsed_ns,
        "mean_processing_ms": sum(durations) / len(durations) / 1_000_000,
        "p95_processing_ms": p95_ns / 1_000_000,
        "detections_per_frame": len(result.objects) if result is not None else 0,
    }, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
