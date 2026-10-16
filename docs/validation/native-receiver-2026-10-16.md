# Native Slint receiver validation — 2026-10-16

The receiver is a native Slint window. The existing monitor's dimensions, dark palette, header/profile/device status, square feed, measured HUD, FOV, detection corners, target reticle/lead and five-button row are retained. No Wry, WebView2, Tao, Win32/GDI UI renderer or Edge/browser receiver fallback remains. The browser serves settings only; `/monitor.html` is explicitly 404 and the config boot placeholder has no preview.

Checks run on Linux:

- Native host Rust tests: **47 passed**, including real local WebSocket envelope/JPEG/button-state reception, bounded pending-frame replacement, size/malformed-message rejection, JPEG dimension mismatch, stale-frame expiry, lifecycle regressions and headless Slint rendering.
- Shared/config Rust tests: **11 passed**.
- Browser-route integration: **1 passed**, proving `/config.html` serves settings-only boot markup and `/monitor.html` returns 404.
- Windows MSVC target `cargo check --tests`: **passed**. This checks Windows-specific code but does not link/run a Windows executable.
- Browser config WASM `cargo check`: **passed**.
- Actionlint, edited PowerShell parsers and `git diff --check`: **passed**.

The headless render test captures waiting/live/narrow layouts with the Slint software renderer, using the repository's 192×192 red-cube JPEG fixture. Linux review uses DejaVu font substitutes; production retains Segoe UI and Consolas on Windows. Fixtures display synthetic 240 FPS, 2.4 ms and a connected Medius/right-button state: these are test inputs, not hardware or throughput measurements. SVG overlay primitives are rendered natively by Slint so the corner/reticle paths also work on its software renderer.

The JPEG decoder runs off the UI thread, stores only one pending JPEG, and publishes only the newest decoded image. UI wake-ups coalesce. Existing backend processing, Medius transport, security/integrity, separate PDB packaging and OBS DLL are unchanged. GPU selection and software fallback are provided by Slint's winit backend.

No GitHub build was dispatched for this migration. Windows linking, title-bar/focus/reopen behavior, GPU/driver behavior and physical Medius/OBS/LAN delivery remain unvalidated by these checks. A native UI migration does not establish that the previously reported device/transport errors are fixed.
