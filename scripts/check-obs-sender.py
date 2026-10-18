"""Windows sender build smoke: DLL exports and isolated loopback packetization."""
import ctypes
import json
from pathlib import Path
import socket
import struct
import subprocess
import sys

dll, selftest = map(lambda value: Path(value).resolve(), sys.argv[1:3])
receiver_ip = sys.argv[3] if len(sys.argv) > 3 else 'auto'
module = ctypes.WinDLL(str(dll))
module.obs_module_name.restype = ctypes.c_char_p
assert module.obs_module_name() == b"SysX Canvas Sender"
assert callable(module.obs_module_load)
assert callable(module.obs_module_unload)
assert receiver_ip.encode('ascii') in dll.read_bytes()

def stream(discover):
    """Run the self-test against a loopback receiver; with discover the
    self-test has no address and must find this receiver by its probe."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
        receiver.bind(("127.0.0.1", 0))
        receiver.settimeout(10)
        args = [str(selftest), str(receiver.getsockname()[1])]
        if discover:
            args += ["0", "auto"]
        process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        frames = {}
        complete = {}
        probes = 0
        try:
            while len(complete) < 3:
                packet, address = receiver.recvfrom(65535)
                if packet == b"SYSXFIND 1":
                    assert discover, "a sender with an address must not probe"
                    probes += 1
                    receiver.sendto(b"SYSCFG 1 CROP 640", address)
                    continue
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
            assert not discover or probes > 0
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
        return list(complete.values())


complete = stream(False)
stream(True)

print(json.dumps({"dll_exports": "PASS", "packetization_and_crop_reply": "PASS",
                  "loopback_discovery": "PASS",
                  "frames": complete, "compiled_destination": f"{receiver_ip}:4200",
                  "scope": "headless loopback; not OBS graphics or LAN delivery"}))
