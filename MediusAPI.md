# Medius Rust library integration

SysX supports **Medius only**, using the official **`medius` Rust crate, version 3.4.6**:
<https://medius.k4tech.net/library>. The crate requires Rust 1.85 or newer.

The runtime already used this Rust library before this migration. The former version of this file described Native wire opcodes/framing and made the application integration look like a custom Native API client. That protocol guide is replaced here by the SDK interface actually used by SysX. SysX neither builds Native frames/CRC nor implements the device handshake itself; the Rust SDK owns the transport underneath its public API.

## Connection and physical buttons

`src-tauri/src/native_app.rs` uses:

1. Enumerate Windows serial ports using the SDK's WCH CH343 identity (`VID 0x1A86`, `PID 0x55D3`) while preserving enumeration errors, then call `Device::open(path)` for the SDK-managed handshake. This recognizes the box independently of cloned-mouse enumeration and distinguishes scan failure from no matching port.
2. `device.query_version()` to obtain the library-validated firmware identity and name.
3. `device.query_health()` before subscribing or injecting. SysX requires `link_up`, `mouse_attached`, and `clone_configured`; otherwise it reports the device as unavailable, tries the next discovered control port, and retries discovery.
4. `device.input_events(filters)` with `CatchFilter::watch` for `Button::LEFT`, `RIGHT`, `MIDDLE`, `SIDE1`, and `SIDE2`.
5. `recv_timeout` and `held(Class::Button)` to update physical held states. Watch filters observe input without taking it away from passthrough.
6. `events.is_connected()` and SDK errors to clear status and reconnect after disconnect.

The SDK owns its fixed 6,000,000-baud serial link, protocol-9 handshake, query correlation, keepalive, framing and recovery. Medius is the only device adapter; no other vendor’s endpoint/baud settings are used. Cloning `Device` shares the SDK link; it does not open another physical port. SysX gives a clone to its output worker so button-event reads do not wait for an output call.

## Movement and Auto

`src-tauri/src/medius_device.rs` is the output adapter:

| SysX operation | Rust SDK call |
| --- | --- |
| Relative movement | `device.move_rel(dx: i16, dy: i16)` |
| One Auto left click | `device.press(Button::LEFT)` then `device.release(Button::LEFT)` |

The queue's `[M, dx_le, dy_le]` and `[C]` values in `serial_queue.rs` are **internal work records**, not device wire packets. The adapter decodes those records and invokes public SDK methods. They must never be written directly to a Medius serial port or replaced with serial KM text commands.

The migration preserves `move_rel` semantics: the library's default movement riding may wait for physical mouse reports. `move_rel_now` is a distinct SDK operation and is not silently substituted. `release` is the SDK's soft release: a physical left-button hold remains intact. The existing immediate press/release sequence is preserved; its actual USB/game visibility requires hardware testing.

SysX's selected physical trigger, fresh-delta aggregation, 100 ms target expiration, sensitivity/calibration, ±32 command clamp and Auto Delay/Cooldown/Sleep configuration remain above the SDK. The worker's queue completion means a library call returned, not a timestamped game acknowledgement. No Native command numbers, protocol-version constants, CRC implementation, raw serial writes or custom handshake should be added to this adapter.

## Validation

The host's test dependency enables `medius`'s `mock` feature. Adapter tests use `MockBox`/`Device::with_mock` to verify signed X/Y moves, exactly one left press/release pair, and rejection of unsupported work records. Production builds keep the normal SDK dependency without `mock`.

No physical Medius device or Windows end-to-end behavior was validated in this migration. Mock tests validate the application-to-SDK boundary, not firmware, USB timing, movement riding or game behavior.

## Official references

- [Rust library overview and installation](https://medius.k4tech.net/library)
- [Connection and discovery](https://medius.k4tech.net/library/connection)
- [Physical input and watch filters](https://medius.k4tech.net/library/catch)
- [Movement](https://medius.k4tech.net/library/move)
- [Button injection and release](https://medius.k4tech.net/library/inject)
- [Calls and timing](https://medius.k4tech.net/library/guides/calls)
- [Mock testing](https://medius.k4tech.net/library/guides/testing)
