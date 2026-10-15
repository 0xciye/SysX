from __future__ import annotations

import asyncio
import json
import math
import secrets
import struct
import threading
import time
from dataclasses import asdict
from datetime import datetime, timezone
from pathlib import Path
from collections.abc import Callable
from collections import deque
from urllib.parse import parse_qs, urlsplit

import cv2
import websockets

from .config import ModelDefinition, ModelRegistry
from .pipeline import LatestFramePipeline
from .sources import FrameSource, UdpDecodedFrameSource
from .storage import DetectionDataStore
from .strategies import PixelColorRule, StrategyName
from .web_config import ConfigWebServer, load_settings


_BINARY_LENGTH = struct.Struct("!I")
_ALLOWED_ORIGINS = {None, "tauri://localhost", "http://tauri.localhost", "http://127.0.0.1:1420"}
_UDP_BIND_RETRY_SECONDS = 3.0


def _next_display_deadline(deadline: float, interval: float, now: float) -> float:
    """Advance one cadence tick and skip missed ticks instead of accumulating delay."""
    next_deadline = deadline + interval
    if next_deadline <= now:
        missed_ticks = math.floor((now - next_deadline) / interval) + 1
        next_deadline += missed_ticks * interval
    return next_deadline


# OpenCV HSV ranges (H: 0..179, S/V: 0..255), applied after BGR -> RGB -> HSV.
PIXEL_COLOR_PROFILES: dict[str, tuple[PixelColorRule, ...]] = {
    "yellow": (PixelColorRule(1, "Yellow", (((20, 157, 252), (40, 255, 255)),)),),
    "red": (PixelColorRule(2, "Red", (((0, 50, 180), (10, 255, 255)),)),),
    "purple": (PixelColorRule(3, "Purple", (((140, 60, 240), (160, 255, 255)),)),),
}


