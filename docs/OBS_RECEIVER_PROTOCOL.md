# Receiver Protocol for the SysX OBS Sender Plugin

This document specifies how the UI-less SysX OBS plugin sends frames to SysX. The protocol details below describe the implemented Rust receiver v1 behavior and the sender implementation in `obs-plugin/sysx-canvas-sender`; they are intended to prevent a sender/receiver mismatch.

## 1. Connection model

The OBS plugin sends successive JPEG-encoded images (MJPEG) over UDP to the SysX computer at the OBS render rate (optionally capped by `max_fps`). The receiver tells the plugin its Crop Resolution with a `SYSCFG` control datagram (section 2.1); the plugin crops that centered square on the GPU before encoding. SysX still applies its centered crop after decoding — an identity crop when the sizes match, so a sender that transmits full frames is also accepted — and sends that image to the detection engine; Preview displays the crop. Each datagram sequence is one full frame using SysX's versioned UDP packet format; this is not an HTTP multipart MJPEG endpoint. The plugin does not connect to the desktop UI, the private WebSocket, or Tauri IPC. V1 has no sender/session identifier, so the receiver is designed for **one active sender at a time**; simultaneous senders can collide on frame IDs and are not supported.

| Setting | Value |
|---|---|
| Transport | UDP over IPv4 |
| Receiver bind address | `0.0.0.0` (all local IPv4 interfaces) |
| Default UDP port | `4200` |
| Sender destination | Receiver computer's LAN IPv4 address and configured port |
| Payload | One JPEG image split into protocol chunks |
| Handshake / acknowledgement | None for frames; receiver → sender `SYSCFG` crop message (section 2.1) |

The Receiver starts listening automatically. Its preview changes from “Waiting for sender” on the first valid, processed frame. It has no Start, Pause, or Stop action. The UDP sender must not connect to the authenticated loopback WebSocket; that channel is private to the desktop UI and backend.

Allow inbound UDP on the selected Receiver port in the Windows firewall for the trusted private network. UDP v1 has no sender authentication, encryption, or source-IP allowlist. Do not expose the port to an untrusted network.

## 2. Datagram format, version 1

All integer fields use **network byte order (big-endian)**. Every datagram contains the 30-byte header followed by a non-empty part of the JPEG byte stream.

| Byte offset | Size | Field | Meaning |
|---:|---:|---|---|
| 0 | 4 | `magic` | ASCII `RCVR` |
| 4 | 1 | `version` | `1` |
| 5 | 1 | `flags` | Must be `0` in v1 |
| 6 | 8 | `frame_id` | Unsigned 64-bit frame identifier |
| 14 | 2 | `chunk_index` | Zero-based chunk index |
| 16 | 2 | `total_chunks` | Number of chunks in this frame; at least `1` |
| 18 | 2 | `width` | JPEG image width in pixels |
| 20 | 2 | `height` | JPEG image height in pixels |
| 22 | 8 | `timestamp_ns` | Sender timestamp in nanoseconds |
| 30 | variable | `payload` | JPEG bytes for this chunk |

The Rust packetizer and parser are in `rust-backend/src/udp.rs`; the header uses the equivalent 30-byte big-endian layout.

### 2.1 Receiver → sender control message

When the receiver sees datagrams from a new sender address, and again whenever Crop Resolution changes, it sends one UDP datagram back to that sender's source address and port:

```text
SYSCFG 1 CROP <resolution>
```

ASCII, no terminator, `<resolution>` in 1–8192. The plugin accepts it only from its configured receiver address/port, ignores anything else, and recreates its GPU crop resources for the new size. The message is not acknowledged and is not retransmitted; a sender that never receives it keeps its `default_crop`.

### Chunking rules

