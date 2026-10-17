# Normal + ABCurves Renderer

The default remains **Normal**. The optional **Normal + ABCurves (experimental)**
output uses ABCurves' actual C99 Renderer after Normal has computed device counts.
It does not replace Normal with ABCurves' Continuous or Static Planner, add Python,
or load ONNX models. Auto remains independent.

## Baseline and boundaries

Implementation started from main `d568de5`. Its relevant Motion/Auto source matched
the user-confirmed `stable/motion-auto-2026-10-17` branch (`a7d7a5b`). That stable
branch is preserved. `movement/normal.rs`, the Normal controller's target/error
conversion, target selection, Auto processing/state, native pending queue/clamp,
and Medius output implementation have not been changed.

Normal's speed, smoothing, sensitivity and fractional carry still determine the
intended counts. With no enabled renderer profile, the ticker forwards those
counts directly, without calling the model or resampling them. Older settings
default to this path. The addon does not promise identical wall-clock scheduling.

With ABCurves enabled:

1. Prepare the immutable 256-report profile when settings change, outside the
   movement ticker. Reuse it across trigger transitions; each active stream owns
   an independent event state. Model bytes and profile outlive every event.
2. Advance only already closed 1 ms numerical intervals. A newly received Normal
   delta becomes intent for the next four intervals, never an earlier interval.
3. Aggregate numerical reports into the existing ticker/batcher and IPC path.
   The host still clamps each axis to 32 counts and carries its normal remainder.
   Recoil is added by the host after the texture stage.
4. Reset generated intent on target loss/staleness, revision/sensitivity changes,
   inactive Motion/trigger, Auto ownership, or the Normal dead zone. A scheduling
   gap over eight numerical milliseconds discards old pending intent instead of
   replaying a catch-up burst. Renderer errors latch output off and publish an
   invalid movement target so existing pending queues clear; the existing error
   status reports the fault. Reset/reacquisition or choosing Normal recovers.

This is **batched texture**, not physical 1 kHz transport. The selected output
changes the timing and distribution of counts and adds numerical/output delay;
aggregation and native clamps can change the renderer's physical result. It has
not been qualified for a particular game, device or hardware rate.

## Mouse profile

In the Motion card, import a mouse-profile JSON file, then select the output and
use the existing global **Apply Changes** action. Import never enables the addon
by itself. Selecting Normal immediately returns to the original output path when
the configuration is applied. The profile and enabled flag are stored per saved
settings profile under `motionRenderer`.

The JSON object has these fields:

| Field | Value |
| --- | --- |
| `sampleIntervalUs` | `1000` |
| `reports` | Exactly 256 chronological `[dx, dy]` physical mouse reports |
| `enabled` | Optional; import always sets this to `false` |

Reports use native device counts, X right and Y down; they are not screen-pixel
coordinates, screenshots, Normal-generated deltas or a target trace. X accepts
signed int16; Y accepts -32767 through 32767 because canonical Y inversion cannot
represent -(-32768) in int16. Files are limited to 16 KiB. The C shim converts Y
to ABCurves' canonical Y-up convention and converts output back to Y down.

Use exactly 256 consecutive genuine reports from a verified 1,000 Hz capture
(about 256 ms). Preserve real zero-motion reports; do not invent zeros for absent
events or interpolate physical input. The sample represents the device/setup and
need not end at a target handoff. The importer validates the declared interval,
shape and range, but cannot prove physical origin or actual sample cadence.

There is currently **no automatic physical sampler**. The Medius movement-event
stream has not been verified as a lossless 1 ms report source including genuine
zero reports, so this integration does not turn it into a fabricated profile.
No synthetic or participant dataset profile is installed by default. Synthetic
report arrays exist only inside explicitly labeled tests.

## Source and license

The unmodified C Renderer and frozen 44,484-byte model are vendored from
[ABCurves](https://github.com/optima-manent/ABCurves), commit
`e197441f8d343d3b4a4069c7cae20764d0953d15` (MIT, Optima Manent).
[Provenance](../third-party/abcurves/UPSTREAM.json) records file hashes. The Rust
ownership wrapper and C ABI shim are separate SysX code. No prebuilt DLL is used;
the existing native compiler statically links the C99 core into the backend.

The MIT notice is embedded and served at `/licenses/abcurves.txt`, linked from the
Motion card, so portable distributions retain the notice. Raw ABCurves datasets
and planner checkpoints are not redistributed here.

## Validation

Local Linux x86_64, Rust 1.99.1:

- Backend: 106 unit and 42 integration tests passed; two existing tests ignored.
- UI: 13 native tests passed; WASM configuration UI check passed.
- Native host: 55 tests passed, including the headless render and mock SDK tests.
  Test-only fontconfig uses DejaVu; production UI source is unchanged.
- Upstream C loader/API and fixed-math tests: 2 passed.
- The wrapper matches upstream's frozen seed-123 output sequence with Y inversion.
- Tests cover exact disabled identity, immutable profile reuse, causal stepping,
  reset/stall/error debt rejection, settings/profile persistence, and stale routing
  in both output modes. A synthetic feedback-loop check covers 16/30/50 ms delayed
  feedback at nominal gain; it is not a physical performance measurement.

No Windows build workflow, physical capture, OBS/game session or device command
was run for this change. The optional mode needs physical validation before it
can be described as matching the confirmed Normal experience.
