"""Windows sender build smoke: DLL exports and isolated loopback packetization."""
import ctypes
import json
from pathlib import Path
import socket
import struct
import subprocess
import sys

dll, selftest = map(lambda value: Path(value).resolve(), sys.argv[1:3])
receiver_ip = sys.argv[3] if len(sys.argv) > 3 else '192.168.88.59'
module = ctypes.WinDLL(str(dll))
module.obs_module_name.restype = ctypes.c_char_p
assert module.obs_module_name() == b"SysX Canvas Sender"
assert callable(module.obs_module_load)
assert callable(module.obs_module_unload)
assert receiver_ip.encode('ascii') in dll.read_bytes()

with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
    receiver.bind(("127.0.0.1", 0))
    receiver.settimeout(10)
    process = subprocess.Popen(
        [str(selftest), str(receiver.getsockname()[1])],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    frames = {}
    complete = {}
    try:
        while len(complete) < 3:
            packet, address = receiver.recvfrom(65535)
            assert 30 < len(packet) <= 1200
            magic, version, flags, frame, index, count, width, height, timestamp = struct.unpack(
                "!4sBBQHHHHQ", packet[:30]
            )
            assert (magic, version, flags) == (b"RCVR", 1, 0)
            assert 0 <= index < count <= 65535
            metadata = (count, width, height, timestamp)
            entry = frames.setdefault(frame, (metadata, {}))
            assert entry[0] == metadata
            entry[1][index] = packet[30:]
            receiver.sendto(b"SYSCFG 1 CROP 640", address)
            if len(entry[1]) == count:
                jpeg = b"".join(entry[1][i] for i in range(count))
                assert jpeg.startswith(b"\xff\xd8") and jpeg.endswith(b"\xff\xd9")
                complete[frame] = (width, height)
        stdout, stderr = process.communicate(timeout=10)
        assert process.returncode == 0, (stdout, stderr)
        assert json.loads(stdout)["receiver_crop"] == 640
        assert sorted(complete.values()) == [(1, 1), (200, 120), (320, 320)]
    finally:
        if process.poll() is None:
            process.kill()
            process.communicate()

print(json.dumps({"dll_exports": "PASS", "packetization_and_crop_reply": "PASS",
                  "frames": list(complete.values()), "compiled_destination": f"{receiver_ip}:4200",
                  "scope": "headless loopback; not OBS graphics or LAN delivery"}))
