# Development

## Architecture decision

Use **Tauri 2 + React + TypeScript** for the installed desktop window and a separate **Python sidecar** for frame acquisition, detection, and persistence.

The monitor is bundled as a native desktop window and rendered through the operating system's WebView (WebView2 on Windows and WKWebView on macOS). Configuration runs in a separate browser window against a loopback-only page served by the Python sidecar; it is not public or cloud hosted. Tauri fits the requested desktop distribution while preserving the independent Python engine. Neither framework by itself guarantees a 180–360 FPS processing rate; that depends on frame size, model, device, and bounded pipeline behavior and must be benchmarked.

For Windows offline installation, the release build should bundle the WebView2 offline installer. This adds roughly 127 MB to the installer per current Tauri documentation. Windows installers and PyInstaller sidecars must be built on Windows CI, not cross-compiled from macOS.

References:

- [Tauri process model](https://v2.tauri.app/concept/process-model/)
- [Tauri sidecar binaries](https://v2.tauri.app/develop/sidecar/)
- [Tauri Windows installer and WebView2 modes](https://v2.tauri.app/distribute/windows-installer/)
- [Electron process model](https://www.electronjs.org/docs/latest/tutorial/process-model)
- [Electron IPC](https://www.electronjs.org/docs/latest/tutorial/ipc)

## Runtime responsibilities

### Brand assets

- `public/receiver-mark.svg` is the canonical SysX mark. Keep the title-bar mark and the Tauri PNG/ICO exports visually aligned with it.
- `src-tauri/tauri.conf.json` lists the PNG and ICO used by desktop bundles.

### Desktop frontend

- Keep the native monitor window focused on Preview, Mouse State, device state, and receiver connection state. Preview is always visible; its HUD shows only processing FPS, latency, and processing resolution.
- Put settings in the separate browser page, with General and FOV sections and one global Apply Changes action. Persist settings in app data and restore them when the backend starts.
- Cap Preview at 640 × 640 px; preserve aspect ratio and enlarge smaller frames with smooth or nearest-neighbor scaling.
- Keep a circle-only FOV overlay always enabled; expose its size as a 1–360° setting in the local config page, with no enable/disable control. Scale its diameter against the shorter side of the displayed image so it stays proportional across source resolutions; 360° is the largest inscribed circle.
- Reflect backend IPC readiness in the connection status; show “Waiting for sender” after the authenticated socket connects and reserve “Starting receiver” for startup.
- Retry frame-source runtime failures every 3 seconds. Retain the active listener while a requested port is occupied, then switch to the requested port and clear its error after a successful bind. Startup bind failures and later UDP read/decode failures retry until the listener returns.
- Let the local configuration page scroll vertically as additional settings are added.
- Keep backend, precision, and compute preferences disconnected from inference runtime selection.
- Send receiver, crop, and detection configuration to the backend. The sender provides full-frame MJPEG; the receiver takes a centered square crop before detection and publishes that cropped image to Preview.
- Default the UDP listener to port `4200`. Show model controls (including import) only in Object Detection mode. Do not add a Data or export section to the desktop UI.
- Object Detection's Min confidence threshold ranges from `0` to `1` and filters which detections are drawn in Preview.
- Place Pixel Seek's Yellow/Red/Purple target-color selector in a separate full-width row below the Detection Engine cards; show it only in Pixel Seek mode.
- Render Pixel Seek detections as boxes tinted with the selected target color, without AI class/confidence labels. Object Detection filters by minimum confidence, draws stable per-class boxes, and shows `[id] score% | ...` in a compact summary badge.
- Never run inference on the UI thread.

### Python backend

- UDP receiver for successive full-frame JPEG images (MJPEG); require JPEG SOI/EOI markers before OpenCV decoding so PNG or other formats cannot pass as sender frames. Apply the configured centered square crop after decode and before processing.
- Validate SOI/EOI on UDP boundary chunks before adding payloads to a reassembly, and discard any existing partial frame if a boundary marker is invalid.
- Bounded latest-frame pipeline.
- `DetectionStrategy`, `PixelSeekStrategy`, `ObjectDetectionStrategy`, and `StrategyFactory`.
- Shared detection result schema and storage.
- Pixel Seek target profiles for yellow, red, and purple are code-defined in `receiver_backend.service.PIXEL_COLOR_PROFILES`; only the selected color's profile reaches the processing pipeline. Decoded BGR frames are converted to RGB and then HSV. The OpenCV ranges are red H 0–10/S 50–255/V 180–255, yellow H 20–40/S 157–255/V 252–255, and purple H 140–160/S 60–255/V 240–255. `PixelColorRule.roi` optionally restricts a rule to normalized source-frame coordinates `(left, top, right, bottom)`; `None` means the whole frame.
- Model registry removal deletes only its YAML entry, never the user's model file. Removing the active model while Object Detection is selected returns processing to Pixel Seek; removing an inactive model keeps the active strategy/model.
- Optional Ultralytics integration for user-provided `.pt` models only.
- The supplied DetectionMode reconstruction matches the two `StrategyName` branches; unsupported values are rejected by the enum parser. Pixel Seek applies the supplied inclusive RGB-to-HSV mask thresholds. Object Detection passes configured confidence and IoU thresholds to Ultralytics and keeps class-aware NMS (`agnostic_nms=False`). Its 6/7-field raw-row decoder stays in Ultralytics because the current `.pt` path returns decoded, NMS-filtered boxes and owns coordinate restoration. The recovered mode-5 center crop is applied before either strategy; crop-relative detections are translated to original-frame coordinates.
- Sidecar lifecycle managed by the Tauri process.

Implemented backend modules include a YAML model registry, shared detection types, Pixel Seek, lazy Ultralytics inference, UDP reassembly, a one-pending-frame worker, and asynchronous JSONL persistence. The desktop receives uncropped MJPEG frames, crops them before processing, and shows the crop in Preview. Tauri manages the backend process; the frontend uses a typed command channel and authenticated loopback WebSocket for state, configuration, diagnostics, and binary preview frames.

### Frame flow

1. Receive and validate a packet/frame.
2. Reassemble UDP frames with bounded memory and timeout.
3. Keep queues bounded and discard stale frames under load.
4. Process every frame still admitted to the processing queue.
5. Persist detection output asynchronously.
6. Publish only the latest preview frame at a display-friendly cadence; report processing FPS and preview FPS separately.

### Target selection and mouse output

- `receiver_backend.targeting` applies Object Detection score gates and reconstructs center-ellipse eligibility, nearest box-center ranking, previous-box coverage preference, and class-dependent vertical anchor. Class-group configuration is unavailable and remains unset.
- Detection runs over the full processed crop. FOV filters only downstream targeting candidates; Pixel Seek chooses the highest accepted mask pixel inside the configured centered FOV, while full-frame detections remain available for Preview. The selected point is sent with frame metadata and Preview draws a center-to-target line and ring.
- Pixel Seek's source-specific center-bounded scan and upper-half fallback, and LogicManager error/offset/divisor interpolation remain extension points because their complete algorithms and parameters are unavailable.
- Fractional relative deltas retain sub-pixel residuals before integer conversion. Axis locks and command/method mouse transport are typed skeletons only; no mouse injection is wired to the device monitor.
- `receiver_backend.serial_command_queue` models the recovered type-0 async serial handoff: it keeps immutable command bytes and completion state alive until the writer callback retires them, then reports completion so a caller can resume draining. The actual Win32 `WriteFile`/IOCP adapter, command producer, nonzero timer variants, and vendor output wiring remain unimplemented; `request_ms` is metadata, not a deadline.
- Do not expose these paths as active controls or claim movement support until the source logic, runtime configuration, device adapters, and validation are available.

The development launcher uses the project Python environment. The Windows build script creates a PyInstaller backend executable and packages it with Tauri. UDP is reserved for sender frames. Large frame payloads use binary WebSocket messages with small metadata rather than repeated JSON serialization.

The Tauri host runs a background device monitor independently from the Python vision backend. It checks Medius devices using the official `medius` Rust client (CH343 USB identity followed by the protocol handshake and version/name read; the crate uses its fixed 6,000,000 baud setting), then listens for the five physical button edges with `input_events`. The current Ferrum probe opens Silicon Labs CP210x at the documented Legacy API baud (115,200), verifies `km.version()`, and parses button callback payloads as raw five-bit values, not UTF-8 text. These are Medius/Ferrum rates; do not use Makcu baud values. The App-managed Ferrum Software API remains preferred; its configured serial endpoint is not discoverable from the public docs and is not yet wired. The header caches the device state and displays Left, Right, Middle, Mouse4, and Mouse5; values stay unknown until an event arrives. This is read-only physical telemetry and sends no mouse/key injection. Ferrum endpoint/firmware compatibility and both vendors' hardware behavior are not yet validated on Windows in this workspace.

No GitHub Actions build workflow is configured. The Windows x64 build and smoke-test scripts remain available for manual validation: they build the Python sidecar, verify UDP `4200` and 192×192 frame receipt, package the Tauri app, silently install the NSIS artifact, check installed app/backend startup and shutdown, and uninstall it. The PowerShell build script explicitly checks each native command's `$LASTEXITCODE`; `$ErrorActionPreference` alone does not make every native nonzero exit terminating.

## Build prerequisites

- Node.js 20+ and pnpm.
- Rust stable and the Tauri platform prerequisites.
- Python 3.11+ for the backend.
- For Windows release builds: Windows x64 runner, WebView2 offline installer, and a Windows-built Python sidecar.

## Commands

Build and launch commands from the project root:

```sh
pnpm install
pnpm tauri dev
pnpm build
pnpm tauri build
```

Python setup and checks from the repository root:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -e 'backend[dev]'
.venv/bin/python -m pytest backend/tests
```

Local UDP protocol verification:

```sh
.venv/bin/receiver-backend udp-listen --host 127.0.0.1 --port 4200
.venv/bin/receiver-backend udp-send-image path/to/image.png --host 127.0.0.1 --port 4200
```

Receiver backend loopback benchmark (synthetic JPEG fixture; includes UDP packetization/reassembly, JPEG decode, Pixel Seek, and binary WebSocket preview transport; excludes OBS capture, Tauri renderer, Windows, and production HSV profiles):

```sh
.venv/bin/python scripts/benchmark_receiver_udp.py --width 1280 --height 720 --fps 360 --duration 3
```

Current repeat runs on macOS 15.3.1 ARM64 used 1280×720 JPEG frames (16,634 bytes, 15 datagrams each) for 3 seconds. At 180 requested sender FPS, sender/source/processing measured 179.95/179.93/180.05 FPS. At 360 requested FPS, they measured 359.99/359.81/359.79 FPS. Both runs had 0 input drops, transport drops, invalid packets, and rejected frames, with 181 WebSocket preview frames. An earlier 360 FPS run measured 348.4 processing FPS and 4 input drops, showing that throughput varies by run. These are local synthetic backend measurements, not Windows, OBS, target-network, production-profile, or Tauri-renderer validation.

Pixel Seek strategy-only microbenchmark (synthetic full-mask fixture; not an end-to-end application or hardware acceptance test):

```sh
.venv/bin/python scripts/benchmark_pixel_seek.py --width 1280 --height 720 --frames 3000
```

The fixture HSV range is intentionally synthetic and is not installed in `PIXEL_COLOR_PROFILES`. The strategy-only result excludes UDP, JPEG decoding, storage, Object Detection, Tauri, and UI preview. Measure those separately before making a system throughput claim.

Use `0.0.0.0` as the listener host only when intentionally accepting frames from other computers on the network. The verifier reports frames it actually receives; it does not measure inference throughput.

The OBS sender is out of scope for the current application. Its interoperable Receiver contract and implementation limits are documented in [`docs/OBS_RECEIVER_PROTOCOL.md`](docs/OBS_RECEIVER_PROTOCOL.md). Keep that document synchronized with `receiver_backend.udp` if the wire protocol changes.

## Milestones

1. [x] Product direction and desktop layout recorded.
2. [x] Single-screen Preview and Settings layout refined from the supplied reference; rejected early concept sketches have been removed. Older captures in `design-review/` show previous UI states; `receiver-visual-qa-980x680.png` records the latest layout-only native review.
3. [x] Tauri/React desktop shell with Preview and Settings together on one screen.
4. [x] Python backend lifecycle, typed local commands, and authenticated preview transport; Windows packaged runtime still needs validation.
5. [x] Shared detection schema and Pixel Seek strategy.
6. [x] Model registry and optional user-supplied Ultralytics `.pt` inference (model-runtime test remains blocked on a user model).
7. [x] Versioned UDP receiver integrated with the desktop preview; full-frame MJPEG from the sender is the desktop input source.
8. [x] JSONL/CSV persistence module, bounded processing worker, and application diagnostics.
9. [x] Backend and frontend checks (50 backend tests and 11 frontend tests pass; UI integration coverage remains limited).
10. [x] Windows x64 build scripts and UDP sidecar/desktop executable lifecycle smoke tests; [ ] manual Windows build, NSIS installation, and visual application validation. GitHub Actions build workflow removed.
11. [x] Impeccable and UI UX Pro Max review: developer-tool dark/minimal direction and keyboard/focus guidance checked; the earlier Impeccable detector returned `[]`. Existing `design-review/` captures predate the current UI and are historical only; current native review has been performed, while Windows visual review remains pending.
12. [x] Preview HUD reports source and inference resolution, measured performance, and input/transport drops; `pnpm build` passes.
13. [x] UDP defaults to port `4200`; Pixel Seek hides model configuration and the desktop UI has no Data/export section. Frontend build/tests and backend tests pass.
14. [x] Final desktop screenshot review increased settings switch hit targets to 24 px; Impeccable detector returned no findings and frontend/backend checks pass.
15. [x] Preview overlay coordinate transforms and HiDPI backing dimensions extracted into tested helpers; frontend build, seven frontend tests, and detector pass.
16. [x] Explicit accelerator availability validation and platform-aware MPS option; frontend build, seven frontend tests, and 41 backend tests pass.
17. [x] Runtime inference device added to result metadata, HUD, JSONL, and CSV; frontend build, seven frontend tests, 41 backend tests, and detector pass.
18. [x] Measured valid source FPS added alongside processing/preview rates; incomplete UDP frame assemblies expire while the socket is idle. Backend suite: 43 passed.
19. [x] Streaming HUD reads FPS from each frame, and clears live telemetry and detection overlays when the receiver returns to waiting; frontend build and 7 frontend tests pass.
20. [x] Synthetic UDP benchmark reports measured source FPS. At 1280×720 it processed 180 FPS with no drops at the 180 target and 348.4 FPS with 4 input drops at the 360 target; Windows/Tauri/OBS scope remains unverified.
21. [x] Live macOS Tauri preview verified during a continuous 192×192 UDP sample stream: 180.2 source FPS, 180.0 processing FPS, 60.1 preview FPS, and 0 input/transport drops. Windows and OBS sender behavior remain unverified.
22. [x] Windows packaging scripts support local builds and smoke checks; no GitHub Actions workflow is configured.
23. [x] Center FOV overlay is a permanent circle outline sized with a 1–360° control. Its diameter scales against the shorter side of the displayed image, so all source resolutions share a proportional overlay and 360° remains within the image. Frontend build passes; automated frontend coverage checks the ratio and bounds.
24. [x] Replaced the four-square title mark with a custom crop-frame/focus-point SVG and generated matching 512 × 512 PNG and multi-size Windows ICO bundle assets. Packaging configuration now references both icons; Windows installer appearance remains unverified until manually checked on Windows.
25. [x] Min confidence is now visible whenever Object Detection is selected, including before a model is imported or activated; the threshold is applied to Preview and is saved to the active model when one exists.
26. [x] Detection Engine selection now uses side-by-side Pixel Seek/Object Detection radio cards following the user reference; Object Detection Min confidence is a 0–1 slider.
27. [x] Pixel Seek target colors and HSV-profile status appear inside Detection Engine; the controls are hidden while Object Detection is selected.
28. [x] Fixed the macOS build script's Tauri argument forwarding and built `src-tauri/target/release/bundle/macos/Receiver.app` with the architecture-specific Python sidecar. The frontend built successfully; the app bundle is about 300 MiB.
29. [x] Fixed Tauri's bundled sidecar filename lookup, cleared recovered UDP bind errors after a successful port change, and cleared the preview canvas when the stream goes idle. The packaged macOS app received a continuous 192×192 UDP stream at 120 source/processing FPS and 60 preview FPS with 0 input/transport drops. Quitting the app stopped the PyInstaller worker process group. Backend, frontend, and Tauri unit suites passed.
30. [x] Current minimum-width review found the Object Detection card title truncating at 980×680. Detection Engine cards now stack below 1080 px; `pnpm build` and all 8 frontend tests pass. A fresh visual capture at the minimum size remains pending.
31. [x] Added a Windows CI smoke test that launches the packaged Tauri executable, confirms its bundled backend process starts, closes the app window, and checks backend shutdown. PowerShell and Windows execution are unavailable in the current macOS workspace; CI must validate the new check.
32. [x] Fixed Windows Tauri argument forwarding: removed the extra pnpm `--` that prevented `--config` from being parsed as a Tauri option. Confirmed locally that `pnpm tauri:build --help` exposes the Tauri build options; the Windows build and lifecycle smoke test still need a Windows CI run.
33. [x] Pixel Seek color selection now appears inside Detection Engine. Frontend build, 8 frontend tests, 44 backend tests, 1 Tauri unit test, and debug/release macOS app bundle builds pass. Native accessibility inspection confirms the updated grouping; a minimum-size visual capture, Windows CI/runtime, OBS interoperability, and target-hardware validation remain pending.
34. [x] Pixel Seek detections render as target-colored boxes without AI class/confidence labels; Object Detection retains its confidence filter and labels. Frontend build, all 8 frontend tests, and the Impeccable detector pass.
35. [x] Rechecked native readiness and recovery states: the bundled release app reports a port bind failure when `4200` is occupied, then reports “Waiting for sender” after IPC connects and a free UDP port is selected. The 192 × 192 sample stream changes the status to “Receiving frames.”
36. [x] Processing FPS now uses a trailing one-second window instead of retaining idle completions; the focused regression test, 45 backend tests, frontend build, and all 8 frontend tests pass.
37. [x] Current release-bundle native smoke review with the 192 × 192 sample at a 180 FPS send target showed 180.0 source FPS, 179.5 processing FPS, 60.1 preview FPS, 5 input drops, and 0 transport drops. At 980 × 680, Detection Engine choices stack and Settings scrolls. This does not verify Windows or OBS interoperability.
38. [ ] Object Detection's Backend, Precision, and Compute controls are visual placeholders: selected preferences can be saved, but they are not applied to inference. The implemented inference path accepts user `.pt` models through Ultralytics; implement and validate each runtime adapter before presenting these selections as active settings.
39. [x] Increased processing-FPS sample capacity to 4096 so the rolling estimator can represent rates above 360 FPS; a 600-FPS timestamp regression check passes. All 46 backend tests pass, PyInstaller rebuilt the ARM64 sidecar, the release bundle's sidecar `--help` check passes, and the latest 300.42 MiB unsigned macOS `Receiver.app` release bundle builds successfully. This does not validate Windows or OBS interoperability.
40. [x] Hardened `scripts/build-windows.ps1` to fail immediately when Python, pytest, PyInstaller, or Tauri native commands return nonzero; this prevents a later successful build step from hiding an earlier failed test. PowerShell/Windows execution is unavailable on this macOS workspace, so syntax and runtime validation require a Windows machine.
41. [x] Added an NSIS artifact smoke test for silent install, installed app/backend startup and shutdown, and uninstall; the installed executable is exercised by the existing lifecycle check. Windows execution remains pending because this workspace has no Windows host or PowerShell runtime.
42. [x] Strengthened the installed-app smoke check to wait for UDP port `4200` to be bound by the app's own backend process (up to 120 seconds), rather than treating process creation alone as startup evidence. Increased the standalone frozen-sidecar readiness timeout to the same window. Windows execution remains pending.
43. [x] Restricted decoded UDP input to complete JPEG frames with SOI/EOI markers, matching the future OBS sender contract even though OpenCV accepts additional image formats. Regression tests reject PNG and truncated JPEG; all 47 backend tests pass, and the updated ARM64 sidecar and 300.42 MiB macOS release bundle build successfully. The running UI review was not repeated because UDP port `4200` is occupied by an existing process; the old process was left untouched.
44. [x] Re-ran the 1280×720 UDP/decode/Pixel Seek/WebSocket backend benchmark after the JPEG guard at a 360 FPS target: sender/source/processing measured 359.94/360.04/359.49 FPS, with no skipped sender slots, input drops, transport drops, invalid packets, or rejected frames and 181 preview messages. This does not validate OBS, Tauri rendering, Windows, or production HSV profiles.
45. [x] UDP reassembly now rejects missing JPEG SOI/EOI boundary markers before retaining frame payloads and clears an existing partial assembly on an invalid boundary. Backend suite: 49 passed. Three-second 1280×720 synthetic runs measured 179.88/179.96/179.87 sender/source/processing FPS at the 180 target and 359.96/360.06/359.79 at the 360 target, with no skipped sender slots or drops. Rebuilt the unsigned 300.42 MiB macOS app; the packaged sidecar matches its source binary and both sidecar help checks pass. Windows, OBS, target-hardware, and production HSV-profile behavior remain unverified.
46. [x] Extracted Preview confidence filtering into a tested helper: Object Detection hides scores below the configured threshold and unscored results, while Pixel Seek is not confidence-filtered. Frontend tests: 11 passed; `pnpm build` passes. Native Windows behavior remains unverified.
47. [x] Added `src/assets/red-cube-192.jpg` as a real 192×192 SOI/EOI-marked sample and switched the Windows UDP smoke to it; verified the fixture traverses local UDP reassembly. The Windows smoke itself remains pending on a Windows runner.
48. [x] Added a service integration regression that changes the UDP port and verifies the replacement listener processes a frame; all 50 backend tests pass. A manual test against the currently running macOS release window did not advance from “Waiting for sender” after sending the sample to port `4201`, so packaged preview delivery through a live port change remains unverified and needs follow-up. The restore attempt to port `4200` failed because that port is occupied; the active instance remains on `4201`.
49. [x] Moved the Pixel Seek target-color controls into the Detection Engine group as an inline section with no duplicate Pixel Seek heading. Object Detection still hides those controls. `pnpm build` passes and the Impeccable detector returns no findings.
50. [x] Re-ran frontend and backend suites (11 and 50 passed) and rebuilt the 300.42 MiB macOS release bundle from the latest source. The bundled sidecar hash matches its release binary, and its `--help` check passes. The currently open release process predates that bundle and reports a UDP bind conflict; it and the other existing backend process were left untouched. Fresh native review, Windows CI/install validation, OBS interoperability, and target-hardware throughput remain open.
51. [x] Built a temporary release-mode validation app with an isolated bundle/data identifier and verified automatic preview from 192 × 192 MJPEG at a 30 FPS send target on UDP `4202` (before receiver-side cropping was added). The native HUD reported 26.8 source/processing FPS, 26.9 preview FPS, and 0 input/transport drops. The validation window was closed and its temporary bundle removed. This is macOS local-loopback evidence; Windows, OBS, and target-hardware behavior remain unverified.
52. [x] Revalidated the packaged Tauri preview with the 192 × 192 JPEG fixture on UDP `4202` during an 8-second 360-FPS-target run. The sender measured 350.74 FPS and skipped 74 sender slots; a live HUD snapshot showed 355.0 source/processing FPS, 60.0 preview FPS, 0 input/transport drops, and 0.04 ms processing latency. Fresh native captures at the 980 × 680 minimum and current display size confirm the stacked Detection Engine cards, readable labels, inline Pixel Seek color selector, and 192 × 192 Preview scaling. Frontend build, 11 frontend tests, 50 backend tests, and 1 Tauri test pass. This does not validate OBS, Windows, sustained 360-FPS sending, or target hardware.
53. [x] Nested the Target color selector inside the Pixel Seek option card so its visual grouping is explicit; it remains hidden in Object Detection. `pnpm build` passes, all 11 frontend tests, 50 backend tests, and 1 Tauri test pass, and the Impeccable detector returns no findings. The development preview accessibility tree confirms the grouping.
54. [x] Built and captured an isolated current-source macOS app at 980 × 680. The visual capture at `design-review/receiver-visual-qa-980x680.png` confirms the strategy choices stack, Target color controls stay inside the Pixel Seek card, and Settings scrolls to Preview controls. Its backend could not bind UDP `4200` because other Receiver processes already owned the port, so this is layout evidence only; no existing Receiver process was stopped. Windows/OBS interoperability and target-hardware validation remain open.
55. [x] Diagnosed the `4200` conflict with `lsof`: an older orphaned backend process owns the UDP socket. The isolated QA backend exited after its bind failed; closing the QA app left no processes from its unique bundle. This does not verify shutdown of a running backend, and existing Receiver processes were not terminated.
56. [x] Added a Device status at the top of the app. A background Tauri monitor checks the official Medius protocol and Ferrum `km.version()` API, and the header displays the verified device name in green or `No device` / `Device error` in red. Rust unit coverage validates Ferrum reply parsing. This macOS workspace has no attached hardware or Windows runner, so hardware discovery remains unverified on target Windows.
57. [x] Added compact Left/Right/Middle/Mouse4/Mouse5 indicators beside Device and receiver connection status. Medius button edges come from the `input_events` stream; Ferrum Legacy API bitmap callbacks are parsed as raw bytes. Updated the CP210x baud to the documented 115200 and added parser regression coverage. Expanded Medius/Ferrum API references with button-state APIs and broader API-family indexes. Rust tests pass; no vendor hardware is attached, so live physical-button state and Ferrum endpoint compatibility remain unverified.
58. [x] Added a global Apply Changes button that saves the current UI settings to local application storage and restores them on launch. It applies the port, detection engine, target color, and active model configuration to the backend; placeholder Backend/Precision/Compute choices persist as preferences only and do not configure inference. Frontend build and settings persistence tests pass; Windows storage behavior remains unverified.
59. [x] Removed Pixel Seek explanatory copy and the extra color heading/status row from Detection Engine; Yellow/Red/Purple controls now fill the available card width. All 13 frontend tests pass. A macOS Tauri release executable compiled successfully; the configured NSIS installer cannot be produced on this macOS host. Existing Receiver processes were left running, so no fresh native window capture was taken.
60. [x] Split the Pixel Seek and Object Detection choices into independent radio cards and moved the three target colors outside them into a separate full-width row shown only for Pixel Seek. Successful Apply Changes reports “Changes applied successfully” and hides the notice after three seconds. All 13 frontend tests pass, and the macOS release app bundle rebuilt successfully. The running app instance was left intact, so fresh native visual review remains pending until it is restarted.
61. [x] Fixed Preview's no-frame state stacking so the status renders above the opaque canvas. It now labels the empty preview “NO FRAME” and distinguishes receiver startup from waiting for a sender. Frontend build/tests and the macOS release bundle are updated; visual confirmation in the already-running window is pending a restart.
62. [x] Reduced the Preview panel cap from 640 × 640 to 480 × 480 CSS pixels so low-resolution sender frames are enlarged less. Aspect-preserving fit and Smooth/Pixel Perfect scaling remain unchanged. A fresh isolated macOS release capture at 980 × 680 confirms the smaller panel. It shows the existing UDP `4200` conflict and is layout evidence only; no existing Receiver process was stopped.
63. [x] Reorganized the shell around Settings with General/FOV navigation. General includes a persisted Preview visibility switch; hiding it does not stop receiver processing. FOV remains an always-on overlay and its size control lives on the FOV page. Frontend production build and 13 frontend tests pass; Impeccable detector returned no findings. Native release capture confirms the General layout; its backend shows an existing UDP `4200` conflict, so it is layout evidence only.
64. [x] Renamed the user-facing product to SysX in the desktop header, window metadata, and current product documentation. Built and opened `src-tauri/target/release/bundle/macos/SysX.app`; the window title, header, General/FOV navigation, and Show preview switch match the current source. The backend had not reached ready state during the visual check, so this is a UI check only. The Tauri identifier and settings storage key remain unchanged to preserve existing app data. This macOS build does not validate Windows.
65. [x] Frame-source runtime errors now retry every 3 seconds. If a requested port is occupied, the existing listener stays active while the backend retries; successful recovery switches ports, updates runtime state, and clears the recovered error. Startup bind and later UDP read/decode failures retry too. Backend service tests cover conflict recovery; Windows behavior remains unverified.
66. [x] Added code-defined red, yellow, and purple HSV profiles using RGB-to-HSV conversion for Pixel Seek. Backend coverage checks each profile against decoded BGR color samples and verifies the exact thresholds.
67. [x] Moved configuration out of the native monitor window into a separate browser page served on loopback. SysX opens that page at startup; preferences are stored in app data and loaded by the backend on the next launch. The native window now keeps Preview always visible and shows only processing FPS, latency, and processing resolution in its HUD alongside connection/device and mouse-button states. Browser config uses the next available port beginning at `4201` if the preferred port is occupied. Frontend build, frontend tests, backend tests, and Tauri Rust check pass on macOS; Windows packaging and behavior remain unverified.
68. [x] Reduced whitespace around Preview and expanded its cap to 640 × 640 px. Mouse State buttons now use equal widths. Applied compatible CapkfaPlus behavior to Object Detection with class-aware Ultralytics NMS and configured confidence/IoU thresholds. Pixel Seek RGB-to-HSV and two-mode validation already matched. Raw 6/7-field decoding remains in Ultralytics because the `.pt` runtime returns decoded boxes.
69. [x] Added a persisted Crop Resolution setting. The receiver applies the recovered mode-5 centered `N × N` crop to each full MJPEG frame before detection and displays the cropped frame in Preview; boxes stay in original-frame coordinates and are translated for Preview. Backend suite: 71 passed; frontend suite: 14 passed; `pnpm build` passes. Windows and OBS runtime validation remain pending.
70. [x] Added post-detection targeting contracts. Object Detection class/score gates and box-center candidate conversion, frame-sized Pixel Seek mask validation, fractional-delta accumulation, and axis-lock math are covered by backend tests. Source-specific mask fallback, ellipse and prior-box ranking, LogicManager interpolation, and vendor movement remain unimplemented skeletons; device integration is still read-only.
71. [x] Pixel Seek continues to detect across the full processed crop, then selects the highest accepted mask pixel inside the configured FOV for downstream targeting. The selected frame coordinate is carried in detection metadata and rendered as a center-to-target line and ring in Preview. Object Detection candidates are FOV-filtered before its ranking extension point; detections outside FOV remain visible. Backend/frontend regression tests cover this behavior and crop-coordinate mapping.
72. [x] Wired Object Detection to the reconstructed FOV/nearest-box/prior-box/class-anchor selector and the Preview target point. Preview now draws a solid center-to-target line, a complete target ring, per-class colored corner boxes, and a `[id] score% | ...` summary badge. Medius remains at its official crate rate (6,000,000 baud) and Ferrum Legacy at 115,200 baud; no Makcu rate is used. Backend/frontend tests cover target selection and summary formatting; Windows/device visual validation remains pending.
73. [x] Added the recovered type-0 async serial queue-consumer contract. The consumer retains command bytes and completion state until the writer reports completion, handles synchronous submission failures, and ignores unimplemented request kinds. This is a tested writer-interface skeleton only; no Win32 overlapped writer or device movement is active. The RAR's 17 files currently map to 6 integrated/equivalent behaviors, 6 partial or skeleton behaviors, and 5 intentionally unported or out-of-scope behaviors; this async consumer is an additional partial reconstruction from the pasted snippet.

## Limitations to preserve

- There are no sender binaries, user models, or target-Hardware measurements in the repository.
- Object Detection's ONNX Runtime/TensorRT/OpenVINO, FP16/INT8, and CPU/GPU/NPU controls persist only as UI preferences and are not wired to inference runtime selection. Do not report those backends or compute targets as supported; `.pt` inference is implemented through Ultralytics.
- Do not report the sample values in the prototype as live measurements.
- The app can only be called Windows-tested after a Windows CI or device build and runtime check.
- `scripts/smoke-windows-app.ps1` has not been executed locally because this workspace is macOS and has no PowerShell runtime; run it on a Windows machine for authoritative validation.
- The local macOS app bundle contains the Tauri executable, icon, and Python sidecar. A short 192 × 192 local-sender run at a 360-FPS target measured 350.74 sender FPS (74 skipped slots), with a live HUD snapshot of 355.0 source/processing FPS and 60.0 preview FPS, and zero receiver drops. This does not verify OBS interoperability, Windows behavior, sustained 360-FPS sender throughput, or target hardware.
- Except for `design-review/receiver-visual-qa-980x680.png`, the PNGs in `design-review/` are historical captures of earlier layouts. The current screenshot verifies layout only and shows a UDP bind error; Preview fit, overlay mapping, and HiDPI geometry have automated unit coverage, while Windows visual behavior remains unverified.

