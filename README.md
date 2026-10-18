# SysX

The Windows portable test build uses `scripts/build-windows.ps1` with `ort/load-dynamic` for the retained, unavailable Object Detection adapter. Pixel Seek does not require an ONNX Runtime installation. Build and packaging do not require or run VMProtect; all built-in release security and integrity checks remain enabled. VMProtect is applied manually by the user if desired.

> **Current availability:** Pixel Seek is the only usable detection engine. Object Detection and its ONNX Runtime, OpenVINO and TensorRT integrations are **not ready for use**. Their source/dependencies remain for development; build and unit-test results are not functional validation. SysX supports **Medius only** through its Rust SDK.

SysX supports a portable Windows desktop application (`SysX.exe`) and now has a native macOS `.app` build path. The macOS build targets the architecture of the Mac that builds it; its Rust code type-checks for Apple Silicon, but it has not yet been linked or run on macOS (the manual `Build SysX macOS app` workflow builds and tests it on a hosted Mac). The receiver and Settings are native Slint windows; the app does not open a browser or embed a WebView. Settings use the authenticated host connection for reading and applying configuration. Closing the receiver hides it and keeps SysX alive while another native window is open; after the native clients close it exits with a three-second grace period.

The application is Rust: the native host supervises an embedded Rust backend, renders Preview and Settings with Slint and owns Medius I/O. Configuration is edited in the native Settings window; there is no browser config page or config HTTP endpoint. Legacy Python, React, Leptos/WASM, egui and receiver WebView implementations are not part of the desktop app. Pixel Seek is the only usable engine; Object Detection integrations remain unavailable.

## Current status (2026-10-16)

Native receiver and Settings implementation: `src-tauri/src/monitor_window.rs` (Slint windows, settings editor and overlays) and `native_monitor.rs` (authenticated WebSocket state, bounded JPEG queue and background decode). Shared geometry/protocol types keep the source image and overlays aligned. Desktop builds do not launch or serve `/config.html`; the OBS sender remains the only production image source.

The desktop shell shows the always-visible Preview, Mouse State, device status, and receiver status. Preview displays “NO FRAME” while waiting for a sender and starts automatically when a valid frame arrives. The HUD contains only measured processing FPS, latency, and processing resolution. The circle-only FOV overlay is always enabled; its size is configured in the native Settings window and scales with the displayed image.

The native Settings window edits Color Filter, Pixel Seek color/FOV, target offsets, Motion/Auto settings, recoil, sensitivity, and UDP port on one scrollable page. It also exposes profile selection, create/clone, rename, and delete actions, backed by the persisted `app-settings.json` profile model. The middle mouse rising edge continues to cycle profiles. **Apply Changes** writes settings and applies supported runtime changes through the authenticated host connection. Native Settings are code-checked; visual/runtime behavior has not been opened during this work.

Current movement implementation: only the Normal controller and optional Normal + ABCurves renderer are available. Smoothing, speed, target selection and the Normal path remain unchanged.

Audit fixes: deleting a profile preserves surviving snapshots; failed settings writes show an error instead of a successful save. Auto controls are sent independently of video, so disabling Auto or changing its trigger/timing cancels a pending click even when frames stop. Cooldown remains the minimum click interval through target flicker and trigger rearming. Recovered source errors clear from live status. See [the audit fixes and validation limits](docs/SOURCE_AUDIT_FIXES.md).

The SysX mark is a custom crop-frame and focus-point SVG used by the native desktop assets.

The backend tracks source and inference resolutions, actual inference device, detection counts, measured source/processing/preview FPS, processing latency, input/transport drops, and the active profile. The native monitor header shows the active profile name and updates when settings change. The native monitor HUD intentionally shows only processing FPS, latency, and processing resolution. Source FPS counts valid decoded frames; processing FPS counts completed detections. Both use a trailing one-second window. The processing-FPS window retains up to 4096 completion timestamps; a regression test verifies its count is not capped at 360 FPS (synthetic timestamps, not a throughput benchmark). The preview returns to its sender-waiting state and clears the old image and live measurements when the stream goes idle. Once its authenticated backend connection is ready, the status reports “Waiting for sender.” A circle-only FOV overlay is always visible; its size is adjustable from 1–360°. Its diameter scales with the shorter side of the displayed image, and 360° fits inside that image at any sender resolution. Motion and Auto use one shared RGB-to-HSV profile registry and the selected Motion target color. Auto has its own FOV, and while its enabled trigger is held the backend uses Auto's FOV and the monitor colors that FOV ring blue. When released, Motion's green ring and FOV return.

