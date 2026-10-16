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
  builds do not enable this guard.
- After extraction, the release host hashes the backend against the exact
  embedded bytes before launch and rechecks that file about every two seconds;
  a modified or replaced backend terminates the host. Debug builds skip this
  integrity check.
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

For both VMProtect projects, enable anti-debug and anti-dump protections and
keep import protection enabled. Apply virtualization/mutation selectively to
the host startup and backend processing/movement functions; leave the normal
Windows/serial/WebView2 initialization paths unvirtualized if a protection
option changes runtime behavior. The protected backend must still accept the
`serve` command used by the host.

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
