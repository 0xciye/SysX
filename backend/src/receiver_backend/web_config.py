from __future__ import annotations

import json
import os
import tempfile
import threading
import uuid
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote, urlsplit


_MAX_MODEL_BYTES = 2 * 1024 * 1024 * 1024
_DEFAULT_SETTINGS = {
    "strategy": "pixel_seek", "fovDegrees": 36, "scale": "Smooth", "showPreview": True,
    "cropResolution": 640,
    "port": "4200", "pixelColor": "yellow", "activeModelId": None,
    "modelWidth": "640", "modelHeight": "640", "modelConfidence": "0.5", "modelIou": "0.45",
    "modelBackend": "onnxruntime", "modelPrecision": "fp16", "computeTarget": "cpu",
}


def normalize_settings(value: object) -> dict:
    if not isinstance(value, dict):
        raise ValueError("settings must be an object")
    settings = {**_DEFAULT_SETTINGS, **value, "showPreview": True}
    if settings["strategy"] not in {"pixel_seek", "object_detection"}:
        raise ValueError("invalid detection strategy")
    if not isinstance(settings["fovDegrees"], int) or isinstance(settings["fovDegrees"], bool) or not 1 <= settings["fovDegrees"] <= 360:
        raise ValueError("FOV must be from 1 to 360 degrees")
    if settings["scale"] not in {"Smooth", "Pixel perfect"}:
        raise ValueError("invalid preview scale")
    if not isinstance(settings["port"], str) or not settings["port"].isdigit() or not 1 <= int(settings["port"]) <= 65535:
        raise ValueError("UDP port must be from 1 to 65535")
    if settings["pixelColor"] not in {"yellow", "red", "purple"}:
        raise ValueError("invalid Pixel Seek color")
    if not isinstance(settings["cropResolution"], int) or isinstance(settings["cropResolution"], bool) or not 1 <= settings["cropResolution"] <= 8192:
        raise ValueError("crop resolution must be an integer from 1 to 8192")
    if settings["activeModelId"] is not None and not isinstance(settings["activeModelId"], str):
        raise ValueError("active model id must be text or null")
    for name in ("modelWidth", "modelHeight", "modelConfidence", "modelIou", "modelBackend", "modelPrecision", "computeTarget"):
        if not isinstance(settings[name], str):
            raise ValueError(f"{name} must be text")
    return settings


def load_settings(path: str | Path) -> dict:
    try:
        return normalize_settings(json.loads(Path(path).read_text(encoding="utf-8")))
    except (OSError, ValueError, TypeError, json.JSONDecodeError):
        return dict(_DEFAULT_SETTINGS)


class _ConfigHandler(SimpleHTTPRequestHandler):
    def __init__(self, *args, directory: str, token: str, backend_port: int, data_dir: Path, **kwargs):
        self.token = token
        self.backend_port = backend_port
        self.data_dir = data_dir
        super().__init__(*args, directory=directory, **kwargs)

    def _valid_local_request(self) -> bool:
        host = self.headers.get("Host", "").split(":", 1)[0]
        origin = self.headers.get("Origin")
        allowed_origin = f"http://127.0.0.1:{self.server.server_port}"
        return host in {"127.0.0.1", "localhost"} and origin in {None, allowed_origin}

    def do_GET(self) -> None:
        if not self._valid_local_request():
            self.send_error(403)
            return
        if urlsplit(self.path).path == "/api/session":
            body = json.dumps({"host": "127.0.0.1", "port": self.backend_port, "token": self.token}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if urlsplit(self.path).path == "/api/settings":
            body = json.dumps(load_settings(self.data_dir / "app-settings.json")).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def do_POST(self) -> None:
        if urlsplit(self.path).path != "/api/upload-model" or not self._valid_local_request():
            self.send_error(403)
            return
        filename = unquote(self.headers.get("X-Filename", ""))
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400, "Invalid content length")
            return
        if not filename.lower().endswith(".pt") or length <= 0 or length > _MAX_MODEL_BYTES:
            self.send_error(400, "Upload a non-empty .pt model smaller than 2 GiB")
            return

        model_dir = self.data_dir / "models"
        model_dir.mkdir(parents=True, exist_ok=True)
        model_path = model_dir / f"{uuid.uuid4().hex}.pt"
        temporary_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(dir=model_dir, prefix=".upload-", delete=False) as output:
                temporary_path = Path(output.name)
                remaining = length
                while remaining:
                    chunk = self.rfile.read(min(1024 * 1024, remaining))
                    if not chunk:
                        raise OSError("Model upload ended before all bytes were received")
                    output.write(chunk)
                    remaining -= len(chunk)
            os.replace(temporary_path, model_path)
        except OSError as error:
            if temporary_path is not None:
                temporary_path.unlink(missing_ok=True)
            self.send_error(500, str(error))
            return

        body = json.dumps({"path": str(model_path)}).encode()
        self.send_response(201)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self) -> None:
        if urlsplit(self.path).path != "/api/settings" or not self._valid_local_request():
            self.send_error(403)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 64 * 1024:
                raise ValueError("settings payload must be at most 64 KiB")
            value = json.loads(self.rfile.read(length))
            settings = normalize_settings(value)
            self.data_dir.mkdir(parents=True, exist_ok=True)
            destination = self.data_dir / "app-settings.json"
            with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=self.data_dir, prefix=".settings-", delete=False) as output:
                temporary = Path(output.name)
                json.dump(settings, output, separators=(",", ":"))
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, destination)
        except (OSError, ValueError, TypeError, json.JSONDecodeError) as error:
            try:
                temporary.unlink(missing_ok=True)
            except UnboundLocalError:
                pass
            self.send_error(400, str(error))
            return
        body = json.dumps({"saved": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def end_headers(self) -> None:
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        super().end_headers()

    def log_message(self, format: str, *args) -> None:
        return


class ConfigWebServer:
    def __init__(self, root: str | Path, token: str, backend_port: int, data_dir: str | Path, port: int = 4201):
        self.root = Path(root).resolve()
        if not (self.root / "config.html").is_file():
            raise FileNotFoundError(f"Config page is missing from {self.root}")
        self.backend_port = backend_port
        handler = lambda *args, **kwargs: _ConfigHandler(
            *args, directory=str(self.root), token=token, backend_port=self.backend_port,
            data_dir=Path(data_dir), **kwargs,
        )
        candidates = (0,) if port == 0 else range(port, min(port + 10, 65536))
        last_error: OSError | None = None
        for candidate in candidates:
            try:
                self.server = ThreadingHTTPServer(("127.0.0.1", candidate), handler)
                break
            except OSError as error:
                last_error = error
        else:
            raise OSError(f"No local configuration port is available from {port} to {port + 9}") from last_error
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, name="config-web", daemon=True)

    @property
    def port(self) -> int:
        return int(self.server.server_port)

    def start(self) -> None:
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
