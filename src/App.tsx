import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { PreviewCanvas, type PreviewFrame } from "./PreviewCanvas";
import type { PreviewScale } from "./preview-layout";
import { ReceiverBackendClient, type BackendModel, type BackendState, type DetectionObject, type LiveFrame, type PixelTargetColor } from "./backend-client";
import { filterPreviewDetections, type Strategy } from "./detection-display";
import { loadAppSettings, saveAppSettings, type AppSettings } from "./app-settings";

function isUdpBindConflict(message: string) {
  const normalized = message.toLowerCase();
  return normalized.includes("address already in use") || normalized.includes("only one usage of each socket address");
}

function Mark() {
  return <img className="brand-mark" src="/receiver-mark.svg" alt="" aria-hidden="true" />;
}

function PreviewPanel({ frame, scale, detections, liveFrame, previewFps, strategy, activeModelName, pixelColor, fovDegrees, backendError, backendConnected, backendState }: {
  frame: PreviewFrame | null;
  scale: PreviewScale;
  detections: DetectionObject[];
  liveFrame: LiveFrame | null;
  previewFps: number | null;
  strategy: Strategy;
  activeModelName: string | null;
  pixelColor: PixelTargetColor;
  fovDegrees: number;
  backendError: string;
  backendConnected: boolean;
  backendState: BackendState | null;
}) {
  const receiving = Boolean(frame) && backendState?.status === "streaming";
  return <section className="preview-panel" aria-label="Preview">
    <div className="preview-canvas">
      <PreviewCanvas
        frame={frame}
        scale={scale}
        detections={detections}
        processedWidth={liveFrame?.detection.processed_width}
        processedHeight={liveFrame?.detection.processed_height}
        overlay={{ boxes: true, classId: strategy === "object_detection", className: strategy === "object_detection", confidence: strategy === "object_detection" }}
        detectionColor={strategy === "pixel_seek" ? ({ yellow: "#e3bf4d", red: "#e26868", purple: "#a682dc" } as const)[pixelColor] : "#51d7c4"}
        fovDegrees={fovDegrees}
      />
      {!frame && <div className="waiting-message" role="status" aria-live="polite">
        <span className="waiting-indicator" aria-hidden="true" />
        <span className="frame-empty-label">NO FRAME</span>
        <strong>{backendError ? "Backend unavailable" : backendConnected ? "Waiting for sender" : "Starting receiver"}</strong>
        {backendError && <span>{backendError}{isUdpBindConflict(backendError) ? " · Retrying every 3 seconds" : ""}</span>}
      </div>}
    </div>
    <footer className="preview-footer">
      <div className="preview-meta">
        <span><i>Source</i><b>{frame ? `${frame.width} × ${frame.height}` : "No frame"}</b></span>
        <span><i>Inference</i><b>{liveFrame ? `${liveFrame.detection.processed_width} × ${liveFrame.detection.processed_height}` : "—"}</b></span>
        <span><i>Mode</i><b>{strategy.replace("_", " ").toUpperCase()}{activeModelName && strategy === "object_detection" ? ` · ${activeModelName}` : ""}</b></span>
        <span><i>Device</i><b>{liveFrame?.detection.inference_device ?? "—"}</b></span>
      </div>
      <div className="footer-metrics" aria-label="Performance metrics">
        <span><i>Detections</i><b>{receiving ? detections.length : "—"}</b></span>
        <span><i>Source</i><b>{receiving ? liveFrame?.source_fps?.toFixed(1) ?? "—" : "—"}<small> FPS</small></b></span>
        <span><i>Processing</i><b>{receiving ? liveFrame?.processing_fps?.toFixed(1) ?? "—" : "—"}<small> FPS</small></b></span>
        <span><i>Preview</i><b>{receiving ? previewFps?.toFixed(1) ?? "—" : "—"}<small> FPS</small></b></span>
        <span><i>Latency</i><b>{receiving ? `${liveFrame?.detection.total_processing_ms.toFixed(2)} ms` : "—"}</b></span>
        <span><i>Input drops</i><b>{receiving ? liveFrame?.dropped_input_frames ?? "—" : "—"}</b></span>
        <span><i>Transport drops</i><b>{receiving ? liveFrame?.transport_dropped_frames ?? "—" : "—"}</b></span>
      </div>
    </footer>
  </section>;
}

