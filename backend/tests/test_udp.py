from __future__ import annotations

import socket

from receiver_backend.udp import HEADER, UdpFrameReassembler, UdpFrameSource, encode_jpeg_frame


def test_reassembles_out_of_order_chunks_and_ignores_exact_duplicates():
    data = b"\xff\xd8" + bytes(index % 251 for index in range(2200)) + b"\xff\xd9"
    packets = encode_jpeg_frame(41, 192, 192, 99, data)
    receiver = UdpFrameReassembler()

    assert receiver.accept(packets[1], now_ns=1) is None
    assert receiver.accept(packets[1], now_ns=2) is None
    result = receiver.accept(packets[0], now_ns=3)

    assert result is not None
    assert (result.frame_id, result.width, result.height, result.timestamp_ns) == (41, 192, 192, 99)
    assert result.jpeg == data
    assert receiver.in_flight_bytes == 0


def test_rejects_conflicting_metadata_and_corrupts_only_that_frame():
    packet = encode_jpeg_frame(3, 12, 12, 90, b"\xff\xd8jpeg\xff\xd9")[0]
    receiver = UdpFrameReassembler()
    receiver.accept(packet, now_ns=1)
    modified = bytearray(packet)
    modified[16:18] = (13).to_bytes(2, "big")

    assert receiver.accept(bytes(modified), now_ns=2) is None
    assert receiver.invalid_packets == 1
    assert receiver.in_flight_bytes == 0


def test_expires_incomplete_frames_and_bounds_memory():
    packet = encode_jpeg_frame(8, 12, 12, 101, b"\xff\xd8" + bytes(1396) + b"\xff\xd9")[0]
    receiver = UdpFrameReassembler(timeout_ns=10)
    receiver.accept(packet, now_ns=1)
    assert receiver.in_flight_bytes > 0

    receiver.accept(packet, now_ns=12)
    assert receiver.expired_frames == 1
    assert receiver.in_flight_bytes > 0  # the second packet starts a fresh assembly


def test_latest_completed_frame_replaces_old_and_counts_drop():
    receiver = UdpFrameReassembler()
    first = encode_jpeg_frame(1, 1, 1, 1, b"\xff\xd8one\xff\xd9")[0]
    second = encode_jpeg_frame(2, 1, 1, 2, b"\xff\xd8two\xff\xd9")[0]
    receiver.accept(first, now_ns=1)
    receiver.accept(second, now_ns=2)

    assert receiver.take_latest().frame_id == 2
    assert receiver.dropped_frames == 1


def test_rejects_oversized_datagram_before_parsing():
    receiver = UdpFrameReassembler()
    assert receiver.accept(bytes(HEADER.size + 1200), now_ns=1) is None
    assert receiver.invalid_packets == 1


def test_rejects_invalid_jpeg_boundary_markers_before_retaining_payloads():
    receiver = UdpFrameReassembler()
    missing_start = encode_jpeg_frame(10, 12, 12, 1, b"bad-start\xff\xd9")[0]
    missing_end = encode_jpeg_frame(11, 12, 12, 2, b"\xff\xd8bad-end")[0]

    assert receiver.accept(missing_start, now_ns=1) is None
    assert receiver.accept(missing_end, now_ns=2) is None
    assert receiver.invalid_packets == 2
    assert receiver.in_flight_bytes == 0


def test_invalid_final_jpeg_marker_discards_existing_assembly():
    jpeg = b"\xff\xd8" + bytes(1170) + b"tail\xff\xd9"
    packets = encode_jpeg_frame(12, 12, 12, 3, jpeg)
    receiver = UdpFrameReassembler()
    assert len(packets) == 2
    assert receiver.accept(packets[0], now_ns=1) is None
    assert receiver.in_flight_bytes > 0

    assert receiver.accept(packets[1][:-1] + b"x", now_ns=2) is None
    assert receiver.invalid_packets == 1
    assert receiver.in_flight_bytes == 0


def test_udp_reader_expires_incomplete_frame_while_sender_is_idle():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    source = UdpFrameSource("127.0.0.1", port)
    source.reassembler.timeout_ns = 10_000_000
    source.open()
    packets = encode_jpeg_frame(9, 12, 12, 1, b"\xff\xd8" + bytes(1396) + b"\xff\xd9")
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto(packets[0], source.address)
            assert source.read(timeout_s=0.15) is None
        assert source.reassembler.expired_frames == 1
        assert source.reassembler.in_flight_bytes == 0
    finally:
        source.close()
