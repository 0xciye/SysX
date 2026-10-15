# SysX

SysX is a portable Windows desktop application distributed as one `SysX.exe`; no installer is produced. The executable embeds its Rust backend and configuration assets, then extracts them to app data while it runs. Its native window is a compact monitor with Preview, five equal-width Mouse State buttons, device state, and receiver connection state. Configuration opens automatically in a separate browser window at a loopback-only local address. Settings are stored in the computer's app-data folder and restored on launch. There are no Start, Pause, Stop, or Preview visibility controls.

The application source is Rust: Tauri 2 hosts a Leptos/WebAssembly UI and a Rust backend sidecar embedded in the desktop executable. Trunk builds one UI bundle for the native monitor and the separate loopback-only configuration page. The legacy React/TypeScript frontend and Python backend have been removed. Object Detection has ONNX Runtime, OpenVINO, and optional TensorRT adapters; no user model is included, so inference has not been exercised with a real model or target hardware.

An earlier macOS bundle used the Python backend. Its 192 × 192 synthetic MJPEG smoke at a requested 180 FPS showed 180.0 source FPS, 179.5 processing FPS, 60.1 preview FPS, 5 input drops, and 0 transport drops. This is historical evidence from the retired implementation, not an OBS, Windows, or target-hardware acceptance test. The current source builds `SysX.app` with the Rust backend.

## Current status (2026-10-15)

The active source and runtime are Rust across the Tauri host, Leptos/WASM UI, and receiver sidecar. The current release macOS bundle was rebuilt from this source; its native WebView mounts the Rust UI through an embedded WASM bootstrap and shows a square preview, `NO FRAME`, and FPS/latency/resolution HUD. The loopback configuration page mounts the same Rust UI in Chrome and persists settings through `app-settings.json`. Windows executable and physical Medius/Ferrum behavior still require target-environment validation.

The desktop shell shows the always-visible Preview, Mouse State, device status, and receiver status. Preview displays “NO FRAME” while waiting for a sender and starts automatically when a valid frame arrives. The HUD contains only measured processing FPS, latency, and processing resolution. The circle-only FOV overlay is always enabled; its size is configured in the separate browser page and scales with the displayed image.

The local configuration page keeps General, target-relative X/Y offsets, independent X/Y exponential-smoothing alpha sliders, Sensitivity, DPI, recoil, and FOV together on one vertically scrolling page. Trigger button can be set to Right, Middle, M4, or M5; Left is excluded. **Apply Changes** writes `app-settings.json` under app data and applies supported runtime settings. The backend loads that file on startup and creates it with defaults on first launch. The selected physical trigger gates movement; fresh target deltas are summed before output, recoil is added to Y, and each device command is clamped to ±32 per axis. The offset marker previews the signed crop-pixel destination relative to the selected target. Lower alpha filters more; `1.00` leaves the target unchanged. Movement uses the internal NCAF three-zone response, prediction, cubic Bézier response, and occasional micro-corrections. The filtered target is used separately from raw detections for Preview and movement. The page opens on an available local port beginning at `4201` if the preferred port is busy. Backend selects the model artifact; Compute is passed to the selected runtime where supported. Precision remains a saved preference because the imported artifact determines its precision.

The SysX mark is a custom crop-frame and focus-point SVG shared by the title bar and desktop icon. Tauri packages its 512 × 512 PNG and multi-size Windows ICO exports.

The backend tracks source and inference resolutions, actual inference device, detection counts, measured source/processing/preview FPS, processing latency, and input/transport drops. The native monitor HUD intentionally shows only processing FPS, latency, and processing resolution. Source FPS counts valid decoded frames; processing FPS counts completed detections. Both use a trailing one-second window. The processing-FPS window retains up to 4096 completion timestamps; a regression test verifies its count is not capped at 360 FPS (synthetic timestamps, not a throughput benchmark). The preview returns to its sender-waiting state and clears the old image and live measurements when the stream goes idle. Once its authenticated backend connection is ready, the status reports “Waiting for sender.” A circle-only FOV overlay is always visible; its size is adjustable from 1–360°. Its diameter scales with the shorter side of the displayed image, and 360° fits inside that image at any sender resolution. Pixel Seek has code-defined RGB-to-HSV profiles for yellow, red, and purple.

The native window enforces a 980 × 680 minimum size. Older screenshots in `design-review/` show earlier layouts and should not be treated as current UI references.

