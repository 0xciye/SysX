# Project instructions

- The product name is **SysX**. Use SysX in user-facing UI, window metadata, and current product documentation. Preserve internal identifiers and settings storage keys unless migration is explicitly required.

## Product direction

- Build an installed Windows desktop application, not a browser-hosted website.
- Keep the English UI dark, minimal, and technically professional.
- Make Settings the primary workspace, with General and FOV destinations inside the Settings navigation. Preview is an on/off display option on General; turning it off hides the Preview pane while receiver acquisition and processing continue.
- Cap the Preview viewport at 480 × 480 CSS pixels. Smooth scaling should enlarge small sender frames while preserving aspect ratio; Pixel Perfect should use nearest-neighbor interpolation.
- Keep a circle-only FOV overlay always enabled; let the user set it from 1–360° on the FOV settings page. Scale its diameter against the shorter side of the displayed image, so the same degree setting has the same proportion at every sender resolution and 360° stays inside the image/preview. Do not add an enable/disable control.
- Let the Settings column scroll inside the window as additional options are added.
- Provide one global **Apply Changes** action that persists the current user-editable settings and restores them on the next launch. Backend, Precision, and Compute remain saved UI preferences only until their runtime adapters are implemented.
- Show “Changes applied successfully.” when Apply Changes succeeds, then hide the confirmation after three seconds.
- Keep the Preview empty state visible above its canvas when no frame has arrived; label it “NO FRAME” and show the appropriate startup or sender-waiting status.
- At the 980 × 680 minimum window size, keep Detection Engine labels readable; stack strategy choices at compact widths instead of truncating names. Recapture the native UI after layout changes and do not present older `design-review/` screenshots as current.
- Recapture native UI after layout changes; current settings-first layout is captured in `design-review/receiver-settings-general.png`. The isolated review app showed the existing UDP port conflict, so this is layout-only evidence. Do not treat older captures or an already-running installed window as current.
- Do not terminate existing Receiver/backend processes during native smoke checks. A process inspection identified an older orphaned backend as the current owner of UDP port `4200`; leave it untouched. A separate release-mode validation build with isolated app data previously previewed 192 × 192 MJPEG on port `4202`.
- Receiver waits automatically for frames and begins preview on the first valid frame. Never add Start, Pause, or Stop controls.
- Show “Waiting for sender” once the authenticated backend connection is ready; reserve “Starting receiver” for backend startup and show connection/runtime failures as errors.
- Default UDP input port is `4200` across the app, backend, local verifier, and documentation.
- Sender-provided, already-cropped MJPEG frames are the only production frame source. Do not add local sample, image, or video source controls to the desktop UI.
- Show model controls (including import) only in Object Detection mode; do not add a Data or export section to the desktop UI.
- Backend, Precision, and Compute selectors currently exist as UI placeholders only; saved selections do not configure or persist inference. When implementing them, update model registry, runtime validation, UI state, and tests together. Current inference accepts user-supplied Ultralytics `.pt` models only.
- Present Pixel Seek and Object Detection as radio cards in Detection Engine; keep them side by side at normal widths and stack them when compact so both names remain readable.
- Present Pixel Seek and Object Detection as separate Detection Engine cards. Show the Yellow/Red/Purple target-color choices in a separate full-width row below the cards when Pixel Seek is selected; hide them in Object Detection.
- Keep the Pixel Seek card free of explanatory copy.
- Render Pixel Seek detections as bounding boxes tinted to the selected target color; do not show AI class/confidence labels for Pixel Seek.
- Object Detection always shows Min confidence as a 0–1 slider, including before a model is selected; do not render detections below it in Preview.
- Implement only `pixel_seek` and `object_detection` strategies.
- Pixel Seek selects exactly one target color at a time: yellow, red, or purple. HSV thresholds are code-defined in `PIXEL_COLOR_PROFILES`; empty profiles must produce no detections, and the UI must not expose editable HSV ranges.
- Pixel Seek ROI is code-defined per `PixelColorRule` as normalized source-frame coordinates `(left, top, right, bottom)`; `None` means the entire frame. Return boxes in original frame coordinates.

