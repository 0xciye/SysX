import asyncio
import json
import socket
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest
import websockets

from receiver_backend.service import serve
from receiver_backend.web_config import ConfigWebServer, load_settings
from receiver_backend.web_config import normalize_settings


@pytest.fixture
def config_server(tmp_path):
    root = tmp_path / "web"
    root.mkdir()
    (root / "config.html").write_text("<main>SysX configuration</main>")
    server = ConfigWebServer(root, "a" * 64, 41234, tmp_path / "data", port=0)
    server.start()
    try:
        yield server
    finally:
        server.close()


def test_local_config_server_serves_page_and_backend_session(config_server):
    base = f"http://127.0.0.1:{config_server.port}"
    with urlopen(f"{base}/config.html") as response:
        assert b"SysX configuration" in response.read()
    with urlopen(f"{base}/api/session") as response:
        assert json.load(response) == {"host": "127.0.0.1", "port": 41234, "token": "a" * 64}


def test_model_upload_is_stored_under_app_data(config_server, tmp_path):
    request = Request(
        f"http://127.0.0.1:{config_server.port}/api/upload-model",
        data=b"model-bytes",
        headers={"X-Filename": "detector.pt"},
        method="POST",
    )
    with urlopen(request) as response:
        path = json.load(response)["path"]
    assert Path(path).read_bytes() == b"model-bytes"
    assert str(path).startswith(str(tmp_path / "data" / "models"))


def test_saved_settings_are_persisted_and_read_back(config_server, tmp_path):
    settings = {
        "strategy": "pixel_seek", "fovDegrees": 144, "scale": "Pixel perfect", "showPreview": False,
        "cropResolution": 256,
        "port": "4202", "pixelColor": "purple", "activeModelId": None,
        "modelWidth": "640", "modelHeight": "640", "modelConfidence": "0.5", "modelIou": "0.45",
        "modelBackend": "onnxruntime", "modelPrecision": "fp16", "computeTarget": "cpu",
    }
    request = Request(
        f"http://127.0.0.1:{config_server.port}/api/settings",
        data=json.dumps(settings).encode(),
        headers={"Content-Type": "application/json"},
        method="PUT",
    )
    with urlopen(request) as response:
        assert json.load(response) == {"saved": True}
    assert load_settings(tmp_path / "data" / "app-settings.json")["fovDegrees"] == 144
    with urlopen(f"http://127.0.0.1:{config_server.port}/api/settings") as response:
        stored = json.load(response)
    assert stored["showPreview"] is True
    assert stored["cropResolution"] == 256
    assert stored["scale"] == "Pixel perfect"


def test_local_config_server_rejects_cross_origin_api_request(config_server):
    request = Request(
        f"http://127.0.0.1:{config_server.port}/api/session",
        headers={"Origin": "https://example.com"},
    )
    with pytest.raises(HTTPError) as error:
        urlopen(request)
    assert error.value.code == 403


@pytest.mark.parametrize("resolution", [0, -1, 8193, True, 256.0, "256"])
def test_crop_resolution_setting_requires_integer_pixels(resolution):
    with pytest.raises(ValueError, match="crop resolution"):
        normalize_settings({"cropResolution": resolution})


def test_config_server_uses_next_loopback_port_when_default_is_busy(tmp_path):
    root = tmp_path / "web"
    root.mkdir()
    (root / "config.html").write_text("<main>SysX configuration</main>")
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as occupied:
        occupied.bind(("127.0.0.1", 0))
        first_port = occupied.getsockname()[1]
        occupied.listen()
        server = ConfigWebServer(root, "b" * 64, 41234, tmp_path / "data", port=first_port)
        try:
            assert server.port != first_port
            assert first_port < server.port <= first_port + 9
        finally:
            server.start()
            server.close()


def test_browser_origin_can_connect_to_authenticated_backend(tmp_path):
    async def scenario():
        root = tmp_path / "web"
        root.mkdir()
        (root / "config.html").write_text("<main>SysX configuration</main>")
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            config_port = probe.getsockname()[1]
        ready = []
        token = "c" * 64
        server_task = asyncio.create_task(
            serve(token, tmp_path / "data", host="127.0.0.1", port=0,
                  on_ready=lambda ipc_port, browser_port, error: ready.append((ipc_port, browser_port, error)),
                  web_root=root, web_port=config_port)
        )
        try:
            deadline = asyncio.get_running_loop().time() + 3
            while not ready and asyncio.get_running_loop().time() < deadline:
                await asyncio.sleep(0.01)
            assert ready and ready[0][2] is None
            ipc_port, browser_port, _ = ready[0]
            assert browser_port is not None
            uri = f"ws://127.0.0.1:{ipc_port}/receiver?token={token}"
            async with websockets.connect(uri, origin=f"http://127.0.0.1:{browser_port}") as client:
                state = json.loads(await client.recv())
                assert state["event"] == "state"
        finally:
            server_task.cancel()
            await asyncio.gather(server_task, return_exceptions=True)

    asyncio.run(scenario())
