# Receiver Protocol for a Future OBS Sender Plugin

This document specifies how a future OBS plugin should send frames to the current Receiver. The plugin is not part of this repository yet. The protocol details below describe the implemented Receiver v1 behavior; they are intended to prevent a sender/receiver mismatch.

## 1. Connection model

The OBS plugin captures the configured region, crops it at the sender, and sends successive JPEG-encoded frames (MJPEG) over UDP to the Receiver computer. Each datagram sequence is one cropped frame using the Receiver's versioned UDP packet format; this is not an HTTP multipart MJPEG endpoint. The plugin does not connect to the desktop UI, Python WebSocket, or Tauri IPC. V1 has no sender/session identifier, so the current Receiver is designed for **one active sender at a time**; simultaneous senders can collide on frame IDs and are not supported.

| Setting | Value |
|---|---|
| Transport | UDP over IPv4 |
| Receiver bind address | `0.0.0.0` (all local IPv4 interfaces) |
| Default UDP port | `4200` |
| Sender destination | Receiver computer's LAN IPv4 address and configured port |
| Payload | One JPEG image split into protocol chunks |
| Handshake / acknowledgement | None |

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

Python reference layout: `struct.Struct("!4sBBQHHHHQ")`. The current packetizer and parser are in `backend/src/receiver_backend/udp.py`.

### Chunking rules

- The complete UDP datagram must be **1,200 bytes or smaller**, including its header.
- Header size is 30 bytes, so a full non-final chunk carries exactly 1,170 JPEG bytes.
- The final chunk carries 1–1,170 bytes. A JPEG whose length is an exact multiple of 1,170 has a full-sized final chunk.
- Send one datagram for each chunk, with `chunk_index` from `0` through `total_chunks - 1`.
- Chunk `0` must start with JPEG SOI bytes `FF D8`; the final chunk must end with JPEG EOI bytes `FF D9`. The Receiver checks these boundary markers before keeping those chunks in memory; an invalid boundary marker rejects and clears any partial assembly for that frame.
- Every chunk of one frame must repeat identical `frame_id`, `total_chunks`, dimensions, and timestamp.
- `total_chunks` must equal `ceil(jpeg_length / 1170)`. The JPEG must be no larger than 64 MiB.
- A frame must have dimensions from 1 through 8,192 pixels per side, with at most 33,554,432 pixels total.
- The dimensions in the header must exactly match the dimensions decoded from the JPEG. Do not crop or resize after setting the header.

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

## 4. OBS plugin capture and send requirements

The future plugin should:

1. Capture the OBS output at the exact intended source resolution. Object Detection requires a source size equal to the selected model's configured width and height; UDP frames with a mismatch are rejected. Pixel Seek accepts valid arbitrary resolutions.
2. Convert the captured output to an 8-bit sRGB image and JPEG-encode it without changing width or height. The Receiver decodes JPEG to OpenCV BGR and converts that image to HSV for Pixel Seek.
3. Assign a frame ID and capture timestamp, split the JPEG into 1,170-byte payloads, write the v1 header in big-endian order, and send all chunks to the Receiver IP and port.
4. Run capture conversion, compression, and socket sends outside OBS's render callback. Use a bounded queue with latest-frame behavior; discard obsolete unsent frames if encoding or the network falls behind.
5. Send chunks in order for efficiency, while still tolerating receiver-side reordering. Do not use IP fragmentation or datagrams larger than the 1,200-byte protocol limit.
6. Surface sender-side configuration, encode, and socket errors in the OBS plugin. The current protocol has no Receiver acknowledgement, so a successful local `sendto` call does not prove that the Receiver accepted or processed that frame.

Respect the cadence of the source OBS output. The requested 180–360 FPS is a system target to benchmark, not a promise that every source, encoder, network, Receiver, or UI can sustain it. At high rates, JPEG encoding and packets per second can become the bottleneck. Measure sender encode FPS, bytes per second, datagrams per second, Receiver source FPS, processing FPS, dropped frames, and preview FPS separately. Receiver source FPS counts valid decoded frames observed over a trailing one-second window. The desktop preview is currently published at a 60 FPS cadence by default; it is not intended to redraw 360 times per second.

### Pixel Seek color path

Pixel Seek exposes one selected target at a time: Yellow, Red, or Purple. The Receiver converts decoded sRGB JPEG channel values to HSV using OpenCV (`BGR` input to `HSV`). The code-defined HSV ranges are stored in `PIXEL_COLOR_PROFILES` in `backend/src/receiver_backend/service.py`. All profiles are currently empty; the Receiver intentionally reports no color detections until a real profile is added. A configured `PixelColorRule` may also specify a normalized source-frame ROI `(left, top, right, bottom)`; omitting it means the full frame. Output box coordinates remain relative to the original frame. The OBS plugin does not choose thresholds or ROI and should send the same frame regardless of the selected color.

## 5. Receiver operator workflow

1. Install and launch the Receiver desktop application on the secondary computer.
2. Confirm the UDP port in **Settings → Input** (default `4200`) and allow that port through the private-network firewall if needed.
3. Configure the future OBS plugin with the Receiver computer's reachable LAN address and the same UDP port.
4. Match the OBS output dimensions to the active model dimensions when Object Detection is selected. Pixel Seek has no model-resolution requirement.
5. Start the stream from OBS. The Receiver automatically changes to its live preview when valid frames arrive. Use its source resolution, processing FPS, preview FPS, processing latency, drop counters, and status to diagnose behavior.
6. To verify the wire protocol during development, use the local `udp-send-image` / `udp-listen` protocol verifier. Those CLI commands are development verifiers, not the production OBS sender.

The repository includes `src/assets/red-cube-192.jpg`, a 192 × 192 JPEG fixture used by the UDP smoke checks. It validates JPEG transport and frame reconstruction only; it is not an OBS capture or evidence of production color profiles.

## 6. Sender interoperability checklist

Before calling a future plugin compatible, verify it against the current Receiver with:

- 192×192 and non-square frame dimensions.
- A one-datagram JPEG and a multi-chunk JPEG.
- Out-of-order chunks and identical duplicates.
- A missing chunk, conflicting duplicate, mismatched chunk metadata, invalid JPEG, and decoded-size mismatch.
- Maximum allowed datagram size, invalid version/flags, and frame/aggregate memory limits.
- Strategy switch during a stream and source resolution changes.
- Sustained rate tests that record actual sender encode/network rates, Receiver processing/drop rates, and UI preview rate. Do not infer performance from the protocol verifier alone.

The protocol verifier is currently exercised for local loopback frames. A real OBS plugin, Windows network/firewall validation, interoperability tests on separate computers, and 180–360 FPS hardware measurements remain future work.