The monitor window opens at 720 × 820 and adapts down to phone-like widths (feed and Mouse State shrink together; the port label hides below 520 px). Screenshots in `design-review/` predate the web monitor and are not current UI references; the new monitor has not been visually captured yet.

An earlier release-mode validation app with isolated app data received the 192 × 192 JPEG fixture on UDP port `4202` during an 8-second 360-FPS-target run. The sender measured 350.74 FPS and skipped 74 sender slots; the HUD showed 355.0 source/processing FPS, 60.0 preview FPS, 0 input/transport drops, and 0.04 ms processing latency. This is historical pre-Rust performance evidence; it does not validate the current Rust path, OBS, Windows, sustained sender throughput at 360 FPS, or target hardware.

Rust UI unit tests cover aspect-preserving preview scaling, letterboxed detection-box mapping, and HiDPI canvas sizing. Rust backend tests cover detection confidence, target selection, committing the ONNX/TensorRT/OpenVINO model bundle, and changing the UDP port then processing the actual JPEG fixture on the new port.

The unavailable Object Detection development path accepts one bundle containing `.onnx`, `.engine` or `.plan`, and an OpenVINO `.xml`/`.bin` pair. The ONNX Runtime adapter targets CPU; OpenVINO accepts CPU/GPU/NPU targets supported by the installed runtime; TensorRT is built only when TensorRT headers and `CUDA_PATH` are available and requires a GPU engine. The exported YOLOv8 model must accept NCHW RGB float input at exactly the configured crop dimensions and return raw `[1, 4+classes, anchors]` or `[1, anchors, 4+classes]` output. Confidence/IoU filtering and class-aware NMS run in Rust, then the existing FOV and prior-target selection runs. Crop/model resolution mismatch is reported instead of resizing. The Precision selector is saved as a preference; the selected artifact determines actual precision. The desktop UI has no Data or export section.

Object Detection and ONNX Runtime/OpenVINO/TensorRT are unavailable for use. Their retained model/Compute controls are disabled for profiles saved with Object Detection; select Pixel Seek to apply a usable profile.

The UDP receiver defaults to port `4200`; the backend suite verifies that changing the port moves the listener and that a subsequent frame is processed. Crop Resolution is configured in native Settings and sent to the OBS plugin; for example, `256` makes the plugin send a centered 256 × 256 region. The receiver processes each frame exactly as received (no second crop, never resized) and shows it in Preview. Socket failures retry every three seconds; corrupt JPEGs are dropped without closing the listener. Completed UDP frames are ordered by capture timestamp/frame ID so late old frames cannot regress tracking. Sender changes or a one-second valid-packet gap reset ordering for restarts. If another process owns a requested port, SysX keeps the current listener active and retries the bind; runtime status clears the bind error and updates when the port becomes available. Model controls are shown only in Object Detection mode. The preview HUD shows processing FPS; Preview itself is designed to draw every input frame.

The Rust sidecar owns UDP reassembly, JPEG decoding, processing of the centered crop supplied by the OBS sender, Pixel Seek, bounded latest-frame processing, settings persistence/runtime application, and authenticated preview transport. The release executable embeds the sidecar and extracts it under app data; the Windows build emits `SysX.exe` without an installer. The sidecar processed the 192 × 192 JPEG fixture once on UDP `4202` in an isolated service smoke run; this was not a native-window, performance, Windows, OBS, or hardware validation. Earlier 1280 × 720 synthetic rates were measured by the retired Python implementation and do not measure Rust. No GitHub Actions build workflow is configured. Windows runtime and target-hardware performance remain unverified. No user model is present.

## Requirements

- Windows 10/11 x64 or macOS 12+.
- Rust stable and the platform's native build tools (Windows MSVC, or Xcode Command Line Tools plus CMake on macOS; Intel Macs also need NASM). Node/pnpm are only needed for the package-script shortcuts; `bash scripts/build-macos.sh` can be run directly.
- Build each platform on its native OS; the macOS script builds the host architecture (Apple Silicon or Intel) and does not produce a universal binary.
- A Windows release is built with [`scripts/build-windows.ps1`](scripts/build-windows.ps1), yielding `src-tauri/target/release/SysX.exe`. The receiver and Settings are native Slint and need no WebView2 runtime. `pnpm build:desktop` builds the backend sidecar and native executable without Trunk.
- A macOS app bundle is assembled with `pnpm build:macos` (or `bash scripts/build-macos.sh`) on a Mac, yielding `dist/SysX.app`. The bundle is ad-hoc signed (not Developer ID) and not notarized, so copies downloaded from elsewhere need right-click → Open once; a locally built bundle opens normally. Allow incoming connections when macOS asks, so the OBS sender's UDP frames reach the backend. The Medius box needs a CH343 serial driver that exposes a `/dev/cu.*` port; USB/device behavior still needs validation on macOS.

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture, setup, build commands, and milestone status.

