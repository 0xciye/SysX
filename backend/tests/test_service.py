from __future__ import annotations

import asyncio
import json
import socket
import struct
import time

import cv2
import numpy as np
import pytest
import websockets

import receiver_backend.service as service_module
from receiver_backend.service import PIXEL_COLOR_PROFILES, ReceiverService, _next_display_deadline
from receiver_backend.config import ModelDefinition
from receiver_backend.strategies import PixelColorRule, PixelSeekStrategy
from receiver_backend.udp import encode_jpeg_frame


def test_preview_deadline_preserves_cadence_and_skips_missed_ticks():
    assert _next_display_deadline(10.0, 0.1, 10.05) == pytest.approx(10.1)
    assert _next_display_deadline(10.0, 0.1, 10.25) == pytest.approx(10.3)


def test_source_fps_uses_recent_valid_frame_arrivals(tmp_path):
    service = ReceiverService("i" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        with service._status_lock:
            service._source_times.extend((1_000_000_000, 1_010_000_000, 1_020_000_000))
        assert service._source_fps(now_ns=1_020_000_000) == pytest.approx(100)
        assert service._source_fps(now_ns=2_020_000_001) is None
    finally:
        asyncio.run(service.close())


def test_successful_udp_port_change_clears_recovered_bind_error(tmp_path):
    service = ReceiverService("i" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        service._record_error("[Errno 48] Address already in use")

        result = service._dispatch({"version": 1, "type": "set_udp_port", "port": free_udp_port()})

        assert result["state"]["last_error"] is None
        assert result["state"]["errors"] == []
    finally:
        asyncio.run(service.close())


def test_udp_port_bind_conflict_retries_and_clears_error_after_recovery(tmp_path, monkeypatch):
    monkeypatch.setattr(service_module, "_UDP_BIND_RETRY_SECONDS", 0.03)
    occupied = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    occupied.bind(("0.0.0.0", 0))
    unavailable_port = occupied.getsockname()[1]
    original_port = free_udp_port()
    service = ReceiverService("i" * 48, tmp_path, udp_host="127.0.0.1", udp_port=original_port)
    try:
        with pytest.raises(OSError, match="Address already in use") as bind_error:
            service._dispatch({"version": 1, "type": "set_udp_port", "port": unavailable_port})
        service._record_error(str(bind_error.value))

        time.sleep(0.1)
        assert service._udp_port == original_port
        assert service.state()["last_error"] == str(bind_error.value)

        occupied.close()
        deadline = time.monotonic() + 2
        while service._udp_port != unavailable_port and time.monotonic() < deadline:
            time.sleep(0.01)

        assert service._udp_port == unavailable_port
        assert service._source.receiver.address[1] == unavailable_port
        assert service.state()["last_error"] is None
        assert str(bind_error.value) not in service.state()["errors"]
    finally:
        occupied.close()
        asyncio.run(service.close())


def test_startup_udp_bind_conflict_retries_until_port_is_available(tmp_path, monkeypatch):
    monkeypatch.setattr(service_module, "_UDP_BIND_RETRY_SECONDS", 0.03)
    occupied = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    occupied.bind(("0.0.0.0", 0))
    port = occupied.getsockname()[1]
    service = ReceiverService("i" * 48, tmp_path, udp_port=port)
    try:
        deadline = time.monotonic() + 1
        while service.state()["last_error"] is None and time.monotonic() < deadline:
            time.sleep(0.01)
        assert "Address already in use" in service.state()["last_error"]

        occupied.close()
        deadline = time.monotonic() + 2
        while (service._source.address is None or service.state()["last_error"] is not None) and time.monotonic() < deadline:
            time.sleep(0.01)

        assert service._source.address[1] == port
        assert service.state()["last_error"] is None
    finally:
        occupied.close()
        asyncio.run(service.close())


def test_changed_udp_port_receives_and_processes_frames(tmp_path):
    original_port = free_udp_port()
    service = ReceiverService("i" * 48, tmp_path, udp_host="127.0.0.1", udp_port=original_port)
    new_port = free_udp_port()
    try:
        service._dispatch({"version": 1, "type": "set_crop_resolution", "resolution": 32})
        result = service._dispatch({"version": 1, "type": "set_udp_port", "port": new_port})
        assert result["state"]["source"].endswith(f":{new_port}")
        wait_for_udp_bind(service)
        assert service._source.address[1] == new_port

        image = np.full((48, 64, 3), 120, dtype=np.uint8)
        ok, jpeg = cv2.imencode(".jpg", image)
        assert ok
        packet = encode_jpeg_frame(88, 64, 48, 5678, jpeg.tobytes())[0]
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto(packet, ("127.0.0.1", new_port))

        deadline = time.monotonic() + 2
        while service.pipeline.latest is None and time.monotonic() < deadline:
            time.sleep(0.005)
        assert service.pipeline.latest is not None
        assert service.pipeline.latest.frame.frame_id == 88
    finally:
        asyncio.run(service.close())


def test_crop_resolution_error_clears_after_valid_crop_is_applied(tmp_path):
    port = free_udp_port()
    service = ReceiverService("i" * 48, tmp_path, udp_host="127.0.0.1", udp_port=port)
    try:
        wait_for_udp_bind(service)
        image = np.full((48, 64, 3), 120, dtype=np.uint8)
        ok, jpeg = cv2.imencode(".jpg", image)
        assert ok
        packet = encode_jpeg_frame(1, 64, 48, 1, jpeg.tobytes())[0]
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto(packet, ("127.0.0.1", port))

        deadline = time.monotonic() + 2
        while service.state()["last_error"] is None and time.monotonic() < deadline:
            time.sleep(0.005)
        assert "crop resolution 640x640 does not fit" in service.state()["last_error"]

        service._dispatch({"version": 1, "type": "set_crop_resolution", "resolution": 32})
        packet = encode_jpeg_frame(2, 64, 48, 2, jpeg.tobytes())[0]
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto(packet, ("127.0.0.1", port))
        deadline = time.monotonic() + 2
        while service.pipeline.latest is None and time.monotonic() < deadline:
            time.sleep(0.005)
        assert service.pipeline.latest is not None
        assert service.state()["last_error"] is None
    finally:
        asyncio.run(service.close())


def free_udp_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def test_authenticated_loopback_service_streams_binary_preview_from_udp(tmp_path):
    async def scenario():
        udp_port = free_udp_port()
        service = ReceiverService("a" * 48, tmp_path, udp_host="127.0.0.1", udp_port=udp_port)
        server = await websockets.serve(service.handle, "127.0.0.1", 0, max_size=64 * 1024)
        ipc_port = server.sockets[0].getsockname()[1]
        uri = f"ws://127.0.0.1:{ipc_port}/receiver?token={'a' * 48}"
        try:
            async with websockets.connect(uri, origin="http://127.0.0.1:1420") as client:
                state = json.loads(await client.recv())
                assert state["status"] == "waiting"
                await client.send(json.dumps({"version": 1, "type": "set_strategy", "mode": "pixel_seek"}))
                ack = json.loads(await client.recv())
                assert ack["event"] == "ack"
                await client.send(json.dumps({"version": 1, "type": "set_crop_resolution", "resolution": 32}))
                ack = json.loads(await client.recv())
                assert ack["state"]["crop_resolution"] == 32
                await asyncio.to_thread(wait_for_udp_bind, service)

                image = np.full((48, 64, 3), 120, dtype=np.uint8)
                ok, jpeg = cv2.imencode(".jpg", image)
                assert ok
                packet = encode_jpeg_frame(77, 64, 48, 1234, jpeg.tobytes())[0]
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
                    sender.sendto(packet, ("127.0.0.1", udp_port))

                payload = await asyncio.wait_for(client.recv(), timeout=3)
                assert isinstance(payload, bytes)
                metadata_size = struct.unpack_from("!I", payload)[0]
                metadata = json.loads(payload[4:4 + metadata_size])
                preview = cv2.imdecode(np.frombuffer(payload[4 + metadata_size:], dtype=np.uint8), cv2.IMREAD_COLOR)
                assert metadata["event"] == "frame"
                assert metadata["frame_id"] == 77
                assert metadata["detection"]["source_width"] == 64
                assert metadata["detection"]["inference_device"] == "cpu"
                assert "source_fps" in metadata
                assert metadata["crop_x"] == 16
                assert metadata["crop_y"] == 8
                assert metadata["detection"]["processed_width"] == 32
                assert preview.shape[:2] == (32, 32)
        finally:
            server.close()
            await server.wait_closed()
            await service.close()

    asyncio.run(scenario())


def test_service_rejects_bad_loopback_token(tmp_path):
    async def scenario():
        service = ReceiverService("b" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
        server = await websockets.serve(service.handle, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        try:
            async with websockets.connect(f"ws://127.0.0.1:{port}/receiver?token={'c' * 48}") as client:
                with pytest.raises(websockets.ConnectionClosed):
                    await client.recv()
        finally:
            server.close()
            await server.wait_closed()
            await service.close()

    asyncio.run(scenario())


def test_pixel_color_selects_only_its_code_profile(tmp_path, monkeypatch):
    red_rule = PixelColorRule(1, "Red", (((0, 100, 100), (10, 255, 255)),))
    monkeypatch.setitem(PIXEL_COLOR_PROFILES, "red", (red_rule,))
    service = ReceiverService("d" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        assert service.state()["pixel_profile_configured"] is True
        response = service._dispatch({"version": 1, "type": "set_pixel_color", "color": "red"})
        assert response["state"]["pixel_color"] == "red"
        assert response["state"]["pixel_profile_configured"] is True
        assert service.pipeline._pixel_rules == (red_rule,)

        service._dispatch({"version": 1, "type": "set_pixel_color", "color": "purple"})
        assert service.pipeline._pixel_rules == PIXEL_COLOR_PROFILES["purple"]
    finally:
        asyncio.run(service.close())


def test_pixel_color_rejects_unknown_values(tmp_path):
    service = ReceiverService("e" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        with pytest.raises(ValueError, match="yellow, red, or purple"):
            service._dispatch({"version": 1, "type": "set_pixel_color", "color": ["red"]})
    finally:
        asyncio.run(service.close())


def test_display_config_updates_fov_and_preview_scale(tmp_path):
    service = ReceiverService("f" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        response = service._dispatch({"version": 1, "type": "set_display_config", "fov_degrees": 120, "preview_scale": "Pixel perfect"})
        assert response["state"]["fov_degrees"] == 120
        assert service.pipeline._fov_degrees == 120
        assert response["state"]["preview_scale"] == "Pixel perfect"
        with pytest.raises(ValueError, match="1 to 360"):
            service._dispatch({"version": 1, "type": "set_display_config", "fov_degrees": 361, "preview_scale": "Smooth"})
    finally:
        asyncio.run(service.close())


def test_service_exports_csv_and_flushes_pending_rows(tmp_path):
    service = ReceiverService("h" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        service.store.submit(PixelSeekStrategy(()).process(np.zeros((8, 8, 3), dtype=np.uint8), frame_id=29))
        path = tmp_path / "current-session.csv"
        response = service._dispatch({"version": 1, "type": "export_csv", "path": str(path)})
        assert response == {"event": "exported", "path": str(path)}
        assert "frame_id,timestamp_ns,mode" in path.read_text()
        assert json.loads(service.store.jsonl_path.read_text().splitlines()[0])["frame_id"] == 29
    finally:
        asyncio.run(service.close())


def test_removing_inactive_model_keeps_active_detection_strategy(tmp_path):
    service = ReceiverService("f" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        for model_id in ("active", "unused"):
            service.registry.add(ModelDefinition(model_id, model_id, f"{model_id}.pt", 640, 640))
        service.registry.activate("active")
        service._set_mode("object_detection")

        result = service._dispatch({"version": 1, "type": "remove_model", "id": "unused"})
        assert result["active_model_id"] == "active"
        assert service._mode.value == "object_detection"
    finally:
        asyncio.run(service.close())


def test_removing_active_model_falls_back_to_pixel_seek(tmp_path):
    service = ReceiverService("g" * 48, tmp_path, udp_host="127.0.0.1", udp_port=free_udp_port())
    try:
        service.registry.add(ModelDefinition("active", "Active", "active.pt", 640, 640))
        service.registry.activate("active")
        service._set_mode("object_detection")

        result = service._dispatch({"version": 1, "type": "remove_model", "id": "active"})
        assert result["active_model_id"] is None
        assert service._mode.value == "pixel_seek"
    finally:
        asyncio.run(service.close())


def wait_for_udp_bind(service: ReceiverService) -> None:
    import time

    deadline = time.monotonic() + 2
    while service._source.address is None and time.monotonic() < deadline:
        time.sleep(0.005)
    assert service._source.address is not None