## Architecture boundaries

- Keep the Python vision backend independent from the React frontend.
- Keep frame acquisition, processing, persistence, and rendering decoupled.
- Use bounded queues and discard obsolete frames when processing falls behind.
- Render the latest useful preview at display cadence; do not claim the UI renders at the processing rate.
- Keep source, inference, and display resolutions distinct. Preview scaling must not alter detection input.
- Keep preview fitting, detection-box mapping, and HiDPI backing-size math in pure helpers with frontend tests.
- Keep Preview confidence filtering in a pure helper with regression coverage for the threshold boundary, missing confidence, and Pixel Seek bypass.
- Show source/inference resolutions and measured performance/drop counters in the compact Preview HUD; distinguish source, processing, and preview FPS. Source FPS is valid decoded frames observed by the receiver over a trailing one-second window; processing FPS is completed detections over a trailing one-second window. The processing completion-time deque holds 4096 samples and has a regression test above the requested 360 FPS range; update its capacity and test together if the supported rate rises beyond that window's capacity.
- Show the device reported by the inference runtime, not only the configured device preference.
- Keep desktop WebView controls keyboard-visible and provide at least 24 CSS px pointer targets for compact switches.
- Apply boxes and labels in display coordinates after scaling the source image.

## Data and safety

- Never fabricate FPS, latency, detections, model metadata, or benchmark results.
- Keep the synthetic Pixel Seek microbenchmark explicitly separate from production color profiles and end-to-end performance claims.
- Retain a backend integration test that changes the UDP port, confirms the new bind, and processes a frame on the new port.
- Use `src/assets/red-cube-192.jpg` as the actual JPEG fixture for sender protocol smoke checks; preserve its SOI/EOI markers.
- `scripts/benchmark_receiver_udp.py` is a synthetic local-backend benchmark, not Windows, OBS capture, or Tauri renderer validation. Report its scope with any FPS result.
- Latest macOS 15.3.1 ARM64 synthetic 1280×720 backend run measured 179.95/179.93/180.05 FPS (sender/source/processing) at 180 and 359.99/359.81/359.79 FPS at 360, with zero drops; this does not validate Windows or the OBS sender.
- After adding SOI/EOI validation during UDP reassembly, 3-second 1280×720 synthetic runs measured 179.88/179.96/179.87 sender/source/processing FPS at the 180 target and 359.96/360.06/359.79 at the 360 target, with 0 skipped sender slots, input drops, transport drops, invalid packets, or rejected frames and 181 preview messages per run. This remains backend-only synthetic evidence.
- A current packaged macOS UI smoke snapshot with the 192×192 sample at a 180 FPS send target showed 180.0 source, 179.5 processing, and 60.1 preview FPS, 5 input drops, and 0 transport drops. This is loopback test evidence, not OBS or Windows validation.
- A macOS 15.3.1 ARM64 release-mode Tauri validation app with isolated app data received the 192×192 JPEG fixture on UDP `4202` during an 8-second 360-FPS-target run. The sender measured 350.74 FPS with 74 skipped slots; a live HUD snapshot showed 355.0 source and processing FPS, 60.0 preview FPS, 0 input/transport drops, and 0.04 ms processing latency. This is local synthetic-sender evidence, not OBS, Windows, sustained 360-FPS sender, or target-hardware validation.
- Include requested sender FPS, measured source FPS, processing FPS, preview FPS, and available sender/receiver drops in benchmark reports.
- Measure the live Tauri preview separately from the Python-only synthetic benchmark; report the OS, input fixture, rates, and drop counters for each.
- Never download or substitute a model for the user's own model.
- Validate every network packet, dimensions, chunk count, payload length, and aggregate memory before allocating or decoding. Check JPEG SOI on chunk 0 and EOI on the final chunk before retaining those payloads; discard any partial assembly if a boundary marker is invalid. UDP image payloads must be complete JPEG frames; do not let OpenCV's broader decoder accept PNG or other formats through the production sender path.
- Preserve AI class IDs exactly. Pixel Seek confidence is null unless a probabilistic metric is separately implemented.
- Do not silently resize UDP frames for Object Detection. Report and count model-resolution mismatches.
- Removing a registry model must preserve its `.pt` file. Fall back to Pixel Seek only when the active model is removed.
- Persist data without blocking frame processing; report storage failures.
- Exporting session CSV must flush queued JSONL records first and keep file I/O off the asyncio event loop.

