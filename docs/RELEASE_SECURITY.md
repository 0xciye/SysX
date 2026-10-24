# SysX release security

SysX has no built-in protection layer (user request, 2026-10-24). Anyone who
controls the machine can patch, dump or instrument it. It is not a replacement
for licensing or a server-side trust decision.

## What the source does

- Native release profiles use fat LTO, one codegen unit, no debug information
  (`debug = 0`, `strip = "symbols"`), disabled debug assertions and
  `panic = "unwind"` (the pipeline and UI recover from panics with
  `catch_unwind`). Dev and test profiles also build without debug information
  (`debug = 0`): `rustc` 1.99.1 crashed with `STATUS_ACCESS_VIOLATION` on the
  user's machine whenever it emitted debug information.
- No PDBs, error logs, crash reports or minidumps are produced. The backend's
  stderr goes to `Stdio::null()`.
- No linker hardening flags are set (the former `.cargo/config.toml` with
  `/DYNAMICBASE`, `/HIGHENTROPYVA` and `/NXCOMPAT` was removed); the MSVC
  linker defaults apply. There is no Control Flow Guard, debugger guard,
  image-load mitigation policy or runtime integrity check.
- The random loopback IPC token remains per launch (kept at the user's
  choice): only the host that started the backend can control it. No license
  secret or reusable authentication credential is stored in the executable or
  repository.
- `src-tauri/build.rs` embeds the release backend sidecar. The final
  distribution contains one `SysX.exe`; the backend is extracted to
  `%LOCALAPPDATA%\SysX\runtime` while the app is running. A Windows release
  host fails the build if the backend sidecar is missing.
- The desktop distribution uses native Slint for Preview and Settings. There
  is no browser config page, WASM UI bundle, or config HTTP endpoint.

## Build and package

Build the release EXE on Windows x64:

```powershell
.\scripts\build-windows.ps1
.\scripts\package-windows.ps1 -SkipBuild
```

VMProtect is neither invoked nor required by build/packaging. Packaging still
validates x86-64 PE format and excludes debug/source/map files and the separate
backend. Pixel Seek requires no ONNX Runtime installation; Object Detection
remains unavailable. The target computer needs the Microsoft Visual C++ x64
runtime.

## Optional manual third-party protection

The user manages VMProtect separately after the build. Protecting only the outer
host does not protect the backend extracted at runtime. If manually protecting
the backend, embed that exact final backend before protecting the host. Preserve
CLI entry points and exclude receive/decode, detection, targeting, movement,
queue and UI loops from virtualization. Re-run Windows lifecycle and device
checks on the final protected output.

To package an existing final executable without rebuilding:

```powershell
.\scripts\package-windows.ps1 -SkipBuild -ExecutablePath .\SysX-final.exe
```

## Future authentication

When authentication is added, keep the authority server-side. The client may
cache a short-lived entitlement, but must not contain a reusable API secret.