function App() {
  const [initialSettings] = useState(() => loadAppSettings(window.localStorage));
  const [strategy, setStrategy] = useState<Strategy>(initialSettings.strategy);
  const [fovDegrees, setFovDegrees] = useState(initialSettings.fovDegrees);
  const [scale, setScale] = useState<PreviewScale>(initialSettings.scale);
  const [showPreview, setShowPreview] = useState(initialSettings.showPreview);
  const [settingsPage, setSettingsPage] = useState<"general" | "fov">("general");
  const [port, setPort] = useState(initialSettings.port);
  const [backendState, setBackendState] = useState<BackendState | null>(null);
  const [backendConnected, setBackendConnected] = useState(false);
  const [deviceStatus, setDeviceStatus] = useState<{ state: "connected" | "error" | "missing"; name: string | null; buttons: (boolean | null)[] }>({ state: "missing", name: null, buttons: Array(5).fill(null) });
  const [backendError, setBackendError] = useState("");
  const [models, setModels] = useState<BackendModel[]>([]);
  const [activeModelId, setActiveModelId] = useState<string | null>(null);
  const [frame, setFrame] = useState<PreviewFrame | null>(null);
  const [liveFrame, setLiveFrame] = useState<LiveFrame | null>(null);
  const [previewFps, setPreviewFps] = useState<number | null>(null);
  const [pixelColor, setPixelColor] = useState<PixelTargetColor>(initialSettings.pixelColor);
  const [modelWidth, setModelWidth] = useState(initialSettings.modelWidth);
  const [modelHeight, setModelHeight] = useState(initialSettings.modelHeight);
  const [modelConfidence, setModelConfidence] = useState(initialSettings.modelConfidence);
  const [modelIou, setModelIou] = useState(initialSettings.modelIou);
  const [modelBackend, setModelBackend] = useState(initialSettings.modelBackend);
  const [modelPrecision, setModelPrecision] = useState(initialSettings.modelPrecision);
  const [computeTarget, setComputeTarget] = useState(initialSettings.computeTarget);
  const [applyMessage, setApplyMessage] = useState("");
  const client = useRef<ReceiverBackendClient | null>(null);
  const requestedPort = useRef(initialSettings.port);
  const pendingStrategy = useRef<Strategy | null>(null);
  const restoredSettings = useRef(false);
  const restoredModels = useRef(false);
  const lastAppliedSettings = useRef(JSON.stringify(initialSettings));

  const settings: AppSettings = { strategy, fovDegrees, scale, showPreview, port, pixelColor, activeModelId, modelWidth, modelHeight, modelConfidence, modelIou, modelBackend, modelPrecision, computeTarget };
  const settingsJson = JSON.stringify(settings);
  const settingsChanged = settingsJson !== lastAppliedSettings.current;

  useEffect(() => {
    if (settingsJson !== lastAppliedSettings.current) setApplyMessage("");
  }, [settingsJson]);

  useEffect(() => {
    if (!applyMessage) return;
    const timer = window.setTimeout(() => setApplyMessage(""), 3000);
    return () => window.clearTimeout(timer);
  }, [applyMessage]);

  useEffect(() => {
    const receiver = new ReceiverBackendClient({
      onFrame: (next, src, fps) => {
        setBackendConnected(true);
        setLiveFrame(next);
        setBackendState((current) => current ? { ...current, status: "streaming" } : current);
        setFrame({ id: next.frame_id, src, width: next.source_width, height: next.source_height });
        setPreviewFps(fps);
        setBackendError((current) => {
          if (!isUdpBindConflict(current)) return "";
          const runtimePort = Number(next.source.match(/:(\d+)$/)?.[1]);
          return runtimePort && runtimePort !== Number(requestedPort.current) ? current : "";
        });
      },
      onState: (state) => {
        setBackendConnected(true);
        setBackendState(state);
        if (state.status === "waiting") {
          setFrame(null);
          setLiveFrame(null);
          setPreviewFps(null);
        }
        if (!restoredSettings.current) {
          if (!pendingStrategy.current) setStrategy(state.mode);
          const runtimePort = state.source.match(/:(\d+)$/)?.[1] ?? "4200";
          requestedPort.current = runtimePort;
          setPort(runtimePort);
          setPixelColor(state.pixel_color);
        }
        setBackendError(state.last_error ?? "");
      },
      onModels: (next, active) => {
        setModels(next);
        const restoreId = initialSettings.activeModelId;
        if (!restoredModels.current) {
          restoredModels.current = true;
          if (restoreId && next.some((item) => item.id === restoreId)) {
            setActiveModelId(restoreId);
            if (active !== restoreId) client.current?.send({ type: "activate_model", id: restoreId });
            if (initialSettings.strategy === "object_detection" && active) client.current?.send({ type: "set_strategy", mode: "object_detection" });
          } else {
            setActiveModelId(active);
            if (initialSettings.strategy === "object_detection") client.current?.send({ type: "set_strategy", mode: "object_detection" });
          }
        } else {
          setActiveModelId(active);
          if (initialSettings.strategy === "object_detection" && active) client.current?.send({ type: "set_strategy", mode: "object_detection" });
        }
        const model = next.find((item) => item.id === active);
        if (active && pendingStrategy.current === "object_detection") {
          pendingStrategy.current = null;
          client.current?.send({ type: "set_strategy", mode: "object_detection" });
        }
        if (model) {
          setModelWidth(String(model.input_width));
          setModelHeight(String(model.input_height));
          setModelConfidence(String(model.confidence));
          setModelIou(String(model.iou));
        }
      },
      onError: (message) => {
        setBackendError(message);
      },
      onReady: () => {
        setBackendConnected(true);
        setBackendError("");
        if (restoredSettings.current) return;
        restoredSettings.current = true;
        client.current?.send({ type: "set_pixel_color", color: initialSettings.pixelColor });
        const savedPort = Number(initialSettings.port);
        if (Number.isInteger(savedPort) && savedPort >= 1 && savedPort <= 65535) client.current?.send({ type: "set_udp_port", port: savedPort });
        if (initialSettings.strategy === "pixel_seek") client.current?.send({ type: "set_strategy", mode: "pixel_seek" });
      },
      onDisconnected: () => setBackendConnected(false),
    });
    client.current = receiver;
    void receiver.connect();
    return () => { receiver.dispose(); client.current = null; };
  }, []);

  useEffect(() => {
    let active = true;
    const refresh = () => invoke<{ state: "connected" | "error" | "missing"; name: string | null; buttons: (boolean | null)[] }>("device_status")
      .then((status) => { if (active) setDeviceStatus(status); })
      .catch(() => { if (active) setDeviceStatus({ state: "error", name: null, buttons: Array(5).fill(null) }); });
    void refresh();
    const timer = window.setInterval(refresh, 100);
    return () => { active = false; window.clearInterval(timer); };
  }, []);

  const changeStrategy = (next: Strategy) => {
    setStrategy(next);
    if (next === "object_detection" && !activeModelId) {
      pendingStrategy.current = next;
      return;
    }
    pendingStrategy.current = null;
    client.current?.send({ type: "set_strategy", mode: next });
  };

  const changePort = () => {
    const value = Number(port);
    if (Number.isInteger(value) && value >= 1 && value <= 65535) client.current?.send({ type: "set_udp_port", port: value });
  };

  const importModel = async () => {
    const path = await open({ multiple: false, directory: false, filters: [{ name: "Ultralytics model", extensions: ["pt"] }] });
    if (typeof path !== "string") return;
    const name = path.split(/[\\/]/).at(-1)?.replace(/\.pt$/i, "") || "User model";
    const id = crypto.randomUUID();
    client.current?.send({ type: "add_model", id, name, path, input_width: Number(modelWidth), input_height: Number(modelHeight), confidence: Number(modelConfidence), iou: Number(modelIou), device: "auto" });
    client.current?.send({ type: "activate_model", id });
  };

  const updateModel = (updates: Record<string, unknown>) => {
    if (!activeModelId) return;
    client.current?.send({ type: "update_model", id: activeModelId, ...updates });
  };

  const removeActiveModel = () => {
    if (!activeModelId) return;
    client.current?.send({ type: "remove_model", id: activeModelId });
    client.current?.send({ type: "state" });
  };

  const changePixelColor = (color: PixelTargetColor) => {
    setPixelColor(color);
    client.current?.send({ type: "set_pixel_color", color });
  };

  const applyChanges = () => {
    const value = Number(port);
    if (!Number.isInteger(value) || value < 1 || value > 65535) {
      setApplyMessage("Enter a UDP port from 1 to 65535.");
      return;
    }
    try {
      const hasSelectedModel = Boolean(activeModelId && models.some((model) => model.id === activeModelId));
      const sent = [
        client.current?.send({ type: "set_udp_port", port: value }),
        client.current?.send({ type: "set_pixel_color", color: pixelColor }),
      ];
      if (strategy === "pixel_seek" || hasSelectedModel) sent.push(client.current?.send({ type: "set_strategy", mode: strategy }));
      if (hasSelectedModel && activeModelId) {
        sent.push(client.current?.send({ type: "update_model", id: activeModelId, input_width: Number(modelWidth), input_height: Number(modelHeight), confidence: Number(modelConfidence), iou: Number(modelIou) }));
        sent.push(client.current?.send({ type: "activate_model", id: activeModelId }));
      }
      saveAppSettings(window.localStorage, settings);
      lastAppliedSettings.current = settingsJson;
      setApplyMessage(!sent.every(Boolean) ? "Settings saved. Backend will apply them when connected." : strategy === "object_detection" && !hasSelectedModel ? "Settings saved. Import a model to activate Object Detection." : "Changes applied successfully.");
    } catch {
      setApplyMessage("Could not save settings. Check app storage and try again.");
    }
  };

  const activeModel = models.find((model) => model.id === activeModelId);
  const receiving = Boolean(frame) && backendState?.status === "streaming";
  const confidenceThreshold = Number(modelConfidence);
  const detections: DetectionObject[] = receiving
    ? filterPreviewDetections(liveFrame?.detection.objects ?? [], strategy, confidenceThreshold)
    : [];
  const statusLabel = backendError ? "Backend error" : receiving || backendState?.status === "streaming" ? "Receiving frames" : backendConnected ? "Waiting for sender" : "Starting receiver";

  return (
    <main className="app-window">
      <header className="app-header">
        <div className="nav-brand"><Mark /><span>SysX</span></div>
        <div className="header-status-group">
          <span className="header-device-label">Device</span>
          <span className={`device-chip ${deviceStatus.state === "connected" ? "connected" : "error"}`} aria-live="polite"><i />{deviceStatus.state === "connected" ? deviceStatus.name : deviceStatus.state === "error" ? "Device error" : "No device"}</span>
          <div className="mouse-state" role="group" aria-label="Physical mouse button state">
            <span className="mouse-state-title">Mouse</span>
            {["Left", "Right", "Middle", "Mouse4", "Mouse5"].map((label, index) => {
              const pressed = deviceStatus.state === "connected" ? deviceStatus.buttons[index] : null;
              const state = pressed === true ? "pressed" : pressed === false ? "released" : "unknown";
              return <span key={label} className={`mouse-state-key ${state}`} aria-label={`${label}: ${state}`} title={`${label}: ${state}`}><i aria-hidden="true" />{label === "Mouse4" ? "M4" : label === "Mouse5" ? "M5" : label}</span>;
            })}
          </div>
          <span className={`status-chip ${backendError ? "error" : backendState?.status === "streaming" ? "ready" : "waiting"}`}><i />{statusLabel}</span>
        </div>
      </header>

      <div className="settings-workspace">
        <nav className="settings-nav" aria-label="Settings pages">
          <button type="button" className={`settings-nav-item${settingsPage === "general" ? " active" : ""}`} aria-current={settingsPage === "general" ? "page" : undefined} onClick={() => setSettingsPage("general")}>General</button>
          <button type="button" className={`settings-nav-item${settingsPage === "fov" ? " active" : ""}`} aria-current={settingsPage === "fov" ? "page" : undefined} onClick={() => setSettingsPage("fov")}>FOV</button>
        </nav>

        <section className="settings-view" aria-label="Settings">
          <div className="page-heading">
            <h1>{settingsPage === "general" ? "General" : "FOV"}</h1>
            <div className="apply-controls"><button type="button" className="apply-button" onClick={applyChanges} disabled={!settingsChanged && !applyMessage.startsWith("Enter")}>Apply Changes</button>{applyMessage && <span className={`apply-message${applyMessage.startsWith("Enter") || applyMessage.startsWith("Could not") ? " error" : ""}`} role="status">{applyMessage}</span>}</div>
          </div>

          {settingsPage === "general" ? <div className={`general-page${showPreview ? " preview-enabled" : ""}`}>
            <div className="settings-panel general-settings">
              <div className="settings-row preview-options-row">
                <div className="setting-description"><h2>Preview</h2></div>
                <div className="settings-fields">
                  <div className="preview-toggle-row settings-wide"><span className="field-label">Show preview</span><button type="button" role="switch" aria-checked={showPreview} aria-label="Show preview" className={`switch${showPreview ? " is-on" : ""}`} onClick={() => setShowPreview((value) => !value)}><span /></button></div>
                  <label className="field settings-wide"><span className="field-label">Scaling</span><select value={scale} onChange={(event) => setScale(event.target.value as PreviewScale)}><option>Smooth</option><option>Pixel perfect</option></select></label>
                </div>
              </div>
              <div className="settings-row">
                <div className="setting-description"><h2>Input</h2></div>
          <label className="field"><span className="field-label">UDP port</span><input className="text-input" inputMode="numeric" value={port} onChange={(event) => { const value = event.target.value.replace(/\D/g, "").slice(0, 5); requestedPort.current = value; setPort(value); }} onBlur={changePort} onKeyDown={(event) => { if (event.key === "Enter") changePort(); }} /></label>
              </div>
              <div className="settings-row">
                <div className="setting-description"><h2>Detection Engine</h2></div>
                <div className="settings-fields">
                  <div className="strategy-picker settings-wide" role="radiogroup" aria-label="Detection engine">
                    <label className={`strategy-option${strategy === "pixel_seek" ? " selected" : ""}`}>
                      <input type="radio" name="detection-engine" value="pixel_seek" checked={strategy === "pixel_seek"} onChange={() => changeStrategy("pixel_seek")} />
                      <span className="strategy-radio" aria-hidden="true" />
                      <span className="strategy-copy"><strong>Pixel Seek</strong></span>
                    </label>
                    <label className={`strategy-option${strategy === "object_detection" ? " selected" : ""}`}>
                      <input type="radio" name="detection-engine" value="object_detection" checked={strategy === "object_detection"} onChange={() => changeStrategy("object_detection")} />
                      <span className="strategy-radio" aria-hidden="true" />
                      <span className="strategy-copy"><strong>Object Detection</strong></span>
                    </label>
                  </div>
                  {strategy === "pixel_seek" && <div className="pixel-color-picker settings-wide">
                    <div className="color-options" role="group" aria-label="Pixel Seek target color">
                      {(["yellow", "red", "purple"] as const).map((color) => <button type="button" key={color} aria-pressed={pixelColor === color} className={`color-option color-${color}${pixelColor === color ? " selected" : ""}`} onClick={() => changePixelColor(color)}><i aria-hidden="true" />{color[0].toUpperCase() + color.slice(1)}</button>)}
                    </div>
                  </div>}
                  {strategy === "object_detection" && <>
                    <label className="field settings-wide model-confidence">
                      <span className="range-heading"><span className="field-label">Min confidence</span><output>{Number(modelConfidence).toFixed(2)}</output></span>
                      <input type="range" min="0" max="1" step="0.01" value={modelConfidence} onChange={(event) => setModelConfidence(event.target.value)} onBlur={(event) => { if (event.currentTarget.validity.valid) updateModel({ confidence: Number(modelConfidence) }); }} aria-label="Minimum confidence" />
                    </label>
                    <label className="field"><span className="field-label">Active model</span><select value={activeModelId ?? ""} onChange={(event) => { if (event.target.value) client.current?.send({ type: "activate_model", id: event.target.value }); }}><option value="">{models.length ? "Select a model" : "No model imported"}</option>{models.map((model) => <option key={model.id} value={model.id}>{model.name}</option>)}</select></label>
                    <button type="button" className="text-button" onClick={() => void importModel()}>Import user .pt model</button>
                    <label className="field"><span className="field-label">Backend</span><select value={modelBackend} onChange={(event) => setModelBackend(event.target.value)}><option value="onnxruntime">ONNX Runtime</option><option value="tensorrt">TensorRT</option><option value="openvino">OpenVINO</option></select></label>
                    <label className="field"><span className="field-label">Precision</span><select value={modelPrecision} onChange={(event) => setModelPrecision(event.target.value)}><option value="fp16">FP16</option><option value="int8">INT8</option></select></label>
                    <label className="field settings-wide"><span className="field-label">Compute</span><select value={computeTarget} onChange={(event) => setComputeTarget(event.target.value)}><option value="cpu">CPU</option><option value="gpu">GPU</option><option value="npu">NPU</option></select></label>
                  </>}
                  {activeModel && strategy === "object_detection" && <div className="model-config settings-wide">
                    <label className="field"><span className="field-label">Input width</span><input className="text-input" inputMode="numeric" value={modelWidth} onChange={(event) => setModelWidth(event.target.value)} onBlur={() => updateModel({ input_width: Number(modelWidth) })} /></label>
                    <label className="field"><span className="field-label">Input height</span><input className="text-input" inputMode="numeric" value={modelHeight} onChange={(event) => setModelHeight(event.target.value)} onBlur={() => updateModel({ input_height: Number(modelHeight) })} /></label>
                    <label className="field"><span className="field-label">IoU</span><input className="text-input" type="number" min="0" max="1" step="0.01" value={modelIou} onChange={(event) => setModelIou(event.target.value)} onBlur={() => updateModel({ iou: Number(modelIou) })} /></label>
                    <span className="readout settings-wide model-path">{activeModel.path}</span>
                    <button type="button" className="text-button remove-model settings-wide" onClick={removeActiveModel}>Remove model from registry</button>
                  </div>}
                </div>
              </div>
            </div>
            {showPreview && <PreviewPanel frame={frame} scale={scale} detections={detections} liveFrame={liveFrame} previewFps={previewFps} strategy={strategy} activeModelName={activeModel?.name ?? null} pixelColor={pixelColor} fovDegrees={fovDegrees} backendError={backendError} backendConnected={backendConnected} backendState={backendState} />}
          </div> : <div className="settings-panel fov-page">
            <div className="fov-page-content">
              <label className="field fov-size">
                <span className="range-heading"><span className="field-label">FOV</span><output>{fovDegrees}°</output></span>
                <input type="range" min="1" max="360" step="1" value={fovDegrees} onChange={(event) => setFovDegrees(Number(event.target.value))} aria-label="FOV size in degrees" />
              </label>
            </div>
          </div>}
          {backendError && <p className="inline-error" role="status">{backendError}{isUdpBindConflict(backendError) ? " · Retrying every 3 seconds" : ""}</p>}
        </section>
      </div>
    </main>
  );
}

export default App;
