# Ferrum API

Implementation reference for connecting SysX to Ferrum, verifying Ferrum identity, reading command replies, and understanding the available input-control APIs. This is a summary of the official API documentation.

**Documentation checked:** 2026-10-15  
**Preferred API:** Software API through Ferrum App on Windows  
**Legacy API:** deprecated; document only for compatibility context

## 1. API families

Ferrum publishes two API families:

| API | Transport | Status / use |
|---|---|---|
| Software API — Keyboard Mouse (KM) API | Serial command interface managed by Ferrum App | Preferred API on Windows; provides mouse/keyboard commands and reads. |
| Software API — Mouse Proxy API | Virtual serial port | Intercepts and replaces each mouse input frame. Use only when implementing a time-critical input proxy, not for device presence checks. |
| Software API — KMBox Net-compatible API | UDP socket configured in Ferrum App's Net API tab | Compatibility option; official Ferrum docs discourage choosing this for new work. |
| Software API — DHZBox-compatible API | UDP socket configured in Ferrum App's Net API tab | Compatibility option; supported command subset. |
| Legacy API | Direct Silicon Labs CP210x COM port | Deprecated; intended as fallback for platforms without Ferrum App API support. |

Ferrum Software API is powered by Ferrum App on the input computer and is documented as Windows-only. It is not a REST/HTTP API. The KM API communicates using serial text commands; the two compatibility APIs use UDP. The Ferrum App must be running/owning the configured API endpoint where required.

## 2. KM serial connection and command framing

The KM API uses commands such as `km.version()` and `km.left(1)` sent as text lines. The official Mouse Proxy example uses serial at `9600` baud. The exact endpoint/baud should be confirmed from the Ferrum App and API version rather than assumed for every setup.

Ferrum Software serial replies echo the command, use `\r\n`, return any result, and end at the prompt `>>> ` (three `>` plus a space, no newline). For example:

```text
send:    km.version()\n
receive: km.version()\r\n
         kmbox: Ferrum\r\n
         >>> 
```

Some commands have no result besides the echo and prompt. The Legacy API has no command echo and no prompt, so its parser must not be reused for Software API. The Software serial API supports `km.echo(0)` to suppress both command echo and prompt; echo is enabled again when the serial port closes. With echo disabled, results end in `\r\n`, while a command with no result sends no response.

### Robust query loop

1. Open only the endpoint configured/identified for Ferrum.
2. Apply the configured serial settings and clear stale receive data before a query.
3. Send a newline-terminated command such as `km.version()\n`.
4. Read bounded chunks until `>>> ` or timeout; preserve partial data between reads.
5. Treat serial errors, timeout, malformed result, or a result other than `kmbox: Ferrum` as unverified/error.
6. Close cleanly on unplug, app exit, or endpoint switch. Use reconnect backoff; do not poll every image frame.

## 3. Device identity and naming

`km.version()` is the documented universal check for the device/API type. Through Ferrum App's KM API its documented response is:

```text
kmbox: Ferrum
```

This verifies Ferrum is answering. The documented response does **not** provide a unique Ferrum box name, stable serial/MAC identity, or the product name of the real mouse. Therefore:

- UI label for a verified reply: `Ferrum` or `Ferrum API`.
- Do not present `Ferrum` as a particular mouse model.
- The public docs reviewed do not identify a KM API call that returns the attached mouse product string. If Receiver must show that model, confirm a supported endpoint/API with Ferrum rather than infer it from the COM port.
- There is no dedicated Software API discovery/list API documented in the pages reviewed. Detection depends on a configured/identifiable serial or UDP endpoint and a valid API reply.

## 4. Keyboard Mouse (KM) API command surface

The KM API controls much of Ferrum and supports mouse movement, buttons, keys, key state reads, and blocking selected physical input. The device type (Mouse or Keyboard) is configured in Ferrum Utility on the input computer.

### Mouse commands relevant to a receiver

| Command family | Example | Purpose |
|---|---|---|
| Relative movement | `km.move(dx, dy)` | Relative X/Y motion, not an absolute desktop coordinate. Use calibrated deltas only. |
| Button state | `km.left(1)`, `km.left(0)` | Software press/release. Other Windows mouse buttons are `right`, `middle`, `side1`, `side2`. |
| Wheel | `km.wheel(1)` / `km.wheel(-1)` | One wheel step up/down. Official docs caution against values outside `-1`/`+1` unless understood. |
| Physical state reads | `km.left()` and corresponding button query | Read the state of a physical peripheral button where supported. |

`1` means press and `0` means release for button state commands. Always pair presses with releases. Ferrum includes a hardware override intended to let physical input recover if software sends an incorrect/stuck state, but this must not replace correct release handling.