## Development

- Read `DEVELOPMENT.md` before changing the application architecture.
- Add automated tests for new parsing, processing, persistence, and coordinate-mapping behavior. The user explicitly requested automated tests in the source specification.
- Do not say a Windows build or hardware throughput target passed unless it was actually run on Windows or the target hardware.
- The local macOS app bundles its Python sidecar and has been smoke-tested with 192×192 UDP frames; do not treat this as Windows, OBS, or 180–360 FPS validation. App shutdown must stop the full PyInstaller process group.
- A current native 980 × 680 review confirmed stacked Detection Engine choices and a scrollable Settings panel; the `design-review/` screenshots remain historical and must not be presented as that review.
- The Windows workflow runs on pushes, pull requests, and manual dispatch. It must run `scripts/smoke-windows-udp.ps1` against the built sidecar and `scripts/smoke-windows-installer.ps1` against the NSIS artifact. These verify the default UDP port and 192×192 frame reassembly, then silent install, installed app/backend startup with UDP `4200` bound by that backend process, shutdown, and uninstall; they do not verify visual rendering.
- In Windows PowerShell build scripts, check `$LASTEXITCODE` after every native command. `$ErrorActionPreference = "Stop"` alone does not reliably turn native nonzero exit codes into terminating errors.
- Pass Tauri CLI options directly through the pnpm script (for example `pnpm tauri:build --config ...`); do not insert a standalone `--` before them.
- Validate explicit CUDA/MPS availability before model inference and surface recovery guidance; never offer Apple MPS as a usable Windows device.
- Update `README.md`, `AGENTS.md`, and `DEVELOPMENT.md` at each completed project milestone, as requested.
- Use the approved tokens in `design-system/MASTER.md` for all production UI work.
- Use `public/receiver-mark.svg` as the canonical SysX mark; keep desktop bundle icons visually aligned with this SVG.
- Device status in the desktop header checks Medius by its official CH343 VID/PID and version handshake, and Ferrum through a valid `km.version()` reply. The current Ferrum probe uses the documented Legacy API on CP210x at 115200 baud; the App-managed Software API remains the preferred path when its endpoint can be configured. Keep status green only after a valid API reply; missing devices and handshake failures are red. Show Left, Right, Middle, Mouse4, and Mouse5 states from the physical input stream when supported; show unknown until the first event rather than guessing. This telemetry must not inject input. Medius uses the decoded input-event stream; Ferrum bitmap callback support depends on the active endpoint/build and needs hardware verification.

## Backend notes

- Backend modules live in `backend/src/receiver_backend`; tests live in `backend/tests`.
- UDP v1 uses the bounded `RCVR` chunk protocol in `udp.py`. Keep each datagram at or below 1200 bytes, validate metadata before retaining payloads, cap frame/reassembly memory, expire incomplete frames during socket-idle timeouts as well as while packets arrive, and preserve latest-frame semantics.
- Retry frame-source runtime errors every 3 seconds. Keep the current listener active while a requested replacement port is unavailable; once it binds, update runtime state and clear the recovered error.
- `LatestFramePipeline` must stay bounded and discard a result if strategy configuration changed while it was processing.
- Detection persistence runs outside the processing worker. Surface storage failures; never claim dropped records were saved.
- The `udp-send-image` and `udp-listen` CLI commands are a local protocol verifier, not a production sender or a substitute for desktop integration.
- The future OBS plugin must follow `docs/OBS_RECEIVER_PROTOCOL.md`; keep the document aligned with `udp.py` and label unimplemented sender behavior as future work.
- Run backend tests with `.venv/bin/python -m pytest backend/tests` from the project root.