For sender integration, see [the OBS plugin and SysX protocol guide](docs/OBS_RECEIVER_PROTOCOL.md). It documents the implemented UDP v1 packet format, the `SYSCFG` crop control message, limits, and the implemented OBS sender plugin.

```sh
pnpm install
pnpm native:dev
```

Rust backend development and the current migration status are documented in `DEVELOPMENT.md`.

The Rust UDP verifier can receive packets and send the actual JPEG fixture:

```sh
cargo run --manifest-path rust-backend/Cargo.toml -- udp-listen --host 127.0.0.1 --port 4200 --count 1
cargo run --manifest-path rust-backend/Cargo.toml -- udp-send-image src/assets/red-cube-192.jpg --host 127.0.0.1 --port 4200
```

This utility verifies the frame protocol; the production sender is the OBS plugin in `obs-plugin/sysx-canvas-sender`.

## Product constraints

- The UI is English, dark, minimal, and technical.
- The native window is a live monitor; configuration is edited in the in-app Settings window. The active profile is shown in the monitor and follows saved profile changes live.
- Pixel Seek and Object Detection are the only detection strategies.
- User models are loaded only when selected; no model is downloaded automatically.
- Motion Pixel Seek selects exactly one code-profile color (Yellow, Red, or Purple). Auto reuses that selected HSV profile and has no color selector; blue identifies its FOV overlay only.
- Pixel Seek code profiles can restrict detection to a normalized source-frame region; with no region configured, detection uses the full frame.
- FPS, latency, detections, and frame-drop counts are always measured values. Prototype sample data is never used in production.
- The target of 180–360+ processing FPS is a requirement to benchmark, not a performance claim.
- Device I/O uses the official `medius 3.4.6` Rust SDK for the handshake, health checks, input subscription, and output. SysX enumerates Windows serial ports with the same CH343 VID/PID as the SDK, preserving scan errors separately from an empty device list, then tries each matching control port until its health reports `link_up`, `mouse_attached`, and `clone_configured`. It retries discovery every 500 ms when no matching control port is listed and records connection failures and state changes in the host log. The adapter in `src-tauri/src/medius_device.rs` calls `move_rel` and `press/release(Button::LEFT)`; queue records are internal work items, not Native wire frames. Ferrum and MAKCU integrations are removed. Windows/device validation remains pending.
- API implementation notes: [MediusAPI.md](MediusAPI.md). It summarizes SysX device connection, button telemetry, and relative-movement paths and index the broader vendor API families; consult the linked vendor pages for exact signatures and version support.
- Device output uses the verified Medius SDK connection, fresh-delta aggregation, 100 ms stale-target cutoff and ±32 per-axis clamp. Motion uses the held-trigger gate unless Always on is enabled; Always on suppresses Auto without erasing its saved configuration. Auto Delay/Cooldown/Sleep remain configured milliseconds. See [MediusAPI.md](MediusAPI.md); library call completion does not acknowledge a USB/game event.
- Movement settings persist the recoil profile. Recoil starts only while a fresh, valid Motion target exists and the physical left mouse button is held; it starts after its configured delay and advances by the configured time per level. It does not require the configured Motion trigger, but it never moves without a valid target. Reconstructed active-state and timed-click branches remain out of the product because their source producers are unavailable. Auto's separate target-valid state machine uses its persisted Delay, Cooldown, and Sleep values and sends a left-click through the verified device path when its trigger is held.
- The historical synthetic benchmark figures above predate the Rust migration. There is no current Rust end-to-end benchmark result.

## Sample frame

This 192 × 192 JPEG is the sample used by the UDP smoke test:

![192 by 192 red cube JPEG sender frame](src/assets/red-cube-192.jpg)

Motion Pixel Seek offers one target color at a time (yellow, red, or purple); Auto reuses the selected Motion HSV profile. The monitor uses blue for the Auto FOV ring while its trigger is held. Motion and Auto use the same shared HSV lookup tables in `rust-backend/src/pixel_seek.rs`: red H 0–10/S 50–255/V 180–255, yellow H 20–40/S 157–255/V 252–255, and purple H 140–160/S 60–255/V 240–255. The UDP receiver decodes JPEG frames to RGB before thresholding; hue uses 0–179, while saturation and value use 0–255.