- The complete UDP datagram must be **1,200 bytes or smaller**, including its header.
- Header size is 30 bytes, so a full non-final chunk carries exactly 1,170 JPEG bytes.
- The final chunk carries 1–1,170 bytes. A JPEG whose length is an exact multiple of 1,170 has a full-sized final chunk.
- Send one datagram for each chunk, with `chunk_index` from `0` through `total_chunks - 1`.
- Chunk `0` must start with JPEG SOI bytes `FF D8`; the final chunk must end with JPEG EOI bytes `FF D9`. The Receiver checks these boundary markers before keeping those chunks in memory; an invalid boundary marker rejects and clears any partial assembly for that frame.
- Every chunk of one frame must repeat identical `frame_id`, `total_chunks`, dimensions, and timestamp.
- `total_chunks` must equal `ceil(jpeg_length / 1170)`. The JPEG must be no larger than 64 MiB.
- A frame must have dimensions from 1 through 8,192 pixels per side, with at most 33,554,432 pixels total.
- The dimensions in the header must exactly match the dimensions decoded from the JPEG. Do not crop or resize the image after setting the header. Each side must be at least the receiver's Crop Resolution, or the receiver rejects the frame.

Use a monotonically increasing `frame_id` per sender session and avoid reusing IDs while frames may still be in flight. The current Receiver uses the ID to associate chunks; it does not enforce monotonic ordering between completed frames.

`timestamp_ns` is carried into the frame and detection record. Use a monotonic capture timestamp if available. Sender and Receiver clocks are not synchronized, so this value is **not** a valid one-way network latency measurement. Current UI latency is processing latency, not sender-to-receiver latency.

## 3. Reassembly and overload behavior

The Receiver accepts chunks out of order and reconstructs a frame after all unique chunks arrive. Identical duplicate chunks are ignored. If a repeated chunk index has different bytes, or a frame's chunks disagree on metadata, that incomplete frame is discarded.

Current safety limits and behavior:

| Limit / condition | Current Receiver behavior |
|---|---|
| Datagram size | Rejects packets larger than 1,200 bytes or no larger than the header |
| Unsupported magic, version, flags, or invalid metadata | Rejects the packet |
| Incomplete frame timeout | Expires after 100 ms |
| Concurrent incomplete frames | At most 32; oldest incomplete assembly is evicted when full |
| Aggregate reassembly memory | At most 128 MiB |
| Per-frame encoded JPEG | At most 64 MiB |
| Completed frame waiting to be read | Keeps the newest completed frame; replacing a waiting frame counts as dropped |
| Missing JPEG SOI/EOI, non-JPEG, incomplete JPEG, or decoded-size mismatch | Rejects an invalid boundary chunk before retaining it; completed frames must have JPEG SOI/EOI markers and decoded dimensions matching the header |
| Processing falls behind | A bounded latest-frame pipeline replaces stale pending work and counts dropped input frames |

There are no retransmissions or per-frame acknowledgements. Send each frame once; do not block OBS rendering while waiting for UDP delivery. A partial frame expires and is dropped. Prefer a fresh frame over retransmitting stale video.

## 4. OBS plugin capture and send behavior (implemented)

The plugin:

1. Reads the OBS main texture in the main-rendered callback and copies the centered `N × N` crop (N from `SYSCFG`, capped by the canvas size) on the GPU, then reads it back through a three-surface staging ring so the render thread never waits on the readback it just queued. HDR/non-BGRA canvases are silently skipped. Object Detection also requires the crop to match the selected model input dimensions; mismatches are rejected, not resized.
2. JPEG-encodes the 8-bit BGRA crop with libjpeg-turbo (direct BGRA input, `jpeg_quality` default 80, 4:4:4 chroma by default so small Pixel Seek targets keep their color; `chroma_444 = 0` selects 4:2:0). The Receiver decodes JPEG to RGB, applies its center crop, then converts RGB → HSV for Pixel Seek.
3. Assign a frame ID and capture timestamp, split the JPEG into 1,170-byte payloads, write the v1 header in big-endian order, and send all chunks to the Receiver IP and port.
4. Run capture conversion, compression, and socket sends outside OBS's render callback. Use a bounded queue with latest-frame behavior; discard obsolete unsent frames if encoding or the network falls behind.
5. Send chunks in order for efficiency, while still tolerating receiver-side reordering. Do not use IP fragmentation or datagrams larger than the 1,200-byte protocol limit.
6. Reads its configuration from `%APPDATA%\obs-studio\plugin_config\sysx-canvas-sender\sender.ini` (created with defaults on first load: `host = 127.0.0.1`, `port = 4200`, `jpeg_quality = 80`, `max_fps = 0`, `chroma_444 = 1`, `default_crop = 640`), uses an 8 MiB socket send buffer, writes nothing to the OBS log, and drops encode/socket failures without blocking OBS. The protocol has no frame acknowledgement, so a successful local `sendto` call does not prove that the Receiver accepted or processed that frame.

