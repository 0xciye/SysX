# Medius API

Implementation reference for connecting SysX to a Medius box, identifying the box and attached peripheral, checking readiness, and sending trigger-gated detection-derived mouse movement. This is a summary of the official API documentation, not a verbatim copy.

**Documentation checked:** 2026-10-15  
**Current documented protocol:** 9  
**Official Rust client checked:** `medius` 3.4.3 (Rust 2024 edition, MSRV 1.85)

## 1. Device and transport

Medius is firmware and a binary control protocol for a USB passthrough box. A real mouse/keyboard is passed through to a target PC. A separate USB-serial control connection lets a program inspect the box and inject input.

| Setting | Value |
|---|---|
| USB-serial bridge | WCH CH343 |
| USB VID/PID | `0x1A86:0x55D3` |
| Windows port | `COMx` |
| Baud | Fixed `6,000,000` |
| Framing | `8N1` |
| Protocol | Binary from the first byte; no text console or baud negotiation |

Hardware ports in the documented layout: USB1 clone → target/game PC, USB2 control → computer running SysX, USB3 → physical mouse. The hardware guide warns not to connect USB1 and USB3 to the same computer because of the 5V back-feed risk.

## 2. Discovery, handshake, and identity

### Recommended Rust client path

The official Rust crate should own port scanning, framing, CRC, response matching, handshake, reader threads, and teardown. Avoid writing a second raw-protocol stack in SysX.

```rust
use medius::Device;

// Finds a Medius VID/PID port, opens it, and validates the protocol handshake.
let device = Device::find()?;
let version = device.query_version()?;
let health = device.query_health()?;
let peripheral = device.device_info()?;
```

For multiple boxes:

```rust
use medius::Device;

for box_info in Device::list() {
    println!("{} {} {}", box_info.id(), box_info.name(), box_info.device);
}

// Reopen the same physical box after COM-port renumbering.
let device = Device::open_by_id("0123456789ab")?; // stable MAC hex
```

Discovery operations:

| API | Purpose | Does it verify the protocol? |
|---|---|---|
| `find_medius()` | Lists ports matching the CH343 VID/PID. | No; this is port discovery only. |
| `Device::open(path)` | Opens a known COM/serial path and performs handshake. | Yes. |
| `Device::find()` | Opens the first discovered Medius box. | Yes. |
| `Device::list()` | Opens each candidate, handshakes, reads version/peripheral identity, then closes it. | Yes. |
| `Device::open_by_id(id)` | Reopens by stable MAC hex or CH343 serial identity. | Yes. |
| `device.reconnect()` | Reconnects to the same identity after a link loss. | Yes; does not silently adopt another box. |

Handshake uses `QUERY(VERSION)` and requires `proto_ver == 9`. A version reply includes protocol and firmware versions, a stable 6-byte MAC, and the human-readable box name. A ready `RESP(VERSION)` may also arrive unsolicited at boot or first contact. A timeout, unsupported protocol, bad frame, or failed serial open is a connection error, not a connected device.

### Two different names

Do not conflate the box and the physical peripheral:

- `query_version()` → `Version.name`: the Medius box name. It can be a generated default such as `Medius-XXXX`.
- `device_info()` → `DeviceInfo.product`, `vid`, `pid`, `kind`: the product and USB identity of the physical mouse/keyboard cloned by the box. It can be empty/zero if no peripheral is attached.
- `Version.mac_hex` / `BoxInfo.id`: stable Medius box identity, useful for reconnect and user selection.

For a header that simply says which Medius box is connected, show `Version.name`. For a peripheral label, call `device_info()` too and show `product`; keep the box name available as a separate identity.

## 3. Wire protocol (if a native client is ever required)

```text
[SOF 0xA5][TYPE u8][SEQ u8][LEN u16 LE][PAYLOAD 0..512][CRC16 u16 LE]
```

- Maximum payload: 512 bytes; maximum frame: 519 bytes.
- Multi-byte integers are little-endian.
- CRC16-CCITT covers `TYPE | SEQ | LEN | PAYLOAD`; polynomial `0x1021`, initial value `0xFFFF`, no reflection/final XOR; checksum is little-endian.
- `SEQ` is selected by the host and wraps as one byte. `QUERY` replies echo the sequence.
- `QUERY` (`0x05`) is the normal read/response operation. `RESP` is `0x06` and must match the query sequence and selector.
- Most command types are fire-and-forget. Do not infer success from a write call alone; use queries/counters where supported.
- Validate frame length, payload length, SOF, and CRC before consuming a response. Bound reads and pending requests.

## 4. Read APIs

`QUERY` payload: one-byte `what` selector; selectors that address a particular option/rule/patch add an ID byte. The query returns one matching `RESP`.

