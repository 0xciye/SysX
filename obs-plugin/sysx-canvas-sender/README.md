# SysX OBS canvas sender

A UI-less OBS global module for Windows. Once loaded it:

1. reads the OBS main texture in the main-rendered callback,
2. copies the receiver-requested centered `N × N` crop on the GPU and reads it back through a three-surface staging ring (the render thread never waits on the readback it just queued),
3. JPEG-encodes the crop on a worker thread with libjpeg-turbo (BGRA input, 4:4:4 chroma by default so small Pixel Seek colors are not smeared),
4. sends it to SysX with the `RCVR` v1 UDP protocol (`docs/OBS_RECEIVER_PROTOCOL.md`): 30-byte big-endian header, 1,170-byte JPEG chunks, ≤1,200-byte datagrams.

The receiver answers the first datagram from a new sender with `SYSCFG 1 CROP <resolution>` and repeats it whenever Crop Resolution changes. The plugin accepts that message only from the configured receiver address and recreates its GPU crop resources, so the receiver's Crop Resolution stays authoritative. Only the newest frame is kept; a slow encoder or network drops obsolete frames instead of queueing them.

## Configuration

Configuration is compile-time (no ini file). Set the receiver IP with `scripts/build-obs-plugin.ps1 -ReceiverIp 192.168.88.59` or CMake `-DSYSX_RECEIVER_IP=192.168.88.59`. The manual GitHub DLL workflow provides a receiver-IP field. JPEG/port/crop constants remain in `sysx_canvas_sender.cpp`:

```cpp
// kReceiverHost comes from the SYSX_RECEIVER_IP CMake build option.
constexpr uint16_t kReceiverPort = 4200;        // receiver UDP port
constexpr int kJpegQuality = 80;                // 1-100
constexpr uint32_t kMaxFps = 0;                 // 0 = every OBS render frame
constexpr bool kChroma444 = true;               // false = 4:2:0 (smaller frames)
constexpr uint32_t kDefaultCrop = 640;          // until the receiver reports its crop
```

The plugin writes nothing to the OBS log (by request). OBS itself only logs `Loading module: …sysx-canvas-sender.dll`. HDR (non-8-bit-BGRA) canvases are silently not sent.

## Install

Copy `sysx-canvas-sender.dll` into `C:\Program Files\obs-studio\obs-plugins\64bit\` and restart OBS. Nothing else is required: libjpeg-turbo and the Visual C++ runtime are linked statically, so the DLL imports only `KERNEL32.dll` and `WS2_32.dll` (both part of Windows) and resolves libobs functions from the running `obs.dll`. No `jpeg62.dll`, VC++ redistributable, or config file is needed; the destination is compiled in (default 192.168.88.59:4200). Remove older SysX sender DLLs (for example `sysx-canvas-sender-gpu-localhost.dll`) so two senders do not stream at once — the protocol supports one sender per receiver.

## Build

Requires Visual Studio 2022 Build Tools, the libobs headers from the matching OBS source release (plus a directory containing `obsconfig.h`), and libjpeg-turbo. libobs graphics functions are resolved from the loaded `obs.dll` at runtime, so no OBS import library is required.

```powershell
cmake -S obs-plugin/sysx-canvas-sender -B work/obs-build-release -G "NMake Makefiles" -DCMAKE_BUILD_TYPE=Release `
  -DOBS_INCLUDE_DIR="work/obs-source/obs-studio-32.2.2/libobs;work/obs-config" `
  -DJPEG_INCLUDE_DIR="C:/libjpeg-turbo64/include" `
  -DJPEG_LIBRARY="C:/libjpeg-turbo64/lib/jpeg-static.lib"
cmake --build work/obs-build-release
```

Run these from a "x64 Native Tools" prompt (or after `vcvars64.bat`).

## Headless self-test

`sysx-sender-selftest` builds the same source without OBS and pushes a 320×320 frame, a 200×120 frame with a padded stride, and a 1×1 frame through the real encode/packetize path. It passes only if the receiver's `SYSCFG` crop reply arrives:

```powershell
cmake --build work/obs-build-release --target sysx-sender-selftest
src-tauri/binaries/receiver-backend-x86_64-pc-windows-msvc.exe udp-listen --host 127.0.0.1 --port 4299 --count 3
work/obs-build-release/sysx-sender-selftest.exe 4299
```

Validated on Windows 10 (2026-10-15) against the Rust `udp-listen` verifier: 3/3 frames decoded at their declared sizes, 0 invalid packets, 0 dropped/expired frames, crop reply 640.

## Headless libobs check

`sysx-obs-host-check` loads the real `obs.dll` (OBS 32.2.2 runtime in `work/obs-runtime`), starts D3D11 video at 1920×1080/60 with no window, loads the plugin via `obs_open_module` + `obs_init_module` exactly as OBS does, runs, then calls `obs_shutdown` (which unloads the plugin). Run `receiver-backend udp-listen` on the compiled-in port first:

```powershell
cmake --build work/obs-build-release --target sysx-obs-host-check
cd work/obs-runtime/bin/64bit
# the DLL sends to its compiled-in kReceiverHost:kReceiverPort; run udp-listen there
..\..\..\obs-build-release\sysx-obs-host-check.exe <path>\sysx-canvas-sender.dll 10
```

Result on Windows 10 + RTX 4060 (2026-10-15, static-CRT build): plugin loaded and `udp-listen` received 300/300 frames (640×640, frame IDs 1–300) at OBS's 60 FPS render rate; shutdown/unload was clean. HDR check (Rec.2100 PQ canvas, RGBA16F main texture): the plugin loaded, sent nothing, and unloaded cleanly. After logging was removed the silent build was rechecked: no plugin lines in the libobs log, 239 frames in 4 s at 60 FPS, HDR still skipped. Throughput (640×640, 4:4:4, quality 80, loopback, 5 s including startup): 1,171 frames at a 240 FPS render rate (234.2 FPS) and 1,784 frames at 360 FPS (356.8 FPS). The harness takes optional `<fps>` and `hdr` arguments after the seconds. Not yet validated: the OBS Studio app UI itself, real game/scene content at high FPS, and LAN/firewall delivery between two computers.

The DLL build also emits `dist-symbols/obs/sysx-canvas-sender.pdb`; keep it separate from the installed DLL. The manual workflow verifies its GUID/age against the DLL and saves an OBS symbols ZIP in the draft release. OBS crash reports are managed by OBS under `%APPDATA%\obs-studio\crashes` (ordinary OBS logs: `%APPDATA%\obs-studio\logs`).
