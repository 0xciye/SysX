# Runtime protection and continuous operation

Validated on 2026-10-16 against the source-audit fixes at `ef2fc21` plus the
runtime-integrity change in this PR. Windows application/device execution and
the final VMProtect output were not available.

## Protection changes

- Build-time SHA-256 and lengths cover the final embedded backend and every
  immutable UI asset. Verification happens before backend launch, closing the
  previous launch-before-verification window.
- Startup checks reject missing, changed, resized, linked/reparse and extra
  files. Windows read handles deny write/delete sharing while running; periodic
  scans also check the current path so a replaced pathname cannot hide behind
  an old open handle.
- Hashing runs on its own thread: 256 KiB per chunk, interruptible 2 ms pacing,
  and a two-second wait between full sweeps. The Windows worker requests
  background CPU/I/O priority. No vision, movement or device lock is acquired by
  the verifier. Backend ownership locks are used only on failure/shutdown.
- A violation stops the known backend using its owned process handle, rather
  than finding a process by a reusable PID or looking up a termination program
  through PATH. Backend disconnect drives normal host cleanup.
- Integrity and debugger enforcement stay off for debug/test launches, including
  release-profile tests. User settings, calibration and session writes are
  outside the immutable runtime and remain editable.

This protects extracted runtime files while trusting the original host. It is
not host authenticity verification, an in-memory code-patch detector, or a
guarantee against an administrator patching the host itself. VMProtect/signing
and independent distribution verification remain separate release decisions.

## A/B measurement

Linux cloud host; optimized Rust backend and stripped release WASM assets. The
actual production integrity module ran in an optimized companion harness,
matching the host/sidecar process separation without needing a Windows GUI.
The controlled test manifest covered 3,573,541 bytes. Production uses the
manifest compiled into the host; the harness used a fixture manifest outside
the runtime. There is no product switch to bypass release checks.

Four 12-second trials in OFF/ON/ON/OFF order, with two warm-up seconds and about
ten measured seconds each. Input: synthetic moving yellow marker in 256×256
JPEG, requested 240 FPS, sensitivity 0.25, FOV 360°, recoil off, synthetic held
Right-button telemetry. The target does not respond to emitted moves; this
checks continuity, not closed-loop/game tracking accuracy.

| Metric | Integrity OFF, two trials | Integrity ON, two trials |
|---|---:|---:|
| Requested sender rate | 240 FPS | 240 FPS |
| Measured sender rate | 240.09–240.10 FPS | 240.10 FPS |
| Source FPS at end of trial | 239.91–240.05 | 239.92–240.11 |
| Measured processing FPS | 240.00–240.09 | 240.00 |
| Preview messages received per second | 239.99–240.00 | 240.00–240.60 |
| Sender skipped slots | 0 | 0 |
| Input / transport drops | 0 / 0 | 0 / 0 |
| Invalid packets / frames | 0 / 0 | 0 / 0 |
| Per-frame processing p50 | 0.063 ms | 0.063 ms |
| Per-frame processing p99 | 0.127–0.142 ms | 0.144–0.145 ms |
| Backend CPU, percentage of one core | 10.20–10.50% | 10.10–10.20% |
| Integrity worker CPU, percentage of one core | — | about 0.10% |
| Movement-event timestamp gap p99 | 9 ms | 9 ms |
| Largest movement-event timestamp gap | 10–11 ms | 10 ms |

Preview numbers measure raw WebSocket reception, not canvas drawing; short
measurement boundaries/queued messages can make that rate slightly higher
than the source rate. Movement-event intervals include batching and scheduling
and do not establish a 4 ms physical-device deadline. CPU sampling used Linux
process counters with coarse tick resolution. Two trials per condition are not
a statistical proof of zero overhead or a sustained hardware benchmark.