| `what` | Response/data | Purpose |
|---:|---|---|
| `0` | `VERSION` | Protocol, firmware, stable MAC, box name. Required for handshake. |
| `1` | `HEALTH` | Link, physical mouse, clone, injection, rate, lock, catch, keyboard and other readiness flags. |
| `2` | `DEVICE_INFO` | Cloned USB VID/PID, kind, product string, descriptor flags. |
| `3` | `CAPS` | Mouse axes/buttons and keyboard/media capabilities. Check before sending a usage. |
| `4` | `RATE` | Native report period and clone polling period; not Receiver inference FPS. |
| `5` | `STATS` | Inject output and transport/drop counters; `session` reports box-side state release. |
| `6` | `LOCKS` | Current input locks/scales. |
| `7` | `CATCH` | Catch subscriptions, drop counts, cross-chip clock estimate. |
| `8` | Retired | Folded into `CAPS`; do not use. |
| `9` | `OPTIONS` | Persistent options addressed by option ID. |
| `10` | `CLIP` | Buffered clip status and configuration. |
| `11` | `FIRMWARE` | Versions/slots/staged image information for both chips. |
| `12` | `REWRITE` | Rewrite table summary. |
| `13` | `REWRITE_ENTRY` | One rewrite entry, addressed by ID. |
| `14` | `PATCHES` | Descriptor patch summary. |
| `15` | `PATCH_ENTRY` | One descriptor patch, addressed by ID. |
| `16` | `TRANSFORMS` | Field-transform table. |

Useful `HEALTH` bits include `LINK_UP` (`0x01`), `MOUSE_ATTACHED` (`0x02`), and `CLONE_CONFIGURED` (`0x04`). Those three being set means the input path reaches the target PC. A successful VERSION handshake only means the box is responding; it does not guarantee the peripheral/clone path is ready.

Useful `STATS` fields include `inject_emits`, `tx_drops`, `tx_wedges`, `link_rx_drops`, `host_rx_drops`, `relay_drops`, and `session`. Treat `session` changes as possible loss of held state and re-check/reapply only the state the application owns.

## 5. Write/control APIs

| Command | Opcode | Purpose / cautions |
|---|---:|---|
| `MOVE` | `0x01` | Relative cursor delta, wheel, or horizontal pan; fire-and-forget. |
| `INJECT` | `0x03` | Press/release mouse button, keyboard usage, or media usage; fire-and-forget. |
| `RESET` | `0x04` | Clears injected/held state and related box state. |
| `QUERY` | `0x05` | Read APIs above. |
| `REBOOT` | `0x07` | Reboots a device chip. Never use for discovery. |
| `LED` | `0x09` | Device LED override. |
| `LOCK` | `0x0A` | Scales or blocks physical input. |
| `CATCH` | `0x0B` | Subscribe to input/raw traffic events. |
| `OPTION` | `0x11` | Configure supported box options. |
| `CLIP_*` | `0x12`–`0x15` | Upload/configure/play buffered input clips. |
| `UPDATE` | `0x17`/`0x18` | Firmware update transfer and reply. High impact. |
| `RAW` | `0x19` | Raw endpoint/control data. Advanced, easy to misuse. |
| `TRANSFER` | `0x1A`/`0x1B` | USB control transfer through the box; has a response. |
| `REWRITE` | `0x1C` | Rewrite rules for relayed traffic. |
| `PATCH` | `0x1D` | Patch descriptors served by the clone; can re-enumerate it. |
| `TRANSFORM` | `0x1E` | Transform selected report fields. |

### Mouse movement from a detection

`MOVE` cursor payload is six bytes: `motion=0`, signed `dx:i16`, signed `dy:i16`, then `flags`. Positive X is right; positive Y is down. It is a relative delta, not an absolute screen coordinate. For CV use, first compute a target offset from the image center, then apply user calibration/sensitivity and clamp to signed 16-bit. Preview resizing must not change the source-coordinate calculation.

Do not transmit raw pixel offsets directly. The sender frame dimensions, FOV, camera crop, detection center, calibration and device sensitivity determine the conversion. Start with bounded rates and a stop/deadman behavior before enabling continuous movement.

## 6. Physical mouse button state

Use the decoded `input_events` stream to observe the physical buttons. This is telemetry; it does not inject or change input.

```rust
use medius::{Button, CatchFilter, Device, Input};

let device = Device::find()?;
let mut input = device.input_events([
    CatchFilter::watch(Button::LEFT),
    CatchFilter::watch(Button::RIGHT),
    CatchFilter::watch(Button::MIDDLE),
    CatchFilter::watch(Button::SIDE1),
    CatchFilter::watch(Button::SIDE2),
])?;

while let Some(event) = input.recv_timeout(std::time::Duration::from_millis(100)) {
    match event.input {
        Input::Press(usage) => { /* usage.id is 0..4 for Left..Mouse5 */ }
        Input::Release(usage) => { /* usage.id is 0..4 for Left..Mouse5 */ }
        Input::Motion { .. } => {}
    }
    if !input.is_connected() { break; }
}
```

