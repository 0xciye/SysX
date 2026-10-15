import init, * as bindings from "./sysx-ui.js";

export async function startRustUi(wasmSource) {
  const wasm = await init(wasmSource ? { module_or_path: wasmSource } : { module_or_path: "./sysx-ui_bg.wasm" });
  window.wasmBindings = bindings;
  dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
}

if (!window.__SYSX_NATIVE_WASM_SOURCE__) {
  await startRustUi();
}