Integrity startup verification took 2.76–3.92 ms in the ON trials with cached
files. A separate paced pass read 3,573,541 bytes in 20 chunks over 44.06 ms.
Larger/protected binaries, cold disks, antivirus and Windows timer granularity
can increase these times. The two-second scan interval is a wait between
sweeps, not a maximum time to detect tampering.

No throughput drop, input/transport drop or guard-triggered interruption was
observed in these trials. The small p99 processing difference is within the
limits of this short shared-host experiment; it does not justify claiming zero
cost. An exploratory debug build only reached roughly 95 FPS on the same host
and is not used as release performance evidence.

## Existing anti-reverse-engineering controls

Raw trial values: [Linux A/B results](validation/runtime-integrity-linux.json).

| Technique | Where cost occurs | Continuity/compatibility assessment |
|---|---|---|
| Native debugger queries | Startup and two-second Windows watchdog, outside frame/ticker loops | Intentionally terminates under a debugger; Windows API cost and profiler compatibility were not measured here |
| SHA-256/layout checks | Startup plus paced background sweeps | Small measured file-guard cost above; never blocks frame/target/device locks, but still shares CPU/storage resources |
| Read locks | Windows file open and lifetime sharing rules | Deny runtime mutation while permitting readers; ordinary settings remain outside the lock set; Windows lock behavior is compile-checked but not runtime-tested here |
| Image-load / extension-point policies | Windows startup/loader | May reject remote/low-integrity DLLs or extension hooks; unsupported policies are nonfatal; standard Pixel Seek path does not need those hooks |
| CFG, ASLR, NX/DEP | Linker/Windows runtime | Source/build flags inspected; Windows/VMP compatibility and overhead still require the final executable |
| LTO / stripped native symbols / stripped WASM metadata | Build/startup/debug tooling | No new per-frame integrity operation; optimized build sustains the synthetic stream in this test |
| VMProtect virtualization/mutation/anti-dump | Depends on the external project and protected functions | Not applied or tested here. Virtualizing vision, movement, queue, UI or hashing loops can materially harm throughput; exclude those paths and benchmark exact final output |

## Verification and remaining checks

- 88 backend tests passed, 4 ignored.
- 34 portable native logic tests passed, including 8 integrity tests for startup
  mutations, repeated valid scans/stop, path validation, cancellation, injected
  files, replacement paths, matching-content symlinks and watchdog detection.
- Build-script fixtures reject a missing release sidecar/index and their
  generated backend/UI hashes match an independent SHA-256 implementation.
- The Windows host/test code passes cross-target `cargo check`; production
  release-guard modules and Windows-only read-lock tests also compile in the
  standalone validation harness.
- Updated backend and WASM build in release mode; WASM metadata stripping runs
  on the assets used for the continuity experiment.
- Headless Chromium with the optimized backend/WASM and an active integrity
  worker passed disk/network save-failure reporting, successful save/apply/
  reload, no-frame Auto disable propagation, malformed-JPEG recovery and
  occupied-port recovery. The worker did not trip during legitimate settings
  writes or these source/configuration transitions.
- An integration probe modified a UI file at the same length while an actual
  optimized backend was alive. The production scanner/owned-child failure
  handler detected the SHA-256 mismatch, stopped that backend and released its
  UDP port after about 2.03 seconds. This was Linux detection after a write,
  not execution of Windows write-prevention locks. [Raw failure result](validation/runtime-integrity-tamper-linux.json).
- The Windows read-lock test, GUI lifecycle, physical Medius output, USB/game
  response, OBS DLL/LAN operation and protected VMProtect binary remain unrun.
  Object Detection and its runtimes remain unavailable; backend builds used
  `ort/load-dynamic` without validating inference.

On Windows, run the native tests and the existing app smoke, then repeat the
same input against the exact raw and final protected executable. Check startup,
WebView2/Edge, SDK handshake/button telemetry, settings/profile edits, steady
FPS/latency/drop counts, normal shutdown, and that runtime writes/deletes are
denied or detected. A security violation deliberately stops operation; healthy
operation must continue without false trips.
