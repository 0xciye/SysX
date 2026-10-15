import { useEffect, useRef, useState } from "react";
import type { PreviewScale } from "./preview-layout";
import { ReceiverBackendClient, type BackendModel, type ConnectionInfo, type PixelTargetColor } from "./backend-client";
import type { Strategy } from "./detection-display";
import { loadAppSettings, saveAppSettings, type AppSettings } from "./app-settings";

function isUdpBindConflict(message: string) {
  const normalized = message.toLowerCase();
  return normalized.includes("address already in use") || normalized.includes("only one usage of each socket address");
}

function Mark() {
  return <img className="brand-mark" src="/receiver-mark.svg" alt="" aria-hidden="true" />;
}

async function getConfigConnection(): Promise<ConnectionInfo> {
  const query = new URLSearchParams(window.location.search);
  const port = Number(query.get("backendPort"));
  const token = query.get("token");
  if (Number.isInteger(port) && port > 0 && token) return { host: "127.0.0.1", port, token };
  const response = await fetch("/api/session", { cache: "no-store" });
  if (!response.ok) throw new Error("Local configuration service is unavailable");
  return await response.json() as ConnectionInfo;
}

function App() {
  const [initialSettings] = useState(() => loadAppSettings(window.localStorage));
  const [strategy, setStrategy] = useState<Strategy>(initialSettings.strategy);
  const [fovDegrees, setFovDegrees] = useState(initialSettings.fovDegrees);
  const [scale, setScale] = useState<PreviewScale>(initialSettings.scale);
  const [cropResolution, setCropResolution] = useState(initialSettings.cropResolution);
  const [settingsPage, setSettingsPage] = useState<"general" | "fov">("general");
  const [port, setPort] = useState(initialSettings.port);
  const [backendConnected, setBackendConnected] = useState(false);
  const [backendError, setBackendError] = useState("");
  const [models, setModels] = useState<BackendModel[]>([]);
  const [activeModelId, setActiveModelId] = useState<string | null>(null);
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

  const settings: AppSettings = { strategy, fovDegrees, scale, showPreview: true, cropResolution, port, pixelColor, activeModelId, modelWidth, modelHeight, modelConfidence, modelIou, modelBackend, modelPrecision, computeTarget };
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
      onFrame: () => setBackendConnected(true),
      onState: (state) => {
        setBackendConnected(true);
        if (!restoredSettings.current) {
          if (!pendingStrategy.current) setStrategy(state.mode);
          setFovDegrees(state.fov_degrees ?? initialSettings.fovDegrees);
          setScale(state.preview_scale ?? initialSettings.scale);
          setCropResolution(state.crop_resolution ?? initialSettings.cropResolution);
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
          if (initialSettings.strategy === "object_detection" && active) client.current?.send({ type: "set_strategy", mode: "object_detection" });
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
        client.current?.send({ type: "set_display_config", fov_degrees: initialSettings.fovDegrees, preview_scale: initialSettings.scale });
        const savedPort = Number(initialSettings.port);
        if (Number.isInteger(savedPort) && savedPort >= 1 && savedPort <= 65535) client.current?.send({ type: "set_udp_port", port: savedPort });
        if (initialSettings.strategy === "pixel_seek") client.current?.send({ type: "set_strategy", mode: "pixel_seek" });
      },
      onDisconnected: () => setBackendConnected(false),
    }, getConfigConnection);
    client.current = receiver;
    void receiver.connect();
    return () => { receiver.dispose(); client.current = null; };
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

  const importModel = async (file?: File) => {
    if (!file) return;
    const response = await fetch("/api/upload-model", { method: "POST", headers: { "X-Filename": encodeURIComponent(file.name) }, body: file });
    if (!response.ok) {
      setApplyMessage("Model upload failed. Check the local config connection and try again.");
      return;
    }
    const { path } = await response.json() as { path: string };
    const name = file.name.replace(/\.pt$/i, "") || "User model";
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

  const applyChanges = async () => {
    const value = Number(port);
    if (!Number.isInteger(value) || value < 1 || value > 65535) {
      setApplyMessage("Enter a UDP port from 1 to 65535.");
      return;
    }
    if (!Number.isInteger(cropResolution) || cropResolution < 1 || cropResolution > 8192) {
      setApplyMessage("Enter a crop resolution from 1 to 8192 pixels.");
      return;
    }
    try {
      const saved = await fetch("/api/settings", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(settings),
      });
      if (!saved.ok) throw new Error("Local settings could not be saved");
      const hasSelectedModel = Boolean(activeModelId && models.some((model) => model.id === activeModelId));
      const sent = [
        client.current?.send({ type: "set_udp_port", port: value }),
        client.current?.send({ type: "set_pixel_color", color: pixelColor }),
        client.current?.send({ type: "set_display_config", fov_degrees: fovDegrees, preview_scale: scale }),
        client.current?.send({ type: "set_crop_resolution", resolution: cropResolution }),
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
  const statusLabel = backendError ? (backendConnected ? "Runtime error" : "Backend unavailable") : backendConnected ? "Connected" : "Connecting";

  return (
    <main className="app-window">
      <header className="app-header">
        <div className="nav-brand"><Mark /><span>SysX Configuration</span></div>
        <span className={`status-chip ${backendError ? "error" : backendConnected ? "ready" : "waiting"}`}><i />{statusLabel}</span>
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

          {settingsPage === "general" ? <div className="general-page config-general">
            <div className="settings-panel general-settings">
              <div className="settings-row preview-options-row">
                <div className="setting-description"><h2>Preview</h2></div>
                <div className="settings-fields">
                  <label className="field settings-wide"><span className="field-label">Scaling</span><select value={scale} onChange={(event) => setScale(event.target.value as PreviewScale)}><option>Smooth</option><option>Pixel perfect</option></select></label>
                </div>
              </div>
              <div className="settings-row">
                <div className="setting-description"><h2>Input</h2></div>
                <div className="settings-fields">
                  <label className="field"><span className="field-label">UDP port</span><input className="text-input" inputMode="numeric" value={port} onChange={(event) => { const value = event.target.value.replace(/\D/g, "").slice(0, 5); requestedPort.current = value; setPort(value); }} onBlur={changePort} onKeyDown={(event) => { if (event.key === "Enter") changePort(); }} /></label>
                  <label className="field"><span className="field-label">Crop resolution</span><input className="text-input" type="number" min="1" max="8192" step="1" value={cropResolution} onChange={(event) => setCropResolution(Number(event.target.value))} aria-label="Crop resolution" /></label>
                </div>
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
                    <label className="text-button model-upload">Import user .pt model<input type="file" accept=".pt" onChange={(event) => { void importModel(event.currentTarget.files?.[0]); event.currentTarget.value = ""; }} /></label>
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