The Rust sidecar has adapters for ONNX Runtime CPU, OpenVINO, and optional TensorRT. Runtime adapters compile and the decoder/selection behavior has unit coverage; the repository contains no model, so model inference, accelerator availability, Windows packaging, and device behavior remain unverified. CapkfaPlus reconstruction logic was translated selectively; see the development status for the mapped behaviors and limits.

The receiver and Settings are native Slint windows. The desktop does not start the former config HTTP server or WASM bootstrap.

Release hardening is documented in [`docs/RELEASE_SECURITY.md`](docs/RELEASE_SECURITY.md). The normal build produces a portable `SysX.exe` with native PDB debug symbols stored separately, LTO, Windows CFG/ASLR/NX, loader mitigations, release-only debugger guards, and SHA-256 verification of the extracted backend. Windows read locks and paced background integrity sweeps remain enabled; debug/test enforcement stays disabled. Build and packaging neither run nor require VMProtect. No reusable authentication credential is stored in the client.

The [continuity report](docs/SECURITY_CONTINUITY_REPORT.md) records a Linux synthetic release A/B test near 240 FPS with and without the file-integrity worker, including CPU/startup cost and the remaining Windows, physical-device and VMProtect limits. It is not a game/hardware benchmark or proof of zero overhead.

The Rust backend keeps UDP packet reassembly in `rust-backend/src/udp.rs`, decoded input in `receiver.rs`, and crop geometry in `crop.rs`. Native Settings are read and applied through the authenticated host connection; the backend exposes no web-config HTTP endpoint. The movement ticker and native host aggregate fresh deltas and discard targets older than 100 ms. Motion normally requires the configured physical trigger; Motion's **Always on** option bypasses that held-button gate while retaining the fresh-target and enabled-motion checks. Auto is unavailable while Always on is enabled, but its saved settings are preserved. The native host adds sampled recoil and clamps each axis to ±32. Only Medius receives output: internal queue records are retained until a worker invokes `medius_device.rs` through the official Rust SDK on a cloned `Device`. SDK I/O timeouts apply, and library call completion does not acknowledge a game event. Windows/device validation remains pending.

The current LAN sender DLL targets `192.168.88.59:4200` and is built separately from the receiver.

## Windows portable build validation (2026-10-16)

These results describe the earlier build recorded below. The native PDB/crash-diagnostics changes have not been rebuilt at the user's request; run the manual workflows to validate that output.

