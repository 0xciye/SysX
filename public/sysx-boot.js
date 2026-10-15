import init, * as bindings from "./sysx-ui.js";

const wasm = await init({ module_or_path: "./sysx-ui_bg.wasm" });
window.wasmBindings = bindings;
dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
