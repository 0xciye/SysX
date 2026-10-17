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

In the Motion card select **Normal + ABCurves (experimental)** and use global
**Apply Changes**. If this settings profile has no mouse sample yet, SysX arms
physical capture automatically. Connect Medius and move the physical mouse
continuously at 1,000 Hz for at least 256 consecutive reports (about 256 ms).
The card shows collection/progress or a restart/save error. No video frame or
Motion trigger is needed for collection. Normal continues until preparation and
saving succeed; ABCurves then becomes ready automatically.

The sample is saved in local `app-settings.json` under this settings profile's
`motionRenderer` and reused on subsequent launches. It is not continuously
retrained. A settings profile without a sample arms capture automatically when
Normal + ABCurves is selected and applied. Choosing Normal bypasses the renderer;
a saved sample is retained for reuse. Switching profiles/configuration, disabling
the option, or disconnecting cancels the in-flight capture. Automatic completion
updates only the renderer, without resetting detection or Auto's pending clicks.

There is no profile import control. `enabled: true` with empty `reports` is the
persisted pending-capture state, safely using Normal meanwhile.

The JSON object has these fields:

| Field | Value |
| --- | --- |
| `sampleIntervalUs` | `1000` |
| `reports` | Exactly 256 chronological `[dx, dy]` physical mouse reports |
| `enabled` | Whether this settings profile has ABCurves selected |

Reports use native device counts, X right and Y down; they are not screen-pixel
coordinates, screenshots, Normal-generated deltas or a target trace. X accepts
signed int16; Y accepts -32767 through 32767 because canonical Y inversion cannot
represent -(-32768) in int16. Files are limited to 16 KiB. The C shim converts Y
to ABCurves' canonical Y-up convention and converts output back to Y down.

Use exactly 256 consecutive genuine reports from a verified 1,000 Hz capture
(about 256 ms). Preserve real zero-motion reports; do not invent zeros for absent
events or interpolate physical input. The sample represents the device/setup and
need not end at a target handoff. The collector and backend validate the declared
interval, shape and range and only accept timestamped physical Medius reports.

Automatic capture uses a separate SDK axis-watch subscription and worker;
it does not add high-rate events to the button/output monitor. Only physical
`Input::Motion` events stamped with `ClockDomain::HostChip` are accepted. The
Medius 3.4.6 source suppresses all-zero motion events, so pauses do not supply zero
reports. SysX never invents them: any gap, reordered timestamp, unsafe Y value or
SDK queue loss restarts the window. Consecutive timestamp intervals must be
800–1,200 µs, with a total 255-interval span within 2% of nominal 1 ms cadence.
Timestamp wrap is handled. Other polling rates require changing the mouse to
1,000 Hz; the collector does not resample input.

The authenticated native host sends one timestamped result for its current
session/profile ticket. The backend independently validates it, prepares the C
profile off the ticker, compares current and saved settings, then atomically saves
before publishing it to the renderer. A newer Apply cannot be overwritten by a
late capture. Failed saves retain Normal and retry at two-second intervals.
Inactive profile snapshots are preserved. UI state adoption preserves unrelated
unsaved edits and captured samples. No OS-wide hook, generated movement or raw
sample file is involved; only the saved profile's reports are persisted. No
synthetic or participant dataset profile is installed by default. Synthetic report
arrays exist only inside explicitly labeled tests.

## Source and license

The unmodified C Renderer and frozen 44,484-byte model are vendored from
[ABCurves](https://github.com/optima-manent/ABCurves), commit
`e197441f8d343d3b4a4069c7cae20764d0953d15` (MIT, Optima Manent).
[Provenance](../third-party/abcurves/UPSTREAM.json) records file hashes. The Rust
ownership wrapper and C ABI shim are separate SysX code. No prebuilt DLL is used;
the existing native compiler statically links the C99 core into the backend.

The MIT notice is embedded and served at `/licenses/abcurves.txt` so portable
distributions retain the required notice; it is not linked from the Motion card.
Raw ABCurves datasets
and planner checkpoints are not redistributed here.

## Validation

Local Linux x86_64, Rust 1.99.1:

- Backend: 112 unit and 42 integration tests passed; two existing tests ignored.
- UI: 14 native tests passed; WASM configuration UI check passed.
- Native host: 61 tests passed, including the headless render and mock SDK tests.
  Test-only fontconfig uses DejaVu; production UI source is unchanged.
- Upstream C loader/API and fixed-math tests: 2 passed.
- The wrapper matches upstream's frozen seed-123 output sequence with Y inversion.
- Tests cover exact disabled identity, immutable profile reuse, causal stepping,
  reset/stall/error debt rejection, settings/profile persistence, and stale routing
  in both output modes. A synthetic feedback-loop check covers 16/30/50 ms delayed
  feedback at nominal gain; it is not a physical performance measurement.

Automatic capture regressions cover timestamp wrap/rates/gaps/loss, isolated
mock SDK subscriptions, stale/cancelled tickets, persistence and renderer readiness
without video frames, unchanged Auto revision, write failures/newer saves, retry
throttling, and UI draft preservation. The SDK mock sends synthetic hardware frames
only and confirms no movement/injection frame was emitted.

No Windows build workflow, physical capture, OBS/game session or device command
was run for this change. The optional mode needs physical validation before it
can be described as matching the confirmed Normal experience.