class ReceiverService:
    """Authenticated loopback control and binary preview transport for the desktop UI."""

    def __init__(self, token: str, data_dir: str | Path, *, display_fps: int = 60, udp_host: str = "0.0.0.0", udp_port: int = 4200):
        if len(token) < 32:
            raise ValueError("IPC token must contain at least 32 characters")
        if not 1 <= display_fps <= 120:
            raise ValueError("display FPS must be between 1 and 120")
        self.token = token
        self.allowed_origins = set(_ALLOWED_ORIGINS)
        self.data_dir = Path(data_dir).expanduser()
        self.data_dir.mkdir(parents=True, exist_ok=True)
        settings_path = self.data_dir / "app-settings.json"
        app_settings = load_settings(settings_path)
        self.display_fps = display_fps
        self.registry = ModelRegistry(self.data_dir / "models.yaml")
        self._pixel_color = app_settings["pixelColor"]
        self._mode = StrategyName(app_settings["strategy"])
        if self._mode is StrategyName.OBJECT_DETECTION and self._active_model() is None:
            self._mode = StrategyName.PIXEL_SEEK
        self._fov_degrees = app_settings["fovDegrees"]
        self._preview_scale = app_settings["scale"]
        self._crop_resolution = app_settings["cropResolution"]
        self._udp_host = udp_host
        # Honor explicit ports supplied by local callers when there is no saved
        # application config. The installed app's default remains 4200.
        self._udp_port = int(app_settings["port"]) if settings_path.is_file() else udp_port
        self._pending_udp_port: int | None = None
        self._pending_udp_error: str | None = None
        self._next_udp_retry_at = 0.0
        self._session_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self._errors: list[str] = []
        self._status_lock = threading.Lock()
        self._source_times: deque[int] = deque(maxlen=720)
        self._source: FrameSource | None = None
        self._source_kind = "udp"
        self._source_name = ""
        self._reader_stop: threading.Event | None = None
        self._reader_thread: threading.Thread | None = None
        session_dir = self.data_dir / "sessions" / self._session_id
        self.store = DetectionDataStore(session_dir, on_error=self._record_error)
        self.pipeline = LatestFramePipeline(crop_resolution=self._crop_resolution, store=self.store,
                                            fov_degrees=self._fov_degrees,
                                            on_error=self._record_error, on_recovered=self._clear_error)
        self._replace_source(UdpDecodedFrameSource(udp_host, self._udp_port), "udp", f"UDP · {udp_host}:{self._udp_port}")

    def _record_error(self, message: str) -> None:
        with self._status_lock:
            self._errors.append(message)
            del self._errors[:-20]

    def _clear_error(self, message: str | None) -> None:
        if message is None:
            return
        with self._status_lock:
            self._errors = [error for error in self._errors if error != message]

    def _active_model(self) -> ModelDefinition | None:
        model_id = self.registry.active_model_id
        return self.registry.models.get(model_id) if model_id else None

    def _record_source_frame(self) -> None:
        with self._status_lock:
            self._source_times.append(time.perf_counter_ns())

    def _source_fps(self, now_ns: int | None = None) -> float | None:
        now = time.perf_counter_ns() if now_ns is None else now_ns
        with self._status_lock:
            recent = [stamp for stamp in self._source_times if 0 <= now - stamp <= 1_000_000_000]
        if len(recent) < 2 or recent[-1] <= recent[0]:
            return None
        return (len(recent) - 1) * 1_000_000_000 / (recent[-1] - recent[0])

    def state(self) -> dict[str, object]:
        latest = self.pipeline.latest
        stats = self.pipeline.stats()
        with self._status_lock:
            errors = list(self._errors)
            source_kind, source_name = self._source_kind, self._source_name
            source = self._source
        is_live = latest is not None and (
            source_kind != "udp" or time.perf_counter_ns() - latest.published_ns < 1_000_000_000
        )
        transport = source.receiver.reassembler if isinstance(source, UdpDecodedFrameSource) else None
        model = self._active_model() if self._mode is StrategyName.OBJECT_DETECTION else None
        return {
            "event": "state",
            "version": 1,
            "status": "streaming" if is_live else "waiting",
            "input": source_kind,
            "source": source_name,
            "mode": self._mode.value,
            "fov_degrees": self._fov_degrees,
            "preview_scale": self._preview_scale,
            "crop_resolution": self._crop_resolution,
            "pixel_color": self._pixel_color,
            "pixel_profile_configured": bool(PIXEL_COLOR_PROFILES[self._pixel_color]),
            "active_model_id": self.registry.active_model_id,
            "expected_source_resolution": [model.input_width, model.input_height] if model else None,
            "session_id": self._session_id,
            "processed_frames": stats.processed_frames,
            "dropped_input_frames": stats.dropped_input_frames,
            "rejected_frames": stats.rejected_frames,
            "resolution_mismatches": stats.resolution_mismatches,
            "transport_dropped_frames": (transport.dropped_frames + transport.expired_frames) if transport else 0,
            "invalid_packets": transport.invalid_packets if transport else 0,
            "processing_fps": stats.processing_fps,
            "source_fps": self._source_fps(),
            "last_error": stats.last_error or (errors[-1] if errors else None),
            "errors": errors,
        }

    def _replace_source(self, source: FrameSource, kind: str, name: str) -> None:
        old_stop, old_thread, old_source = self._reader_stop, self._reader_thread, self._source
        if old_stop is not None:
            old_stop.set()
        if old_thread is not None:
            old_thread.join(timeout=2)
            if old_thread.is_alive():
                raise RuntimeError("previous frame source did not shut down")
        if old_source is not None:
            old_source.close()
        model = self._active_model() if self._mode is StrategyName.OBJECT_DETECTION else None
        self.pipeline.configure(strategy=self._mode, pixel_rules=PIXEL_COLOR_PROFILES[self._pixel_color], model=model,
                                crop_resolution=self._crop_resolution, fov_degrees=self._fov_degrees)
        self._source = source
        self._source_kind = kind
        self._source_name = name
        stop = threading.Event()
        self._reader_stop = stop
        self._reader_thread = threading.Thread(target=self._read_source, args=(source, stop), name="frame-source", daemon=True)
        self._reader_thread.start()

    def _read_source(self, source: FrameSource, stop: threading.Event) -> None:
        source_error: str | None = None
        while not stop.is_set():
            source = self._retry_udp_port(source, stop)
            try:
                frame = source.read()
            except Exception as error:
                if stop.is_set():
                    break
                message = str(error)
                if message != source_error:
                    self._clear_error(source_error)
                    self._record_error(message)
                    source_error = message
                source.close()
                if stop.wait(_UDP_BIND_RETRY_SECONDS):
                    break
                continue
            if source_error is not None:
                self._clear_error(source_error)
                source_error = None
            if frame is not None:
                self._record_source_frame()
                self.pipeline.submit(frame)

    def _retry_udp_port(self, current_source: FrameSource, stop: threading.Event) -> FrameSource:
        now = time.monotonic()
        with self._status_lock:
            port = self._pending_udp_port
            if port is None or now < self._next_udp_retry_at or stop.is_set():
                return current_source
            self._next_udp_retry_at = now + _UDP_BIND_RETRY_SECONDS

        candidate = UdpDecodedFrameSource("0.0.0.0", port)
        try:
            candidate.receiver.open()
        except OSError as error:
            message = str(error)
            with self._status_lock:
                if self._pending_udp_port == port and self._pending_udp_error != message:
                    previous = self._pending_udp_error
                    self._pending_udp_error = message
                    self._errors = [item for item in self._errors if item != previous]
                    self._errors.append(message)
                    del self._errors[:-20]
            candidate.close()
            return current_source

        with self._status_lock:
            if self._pending_udp_port != port or stop.is_set():
                candidate.close()
                return current_source
            previous_error = self._pending_udp_error
            self._source = candidate
            self._source_name = f"UDP · 0.0.0.0:{port}"
            self._udp_host, self._udp_port = "0.0.0.0", port
            self._pending_udp_port = None
            self._pending_udp_error = None
            self._next_udp_retry_at = 0.0
            self._errors = [item for item in self._errors if item != previous_error]
        current_source.close()
        return candidate

    def _configure_pipeline(self, *, strategy: StrategyName | None = None) -> None:
        latest = self.pipeline.latest
        selected = strategy or self._mode
        model = self._active_model() if selected is StrategyName.OBJECT_DETECTION else None
        self.pipeline.configure(strategy=selected, pixel_rules=PIXEL_COLOR_PROFILES[self._pixel_color], model=model,
                                crop_resolution=self._crop_resolution, fov_degrees=self._fov_degrees)

    def _set_mode(self, value: object) -> None:
        mode = StrategyName(str(value))
        self._configure_pipeline(strategy=mode)
        self._mode = mode

    def _dispatch(self, command: object) -> dict[str, object]:
        if not isinstance(command, dict) or command.get("version") != 1:
            raise ValueError("command must be a version 1 object")
        kind = command.get("type")
        if kind == "state":
            return self.state()
        if kind == "set_udp_port":
            port = int(command.get("port", 0))
            if not 1 <= port <= 65535:
                raise ValueError("UDP port must be between 1 and 65535")
            if port == self._udp_port:
                with self._status_lock:
                    pending_error = self._pending_udp_error
                    self._pending_udp_port = None
                    self._pending_udp_error = None
                    self._next_udp_retry_at = 0.0
                self._clear_error(pending_error)
                return {"event": "ack", "type": kind, "state": self.state()}
            source = UdpDecodedFrameSource("0.0.0.0", port)
            try:
                source.receiver.open()
            except OSError as error:
                source.close()
                with self._status_lock:
                    self._pending_udp_port = port
                    self._pending_udp_error = str(error)
                    self._next_udp_retry_at = time.monotonic() + _UDP_BIND_RETRY_SECONDS
                raise
            self._replace_source(source, "udp", f"UDP · 0.0.0.0:{port}")
            with self._status_lock:
                self._udp_host, self._udp_port = "0.0.0.0", port
                self._pending_udp_port = None
                self._pending_udp_error = None
                self._next_udp_retry_at = 0.0
            with self._status_lock:
                self._errors.clear()
            return {"event": "ack", "type": kind, "state": self.state()}
        if kind == "set_pixel_color":
            color = command.get("color")
            if not isinstance(color, str) or color not in PIXEL_COLOR_PROFILES:
                raise ValueError("pixel color must be yellow, red, or purple")
            self._pixel_color = color
            self._configure_pipeline()
            return {"event": "ack", "type": kind, "state": self.state()}
        if kind == "set_display_config":
            degrees = command.get("fov_degrees", self._fov_degrees)
            scale = command.get("preview_scale", self._preview_scale)
            if not isinstance(degrees, int) or isinstance(degrees, bool) or not 1 <= degrees <= 360:
                raise ValueError("FOV must be an integer from 1 to 360 degrees")
            if scale not in {"Smooth", "Pixel perfect"}:
                raise ValueError("preview scale must be Smooth or Pixel perfect")
            self._fov_degrees = degrees
            self._preview_scale = scale
            self._configure_pipeline()
            return {"event": "ack", "type": kind, "state": self.state()}
        if kind == "set_crop_resolution":
            resolution = command.get("resolution")
            if not isinstance(resolution, int) or isinstance(resolution, bool) or not 1 <= resolution <= 8192:
                raise ValueError("crop resolution must be an integer from 1 to 8192")
            self._crop_resolution = resolution
            self.pipeline.configure(strategy=self._mode, pixel_rules=PIXEL_COLOR_PROFILES[self._pixel_color],
                                    model=self._active_model() if self._mode is StrategyName.OBJECT_DETECTION else None,
                                    crop_resolution=resolution, fov_degrees=self._fov_degrees)
            return {"event": "ack", "type": kind, "state": self.state()}
        if kind == "export_csv":
            path = command.get("path")
            if not isinstance(path, str) or not path.strip():
                raise ValueError("a CSV destination path is required")
            self.store.export_csv(path)
            return {"event": "exported", "path": str(Path(path).expanduser())}
        if kind == "set_strategy":
            self._set_mode(command.get("mode"))
            return {"event": "ack", "type": kind, "state": self.state()}
        if kind == "list_models":
            return {"event": "models", "models": [asdict(model) for model in self.registry.models.values()], "active_model_id": self.registry.active_model_id}
        if kind == "add_model":
            model = ModelDefinition(
                id=str(command.get("id", "")), name=str(command.get("name", "")), path=str(command.get("path", "")),
                input_width=int(command.get("input_width", 0)), input_height=int(command.get("input_height", 0)),
                confidence=float(command.get("confidence", 0.5)), iou=float(command.get("iou", 0.45)),
                device=str(command.get("device", "auto")), enabled=bool(command.get("enabled", True)),
                class_names=command.get("class_names"),
            )
            self.registry.add(model)
            return {"event": "models", "models": [asdict(value) for value in self.registry.models.values()], "active_model_id": self.registry.active_model_id}
        if kind == "update_model":
            model_id = str(command.get("id", ""))
            current = self.registry.models.get(model_id)
            if current is None:
                raise ValueError("model does not exist")
            model = ModelDefinition(
                id=current.id,
                name=str(command.get("name", current.name)),
                path=current.path,
                input_width=int(command.get("input_width", current.input_width)),
                input_height=int(command.get("input_height", current.input_height)),
                confidence=float(command.get("confidence", current.confidence)),
                iou=float(command.get("iou", current.iou)),
                device=str(command.get("device", current.device)),
                enabled=bool(command.get("enabled", current.enabled)),
                class_names=current.class_names,
            )
            self.registry.update(model)
            if self.registry.active_model_id == model_id and self._mode is StrategyName.OBJECT_DETECTION:
                self._configure_pipeline()
            return {"event": "models", "models": [asdict(value) for value in self.registry.models.values()], "active_model_id": self.registry.active_model_id}
        if kind == "activate_model":
            model_id = str(command.get("id", ""))
            model = self.registry.models.get(model_id)
            if model is None or not model.enabled:
                raise ValueError("selected model is missing or disabled")
            self.registry.activate(model_id)
            if self._mode is StrategyName.OBJECT_DETECTION:
                self._set_mode(self._mode.value)
            return {"event": "models", "models": [asdict(value) for value in self.registry.models.values()], "active_model_id": self.registry.active_model_id}
        if kind == "remove_model":
            model_id = str(command.get("id", ""))
            was_active = self.registry.active_model_id == model_id
            self.registry.remove(model_id)
            if was_active and self._mode is StrategyName.OBJECT_DETECTION:
                self._set_mode(StrategyName.PIXEL_SEEK.value)
            return {"event": "models", "models": [asdict(value) for value in self.registry.models.values()], "active_model_id": self.registry.active_model_id}
        raise ValueError(f"unsupported command type: {kind}")

    async def handle(self, websocket) -> None:
        request_path = urlsplit(websocket.request.path)
        query_token = parse_qs(request_path.query).get("token", [""])[0]
        origin = websocket.request.headers.get("Origin")
        if request_path.path != "/receiver" or origin not in self.allowed_origins or not secrets.compare_digest(query_token, self.token):
            await websocket.close(code=4401, reason="unauthorized")
            return
        await websocket.send(json.dumps(self.state(), separators=(",", ":")))
        sender = asyncio.create_task(self._stream(websocket))
        try:
            async for message in websocket:
                if not isinstance(message, str) or len(message) > 64 * 1024:
                    await websocket.send(json.dumps({"event": "error", "message": "commands must be text messages under 64 KiB"}))
                    continue
                try:
                    command = json.loads(message)
                    if isinstance(command, dict) and command.get("type") == "export_csv":
                        response = await asyncio.to_thread(self._dispatch, command)
                    else:
                        response = self._dispatch(command)
                except Exception as error:
                    response = {"event": "error", "message": str(error)}
                    self._record_error(str(error))
                await websocket.send(json.dumps(response, separators=(",", ":"), allow_nan=False))
        finally:
            sender.cancel()
            try:
                await sender
            except asyncio.CancelledError:
                pass

    async def _stream(self, websocket) -> None:
        interval = 1.0 / self.display_fps
        next_tick = time.monotonic()
        sent_published_ns: int | None = None
        last_status = time.monotonic()
        while True:
            processed = self.pipeline.latest
            now = time.monotonic()
            if processed is not None and processed.published_ns != sent_published_ns:
                ok, jpeg = cv2.imencode(".jpg", processed.frame.image, [cv2.IMWRITE_JPEG_QUALITY, 88])
                if ok:
                    stats = self.pipeline.stats()
                    metadata = json.dumps({
                        "event": "frame",
                        "frame_id": processed.frame.frame_id,
                        "source": processed.frame.source,
                        "source_width": processed.result.source_width,
                        "source_height": processed.result.source_height,
                        "crop_x": processed.crop_x,
                        "crop_y": processed.crop_y,
                        "mode": processed.result.mode,
                        "model_id": processed.result.model_id,
                        "processing_fps": stats.processing_fps,
                        "source_fps": self._source_fps(),
                        "dropped_input_frames": stats.dropped_input_frames,
                        "rejected_frames": stats.rejected_frames,
                        "resolution_mismatches": stats.resolution_mismatches,
                        "transport_dropped_frames": self.state()["transport_dropped_frames"],
                        "detection": processed.result.to_dict(),
                    }, separators=(",", ":"), allow_nan=False).encode("utf-8")
                    await websocket.send(_BINARY_LENGTH.pack(len(metadata)) + metadata + jpeg.tobytes())
                    sent_published_ns = processed.published_ns
            elif now - last_status >= 1.0:
                await websocket.send(json.dumps(self.state(), separators=(",", ":")))
                last_status = now
            next_tick = _next_display_deadline(next_tick, interval, time.monotonic())
            await asyncio.sleep(max(0.0, next_tick - time.monotonic()))

    async def close(self) -> None:
        stop, thread = self._reader_stop, self._reader_thread
        if stop is not None:
            stop.set()
        if thread is not None:
            await asyncio.to_thread(thread.join, 3)
            if thread.is_alive():
                self._record_error("Frame source did not stop within three seconds")
        if self._source is not None:
            self._source.close()
        self.pipeline.close()
        self.store.close(timeout=3)


