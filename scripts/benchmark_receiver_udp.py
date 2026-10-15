from __future__ import annotations

import argparse
import asyncio
import json
import math
import socket
import tempfile
import time

import cv2
import numpy as np
import websockets

from receiver_backend.service import PIXEL_COLOR_PROFILES, ReceiverService
from receiver_backend.strategies import PixelColorRule
from receiver_backend.udp import MAX_DIMENSION, MAX_FRAME_PIXELS, encode_jpeg_frame


def positive_int(value: str) -> int:
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def free_udp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def send_synthetic_frames(port: int, *, width: int, height: int, fps: int, duration_s: float, quality: int):
    frame = np.zeros((height, width, 3), dtype=np.uint8)
    frame[height // 4:height // 2, width // 4:width // 2] = (0, 0, 255)
    ok, encoded = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, quality])
    if not ok:
        raise RuntimeError("synthetic JPEG encoding failed")
    jpeg = encoded.tobytes()
    chunks = len(encode_jpeg_frame(0, width, height, 1, jpeg))
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sent = datagrams = byte_count = skipped_slots = frame_id = 0
    interval = 1 / fps
    started = time.perf_counter()
    deadline = started + duration_s
    next_send = started
    try:
        while next_send < deadline:
            frame_id += 1
            packets = encode_jpeg_frame(frame_id, width, height, time.perf_counter_ns(), jpeg)
            for packet in packets:
                sock.sendto(packet, ("127.0.0.1", port))
                datagrams += 1
                byte_count += len(packet)
            sent += 1
            next_send += interval
            remaining = next_send - time.perf_counter()
            if remaining > 0:
                time.sleep(remaining)
            else:
                missed_slots = math.floor(-remaining / interval) + 1
                skipped_slots += missed_slots
                frame_id += missed_slots
                next_send += missed_slots * interval
    finally:
        sock.close()
    return {
        "frames": sent,
        "skipped_sender_slots": skipped_slots,
        "elapsed_s": time.perf_counter() - started,
        "datagrams": datagrams,
        "bytes": byte_count,
        "chunks_per_frame": chunks,
        "jpeg_bytes": len(jpeg),
    }


async def run(args: argparse.Namespace) -> dict[str, object]:
    # Synthetic profile only; this never populates the user's production color profiles.
    original_red_profile = PIXEL_COLOR_PROFILES["red"]
    PIXEL_COLOR_PROFILES["red"] = (PixelColorRule(
        1, "Synthetic red fixture", (((0, 200, 200), (10, 255, 255)),)
    ),)
    try:
        with tempfile.TemporaryDirectory(prefix="receiver-udp-benchmark-") as data_dir:
            service = ReceiverService("b" * 48, data_dir, udp_host="127.0.0.1", udp_port=free_udp_port())
            server = await websockets.serve(service.handle, "127.0.0.1", 0, max_size=64 * 1024)
            ipc_port = server.sockets[0].getsockname()[1]
            uri = f"ws://127.0.0.1:{ipc_port}/receiver?token={'b' * 48}"
            try:
                async with websockets.connect(uri, origin="http://127.0.0.1:1420", max_size=16 * 1024 * 1024) as client:
                    await client.recv()
                    await client.send(json.dumps({"version": 1, "type": "set_pixel_color", "color": "red"}))
                    ack = json.loads(await client.recv())
                    if ack.get("state", {}).get("pixel_profile_configured") is not True:
                        raise RuntimeError("synthetic profile was not activated")
                    before = service.pipeline.stats()
                    sender_task = asyncio.create_task(asyncio.to_thread(
                        send_synthetic_frames, service._udp_port,
                        width=args.width, height=args.height, fps=args.fps,
                        duration_s=args.duration, quality=args.quality,
                    ))
                    preview_frames = 0
                    while not sender_task.done():
                        try:
                            message = await asyncio.wait_for(client.recv(), timeout=0.1)
                            if isinstance(message, bytes):
                                preview_frames += 1
                        except asyncio.TimeoutError:
                            pass
                    sender = await sender_task
                    drain_deadline = time.monotonic() + 0.5
                    while time.monotonic() < drain_deadline:
                        try:
                            message = await asyncio.wait_for(client.recv(), timeout=0.05)
                            if isinstance(message, bytes):
                                preview_frames += 1
                        except asyncio.TimeoutError:
                            pass
                    after = service.pipeline.stats()
                    state = service.state()
                    latest = service.pipeline.latest
                    return {
                        "scope": "local UDP -> reassembly/JPEG decode -> Pixel Seek -> binary WebSocket preview; excludes OBS capture, Tauri renderer, Windows, and production HSV profiles",
                        "synthetic_fixture": True,
                        "frame_size": [args.width, args.height],
                        "requested_sender_fps": args.fps,
                        "sender_fps": sender["frames"] / sender["elapsed_s"],
                        "source_fps": state["source_fps"],
                        "sender_skipped_slots": sender["skipped_sender_slots"],
                        "jpeg_bytes": sender["jpeg_bytes"],
                        "chunks_per_frame": sender["chunks_per_frame"],
                        "datagrams_per_second": sender["datagrams"] / sender["elapsed_s"],
                        "udp_megabits_per_second": sender["bytes"] * 8 / sender["elapsed_s"] / 1_000_000,
                        "processed_frames": after.processed_frames - before.processed_frames,
                        "processing_fps": after.processing_fps,
                        "dropped_input_frames": after.dropped_input_frames - before.dropped_input_frames,
                        "transport_dropped_frames": state["transport_dropped_frames"],
                        "invalid_packets": state["invalid_packets"],
                        "rejected_frames": after.rejected_frames - before.rejected_frames,
                        "websocket_preview_frames": preview_frames,
                        "detections_in_latest_result": len(latest.result.objects) if latest else 0,
                    }
            finally:
                server.close()
                await server.wait_closed()
                await service.close()
    finally:
        PIXEL_COLOR_PROFILES["red"] = original_red_profile


def main() -> int:
    parser = argparse.ArgumentParser(description="Measure the Receiver UDP and Pixel Seek backend with synthetic frames.")
    parser.add_argument("--width", type=positive_int, default=1280)
    parser.add_argument("--height", type=positive_int, default=720)
    parser.add_argument("--fps", type=positive_int, default=360)
    parser.add_argument("--duration", type=float, default=3.0, help="sender duration in seconds")
    parser.add_argument("--quality", type=int, default=88, help="synthetic JPEG quality, 1..100")
    args = parser.parse_args()
    if not math.isfinite(args.duration) or args.duration <= 0:
        parser.error("--duration must be positive and finite")
    if not 1 <= args.quality <= 100:
        parser.error("--quality must be between 1 and 100")
    if args.width > MAX_DIMENSION or args.height > MAX_DIMENSION or args.width * args.height > MAX_FRAME_PIXELS:
        parser.error("dimensions exceed Receiver safety limits")
    try:
        print(json.dumps(asyncio.run(run(args)), indent=2))
    except Exception as error:
        parser.exit(1, f"benchmark failed: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
