# Product

> **Current availability:** Pixel Seek is the only usable detection engine. Object Detection and its ONNX Runtime, OpenVINO and TensorRT integrations are **not ready for use**. Their source/dependencies remain for development; build and unit-test results are not functional validation. SysX supports **Medius only** through its Rust SDK.

<!-- impeccable:product-schema 1 -->

## Current implementation status

SysX is maintained as a Rust application across the Tauri host, Leptos/WASM UI, and receiver sidecar. The current release bundle mounts the Rust UI through an embedded WASM bootstrap, with a visible square native preview startup state; the local configuration page runs the same Rust UI in Chrome. Settings are restored from and written to `app-settings.json`; Windows packaging and physical Medius movement remain target-environment validation work.

## Platform
adaptive

## Stack
The application source is Rust: Tauri 2 hosts a Leptos/WebAssembly UI and an embedded Rust backend sidecar. The Windows release is one portable `SysX.exe`, with no installer; the executable extracts runtime files under app data while running. Configuration runs in a separate loopback-only browser page. Object Detection and ONNX Runtime, OpenVINO and TensorRT integrations are unavailable for use. The legacy React/TypeScript frontend and Python backend have been removed.

## Users
Developers or operators monitoring a continuous computer-vision stream between a sender computer and a receiver computer.

## Product Purpose
Receive image frames on a secondary computer, process them using Pixel Seek (Object Detection is unavailable), and show a large realtime preview with trustworthy diagnostics. Success means operators can see the current stream and processing health without manually starting a session.

## Positioning
A receiver-only desktop tool that waits for a sender and automatically crops incoming full-frame MJPEG at the receiver and previews the crop.

## Operating Context
Used on a secondary computer while another computer continuously sends frames. The operator monitors preview, active strategy/model, and processing diagnostics. Target stream rates may exceed 180–360 FPS; this is a requirement to evaluate, not a measured performance claim.

## Capabilities and Constraints
- Develop and test on Windows 10/11 x64; ship a portable `.exe` distribution with no installer.
- Work locally without a cloud service or public internet connection. The browser configuration page is served only from loopback.
- Keep frame acquisition, processing, persistence, and rendering decoupled across the Rust backend and desktop frontend. A sender streams full-frame MJPEG continuously over the implemented UDP protocol; the receiver validates, reassembles, decodes, crops, processes, and previews it.
- Pixel Seek is the only usable strategy. Object Detection is an unavailable development feature.
- ONNX Runtime, OpenVINO and TensorRT integrations are unavailable for use; retain them only as development source.
- Pixel Seek selects one target color at a time (yellow, red, or purple); HSV thresholds are maintained in code.
- Crop Resolution in web config sets a centered `N × N` receiver crop. Pixel Seek rules support optional normalized region-of-interest bounds in original-frame coordinates; absent bounds cover the full source frame.
- Wait for sender input automatically; show a waiting state before frames arrive and start preview on receipt. No Start/Pause/Stop controls in the primary workflow.
- Keep the native window focused on a square Preview, five mouse-button states, device state, and receiver connection state. Preview is always visible and capped at 640 × 640 CSS pixels; its compact HUD shows processing resolution, FPS, and latency.
- Open a separate browser window to the local configuration page when the desktop app starts. Keep all settings on one scrolling page, including signed X/Y target offsets with a relative position indicator. Persist settings under application data and restore them at backend startup.
- Full-frame MJPEG from the OBS sender is the production input. The receiver applies the configured centered square crop before detection and Preview; see `docs/OBS_RECEIVER_PROTOCOL.md`.
- Import one user-provided model bundle containing `.onnx`, `.engine` or `.plan`, and the OpenVINO `.xml`/`.bin` pair; do not download default models.
- Persist Sensitivity and the recoil profile in the local `app-settings.json` file. Load it when the backend starts and write it when the local configuration page applies changes. Movement output is gated by the selected held trigger and uses fresh, batched deltas through the verified Medius relative-move APIs. Reconstructed active-state, mode-predicate, direct-offset, and timed-click routes are not part of the current product.
- Pixel Seek must support arbitrary valid input resolutions; AI input resolution follows each selected model's configuration.
- Do not invent detections, FPS, latency, or benchmark results in production.
- UDP v1 and authenticated loopback WebSocket transport are implemented. Achievable end-to-end throughput remains unmeasured and must be benchmarked on target hardware.

## Brand Commitments
- Product name: SysX.
- Interface language: English.
- Visual direction requested: dark, minimal, professional, technical.
- Keep the native monitor window limited to Preview, device/connection status, and mouse-button states.
- Put configuration in the separate local browser page on one vertically scrolling page, with a global Apply Changes action. Include General, signed X/Y target offsets with a target-relative position indicator, and FOV settings.
- Keep Preview always visible and the FOV overlay always enabled, with a 1–360° control in the local configuration page.
- Cap the square Preview viewport at 640 × 640 CSS pixels; smoothly enlarge low-resolution sender frames while preserving their aspect ratio.
- Let the configuration page accommodate additional options through vertical scrolling.
- User-provided screenshot is a reference for the preview and compact HUD layout; do not copy it literally.

## Evidence on Hand
- User-supplied screenshot reference in the conversation.
- No trained AI model files, sender implementation, or real performance measurements are present in the repository.

## Product Principles
- Keep the incoming frame as the visual focus.
- Show the sender-waiting and connection-error states clearly.
- Prefer measured telemetry over decorative charts or implied benchmarks.
- Keep frame acquisition and inference independent from UI rendering.
- Keep the preview in a clear waiting state until sender frames arrive.

## Device API

Only Medius is supported, through the official Rust SDK. Ferrum and MAKCU are removed. See [MediusAPI.md](MediusAPI.md).