async def serve(token: str, data_dir: str | Path, host: str = "127.0.0.1", port: int = 0,
                on_ready: Callable[[int, int | None, str | None], None] | None = None,
                web_root: str | Path | None = None, web_port: int = 4201) -> None:
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise ValueError("desktop IPC server must bind to loopback only")
    service = ReceiverService(token, data_dir)
    config_server: ConfigWebServer | None = None
    try:
        config_origins = {f"http://127.0.0.1:{port}" for port in range(web_port, web_port + 10)}
        server = await websockets.serve(
            service.handle, host, port, max_size=64 * 1024, ping_interval=20, ping_timeout=20,
            origins=[*_ALLOWED_ORIGINS, *config_origins],
        )
        try:
            actual_port = server.sockets[0].getsockname()[1]
            web_error = None
            if web_root is not None:
                try:
                    config_server = ConfigWebServer(web_root, token, actual_port, data_dir, web_port)
                    config_server.start()
                    service.allowed_origins.add(f"http://127.0.0.1:{config_server.port}")
                except Exception as error:
                    web_error = f"Local configuration page could not start: {error}"
            if on_ready is not None:
                on_ready(actual_port, config_server.port if config_server else None, web_error)
            await server.wait_closed()
        finally:
            server.close()
            if config_server is not None:
                config_server.close()
    finally:
        await service.close()
