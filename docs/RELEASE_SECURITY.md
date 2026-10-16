# SysX release security

SysX is a native desktop application, so a determined user who controls the
machine can always patch, dump, or instrument it. The release hardening below
raises the cost of casual debugging and keeps source/debug artifacts out of the
distribution. It is not a replacement for licensing or a server-side trust
decision.

## What the source now does

- Release profiles use fat LTO, one codegen unit, no debug information, stripped
  symbols, and `panic = "abort"` for the Rust host, backend, and UI.
- The Windows MSVC target enables Rust/LLVM Control Flow Guard through
  `.cargo/config.toml`, and explicitly requests ASLR, high-entropy ASLR, and
  NX/DEP linker flags. These are platform mitigations against invalid indirect
  control-flow targets and executable memory without changing application
  logic.
- The host and backend set Windows image-load and extension-point mitigation
  policies in release builds, rejecting remote/low-integrity images and common
  global extension injection paths. Unsupported policies are ignored so the
  normal runtime remains compatible.
- The Windows release host and backend refuse to start under
  `IsDebuggerPresent`/`CheckRemoteDebuggerPresent` or the native process debug
  port/object/flags checks; a small release-only watchdog checks again every
  two seconds and terminates the process tree if it trips. Debug and test
  builds, including release-profile tests, do not enable this guard.
- The build embeds a SHA-256/length manifest for the final backend and every UI
  asset (HTML/JS/Wasm/CSS/icons). The release host verifies the extracted files
  **before** launching the sidecar or opening the UI. The expected hashes come
  from trusted build inputs, not an editable runtime manifest or a new baseline
  learned from the extracted files. Missing, patched, resized, linked/reparse
  or unexpected runtime files fail verification. Settings, calibrations, model
  imports and session output remain mutable outside the immutable runtime.
- On Windows the verifier keeps read handles without write/delete sharing,
  preventing ordinary in-place writes, deletion and replacement while running.
  A separate worker checks the manifest/layout again after a two-second wait
  between complete sweeps. Hashing uses 256 KiB chunks with interruptible 2 ms
  waits, attempts Windows background CPU/I/O priority for that worker only, and
  takes no frame/target/device locks. Sweep duration depends on file sizes,
  storage and timer/scheduler granularity; two seconds is not a guaranteed
  maximum detection delay. A failed sweep kills the owned backend through its
  process handle; backend disconnect then closes the host and releases runtime
  files. Debug/test launches do not automatically enable integrity enforcement.
- The release UI post-process removes the Wasm `name`, `.debug_*`, and source
  map custom sections before embedding the bundle, reducing symbol/source-map
  metadata in the UI artifact. WASM remains a client artifact rather than a
  security boundary; sensitive runtime logic stays in the protected backend.
- `src-tauri/build.rs` embeds the release backend and the complete Trunk UI
  bundle. The final distribution contains one `SysX.exe`; the backend is only
  extracted to `%LOCALAPPDATA%\SysX\runtime` while the app is running.
- A Windows release host fails the build if either the backend sidecar or the
  Trunk UI bundle is missing. This prevents an apparently successful but
  unusable executable from being sent to VMProtect.
- The random loopback IPC token remains per launch. No license secret or
  reusable authentication credential is stored in the executable or repository.

## VMProtect workflow

Protect both native images. Protecting only the outer host leaves the extracted
backend easy to inspect while SysX is running.

1. On a clean Windows x64 build machine, stage the backend:

   ```powershell
   .\scripts\prepare-vmprotect.ps1 -Phase StageBackend
   ```

   This produces `dist-vmprotect\receiver-backend.raw.exe`.

2. Open that file in VMProtect. Keep the command-line entry points needed by
   the normal build smoke (`serve`, `udp-listen`, and `udp-send-image`) and
   export the protected result as:

   ```text
   dist-vmprotect\receiver-backend.protected.exe
   ```

3. Embed the protected backend and build the one-file host:

   ```powershell
   .\scripts\prepare-vmprotect.ps1 `
     -Phase Finalize `
     -ProtectedBackendPath .\dist-vmprotect\receiver-backend.protected.exe
   ```

   The final unprotected input for the second VMProtect pass is:

   ```text
   dist-vmprotect\SysX.exe
   ```

4. Protect that final host in VMProtect and save the result as:

   ```text
   dist-vmprotect\SysX-vmprotected.exe
   ```

For both VMProtect projects, enable the required anti-tamper/memory-protection
options and keep import protection compatible with the Windows loader and CFG.
Apply virtualization/mutation selectively to cold startup/security/licensing
code. **Exclude frame receive/decode, Pixel Seek/targeting, the movement ticker,
serial queue, UI event loop and SHA-256 implementation from virtualization**;
those paths need predictable throughput. Do not add anti-debug queries, file
hashes or timing probes to per-frame/per-move callbacks. Leave ordinary
Windows/serial/WebView2 initialization compatible with the vendor runtimes.
The protected backend must still accept the `serve` command used by the host.

VMProtect is an external packaging step; this repository does not establish
which options were applied to a final binary. Compare raw and protected output
at the same frame rate and run the Windows lifecycle/device smoke on that exact
final executable. CFG, loader policies and VMProtect can affect compatibility;
Linux tests cannot validate those Windows-specific interactions.

5. Package the protected host without rebuilding the raw executable:

   ```powershell
   .\scripts\package-windows.ps1 `
     -SkipBuild `
     -ExecutablePath .\dist-vmprotect\SysX-vmprotected.exe `
     -VmProtected
   ```

   The archive contains the selected executable under the fixed name
   `SysX.exe`; the raw backend is never copied beside it.

`scripts/package-windows.ps1` refuses to package the raw build by default and
requires the explicit `-VmProtected` acknowledgement for a final host. Use
`-AllowUnprotected` only when making a local smoke package that must not be
distributed. The script cannot independently prove that a third-party
VMProtect project was applied, but it does verify the selected file is an
x86-64 PE and that the staged package has no sidecar, debug, source, or map
artifacts.

The shipped folder can contain only the final `SysX.exe`. The machine still
needs WebView2/Edge and any selected OpenVINO, TensorRT, or CUDA runtime, and
the user imports model artifacts after installation; those dependencies are
not secrets and are not embedded in the executable.

## Future authentication

When authentication is added, keep the authority server-side. The client may
cache a short-lived entitlement, but must not contain a reusable API secret.
Authentication should fail closed for the protected product path without
changing the vision or movement algorithms.
