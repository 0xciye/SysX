# Source audit fixes

The audit of `main` at `f876f50` found eight defects. This change addresses all eight without enabling the unavailable Object Detection runtimes or changing the Medius-only device support.

| Finding | Resulting behavior | Regression coverage |
|---|---|---|
| A01: profile deletion overwrites another profile | Deletion loads the remaining snapshot without saving the deleted profile into its successor | Every active/deleted index combination over three independent profiles preserves all surviving snapshots |
| A02: target flicker bypasses Cooldown | The last click's minimum interval survives target loss, trigger rearming and control resets | A 1000 ms cooldown prevents a second click at 2 ms; reacquisition Delay cannot shorten the interval |
| A03: failed disk save reports success | HTTP and network failures return an error; only confirmed disk saves update the browser cache | HTTP rejection tests and a headless browser probe with an unwritable data directory, an aborted request, and successful save/reload after recovery |
| A04: Auto controls wait for a new frame | Revisioned `auto_config` events reach the host independently of video; controls invalidate target freshness and cancel pending actions | No-frame enable/disable/trigger/timing edits; wire-event cancellation including rapid off/on; pre-edit callbacks cannot rearm Auto |
| A05: UDP delivers old frames after new ones | Reassembly rejects completed frames no newer than the last `(timestamp_ns, frame_id)` pair | Interleaved chunks, late old frames, duplicate completed frames, counter restart, clock restart after an idle gap and a changed sender endpoint |
| A06: bad JPEG closes the listener | Decode/size failures drop and count just that frame while the socket stays bound | Bad JPEG and decoded-size mismatch followed immediately by a valid frame on the same listener |
| A07: recovered error remains active | Current pipeline/source errors are separate from the retained diagnostic history | Occupied-port failure followed by automatic successful bind reports a null current error while preserving history |
| A08: Windows smoke expects only Edge and the old lifecycle | Smoke finds the native WebView2 or dedicated Edge monitor HWND and closes it normally; `--smoke-test` suppresses automatic Config opening | PowerShell AST parse and C# helper compilation; actual Windows lifecycle execution remains required |

## Validation

On the Linux cloud validation host:

- Backend: 88 tests passed, 4 ignored, using `ort/load-dynamic`.
- UI: 11 tests passed, and the updated WASM UI built successfully.
- Native logic: 26 tests passed through a harness importing the actual Auto, movement, serial queue, SDK adapter and single-instance modules; the Medius SDK used its mock feature.
- The existing WASM stripping helper has 2 tests; no stripping behavior changed.
- The Windows host and its test code passed `cargo check --tests --target x86_64-pc-windows-msvc`; this is a compile check, not a Windows executable/runtime test.
- Headless Chromium with the actual backend and updated WASM verified error reporting for a disk-write failure and a network failure, successful save/apply/reload after recovery, Auto disable propagation without another frame, a valid frame immediately after a malformed JPEG, and clearing the error after an occupied-port retry.
- The smoke script parsed under PowerShell and its Win32 window helper compiled. Win32 calls were not executed on Linux.

Default ORT binary download returned HTTP 403 in this environment; dynamic loading was used for backend validation, without loading a model or validating inference. Object Detection, ONNX Runtime, OpenVINO and TensorRT remain unavailable for use.

No physical Medius, Windows application lifecycle, OBS DLL, game tracking, LAN performance or VMProtect release was tested. The test harness and synthetic frames do not establish USB/game event timing or hardware throughput.

## Behavior details

Auto Delay still starts on a center crossing and survives the target leaving the center. Trigger release or a configuration revision cancels the pending action. The previous click's Cooldown deadline remains a minimum interval; Sleep only controls polling cadence. Config events use timestamp zero and no valid target, and cannot make a stale target fresh. Frames carry the Auto configuration revision used for processing so an older callback cannot undo a later control change.

UDP v1 still supports one sender at a time and has no session identifier. A changed sender endpoint or a gap of at least one second between valid chunks clears ordering/partial assemblies for a new session. With a continuous monotonic capture clock, an OBS frame-counter restart can be accepted immediately. Corrupt completed frames increment `invalid_frames` and transport drops; they do not trigger the three-second socket recovery path. See [the sender protocol](OBS_RECEIVER_PROTOCOL.md).

Run `scripts/smoke-windows-app.ps1` on a free Windows machine for authoritative app lifecycle validation. It opens a monitor window and refuses to disturb an existing monitor or process owning UDP 4200. Normal app launches still open Config; only the explicit `--smoke-test` launch suppresses it.
