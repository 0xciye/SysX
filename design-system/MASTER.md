# SysX design system

## Direction

Installed Windows desktop receiver. Dark, minimal, precise, technical. Preview and Settings share one workspace. The image preview is prominent but constrained to 640 × 640 CSS pixels.

The only production preview source is the sender's already-cropped MJPEG frame stream; the desktop UI has no local image, video, or sample source controls.

## Tokens

| Role | Token |
|---|---|
| App background | `#0A0D0F` |
| Window surface | `#101315` |
| Navigation surface | `#121619` |
| Panel surface | `#111618` |
| Control surface | `#151A1C` |
| Primary text | `#E6EBEC` |
| Secondary text | `#99A5A8` |
| Muted text | `#788589` |
| Border | `#2B3336` |
| Focus / active accent | `#43CFBE` |
| Good / ready status | `#45C6A8` |
| Waiting / sample status | `#D9B763` |
| Error | `#E87979` |

## Typography

- UI: system sans-serif, with Segoe UI on Windows.
- Numeric telemetry: system monospace.
- Page title: 24 px semibold.
- Section title: 15–16 px medium.
- Body: 11–13 px.
- Metadata labels: 9–10 px uppercase with restrained contrast.
- Keep technical values readable; never use tiny dense logs as the primary content.

## Layout and components

- Native desktop window with the operating system title bar.
- Put Preview and Settings side by side in the single receiver screen.
- Cap the preview image viewport at 640 × 640 CSS pixels.
- Keep the Settings column independently scrollable as options are added.
- Smooth scaling enlarges low-resolution frames with aspect ratio preserved; Pixel Perfect uses nearest-neighbor scaling.
- Panels use 1 px borders, 6–8 px corner radii, and no decorative shadows or gradients.
- Controls use clear selected, disabled, waiting, and error states.
- Status uses text and a small colored indicator, never color alone.

## Preview behavior

- Preserve source aspect ratio; expose Smooth and Pixel Perfect scaling.
- Render image pixels separately from vector detection overlays.
- HUD shows measured source/inference/preview values and marks unavailable values as `—`.
- Center FOV is an always-visible circle outline with a 1–360° size control and no enable/disable option. Scale its diameter against the shorter side of the displayed image, making the setting resolution-independent; 360° is the largest inscribed circle.
- Prototype data must be visibly marked as simulated. Production never reuses the prototype telemetry.
- No Start/Pause/Stop controls. Connection is automatic; first valid frame changes the preview from waiting to active.

## Motion and accessibility

- No decorative animation. Animate only status changes with a brief opacity transition.
- Visible keyboard focus uses the accent color with sufficient contrast.
- Buttons and toggles have at least 32 px interactive height in the compact desktop layout.
- Respect reduced-motion settings.