Respect the cadence of the source OBS output. The requested 180–360 FPS is a system target to benchmark, not a promise that every source, encoder, network, Receiver, or UI can sustain it. At high rates, JPEG encoding and packets per second can become the bottleneck. Measure sender encode FPS, bytes per second, datagrams per second, Receiver source FPS, processing FPS, dropped frames, and preview FPS separately. Receiver source FPS counts valid decoded frames observed over a trailing one-second window. The desktop preview is currently published at a 60 FPS cadence by default; it is not intended to redraw 360 times per second.

### Pixel Seek color path

Pixel Seek exposes one selected target at a time: Yellow, Red, or Purple. The Rust receiver decodes frames to RGB, applies the centered crop, and converts RGB to HSV in `rust-backend/src/pixel_seek.rs`. HSV thresholds are code-defined and are not configurable in the UI. Detection runs across the processed crop; FOV filters candidates passed to target selection, while detections outside FOV remain available to Preview. Box coordinates are mapped with the crop origin. The OBS plugin does not choose thresholds or ROI and should send the same frame regardless of the selected color.

## 5. Receiver operator workflow

1. Install and launch the SysX desktop application on the secondary computer.
2. In the local web config, set the UDP port (default `4200`) and Crop Resolution to a square that fits the OBS source; allow that port through the private-network firewall if needed.
3. Install `sysx-canvas-sender.dll` into OBS's `obs-plugins\64bit` folder, start OBS once, then set `host` (the Receiver computer's reachable LAN address) and `port` in `sender.ini` and restart OBS.
4. For Object Detection, make the Crop Resolution match the active model input (square models are required by the square crop). Pixel Seek has no model-resolution requirement.
5. Start the stream from OBS. The Receiver automatically changes to its live preview when valid frames arrive. Use its source resolution, processing FPS, preview FPS, processing latency, drop counters, and status to diagnose behavior.
6. To verify the wire protocol during development, use the local `udp-send-image` / `udp-listen` protocol verifier. Those CLI commands are development verifiers, not the production OBS sender.

The repository includes `src/assets/red-cube-192.jpg`, a 192 × 192 JPEG fixture used by the UDP smoke checks. It validates JPEG transport and frame reconstruction only; it is not an OBS capture or evidence of production color profiles.

## 6. Sender interoperability checklist

Before calling the plugin compatible with the current Receiver, verify it with:

- 192×192 and non-square frame dimensions.
- A one-datagram JPEG and a multi-chunk JPEG.
- Out-of-order chunks and identical duplicates.
- A missing chunk, conflicting duplicate, mismatched chunk metadata, invalid JPEG, and decoded-size mismatch.
- Maximum allowed datagram size, invalid version/flags, and frame/aggregate memory limits.
- Strategy switch during a stream and source resolution changes.
- Sustained rate tests that record actual sender encode/network rates, Receiver processing/drop rates, and UI preview rate. Do not infer performance from the protocol verifier alone.

Verified so far (Windows 10, loopback): the plugin's headless `sysx-sender-selftest` delivered a 320×320 multi-chunk frame, a 200×120 frame with a padded source stride, and a 1×1 frame to `receiver-backend udp-listen` (3/3 decoded at the declared sizes, 0 invalid packets, 0 dropped/expired) and received the receiver's `SYSCFG 1 CROP 640`; its streaming mode also drove the live monitor at ~32 processed FPS. The headless `sysx-obs-host-check` loaded the static-CRT DLL inside the real OBS 32.2.2 `obs.dll` (D3D11, 1920×1080/60, no window); the plugin captured the 640×640 centered crop on the GPU and `udp-listen` received 300/300 frames with a clean unload. HDR check (Rec.2100 PQ canvas, RGBA16F main texture): the plugin loaded, sent nothing, and unloaded cleanly. Throughput (640×640, 4:4:4, quality 80, loopback, 5 s including startup): 1,171 frames at a 240 FPS render rate (234.2 FPS) and 1,784 frames at 360 FPS (356.8 FPS). The harness takes optional `<fps>` and `hdr` arguments after the seconds. The OBS Studio app UI itself, Windows network/firewall behavior between separate computers, and 180–360 FPS hardware measurements remain deployment work.
