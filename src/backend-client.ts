import { invoke } from "@tauri-apps/api/core";

export type BackendState = {
  event: "state";
  status: "waiting" | "streaming";
  input: string;
  source: string;
  mode: "pixel_seek" | "object_detection";
  pixel_color: PixelTargetColor;
  pixel_profile_configured: boolean;
  active_model_id: string | null;
  processed_frames: number;
  source_fps: number | null;
  dropped_input_frames: number;
  processing_fps: number | null;
  rejected_frames: number;
  resolution_mismatches: number;
  transport_dropped_frames: number;
  invalid_packets: number;
  last_error: string | null;
};

export type PixelTargetColor = "yellow" | "red" | "purple";

export type DetectionObject = {
  class_id: number;
  class_name: string;
  bbox_xyxy: [number, number, number, number];
  confidence: number | null;
};

export type LiveFrame = {
  frame_id: number;
  source: string;
  source_width: number;
  source_height: number;
  mode: "pixel_seek" | "object_detection";
  model_id: string | null;
  processing_fps: number | null;
  source_fps: number | null;
  dropped_input_frames: number;
  transport_dropped_frames: number;
  detection: {
    processed_width: number;
    processed_height: number;
    inference_device: string | null;
    objects: DetectionObject[];
    total_processing_ms: number;
  };
};

type ConnectionInfo = { host: string; port: number; token: string };
type Callbacks = {
  onFrame: (frame: LiveFrame, src: string, previewFps: number | null) => void;
  onState: (state: BackendState) => void;
  onModels: (models: BackendModel[], activeModelId: string | null) => void;
  onError: (message: string) => void;
  onReady: () => void;
  onDisconnected: () => void;
};

export type BackendModel = {
  id: string;
  name: string;
  path: string;
  input_width: number;
  input_height: number;
  confidence: number;
  iou: number;
  device: "auto" | "cpu" | "mps" | "cuda";
  enabled: boolean;
  class_names: Record<string, string> | null;
};

export class ReceiverBackendClient {
  private socket: WebSocket | null = null;
  private disposed = false;
  private reconnectTimer: number | undefined;
  private previewTimes: number[] = [];
  private lastObjectUrl: string | undefined;
  private readonly objectUrls = new Set<string>();

  constructor(private readonly callbacks: Callbacks) {}

  async connect() {
    while (!this.disposed) {
      try {
        const connection = await this.waitForBackend();
        if (this.disposed) return;
        const socket = new WebSocket(`ws://${connection.host}:${connection.port}/receiver?token=${encodeURIComponent(connection.token)}`);
        socket.binaryType = "arraybuffer";
        this.socket = socket;
        socket.onopen = () => {
          this.callbacks.onReady();
          this.send({ type: "state" });
          this.send({ type: "list_models" });
        };
        socket.onmessage = (event) => this.handleMessage(event.data);
        socket.onerror = () => this.callbacks.onError("Local backend connection failed");
        await new Promise<void>((resolve) => {
          socket.onclose = () => {
            this.callbacks.onDisconnected();
            resolve();
          };
        });
        this.socket = null;
      } catch (error) {
        if (!this.disposed) this.callbacks.onError(String(error));
      }
      if (!this.disposed) await new Promise((resolve) => { this.reconnectTimer = window.setTimeout(resolve, 1000); });
    }
  }

  send(command: Record<string, unknown>): boolean {
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(JSON.stringify({ version: 1, ...command }));
      return true;
    }
    return false;
  }

  dispose() {
    this.disposed = true;
    if (this.reconnectTimer !== undefined) window.clearTimeout(this.reconnectTimer);
    this.socket?.close();
    for (const url of this.objectUrls) URL.revokeObjectURL(url);
    this.objectUrls.clear();
  }

  private async waitForBackend(): Promise<ConnectionInfo> {
    for (;;) {
      if (this.disposed) throw new Error("Backend client closed");
      try {
        return await invoke<ConnectionInfo>("backend_connection_info");
      } catch (error) {
        const message = String(error);
        if (!message.includes("starting")) throw error;
        await new Promise((resolve) => window.setTimeout(resolve, 200));
      }
    }
  }

  private handleMessage(data: string | ArrayBuffer) {
    if (typeof data === "string") {
      const event = JSON.parse(data) as BackendState | { event: "models"; models: BackendModel[]; active_model_id: string | null } | { event: "error"; message: string } | { event: "ack"; state?: BackendState };
      if (event.event === "state") this.callbacks.onState(event);
      else if (event.event === "models") this.callbacks.onModels(event.models, event.active_model_id);
      else if (event.event === "error") this.callbacks.onError(event.message);
      else if (event.event === "ack" && event.state) this.callbacks.onState(event.state);
      return;
    }
    if (!(data instanceof ArrayBuffer) || data.byteLength < 5) return;
    const metadataLength = new DataView(data).getUint32(0, false);
    if (metadataLength > data.byteLength - 4) return;
    const metadata = JSON.parse(new TextDecoder().decode(data.slice(4, 4 + metadataLength))) as { event: "frame" } & LiveFrame;
    if (metadata.event !== "frame") return;
    const url = URL.createObjectURL(new Blob([data.slice(4 + metadataLength)], { type: "image/jpeg" }));
    this.objectUrls.add(url);
    if (this.lastObjectUrl) {
      const previous = this.lastObjectUrl;
      window.setTimeout(() => {
        URL.revokeObjectURL(previous);
        this.objectUrls.delete(previous);
      }, 1000);
    }
    this.lastObjectUrl = url;
    const now = performance.now();
    this.previewTimes.push(now);
    this.previewTimes = this.previewTimes.filter((time) => now - time <= 1000);
    const previewFps = this.previewTimes.length > 1 ? (this.previewTimes.length - 1) * 1000 / (this.previewTimes.at(-1)! - this.previewTimes[0]) : null;
    this.callbacks.onFrame(metadata, url, previewFps);
  }
}
