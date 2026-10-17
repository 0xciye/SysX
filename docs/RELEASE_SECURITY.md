# SysX release security

SysX is a native desktop application, so a determined user who controls the
machine can always patch, dump, or instrument it. The release hardening below
raises the cost of casual debugging and keeps source/PDB artifacts outside the
portable runtime; matching native PDBs are available in separate archives. It is not a replacement for licensing or a server-side trust
decision.

## What the source now does

- Native release profiles use fat LTO, one codegen unit, full PDB debug information (`debug = 2`, `strip = "none"`), disabled debug assertions and `panic = "abort"`. PDBs are stored separately. The WASM UI still strips its debug metadata. Retaining symbols does not disable the release-only anti-debug guard.
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
  security boundary; sensitive runtime logic stays in the native backend.
- `src-tauri/build.rs` embeds the release backend and the complete Trunk UI
  bundle. The final distribution contains one `SysX.exe`; the backend is only
  extracted to `%LOCALAPPDATA%\SysX\runtime` while the app is running.
- A Windows release host fails the build if either the backend sidecar or the
  Trunk UI bundle is missing. This prevents an apparently successful but
  unusable executable from being packaged.
- The random loopback IPC token remains per launch. No license secret or
  reusable authentication credential is stored in the executable or repository.

## Build and package

Build the release EXE on Windows x64:

```powershell
.\scripts\build-windows.ps1
.\scripts\package-windows.ps1 -SkipBuild
```

The normal EXE includes every built-in release security feature described above.
VMProtect is neither invoked nor required by build/packaging; the preparation
script and mandatory protection flags were removed at the user's request.
Packaging still validates x86-64 PE format and excludes debug/source/map files
and the separate backend. Pixel Seek requires no ONNX Runtime installation;
Object Detection remains unavailable. The target computer needs WebView2/Edge
and the Microsoft Visual C++ x64 runtime.

## Optional manual third-party protection

The user manages VMProtect separately after the build. Protecting only the outer
host does not protect the backend extracted at runtime. If manually protecting
the backend, embed that exact final backend before protecting the host, so the
build-time integrity manifest matches its bytes. Preserve CLI entry points and
exclude receive/decode, detection, targeting, movement, queue, UI and hashing
loops from virtualization. Re-run Windows lifecycle and device checks on the
final protected output; no current raw-build result validates that output.

To package an existing final executable without rebuilding:

```powershell
.\scripts\package-windows.ps1 -SkipBuild -ExecutablePath .\SysX-final.exe
```

## Future authentication

When authentication is added, keep the authority server-side. The client may
cache a short-lived entitlement, but must not contain a reusable API secret.
Authentication should fail closed for the protected product path without
changing the vision or movement algorithms.

## Crash diagnostics

Native diagnostics are installed before the release guard. Release panic strategy must remain `unwind` so Rust panic hooks can record the panic and backtrace. Text logs and panic backtraces go to `%LOCALAPPDATA%\SysX\logs`; each Rust panic or unhandled Windows exception also writes a `*-crash.txt` with the reason and all logs from that process session. A best-effort minidump is written alongside it. The host writes a session marker only after singleton acquisition and clears it on normal shutdown; if it remains at the next launch, an `*-unclean-exit.txt` includes the previous session logs and labels the cause unknown. Backend protocol stdout remains piped to the host; only stderr is redirected to a log file, and the host records unexpected backend exit status. Settings/logs/dumps/PDBs stay outside immutable runtime verification. No routine frame/target/device callback writes diagnostic files. Hard termination, power loss, severe corruption, or a replaced exception handler may prevent a report or dump. Dumps may contain process stack data and should be handled privately.
