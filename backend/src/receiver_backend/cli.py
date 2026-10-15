from __future__ import annotations

import argparse
import asyncio
import json
import os
import socket
import sys
import time

import cv2

from .sources import UdpDecodedFrameSource
from .udp import encode_jpeg_frame


def _listen(args: argparse.Namespace) -> int:
    source = UdpDecodedFrameSource(args.host, args.port)
    try:
        address = source.address
        if address is None:
            source.receiver.open()
            address = source.address
        print(json.dumps({"event": "listening", "host": address[0], "port": address[1]}), flush=True)
        seen = 0
        while not args.count or seen < args.count:
            frame = source.read()
            if frame is None:
                continue
            seen += 1
            print(json.dumps({"event": "frame", "frame_id": frame.frame_id, "width": frame.width, "height": frame.height, "timestamp_ns": frame.timestamp_ns}), flush=True)
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(json.dumps({"event": "error", "message": str(error)}), file=sys.stderr, flush=True)
        return 1
    finally:
        source.close()
    return 0


def _send_image(args: argparse.Namespace) -> int:
    image = cv2.imread(args.image, cv2.IMREAD_COLOR)
    if image is None or image.size == 0:
        print(f"unable to decode image: {args.image}", file=sys.stderr)
        return 2
    height, width = image.shape[:2]
    ok, encoded = cv2.imencode(".jpg", image, [cv2.IMWRITE_JPEG_QUALITY, args.quality])
    if not ok:
        print("JPEG encoding failed", file=sys.stderr)
        return 2
    try:
        packets = encode_jpeg_frame(1, width, height, time.time_ns(), encoded.tobytes())
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        for packet in packets:
            sock.sendto(packet, (args.host, args.port))
    print(json.dumps({"event": "sent", "frame_id": 1, "width": width, "height": height, "chunks": len(packets)}))
    return 0


def _serve(args: argparse.Namespace) -> int:
    from .service import serve

    token = args.token or os.environ.get("RECEIVER_IPC_TOKEN", "")
    if len(token) < 32:
        print("serve requires a 32-character token via --token or RECEIVER_IPC_TOKEN", file=sys.stderr)
        return 2

    def ready(port: int, config_port: int | None, config_error: str | None) -> None:
        print(json.dumps({"event": "ready", "host": "127.0.0.1", "port": port,
                          "config_port": config_port, "config_error": config_error}), flush=True)

    try:
        asyncio.run(serve(token, args.data_dir, host="127.0.0.1", port=args.port, on_ready=ready,
                          web_root=args.config_root, web_port=args.config_port))
    except KeyboardInterrupt:
        return 130
    except Exception as error:
        print(json.dumps({"event": "error", "message": str(error)}), file=sys.stderr, flush=True)
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(prog="receiver-backend", description="Receiver backend and UDP loopback verifier")
    commands = parser.add_subparsers(dest="command", required=True)
    listen = commands.add_parser("udp-listen", help="receive and validate UDP frames")
    listen.add_argument("--host", default="0.0.0.0", help="bind address; use 127.0.0.1 for loopback-only tests")
    listen.add_argument("--port", type=int, default=4200)
    listen.add_argument("--count", type=int, default=0, help="stop after N frames; 0 listens until interrupted")
    listen.set_defaults(run=_listen)
    send = commands.add_parser("udp-send-image", help="send one local image to the loopback receiver verifier")
    send.add_argument("image")
    send.add_argument("--host", default="127.0.0.1")
    send.add_argument("--port", type=int, default=4200)
    send.add_argument("--quality", type=int, default=90)
    send.set_defaults(run=_send_image)
    service = commands.add_parser("serve", help="run authenticated loopback control and preview transport")
    service.add_argument("--token", help="high-entropy IPC token; RECEIVER_IPC_TOKEN is also accepted")
    service.add_argument("--data-dir", default=str(os.path.expanduser("~/.local/share/Receiver")))
    service.add_argument("--port", type=int, default=0, help="loopback IPC port; 0 selects an available port")
    service.add_argument("--config-root", help="built local browser configuration page directory")
    service.add_argument("--config-port", type=int, default=4201, help="local browser configuration port")
    service.set_defaults(run=_serve)
    args = parser.parse_args()
    if getattr(args, "count", 1) < 0:
        parser.error("--count cannot be negative")
    if hasattr(args, "quality") and not 1 <= args.quality <= 100:
        parser.error("--quality must be between 1 and 100")
    return args.run(args)


if __name__ == "__main__":
    raise SystemExit(main())