`km.move(x_amount, y_amount)` uses generic relative units, not guaranteed screen pixels. Positive X moves right, negative X left; positive Y moves down, negative Y up. Windows acceleration and pointer sensitivity affect the resulting cursor displacement. Calibrate against the target machine; never assume one input unit equals one display pixel.

### Keyboard commands

The KM API includes software key press/release and supported physical key-state reads; commands are organized by the keyboard API section and key identifiers. Use the API's documented key names/usage rather than inventing string mappings. Send release for every software press, including application shutdown and error paths. Verify the peripheral is configured as the intended type in Ferrum Utility.

### General/misc commands

| Command | Role |
|---|---|
| `km.version()` | Read API/device family marker (`kmbox: Ferrum` through Ferrum App). |
| `km.echo()` | Read whether serial command echo is enabled (`1`/`0`). |
| `km.echo(0)` / `km.echo(1)` | Disable/enable command echo and the prompt for the current serial session. |
| Read-only KM queries | Read supported button/key state and API/device values. |
| Button/key/mouse setters | Change software input state; these are writes and can affect the target PC. |

Refer to the current Ferrum KM API reference for exact command arguments and key identifiers; do not use a Legacy command parser with Software API.

### Five-button state stream

The KM API has a button-change callback commonly exposed as `km.buttons(1)`. It emits a one-byte bitmap when the physical button state changes. Bit order is Left, Right, Middle, Mouse4/rear-side, Mouse5/front-side (bit 0 through bit 4); decode it as an unsigned byte because the all-released state is a NUL byte. The callback reports a snapshot, so it also corrects the UI after a missed event.

```text
km.buttons(1)      enable change callbacks
km.<bitmap byte>   callback: 5-bit held-button bitmap
km.buttons(0)      disable callback
```

The command syntax and byte format are documented in Ferrum's older KM callback reference, while the current `docs.xferrum.dev` mouse-button page lists the five names but does not specify this callback. Confirm callback availability on the selected Software API endpoint/build before depending on it. Do not parse the callback byte as UTF-8 text or confuse echoed `km.buttons(1)` with a state event. SysX keeps each button unknown until it receives the first bitmap.

`km.left()` through `km.side2()` are not equivalent to the callback: KM-compatible APIs may return ownership/lock state rather than a clean physical-only boolean. Prefer the bitmap callback for held physical state when that callback is supported.

## 5. Mouse Proxy API

The Mouse Proxy API is specifically for intercepting every physical mouse frame (including high-rate input) and returning modified or unchanged data. It uses the virtual serial port. Once enabled, it overrides/disables other mouse APIs.

Typical lifecycle:

```text
mouse_proxy.start(window_size)
loop within the selected timing window:
    frames = mouse_proxy.fetch()
    mouse_proxy.modify(frames_or_replacement)
mouse_proxy.stop()
```

- Window size is measured in USB microframes; `8` microframes = `1 ms`; documented range `3..240`.
- Data not modified before the window expires is sent unmodified.
- The official example uses serial `9600` and a loop sleeping 5 ms with an 80-microframe/10 ms window.
- It is a real-time proxy contract, not a simple device health query. Do not call `start()` as part of status detection.
- The docs describe KM API as unavailable/overwritten while Mouse Proxy is active; stop it to restore ordinary mouse API behavior.

## 6. UDP compatibility APIs

Ferrum Software API also supports KMBox Net and DHZBox-style command sets over UDP, configured through Ferrum App's **Net API** tab. These are compatibility APIs, not the same as the serial KM API. The documented KMBox Net page strongly discourages its use for new projects and recommends DHZBox or standard KM API instead. Neither compatibility page establishes an HTTP interface or a general device-discovery endpoint; address and port must come from the Ferrum App configuration.

## 7. Legacy API (deprecated)

The Legacy API is a small KM-style command set for platforms that cannot use Ferrum App APIs. Docs say connect directly to the Silicon Labs CP210x COM port at `115200` baud; it resets to that baud after device power cycle. The same move commands have behavior matching Software API, but Legacy has no command echo. Use only when Software API is unavailable and the user explicitly configures it; do not silently fall back because baud, endpoint ownership and response framing differ.

## 8. SysX integration rules

### Presence status

- Status is green only after an actual KM API reply contains `kmbox: Ferrum`; opening a serial port alone is not proof.
- No matching endpoint → `No device`.
- Candidate exists but open, timeout, or parsing fails → `Device error`.
- The string to display is `Ferrum`, since public `km.version()` does not reveal a per-unit/peripheral product name.
- A verified version reply establishes the Ferrum API family only; it does not prove that a cloned physical mouse is present or that the current UI endpoint can stream physical button state.
- Run serial discovery/query on a background worker, not on the image processing path. Cache status and refresh at a low cadence.
- The current SysX monitor probes a Silicon Labs CP210x COM port with the Legacy API at its documented 115200 baud, verifies `km.version()`, and requests the button bitmap. This is a compatibility path, not the preferred Ferrum Software API. The Software API is powered by Ferrum App and its endpoint must be obtained/configured from that app; public docs reviewed do not provide automatic serial-port discovery for it.

