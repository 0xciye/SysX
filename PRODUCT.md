# Product

<!-- impeccable:product-schema 1 -->

## Platform
adaptive

## Stack
Tauri 2 + React + TypeScript native monitor window with a separate Python backend sidecar and a loopback-only browser configuration page. The monitor is packaged as an installed desktop application; configuration runs in the user's separate browser.

## Users
Developers or operators monitoring a continuous computer-vision stream between a sender computer and a receiver computer.

## Product Purpose
Receive image frames on a secondary computer, process them using Pixel Seek or Object Detection, and show a large realtime preview with trustworthy diagnostics. Success means operators can see the current stream and processing health without manually starting a session.

## Positioning
A receiver-only desktop tool that waits for a sender and automatically crops incoming full-frame MJPEG at the receiver and previews the crop.

## Operating Context
Used on a secondary computer while another computer continuously sends frames. The operator monitors preview, active strategy/model, and processing diagnostics. Target stream rates may exceed 180–360 FPS; this is a requirement to evaluate, not a measured performance claim.

## Capabilities and Constraints
- Develop on macOS; target an installed Windows 10/11 x64 desktop distribution.
- Work locally without a cloud service or public internet connection. The browser configuration page is served only from loopback.
- Use a Python vision backend independent from the desktop frontend.
- Support exactly two primary strategies: Pixel Seek and Object Detection.
- Pixel Seek selects one target color at a time (yellow, red, or purple); HSV thresholds are maintained in code.
- Crop Resolution in web config sets a centered `N × N` receiver crop. Pixel Seek rules support optional normalized region-of-interest bounds in original-frame coordinates; absent bounds cover the full source frame.
- Wait for sender input automatically; show a waiting state before frames arrive and start preview on receipt. No Start/Pause/Stop controls in the primary workflow.
- Keep the native window focused on Preview, five mouse-button states, device state, and receiver connection state. Preview is always visible; its compact HUD shows processing FPS, latency, and processing resolution.
- Open a separate browser window to the local configuration page when the desktop app starts. Persist settings under application data and restore them at backend startup.
- Full-frame MJPEG from the OBS sender is the production input. The receiver applies the configured centered square crop before detection and Preview; see `docs/OBS_RECEIVER_PROTOCOL.md`.
- Prioritize custom Ultralytics YOLO `.pt` detection models supplied by the user; do not download default models.
- Pixel Seek must support arbitrary valid input resolutions; AI input resolution follows each selected model's configuration.
- Do not invent detections, FPS, latency, or benchmark results in production.
- UDP v1 and authenticated loopback WebSocket transport are implemented. Achievable end-to-end throughput remains unmeasured and must be benchmarked on target hardware.

## Brand Commitments
- Product name: SysX.
- Interface language: English.
- Visual direction requested: dark, minimal, professional, technical.
- Keep the native monitor window limited to Preview, device/connection status, and mouse-button states.
- Put configuration in the separate local browser page, with General and FOV settings and a global Apply Changes action.
- Keep Preview always visible and the FOV overlay always enabled, with a 1–360° control in the local configuration page.
- Cap the Preview viewport at 640 × 640 CSS pixels; smoothly enlarge low-resolution sender frames while preserving their aspect ratio.
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