An earlier release-mode validation app with isolated app data received the 192 × 192 JPEG fixture on UDP port `4202` during an 8-second 360-FPS-target run. The sender measured 350.74 FPS and skipped 74 sender slots; the HUD showed 355.0 source/processing FPS, 60.0 preview FPS, 0 input/transport drops, and 0.04 ms processing latency. This is historical pre-Rust performance evidence; it does not validate the current Rust path, OBS, Windows, sustained sender throughput at 360 FPS, or target hardware.

Rust UI unit tests cover aspect-preserving preview scaling, letterboxed detection-box mapping, and HiDPI canvas sizing. Rust backend tests cover detection confidence, target selection, committing the ONNX/TensorRT/OpenVINO model bundle, and changing the UDP port then processing the actual JPEG fixture on the new port.

Object Detection imports one bundle containing `.onnx`, `.engine` or `.plan`, and an OpenVINO `.xml`/`.bin` pair. ONNX Runtime currently runs on CPU; OpenVINO accepts CPU/GPU/NPU targets supported by the installed runtime; TensorRT is built only when TensorRT headers and `CUDA_PATH` are available and requires a GPU engine. The exported YOLOv8 model must accept NCHW RGB float input at exactly the configured crop dimensions and return raw `[1, 4+classes, anchors]` or `[1, anchors, 4+classes]` output. Confidence/IoU filtering and class-aware NMS run in Rust, then the existing FOV and prior-target selection runs. Crop/model resolution mismatch is reported instead of resizing. The Precision selector is saved as a preference; the selected artifact determines actual precision. The desktop UI has no Data or export section.

Compute offers CPU, GPU, or NPU. ONNX Runtime is CPU-only, OpenVINO uses the installed device plugin, and TensorRT requires a GPU plus an SDK-enabled build; unavailable targets surface as backend runtime errors.

The UDP receiver defaults to port `4200`; the backend suite verifies that changing the port moves the listener and that a subsequent frame is processed. Crop Resolution is configured in the browser settings; for example, `256` extracts a centered 256 × 256 region from each full frame. The receiver sends that crop to the selected detection engine and shows the cropped image in Preview. Frames smaller than the configured crop are rejected; the crop is never silently resized. Source runtime failures retry every three seconds. If another process owns a requested port, SysX keeps the current listener active and retries the bind; runtime status clears the bind error and updates when the port becomes available. Model controls are shown only in Object Detection mode. The preview HUD reports measured source, processing, and preview FPS separately.

The Rust sidecar owns UDP reassembly, JPEG decoding, centered crop, Pixel Seek, bounded latest-frame processing, JSONL storage, local web configuration, and authenticated preview transport. The release executable embeds the sidecar and config assets and extracts a per-process runtime under app data; the Windows build script emits `SysX.exe` without an installer. The sidecar processed the 192 × 192 JPEG fixture once on UDP `4202` in an isolated macOS smoke run; this was not a Tauri-window, performance, Windows, OBS, or hardware validation. Earlier 1280 × 720 synthetic rates were measured by the retired Python implementation and do not measure Rust. No GitHub Actions build workflow is configured. Windows runtime and target-hardware performance remain unverified. No user model is present.

## Requirements

- macOS for development; Windows 10/11 x64 for building and running the portable executable.
- Node.js 20+, pnpm, Rust stable, and Trunk. Node and pnpm are used only for Tauri CLI and build orchestration; application runtime and logic are Rust.
- Windows builds must be produced and validated on Windows (or a Windows CI runner).
- A Windows release is built with `scripts/build-windows.ps1` and yields `src-tauri/target/release/SysX.exe`; the OS WebView2 runtime and any selected inference runtimes remain machine prerequisites.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture, setup, build commands, and milestone status.

For sender integration, see [the OBS plugin and SysX protocol guide](docs/OBS_RECEIVER_PROTOCOL.md). It documents the implemented UDP v1 packet format for successive full-frame JPEG images, limits, receiver-side cropping, and unimplemented sender requirements.

```sh
pnpm install
pnpm tauri dev
```

Rust backend development and the current migration status are documented in `DEVELOPMENT.md`.

The Rust UDP verifier can receive packets and send the actual JPEG fixture:

