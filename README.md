# SysX

SysX is an installed Windows desktop application for continuously receiving and processing image frames. Settings is the primary workspace, with General and FOV pages. Preview can be shown or hidden from General; hiding it leaves frame reception and processing running. When shown, it displays “NO FRAME” until a sender's first frame arrives. There are no Start, Pause, or Stop controls.

The application is built with Tauri 2, React, TypeScript, and a separate Python vision backend. It runs as a desktop window; it does not require a browser tab or a public web server. On Windows, the UI is rendered by the system WebView2 runtime.

The prior local macOS `Receiver.app` bundle included the Tauri desktop shell and architecture-specific Python backend sidecar. Its 192 × 192 MJPEG smoke run at a 180 FPS send target showed 180.0 source FPS, 179.5 processing FPS, 60.1 preview FPS, 5 input drops, and 0 transport drops in the live HUD. This is local synthetic evidence, not an OBS, Windows, or target-hardware acceptance test. The current-source macOS bundle is `SysX.app`; its desktop title and header show SysX.

## Current status

The Tauri/React desktop shell opens to Settings. General contains receiver and detection controls, plus an optional Preview pane. FOV has its own settings page, while its circle overlay remains enabled in every preview. The Preview pane is capped at 480 × 480 px and supports smooth upscaling or nearest-neighbor Pixel Perfect display while preserving aspect ratio. Its smaller viewport limits how far low-resolution sender frames are enlarged. Settings scrolls inside the window as options are added.

The SysX mark is a custom crop-frame and focus-point SVG shared by the title bar and desktop icon. Tauri packages its 512 × 512 PNG and multi-size Windows ICO exports.

The compact Preview HUD reports source and inference resolutions, actual inference device, detection count, measured source/processing/preview FPS, processing latency, and input/transport dropped-frame counters. Source FPS counts valid decoded frames; processing FPS counts completed detections. Both use a trailing one-second window. The processing-FPS window retains up to 4096 completion timestamps; a regression test verifies its count is not capped at 360 FPS (synthetic timestamps, not a throughput benchmark). The preview returns to its sender-waiting state and clears the old image and live measurements when the stream goes idle. Once its authenticated backend connection is ready, the status reports “Waiting for sender.” A circle-only FOV overlay is always visible; its size is adjustable from 1–360°. Its diameter scales with the shorter side of the displayed image, and 360° fits inside that image at any sender resolution. Empty HSV profiles remain unconfigured until real thresholds are supplied in code.

The native window enforces a 980 × 680 minimum size. The current settings-first General page is captured in `design-review/receiver-settings-general.png`; it shows the optional Preview pane and current Detection Engine layout. The isolated app reports a UDP port conflict and does not verify live streaming. A separate release Tauri capture with a live 192 × 192 sender frame shows the image enlarged within its viewport and the always-on FOV circle. Older screenshots in `design-review/` predate the current interface and should not be treated as current UI references.

The latest 300.42 MiB macOS release bundle has been rebuilt with the Pixel Seek color selector inside Detection Engine. A release-mode validation app with isolated app data received the 192 × 192 JPEG fixture on UDP port `4202` during an 8-second 360-FPS-target run. The sender measured 350.74 FPS and skipped 74 sender slots; the live HUD snapshot showed 355.0 source/processing FPS, 60.0 preview FPS, 0 input/transport drops, and 0.04 ms processing latency. This verifies the local macOS Tauri preview path at that moment only; it does not validate OBS, Windows, sustained sender throughput at 360 FPS, or target hardware.

Frontend tests cover aspect-preserving preview scaling, letterboxed detection-box mapping, HiDPI canvas sizing, and Object Detection confidence filtering (including null scores and Pixel Seek bypass).

Settings supports importing user `.pt` models. The Detection Engine uses separate radio cards for Pixel Seek and Object Detection, stacking them at compact window widths. When Pixel Seek is selected, the Yellow/Red/Purple choices appear in their own full-width row below both cards; detections use the selected color for their boxes and have no AI labels. Object Detection always shows its 0–1 minimum-confidence slider, even before a model is selected; model-specific settings appear with the active model. **Apply Changes** saves the current application preferences locally and restores them on the next launch, including detection engine, target color, FOV, preview scaling, UDP port, and model selections/settings; successful applies show “Changes applied successfully” for three seconds. Backend, Precision, and Compute selections are saved as preferences only; they do not configure inference. Actual inference support remains user-supplied Ultralytics `.pt` models using the runtime-reported device. The desktop UI has no Data or export section.

The Device control offers Apple MPS only on macOS. Explicit CUDA/MPS choices are checked against the backend runtime and surface a recovery message when unavailable.

The UDP receiver defaults to port `4200`; the backend suite verifies that changing the port moves the listener and that a subsequent frame is processed. Source runtime failures retry every three seconds. If another process owns a requested port, SysX keeps the current listener active and retries the bind; runtime status clears the bind error and updates when the port becomes available. Model controls are shown only in Object Detection mode. The preview HUD reports measured source, processing, and preview FPS separately.

