# Product

> **Current availability:** Pixel Seek is the only usable detection engine. Object Detection and its ONNX Runtime, OpenVINO and TensorRT integrations are **not ready for use**. Their source/dependencies remain for development; build and unit-test results are not functional validation. SysX supports **Medius only** through its Rust SDK.

<!-- impeccable:product-schema 1 -->

## Current implementation status

SysX is maintained as a Rust application across the native Slint host, shared Rust UI helpers, and receiver sidecar. The receiver and Settings are native windows; no browser config page, WebView, Leptos UI, or config HTTP endpoint is part of the product. Settings are restored from and written to `app-settings.json`; Windows packaging and physical Medius movement remain target-environment validation work.

## Platform
adaptive

## Stack
The application source is Rust: a native Slint host supervises an embedded Rust backend sidecar and owns Medius I/O. Windows ships as one portable `SysX.exe`; macOS ships as a native `.app` bundle. Each host extracts runtime files under platform app data while running. Configuration is edited in the native Settings window over authenticated loopback IPC. Object Detection and ONNX Runtime, OpenVINO and TensorRT integrations are unavailable for use.

## Users
Developers or operators monitoring a continuous computer-vision stream between a sender computer and a receiver computer.

## Product Purpose
Receive image frames on a secondary computer, process them using Pixel Seek (Object Detection is unavailable), and show a large realtime preview with trustworthy diagnostics. Success means operators can see the current stream and processing health without manually starting a session.

## Positioning
A receiver-only desktop tool that waits for the OBS sender and previews the centered crop the sender produces on the GPU.

## Operating Context
Used on a secondary computer while another computer continuously sends frames. The operator monitors preview, active strategy/model, and processing diagnostics. Target stream rates may exceed 180–360 FPS; this is a requirement to evaluate, not a measured performance claim.

## Capabilities and Constraints
- Support Windows 10/11 x64 and macOS 12+; ship a portable Windows `.exe` and native macOS `.app`, with platform-native builds.
- Work locally without a cloud service or public internet connection. The native Settings window uses authenticated loopback IPC.
- Keep frame acquisition, processing, persistence, and rendering decoupled across the Rust backend and desktop frontend. A sender streams full-frame MJPEG continuously over the implemented UDP protocol; the receiver validates, reassembles, decodes, crops, processes, and previews it.
- Pixel Seek is the only usable strategy. Object Detection is an unavailable development feature.
- ONNX Runtime, OpenVINO and TensorRT integrations are unavailable for use; retain them only as development source.
- Pixel Seek selects one target color at a time (yellow, red, or purple); HSV thresholds are maintained in code.
- Crop Resolution in native Settings is sent to the OBS sender, which produces a centered `N × N` crop on the GPU. The receiver processes the received frame as-is and does not crop again. Pixel Seek rules support optional normalized region-of-interest bounds in source-frame coordinates; absent bounds cover the full frame.
- Wait for sender input automatically; show a waiting state before frames arrive and start preview on receipt. No Start/Pause/Stop controls in the primary workflow.
- Keep the native window focused on a square Preview, five mouse-button states, device state, and receiver connection state. Preview is always visible and capped at 640 × 640 CSS pixels; its compact HUD shows processing resolution, FPS, and latency.
- Open Settings in a native Slint window from the receiver. Keep settings on one scrolling page, including signed X/Y target offsets with a relative position indicator. Persist settings under application data and restore them at backend startup.
- MJPEG from the OBS sender is the only production input. The sender applies the configured centered square crop before encoding; the receiver never crops a second time. See `docs/OBS_RECEIVER_PROTOCOL.md`.
- Import one user-provided model bundle containing `.onnx`, `.engine` or `.plan`, and the OpenVINO `.xml`/`.bin` pair; do not download default models.
- Persist Sensitivity and the recoil profile in the local `app-settings.json` file. Load it when the backend starts and write it when native Settings applies changes. Movement output is gated by the selected held trigger and uses fresh, batched deltas through the verified Medius relative-move APIs. Reconstructed active-state, mode-predicate, direct-offset, and timed-click routes are not part of the current product.
- Pixel Seek must support arbitrary valid input resolutions; AI input resolution follows each selected model's configuration.
- Do not invent detections, FPS, latency, or benchmark results in production.
- UDP v1 and authenticated loopback WebSocket transport are implemented. Achievable end-to-end throughput remains unmeasured and must be benchmarked on target hardware.

## Brand Commitments
- Product name: SysX.
- Interface language: English.
- Visual direction requested: dark, minimal, professional, technical.
- Keep the native monitor window limited to Preview, device/connection status, and mouse-button states.
- Put configuration in the native Settings window on one vertically scrolling page, with a global Apply Changes action. Include Main, Motion, Auto, Recoil, signed X/Y target offsets with a target-relative position indicator, and FOV settings.
- Keep Preview always visible and the FOV overlay always enabled, with a 1–360° control in native Settings.
- Cap the square Preview viewport at 640 × 640 CSS pixels; smoothly enlarge low-resolution sender frames while preserving their aspect ratio.
- Let the Settings window accommodate additional options through vertical scrolling.
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