```sh
cargo run --manifest-path rust-backend/Cargo.toml -- udp-listen --host 127.0.0.1 --port 4200 --count 1
cargo run --manifest-path rust-backend/Cargo.toml -- udp-send-image src/assets/red-cube-192.jpg --host 127.0.0.1 --port 4200
```

This utility verifies the frame protocol; it is not the future production OBS sender.

## Product constraints

- The UI is English, dark, minimal, and technical.
- The native window is a live monitor; configuration opens in a separate local browser page with General and FOV sections on the same page.
- Pixel Seek and Object Detection are the only detection strategies.
- User models are loaded only when selected; no model is downloaded automatically.
- Pixel Seek selects exactly one code-profile color (Yellow, Red, or Purple).
- Pixel Seek code profiles can restrict detection to a normalized source-frame region; with no region configured, detection uses the full frame.
- FPS, latency, detections, and frame-drop counts are always measured values. Prototype sample data is never used in production.
- The target of 180–360+ processing FPS is a requirement to benchmark, not a performance claim.
- The desktop header identifies Medius boxes through the official USB handshake and Ferrum through `km.version()`. Medius uses the official crate's fixed 6,000,000 baud; Ferrum Legacy CP210x uses 115,200 baud. The header also shows physical button states. While the configured Right/Middle/M4/M5 trigger is held, the selected device receives relative movement from fresh, batched deltas. Movement uses the X/Y filtered target, crop-center error, saved offsets, fractional residuals, and the sensitivity/DPI formula (defaults: sens 0.235, DPI 800), with a ±32 per-axis command limit. No Windows hardware test has been run.
- API implementation notes: [MediusAPI.md](MediusAPI.md) and [FerrumAPI.md](FerrumAPI.md). They summarize SysX device connection, button telemetry, and relative-movement paths and index the broader vendor API families; consult the linked vendor pages for exact signatures and version support.
- Device output sends Medius `move_rel(dx,dy)` or Ferrum Legacy `km.move(dx,dy)` through the verified connection. Ferrum App Software API remains preferred when its endpoint can be configured.
- Movement settings persist the recoil profile; active-state and timed-action fields are not exposed or run. The current route requires a fresh target, a held configured trigger, and a verified device connection.
- The historical synthetic benchmark figures above predate the Rust migration. There is no current Rust end-to-end benchmark result.

## Sample frame

This 192 × 192 JPEG is the sample used by the UDP smoke test. The packaged macOS app has also received 192 × 192 sender frames and enlarged them in Preview while preserving their aspect ratio:

![192 by 192 red cube JPEG sender frame](/Users/ciye/Documents/pixelobject/src/assets/red-cube-192.jpg)

Pixel Seek offers one target color at a time (yellow, red, or purple). Its code-defined HSV ranges are in `rust-backend/src/pixel_seek.rs`: red H 0–10/S 50–255/V 180–255, yellow H 20–40/S 157–255/V 252–255, and purple H 140–160/S 60–255/V 240–255. The UDP receiver decodes JPEG frames to RGB before thresholding; hue uses 0–179, while saturation and value use 0–255.

The Rust sidecar has adapters for ONNX Runtime CPU, OpenVINO, and optional TensorRT. Runtime adapters compile and the decoder/selection behavior has unit coverage; the repository contains no model, so model inference, accelerator availability, Windows packaging, and device behavior remain unverified. CapkfaPlus reconstruction logic was translated selectively; see the development status for the mapped behaviors and limits.

The packaged receiver includes a visible startup/`NO FRAME` fallback for WebView initialization failures, so a platform WebView failure does not present as an empty black window. Normal startup removes that fallback when the Leptos UI mounts.

The Rust backend keeps UDP packet reassembly in `rust-backend/src/udp.rs`, decoded UDP input in `receiver.rs`, centered crop geometry in `crop.rs`, and local configuration in `web_config.rs`. Movement conversion and 4 ms batching live in `rust-backend/src/movement.rs`; Tauri's pending-delta merge, freshness check, trigger mapping, and ±32 per-axis command clamp live in `src-tauri/src/movement.rs`. Each stage sums fresh deltas waiting in its batch and drops pending deltas older than 100 ms before merging; one device API command carries each resulting batch. Excess beyond ±32 is clipped for that command and is not carried forward. Medius output uses its Rust client; Ferrum output uses the documented serial API. The reconstructed type-0 serial queue is Rust-only and remains behind an unbound writer interface.
