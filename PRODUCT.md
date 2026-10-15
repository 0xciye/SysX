# Product

<!-- impeccable:product-schema 1 -->

## Platform
adaptive

## Stack
Tauri 2 + React + TypeScript desktop frontend with a separate Python backend sidecar. The frontend is packaged as an installed desktop application; it is not a browser-hosted service.

## Users
Developers or operators monitoring a continuous computer-vision stream between a sender computer and a receiver computer.

## Product Purpose
Receive image frames on a secondary computer, process them using Pixel Seek or Object Detection, and show a large realtime preview with trustworthy diagnostics. Success means operators can see the current stream and processing health without manually starting a session.

## Positioning
A receiver-only desktop tool that waits for a sender and automatically previews cropped MJPEG frames as they arrive.

## Operating Context
Used on a secondary computer while another computer continuously sends frames. The operator monitors preview, active strategy/model, and processing diagnostics. Target stream rates may exceed 180–360 FPS; this is a requirement to evaluate, not a measured performance claim.

## Capabilities and Constraints
- Develop on macOS; target an installed Windows 10/11 x64 desktop distribution.
- Work locally without a cloud service or public internet connection.
- Use a Python vision backend independent from the desktop frontend.
- Support exactly two primary strategies: Pixel Seek and Object Detection.
- Pixel Seek selects one target color at a time (yellow, red, or purple); HSV thresholds are maintained in code and remain unconfigured until supplied.
- Pixel Seek rules support optional normalized region-of-interest bounds; absent bounds cover the full source frame.
- Wait for sender input automatically; show a waiting state before frames arrive and start preview on receipt. No Start/Pause/Stop controls in the primary workflow.
- The sender's cropped MJPEG frames are the only production input. The receiver automatically waits for frames over its versioned UDP protocol; the complete OBS sender plugin is out of scope and follows `docs/OBS_RECEIVER_PROTOCOL.md`.
- Prioritize custom Ultralytics YOLO `.pt` detection models supplied by the user; do not download default models.
- Pixel Seek must support arbitrary valid input resolutions; AI input resolution follows each selected model's configuration.
- Do not invent detections, FPS, latency, or benchmark results in production.
- UDP v1 and authenticated loopback WebSocket transport are implemented. Achievable end-to-end throughput remains unmeasured and must be benchmarked on target hardware.

## Brand Commitments
- Product name: SysX.
- Interface language: English.
- Visual direction requested: dark, minimal, professional, technical.
- Make Settings the primary workspace, with General and FOV destinations inside Settings navigation.
- Offer Preview as an on/off display option on General; hiding the pane must not stop frame reception or processing.
- Keep the FOV overlay always enabled, with its 1–360° control on the dedicated FOV settings page.
- Cap the optional Preview viewport at 480 × 480 CSS pixels; smoothly enlarge low-resolution sender frames while preserving their aspect ratio.
- Let the Settings column accommodate additional options through vertical scrolling inside the window.
- User-provided screenshot is a reference for the preview and compact HUD layout; do not copy it literally.

## Evidence on Hand
- User-supplied screenshot reference in the conversation.
- No trained AI model files, sender implementation, or real performance measurements are present in the repository.

## Product Principles
- Keep the incoming frame as the visual focus.
- Show the sender-waiting and connection-error states clearly.
- Prefer measured telemetry over decorative charts or implied benchmarks.
- Keep frame acquisition and inference independent from UI rendering.
- Keep the preview in a clear waiting state until cropped sender frames arrive.
