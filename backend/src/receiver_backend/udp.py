from __future__ import annotations

import socket
import struct
import time
import math
from collections import OrderedDict
from dataclasses import dataclass, field


MAGIC = b"RCVR"
VERSION = 1
HEADER = struct.Struct("!4sBBQHHHHQ")
MAX_DATAGRAM_BYTES = 1200
MAX_FRAME_BYTES = 64 * 1024 * 1024
MAX_FRAME_PIXELS = 33_554_432
MAX_DIMENSION = 8192
MAX_IN_FLIGHT_FRAMES = 32
MAX_REASSEMBLY_BYTES = 128 * 1024 * 1024
DEFAULT_TIMEOUT_NS = 100_000_000


@dataclass(frozen=True, slots=True)
class EncodedFrame:
    frame_id: int
    width: int
    height: int
    timestamp_ns: int
    jpeg: bytes


@dataclass(slots=True)
class _Assembly:
    frame_id: int
    width: int
    height: int
    timestamp_ns: int
    total_chunks: int
    created_ns: int
    chunks: dict[int, bytes] = field(default_factory=dict)
    byte_count: int = 0


class UdpFrameReassembler:
    """Versioned chunk reassembly with strict bounds and latest-frame output."""

    def __init__(self, *, timeout_ns: int = DEFAULT_TIMEOUT_NS):
        self.timeout_ns = timeout_ns
        self._assemblies: OrderedDict[int, _Assembly] = OrderedDict()
        self._reassembly_bytes = 0
        self._ready: EncodedFrame | None = None
        self.dropped_frames = 0
        self.invalid_packets = 0
        self.expired_frames = 0

    @property
    def in_flight_bytes(self) -> int:
        return self._reassembly_bytes

    def accept(self, packet: bytes, *, now_ns: int | None = None) -> EncodedFrame | None:
        now = time.monotonic_ns() if now_ns is None else now_ns
        self._expire(now)
        if len(packet) <= HEADER.size or len(packet) > MAX_DATAGRAM_BYTES:
            self.invalid_packets += 1
            return None
        try:
            magic, version, flags, frame_id, index, total, width, height, timestamp_ns = HEADER.unpack_from(packet)
        except struct.error:
            self.invalid_packets += 1
            return None
        payload = packet[HEADER.size:]
        if (magic != MAGIC or version != VERSION or flags != 0 or not payload or
                total < 1 or index >= total or width < 1 or height < 1 or
                width > MAX_DIMENSION or height > MAX_DIMENSION or
                width * height > MAX_FRAME_PIXELS or
                total > math.ceil(MAX_FRAME_BYTES / (MAX_DATAGRAM_BYTES - HEADER.size))):
            self.invalid_packets += 1
            return None
        payload_limit = MAX_DATAGRAM_BYTES - HEADER.size
        if index < total - 1 and len(payload) != payload_limit:
            self.invalid_packets += 1
            return None
        if ((index == 0 and not payload.startswith(b"\xff\xd8")) or
                (index == total - 1 and not payload.endswith(b"\xff\xd9"))):
            self._discard(frame_id)
            self.invalid_packets += 1
            return None

        assembly = self._assemblies.get(frame_id)
        if assembly is None:
            while len(self._assemblies) >= MAX_IN_FLIGHT_FRAMES:
                _, evicted = self._assemblies.popitem(last=False)
                self._reassembly_bytes -= evicted.byte_count
                self.dropped_frames += 1
            assembly = _Assembly(frame_id, width, height, timestamp_ns, total, now)
            self._assemblies[frame_id] = assembly
        elif (assembly.width, assembly.height, assembly.timestamp_ns, assembly.total_chunks) != (width, height, timestamp_ns, total):
            self._discard(frame_id)
            self.invalid_packets += 1
            return None

        previous = assembly.chunks.get(index)
        if previous is not None:
            if previous != payload:
                self._discard(frame_id)
                self.invalid_packets += 1
            return None
        if assembly.byte_count + len(payload) > MAX_FRAME_BYTES or self._reassembly_bytes + len(payload) > MAX_REASSEMBLY_BYTES:
            self._discard(frame_id)
            self.invalid_packets += 1
            return None
        assembly.chunks[index] = payload
        assembly.byte_count += len(payload)
        self._reassembly_bytes += len(payload)
        if len(assembly.chunks) != total:
            return None

        jpeg = b"".join(assembly.chunks[position] for position in range(total))
        self._discard(frame_id)
        completed = EncodedFrame(frame_id, width, height, timestamp_ns, jpeg)
        if self._ready is not None:
            self.dropped_frames += 1
        self._ready = completed
        return completed

    def take_latest(self) -> EncodedFrame | None:
        frame, self._ready = self._ready, None
        return frame

    def _discard(self, frame_id: int) -> None:
        removed = self._assemblies.pop(frame_id, None)
        if removed is not None:
            self._reassembly_bytes -= removed.byte_count

    def _expire(self, now_ns: int) -> None:
        expired = [frame_id for frame_id, item in self._assemblies.items() if now_ns - item.created_ns > self.timeout_ns]
        for frame_id in expired:
            self._discard(frame_id)
            self.expired_frames += 1


