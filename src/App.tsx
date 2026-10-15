import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { PreviewCanvas, type PreviewFrame } from "./PreviewCanvas";
import type { PreviewScale } from "./preview-layout";
import { ReceiverBackendClient, type BackendModel, type BackendState, type DetectionObject, type LiveFrame } from "./backend-client";
import { filterPreviewDetections } from "./detection-display";

type DeviceStatus = {
  state: "connected" | "error" | "missing";
  name: string | null;
  buttons: (boolean | null)[];
};

function Mark() {
  return <img className="brand-mark" src="/receiver-mark.svg" alt="" aria-hidden="true" />;
}

function App() {
  const [backendState, setBackendState] = useState<BackendState | null>(null);
  const [backendConnected, setBackendConnected] = useState(false);
  const [backendError, setBackendError] = useState("");
  const [deviceStatus, setDeviceStatus] = useState<DeviceStatus>({ state: "missing", name: null, buttons: Array(5).fill(null) });
  const [frame, setFrame] = useState<PreviewFrame | null>(null);
  const [liveFrame, setLiveFrame] = useState<LiveFrame | null>(null);
  const [fovDegrees, setFovDegrees] = useState(36);
  const [previewScale, setPreviewScale] = useState<PreviewScale>("Smooth");
  const modelConfidence = useRef(0.5);
  const client = useRef<ReceiverBackendClient | null>(null);

  useEffect(() => {
    const receiver = new ReceiverBackendClient({
      onFrame: (next, src) => {
        setLiveFrame(next);
        setFrame({ id: next.frame_id, src, width: next.detection.processed_width, height: next.detection.processed_height });
        setBackendError("");
      },
      onState: (state) => {
        setBackendConnected(true);
        setBackendState(state);
        setFovDegrees(state.fov_degrees ?? 36);
        setPreviewScale(state.preview_scale ?? "Smooth");
        if (state.status === "waiting") {
          setFrame(null);
          setLiveFrame(null);
        }
        setBackendError(state.last_error ?? "");
      },
      onModels: (models: BackendModel[], activeId) => {
        const model = models.find((item) => item.id === activeId);
        if (model) modelConfidence.current = model.confidence;
      },
      onError: setBackendError,
      onReady: () => {
        setBackendConnected(true);
        setBackendError("");
      },
      onDisconnected: () => setBackendConnected(false),
    });
    client.current = receiver;
    void receiver.connect();
    return () => { receiver.dispose(); client.current = null; };
  }, []);

  useEffect(() => {
    let active = true;
    const refresh = () => invoke<DeviceStatus>("device_status")
      .then((status) => { if (active) setDeviceStatus(status); })
      .catch(() => { if (active) setDeviceStatus({ state: "error", name: null, buttons: Array(5).fill(null) }); });
    void refresh();
    const timer = window.setInterval(refresh, 100);
    return () => { active = false; window.clearInterval(timer); };
  }, []);

  const receiving = Boolean(frame) && backendState?.status === "streaming";
  const strategy = liveFrame?.mode ?? backendState?.mode ?? "pixel_seek";
  const detections: DetectionObject[] = receiving
    ? filterPreviewDetections(liveFrame?.detection.objects ?? [], strategy, modelConfidence.current)
    : [];
  const statusLabel = backendError
    ? backendConnected ? "Runtime error" : "Connection error"
    : receiving || backendState?.status === "streaming"
      ? "Receiving frames"
      : backendConnected
        ? "Waiting for sender"
        : "Starting receiver";

  return <main className="app-window monitor-window">
    <header className="app-header">
      <div className="nav-brand"><Mark /><span>SysX</span></div>
      <div className="header-status-group monitor-status-group">
        <span className={`device-chip ${deviceStatus.state === "connected" ? "connected" : "error"}`} aria-live="polite">
          <i />{deviceStatus.state === "connected" ? deviceStatus.name : deviceStatus.state === "error" ? "Device error" : "No device"}
        </span>
        <div className="mouse-state" role="group" aria-label="Physical mouse button state">
          <span className="mouse-state-title">Mouse</span>
          {["Left", "Right", "Middle", "Mouse4", "Mouse5"].map((label, index) => {
            const pressed = deviceStatus.state === "connected" ? deviceStatus.buttons[index] : null;
            const state = pressed === true ? "pressed" : pressed === false ? "released" : "unknown";
            return <span key={label} className={`mouse-state-key ${state}`} aria-label={`${label}: ${state}`} title={`${label}: ${state}`}><i aria-hidden="true" />{label === "Mouse4" ? "M4" : label === "Mouse5" ? "M5" : label}</span>;
          })}
        </div>
        <span className={`status-chip ${backendError ? "error" : receiving ? "ready" : "waiting"}`} aria-live="polite"><i />{statusLabel}</span>
      </div>
    </header>

    <section className="monitor-content" aria-label="Live preview">
      <section className="preview-panel monitor-preview" aria-label="Preview">
        <div className="preview-canvas">
          <PreviewCanvas
            frame={frame}
            scale={previewScale}
            detections={detections}
            targetPoint={receiving ? liveFrame?.detection.target_xy ?? null : null}
            processedWidth={liveFrame?.detection.processed_width}
            processedHeight={liveFrame?.detection.processed_height}
            cropX={liveFrame?.crop_x ?? 0}
            cropY={liveFrame?.crop_y ?? 0}
            overlay={{ boxes: true, classId: strategy === "object_detection", confidence: strategy === "object_detection" }}
            detectionColor={strategy === "pixel_seek" ? ({ yellow: "#e3bf4d", red: "#e26868", purple: "#a682dc" } as const)[backendState?.pixel_color ?? "yellow"] : "#51d7c4"}
            fovDegrees={fovDegrees}
          />
          {!frame && <div className="waiting-message" role="status" aria-live="polite">
            <span className="waiting-indicator" aria-hidden="true" />
            <span className="frame-empty-label">NO FRAME</span>
            <strong>{backendError ? backendConnected ? "Receiver error" : "Receiver unavailable" : backendConnected ? "Waiting for sender" : "Starting receiver"}</strong>
            {backendError && <span>{backendError}</span>}
          </div>}
        </div>
        <footer className="monitor-hud" aria-label="Performance">
          <span><i>FPS</i><b>{receiving ? liveFrame?.processing_fps?.toFixed(1) ?? "—" : "—"}</b></span>
          <span><i>Latency</i><b>{receiving ? `${liveFrame?.detection.total_processing_ms.toFixed(2)} ms` : "—"}</b></span>
          <span><i>Processing resolution</i><b>{receiving ? `${liveFrame?.detection.processed_width} × ${liveFrame?.detection.processed_height}` : "—"}</b></span>
        </footer>
      </section>
    </section>
  </main>;
}

export default App;
