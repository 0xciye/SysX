# Development

## Current milestone status (2026-10-15)

The Rust conversion is the active implementation: `rust-backend`, `rust-ui`, and `src-tauri` build and test without the retired Python/React runtime. The release macOS bundle was rebuilt from the current source and visually checked with the native Rust UI mounted through embedded WASM; its square preview startup state and FPS/latency/resolution HUD are visible. Chrome configuration mounts the same Rust WASM UI and uses the persisted app-data settings file. Windows and physical-device validation remain outstanding.

## Architecture

The application source is Rust: **Tauri 2 + a Leptos/WebAssembly UI + a Rust sidecar**. Trunk builds the UI bundle consumed by the native monitor and by the sidecar's loopback-only `/config.html` route. Windows release builds embed both the sidecar and config assets into one portable `SysX.exe`; the app extracts them under app data while it runs. The legacy React/TypeScript frontend, Python backend, Python benchmark scripts, and PyInstaller spec have been removed. Rust Object Detection has ONNX Runtime, OpenVINO, and optional TensorRT adapters; no real model or target hardware has validated inference.

The monitor is a native desktop window rendered through the operating system's WebView (WebView2 on Windows and WKWebView on macOS). Configuration runs in a separate browser window against a loopback-only page served by the Rust sidecar; it is not public or cloud hosted. Windows builds emit a single executable without an installer. The Windows host must provide WebView2 Runtime, and accelerator backends may require their vendor runtime. Neither framework by itself guarantees a 180–360 FPS processing rate; that depends on frame size, model, device, and bounded pipeline behavior and must be benchmarked.

Build the Windows executable and its embedded sidecar on Windows; do not claim Windows validation from a macOS build.

References:

- [Tauri process model](https://v2.tauri.app/concept/process-model/)
- [Tauri sidecar binaries](https://v2.tauri.app/develop/sidecar/)
- [Electron process model](https://www.electronjs.org/docs/latest/tutorial/process-model)
- [Electron IPC](https://www.electronjs.org/docs/latest/tutorial/ipc)

## Runtime responsibilities

### Brand assets

- `public/receiver-mark.svg` is the canonical SysX mark. Keep the title-bar mark and the Tauri PNG/ICO exports visually aligned with it.
- `src-tauri/tauri.conf.json` lists the PNG and ICO used by desktop bundles.

### Rust UI

- UI components live in `rust-ui/src`; `layout.rs` contains pure preview geometry helpers.
- `scripts/build-rust-ui.mjs` runs Trunk for both development and release builds. The same output directory feeds Tauri's monitor and the Rust sidecar's local configuration server.
- Tauri enables `withGlobalTauri` so the Rust UI can select the monitor view and call native commands. The browser-served configuration view uses the local backend HTTP/WebSocket APIs.

### Desktop frontend

- Keep the native monitor window focused on Preview, Mouse State, device state, and receiver connection state. Preview is always visible; its HUD shows only processing FPS, latency, and processing resolution.
- Put all settings in one vertically scrolling page in the separate browser window, grouped into General and FOV sections with one global Apply Changes action. Persist settings in app data and restore them when the backend starts.
- Cap Preview at 640 × 640 px; preserve aspect ratio and enlarge smaller frames with smooth or nearest-neighbor scaling.
- Keep a circle-only FOV overlay always enabled; expose its size as a 1–360° setting in the local config page, with no enable/disable control. Scale its diameter against the shorter side of the displayed image so it stays proportional across source resolutions; 360° is the largest inscribed circle.
- Persist `offsetX` and `offsetY` as signed crop-pixel settings and display them relative to the selected target in the web config. Movement adds these offsets to the selected point before calculating crop-center error.
- Persist a Trigger button preference in the web config with Right, Middle, M4, and M5 choices; exclude Left. Device output is sent only while that physical button is held.
- Provide independent X/Y exponential-smoothing alpha sliders in the local config. The backend applies `previous + alpha * (current - previous)` to selected target coordinates; the filtered point feeds Preview and device movement while raw detections remain intact. Alpha `1.0` disables filtering. Clear smoothing history when a target is lost or pipeline configuration changes.
- Provide persisted user-entered sensitivity (`0.01..10`) and DPI (`100..100000`) in the local config. Movement scale uses `360 * 2.54 * max(sens, 0.01) / (54.54 * DPI)`.
- Reflect backend IPC readiness in the connection status; show “Waiting for sender” after the authenticated socket connects and reserve “Starting receiver” for startup.
- Retry frame-source runtime failures every 3 seconds. Retain the active listener while a requested port is occupied, then switch to the requested port and clear its error after a successful bind. Startup bind failures and later UDP read/decode failures retry until the listener returns.
- Let the local configuration page scroll vertically as additional settings are added.
- Apply backend and compute selections to inference. Precision remains a saved preference because the imported model artifact supplies its precision.
- Send receiver, crop, and detection configuration to the backend. The sender provides full-frame MJPEG; the receiver takes a centered square crop before detection and publishes that cropped image to Preview.
- Default the UDP listener to port `4200`. Show model controls (including import) only in Object Detection mode. Do not add a Data or export section to the desktop UI.
- Object Detection's Min confidence threshold ranges from `0` to `1` and filters which detections are drawn in Preview.
- Place Pixel Seek's Yellow/Red/Purple target-color selector in a separate full-width row below the Detection Engine cards; show it only in Pixel Seek mode.
- Render Pixel Seek detections as boxes tinted with the selected target color, without AI class/confidence labels. Object Detection filters by minimum confidence, draws stable per-class boxes, and shows `[id] score% | ...` in a compact summary badge.
- Never run inference on the UI thread.

### Rust backend

- Packet reassembly and JPEG boundary validation live in `rust-backend/src/udp.rs`; UDP receive and JPEG decode live in `receiver.rs`; centered crop geometry is isolated in `crop.rs`.
- Bounded latest-frame processing lives in `pipeline.rs`. Pixel Seek profiles and target selection live in `pixel_seek.rs` and `targeting.rs`.
- Model loading, NCHW RGB preprocessing, runtime adapters, YOLOv8 output decoding, confidence/IoU filtering, and class-aware NMS live in `inference.rs`. The pipeline caches the selected runtime and uses model artifact paths plus backend/compute selection to choose it.
- Movement conversion/batching, persistence, and loopback web configuration live in `movement.rs`, `storage.rs`, and `web_config.rs`.
- User settings live in `app-settings.json` under the operating system's app-data directory. The backend creates a defaults file on first launch, loads it at startup, and the local page reads and atomically updates the same file through `GET/PUT /api/settings` when Apply Changes is used.
- Import one bundle with ONNX `.onnx`, TensorRT `.engine`/`.plan`, and OpenVINO IR `.xml`/`.bin`. ONNX Runtime is CPU-only; OpenVINO requests CPU/GPU/NPU from the installed runtime; TensorRT is compiled only when TensorRT headers and `CUDA_PATH` are set during backend build and needs GPU hardware. Inference rejects crop dimensions that differ from the configured model input; it does not resize.
- The supported decoder expects raw YOLOv8 output shaped `[1, 4+classes, anchors]` or `[1, anchors, 4+classes]`. Confidence and IoU come from the model registry; class-aware NMS precedes the common FOV/prior-box/anchor selector. The Precision selector is saved UI preference only.
- Runtime adapters compile, the pure output decoder and crop mismatch path have unit coverage, but no user model is present. Actual model inference and accelerator/runtime availability remain unverified.
- `scripts/build-rust-backend.mjs` builds the target-specific sidecar after the UI bundle; `src-tauri/build.rs` embeds it and the config assets in the Tauri executable. The host extracts them into a per-process app-data runtime directory, manages the backend process, and removes that directory on normal shutdown.

The Rust sidecar receives full-frame MJPEG, applies the configured crop, runs Pixel Seek or a selected Object Detection runtime, persists detections, and serves local configuration plus authenticated state/preview WebSocket messages. The Rust UI consumes binary preview frames and typed state. No Python or React/TypeScript application source remains in the repository.

### Frame flow

1. Receive and validate a packet/frame.
2. Reassemble UDP frames with bounded memory and timeout.
3. Keep queues bounded and discard stale frames under load.
4. Process every frame still admitted to the processing queue.
5. Persist detection output asynchronously.
6. Publish only the latest preview frame at a display-friendly cadence; report processing FPS and preview FPS separately.

### Target selection and mouse output

- Rust `targeting.rs` and `pipeline.rs` preserve full-crop detection and FOV-gated Pixel Seek/Object Detection selection, including nearest box, prior-box overlap, and the existing class anchor rule. Preview retains detections outside FOV.
- Rust `movement.rs` converts fresh filtered crop targets and batches deltas before Tauri receives them. `src-tauri/src/movement.rs` also sums fresh pending deltas, applies the held-trigger gate and 100 ms freshness limit, adds the sampled recoil value to Y, then caps each device command to ±32 per axis. Medius and Ferrum calls remain in Tauri's device monitors.
- `src-tauri/src/serial_queue.rs` ports the type-0 asynchronous command consumer behind a writer interface. It retains command bytes until completion and is intentionally not connected to a device.

The CapkfaPlus mapping in [`docs/CAPKFA_RECONSTRUCTION_MAPPING.md`](docs/CAPKFA_RECONSTRUCTION_MAPPING.md) is reference material only. SysX does not expose or run the reconstructed active-state, mode-predicate, direct-offset, or timed-click branches because their live state producers are unavailable. The supported output path uses fresh summed deltas, the configured physical trigger, sampled recoil on Y, and the documented Medius/Ferrum relative-move APIs.

The development and release launchers build the Rust UI before the Rust backend. Tauri embeds the sidecar and static config bundle; Windows emits the `SysX.exe` portable executable with `--no-bundle`. UDP is reserved for sender frames. Large frame payloads use binary WebSocket messages with small metadata rather than repeated JSON serialization.

The Tauri host runs the vision sidecar and a background device monitor as separate Rust processes/threads. Pending movement-delta merging, freshness checks, trigger mapping, and the ±32 per-axis command clamp live in `src-tauri/src/movement.rs`; device monitors retain the Medius and Ferrum API calls. The host checks Medius devices using the official `medius` Rust client and watches five physical button states. Ferrum currently uses a Silicon Labs CP210x Legacy API connection at 115,200 baud, verified by `km.version()`. While the configured trigger is held, the host sends the merged movement through that same verified connection. The X/Y exponential filters are applied before movement. The Ferrum App Software API endpoint and movement behavior on either vendor's hardware remain unverified on Windows.

No GitHub Actions build workflow is configured. `scripts/build-windows.ps1` builds the Rust sidecar and UI, emits one `SysX.exe` without an installer, verifies UDP `4200` and 192×192 frame receipt, then checks executable startup, embedded backend bind, and normal shutdown. The PowerShell build script explicitly checks each native command's `$LASTEXITCODE`; `$ErrorActionPreference` alone does not make every native nonzero exit terminating.

## Build prerequisites

- Node.js 20+, pnpm, Rust stable, Trunk, and the Tauri platform prerequisites.
- For Windows release builds: Windows x64 runner and an available WebView2 Runtime on the validation machine.

## Commands

Build and launch commands from the project root:

```sh
pnpm install
pnpm tauri dev
pnpm build
pnpm tauri build
```

Rust backend checks from the repository root:

```sh
cargo check --manifest-path rust-backend/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path rust-backend/Cargo.toml
cargo test --manifest-path rust-ui/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

The Rust UI release bundle is built with `pnpm build` (or `node scripts/build-rust-ui.mjs build`).

Local UDP protocol verification:

```sh
cargo run --manifest-path rust-backend/Cargo.toml -- udp-listen --host 127.0.0.1 --port 4200
cargo run --manifest-path rust-backend/Cargo.toml -- udp-send-image src/assets/red-cube-192.jpg --host 127.0.0.1 --port 4200
```

The verifier reports frames it actually receives; it does not measure inference throughput. Historical 1280×720 synthetic rates in older project records came from the retired Python backend and are not Rust performance evidence. No current Rust end-to-end benchmark has been run.

Use `0.0.0.0` as the listener host only when intentionally accepting frames from other computers on the network. The verifier reports frames it actually receives; it does not measure inference throughput.

The OBS sender is out of scope for the current application. Its interoperable SysX contract and implementation limits are documented in [`docs/OBS_RECEIVER_PROTOCOL.md`](docs/OBS_RECEIVER_PROTOCOL.md). Keep that document synchronized with `rust-backend/src/udp.rs` if the wire protocol changes.

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
10. [x] Windows x64 build and smoke scripts; [ ] manual Windows executable build, startup/shutdown, and visual application validation. GitHub Actions build workflow removed.
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
38. [x] Backend and Compute now configure Rust inference runtime selection. Precision remains a saved preference because precision is determined by each imported model artifact; real-model and device validation is still pending.
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
74. [x] Combined General and FOV controls into one vertically scrolling web configuration page and removed page navigation. `pnpm build` and `git diff --check` pass; Windows visual validation remains pending.
75. [x] Added persistent signed X/Y Target Offset settings and a target-relative map in the single-page web config; offsets validate as finite numbers and restore through backend app settings. The values are not yet applied to mouse motion. Frontend production build, frontend tests, backend web-config/service tests, and diff checks pass; Windows visual/device validation remains pending.
76. [x] Added a persisted Trigger button preference with Right, Middle, M4, and M5 choices; Left and unknown values are rejected by backend validation. The selection is configuration only because trigger-driven movement is not implemented. Frontend and backend regression coverage added.
77. [x] Connected held-trigger movement to the selected physical device. Processed results send filtered targets through the sidecar pipe; the backend applies crop-center error, saved offsets, sensitivity/DPI scaling, fractional residuals, and X/Y filtering before Medius `move_rel` or Ferrum Legacy `km.move`. Commands are limited to ±32 units per axis and stale targets expire after 100 ms. Backend and Rust unit tests pass; physical device or Windows hardware validation remains unavailable.
78. [x] Extracted filtered-target to relative-delta calculation into `receiver_backend.movement.MovementController`; `ReceiverService` retains pipe publication and lifecycle duties. Added focused movement-controller regression tests.
79. [x] Integrated the supplied NCAF single-step movement controller, including three-zone response, prediction, scaling, cubic Bézier response, residual accumulation, and occasional micro-corrections. Retained latest-frame movement and Tauri's stale/±32 safeguards; source path queues and delays remain unused.
80. [x] Added persisted Sensitivity and DPI controls and applied the screenshot's movement conversion formula. Movement deltas are summed over a short backend batch and again while pending at the Tauri device monitor, then sent as one relative API command. At that milestone, the special-route state inputs were still absent; they are now explicitly configured as recorded in milestone 92.
81. [x] Consolidated the supported UDP/JPEG frame source and centered crop helper in `receiver.py`; packet parsing stays in `udp.py`, processing stays in `pipeline.py`, movement stays in `movement.py`, and the local config server stays in `web_config.py`. Removed unused local image/video source classes and their tests. Existing two-stage movement delta aggregation remains in place.
82. [x] Moved the legacy Python backend 4 ms movement batching worker into `movement.py`; the current Rust sidecar has its own movement module.
83. [x] Extracted Tauri pending-movement aggregation, freshness checks, trigger mapping, and command clamping into `src-tauri/src/movement.rs`; Medius and Ferrum monitors keep the vendor API calls. `cargo check` passes; hardware behavior remains unverified.
84. [x] Tauri now launches and packages the Rust sidecar using a target-aware build hook. The Rust UDP CLI can send the actual JPEG fixture; an isolated macOS service smoke processed one 192×192 frame on UDP `4202` with zero rejected frames. The UI remains React/TypeScript, Rust Object Detection is unavailable, and Windows/hardware behavior remains unverified.
85. [x] Replaced the active React frontend build with a Leptos/WebAssembly Rust UI. Trunk now builds the monitor and configuration UI bundle; Tauri uses it for the monitor, and the Rust sidecar serves the same `index.html` at `/config.html`. At that milestone, Object Detection remained unavailable and legacy Python/React sources were still present.
86. [x] Added Rust Object Detection runtime integration for a four-file model bundle: ONNX, TensorRT engine/plan, and OpenVINO XML/BIN. The pipeline caches the selected runtime, enforces exact crop/model dimensions, applies model confidence/IoU, decodes raw YOLOv8 output with class-aware NMS, and passes candidates through the Rust FOV/prior-box/class-anchor selector. Added runtime/decoder/target-selection coverage. Rust backend has 22 passing tests; Rust UI release build and Tauri `cargo check` pass. `cargo check --features tensorrt` passes, but the TensorRT SDK shim, OpenVINO device runtime, real model inference, Windows, and hardware have not been validated.
87. [x] Removed the legacy Python backend/tests, React/TypeScript UI/tests, PyInstaller spec, and Python benchmark scripts. The active application/runtime source is Rust; pnpm/Node remain build tooling for Tauri and Trunk. Added the Rust type-0 serial queue consumer behind an async-writer interface and Rust tests for its buffer lifetime and completion order.
88. [x] Translated supported CapkfaPlus reconstruction formulas into Rust helpers and mapped all reconstruction files in `docs/CAPKFA_RECONSTRUCTION_MAPPING.md`. At that milestone direct/timed click and recoil were helper-only; milestone 92 connects them through explicit persisted state. No reconstructed UDP device packet is used.
89. [x] Rechecked the two movement aggregation stages and fixed stale-delta carryover: each stage now excludes a pending delta older than 100 ms before summing it with a newer frame. Backend tests (28), Tauri tests (9), UI tests (4), Trunk release build, per-crate `cargo fmt --check`, and `git diff --check` pass. Windows, OBS sender interoperability, and device movement remain unverified.
90. [x] Added the required Rust service integration test: change from an ephemeral UDP port to another port, verify the new port is active, send `src/assets/red-cube-192.jpg` through the implemented packet format, and confirm the processing pipeline completes the frame. This exercises local Rust receiver behavior and does not validate a production OBS plugin, Windows, or device hardware. Backend suite: 29 passed.
91. [x] Added a web-config upload/commit integration test for all four files in the selected model bundle: ONNX, TensorRT engine, and OpenVINO XML/BIN. The test verifies that the committed artifacts exist at the paths returned to the model registry. Current full suites: backend 30, Tauri 9, UI 4 passed; TensorRT feature `cargo check`, Trunk release build, per-crate `cargo fmt --check`, and `git diff --check` pass. No model was supplied, so actual inference remains unverified.
92. [x] Added explicit reconstructed movement state and timed-action settings. This milestone is superseded by milestone 94: those state/timed branches were removed because their state producers and behavior are not available in the active source. Recoil remains in the normal movement path.
93. [x] Changed Windows output to a portable single `SysX.exe` with no installer. The Rust backend and local configuration assets are embedded, extracted to per-process app data at launch, and removed on normal shutdown. Updated the Windows smoke path to check the standalone executable and removed the NSIS installer smoke script. The macOS release app compiles with embedded assets; Windows output and runtime validation remain pending.
94. [x] Removed Native Motion State and Timed Action from the active UI/runtime. Restored the Tauri movement module with fresh pending-delta summing, selected-trigger gating, recoil sampling into Y, and ±32 command clamps. The backend now creates `app-settings.json` on first launch; the web config reads and writes that file through its local settings API. Added persistence and movement regression tests; Windows and hardware movement remain unverified.
95. [x] Fixed packaged WebView startup: production CSP now permits the Trunk WASM bootstrap, and the embedded page includes a visible `NO FRAME` startup fallback if a platform WebView cannot initialize WASM. Rebuilt and opened the release macOS bundle successfully; it shows the SysX startup state instead of a black window. The fallback does not replace the Rust UI when WASM initializes normally.
96. [x] Made the native Preview viewport square at up to 640 × 640 CSS pixels and kept its HUD to the three requested measured fields: resolution, FPS, and latency. The startup fallback follows the same square preview and HUD shape.

## Limitations to preserve

- There are no sender binaries, user models, or target-Hardware measurements in the repository.
- No user model is present. Object Detection supports raw YOLOv8 output only; ONNX Runtime uses CPU, OpenVINO requires the matching installed runtime/device plugin, and TensorRT requires a build with TensorRT headers plus `CUDA_PATH` and a compatible GPU. Precision remains a saved preference. No real-model, Windows, accelerator, or hardware inference has been validated.
- Do not report the sample values in the prototype as live measurements.
- The app can only be called Windows-tested after a Windows CI or device build and runtime check.
- `scripts/smoke-windows-app.ps1` has not been executed locally because this workspace is macOS and has no PowerShell runtime; run it on a Windows machine for authoritative validation.
- Previous macOS app bundles and benchmark results include the retired Python path. The 350.74 sender FPS / 355.0 source-processing FPS run is historical and is not Rust or current Tauri measurement. It does not verify OBS interoperability, Windows behavior, sustained 360-FPS sender throughput, or target hardware.
- Except for `design-review/receiver-visual-qa-980x680.png`, the PNGs in `design-review/` are historical captures of earlier layouts. The current screenshot verifies layout only and shows a UDP bind error; Preview fit, overlay mapping, and HiDPI geometry have automated unit coverage, while Windows visual behavior remains unverified.

