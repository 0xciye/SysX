# CapkfaPlus reconstruction mapping

The files in `~/Downloads/CapkfaPlus-logic-and-reconstructions (1)` are analyst reconstructions of selected binary paths, not original vendor source. This document preserves the analysis, but only behavior with a supported producer and clear fit in SysX is active. No C++ file is copied into the application.

## Integrated and adapted

| Reconstruction | Rust implementation | Treatment |
|---|---|---|
| `DetectionMode.cpp` | `rust-backend/src/results.rs`, `pipeline.rs` | Dispatches only the two supported SysX strategies: Pixel Seek and Object Detection. |
| `PixelSeekMask.cpp` | `rust-backend/src/pixel_seek.rs` | Applies the code-defined red/yellow/purple RGB-to-HSV profiles. |
| `PixelSeekMode5Capture.cpp` | `rust-backend/src/crop.rs`, `receiver.rs` | Uses a validated centered square crop and rejects undersized frames. |
| `PixelSeekTargetSelection.cpp` | `rust-backend/src/targeting.rs`, `pipeline.rs` | Selects the topmost accepted mask pixel inside SysX's configured FOV. The reconstructed center scan/fallback is not substituted because SysX already defines its target rule and the source's mode-specific radii are not available. |
| `ObjectDetectionPreNms.cpp`, `ObjectDetectionNms.cpp` | `rust-backend/src/inference.rs` | Decodes supported raw YOLOv8 tensors, applies registry confidence/IoU, then performs class-aware NMS. This follows the ONNX bundle output instead of copying the reconstruction's decoded 6/7-column row contract. |
| `ObjectDetectionTargetSelection.cpp` | `rust-backend/src/targeting.rs` | Uses center-ellipse/FOV eligibility, nearest box center, prior-box coverage preference, and the existing class anchor. Unavailable class-group maps and veto settings are not invented. |
| `DetectionDataQueuePublish.cpp`, `LogicManagerTargetPipeline.cpp` | `rust-backend/src/pipeline.rs`, `service.rs` | Uses bounded latest-frame processing, drops obsolete input, and publishes the selected target as a post-detection result. |
| `TargetMotionSmoothing.cpp`, `TargetOffsetBypass.cpp` | `rust-backend/src/movement.rs`, `src-tauri/src/movement.rs` | Preserves the supported NCAF, Bézier response, residuals, and sensitivity/DPI conversion. The reconstructed effective-mode and direct-offset branches are not active because SysX has no corresponding state producer. |
| `BufferedVerticalAdjustment.cpp` | `rust-backend/src/movement.rs`, `src-tauri/src/movement.rs` | Samples configured levels by elapsed time, clamps the index, applies the percentage and integer truncation, then adds the result to Y while the selected physical trigger is held. |
| `FrameCenterAndRadialScale.cpp` | `rust-backend/src/movement.rs`, `targeting.rs` | Derives center from the actual crop dimensions and keeps FOV independent. Binary radial/divisor inputs are not guessed. |
| `FrameTimingAverage.cpp` | `rust-backend/src/movement.rs`, `pipeline.rs`, `service.rs` | Ports the 30-sample average helper. Live telemetry continues to use SysX's one-second source/processing windows. |
| `TargetOutputBackend.cpp` | `src-tauri/src/movement.rs`, `MediusAPI.md`, `FerrumAPI.md` | Sends relative motion through the documented Medius client or Ferrum KM API. |
| `PreviewRgbToRgba.cpp` | `rust-ui/src/preview.rs` | Converts/paints RGB frame data in the Rust preview path. |
| `SerialMoveQueueSubmit.cpp`, `SerialQueuedCommandApi.cpp`, `SerialQueuedCommandConsumer.cpp` | `src-tauri/src/serial_queue.rs` | Ports type-0 queueing and completion ownership behind an async writer interface. Command bytes stay alive until completion; `request_ms` is the writer's per-command write budget, not an end-to-end deadline. Wired to Ferrum (`km.move` on a cloned port) and Medius (`move_rel` on a cloned `Device`) through `ThreadedSerialWriter`; not validated on hardware. Nonzero kinds remain unsupported. |

## Explicit movement state

`CallerModeAndTimedAction.cpp` describes a timed move, left-button press/release, optional inverse move, and settle delay. SysX does not expose or execute this branch: its active-state, feature, and special-status producers are unavailable in the current source. The UI/runtime retains the supported recoil profile and adds its sampled value to Y while the configured physical trigger is held. The mode and timed-action reconstruction remains documented here for reference only.

`SerialPortSetup.cpp` and `SerialLineExchange.cpp` describe another program's serial setup and echo/reply behavior. SysX uses the documented device paths instead: the Medius Rust client at its fixed official rate, and Ferrum Legacy CP210x at its documented rate and verified `km.version()` handshake. The reconstructed KM queue is not used as a Medius protocol.

`UdpMethodBackendSetup.cpp` and `UdpMovePacketDispatch.cpp` describe an incomplete output packet format with no verified device exchange. SysX reserves UDP for incoming OBS MJPEG frames and does not use those packets for Medius or Ferrum movement.

`UiCursorPlacement.cpp` and the reconstructed axis-lock path are not part of SysX's relative-device-output workflow.

## Verification limits

Rust unit tests cover the translated pure algorithms and the serial queue's fake writer lifecycle and its threaded writer (timeout retry, partial writes, budget expiry, Medius record round trip). They do not validate the C++ reconstructions against the original binary, a real model, a Medius/Ferrum device, Windows, or hardware movement. See [`DEVELOPMENT.md`](../DEVELOPMENT.md) for current build and runtime evidence.