The independent Python backend has the shared detection result, Pixel Seek, lazy user-supplied Ultralytics integration, persistent model registry, bounded latest-frame processing, JSONL storage, and a bounded versioned UDP chunk reassembler. Sender-cropped MJPEG is the only production input; the reassembler validates JPEG SOI/EOI markers on boundary chunks before retaining frame data, and the decoder checks them again before decoding so other formats accepted by OpenCV cannot enter this path. The desktop waits for sender frames and previews them automatically. The Tauri window launches the Python backend and receives binary preview frames and diagnostics over authenticated loopback WebSocket. On macOS 15.3.1 ARM64, a three-second synthetic 1280×720 JPEG benchmark (16,634 bytes, 15 datagrams per frame) measured sender/source/processing rates of 179.95/179.93/180.05 FPS at the 180 target and 359.99/359.81/359.79 FPS at the 360 target. Both runs had zero input, transport, invalid-packet, or rejected-frame drops and sent 181 WebSocket preview frames. After adding SOI/EOI validation during UDP reassembly, 3-second synthetic runs measured 179.88/179.96/179.87 sender/source/processing FPS at the 180 target and 359.96/360.06/359.79 at the 360 target. Both had no skipped sender slots, input or transport drops, invalid packets, or rejected frames and sent 181 preview messages. These synthetic benchmarks exclude OBS capture, the Tauri renderer, Windows, and production HSV profiles. A five-second live Tauri run with 192×192 JPEGs measured 179.9 source/processing FPS at the 180 target and 360.2 source/processing FPS at the 360 target, with a 60 FPS preview. The sender achieved 179.4 and 357.7 FPS respectively; the 360 run skipped 11 sender slots, and the Tauri snapshot showed 7 input drops and 0 transport drops. The Windows build workflow runs on pushes, pull requests, and manual dispatch; it builds the PyInstaller sidecar, runs backend tests, verifies UDP port `4200` with a 192×192 frame, silently installs the NSIS artifact, confirms the installed backend process owns UDP port `4200`, checks shutdown, uninstalls the app, and uploads the installer. The PowerShell build script checks native command exit codes so failed tests or packaging steps fail the workflow. Tauri option forwarding was checked locally; the Windows workflow has not run, so Windows runtime and target-hardware performance remain unverified. No user model is present.

## Requirements

- macOS for development; Windows 10/11 x64 for the installed application.
- Node.js, pnpm, Rust stable, and Python 3.11 or newer.
- Windows builds must be produced and validated on Windows (or a Windows CI runner).

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture, setup, build commands, and milestone status.

For sender integration, see [the OBS plugin and Receiver protocol guide](docs/OBS_RECEIVER_PROTOCOL.md). It documents the implemented UDP v1 packet format for successive cropped JPEG frames, limits, receiver workflow, and unimplemented sender requirements.

```sh
pnpm install
pnpm tauri dev
```

Python backend development uses the environment and commands documented in `DEVELOPMENT.md`.

After creating the project environment, the UDP verifier can receive packets with `receiver-backend udp-listen --host 127.0.0.1 --port 4200` and send a local image using `receiver-backend udp-send-image path/to/image.png --port 4200`. This utility verifies the frame protocol; it is not the future production OBS sender.

## Product constraints

- The UI is English, dark, minimal, and technical.
- Settings is the primary receiver workspace; Preview is optional and FOV has a dedicated settings page.
- Pixel Seek and Object Detection are the only detection strategies.
- User models are loaded only when selected; no model is downloaded automatically.
- Pixel Seek selects only one code-profile color (Yellow, Red, or Purple); HSV profiles remain empty until configured.
- Pixel Seek code profiles can restrict detection to a normalized source-frame region; with no region configured, detection uses the full frame.
- FPS, latency, detections, and frame-drop counts are always measured values. Prototype sample data is never used in production.
- The target of 180–360+ processing FPS is a requirement to benchmark, not a performance claim.
- The desktop header identifies Medius boxes through the official USB handshake and Ferrum through `km.version()`. It also shows the five physical mouse-button states when supported by the device event stream; unknown states remain neutral until the first event. Button monitoring does not inject input. Ferrum currently uses the Legacy CP210x probe; the Ferrum App Software API endpoint needs configuration. Hardware behavior is not validated on Windows in this workspace.
- API implementation notes: [MediusAPI.md](MediusAPI.md) and [FerrumAPI.md](FerrumAPI.md). They summarize the Receiver connection, button telemetry, and relative-movement paths and index the broader vendor API families; consult the linked vendor pages for exact signatures and version support.
- Device integration currently identifies connected hardware and reads its name; it does not inject or modify mouse/keyboard input.
- `scripts/benchmark_pixel_seek.py` measures only synthetic Pixel Seek strategy throughput; it is not an end-to-end benchmark.
- `scripts/benchmark_receiver_udp.py` measures the synthetic local UDP-to-WebSocket backend path; it does not measure the installed Windows UI or OBS plugin.

## Sample frame

This 192 × 192 JPEG is the sample used by the UDP smoke test. The packaged macOS app has also received 192 × 192 sender frames and enlarged them in Preview while preserving their aspect ratio:

![192 by 192 red cube JPEG sender frame](/Users/ciye/Documents/pixelobject/src/assets/red-cube-192.jpg)

Pixel Seek offers one target color at a time (yellow, red, or purple). Its HSV ranges are configured in `backend/src/receiver_backend/service.py` in `PIXEL_COLOR_PROFILES`; the current profiles are intentionally empty, so color detections remain unavailable until real ranges are added.