### Detector-to-mouse control

`km.move(dx,dy)` is relative movement. SysX converts the filtered target from original frame/crop coordinates using saved offsets and the measured image px/count (DPI does not affect injected counts), batches deltas, checks trigger state and 100 ms freshness, and clamps each command to ±32 per axis. It sends `km.move` only while the selected Right/Middle/M4/M5 physical trigger is held. This path has not been validated on Ferrum hardware or Windows; device-count calibration and disconnect behavior still need hardware testing. Moves are written through the type-0 serial queue on a dedicated writer thread (cloned port handle, 100 ms per-command write budget), so `km.buttons` telemetry is never blocked by a write.

## 9. Official references

- [Ferrum API introduction and platform support](https://docs.xferrum.dev/introduction.html)
- [Software API transports](https://docs.xferrum.dev/software_api.html)
- [Keyboard Mouse API overview](https://docs.xferrum.dev/software_api/km_api.html)
- [KM API: mouse](https://docs.xferrum.dev/software_api/km_api/mouse.html)
- [Mouse buttons](https://docs.xferrum.dev/software_api/km_api/mouse/buttons.html)
- [Mouse relative movement](https://docs.xferrum.dev/software_api/km_api/mouse/axes/move.html)
- [Ferrum KM button state callback (older official reference)](https://ferrumllc.github.io/software_api/km_api/mouse/buttons/btn_state_change_callback.html)
- [Mouse wheel](https://docs.xferrum.dev/software_api/km_api/mouse/axes/scroll.html)
- [KM API version and Ferrum identification](https://docs.xferrum.dev/software_api/km_api/misc/version.html)
- [Software serial command echo and prompt](https://docs.xferrum.dev/serial_port/command_echoing.html)
- [Mouse Proxy API and timing example](https://docs.xferrum.dev/software_api/mouse_proxy_api.html)
- [KMBox Net compatibility API](https://docs.xferrum.dev/software_api/kmbox_net_style_api.html)
- [DHZBox compatibility API](https://docs.xferrum.dev/software_api/dhzbox_style_api.html)
- [Legacy API (deprecated)](https://docs.xferrum.dev/legacy_api.html)
- [Hardware override behavior](https://docs.xferrum.dev/hardware_override.html)

## 10. API inventory for future implementation

This is a developer-oriented index of the published API families, not a promise that every command is supported by every endpoint or firmware build.

| Family | Surface | SysX relevance |
|---|---|---|
| Keyboard Mouse | `km.version`, `km.move`, five mouse buttons, click scheduling, wheel, key press/release/state, masks/locks, button/key/axis streams | Preferred Ferrum Software API. Its app-managed endpoint is not currently configured in SysX; the active compatibility path uses Legacy serial. |
| Mouse Proxy | `start`, `fetch`, `modify`, `stop` | Full physical-frame interception/replacement. It occupies the mouse API while active and is not required for current button telemetry. |
| KMBox Net compatibility | UDP command protocol configured in Ferrum App | Supported subset only; vendor discourages it for new work. Requires configured address/port, not generic discovery. |
| DHZBox compatibility | UDP command protocol configured in Ferrum App | Supported subset only; use only if a required command is unavailable in KM. |
| Legacy KM | Direct CP210x serial, 115200 baud, no command echo | Deprecated fallback. Keep its framing and endpoint separate from the App-managed Software API. |

Before adding commands, document the exact endpoint, baud, parser behavior, firmware/app version, query/set semantics, button-state source, reconnect behavior, and whether the feature is physical telemetry or software injection. This avoids treating compatible command names as identical device behavior.

### KM command map

| Group | Commands / behavior |
|---|---|
| Mouse axes | `km.move(x,y)` relative movement; `km.wheel(amount)` scroll; pointer units are affected by OS settings. |
| Mouse buttons | `km.left`, `km.right`, `km.middle`, `km.side1`, `km.side2` set software press/release; `km.click` schedules clicks. |
| Mouse telemetry | `km.buttons` emits the 5-button bitmap on changes where supported; `km.axes` streams movement; verify the callback version and serial response framing. |
| Keyboard | `km.down`, `km.up`, `km.press`, multi-key variants, state queries, masks; use vendor key IDs. |
| Miscellaneous | `km.version`, `km.echo`, baud and device configuration queries. Some commands and response behavior differ between Legacy and Software APIs. |

API details and exact support still belong to the linked per-command pages; this map is an index for developers, not an exhaustive reproduction of every command parameter.
