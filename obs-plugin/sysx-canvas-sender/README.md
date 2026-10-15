# SysX OBS canvas sender

This is a UI-less OBS global module. When loaded, it captures the full main OBS canvas, encodes each frame as JPEG quality 80, and sends it over UDP to the compile-time destination in `sysx_canvas_sender.cpp`.

The sender uses SysX's `RCVR` v1 packet format (30-byte big-endian header, 1,170-byte JPEG chunks, 1,200-byte datagrams). It does not crop or resize. The SysX receiver performs the configured centered crop after decoding.

Edit these constants before building:

```cpp
constexpr char kDestinationIp[] = "192.168.1.100";
constexpr uint16_t kDestinationPort = 4200;
constexpr int kJpegQuality = 80;
```

Build on Windows with the OBS development SDK and libjpeg-turbo:

```powershell
cmake -S obs-plugin/sysx-canvas-sender -B build/obs-sysx `
  -DOBS_INCLUDE_DIR="C:/Program Files/obs-studio/include" `
  -DOBS_LIB_DIR="C:/Program Files/obs-studio/bin/64bit" `
  -DJPEG_INCLUDE_DIR="C:/vcpkg/installed/x64-windows/include" `
  -DJPEG_LIBRARY="C:/vcpkg/installed/x64-windows/lib/jpeg.lib"
cmake --build build/obs-sysx --config Release
```

Copy `sysx-canvas-sender.dll` into OBS's plugin directory under `obs-plugins/64bit`.

The module has no settings page and intentionally emits no plugin logs. It keeps only the newest captured frame, so a slow encoder/network silently drops obsolete frames.