[Windows run](https://github.com/0xciye/SysX/actions/runs/38888803711) built the release EXE and passed 89 backend tests (4 ignored simulation reports), 11 UI tests, 39 native-host tests, UDP reassembly on 4200, and application launch/monitor-close/backend-shutdown smoke. The x64 PE has CFG, ASLR/high-entropy ASLR and NX enabled. The separate OBS DLL targets `192.168.88.59:4200`, passed its three-frame loopback/crop-reply check, and imports only Windows system DLLs. [Hashes and validation](docs/validation/windows-portable-2026-10-16.json). VMProtect was not applied. Visible rendering, physical Medius, real OBS graphics and LAN throughput remain unvalidated by these checks.

## Manual builds and crash diagnostics

Open **Actions** and choose **Build SysX EXE (manual)** or **Build OBS DLL (manual, receiver IP)**, then **Run workflow** on `main`. The DLL workflow has a `receiver_ip` field (default `192.168.88.59`, UDP 4200). Neither workflow runs automatically on push/PR. Successful outputs are saved under **Releases → draft release** instead of Actions artifacts.

Native host/backend release builds retain full PDB information (`debug = 2`, `strip = "none"`) with LTO and the release security guards still enabled. The EXE workflow creates separate app and symbols ZIPs; the DLL workflow produces only the image-sender DLL, without PDBs or custom diagnostics. Keep the matching PDB archive for each exact build. PDBs are not required on a computer that only runs the app. The manual workflow summary reports actual EXE/ZIP sizes; no new size is known until a build runs. The desktop build no longer embeds or strips WASM settings assets.

SysX writes diagnostic text to `%LOCALAPPDATA%\SysX\logs\` (`host-*.log`, `backend-*.log`, `backend-stderr-*.log`). Rust panic reports include a backtrace; unhandled Windows exceptions include a code/address. Each caught Rust panic or unhandled Windows exception also writes a crash report to `%LOCALAPPDATA%\SysX\crashes\*-crash.txt` containing the reason and every log created during that process session. Best-effort Windows minidumps are saved beside it as `*.dmp`. SysX records normal shutdown reasons and watches for an unexpected backend exit. If the host is terminated before it can record a reason, the next launch writes an `*-unclean-exit.txt` with all logs from the previous session and explicitly states that the exact cause is unknown. Logging is confined to startup, shutdown and error/crash paths, not per-frame or per-move loops. Open a dump with WinDbg and the PDBs from that exact build. Forced termination, power loss, severe corruption, or a replaced exception handler may prevent a report or dump.

OBS owns crashes inside its process: check `%APPDATA%\obs-studio\crashes\` and `%APPDATA%\obs-studio\logs\`. The sender adds no log or crash handler.


Earlier web-config migration checks are historical and do not validate the current in-app Settings changes. Current verification is limited to Rust compilation; no UI runtime, game, Windows packaging, physical OBS/Medius, or LAN validation has been run for this overhaul.


### Pixel Seek Motion / Auto contract correction (2026-10-16)

Source changes based on `2fe571eca08bcd7fbf9f9301b15ef6c52368de06`:
Movement IPC uses snake_case on both ends; the host also accepts explicit legacy
camelCase aliases and rejects missing required movement fields. Recoil/config
remain camelCase. The wire regression serializes the actual backend MovementEvent
and feeds the actual host parser, pending queue and trigger/Always-on gates.

Motion Always-on retains priority over Auto. Otherwise an enabled, held Auto
trigger owns Pixel Seek processing: only the shared color mask feeds center
validity; no Motion target, smoothed coordinates or reticle are published. The
movement ticker independently gates Motion while Auto owns routing. Each frame
carries Auto active intent and its selected FOV, so the native circle/reticle do
not depend on the center-valid flag or a newer state snapshot. Motion and Auto
FOVs remain separate pipeline inputs. Auto still fires only at center crossings;
its FOV circle does not authorize clicks elsewhere in the circle. Always-on moves
now use the same applied-move calibration bookkeeping as held-trigger moves.

Follow-up source review also fixed the base commit's incomplete MovementEvent
unit-test helper. A shared control revision now rejects callbacks, samples and
queued deltas across routing changes; both batching stages avoid summing across
revisions. The host checks the current control revision and physical Auto trigger
before allowing Motion. Always-on publishes Auto disabled immediately, cancelling
pending delayed Auto clicks even without another frame.

Local Linux x86_64 validation with Rust 1.99.1: backend 97 unit tests passed
(4 existing diagnostic/report tests ignored), plus 26 integration tests; UI 11
tests passed. The host compiled locally and its native SVG overlay routing test
passed (one test, no window/device launch). Linux libudev development metadata was
provided outside the repository for this check. The integration set includes the
real backend serializer and host movement/Auto modules. The settings-to-pipeline FOV/overlap regression also passed
separately. These checks do not validate Windows, OBS, physical Medius behavior,
calibration accuracy on hardware or production performance. Validation did not dispatch a build workflow, merge changes or issue device
commands.


### Optional ABCurves texture (2026-10-17)

Main retains the confirmed Normal controller and Auto source from
`stable/motion-auto-2026-10-17` (`a7d7a5b`); preserve that branch. Normal is still
the default and bypasses model/resampling calls. The Motion card can opt into
**Normal + ABCurves (experimental)** and Apply Changes to automatically collect
256 consecutive physical mouse reports at 1 ms through a dedicated Medius SDK
subscription. There is no profile import UI. This adds the pinned upstream MIT C99 Renderer,
not a replacement planner or Python runtime. Profile preparation is off the ticker;
output is numerically rendered at 1 ms then aggregated through the existing
roughly 4 ms batching/device path. There is no physical 1 kHz output guarantee.
The sampler checks chip timestamps and queue loss; it never pads absent reports.
Move continuously with a 1,000 Hz mouse; pauses/gaps restart collection. Normal
runs until profile preparation and conditional disk persistence both succeed.
Profiles are saved locally per settings profile and reused after restart. Switching
profiles/settings, disabling the option or disconnecting cancels pending capture.
Background completion does not reset detection or cancel pending Auto clicks. Do not fabricate a profile from missing events or generated
movement. Renderer intent resets on inactive/stale/lost targets, revision changes
and long scheduling gaps; runtime errors clear movement validity and surface in
existing error status. Normal, target selection, Auto, native clamps and vendor
output are unchanged. See [ABCurves integration](docs/ABCURVES_INTEGRATION.md) for
profile schema, provenance, license and validation limits.

Local validation including automatic capture: backend 112 unit + 42 integration
passed (2 existing ignored), UI 14 tests and WASM check passed, host 61 tests
passed with test-only DejaVu fontconfig, upstream C tests 2 passed. No Windows workflow or physical devices
were exercised. Tests and synthetic feedback checks do not establish in-game
quality for the optional mode.