- `Button` IDs are 0=Left, 1=Right, 2=Middle, 3=Side1/Mouse4, 4=Side2/Mouse5.
- `input_events` subscribes to both press and release edges; it emits events only for actual changes and its held set starts empty. Until the first event for a button, show its UI state as unknown rather than assuming released.
- Input snapshots are decoded into edges and recover the complete held set after dropped events. The source observes physical input before lock scaling and injection.
- Use a bounded `recv_timeout` so the monitor can notice disconnects and shut down cleanly. Dropping the stream unsubscribes; `is_connected()` distinguishes idle from a dead stream. Monitor `dropped()` if event delivery health matters.
- The box can stream other event families through `catch_events` (relative motion, wheel/pan, raw HID/vendor/control traffic, emitted output, and USB bus lifecycle). Prefer `input_events` for the five-button UI; raw `catch_events` is for protocol diagnostics.

### Momentary input

`INJECT` payload: `class:u8`, `id:u16 LE`, `action:u8`; class `0=button`, `1=keyboard`, `2=media`; action `0=soft release`, `1=press`, `2=force release`. Soft release removes the software override and lets a physical hold pass through. Force release masks a physical hold. Check `CAPS` first. Always release input owned by the app on shutdown/error; avoid leaving buttons or keys held.

## 7. Errors, lifecycle, and SysX behavior

- Errors to distinguish: no matching VID/PID, open/permission/busy error, query timeout/no reply, invalid CRC/frame, unsupported `proto_ver`, device removed, and a healthy box with no physical peripheral.
- Keep USB/serial reads and queries outside the frame-processing worker. SysX's device monitor owns the connection; movement targets arrive through a bounded latest-value channel.
- Do not rescan/open the same port every frame. Rescan on startup and reconnect with backoff on disconnect.
- Drop the client handle on shutdown; the official client stops its reader/keepalive threads and closes serial on drop.
- Do not report `input chain ready` based only on `VERSION`; call `query_health()` and evaluate the flags relevant to the product.
- Current SysX monitor discovers the CH343 VID/PID, opens the path, verifies `query_version()`, and subscribes to all five physical buttons. While the configured Right/Middle/M4/M5 button is held, it sends fresh movement through `Device::move_rel()`, capped to ±32 per axis. It does not yet call `query_health()`, `device_info()`, or `inject()`.
- No Medius hardware or Windows movement test has been run. The implemented call path and software caps do not establish physical calibration or end-to-end device latency.

## 8. API map and official references

The file above covers the receiver integration path, not every feature in the vendor library. The rest of the API is grouped as follows:

| Area | Main APIs | Typical use |
|---|---|---|
| Discovery / lifecycle | `find`, `list`, `open`, `open_by_id`, `reconnect`, `reset`, `is_connected` | Find a box, retain stable identity, recover after cable/link loss. |
| Identity / telemetry | `query_version`, `query_health`, `device_info`, `caps`, `query_rate`, `query_stats`, `query_locks`, `query_catch`, `firmware_info`, `counters` | Read box, peripheral, readiness, report rate, and loss state. |
| Input | `move_rel` / `move_axis`, `inject`, `press`, `release`, `wheel`, `pan` | Relative motion and momentary button/key/media input. |
| Observe / constrain | `input_events`, `catch_events`, `lock`, `scale`, `set_bearing`, `set_movement_riding`, `set_emit_pace` | Read physical events and optionally constrain or tune input behavior. |
| Configuration tables | `options`, `rewrite`, `transform`, `patch` | Persistent options, report rewriting/transformation, and descriptor changes. These can alter passthrough behavior. |
| Advanced I/O / firmware | `raw`, `transfer`, `clip`, `led`, `reboot`, `update`, `logs` | Endpoint/control access, buffered input, firmware lifecycle, diagnostics. Keep out of routine device detection. |

Use the linked official API pages as the signature and version authority before implementing any group not described above. In particular, firmware update, raw transfer, descriptor patching, and reboot APIs need explicit product-level safeguards.

- [Native API overview and current protocol version](https://medius.k4tech.net/native)
- [Transport, CH343 VID/PID, 6 Mbaud and 8N1](https://medius.k4tech.net/native/transport)
- [Handshake and protocol 9](https://medius.k4tech.net/native/connection)
- [Frame format](https://medius.k4tech.net/native/frame)
- [Quickstart and readiness check](https://medius.k4tech.net/native/quickstart)
- [Complete QUERY / RESP selectors and reply fields](https://medius.k4tech.net/native/commands/requests)
- [MOVE command](https://medius.k4tech.net/native/commands/move)
- [INJECT command](https://medius.k4tech.net/native/commands/inject)
- [Official Rust library](https://medius.k4tech.net/library)
- [Rust connection and handshake](https://medius.k4tech.net/library/connection)
- [Rust discovery and stable IDs](https://medius.k4tech.net/library/discovery)
- [Rust request methods](https://medius.k4tech.net/library/requests)