class UdpFrameSource:
    """Loopback or LAN UDP source; binds only to the explicitly configured address."""

    def __init__(self, host: str = "0.0.0.0", port: int = 4200):
        if not 1 <= port <= 65535:
            raise ValueError("UDP port must be between 1 and 65535")
        self.host = host
        self.port = port
        self.reassembler = UdpFrameReassembler()
        self._socket: socket.socket | None = None

    def open(self) -> None:
        if self._socket is not None:
            return
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.bind((self.host, self.port))
        except OSError:
            sock.close()
            raise
        sock.settimeout(0.05)
        self._socket = sock

    @property
    def address(self) -> tuple[str, int] | None:
        return self._socket.getsockname() if self._socket else None

    def read(self, timeout_s: float = 0.05) -> EncodedFrame | None:
        if self._socket is None:
            self.open()
        deadline = time.monotonic() + max(0.0, timeout_s)
        while time.monotonic() < deadline:
            try:
                assert self._socket is not None
                packet, _ = self._socket.recvfrom(MAX_DATAGRAM_BYTES + 1)
            except socket.timeout:
                self.reassembler._expire(time.monotonic_ns())
                continue
            except OSError:
                if self._socket is None:
                    return None
                raise
            self.reassembler.accept(packet)
            ready = self.reassembler.take_latest()
            if ready is not None:
                return ready
        return self.reassembler.take_latest()

    def close(self) -> None:
        sock, self._socket = self._socket, None
        if sock is not None:
            sock.close()

    def __enter__(self) -> UdpFrameSource:
        self.open()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def encode_jpeg_frame(frame_id: int, width: int, height: int, timestamp_ns: int, jpeg: bytes) -> list[bytes]:
    """Reference packetizer for the loopback protocol verifier and future sender."""
    if not jpeg or len(jpeg) > MAX_FRAME_BYTES:
        raise ValueError("JPEG payload is empty or exceeds the frame limit")
    if not (1 <= width <= MAX_DIMENSION and 1 <= height <= MAX_DIMENSION and width * height <= MAX_FRAME_PIXELS):
        raise ValueError("frame dimensions exceed protocol limits")
    payload_limit = MAX_DATAGRAM_BYTES - HEADER.size
    chunks = [jpeg[offset:offset + payload_limit] for offset in range(0, len(jpeg), payload_limit)]
    if len(chunks) > 65535:
        raise ValueError("frame requires too many UDP chunks")
    total = len(chunks)
    return [HEADER.pack(MAGIC, VERSION, 0, frame_id, index, total, width, height, timestamp_ns) + chunk for index, chunk in enumerate(chunks)]
